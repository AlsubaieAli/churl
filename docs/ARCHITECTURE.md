# churl — Architecture

## Crate layout

### `churl-core` (library)

Zero TUI dependencies — ever. This constraint is enforced by code review and CI; adding `ratatui`/`crossterm`/`termion` to this crate is always wrong.

| Module | Responsibility |
|---|---|
| `model` | Core types: `Method`, `Endpoint`, `Request`, `Response`, `Header`, `Param`, `Auth`/`ApiKeyPlacement` (M5: internally-tagged `[request.auth]`) |
| `auth` | `apply_auth(&Auth) -> AuthWire` — THE single dispatch point on auth kinds (M9 plugin guardrail); resolves basic/bearer/apikey to a `Header` or `Query` wire effect that `execute`/`export` apply without ever matching on `Auth` |
| `persistence` | TOML round-trip via `toml_edit` (format-preserving, deletion-pruning `merge_tables`); lazy collection loading. `Collection::endpoints()` is strict; `endpoints_lenient() -> CollectionLoad { endpoints, warnings }` degrades one unparseable file to a warning (TUI load path). Both skip `folder.toml` **and** `churl.toml` (a nested-workspace manifest is not an endpoint). Sequences (M7.4): `SEQUENCES_DIRNAME` reserved dir (excluded from `collections()`); `OpenWorkspace::sequences() -> SequenceLoad { sequences, warnings }` (lenient); `load/save/create/rename/delete_sequence` CRUD seams |
| `sequence` | Request-sequence run engine (M7.4, UI-free): dependency-free extraction subset (`extract_value`: `status` / `header:<Name>` / JSON-path `$.a.b[0].c`) over `serde_json`; run primitives shared by tests and the live TUI — `prepare_step` (resolver with the extracted scope prepended highest), `extract_step`, `classify_step` (the single classify+extract seam), `ordered_steps`; `run_sequence` wiremock-tested convenience. Rejects `..`/absolute step endpoints; never panics |
| `load` | Concurrent-load / throttle runner (M7.5, UI-free): `run_load` fires N copies of an already-resolved `Request` through the single `execute` chokepoint, bounded by `futures`' `buffer_unordered` (== concurrency in flight) + absolute-target pacing; `classify` (`Ok`/`Failed`≥400/`Error`) is the single seam; pure `stats` (nearest-rank min/median/p95/max/mean over completed timings); `check_config`/`LoadCaps`/`LoadCheck` guardrail classifier. `run_load` is the wiremock-tested twin the TUI launcher mirrors. Uses `tokio::time`/`futures` internally; `execute` stays runtime-agnostic |
| `template` | Hand-rolled `{{var}}` substitution via a single chain resolver; precedence: CLI flag → active profile → collection vars (`folder.toml`) → workspace vars → process env (M6, owner decision 2026-07-06). Sequences prepend an ephemeral highest-precedence `extracted` scope (M7.4) — one extra scope, resolution never forked |
| `import` | curl command parsing (shlex + hand-rolled flag map, M4): strict flag policy — unknown flags are hard errors, `-F`/`@file` are `Unsupported`, query stays in the URL; returns `ImportResult { endpoint, warnings }` |
| `export` | curl command generation from `Endpoint` (M4): shlex-quoted single line, enabled headers/params only; round-trip contract with `import` |
| `http` | Request execution via `reqwest` + `rustls`; coarse timing (`total` only, `connect` stays `None`); `execute(client, request, &ExecuteOptions)` is a plain runtime-agnostic `async fn` — cancellation is task-level in the TUI (`tokio::spawn` + `AbortHandle`), never in core. Body streamed chunk-wise up to `max_body_bytes` (default 10 MB) → `Response.truncated`; `build_client(timeout)` takes the config-resolved timeout. Auth injected via `auth::apply_auth` (M5): header effects skipped when an enabled user header with the same name exists (the user's header wins), query effects appended after enabled params. No `{{var}}` templating (M6); URL/headers/body/auth used verbatim |
| `history` | SQLite via `rusqlite` (bundled); schema managed via migrations at startup. Migration 3 (M7.5) adds a SEPARATE `load_batches` table (`LoadBatchSummary` / `insert_load_batch` / `recent_load_batches`): a load run writes exactly ONE summary row there, never to `history`, so the per-endpoint history view is never flooded; migration 4 appends the `mean_ms` column via `ALTER TABLE` (migrations are append-only — never edit an applied one) |
| `config` | `~/.config/churl/config.toml` (incl. flat `[keys]` override strings, `timeout_secs`, `max_body_bytes` — resolved via `Config::timeout()`/`Config::max_body_bytes()`; the M7.5 `[load]` guardrail caps via `Config::load_caps()`) and per-workspace `churl.toml`; never contains secrets. Secrets heuristics: `looks_like_secret_name`, `is_template_placeholder`, `secret_violations` (manifest), `auth_secret_violations` (endpoint auth, M5) |

### `churl` (binary + thin lib)

The `lib.rs` target exists solely to let integration tests (`tests/`) import internal modules (primarily `tui`). The binary (`main.rs`) is thin: parse CLI, dispatch to subcommand or TUI.

| Module | Responsibility |
|---|---|
| `main` | `Cli` (clap derive); global `--var key=value` (repeatable) + `--profile` args; `Command` variants (`import` since M4; `keymaps` since M6: prints the effective keymap sorted by action name, `(default \| overridden)`); `#[tokio::main]`; color-eyre hook installation |
| `tui` | Terminal init/restore; `run(cli_vars, profile)` entry point (config → keymap → theme → workspace → `App::with_config`; unknown profile / bad theme error before the alternate screen) |
| `tui::app` | `App` state (`Pane` focus incl. `UrlBar`, `Mode` overlays incl. `Jump`/`MethodMenu`/`Prompt`/`Confirm`/`EnvEditor` (the M7.3 env editor, holding `env_editor: Option<EnvEditorState>`)/`SequenceRunner`/`SequenceEditor` (M7.4, holding `sequence_runner`/`sequence_editor`; `AppMsg::SequenceStep` + `sequence_abort` drive the live run)/`LoadRunner` (M7.5, holding `load_runner: Option<LoadRunnerState>` + `load_request`/`load_caps`; `AppMsg::LoadStarted`/`LoadResult` + the single `load_abort` launcher drive the live batch), `RequestTabs`, `loaded_snapshot` for derived dirty, inline `url_editor`/`prompt_editor`, `AppMsg`, active profile, cli vars, `Theme`, `tick_count`); key routing via `lookup_ctx(key, pane.ctx())`; inline `LineEditor` editing (URL bar + request rows + CRUD prompts); in-app CRUD via `churl-core::persistence` seams; send-time `{{var}}` resolution; `tokio::select!` loop; top-level `render` — two-column layout: Explorer (left) + three-row column B: focusable URL bar / Request (tabs) / Response |
| `tui::events` | `Action` enum (+ `Jump`, `SwitchProfile`, and the M6.6 URL-bar/tab/row/CRUD/save actions) + crokey `KeyMap` with per-pane `overlays` (`PaneCtx ∈ {Explorer, UrlBar, Request, Response}`; `lookup_ctx` = overlay-then-global) built from `[keys]` + `[keys.<pane>]` config; `leader_root` (root leader continuations, `LeaderEntry::{Act, Submenu(name)}`) + **data-driven** `submenus: HashMap<String, Submenu{title, binds}>` (built-in `sequences`/`load`/`tabs` seeded as defaults; `[keys.leader.<name>]` creates/extends any submenu — M7.10); `validate(global, overlays) -> Vec<String>` load-time conflict/shadow warnings (leader-as-action, dangling/orphan submenu, in-scope duplicate combo, globally-shadowed global); `iter`/`combos_for`/`leader_combos_for` (+ overlay variants) for the `keymaps` subcommand; `FuzzyFinder` (nucleo-matcher) |
| `tui::theme` | `Theme` (named style slots) parsed from core config strings, mirroring `[keys]`: built-in `dark`/`light` + `[theme_colors]` per-slot overrides (named ANSI or `#rrggbb`); fails loudly on unknown built-in/slot/colour. Core stays TUI-free (carries strings only) |
| `tui::highlight` | Off-thread syntect worker: a dedicated `std::thread` owns the `SyntaxSet`/theme (lazy-loaded on first job), receives `HighlightJob`s over `std::sync::mpsc`, returns `AppMsg::Highlighted`. Embedded theme follows the UI theme (Nord dark / InspiredGithub light). `SyntaxToken` (json/xml/html/plain) derives from the response `Content-Type` |
| `tui::components` | One module per pane/overlay: `explorer` (tree state machine, viewport scroll offset, cached `folder.toml` vars, CRUD-support accessors + `reload`/`select_file`, lenient-load `take_warnings`), `urlbar` (focusable strip: method/URL + inline edit cursor + `●` dirty dot + auth kind + placeholder count), `line_editor` (shared single-line editor — no dep), `request` (tab bar + Params/Headers/Auth row lists + edtui Body), `request_tabs` (pure tab/row/field-edit state machine), `response` (virtualised viewer + M7 display pipeline: cursor, headers view, wrap, JSON folding, body search, copy), `fold` (string-aware JSON fold-region scanner), `env_editor` (M7.3 environments & vars editor: `EnvEditorState` split-view over workspace/collection/profile var scopes — profile CRUD, dirty/discard guard, secret mask+refuse, live precedence display; all UI state here, core stays UI-free), `picker` (generic fuzzy overlay), `method_menu` (one-key method picker), `prompt` (CRUD text prompt + y/n confirm overlays), `search`, `palette` (curated command allowlist), `jump`, `statusline`, `sequence_runner` (M7.4 live run view — reuses `response::render`, UI-only), `sequence_editor` (M7.4 §4 — steps + extraction-rule CRUD + reorder, saves via `save_sequence`), `load_runner` (M7.5 concurrent-load runner: editable config header (total/concurrency/interval via the M7.3 LineEditor field-row pattern) + live O(viewport) results list + reused `response::render` viewer + live stats line; UI-only — `App` owns the single `buffer_unordered` launcher + abort handle + generation guard + guardrail. **R0 memory bound**: retention is O(concurrency + K) — a `live_views` deque keeps full `ResponseState::Done` views only for the last `LIVE_VIEW_WINDOW`=16 completions + the selected row; overflow downgrades the oldest non-selected row to `ResponseState::Dropped { status, timing, size }` (no body bytes, not reconstructable), rendered as a "not retained" placeholder. Stats over all outcomes are unaffected) |
| `tui::clipboard` | Clipboard writes via OSC 52 (`ESC ] 52 ; c ; <base64> BEL`) to ratatui's terminal backend writer — no native clipboard dep (works over SSH/tmux; macOS Terminal.app ignores it). 1 MB payload cap |

### Event / render loop (M2+)

```
crossterm EventStream ──┐
tick timer (250 ms) ────┤──► tokio::select! ──► handle_key() ──► state mutation
app mpsc channel ───────┘                                     └──► terminal.draw()

HTTP requests (M3): spawned as tokio tasks, each with an AbortHandle.
Results arrive on the app mpsc channel as AppMsg::Response { .. }.
The render loop never awaits I/O.
```

Since M3 `AppMsg` also carries `Response { generation, outcome, meta }` (a stale generation — after cancel/resend — is dropped) and `Highlighted { hash, lines }`. Render functions stay pure (`fn render(frame, area, &state)`); the response pane's `render` additionally *returns* an optional `HighlightJob` for the caller to enqueue (top-level `render` already takes `&mut App`). Under `TestBackend` no highlight worker exists, so snapshots deterministically show plain text.

Key routing precedence (per key event, in `Mode::Normal` the crokey map is authoritative):

0. Jump-mode (`f` by default) is an overlay-level mode alongside search/palette: it consumes every key — a label char focuses that pane / selects that explorer row, `Esc` (or the Jump key again) cancels, everything else is ignored.
1. Open overlays consume everything: search `/` + palette `:` (`Esc` closes, `Enter` accepts, `Up`/`Down`/`Ctrl-n`/`Ctrl-p` move, chars edit the query); the method menu (`M`, a label key picks); CRUD prompts (`Mode::Prompt`, a `LineEditor` line; `Enter` commits, `Esc` cancels); and confirms (`Mode::Confirm`; `y/n` for endpoint delete, `s`/`d`/`Esc` for discard-changes).
2. Inline editors in `Mode::Normal` intercept next: the URL bar editor (`url_editor`) and request-row field edits (`RequestTabs::editing`) each own the keyboard — except a CONTROL key the keymap resolves to `Send`/`Quit` (the M4 interception, generalised to `LineEditor`). `Enter` commits, `Esc` cancels.
3. Request pane, Body tab, edtui in a non-Normal mode: keys go to edtui with the same Ctrl-S/Ctrl-C interception (M4, DECISIONS.md).
4. Otherwise: `KeyMap::lookup_ctx(key, focused_pane.ctx())` — the focused pane's overlay wins over the global map (so `1`–`4` are tab jumps in the Request pane but pane-focus globally). Unmapped keys fall through to edtui only on the Body tab; navigation actions forward their key event to edtui there so vim motions keep working.

The Explorer pane overlay binds the arrow keys to navigation (`Up`/`Down` → Up/Down, `Left`/`Right` → Collapse/Expand), mirroring the global `k`/`j`/`h`/`l` (M7.10 follow-up). They are Explorer-scoped, not global, so Left/Right never leak Collapse/Expand into other panes; the flat sequences sub-pane no-ops Collapse/Expand, so they are harmless there.

The find/open pickers hang off the leader key as its own continuation (M7.10 follow-up): `<leader><leader>` (Space Space) opens the endpoint/request picker, `<leader>s <leader>` the sequence picker, `<leader>l <leader>` the load-runner endpoint picker. `f` is freed at root for jump-mode. Space as a leader *continuation* is not flagged by `validate` (which only checks the leader key against the global map + pane overlays).

The Response pane (M7) adds a `[keys.response]` overlay (`h` headers · `W` wrap · `/` search · `n`/`N` match nav · `o`/`O` fold · `y`/`Y` copy) and a new keyboard-owning overlay mode `Mode::BodySearch` (routed like the other overlays; the incremental `/query` input renders in the message-row position via the shared `LineEditor`). The response `[h]` headers-hint is focus-gated — it shows only when its response pane (main, sequence-runner, or load-runner) is focused.

### Response viewer pipeline (M7)

The response viewer composes three pure transforms over the logical lines, evaluated fresh each render (`components/response.rs`):

```
logical lines (body or headers text, CRLF-stripped)
  → fold filter      (JSON-only; folded regions elided to a `⋯ N lines` header)
  → wrap expansion    (optional; each display row = a char sub-range of one logical line)
  → viewport slice    (scroll offset + height)
```

Cursor and scroll are **display-row** indices (post-fold, post-wrap); the cursor follows-and-scrolls at render time. Search matches are stored against *logical* lines (byte ranges) and mapped through the pipeline for navigation, auto-unfold, and highlight overlay. Per-view UI state (view mode, folds, wrap, search) lives on `ResponseView` so it resets on each new response; cursor/scroll/geometry live on `App`. Fold regions come from `components::fold::scan_regions` (single O(n) string-aware pass, cached lazily). Highlighting is deferred under wrap (wrapped bodies render plain); unwrapped bodies keep full off-thread syntect highlighting with a duplicate-enqueue guard (`App::pending_highlight`).

## On-disk format

```
<workspace>/                    # a git repo the user owns
  churl.toml                    # workspace metadata + profiles + workspace [vars] (no secrets)
  <collection>/                 # a directory = a collection
    folder.toml?                # optional collection metadata + flat [vars] defaults (M6)
    <endpoint>.toml             # one file per endpoint; explicit `seq` for ordering
  sequences/                    # reserved dir (M7.4) — request sequences, NOT a collection
    <sequence>.toml             # one file per sequence; ordered [[step]]s + [step.extract] rules
```

Sequence file shape (M7.4):

```toml
seq = 0
name = "Auth flow"
on_error = "halt"               # halt (default) | continue

[[step]]
seq = 0
endpoint = "auth/login.toml"    # workspace-relative endpoint path
[step.extract]
token = "$.data.token"          # var name -> extraction expression
user_id = "$.data.user.id"

[[step]]
seq = 1
endpoint = "users/me.toml"      # its request uses {{token}} — resolved from the extracted scope
```

Endpoint file shape (M1):

```toml
seq = 1                     # explicit ordering within the collection
name = "Get user"

[request]
method = "GET"
url = "https://api.example.com/users/{{id}}"

[[request.headers]]         # array-of-tables; `enabled = false` to disable a line
name = "Accept"
value = "application/json"

[request.body]              # optional; type = text|json|form (default text)
type = "json"
content = '{"q": true}'

[request.auth]              # optional (M5); type = basic|bearer|apikey
type = "basic"              # secret values must be {{var}} placeholders —
username = "alice"          # save_endpoint/endpoint_to_toml refuse literals
password = "{{password}}"
```

Saves are format-preserving: comments and ordering in hand-edited files survive a churl round-trip (see DECISIONS.md for merge semantics and edge cases). Every save is also **atomic and durable** (R0): the single write in `save_value` funnels through `persistence::atomic_write` (temp sibling file → fsync → atomic `rename` → parent-dir fsync), so a crash mid-write can never tear the source-of-truth file. The error contract is unchanged (`PersistenceError::Write`).

Workspace `churl.toml` and collection `folder.toml` (M6):

```toml
# churl.toml
name = "my-api"

[vars]                        # workspace-level template defaults (no secrets)
base_url = "https://api.example.com"

[[profiles]]                  # per-environment; profile vars beat collection vars
name = "prod"
[profiles.vars]
host = "prod.example.com"

# <collection>/folder.toml
[vars]                        # collection-level defaults, environment-independent
page_size = "50"
```

Global config `~/.config/churl/config.toml` (M6 adds theming):

```toml
theme = "dark"                # built-in: "dark" (default) | "light"

[theme_colors]                # per-slot overrides (named ANSI or #rrggbb)
title = "cyan"                # unknown slot / bad colour = loud startup error
jump_label = "#ffcc00"        # (table is [theme_colors], not [theme.colors] — see DECISIONS.md)

[keys]                        # action remaps: "combination" = "action-name"
"ctrl-p" = "open-palette"
```

High-churn state (history, cookies, cached responses) lives in:
```
$XDG_DATA_HOME/churl/state.sqlite
```
Never in the workspace — workspace = safe to commit/sync; state.sqlite = never committed.

Render-side caches (line-offset index, wrap layout, viewport-only syntect highlighting) are in-process only — not persisted.

## Performance budget

| Concern | Target | Mechanism |
|---|---|---|
| Cold start | < 100 ms | Lazy collection loading; lazy syntect init |
| 1 MB response body | Smooth scroll | Virtualised line render; line-offset index; viewport-only highlighting |
| Syntax highlighting | < 23 ms per viewport | `syntect` + `two-face` colours, off-thread, cached by viewport hash |

No HTTP-semantic caching by design — churl is a development tool; stale responses are a footgun. History covers recall.

## Tutorial scaffold (`churl tutorial`)

`crates/churl/src/tutorial.rs` — the `Tutorial` subcommand scaffolds a demo workspace so a first-time user can send a request in under a minute.

The scaffold writes through the real `churl-core::persistence` seams — no hand-written TOML strings for endpoint or folder files:

1. `save_workspace_manifest(root, &ws)` — writes `churl.toml` with `name`, `vars` (`base_url`), and a `dev` profile.
2. `create_collection(root, "examples")` — creates the collection directory.
3. `save_collection_meta(coll_dir, &Default::default())` — writes `examples/folder.toml` (empty vars).
4. Three `create_endpoint(coll_dir, name)` + `save_endpoint(path, &ep)` calls — writes the endpoints through the format-preserving merge serializer.

Endpoints target [httpbingo.org](https://httpbingo.org): Get Anything (GET `/anything?name=churl`), Post JSON (POST `/post` with JSON body), Bearer Auth (GET `/bearer` with `{{token}}` placeholder auth). The scaffold refuses a non-empty directory.

## Release pipeline

Tag-triggered GitHub Actions workflow (`.github/workflows/release.yml`):
- `taiki-e/upload-rust-binary-action@v1` — cross-compiles, strips, archives, and uploads per-target binaries with SHA-256 checksums.
- Targets: `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-musl` (static), `aarch64-unknown-linux-musl` (static), `x86_64-pc-windows-msvc`.
- musl static targets are viable because the HTTP stack is rustls (pure Rust TLS) and SQLite is bundled — no system-lib dep.

`install.sh` (repo root, POSIX sh) — the `curl|sh` installer: detects OS/arch → release target triple, downloads the `.tar.gz` + `.sha256` from GitHub Releases, verifies checksum, extracts to `~/.local/bin`. Options: `--to DIR`, `--force`, `--dry-run`.
