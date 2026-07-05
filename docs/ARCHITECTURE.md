# churl — Architecture

## Crate layout

### `churl-core` (library)

Zero TUI dependencies — ever. This constraint is enforced by code review and CI; adding `ratatui`/`crossterm`/`termion` to this crate is always wrong.

| Module | Responsibility |
|---|---|
| `model` | Core types: `Method`, `Endpoint`, `Request`, `Response`, `Header`, `Param` |
| `persistence` | TOML round-trip via `toml_edit` (format-preserving); lazy collection loading |
| `template` | Hand-rolled `{{var}}` substitution; precedence: CLI flag → profile → process env |
| `import` | curl command parsing (shlex + hand-rolled flag map) |
| `export` | curl command generation from `Endpoint` |
| `http` | Request execution via `reqwest` + `rustls`; coarse timing (connect + total); `AbortHandle` per in-flight request |
| `history` | SQLite via `rusqlite` (bundled); schema managed via migrations at startup |
| `config` | `~/.config/churl/config.toml` (incl. flat `[keys]` override strings) and per-workspace `churl.toml`; never contains secrets |

### `churl` (binary + thin lib)

The `lib.rs` target exists solely to let integration tests (`tests/`) import internal modules (primarily `tui`). The binary (`main.rs`) is thin: parse CLI, dispatch to subcommand or TUI.

| Module | Responsibility |
|---|---|
| `main` | `Cli` (clap derive); `Command` variants; `#[tokio::main]`; color-eyre hook installation |
| `tui` | Terminal init/restore; `run()` entry point (config → keymap → workspace → `App`) |
| `tui::app` | `App` state (`Pane` focus, `Mode` overlays, `AppMsg`); key routing; `tokio::select!` loop; top-level `render` |
| `tui::events` | `Action` enum + crokey `KeyMap` (defaults + `[keys]` config overrides); `FuzzyFinder` (nucleo-matcher) |
| `tui::components` | One module per pane/overlay: `explorer` (tree state machine), `request` (edtui body editor), `response` (M2 placeholder), `picker` (generic fuzzy overlay), `search`, `palette`, `statusline` |

### Event / render loop (M2+)

```
crossterm EventStream ──┐
tick timer (250 ms) ────┤──► tokio::select! ──► handle_key() ──► state mutation
app mpsc channel ───────┘                                     └──► terminal.draw()

HTTP requests (M3): spawned as tokio tasks, each with an AbortHandle.
Results arrive on the app mpsc channel as AppMsg::Response { .. }.
The render loop never awaits I/O.
```

`AppMsg` carries only `Redraw` until M3. All render functions are pure (`fn render(frame, area, &state)`), so snapshot tests drive them through a `TestBackend` without a tokio runtime.

Key routing precedence (per key event, in `Mode::Normal` the crokey map is authoritative):

1. An open overlay (search `/`, palette `:`) consumes everything — `Esc` closes, `Enter` accepts, `Up`/`Down`/`Ctrl-n`/`Ctrl-p` move, other chars edit the query.
2. Request pane focused with edtui in a non-Normal mode (insert/visual/…): all keys go to edtui.
3. Otherwise: crokey `KeyMap` lookup first; unmapped keys fall through to edtui when the request pane is focused. Navigation actions (`j`/`k`/`h`/`l`/`g`/`G`/`Enter`) forward their original key event to edtui when the request pane has focus, so vim motions keep working there.

## On-disk format

```
<workspace>/                    # a git repo the user owns
  churl.toml                    # workspace metadata + profiles (no secrets)
  <collection>/                 # a directory = a collection
    folder.toml?                # optional scoped metadata
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
```

Saves are format-preserving: comments and ordering in hand-edited files survive a churl round-trip (see DECISIONS.md for merge semantics and edge cases).

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
