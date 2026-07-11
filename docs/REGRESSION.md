# churl — Manual Regression Checklist

A living, human-run smoke test seeded from the demo drive-script (D1). Run it each
round against a **freshly built binary** — the round-1 demo bugs traced to a stale
binary, so the built commit is recorded below every pass.

**How to use**
- Build fresh (`cargo build --release`), record the commit + date, then walk every
  item. Each `- [ ]` is one manual check with its expected result.
- Tick items that pass; leave a note on any that fail. A failing item is a
  regression — file it before shipping.
- Keep this in sync with new surfaces: when a milestone adds a pane/flow, add its
  checks here in the same PR.

**Built commit:** `<sha>` · **Date:** `<YYYY-MM-DD>`

---

## Explorer / Sequences nav
- [ ] Launch on a workspace with sequences → Explorer is zoomed, Sequences peeks as a stub (never full-height, never absent).
- [ ] Launch on a zero-sequence workspace → Sequences stub shows the `<leader>s a to add` affordance.
- [ ] `s` (explorer overlay) switches focus/zoom Explorer⇄Sequences; the unfocused pane stays a peeking stub, never vanishes.
- [ ] `<leader>S` switches focus Explorer⇄Sequences and never hides the Sequences pane (interim focus-switch).
- [ ] `<leader>e` toggles the entire left column (hides/shows), restoring prior focus on hide.
- [ ] Arrow / `j`/`k` move the Explorer cursor; `←`/`→` (and `h`/`l`) collapse/expand a collection — arrows navigate the explorer, not just the runners/pickers.
- [ ] From a focused Sequences sub-pane, `f` then `e` reaches the **endpoints** tree (not stranded on Sequences).

## Picker nav (Search · Sequence · Workspace · Palette)
- [ ] Open each picker; Up/Down move the highlight.
- [ ] Ctrl-p / Ctrl-n move up/down in every picker.
- [ ] `j` / `k` move up/down in every picker.
- [ ] Type to fuzzy-filter; Enter accepts the highlighted row; Esc cancels with no side effect.
- [ ] Esc-cancelling a `<leader>s r` / `<leader>l <leader>` pick does not leak intent into the next `<leader><leader>` search.
- [ ] `<leader><leader>` opens the endpoint/request picker; `<leader>s <leader>` the sequence picker; `<leader>l <leader>` the load-runner endpoint picker (all on the leader-as-continuation gesture).

