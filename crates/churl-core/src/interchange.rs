//! JSON collection interchange: import a Postman Collection v2.1 or churl-native
//! document into churl endpoints (the dialect is auto-detected from the
//! envelope), and export churl collections/workspaces to a selectable JSON
//! dialect (Postman v2.1 for round-tripping, or a lossless churl-native shape).
//!
//! The Postman mapping is hand-rolled against `serde_json::Value` on purpose:
//! real-world exports are messy and a strict typed schema would reject valid
//! files. Import is *liberal* — unknown/missing fields degrade to warnings and
//! sensible defaults rather than hard errors.
//!
//! Secret hygiene carries over: literal secret auth values are replaced
//! with `{{password}}`/`{{token}}`/`{{api_key}}` placeholders on import (so
//! [`crate::persistence::save_endpoint`] never refuses them), and export refuses
//! any endpoint still carrying a literal secret auth value — exactly like
//! [`crate::persistence`]. Placeholders pass through verbatim in both directions.

use std::path::Path;

use serde_json::{Map, Value, json};

use crate::config::{auth_secret_violations, is_template_placeholder, looks_like_secret_name};
use crate::model::{ApiKeyPlacement, Auth, Body, BodyKind, Endpoint, Method, Request, Workspace};
use crate::persistence::{self, OpenWorkspace, PersistenceError};

/// The Postman v2.1 collection schema URL churl emits on export and (loosely)
/// recognises on import.
pub const POSTMAN_V21_SCHEMA: &str =
    "https://schema.getpostman.com/json/collection/v2.1.0/collection.json";

/// The churl-native JSON envelope version.
pub const CHURL_NATIVE_VERSION: u64 = 1;

/// A selectable export dialect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonDialect {
    /// Postman Collection v2.1 — round-trips with [`import_postman_v21`].
    Postman,
    /// churl-native JSON — a thin lossless wrapper over the endpoint model.
    Native,
}

/// One request pulled out of an imported collection, with the folder path it was
/// nested under (outermost first). The writer flattens this into collection
/// names (see [`write_import`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedRequest {
    /// Folder names from the collection root down to this request (may be empty).
    pub folder_path: Vec<String>,
    /// The endpoint parsed from the Postman request.
    pub endpoint: Endpoint,
}

/// The result of importing a collection: its name, every request, and any
/// non-fatal warnings raised while mapping messy/unsupported constructs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionImport {
    /// Collection name (`info.name`, or `"Imported"` when absent).
    pub name: String,
    /// Every request, in document order.
    pub requests: Vec<ImportedRequest>,
    /// Human-readable warnings (unsupported body modes, dropped variables, …).
    pub warnings: Vec<String>,
}

/// A summary of a [`write_import`] run, for the CLI/TUI to report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSummary {
    /// Number of endpoints written to disk.
    pub endpoints: usize,
    /// Number of collections created or written into.
    pub collections: usize,
    /// Warnings carried over from the import mapping.
    pub warnings: Vec<String>,
}

/// Error importing or exporting a JSON collection.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum InterchangeError {
    /// The input was not valid JSON.
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// The JSON parsed but is not a recognised collection schema.
    #[error("unsupported collection schema: {0}")]
    UnsupportedSchema(String),
    /// Refused to export an endpoint whose auth carries literal secret values
    /// instead of `{{var}}` placeholders (mirrors
    /// [`crate::persistence::PersistenceError::SecretsInAuth`]).
    #[error("refusing to export endpoint {endpoint:?} with literal secret auth values: {}", names.join(", "))]
    Secrets {
        /// The offending endpoint's name.
        endpoint: String,
        /// Offending fields as `"auth.<field>"` strings.
        names: Vec<String>,
    },
    /// A persistence operation failed while writing imported endpoints to disk.
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
}

// Import — Postman Collection v2.1 → churl

