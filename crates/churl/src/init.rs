//! `churl init [path] [--demo]` — scaffolds a churl workspace.
//!
//! Plain `init` writes just a blank root manifest (`churl.toml`, no
//! collections/endpoints) into the cwd or `[path]`. `init --demo` additionally
//! scaffolds the same three-endpoint demo collection the old `churl tutorial`
//! subcommand used to (that subcommand is gone — hard-removed, no alias — see
//! DECISIONS.md).
//!
//! The scaffold uses the real persistence seams (`create_collection`,
//! `create_endpoint`, `save_endpoint`, `save_collection_meta`,
//! `save_workspace_manifest`) so the generated files are byte-identical to
//! files churl itself would produce — no hand-written TOML strings for
//! endpoint or folder files.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use color_eyre::Result;
use color_eyre::eyre::{Context, bail};

use churl_core::model::{Auth, Body, BodyKind, Header, Method, Param, Profile, Request, Workspace};
use churl_core::persistence::{
    create_collection, create_endpoint, save_collection_meta, save_endpoint,
    save_workspace_manifest,
};

/// Runs `churl init [path] [--demo]`.
///
/// Target directory: `path` if given, else the current directory (like `git
/// init` — unlike the old `tutorial`, which always created a fresh
/// `./churl-tutorial` subdirectory). Refuses only when a `churl.toml` already
/// exists at the target (never demands the whole directory be empty — `init`
/// in an existing project directory, alongside unrelated files, is the normal
/// case).
pub fn run_init(dir: Option<PathBuf>, demo: bool) -> Result<()> {
    let root = dir.unwrap_or_else(|| PathBuf::from("."));

    std::fs::create_dir_all(&root).with_context(|| format!("cannot create {}", root.display()))?;

    let manifest_path = root.join("churl.toml");
    if manifest_path.exists() {
        bail!(
            "a churl workspace already exists at {} (churl.toml present)",
            root.display()
        );
    }

    let name = workspace_name(&root);

    if demo {
        scaffold_demo(&root, &name)?;
    } else {
        save_workspace_manifest(
            &root,
            &Workspace {
                name,
                ..Default::default()
            },
        )
        .with_context(|| format!("failed to write {}/churl.toml", root.display()))?;
    }

    let display = root.display();
    println!("Initialized churl workspace at {display}");
    println!();
    println!("Next steps:");
    if root != Path::new(".") {
        println!("  cd {display}");
    }
    println!("  churl                    # open the TUI");
    println!("  churl run <endpoint>     # or send an endpoint headlessly");
    if demo {
        println!();
        println!("Select an endpoint in the explorer and press Ctrl-S (or <leader>s) to send it.");
        println!("Press ? to open the help overlay and see all keybindings.");
    }

    Ok(())
}

/// Derives a workspace display name from the target directory: its final path
/// component, or `"workspace"` when none can be determined (e.g. `init` at a
/// filesystem root).
fn workspace_name(root: &Path) -> String {
    root.canonicalize()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "workspace".to_owned())
}

/// Scaffolds the demo workspace: manifest with `base_url`/`greeting` vars and
/// a `dev` profile, an `examples` collection, and three endpoints hitting
/// [httpbingo.org](https://httpbingo.org) — carried over verbatim from the old
/// `churl tutorial` scaffold.
fn scaffold_demo(root: &Path, name: &str) -> Result<()> {
    let mut ws_vars = BTreeMap::new();
    ws_vars.insert("base_url".to_owned(), "https://httpbingo.org".to_owned());
    ws_vars.insert("greeting".to_owned(), "hello".to_owned());

    // The dev profile overrides `greeting`, so switching profiles visibly
    // changes what the echo API sends back.
    let mut dev_vars = BTreeMap::new();
    dev_vars.insert("greeting".to_owned(), "hello-from-dev".to_owned());

    let ws = Workspace {
        name: name.to_owned(),
        vars: ws_vars,
        profiles: vec![Profile {
            name: "dev".to_owned(),
            vars: dev_vars,
        }],
        ..Default::default()
    };
    save_workspace_manifest(root, &ws)
        .with_context(|| format!("failed to write {}/churl.toml", root.display()))?;

    let coll_dir = create_collection(root, "examples", root)
        .with_context(|| "failed to create 'examples' collection")?;

    // Write a default (empty-vars) folder.toml.
    save_collection_meta(&coll_dir, &Default::default())
        .with_context(|| "failed to write examples/folder.toml")?;

    scaffold_demo_endpoints(&coll_dir)
}

/// Writes the three demo endpoints into `coll_dir` using only real
/// persistence seams — never hand-written TOML strings.
fn scaffold_demo_endpoints(coll_dir: &Path) -> Result<()> {
    // 1. Get Anything — GET {{base_url}}/anything?name=churl
    {
        let path = create_endpoint(coll_dir, "Get Anything")
            .with_context(|| "failed to create 'Get Anything' endpoint")?;

        let ep = churl_core::model::Endpoint {
            seq: 0,
            name: "Get Anything".to_owned(),
            assertions: Vec::new(),
            extract: std::collections::BTreeMap::new(),
            persist: Vec::new(),
            request: Request {
                method: Method::Get,
                url: "{{base_url}}/anything".to_owned(),
                headers: vec![],
                params: vec![
                    Param {
                        name: "name".to_owned(),
                        value: "churl".to_owned(),
                        enabled: true,
                    },
                    Param {
                        name: "greeting".to_owned(),
                        value: "{{greeting}}".to_owned(),
                        enabled: true,
                    },
                ],
                body: None,
                auth: None,
                insecure: false,
            },
        };
        save_endpoint(&path, &ep).with_context(|| "failed to write 'Get Anything' endpoint")?;
    }

    // 2. Post JSON — POST {{base_url}}/post with JSON body
    {
        let path = create_endpoint(coll_dir, "Post JSON")
            .with_context(|| "failed to create 'Post JSON' endpoint")?;

        let ep = churl_core::model::Endpoint {
            seq: 1,
            name: "Post JSON".to_owned(),
            assertions: Vec::new(),
            extract: std::collections::BTreeMap::new(),
            persist: Vec::new(),
            request: Request {
                method: Method::Post,
                url: "{{base_url}}/post".to_owned(),
                headers: vec![Header {
                    name: "Content-Type".to_owned(),
                    value: "application/json".to_owned(),
                    enabled: true,
                }],
                params: vec![],
                body: Some(Body::Simple {
                    kind: BodyKind::Json,
                    content: r#"{"greeting": "hello"}"#.to_owned(),
                }),
                auth: None,
                insecure: false,
            },
        };
        save_endpoint(&path, &ep).with_context(|| "failed to write 'Post JSON' endpoint")?;
    }

    // 3. Bearer Auth — GET {{base_url}}/bearer with bearer token placeholder
    {
        let path = create_endpoint(coll_dir, "Bearer Auth")
            .with_context(|| "failed to create 'Bearer Auth' endpoint")?;

        let ep = churl_core::model::Endpoint {
            seq: 2,
            name: "Bearer Auth".to_owned(),
            assertions: Vec::new(),
            extract: std::collections::BTreeMap::new(),
            persist: Vec::new(),
            request: Request {
                method: Method::Get,
                url: "{{base_url}}/bearer".to_owned(),
                headers: vec![],
                params: vec![],
                body: None,
                auth: Some(Auth::Bearer {
                    token: "{{token}}".to_owned(),
                }),
                insecure: false,
            },
        };
        save_endpoint(&path, &ep).with_context(|| "failed to write 'Bearer Auth' endpoint")?;
    }

    Ok(())
}
