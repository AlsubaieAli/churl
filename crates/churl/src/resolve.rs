//! Headless workspace/endpoint/profile resolution for `churl run` (and the
//! `--profile` validation `churl send` shares).
//!
//! `run <endpoint>` addresses an endpoint exactly like the TUI explorer's
//! `SelectedEndpoint::display_path`: collection *directory names* joined by
//! `/`, with the final segment matching the endpoint's human `name` field
//! (not its filename) — a root-level endpoint is just its bare name. There is
//! no live `ExplorerState` tree in a one-shot process, so this walks
//! `churl_core::persistence` directly instead of spinning one up.

use std::collections::BTreeMap;
use std::path::PathBuf;

use churl_core::model::Endpoint;
use churl_core::persistence::{Collection, OpenWorkspace, PersistenceError, load_collection_meta};

use crate::output::{CliError, ErrorKind};

/// A resolved endpoint plus the collection var-scope chain
/// [`build_resolver`](crate::tui::app) needs, leaf → root (root collection's
/// `churl.toml [vars]` last) — the same ancestor-chain shape
/// `ExplorerState::collection_ancestor_vars` produces for the TUI.
pub struct ResolvedEndpoint {
    pub file: PathBuf,
    pub endpoint: Endpoint,
    pub ancestor_vars: Vec<BTreeMap<String, String>>,
}

/// Resolves `path` against the open workspace `ws`.
pub fn resolve_endpoint(ws: &OpenWorkspace, path: &str) -> Result<ResolvedEndpoint, CliError> {
    let mut segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let Some(leaf_name) = segments.pop() else {
        return Err(not_found(path));
    };

    // Root → leaf while descending (root's `[vars]` first); reversed below to
    // the leaf → root precedence order the resolver wants.
    let mut vars_root_to_leaf: Vec<BTreeMap<String, String>> = vec![ws.manifest().vars.clone()];
    let mut current: Option<Collection> = None;
    for segment in &segments {
        let children = match &current {
            None => ws.collections(),
            Some(coll) => coll.sub_collections(),
        }
        .map_err(|err| persistence_err(path, err))?;
        let Some(next) = children.into_iter().find(|c| c.name.as_str() == *segment) else {
            return Err(not_found(path));
        };
        vars_root_to_leaf.push(
            load_collection_meta(&next.path)
                .map(|meta| meta.vars)
                .unwrap_or_default(),
        );
        current = Some(next);
    }

    let endpoints = match &current {
        None => ws.root_collection().endpoints(),
        Some(coll) => coll.endpoints(),
    }
    .map_err(|err| persistence_err(path, err))?;
    let Some((file, endpoint)) = endpoints.into_iter().find(|(_, e)| e.name == leaf_name) else {
        return Err(not_found(path));
    };

    let mut ancestor_vars = vars_root_to_leaf;
    ancestor_vars.reverse();
    Ok(ResolvedEndpoint {
        file,
        endpoint,
        ancestor_vars,
    })
}

fn not_found(path: &str) -> CliError {
    CliError::with_detail(
        ErrorKind::EndpointNotFound,
        format!("no endpoint at {path:?}"),
        serde_json::json!({ "path": path }),
    )
}

fn persistence_err(path: &str, err: PersistenceError) -> CliError {
    CliError::new(
        ErrorKind::EndpointNotFound,
        format!("failed to resolve {path:?}: {err}"),
    )
}

/// Resolves `--profile NAME` against the open workspace's manifest (`None`
/// workspace ⇒ no profiles available at all). Mirrors
/// `App::with_config`'s profile validation exactly: an unknown name is a hard
/// error naming the available profiles, an absent `--profile` yields empty
/// vars.
pub fn resolve_profile_vars(
    ws: Option<&OpenWorkspace>,
    profile: Option<&str>,
) -> Result<BTreeMap<String, String>, CliError> {
    let Some(name) = profile else {
        return Ok(BTreeMap::new());
    };
    let profiles: Vec<churl_core::model::Profile> = ws
        .map(|w| w.manifest().profiles.clone())
        .unwrap_or_default();
    match profiles.iter().find(|p| p.name == name) {
        Some(p) => Ok(p.vars.clone()),
        None => {
            let available: Vec<&str> = profiles.iter().map(|p| p.name.as_str()).collect();
            Err(CliError::with_detail(
                ErrorKind::UnknownProfile,
                format!(
                    "unknown profile {name:?} (available: {})",
                    if available.is_empty() {
                        "none".to_owned()
                    } else {
                        available.join(", ")
                    }
                ),
                serde_json::json!({ "profile": name, "available": available }),
            ))
        }
    }
}
