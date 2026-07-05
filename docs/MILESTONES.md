# churl — Milestones

## Status overview

| Milestone | Name | Status |
|---|---|---|
| M0 | Skeleton + CI | **done** |
| M1 | Data model + persistence | **done** |
| M2 | Layout + navigation | **done** |
| M3 | Request execution + response render | **done** |
| M4 | curl import / export + M3 follow-ups | **done** |
| M5 | Auth | planned |
| M6 | Themes + keymaps + jump-mode + templating | planned |
| M7 | Polish + perf + release | planned |

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
- Multipart (`-F`) import: the data model has no multipart body. Reject-with-error is the M4 behaviour — should multipart become a model feature (own milestone or M7 backlog), or stay permanently unsupported? Owner call.

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

**Next**: M6

---

## M6 — Themes + keymaps + jump-mode + templating/profiles

**Scope**: User configuration surface.

**Deliverables**:
- Theme system: built-in (dark/light), user-override via config
- Keymap customisation: crokey map loaded from config; `churl keymaps` subcommand prints current map
- Jump-mode: letter-labelled pane/element navigation (à la EasyMotion/Helix `gw`)
- `churl-core::template`: `{{var}}` substitution with precedence chain; `--var key=value` CLI flag; named profiles in `churl.toml`. Substitution applies to URL, query params, headers, auth fields (first-class since M5), and body (owner request 2026-07-05)
- Tests: template substitution unit tests; keymap round-trip

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
- Highlight micro-nits from the M3 review: skip re-enqueueing a highlight job already in flight for the same viewport hash; strip `\r` from CRLF bodies in the line index
- README: install, quickstart, feature matrix, screenshot
- `cargo publish` dry-run passes for both crates
- GitHub release action (tag-triggered)

**Next**: ship

---

## Post-release backlog (owner requests, 2026-07-05)

Not yet scheduled into milestones; each becomes an M8+ milestone (or folds into an existing one) when picked up.

- ~~Auth types~~ → **promoted to milestone M5** in the 2026-07-05 plan review (OAuth2 client-credentials remains here as backlog).
- **Request sequences (API E2E testing)** — run endpoints in a defined order; extract values from a response (JSONPath or similar) into variables consumed by later requests. Depends on M3 execution + M6 templating (extracted values enter the same `{{var}}` chain). Sequence definitions live in the workspace as TOML (same file-per-unit, `seq`-ordered philosophy).
- **Concurrent requests (throttle / race-condition testing)** — fire N copies of one endpoint (or several endpoints) concurrently; report per-request status/timing side by side to expose rate limits and race bugs. Builds directly on M3's task-per-request + `AbortHandle` architecture; needs a results-comparison view.

### Deferred nits (from M2/M3 reviews)

- ~~Explorer pane has no scroll offset — a tree taller than the pane runs off-screen.~~ **Fixed in M3** (`ExplorerState::scroll_to_fit` keeps the selection in the viewport).
- ~~Ctrl-C/Ctrl-S consumed by edtui in insert mode.~~ → **Scheduled into M4** (owner decision 2026-07-05: intercept both before edtui — they are not text-input keys).
- Highlight job re-enqueued while an identical job is in flight; CRLF bodies keep `\r` in the line index → **scheduled into M7** polish.
