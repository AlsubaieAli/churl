//! Self-contained, `self`-free helpers for the TUI app: URL query
//! splitting/decoding/merging, Auth-tab field access, and export-path
//! resolution. Pure functions with no `App` state — split out of `app.rs`
//! into this child module so they still see the parent's imports and
//! private types via `use super::*`.

use super::*;

/// Splits a URL into its base (everything before `?`) and the decoded
/// `(name, value)` query pairs. A pair with no `=` yields an empty value; a
/// trailing/empty segment is skipped. A URL without `?` yields an empty pair list.
pub(super) fn split_query(url: &str) -> (String, Vec<(String, String)>) {
    let Some((base, query)) = url.split_once('?') else {
        return (url.to_owned(), Vec::new());
    };
    let pairs = query
        .split('&')
        .filter(|seg| !seg.is_empty())
        .map(|seg| match seg.split_once('=') {
            Some((name, value)) => (percent_decode(name.trim()), percent_decode(value.trim())),
            None => (percent_decode(seg.trim()), String::new()),
        })
        .collect();
    (base.to_owned(), pairs)
}

/// Minimal `application/x-www-form-urlencoded` decoding: `+` → space and `%XX`
/// hex escapes; invalid escapes are passed through verbatim.
pub(super) fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi * 16 + lo) as u8);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Merges committed URL query `pairs` into the request `params` per the
/// query-merge policy, returning a human-readable report (`"A updated, B added"`)
/// or `None` when nothing changed:
/// - (a) exact `name=value` row exists → ensure enabled (no duplicate);
/// - (b) name exists with a different value → the first such row gets the new
///   value + enabled;
/// - (c) name absent → append an enabled row;
/// - (d) duplicate names within the URL map positionally onto existing rows of
///   that name (extras appended), preserving multi-value params.
pub(super) fn merge_query_params(
    params: &mut Vec<Param>,
    pairs: &[(String, String)],
) -> Option<String> {
    let mut added: Vec<String> = Vec::new();
    let mut updated: Vec<String> = Vec::new();
    // Track how many rows of each name we have already claimed positionally, so
    // `?tag=a&tag=b` maps onto the 1st then 2nd existing `tag` row (rule d).
    let mut claimed: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    for (name, value) in pairs {
        // (a) an exact name+value row already exists → ensure enabled.
        if let Some(row) = params
            .iter_mut()
            .find(|p| &p.name == name && &p.value == value)
        {
            if !row.enabled {
                row.enabled = true;
                updated.push(name.clone());
            }
            // Count this exact row as claimed for positional mapping.
            *claimed.entry(name.clone()).or_insert(0) += 1;
            continue;
        }
        // (b)/(d): find the Nth existing row of this name not yet claimed.
        let skip = *claimed.get(name).unwrap_or(&0);
        let target = params.iter_mut().filter(|p| &p.name == name).nth(skip);
        if let Some(row) = target {
            row.value = value.clone();
            row.enabled = true;
            updated.push(name.clone());
            *claimed.entry(name.clone()).or_insert(0) += 1;
        } else {
            // (c) name absent (or all rows of it claimed → extra) → append.
            params.push(Param {
                name: name.clone(),
                value: value.clone(),
                enabled: true,
            });
            added.push(name.clone());
            *claimed.entry(name.clone()).or_insert(0) += 1;
        }
    }

    if added.is_empty() && updated.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    if !updated.is_empty() {
        parts.push(format!("{} updated", updated.join(", ")));
    }
    if !added.is_empty() {
        parts.push(format!("{} added", added.join(", ")));
    }
    Some(parts.join(", "))
}

/// The number of editable rows on the Auth tab for a given auth (row 0 is always
/// the kind row).
pub(super) fn auth_field_count(auth: Option<&Auth>) -> usize {
    match auth {
        None => 1,                      // kind row only
        Some(Auth::Basic { .. }) => 3,  // kind + username + password
        Some(Auth::Bearer { .. }) => 2, // kind + token
        Some(Auth::ApiKey { .. }) => 4, // kind + name + value + placement
    }
}

/// The text of an Auth-tab row's value field (row 0 is the kind row, not text).
pub(super) fn auth_field_text(auth: Option<&Auth>, row: usize) -> Option<String> {
    match (auth, row) {
        (Some(Auth::Basic { username, .. }), 1) => Some(username.clone()),
        (Some(Auth::Basic { password, .. }), 2) => Some(password.clone()),
        (Some(Auth::Bearer { token }), 1) => Some(token.clone()),
        (Some(Auth::ApiKey { name, .. }), 1) => Some(name.clone()),
        (Some(Auth::ApiKey { value, .. }), 2) => Some(value.clone()),
        _ => None,
    }
}

/// Writes an edited Auth-tab field back (both name+value edits land in the value
/// column here — auth fields have fixed labels, so `field` is ignored and the
/// text always replaces the row's single editable value).
pub(super) fn write_auth_field(
    auth: Option<&mut Auth>,
    row: usize,
    _field: EditField,
    text: String,
) {
    match (auth, row) {
        (Some(Auth::Basic { username, .. }), 1) => *username = text,
        (Some(Auth::Basic { password, .. }), 2) => *password = text,
        (Some(Auth::Bearer { token }), 1) => *token = text,
        (Some(Auth::ApiKey { name, .. }), 1) => *name = text,
        (Some(Auth::ApiKey { value, .. }), 2) => *value = text,
        _ => {}
    }
}