## Sequences run / edit
- [ ] `<leader>s <leader>` opens the "Open sequence" picker; accepting opens the chosen sequence in the Edit face.
- [ ] `<leader>s r` opens the "Run sequence" picker; accepting runs the **chosen** sequence (not sequence #0 / last-run).
- [ ] In-pane `r` on the hovered sequence still runs it directly.
- [ ] `<leader>s a` adds a new sequence.
- [ ] A sequence run shows live per-step status/timing; extracted secret values are masked.

## Load runner
- [ ] `<leader>l c` opens the load runner on the loaded endpoint; `<leader>l <leader>` picks an endpoint first.
- [ ] Edit config header (total / concurrency / interval); `Ctrl-R` starts/re-runs the run (plain `r` does nothing) → live results list + stats update.
- [ ] Cancel a running batch → launched-then-cancelled rows show a real `{}ms` time-to-cancel next to the cancelled glyph.
- [ ] Never-launched (pending) rows show a blank duration (no fabricated zero).
- [ ] The run writes exactly one summary row to `load_batches` (not per-copy history).
- [ ] A large run (e.g. total 500) stays memory-bounded — scrolling to old completed rows shows a "response body not retained (memory-bounded)" placeholder for evicted rows, while the last ~16 + the selected row show real bodies; stats (ok/failed/percentiles) are correct over the whole run. (R0)

## Env editor
- [ ] `<leader>v` opens the environments & vars editor; scope list + rows render with live precedence.
- [ ] Add a normal var, save → written; reload shows it.
- [ ] Add a secret-named literal (e.g. `api_token = leaked`), save → refused with a message naming the var and the "pre-existing" phrasing.
- [ ] A `{{placeholder}}` secret-named value saves fine.
- [ ] Duplicate var name in one scope → refused before writing.
- [ ] Dirty-discard guard prompts on leaving with unsaved edits.

## Response viewer
- [ ] Send a request → response renders; cursor / headers toggle / wrap / fold / search / copy all work.
- [ ] Large body is truncated with the truncation status; folding JSON regions works.
- [ ] The `[h]` headers hint appears in the response summary **only when the response pane is focused** — unfocused (incl. a collapsed stub or an embedded sequence/load-runner response that isn't the focused sub-pane) omits it.
- [ ] A minified single-line JSON response (e.g. petstore) arrives **pretty** (multi-line, indented) by default, not on one line. (M7.7)
- [ ] `p` toggles raw↔pretty; in raw mode the body shows the **exact on-the-wire bytes**, and toggling back and forth is stable. Toggling resets any open folds. (M7.7)
- [ ] A malformed/non-JSON body with a JSON content-type renders **raw with no error or crash** (silent fallback). (M7.7)
- [ ] Copy body (`y`) / copy line (`Y`) copy the **raw bytes** even while the body is displayed pretty — pasting round-trips the server's actual JSON. (M7.7)
- [ ] `s` on a pretty JSON body toggles **A→Z key sort** — every object's keys sort alphabetically at every nesting level; arrays keep element order. (M7.7)
- [ ] With sort **off**, pretty JSON keeps the **server wire order** (keys in response order, not alphabetical). (M7.7)
- [ ] `s` on a raw or non-JSON view **no-ops with a notice** (`sort: pretty JSON only`), no text change. (M7.7)
- [ ] Copy body (`y`) returns the **exact raw on-the-wire bytes** even with pretty + sort both on. (M7.7)

## Help overlay
- [ ] `?` opens the help overlay; it renders every section (Global / Explorer / URL bar / Request / Response / Leader) and `j`/`k`/`d`/`u` scroll; `?`/`Esc`/`q` close.
- [ ] `/` inside the help overlay opens a search; typing a substring of a known binding label **highlights matches in place and jumps to the current one** — **non-matching rows stay visible** (highlight-and-jump, not a filter). The title shows the query + a `k/N` counter. (M7.7 Stage B)
- [ ] `n`/`N` cycle to the next/previous match and **wrap** around; the view scrolls the current match into view. (M7.7 Stage B)
- [ ] `Esc` in help search **clears the search but keeps the overlay open**; `Enter` **commits** (matches stay highlighted, input closes, `n`/`N` still cycle). (M7.7 Stage B)
- [ ] Smart-case parity with body search: a lowercase query matches case-insensitively; a query with any uppercase char is case-sensitive. (M7.7 Stage B)

## Persistence durability
- [ ] Edit + save an endpoint, then re-open it — the change persisted and comments/ordering survived (atomic saves must not regress the format-preserving round-trip). (R0)
- [ ] Save leaves no `.<name>.<pid>.<n>.tmp` sibling files behind in the collection/workspace directory. (R0)
- [ ] Create an endpoint named `churl` (or `Churl`/`folder`) — it lands on `churl-2.toml`/`folder-2.toml` (NOT `churl.toml`), the workspace manifest survives intact, the note is findable, and the confirmation says `created churl (as churl-2 — name was reserved)`. Display name stays `churl`. (R1 D1)
- [ ] Create a collection named `sequences` — it lands on `sequences-2/` and appears in the tree (the reserved `sequences/` dir is not shadowed). A collection named `churl`/`folder` is fine (those are files). (R1 D1)
- [ ] Rename an endpoint *to* `churl` — same disambiguation as create; old file gone, new file on a `-2` stem. (R1 D1)
- [ ] Hand-add a `# comment` to the middle entry of a `[[request.headers]]` array, then add/remove a header in the TUI and save — the surviving entries' comments/whitespace/order are byte-preserved (array merge is element-wise, not wholesale). (R1 D3)

## State durability / concurrency (SQLite)
- [ ] After first launch, `<data_dir>/churl/state.sqlite-wal` exists — the history DB is in WAL mode. (R1 D2)
- [ ] Open two churl processes on the same machine (shared `state.sqlite`); both start and record history without a `database is locked`/`SQLITE_BUSY` error (busy_timeout + WAL + the `BEGIN IMMEDIATE` migration lock). (R1 D2)
- [ ] A very long / high-`total` load run stays responsive and does not grow `state.sqlite` without bound (prune-on-insert caps `history`/`load_batches` at 10 000 rows each; the bounded app channel applies backpressure). (R1 D2/D4a/D4b)
- [ ] Open several endpoint buffers, scroll each response, then cycle through the tabs — memory stays flat (inactive buffers' highlight caches are evicted on switch). (R1 D4c)

## Import / export
- [ ] `churl import "curl ..."` produces the expected endpoint.
- [ ] Export the loaded request as a curl one-liner (`<leader>y`); re-importing round-trips.

## Clipboard
- [ ] Copy actions write via OSC 52 (works over SSH/tmux); >1 MB payload is capped.