/// Parses a Postman Collection v2.1 document into a [`CollectionImport`].
///
/// The mapping is deliberately liberal (real exports vary): unknown methods
/// default to GET with a warning, unsupported body modes drop the body with a
/// warning, and Postman collection `variable[]` entries are noted as not
/// imported. `{{var}}` placeholders map 1:1 to churl's syntax.
pub fn import_postman_v21(json: &str) -> Result<CollectionImport, InterchangeError> {
    let root: Value = serde_json::from_str(json)?;
    let obj = root
        .as_object()
        .ok_or_else(|| InterchangeError::UnsupportedSchema("top level is not an object".into()))?;

    // Be lenient about the schema string but reject clearly-wrong inputs (an
    // object with neither `item` nor `info` is not a Postman collection).
    if !obj.contains_key("item") && !obj.contains_key("info") {
        return Err(InterchangeError::UnsupportedSchema(
            "missing both `info` and `item` (not a Postman v2.1 collection)".into(),
        ));
    }

    let name = obj
        .get("info")
        .and_then(|info| info.get("name"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("Imported")
        .to_owned();

    let mut ctx = ImportCtx::default();
    if obj
        .get("variable")
        .and_then(Value::as_array)
        .is_some_and(|arr| !arr.is_empty())
    {
        ctx.warnings.push(
            "collection `variable[]` entries were not imported — define them as workspace/profile \
             vars instead"
                .to_owned(),
        );
    }
    if let Some(items) = obj.get("item").and_then(Value::as_array) {
        walk_items(items, &mut Vec::new(), &mut ctx);
    }

    Ok(CollectionImport {
        name,
        requests: ctx.requests,
        warnings: ctx.warnings,
    })
}

/// Imports a churl-native JSON document (as written by [`export_workspace`] /
/// [`export_collection`] in [`JsonDialect::Native`]) into a [`CollectionImport`].
///
/// The envelope is `{ churl_version, name, collections: [ { name, endpoints: [
/// Endpoint... ] } ] }`. A malformed top-level envelope is a hard error, but —
/// mirroring the Postman path — a single endpoint that fails to deserialize is
/// dropped with a warning rather than aborting the whole import.
///
/// A file whose `churl_version` exceeds this build's [`CHURL_NATIVE_VERSION`] is
/// rejected outright (never best-effort-parsed), so a newer producer's added
/// fields can't be silently dropped.
pub fn import_churl_native(json: &str) -> Result<CollectionImport, InterchangeError> {
    let root: Value = serde_json::from_str(json)?;
    let obj = root
        .as_object()
        .ok_or_else(|| InterchangeError::UnsupportedSchema("top level is not an object".into()))?;

    let version = match obj.get("churl_version") {
        None => {
            return Err(InterchangeError::UnsupportedSchema(
                "missing churl_version — not a churl-native collection".into(),
            ));
        }
        Some(raw) => raw.as_u64().ok_or_else(|| {
            InterchangeError::UnsupportedSchema(format!(
                "churl_version must be a non-negative integer, got {raw}"
            ))
        })?,
    };
    if version > CHURL_NATIVE_VERSION {
        return Err(InterchangeError::UnsupportedSchema(format!(
            "churl-native file version {version} is newer than this build supports \
             ({CHURL_NATIVE_VERSION}); upgrade churl"
        )));
    }

    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("Imported")
        .to_owned();

    let mut requests = Vec::new();
    let mut warnings = Vec::new();
    if let Some(collections) = obj.get("collections").and_then(Value::as_array) {
        for (ci, collection) in collections.iter().enumerate() {
            let cname = collection
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("Imported")
                .to_owned();
            let Some(endpoints) = collection.get("endpoints").and_then(Value::as_array) else {
                continue;
            };
            for (ei, raw) in endpoints.iter().enumerate() {
                match serde_json::from_value::<Endpoint>(raw.clone()) {
                    Ok(endpoint) => requests.push(ImportedRequest {
                        folder_path: vec![cname.clone()],
                        endpoint,
                    }),
                    Err(err) => warnings.push(format!(
                        "skipped endpoint {ei} in collection {cname:?} (index {ci}): {err}"
                    )),
                }
            }
        }
    }

    Ok(CollectionImport {
        name,
        requests,
        warnings,
    })
}

/// Imports a JSON collection, auto-detecting its dialect from the envelope: a
/// `churl_version` key routes to [`import_churl_native`], an `info`/`item` key
/// routes to [`import_postman_v21`], and anything else is an
/// [`InterchangeError::UnsupportedSchema`]. This is the seam both the CLI
/// (`--import-collection`) and the in-TUI import use so import "just works"
/// regardless of which format the file is in.
pub fn import_json(json: &str) -> Result<CollectionImport, InterchangeError> {
    let root: Value = serde_json::from_str(json)?;
    let obj = root
        .as_object()
        .ok_or_else(|| InterchangeError::UnsupportedSchema("top level is not an object".into()))?;
    if obj.contains_key("churl_version") {
        import_churl_native(json)
    } else if obj.contains_key("info") || obj.contains_key("item") {
        import_postman_v21(json)
    } else {
        Err(InterchangeError::UnsupportedSchema(
            "unrecognized collection format — expected churl-native or Postman v2.1".into(),
        ))
    }
}

/// Accumulator threaded through the recursive `item[]` walk.
#[derive(Default)]
struct ImportCtx {
    requests: Vec<ImportedRequest>,
    warnings: Vec<String>,
    seq: u32,
}

/// Recursively walks a Postman `item[]` array. An item carrying `request` is a
/// leaf request; an item carrying its own `item[]` is a folder — recurse,
/// pushing its `name` onto `folder_path`.
fn walk_items(items: &[Value], folder_path: &mut Vec<String>, ctx: &mut ImportCtx) {
    for item in items {
        let Some(obj) = item.as_object() else {
            continue;
        };
        if let Some(sub) = obj.get("item").and_then(Value::as_array) {
            // Folder: recurse with the folder name pushed on.
            let folder_name = match obj.get("name").and_then(Value::as_str) {
                Some(name) if !name.trim().is_empty() => name.to_owned(),
                _ => "folder".to_owned(),
            };
            folder_path.push(folder_name);
            walk_items(sub, folder_path, ctx);
            folder_path.pop();
        } else if let Some(request) = obj.get("request") {
            let name = obj.get("name").and_then(Value::as_str);
            let endpoint = map_request(request, name, ctx);
            let seq = ctx.seq;
            ctx.seq += 1;
            ctx.requests.push(ImportedRequest {
                folder_path: folder_path.clone(),
                endpoint: Endpoint { seq, ..endpoint },
            });
        }
    }
}

/// Maps a single Postman `request` object (or a bare URL string) to an
/// [`Endpoint`], collecting warnings into `ctx`.
fn map_request(request: &Value, item_name: Option<&str>, ctx: &mut ImportCtx) -> Endpoint {
    // A request may be a bare URL string in older/hand-written exports.
    if let Some(url) = request.as_str() {
        return Endpoint {
            seq: 0,
            name: item_name
                .map(str::to_owned)
                .unwrap_or_else(|| derive_name(url)),
            request: Request {
                method: Method::Get,
                url: url.to_owned(),
                headers: Vec::new(),
                params: Vec::new(),
                body: None,
                auth: None,
            },
        };
    }

    let method = request
        .get("method")
        .and_then(Value::as_str)
        .map(|raw| match raw.parse::<Method>() {
            Ok(method) => method,
            Err(_) => {
                ctx.warnings
                    .push(format!("unknown HTTP method {raw:?} — defaulted to GET"));
                Method::Get
            }
        })
        .unwrap_or(Method::Get);

    let url = map_url(request.get("url"));
    if url.is_empty() && request.get("url").is_some_and(|v| !v.is_null()) {
        ctx.warnings.push(
            "request URL had no `url.raw` (structured host/path form) — imported with an empty URL"
                .to_owned(),
        );
    }
    let headers = map_headers(request.get("header"));
    let body = map_body(request.get("body"), ctx);
    let auth = map_auth(request.get("auth"), ctx);

    let name = item_name
        .map(str::to_owned)
        .unwrap_or_else(|| derive_name(&url));

    Endpoint {
        seq: 0,
        name,
        request: Request {
            method,
            url,
            headers,
            params: Vec::new(), // query stays in the URL (matches curl import)
            body,
            auth,
        },
    }
}

/// Maps a Postman `url` field: prefer `url.raw`; accept a bare string; else "".
/// The query stays in the URL string (no explosion into params).
fn map_url(url: Option<&Value>) -> String {
    match url {
        Some(Value::String(raw)) => raw.clone(),
        Some(Value::Object(map)) => map
            .get("raw")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// Maps a Postman `header[]` array into churl [`Header`]s. A `disabled: true`
/// entry imports as `enabled = false`.
fn map_headers(header: Option<&Value>) -> Vec<crate::model::Header> {
    let Some(arr) = header.and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|entry| {
            let obj = entry.as_object()?;
            let name = obj.get("key").and_then(Value::as_str)?.to_owned();
            let value = obj
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let disabled = obj
                .get("disabled")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Some(crate::model::Header {
                name,
                value,
                enabled: !disabled,
            })
        })
        .collect()
}

/// Maps a Postman `body` object. `raw` → Text/Json; `urlencoded` → Form;
/// everything else (`formdata`/`file`/`graphql`) drops the body with a warning.
fn map_body(body: Option<&Value>, ctx: &mut ImportCtx) -> Option<Body> {
    let obj = body?.as_object()?;
    let mode = obj.get("mode").and_then(Value::as_str)?;
    match mode {
        "raw" => {
            let content = obj
                .get("raw")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let language = obj
                .get("options")
                .and_then(|o| o.get("raw"))
                .and_then(|r| r.get("language"))
                .and_then(Value::as_str);
            let kind = if language == Some("json") {
                BodyKind::Json
            } else {
                BodyKind::Text
            };
            Some(Body { kind, content })
        }
        "urlencoded" => {
            let pairs = obj.get("urlencoded").and_then(Value::as_array);
            let content = pairs
                .map(|arr| {
                    arr.iter()
                        .filter_map(|p| {
                            let key = p.get("key").and_then(Value::as_str)?;
                            let value = p.get("value").and_then(Value::as_str).unwrap_or("");
                            Some(format!("{key}={value}"))
                        })
                        .collect::<Vec<_>>()
                        .join("&")
                })
                .unwrap_or_default();
            Some(Body {
                kind: BodyKind::Form,
                content,
            })
        }
        other => {
            ctx.warnings.push(format!(
                "unsupported body mode {other:?}, imported without body"
            ));
            None
        }
    }
}

/// Maps a Postman `auth` object into a churl [`Auth`], applying the secret
/// placeholder policy to literal secret values.
fn map_auth(auth: Option<&Value>, ctx: &mut ImportCtx) -> Option<Auth> {
    let obj = auth?.as_object()?;
    let kind = obj.get("type").and_then(Value::as_str)?;
    match kind {
        "basic" => {
            let params = auth_params(obj, "basic");
            let username = params.get("username").cloned().unwrap_or_default();
            let raw = params.get("password").cloned().unwrap_or_default();
            let password = placeholderize(&raw, "{{password}}", "password", ctx);
            Some(Auth::Basic { username, password })
        }
        "bearer" => {
            let params = auth_params(obj, "bearer");
            let raw = params.get("token").cloned().unwrap_or_default();
            let token = placeholderize(&raw, "{{token}}", "token", ctx);
            Some(Auth::Bearer { token })
        }
        "apikey" => {
            let params = auth_params(obj, "apikey");
            let name = params.get("key").cloned().unwrap_or_default();
            let raw = params.get("value").cloned().unwrap_or_default();
            let placement = match params.get("in").map(String::as_str) {
                Some("query") => ApiKeyPlacement::Query,
                _ => ApiKeyPlacement::Header,
            };
            // Only a secret-looking api-key *name* forces placeholder-ization
            // (matches `auth_secret_violations`).
            let value = if looks_like_secret_name(&name) {
                placeholderize(&raw, "{{api_key}}", "api key", ctx)
            } else {
                raw
            };
            Some(Auth::ApiKey {
                name,
                value,
                placement,
            })
        }
        other => {
            ctx.warnings.push(format!(
                "unsupported auth type {other:?} — imported without auth"
            ));
            None
        }
    }
}

/// Reads a Postman auth kind's parameters, which v2.1 stores as an array of
/// `{key, value, type}` objects under the kind name (e.g. `auth.basic[]`). Some
/// exporters use an object instead — both are accepted.
fn auth_params(
    auth: &Map<String, Value>,
    kind: &str,
) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    match auth.get(kind) {
        Some(Value::Array(arr)) => {
            for entry in arr {
                if let (Some(key), Some(value)) = (
                    entry.get("key").and_then(Value::as_str),
                    entry.get("value").and_then(value_as_string),
                ) {
                    out.insert(key.to_owned(), value);
                }
            }
        }
        Some(Value::Object(map)) => {
            for (key, value) in map {
                if let Some(value) = value_as_string(value) {
                    out.insert(key.clone(), value);
                }
            }
        }
        _ => {}
    }
    out
}

/// Coerces a scalar JSON value to a string (strings verbatim, numbers/bools
/// stringified); arrays/objects/null yield `None`.
fn value_as_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Applies the secret placeholder policy: an already-`{{...}}` value is kept
/// verbatim; a literal is replaced with `placeholder` and a warning is raised.
/// An empty value is left empty (nothing to leak).
fn placeholderize(raw: &str, placeholder: &str, label: &str, ctx: &mut ImportCtx) -> String {
    if raw.is_empty() || is_template_placeholder(raw) {
        raw.to_owned()
    } else {
        ctx.warnings.push(format!(
            "{label} replaced with {placeholder} placeholder — no secrets in workspace files; \
             supply the real value via a profile/env"
        ));
        placeholder.to_owned()
    }
}

/// Derives an endpoint name from a URL: the last non-empty path segment, else
/// the host, else `"endpoint"`.
fn derive_name(url: &str) -> String {
    let without_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let without_query = without_scheme
        .split(['?', '#'])
        .next()
        .unwrap_or(without_scheme);
    let mut segments = without_query.split('/');
    let host = segments.next().unwrap_or("");
    let last = segments
        .rev()
        .find(|segment| !segment.is_empty())
        .unwrap_or("");
    let pick = if last.is_empty() { host } else { last };
    if pick.is_empty() {
        "endpoint".to_owned()
    } else {
        pick.to_owned()
    }
}

// Write imported endpoints into a workspace

/// Writes an imported collection into the workspace rooted at `root`, creating
/// one churl collection per top-level folder (nested folders are flattened by
/// joining `folder_path` with `" / "` — nested collection directories are a
/// post-release item). Root-level requests land in a collection named after the
/// import. Shared by the CLI (`--import-collection`) and the in-TUI import.
///
/// Existing collection directories are reused; endpoint filenames are
/// collision-suffixed by [`crate::persistence::create_endpoint`]. Returns a
/// summary for the caller to report.
pub fn write_import(
    root: &Path,
    import: &CollectionImport,
) -> Result<ImportSummary, InterchangeError> {
    use std::collections::BTreeMap;

    // Bootstrap a manifest so an import into a bare directory yields a workspace
    // the TUI can actually open and display — not just loose files on disk. An
    // existing manifest is left untouched (in-TUI imports already have one).
    let manifest = root.join(persistence::MANIFEST_FILENAME);
    if !manifest.exists() {
        let name = if import.name.trim().is_empty() {
            "imported".to_owned()
        } else {
            import.name.clone()
        };
        persistence::save_workspace_manifest(
            root,
            &Workspace {
                name,
                vars: BTreeMap::new(),
                profiles: Vec::new(),
            },
        )?;
    }

    // Group requests by their flattened collection name, preserving first-seen
    // order for a stable, predictable layout.
    let mut order: Vec<String> = Vec::new();
    let mut groups: BTreeMap<String, Vec<&ImportedRequest>> = BTreeMap::new();
    for req in &import.requests {
        let collection_name = if req.folder_path.is_empty() {
            import.name.clone()
        } else {
            req.folder_path.join(" / ")
        };
        if !groups.contains_key(&collection_name) {
            order.push(collection_name.clone());
        }
        groups.entry(collection_name).or_default().push(req);
    }

    let mut warnings = import.warnings.clone();
    let mut endpoints_written = 0usize;
    // Distinct group names can slugify to the same directory (e.g. "A B" and
    // "a-b", or a nested path colliding with a literal folder). `ensure_collection`
    // reuses the existing dir, silently merging them — detect that and warn.
    let mut dir_owner: BTreeMap<std::path::PathBuf, String> = BTreeMap::new();
    for collection_name in &order {
        let dir = ensure_collection(root, collection_name)?;
        match dir_owner.get(&dir) {
            Some(prev) if prev != collection_name => warnings.push(format!(
                "collections {prev:?} and {collection_name:?} map to the same directory \
                 (name collision) — their endpoints were merged"
            )),
            Some(_) => {}
            None => {
                dir_owner.insert(dir.clone(), collection_name.clone());
            }
        }
        for req in &groups[collection_name] {
            // `create_endpoint` makes a default file + name; overwrite it with
            // the imported request via `save_endpoint` (which runs the secrets
            // gate — imported auth is already placeholder-ized so it passes).
            let path = persistence::create_endpoint(&dir, &req.endpoint.name)?;
            let mut endpoint = req.endpoint.clone();
            // Let `create_endpoint`'s per-collection seq stand; keep name/request.
            endpoint.seq = load_seq(&path);
            persistence::save_endpoint(&path, &endpoint)?;
            endpoints_written += 1;
        }
    }

    Ok(ImportSummary {
        endpoints: endpoints_written,
        collections: dir_owner.len(),
        warnings,
    })
}

/// The `seq` assigned by [`crate::persistence::create_endpoint`] to a freshly
/// created endpoint file (so a save preserves the collection ordering it chose).
fn load_seq(path: &Path) -> u32 {
    persistence::load_endpoint(path)
        .map(|ep| ep.seq)
        .unwrap_or(0)
}

/// Returns the directory for the collection named `name`, creating it when
/// absent and reusing it when it already exists (matching by slug).
fn ensure_collection(root: &Path, name: &str) -> Result<std::path::PathBuf, InterchangeError> {
    match persistence::create_collection(root, name) {
        Ok(dir) => Ok(dir),
        Err(PersistenceError::AlreadyExists { path }) => Ok(path),
        Err(err) => Err(err.into()),
    }
}

// Export — churl → JSON (Postman v2.1 | native)

/// Exports the whole workspace to a JSON string in `dialect`. Every collection
/// directory is read and grouped; the secrets gate applies per endpoint.
pub fn export_workspace(
    ws: &OpenWorkspace,
    dialect: JsonDialect,
) -> Result<String, InterchangeError> {
    let mut collections: Vec<(String, Vec<Endpoint>)> = Vec::new();
    for collection in ws.collections()? {
        let endpoints = collection
            .endpoints()?
            .into_iter()
            .map(|(_, ep)| ep)
            .collect::<Vec<_>>();
        collections.push((collection.name, endpoints));
    }
    let name = ws.manifest().name.clone();
    export_collections(&name, &collections, dialect)
}

/// Exports a single collection to a JSON string in `dialect`.
pub fn export_collection(
    name: &str,
    endpoints: &[Endpoint],
    dialect: JsonDialect,
) -> Result<String, InterchangeError> {
    export_collections(
        name,
        std::slice::from_ref(&(name.to_owned(), endpoints.to_vec())),
        dialect,
    )
}

/// Shared export core over `(collection name, endpoints)` groups.
fn export_collections(
    name: &str,
    collections: &[(String, Vec<Endpoint>)],
    dialect: JsonDialect,
) -> Result<String, InterchangeError> {
    // Refuse to write any literal secret auth value (mirrors persistence).
    for (_, endpoints) in collections {
        for endpoint in endpoints {
            let violations = auth_secret_violations(endpoint);
            if !violations.is_empty() {
                return Err(InterchangeError::Secrets {
                    endpoint: endpoint.name.clone(),
                    names: violations,
                });
            }
        }
    }
    let value = match dialect {
        JsonDialect::Native => native_value(name, collections),
        JsonDialect::Postman => postman_value(name, collections),
    };
    Ok(serde_json::to_string_pretty(&value)?)
}

/// Builds the churl-native envelope: `{ churl_version, name, collections: [ {
/// name, endpoints: [Endpoint...] } ] }` (lossless — endpoints reuse their
/// serde derives).
fn native_value(name: &str, collections: &[(String, Vec<Endpoint>)]) -> Value {
    let collections: Vec<Value> = collections
        .iter()
        .map(|(cname, endpoints)| {
            json!({
                "name": cname,
                "endpoints": endpoints,
            })
        })
        .collect();
    json!({
        "churl_version": CHURL_NATIVE_VERSION,
        "name": name,
        "collections": collections,
    })
}

/// Builds a Postman v2.1 document. Multiple collections become folder item
/// groups; a single collection's endpoints sit at the top level.
fn postman_value(name: &str, collections: &[(String, Vec<Endpoint>)]) -> Value {
    let items: Vec<Value> = if collections.len() == 1 {
        collections[0].1.iter().map(postman_item).collect()
    } else {
        collections
            .iter()
            .map(|(cname, endpoints)| {
                json!({
                    "name": cname,
                    "item": endpoints.iter().map(postman_item).collect::<Vec<_>>(),
                })
            })
            .collect()
    };
    json!({
        "info": {
            "name": name,
            "schema": POSTMAN_V21_SCHEMA,
        },
        "item": items,
    })
}

/// Maps one churl [`Endpoint`] to a Postman v2.1 request item.
fn postman_item(endpoint: &Endpoint) -> Value {
    let request = &endpoint.request;
    let mut req = Map::new();
    req.insert("method".into(), json!(request.method.to_string()));

    if !request.headers.is_empty() {
        let headers: Vec<Value> = request
            .headers
            .iter()
            .map(|h| {
                let mut obj = json!({ "key": h.name, "value": h.value });
                if !h.enabled {
                    obj["disabled"] = json!(true);
                }
                obj
            })
            .collect();
        req.insert("header".into(), Value::Array(headers));
    }

    if let Some(body) = &request.body {
        req.insert("body".into(), postman_body(body));
    }
    if let Some(auth) = &request.auth {
        req.insert("auth".into(), postman_auth(auth));
    }
    // `url.raw` keeps the query in the string, matching import.
    req.insert("url".into(), json!({ "raw": request.url }));

    json!({
        "name": endpoint.name,
        "request": Value::Object(req),
    })
}

/// Maps a churl [`Body`] to a Postman body object.
fn postman_body(body: &Body) -> Value {
    match body.kind {
        BodyKind::Form => {
            let pairs: Vec<Value> = split_form(&body.content)
                .into_iter()
                .map(|(k, v)| json!({ "key": k, "value": v }))
                .collect();
            json!({ "mode": "urlencoded", "urlencoded": pairs })
        }
        BodyKind::Json => json!({
            "mode": "raw",
            "raw": body.content,
            "options": { "raw": { "language": "json" } },
        }),
        BodyKind::Text => json!({ "mode": "raw", "raw": body.content }),
    }
}

/// Splits a `k=v&k2=v2` form body into pairs (no decoding — symmetric with
/// import's raw join, so round-trips are lossless).
fn split_form(content: &str) -> Vec<(String, String)> {
    if content.is_empty() {
        return Vec::new();
    }
    content
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (k.to_owned(), v.to_owned()),
            None => (pair.to_owned(), String::new()),
        })
        .collect()
}

/// Maps a churl [`Auth`] to a Postman v2.1 auth object (array-of-params shape).
fn postman_auth(auth: &Auth) -> Value {
    match auth {
        Auth::Basic { username, password } => json!({
            "type": "basic",
            "basic": [
                { "key": "username", "value": username, "type": "string" },
                { "key": "password", "value": password, "type": "string" },
            ],
        }),
        Auth::Bearer { token } => json!({
            "type": "bearer",
            "bearer": [ { "key": "token", "value": token, "type": "string" } ],
        }),
        Auth::ApiKey {
            name,
            value,
            placement,
        } => {
            let in_ = match placement {
                ApiKeyPlacement::Header => "header",
                ApiKeyPlacement::Query => "query",
            };
            json!({
                "type": "apikey",
                "apikey": [
                    { "key": "key", "value": name, "type": "string" },
                    { "key": "value", "value": value, "type": "string" },
                    { "key": "in", "value": in_, "type": "string" },
                ],
            })
        }
    }
}

#[cfg(test)]
mod tests;