/// Toggles the ApiKey placement on the Auth tab's placement row (row 3).
pub(super) fn toggle_auth_placement(auth: Option<&mut Auth>, row: usize) {
    if let (Some(Auth::ApiKey { placement, .. }), 3) = (auth, row) {
        *placement = match placement {
            ApiKeyPlacement::Header => ApiKeyPlacement::Query,
            ApiKeyPlacement::Query => ApiKeyPlacement::Header,
        };
    }
}

/// Whether a multipart part's value carries no user content yet — an empty
/// inline text, or a file part with an empty path (M8.6). Used the same way
/// `discard_row_if_empty` uses "name and value both empty" for Params/Headers:
/// a cancelled `a`(dd) must not leave a nameless ghost part behind.
pub(super) fn part_value_is_empty(value: &PartValue) -> bool {
    match value {
        PartValue::Text(text) => text.is_empty(),
        PartValue::File { path, .. } => path.is_empty(),
    }
}

/// A sensible default export destination inside the workspace: `exports/<slug>.json`.
pub(super) fn default_export_path(name: &str) -> String {
    let mut slug = String::with_capacity(name.len());
    let mut prev_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "export" } else { slug };
    format!("exports/{slug}.json")
}

/// Resolves a user-typed export path against the workspace `root`, refusing any
/// path that escapes the root (`..` traversal or an absolute path outside it).
/// The `root` is canonicalized (it exists); the target is normalized lexically
/// (it may not exist yet, so `Path::canonicalize` cannot be used on it).
pub(super) fn export_target(root: &Path, input: &str) -> Result<PathBuf, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("no path given".to_owned());
    }
    let root = root.canonicalize().unwrap_or_else(|_| root.to_owned());
    let joined = if Path::new(input).is_absolute() {
        PathBuf::from(input)
    } else {
        root.join(input)
    };
    let normalized = lexical_normalize(&joined);
    if !normalized.starts_with(&root) {
        return Err("path escapes the workspace root".to_owned());
    }
    // The lexical check above can be fooled by a symlinked component *inside* the
    // root that points elsewhere (the subsequent write follows symlinks).
    // Canonicalize the deepest existing ancestor of the target and re-check it
    // against the root, so an `exports -> /etc` symlink can't tunnel out.
    if let Some(real) = existing_ancestor_canonical(&normalized)
        && !real.starts_with(&root)
    {
        return Err("path escapes the workspace root (symlinked component)".to_owned());
    }
    Ok(normalized)
}

/// Canonicalizes the deepest ancestor of `path` that actually exists on disk
/// (the target itself usually does not exist yet). Returns `None` if nothing up
/// the chain resolves.
pub(super) fn existing_ancestor_canonical(path: &Path) -> Option<PathBuf> {
    let mut probe = path;
    loop {
        if let Ok(real) = probe.canonicalize() {
            return Some(real);
        }
        probe = probe.parent()?;
    }
}

/// Resolves `.`/`..` components without touching the filesystem. A leading `..`
/// that would climb above the path root simply pops nothing further (so an
/// escaping path fails the later `starts_with` check).
pub(super) fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Resolves the session proxy from the M8 precedence chain: CLI `--proxy` >
/// per-workspace `churl.toml` > global config `proxy`. A `None` result means no
/// explicit proxy — the caller then calls no `.proxy()` at all, so reqwest honors
/// the `HTTP(S)_PROXY`/`NO_PROXY` environment. Pure so the precedence is testable
/// without the `install_runtime` side effects (state DB, workspace recency).
pub(super) fn resolve_proxy(
    cli: Option<String>,
    workspace: Option<String>,
    config_proxy: Option<&str>,
) -> Option<String> {
    cli.or(workspace)
        .or_else(|| config_proxy.map(str::to_owned))
}

#[cfg(test)]
mod proxy_precedence_tests {
    use super::resolve_proxy;

    #[test]
    fn cli_wins_over_workspace_and_config() {
        assert_eq!(
            resolve_proxy(
                Some("http://cli".into()),
                Some("http://ws".into()),
                Some("http://cfg"),
            ),
            Some("http://cli".into())
        );
    }

    #[test]
    fn workspace_wins_over_config_when_no_cli() {
        assert_eq!(
            resolve_proxy(None, Some("http://ws".into()), Some("http://cfg")),
            Some("http://ws".into())
        );
    }

    #[test]
    fn config_used_when_no_cli_or_workspace() {
        assert_eq!(
            resolve_proxy(None, None, Some("http://cfg")),
            Some("http://cfg".into())
        );
    }

    #[test]
    fn all_unset_is_none_env_fallback() {
        // None ⇒ reqwest honors the env proxy (no explicit `.proxy()`).
        assert_eq!(resolve_proxy(None, None, None), None);
    }
}
