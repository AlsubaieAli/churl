# churl — Milestones

## Status overview

| Milestone | Name | Status |
|---|---|---|
| M0 | Skeleton + CI | **done** |
| M1 | Data model + persistence | **done** |
| M2 | Layout + navigation | **done** |
| M3 | Request execution + response render | **done** |
| M4 | curl import / export + M3 follow-ups | **done** |
| M5 | Auth | **done** |
| M6 | Themes + keymaps + jump-mode + templating | **done** |
| M7 | Polish + perf + release | planned |
| M8 | Cookies + proxy | planned |
| M9 | Plugin system | planned |

> Renumbered after the M3 plan review (2026-07-05): Auth was promoted from the post-release backlog to its own milestone M5; the former M5 (themes/templating) and M6 (polish/release) shifted to M6/M7. Sections below M4 use the new numbers.

---

## M0 — Skeleton + CI

**Scope**: Cargo workspace, stub crates, placeholder TUI, CI pipeline, architecture docs.

**Deliverables**:
- Cargo workspace (`resolver = "3"`, edition 2024, shared package metadata)
- `churl-core` lib: `VERSION` const, `model::Method` enum with `Display` + `FromStr` (thiserror), unit tests
- `churl` bin+lib: clap 4 derive CLI (`Option<Command>`); `Import` stub exits 1; no-subcommand launches placeholder TUI (ratatui 0.30 + crossterm 0.29 + color-eyre; alt screen, raw mode, centered title block, q/Esc/Ctrl-C to quit; terminal restored on exit and on panic)
- Insta snapshot test via `TestBackend` 80x24
- CI: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test --all`, `rustsec/audit-check`
- Docs: `CLAUDE.md`, `docs/ARCHITECTURE.md`, `docs/DECISIONS.md`, `docs/MILESTONES.md`

**Verified by**: `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all` all green; `cargo run -p churl -- --version` prints version; `cargo build --release` succeeds.

**Next**: M1

**Open questions**: none

---

## M1 — Data model + persistence

**Scope**: Core types, TOML round-trip, config, SQLite schema.

**Deliverables**:
- `churl-core::model`: `Endpoint`, `Request`, `Response`, `Header`, `Param`, `Profile`, `Workspace`
- `churl-core::persistence`: `toml_edit`-based read/write preserving comments and ordering; lazy collection loading (parse on access, not at startup); round-trip test corpus
- `churl-core::config`: `~/.config/churl/config.toml` + per-workspace `churl.toml` loading; no-secrets enforcement
- `churl-core::history`: SQLite schema via rusqlite (bundled); migration runner; insert/query history entries
- Tests: round-trip property tests, migration idempotency

**Verified by**: `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all` (24 tests) all green; unchanged endpoint round-trip is byte-identical across a 3-fixture comment corpus; proptest (256 cases) covers fresh-save and merge-save paths; migrations idempotent across reopens.

**Notes**:
- Saving canonicalizes an explicit `enabled = true` line away (it's the default and is skipped on serialize).
- Merge preserves comments on unchanged/changed scalar values and equal-length arrays-of-tables; when the header/param count changes, that array is replaced wholesale and its comments are lost.
- `Workspace` in `model` is the parsed `churl.toml` manifest; `persistence::OpenWorkspace` is the lazy on-disk handle.

**Next**: M2

**Open questions**: none

---

## M2 — Layout + navigation

**Scope**: Full pane layout, vim keybindings, fuzzy search, command palette, edtui integration.

**Deliverables**:
- Three-pane layout: Explorer (left) | Request editor (centre) | Response viewer (right)
- Explorer tree: collection → folder → endpoint navigation
- Vim keys: `j`/`k` navigate; `Enter` selects; `/` opens nucleo fuzzy search; `:` opens command palette
- edtui integration for request body / header editing
- crokey + semantic `Key→Action` map; user-overridable via config
- tokio runtime + `EventStream`; `App` struct with `tokio::select!` loop
- Tests: navigation state machine unit tests; snapshot tests for each pane

**Verified by**: `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all` (46 tests) all green; `cargo run -p churl -- --version` works; snapshot suite covers the three-pane layout with a selected endpoint, search overlay with a typed query, palette overlay, and the no-workspace empty state (80x24 `TestBackend`).

**Notes**:
- Fuzzy engine is `nucleo-matcher` (sync), not the threaded `nucleo` crate — see DECISIONS.md.
- Explorer tree is collection → endpoint in M2; nested folders don't exist in the M1 data model yet, so the planned `folder` level is deferred until persistence grows folders.
- Explorer loads endpoint files lazily on first expand (or on search-overlay open); startup only stats collection directories.
- Request pane metadata (method/URL/headers/params) is read-only in M2; only the body is edtui-editable. Full editing UX matures in later milestones.
- Key routing precedence pinned in DECISIONS.md; edtui owns insert/visual modality internally.
- `AppMsg` has only `Redraw`; `Response` arrives with M3.

**Next**: M3

**Open questions**: none

---

## M3 — Request execution + response render

**Scope**: Async HTTP, cancel, virtualised scrolling, history writes.

**Deliverables**:
- `churl-core::http`: reqwest + rustls request execution; coarse timing; `AbortHandle` per request; results as `AppMsg::Response`
- Response viewer: virtualised line render with line-offset index; 1 MB fixture test (< 50 ms draw)
- Syntax highlighting: syntect + two-face, off-thread, viewport-only, cached by viewport hash
- Cancel in-flight request (`Ctrl-C` in request context)
- History writes to SQLite on each completed request
- Tests: wiremock HTTP mocking; 1 MB draw perf test

**Verified by**: `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all` all green; `cargo run -p churl -- --version` works. New tests (22): `churl-core` wiremock suite (GET 200 headers/body, POST derived Content-Type, user Content-Type override, disabled header/param excluded, param appended to existing query, connection-refused error, invalid-URL error, task-abort cancels the request); response-view unit tests (multiline offsets, empty body, trailing/no-trailing newline, scroll clamp, byte formatting); highlight unit tests (Content-Type→token, JSON→spans); explorer scroll-offset unit test; stale-generation drop test; snapshot tests (response pane with JSON body, in-flight, failed, 1 MB draw `< 50 ms`, explorer scrolled to keep the selection visible).

**Notes**:
- `churl-core::http::execute` is runtime-agnostic (plain `async fn`); the TUI owns the `tokio::spawn`ed task + `AbortHandle` and a `generation` counter drops stale results. Ctrl-C cancels an in-flight request; `q`/`Esc` always quit.
- Response viewer is virtualised (line-offset index; only the visible lines are ever materialised). No line wrapping in M3 — long lines truncate at the pane width.
- Syntax highlighting is off-thread (dedicated `std::thread` + `SyntaxSet`/theme loaded lazily), viewport-only, cached by a viewport hash; starts stateless per viewport (known multi-line-construct imperfection — see DECISIONS.md). Foreground RGB only, two-face Nord theme.
- Response status line shows `status · time · size · N hdrs`; a full response-headers view is deferred (see open questions).
- History rows are inserted synchronously on success/failure/cancel; a failed history open disables history for the session (non-fatal, statusline warning).
- The M2 explorer scroll-offset nit is fixed here: the explorer keeps the selected row in the viewport (`scroll_to_fit`, mirroring the picker overlay).
- reqwest 0.13 renamed the pure-rustls feature `rustls-tls` → `rustls` (see DECISIONS.md); `build_client` selects it via `tls_backend_rustls()`.
- Send is captured by edtui while the body editor is in a non-Normal (insert/visual) mode, per the pinned key-routing precedence — trigger it from Normal mode or another pane. (Same class as the M2 Ctrl-C-in-insert nit.)

**Open questions** — all three resolved in the 2026-07-05 plan review:
- ~~Response body-size cap~~ → **M4** (configurable cap + `truncated` flag).
- ~~Response headers view~~ → **M7** (headers toggle; count-only until then).
- ~~Horizontal scroll / wrapping~~ → **M7** (wrap toggle chosen over horizontal scroll).

**Next**: M4

---

## M4 — curl import / export + M3 follow-ups

**Scope**: Full curl command parsing and generation; round-trip corpus. Plus two M3 review decisions (owner, 2026-07-05): the response body-size cap and insert-mode Ctrl-S/Ctrl-C interception.

**Deliverables**:
- `churl-core::import`: shlex tokenisation; hand-rolled flag map covering `-X`, `-H`, `-d`/`--data`/`--data-raw`/`--data-binary`/`--json`, `-F` (multipart), `-u`, `-L`, `--compressed`, `-k`, `-o`, `-s`, `-v`, URL positional
- `churl-core::export`: generate `curl` command from `Endpoint`
- Round-trip test corpus (≥ 20 real-world curl commands)
- `churl import` subcommand wired up (replaces M0 stub)
- **Body-size cap** (closes the M3 open question): stream the response body with a cap — default 10 MB, config-overridable (`max_body_bytes`); `Response` gains a `truncated` flag; the response status line shows `truncated at N MB` when hit
- **Configurable request timeout**: `timeout_secs` in config (default 30, the current hard-coded `DEFAULT_TIMEOUT`) — same knob class as `max_body_bytes`; per-endpoint override deferred until a real need appears
- **Insert-mode Ctrl-S/Ctrl-C**: intercepted *before* edtui in insert/visual mode (send / cancel-or-quit work without Esc). The one documented exception to the "edtui owns non-Normal modes" routing rule — Ctrl-S/Ctrl-C are not text-input keys
- `-u`/`Authorization:` import lands as a plain header in M4; M5 remaps it into the first-class auth model

**Verified by**: `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all` (123 tests) all green; `cargo run -p churl -- --version` works; `cargo run -p churl -- import "curl https://example.com"` prints endpoint TOML. New tests (55): round-trip corpus (25 commands, import → export → import semantic equality) + Stripe-style import + paste-safe export; per-flag import unit tests incl. every error variant (`Tokenize`, `MissingUrl`, `MultipleUrls`, `UnknownFlag`, `MissingValue`, `Unsupported` for `-F`/`@file`, `InvalidMethod`); export unit tests (quoting, disabled header/param exclusion, param query encoding, GET-with-body `-X GET`); body-cap wiremock suite (over-cap truncates at boundary, exact-cap not truncated, small body unchanged); config knob tests (`timeout_secs`/`max_body_bytes` parse + defaults); truncated status-line unit test + 180-column snapshot; insert-mode routing tests (Ctrl-S sends, Ctrl-C cancels in-flight / quits otherwise, plain `s`/`c` reach edtui, remapped CONTROL send intercepted); `churl import` CLI integration tests (stdout TOML, stderr warnings, `--name`, `--out`, non-zero error exit).

**Notes**:
- Flag policy is strict: any flag outside the supported set is a hard `UnknownFlag` error; `@file` data payloads and `-F` multipart are `Unsupported` errors (never silently dropped, never file reads). Query strings stay in the URL on import — never exploded into `Param`s (lossless).
- Export shell-quotes every argument via `shlex::try_quote` (single paste-safe line). `-X` is omitted for a body-less GET but emitted for a GET *with* a body, so the round-trip survives import's body-implies-POST inference.
- `churl import` prints the endpoint TOML via the persistence serializer (`endpoint_to_toml`, identical to on-disk shape); `--out` writes through `save_endpoint`. No workspace discovery in M4.
- `execute` now takes `&ExecuteOptions` and streams the body chunk-wise (`Response.truncated`, cut at the cap boundary); `build_client` takes the timeout `Duration`. Both knobs resolve from config (`Config::max_body_bytes()` / `Config::timeout()`).
- Insert-mode Ctrl-S/Ctrl-C interception resolves through the crokey keymap (not hardcoded key codes), so user remaps are honoured; only CONTROL-modified keys can be intercepted, so no text-input key is ever stolen.

**Open questions**:
- ~~Multipart (`-F`) import: the data model has no multipart body. Reject-with-error is the M4 behaviour — should multipart become a model feature (own milestone or M7 backlog), or stay permanently unsupported? Owner call.~~ **Resolved 2026-07-06 (owner)**: multipart becomes a model feature — approved into the post-release backlog (slot after M8); the hard `Unsupported` error stands until then.

**Next**: M5

---

## M5 — Auth

**Scope**: Minimal first-class auth (promoted from the post-release backlog, owner decision 2026-07-05 — its own milestone so M4 stays lean; costs the `-u`-as-header remap step).

**Deliverables**:
- `churl-core::model`: auth on `Request` — basic, bearer, API key (header or query placement); TOML persistence (format-preserving, same merge rules); **no secrets in workspace files** — auth *values* are `{{var}}` placeholders or env references, enforced by the existing name-marker heuristic
- Request pane: read-only auth line (type + masked/placeholder value)
- Execution: auth applied in `churl-core::http::execute` (header/query injection); user-supplied `Authorization` header still wins
- curl import/export remap: `-u` → basic auth; recognisable `Authorization: Bearer …` → bearer; migration note for M4-imported plain headers
- OAuth2 client-credentials stays in the backlog
- Tests: model round-trip, execute injection (wiremock), import remap corpus

**Verified by**: `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all` (153 tests) all green; `cargo run -p churl -- --version` works; `churl import "curl -u alice:s3cr3t …"` prints a `[request.auth]` basic table with a `{{password}}` placeholder and the remap warning on stderr. New tests (30): `Auth` internally-tagged TOML round-trip per kind + default-placement skip; `apply_auth` wire-effect unit tests; `auth_secret_violations` per kind; wiremock injection suite (basic base64, bearer, apikey header, apikey query appended after params with the existing URL query preserved, enabled user `Authorization` beats auth, disabled user header does not, apikey header beaten by a same-name enabled user header); import remap suite (`-u` placeholder+warning, placeholder pass kept, colon-less `-u`, Bearer remap, placeholder token kept, other `Authorization` schemes stay plain, multiple-auth-sources first-wins both orders); export per kind + apikey wire-equivalence round-trip; comment-bearing `[request.auth]` fixture (byte-identical unchanged save, comments survive mutation); auth merge add/kind-change/remove (stale keys dropped); `save_endpoint` + `endpoint_to_toml` literal-secret refusal; proptest strategy extended with `Option<Auth>`; request-pane snapshots (placeholder shown verbatim, literal masked).

**Notes**:
- The auth model is an internally-tagged enum (`[request.auth]`, `type = "basic" | "bearer" | "apikey"`); toml_edit handles the tagged representation fine (no fallback struct needed). `placement = "header"` is the apikey default and omitted on serialize.
- Plugin guardrail (§M9): `churl-core::auth::apply_auth` is the single dispatch point — every kind resolves to an `AuthWire::Header`/`AuthWire::Query` effect there; `execute()` only applies effects and never matches on `Auth`.
- **No secrets in workspace files**: import replaces literal `-u` passwords / Bearer tokens with `{{password}}`/`{{token}}` placeholders (a value that is already a placeholder is kept verbatim, no warning); `save_endpoint` *and* `endpoint_to_toml` (the `churl import` stdout path — a redirected stdout is a workspace file too) refuse literal secret auth values. Secret-named fields: `password` and `token` always; apikey `value` only when its `name` looks secret (`looks_like_secret_name`).
- Precedence: an enabled user `Authorization` (or same-named apikey) header always beats the auth-injected header; a disabled one does not. Query-placed api keys are appended after enabled params; no precedence rule for query pairs (a same-named user param and the auth pair are both sent).
- **M4 → M5 migration**: M4-imported `Authorization: Basic <base64>` plain headers are left as-is (they still execute correctly); re-import the original curl command to get first-class auth. With multiple auth sources in one command, the first takes the first-class slot and the rest stay plain headers (warning emitted).
- No `{{var}}` resolution in M5 — placeholders are sent verbatim until M6's template resolver (auth fields are already in M6's substitution list).
- Basic and bearer round-trip curl export→import structurally; apikey exports to its wire form (header / URL query pair) and re-imports as a plain header/query — wire-equivalent by design, pinned in a test.
- The request pane shows a read-only auth line; `{{...}}` placeholders render verbatim, literal secret values render masked (`*****`), never raw.

**Open questions**: none

**Next**: M6

---

## M6 — Themes + keymaps + jump-mode + templating/profiles

**Scope**: User configuration surface.

**Deliverables**:
- Theme system: built-in (dark/light), user-override via config
- Keymap customisation: crokey map loaded from config; `churl keymaps` subcommand prints current map
- Jump-mode: letter-labelled pane/element navigation (à la EasyMotion/Helix `gw`)
- `churl-core::template`: `{{var}}` substitution through a single chain resolver — one function over an ordered scope list (the M9 plugin-guardrail seam). Precedence: CLI `--var` flag → active profile → collection vars → workspace vars → process env. Substitution applies to URL, query params, headers, auth fields (first-class since M5), and body (owner request 2026-07-05)
- Variable scopes (owner decision 2026-07-06): workspace-level `[vars]` in `churl.toml` (shared defaults) + named profiles in `churl.toml` (per-environment; profile beats collection so switching dev→prod always takes effect) + collection-level flat `[vars]` table in the collection's `folder.toml` (the manifest filename reserved since M1 — `persistence::FOLDER_FILENAME`; environment-independent collection defaults, no per-collection profiles). All three scopes ship in M6; `folder.toml` gets the same format-preserving merge writes + secrets name-marker enforcement as `churl.toml`
- `--var key=value` CLI flag
- Tests: template substitution unit tests incl. full five-scope precedence chain; `folder.toml` round-trip + secrets refusal; keymap round-trip

**Verified by**: `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all` (194 tests) all green; `cargo run -p churl -- --version` works; `cargo run -p churl -- keymaps` prints the effective map. New tests (39): `template.rs` unit suite (five-scope precedence each-beats-below, env last, unresolved/malformed left verbatim, multiple occurrences, inner-whitespace trim, `substitute_request` hits url/header-value/param-value/body/all auth kinds and *not* names); config (`theme_colors` parse, workspace-`vars`/collection secret-violation flagging); persistence integration (`workspace [vars]` round-trip + secret refusal, `folder.toml` comment-preserving round-trip, missing-`folder.toml` ⇒ default, collection secret refusal); theme (`resolve` built-in selection, named + hex overrides win, unknown built-in/slot/colour errors); jump (`JumpState` label assignment — panes first, rows follow, alphabet exhaustion — plus app-level routing: pane-label focus, row-label focus+select, `Esc` cancels, unknown char ignored); events (`iter`/`combos_for` coverage, `f`→Jump default); app (`with_config` unknown-profile error, resolver profile-beats-workspace, profile-picker sets active); CLI integration (`keymaps` default map + one overridden binding, `--var` bad-format error, `--profile` unknown-name error); jump-overlay insta snapshot (labels visible in the `TestBackend` text).

**Notes**:
- `Resolver` is the single `{{var}}` seam (`churl-core::template`): an ordered `Vec<Scope>` + `std::env::var` fallback; `substitute_request` runs in the TUI at send time on the cloned request only (`execute()`/`export_curl` stay substitution-free; resolved values never touch disk). Unresolved/malformed placeholders stay verbatim (M5's rule).
- Precedence: cli `--var` → active profile → collection `folder.toml` vars → workspace `[vars]` → env. `CollectionMeta` (in `folder.toml`) reuses the format-preserving `save_value` merge; secrets enforcement now covers workspace `[vars]` (prefixed `vars.`) and collection vars, alongside profiles and auth.
- Theme mirrors `[keys]`: core carries strings, the TUI `Theme` (in the `churl` crate) parses built-in `dark`/`light` + `[theme_colors]` slot overrides and fails loudly. Dark is the default and keeps every text snapshot byte-identical except the statusline hint (which gained `f jump`). Syntect follows the theme (Nord / InspiredGithub).
- Jump-mode is routing precedence slot 0 (overlay-level, `f` default); it labels panes then explorer rows *starting at the scroll offset* (main-session review fix: labelling from row 0 let offscreen rows eat the alphabet in a scrolled tree while the viewport went unlabelled), capping at the label alphabet. An assigned label wins over "Jump key again cancels" (the default `f` also labels the first row; `Esc` always cancels). `SwitchProfile` is palette-only.

**Deviations from the pinned design**:
- **`[theme_colors]` table, not `[theme.colors]`**: `theme` is a scalar config key (`theme = "dark"`), so a `[theme.colors]` sub-table collides with it in TOML. Used a flat top-level `[theme_colors]` table instead — same slot names and semantics.
- **`resolve` returns `Option<String>`, not `Option<&str>`**: the design's `resolve(&self) -> Option<&str>` can't borrow from the `std::env::var` fallback (it returns an owned `String`). Returning `Option<String>` keeps the env fallback live (never snapshotted) at the cost of a clone per scoped hit — negligible at send-time volume.

**Open questions**:
- ~~Variable scoping (owner question 2026-07-05): base URLs are meant to be profile vars (`url = "{{base_url}}/users"`, per-profile values). Does M6 also need a collection-level var scope (collection defaults overriding workspace profiles) in the precedence chain, or do profiles suffice? Decide before M6 starts.~~ **Resolved 2026-07-06 (owner)**: three scopes — workspace vars + profiles + collection-level flat overrides; profile wins over collection; full system incl. collection `folder.toml` vars lands in M6 (see deliverables + DECISIONS entry).

**Next**: M7

---

## M7 — Polish + perf + release

**Scope**: Performance validation, final UX touches, release preparation.

**Deliverables**:
- Cold-start benchmark: `hyperfine 'churl --help'` < 100 ms on reference hardware
- JSON folding in response viewer
- Full-screen response toggle (`F` key)
- **Response headers view**: toggle between body and full headers in the response pane (closes the M3 open question; count-only until then)
- **Wrap toggle** in the response viewer (closes the M3 horizontal-scroll open question — wrap chosen over horizontal scroll)
- **Response body search** (owner request 2026-07-05): `/`-style incremental search within the response viewer with match navigation — the explorer `/` fuzzy search never covered response bodies, and search beats folding for large payloads
- Highlight micro-nits from the M3 review: skip re-enqueueing a highlight job already in flight for the same viewport hash; strip `\r` from CRLF bodies in the line index
- README: install, quickstart, feature matrix, screenshot
- `cargo publish` dry-run passes for both crates
- GitHub release action (tag-triggered), building per-platform binaries: macOS arm64 + x86_64, Linux x86_64 **musl static** + aarch64, Windows x86_64 (owner requirement 2026-07-06: installable without Rust — rustls + bundled SQLite already make the binary self-contained)
- **`curl | sh` installer** (owner request 2026-07-06): `install.sh` in the repo, served via the release — detects OS/arch, downloads the matching release binary, installs to `~/.local/bin` (prompting/`--to` for override). `cargo install churl` remains the Rust-user path
- **`?` help overlay** (owner request 2026-07-06): in-app overlay rendering the effective keymap (reuses M6's `KeyMap::iter` — the `churl keymaps` output as an overlay)
- **`churl tutorial` onboarding** (owner request 2026-07-06): scaffolds a demo workspace (commented `churl.toml` with a profile + vars, one collection with `folder.toml`, an example endpoint against a public echo API) so a first-time user sends a request in under a minute; README quickstart mirrors it

**Next**: ship 0.1, then M8

---

## M8 — Cookies + proxy

**Scope**: Session and network-environment support (promoted from the backlog, owner decision 2026-07-05 — first post-release milestone).

**Deliverables**:
- **Cookie jar**: opt-in per workspace (`cookies = true` in `churl.toml`); reqwest cookie store enabled on the client; `Set-Cookie` responses carried into subsequent requests. Persistent cookies live in the SQLite state DB (the day-one ARCHITECTURE decision — never in workspace files); a `churl cookies` subcommand (or palette action) lists/clears the jar
- **Proxy configuration**: `proxy` knob in global config (URL, applies to http+https), overriding the already-honoured `HTTP_PROXY`/`HTTPS_PROXY` env vars; per-workspace override in `churl.toml`
- **Insecure-TLS opt-in**: explicit `insecure = true` (global or per-workspace) for local intercepting proxies (Charles/mitmproxy); curl import's `-k` maps to a warning pointing at the knob instead of "always ignored"; export emits `-k` when set
- Tests: wiremock cookie round-trip, proxy config plumbing, `-k` import/export remap

**Next**: M9

---

## M9 — Plugin system

**Scope**: Community extensibility (owner request 2026-07-05 — deliberately last: the plugin API freezes the shapes everything M5–M8 stabilises).

**Deliverables** (design session first; tech choice is an open question below):
- Plugin runtime + discovery (`~/.config/churl/plugins/`), enable/disable via config; a broken plugin fails loudly and never takes the app down
- Extension points, in priority order: ① request/response middleware (pre-send mutate, post-receive inspect), ② custom importers/exporters (beyond curl), ③ template functions (into the M6 `{{var}}` chain), ④ custom auth kinds (beyond M5's basic/bearer/api-key), ⑤ palette commands
- Plugin manifest (name, version, API version, capabilities) + compatibility check on load
- Docs: plugin authoring guide + a worked example plugin
- Tests: a fixture plugin exercising each extension point; load-failure isolation

**Open questions**:
- Runtime tech (decide in the M9 design session): embedded Lua (mlua — the lazygit/wezterm route), WASM (extism/wasmtime — sandboxed, language-agnostic, heavier), or a subprocess protocol (simplest, slowest per call). The standing "no JS runtime" decision (DECISIONS.md, binary-bloat rationale) excludes Deno/JS regardless.

**Plugin-readiness guardrails — ACTIVE FROM M5** (the "act early" half of the owner request; every milestone session must respect these so M9 doesn't require re-architecting):
- **M5 (auth)**: apply auth through a single dispatch point (one `apply_auth(...)` seam in core, match on auth kind there) — a future plugin-provided auth kind slots into that match, not into scattered call sites.
- **M6 (templating)**: route all `{{var}}` resolution through one resolver function that takes a name → value lookup — plugin template *functions* later extend that lookup. Keep palette commands data-driven (id/label/action entries in one table), never hardcoded match arms spread across the TUI.
- **M7 (viewer polish)**: keep content-type → formatter/highlighter selection in the single existing mapping point (`SyntaxToken::from_content_type`); JSON folding and wrap must not fork per-format code paths.
- **Always**: anything a plugin would touch flows through `churl-core` types (`Request`/`Response`/`Endpoint`) — they are the de-facto plugin API, so treat their serde shapes as stable; `execute()` stays the single HTTP chokepoint so middleware has exactly one place to wrap.

**Next**: ship follow-ups from backlog

---

## Post-release backlog (owner requests, 2026-07-05)

Not yet scheduled into milestones; each becomes an M9+ milestone (or folds into an existing one) when picked up.

- ~~Auth types~~ → **promoted to milestone M5** in the 2026-07-05 plan review (OAuth2 client-credentials remains here as backlog).
- **Request sequences (API E2E testing)** — run endpoints in a defined order; extract values from a response (JSONPath or similar) into variables consumed by later requests. Depends on M3 execution + M6 templating (extracted values enter the same `{{var}}` chain). Sequence definitions live in the workspace as TOML (same file-per-unit, `seq`-ordered philosophy).
- **Concurrent requests (throttle / race-condition testing)** — fire N copies of one endpoint (or several endpoints) concurrently; report per-request status/timing side by side to expose rate limits and race bugs. Builds directly on M3's task-per-request + `AbortHandle` architecture; needs a results-comparison view.
- ~~Cookies / sessions~~ → **promoted to milestone M8** (owner decision 2026-07-05).
- ~~Proxy configuration + per-request TLS-skip~~ → **promoted to milestone M8** (owner decision 2026-07-05).
- **Multipart (`-F`) bodies** (approved, owner decision 2026-07-06 — resolves the M4 open question): multipart/form-data as a model feature — multi-part body (fields + file refs), TUI body-type editing, reqwest multipart execution, `-F` import/export remap replacing the hard `Unsupported` error. Slot after M8.

### Deferred nits (from M2/M3 reviews)

- ~~Explorer pane has no scroll offset — a tree taller than the pane runs off-screen.~~ **Fixed in M3** (`ExplorerState::scroll_to_fit` keeps the selection in the viewport).
- ~~Ctrl-C/Ctrl-S consumed by edtui in insert mode.~~ → **Scheduled into M4** (owner decision 2026-07-05: intercept both before edtui — they are not text-input keys).
- Highlight job re-enqueued while an identical job is in flight; CRLF bodies keep `\r` in the line index → **scheduled into M7** polish.
