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
| `config` | `~/.config/churl/config.toml` and per-workspace `churl.toml`; never contains secrets |

### `churl` (binary + thin lib)

The `lib.rs` target exists solely to let integration tests (`tests/`) import internal modules (primarily `tui`). The binary (`main.rs`) is thin: parse CLI, dispatch to subcommand or TUI.

| Module | Responsibility |
|---|---|
| `main` | `Cli` (clap derive); `Command` variants; color-eyre hook installation |
| `tui` | Terminal init/restore; render loop; top-level `App` state |
| `tui::components` | Ratatui component tree: Explorer pane, Request editor (edtui), Response viewer, Command palette |
| `tui::events` | `Key→Action` semantic map (crokey); fuzzy search via nucleo |
| `tui::app` | `App` struct; `tokio::select!` loop multiplexing crossterm `EventStream` / tick timer / `mpsc` app-message channel |

### Event / render loop (M2+)

```
crossterm EventStream ──┐
tick timer ─────────────┤──► tokio::select! ──► handle() ──► state mutation
app mpsc channel ───────┘                                  └──► terminal.draw()

HTTP requests: spawned as tokio tasks, each with an AbortHandle.
Results arrive on the app mpsc channel as AppMsg::Response { .. }.
The render loop never awaits I/O.
```

M0 uses a plain synchronous `crossterm::event::read()` loop — async arrives in M2.

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
