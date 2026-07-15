# churl — Roadmap

The authoritative roadmap. Detailed build tracking lives with the maintainers.

## Legend

**Milestone classes** — **F** feature · **R** regression-hardening · **D** debug/drive-test · **refactor** behaviour-preserving.

**Status** — ✅ shipped · 🚧 in progress · ⏳ planned · 🔭 exploring.

## Shipped ✅

**0.2.0** — the TUI grows into a full request workbench.
- Collection interchange — Postman JSON import/export, plus curl paste/copy inside the TUI.
- Interchange parity — churl-native JSON import, symmetric with the existing export, with dialect auto-detected from the file's envelope.
- Environments & variables editor — manage workspace, collection, and profile vars, with live precedence display and masked secrets.
- Quick-jump pickers for requests and workspaces.
- Request sequences — chain requests into end-to-end flows, extracting values from each response to feed the next (with an in-memory Session store).
- Concurrent load testing — fire throttled batches with live stats and bounded, memory-safe retention.
- Response viewer polish — JSON pretty-printing, fold/wrap, in-viewer search, line-number gutter, control-char/ANSI sanitizing.
- Navigation & keymap unification — a 4-region Tab model, jump-to-pane, and a fully data-driven remappable keymap with load-time conflict warnings.
- Unified creation & tree CRUD — one `<leader>n`/`<leader>N` gesture (destination picker → name prompt, with a pasted curl auto-detected and imported), `K`/`J` reorder, and move-to / copy-to / duplicate across the recursive collection tree (per-collection `seq` ordering; a move rewrites referencing sequence steps). Plus a writable in-memory Session var group in the env editor (`a` set / `d` delete / `c` clear, masked, never persisted).
- Durability hardening — atomic saves, SQLite WAL + migration locking, comment-preserving TOML merges, reserved-name guards, and bounded memory/disk growth.
- Cross-platform proof — macOS + Windows CI matrix, `cargo-deny`, `install.ps1`, native Wayland clipboard.

**0.1.x** — first public release and the automated release train.
- The core TUI: three-pane layout, vim-style navigation, request execution, virtualized response rendering, and history.
- curl import/export with a strict round-trip corpus.
- First-class auth (basic / bearer / API-key), themes, keymaps, jump-mode, and `{{var}}` templating.
- In-app request editing and collection CRUD.
- Prebuilt binaries + `curl | sh` installer; automated release-plz + conventional-commit pipeline.

## In progress 🚧

- **Lifecycle & distribution** — self-update, uninstall, version pinning. (F)
- **Secret & request safety hardening** — tighter placeholder gating, broadened secret markers, grandfathered pre-existing secrets, request-wide save-gate coverage (headers/URL/body/params), `secret_policy = strict | warn`, and a cross-origin `redirect = strict | strip | follow-all` policy (default `strip`: auth-bearing headers are dropped when a redirect crosses the scheme+host+port origin), plus a bundled UX papercut: a first-class `<leader>r` reload that re-reads `churl.toml` + rebuilds the explorer (dirty-guarded), so external edits are picked up without a restart. (R)
- **M8.1 — request-safety follow-ups** — durable **per-endpoint** insecure-TLS opt-in (`<leader>K`, persisted on the endpoint; effective insecure = `endpoint || session`, sibling secure endpoints still verify), off-UI-thread cookie-jar persistence (no stall under WAL-lock contention), cookie-jar `RwLock` poison recovery, and masking the proxy password *while it is typed*. (F)

## Planned ⏳

- **Nested collections & root-level endpoints** — the workspace becomes one recursive collection tree (the root *is* a collection): collections nest to arbitrary depth and endpoints can live directly at the root (today the tree is one level deep and every endpoint lives inside a collection). Variables inherit down the tree (child overrides parent); existing workspaces keep working unchanged. (F)
- **Cookies + proxy + insecure-TLS** — a persistent per-workspace cookie jar (opt-in, origin-scoped, stored in `state.sqlite`), an HTTP(S) proxy (CLI `--proxy` > workspace `churl.toml` > global config > env; credentials never persisted), and a session insecure-TLS opt-in (`-k`/`<leader>k`, loud RED statusline flag). All three are session state applied by rebuilding the single client, configurable at launch (CLI + config) and live from an in-TUI **Options overlay** (`<leader>o`). Headless: `churl cookies list|clear`. (F)

### M8.1 scope 🚧 (in progress)
- 🚧 Durable **per-endpoint insecure-TLS opt-in** (`<leader>K`, persisted on the endpoint file; effective insecure = `endpoint || session`, sibling secure endpoints still verify). Per-*workspace* persistence stays out of scope by design.
- 🚧 **Off-UI-thread cookie-jar persistence** — the jar was written synchronously on the UI thread (after a mutating send / toggle-off / clear / exit); under cross-process WAL-lock contention that could stall the UI up to the ~5 s `busy_timeout`. Now offloaded to a dedicated writer thread (coalescing, flush-and-join on quit, no clobber on failure).
- 🚧 **Cookie-jar `RwLock` poison recovery** — `ChurlCookieJar` methods used `.expect("lock poisoned")`, so a prior panic while holding the lock would crash the next jar access; now recovers from a poisoned lock and continues.
- 🚧 **Mask the proxy password while it is typed** — the Options overlay's inline proxy edit masked the password of a *complete* `user:pass@` value, but a password typed *before* the `@` still rendered in plaintext; now masked within the userinfo segment as it is typed.

### Still deferred ⏳
- **Adding/editing** a cookie in the Options overlay (M8 ships view + delete only).
- curl-import remap of the cookie flags `-b`/`--cookie`, `-c`/`--cookie-jar` (M8 remaps `-x`/`--proxy` and re-notes `-k`).
- "Save current session settings as a workspace/global default" from the overlay.
- **SOCKS** proxy (`socks` feature), per-scheme distinct proxies, PAC.
- SameSite / third-party cookie policy knobs (rely on crate defaults); cookie-jar encryption at rest (parity with the unencrypted, local-only `state.sqlite`).

### Known limitations (M8.1) 🐞
- **Cookies learned over an insecure hop are not quarantined.** The per-endpoint insecure client shares the single `Arc<ChurlCookieJar>` with the verifying client, so a cookie set over an unverified request to `a.example.com` (`Domain=example.com`) can later ride a *verified* request to `api.example.com`. Cross-*origin* leak is still blocked by RFC 6265 scoping; this is the cross-*subdomain*, insecure→secure seam (see DECISIONS "Per-endpoint insecure-TLS"). The real fix — tagging jar entries with the verification state of the hop that set them and withholding insecure-origin cookies from verified requests — is bigger than M8.1.
- **No end-to-end integration test drives a real opted-in send through `App::client_for`.** Per-endpoint insecure routing is currently covered by unit tests (the `client_for` divergence white-box + the http-layer secure-vs-insecure TLS test) plus a manual PTY drive, not an automated test that sends an opted-in endpoint through the real send path against a self-signed server. Add one.

## Exploring 🔭

- **Plugin system** — community extensibility for auth schemes, body types, and viewers. (F)
