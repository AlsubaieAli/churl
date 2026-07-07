//! `churl tutorial` — scaffolds a demo workspace so a first-time user can
//! send a request in under a minute.
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

/// Runs `churl tutorial [--dir DIR]`.
///
/// Scaffolds a demo workspace at `dir` (default: `./churl-tutorial`).
/// Refuses to overwrite a non-empty existing directory — the user must
/// delete it first or pass a different `--dir`.
pub fn run_tutorial(dir: Option<PathBuf>) -> Result<()> {
    let root = dir.unwrap_or_else(|| PathBuf::from("churl-tutorial"));

    // Guard: refuse to overwrite a non-empty directory.
    if root.exists() {
        let is_empty = root
            .read_dir()
            .with_context(|| format!("cannot read {}", root.display()))?
            .next()
            .is_none();
        if !is_empty {
            bail!(
                "{} already exists and is not empty — delete it first or use --dir to choose a different location",
                root.display()
            );
        }
    } else {
        std::fs::create_dir_all(&root)
            .with_context(|| format!("cannot create {}", root.display()))?;
    }

    // --- workspace manifest (churl.toml) ---
    let mut ws_vars = BTreeMap::new();
    ws_vars.insert("base_url".to_owned(), "https://httpbingo.org".to_owned());
    ws_vars.insert("greeting".to_owned(), "hello".to_owned());

    // The dev profile overrides `greeting`, so switching profiles visibly
    // changes what the echo API sends back.
    let mut dev_vars = BTreeMap::new();
    dev_vars.insert("greeting".to_owned(), "hello-from-dev".to_owned());

    let ws = Workspace {
        name: "churl-tutorial".to_owned(),
        vars: ws_vars,
        profiles: vec![Profile {
            name: "dev".to_owned(),
            vars: dev_vars,
        }],
    };
    save_workspace_manifest(&root, &ws)
        .with_context(|| format!("failed to write {}/churl.toml", root.display()))?;

    // --- collection directory ---
    let coll_dir = create_collection(&root, "examples")
        .with_context(|| "failed to create 'examples' collection")?;

    // Write a default (empty-vars) folder.toml.
    save_collection_meta(&coll_dir, &Default::default())
        .with_context(|| "failed to write examples/folder.toml")?;

    // --- endpoints ---
    scaffold_endpoints(&coll_dir)?;

    // --- next steps ---
    let display = root.display();
    println!("Created tutorial workspace at {display}");
    println!();
    println!("Next steps:");
    if root == Path::new("churl-tutorial") {
        println!("  cd churl-tutorial");
    } else {
        println!("  cd {display}");
    }
    println!("  churl");
    println!();
    println!("Select an endpoint in the explorer and press Ctrl-S (or <leader>s) to send it.");
    println!("Press ? to open the help overlay and see all keybindings.");

    Ok(())
}

/// Writes the three tutorial endpoints into `coll_dir` using only real
/// persistence seams — never hand-written TOML strings.
fn scaffold_endpoints(coll_dir: &Path) -> Result<()> {
    // 1. Get Anything — GET {{base_url}}/anything?name=churl
    {
        let path = create_endpoint(coll_dir, "Get Anything")
            .with_context(|| "failed to create 'Get Anything' endpoint")?;

        let ep = churl_core::model::Endpoint {
            seq: 0,
            name: "Get Anything".to_owned(),
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
            request: Request {
                method: Method::Post,
                url: "{{base_url}}/post".to_owned(),
                headers: vec![Header {
                    name: "Content-Type".to_owned(),
                    value: "application/json".to_owned(),
                    enabled: true,
                }],
                params: vec![],
                body: Some(Body {
                    kind: BodyKind::Json,
                    content: r#"{"greeting": "hello"}"#.to_owned(),
                }),
                auth: None,
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
            request: Request {
                method: Method::Get,
                url: "{{base_url}}/bearer".to_owned(),
                headers: vec![],
                params: vec![],
                body: None,
                auth: Some(Auth::Bearer {
                    token: "{{token}}".to_owned(),
                }),
            },
        };
        save_endpoint(&path, &ep).with_context(|| "failed to write 'Bearer Auth' endpoint")?;
    }

    Ok(())
}
