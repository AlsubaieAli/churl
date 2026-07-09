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
- [ ] Arrow / `j`/`k` move the Explorer cursor; expand/collapse a collection.

## Picker nav (Search · Sequence · Workspace · Palette)
- [ ] Open each picker; Up/Down move the highlight.
- [ ] Ctrl-p / Ctrl-n move up/down in every picker.
- [ ] `j` / `k` move up/down in every picker.
- [ ] Type to fuzzy-filter; Enter accepts the highlighted row; Esc cancels with no side effect.
- [ ] Esc-cancelling a `<leader>s r` / `<leader>l f` pick does not leak intent into the next `<leader>f` search.

## Sequences run / edit
- [ ] `<leader>s o` opens the "Open sequence" picker; accepting opens the chosen sequence in the Edit face.
- [ ] `<leader>s r` opens the "Run sequence" picker; accepting runs the **chosen** sequence (not sequence #0 / last-run).
- [ ] In-pane `r` on the hovered sequence still runs it directly.
- [ ] `<leader>s a` adds a new sequence.
- [ ] A sequence run shows live per-step status/timing; extracted secret values are masked.

## Load runner
- [ ] `<leader>l c` opens the load runner on the loaded endpoint; `<leader>l f` picks an endpoint first.
- [ ] Edit config header (total / concurrency / interval); start a run → live results list + stats update.
- [ ] Cancel a running batch → launched-then-cancelled rows show a real `{}ms` time-to-cancel next to the cancelled glyph.
- [ ] Never-launched (pending) rows show a blank duration (no fabricated zero).
- [ ] The run writes exactly one summary row to `load_batches` (not per-copy history).

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

## Import / export
- [ ] `churl import "curl ..."` produces the expected endpoint.
- [ ] Export the loaded request as a curl one-liner (`<leader>y`); re-importing round-trips.

## Clipboard
- [ ] Copy actions write via OSC 52 (works over SSH/tmux); >1 MB payload is capped.
