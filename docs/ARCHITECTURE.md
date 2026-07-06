# churl — Architecture

## Crate layout

### `churl-core` (library)

Zero TUI dependencies — ever. This constraint is enforced by code review and CI; adding `ratatui`/`crossterm`/`termion` to this crate is always wrong.

| Module | Responsibility |
|---|---|
| `model` | Core types: `Method`, `Endpoint`, `Request`, `Response`, `Header`, `Param`, `Auth`/`ApiKeyPlacement` (M5: internally-tagged `[request.auth]`) |
| `auth` | `apply_auth(&Auth) -> AuthWire` — THE single dispatch point on auth kinds (M9 plugin guardrail); resolves basic/bearer/apikey to a `Header` or `Query` wire effect that `execute`/`export` apply without ever matching on `Auth` |
| `persistence` | TOML round-trip via `toml_edit` (format-preserving); lazy collection loading |
| `template` | Hand-rolled `{{var}}` substitution via a single chain resolver; precedence: CLI flag → active profile → collection vars (`folder.toml`) → workspace vars → process env (M6, owner decision 2026-07-06) |
| `import` | curl command parsing (shlex + hand-rolled flag map, M4): strict flag policy — unknown flags are hard errors, `-F`/`@file` are `Unsupported`, query stays in the URL; returns `ImportResult { endpoint, warnings }` |
| `export` | curl command generation from `Endpoint` (M4): shlex-quoted single line, enabled headers/params only; round-trip contract with `import` |
| `http` | Request execution via `reqwest` + `rustls`; coarse timing (`total` only, `connect` stays `None`); `execute(client, request, &ExecuteOptions)` is a plain runtime-agnostic `async fn` — cancellation is task-level in the TUI (`tokio::spawn` + `AbortHandle`), never in core. Body streamed chunk-wise up to `max_body_bytes` (default 10 MB) → `Response.truncated`; `build_client(timeout)` takes the config-resolved timeout. Auth injected via `auth::apply_auth` (M5): header effects skipped when an enabled user header with the same name exists (the user's header wins), query effects appended after enabled params. No `{{var}}` templating (M6); URL/headers/body/auth used verbatim |
| `history` | SQLite via `rusqlite` (bundled); schema managed via migrations at startup |
| `config` | `~/.config/churl/config.toml` (incl. flat `[keys]` override strings, `timeout_secs`, `max_body_bytes` — resolved via `Config::timeout()`/`Config::max_body_bytes()`) and per-workspace `churl.toml`; never contains secrets. Secrets heuristics: `looks_like_secret_name`, `is_template_placeholder`, `secret_violations` (manifest), `auth_secret_violations` (endpoint auth, M5) |

### `churl` (binary + thin lib)

The `lib.rs` target exists solely to let integration tests (`tests/`) import internal modules (primarily `tui`). The binary (`main.rs`) is thin: parse CLI, dispatch to subcommand or TUI.

| Module | Responsibility |
|---|---|
| `main` | `Cli` (clap derive); global `--var key=value` (repeatable) + `--profile` args; `Command` variants (`import` since M4; `keymaps` since M6: prints the effective keymap sorted by action name, `(default \| overridden)`); `#[tokio::main]`; color-eyre hook installation |
| `tui` | Terminal init/restore; `run(cli_vars, profile)` entry point (config → keymap → theme → workspace → `App::with_config`; unknown profile / bad theme error before the alternate screen) |
| `tui::app` | `App` state (`Pane` focus, `Mode` overlays incl. `Jump`, `AppMsg`, active profile, cli vars, `Theme`, `tick_count`); key routing; send-time `{{var}}` resolution (`build_resolver` → `Resolver::substitute_request` on the cloned request only); `tokio::select!` loop; top-level `render` — two-column layout: Explorer (left) + three-row column B: URL bar (display-only) / Request / Response |
| `tui::events` | `Action` enum (+ `Jump`, `SwitchProfile`) + crokey `KeyMap` (defaults + `[keys]` config overrides; `KeyMap::iter`/`combos_for` for the `keymaps` subcommand); `FuzzyFinder` (nucleo-matcher) |
| `tui::theme` | `Theme` (named style slots) parsed from core config strings, mirroring `[keys]`: built-in `dark`/`light` + `[theme_colors]` per-slot overrides (named ANSI or `#rrggbb`); fails loudly on unknown built-in/slot/colour. Core stays TUI-free (carries strings only) |
| `tui::highlight` | Off-thread syntect worker: a dedicated `std::thread` owns the `SyntaxSet`/theme (lazy-loaded on first job), receives `HighlightJob`s over `std::sync::mpsc`, returns `AppMsg::Highlighted`. Embedded theme follows the UI theme (Nord dark / InspiredGithub light). `SyntaxToken` (json/xml/html/plain) derives from the response `Content-Type` |
| `tui::components` | One module per pane/overlay: `explorer` (tree state machine, viewport scroll offset, cached `folder.toml` vars), `urlbar` (slim display-only strip: method/URL + auth kind + placeholder count; M6.5, not yet focusable — M6.6), `request` (edtui body editor), `response` (virtualised viewer: line-offset index + `ResponseState`/`ResponseView`), `picker` (generic fuzzy overlay), `search`, `palette`, `jump` (label assignment for jump-mode), `statusline` |

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
1. An open overlay (search `/`, palette `:`) consumes everything — `Esc` closes, `Enter` accepts, `Up`/`Down`/`Ctrl-n`/`Ctrl-p` move, other chars edit the query.
2. Request pane focused with edtui in a non-Normal mode (insert/visual/…): all keys go to edtui — except a CONTROL-modified key that the keymap resolves to `Send` or `Quit` (Ctrl-S / Ctrl-C by default), which is dispatched instead. The single documented exception (M4, DECISIONS.md): both are non-text keys, and the keymap lookup honours user remaps.
3. Otherwise: crokey `KeyMap` lookup first; unmapped keys fall through to edtui when the request pane is focused. Navigation actions (`j`/`k`/`h`/`l`/`g`/`G`/`Enter`) forward their original key event to edtui when the request pane has focus, so vim motions keep working there.

## On-disk format

```
<workspace>/                    # a git repo the user owns
  churl.toml                    # workspace metadata + profiles + workspace [vars] (no secrets)
  <collection>/                 # a directory = a collection
    folder.toml?                # optional collection metadata + flat [vars] defaults (M6)
    <endpoint>.toml             # one file per endpoint; explicit `seq` for ordering
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

Saves are format-preserving: comments and ordering in hand-edited files survive a churl round-trip (see DECISIONS.md for merge semantics and edge cases).

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
