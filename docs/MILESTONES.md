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
| M6.5 | UX review round 1 (owner drive-test fixes) | **done** |
| M6.6 | Request editing UX (URL bar, tabs, in-app CRUD) | **done** |
| M6.7 | UX round 2 (leader key, zoom, URL↔params sync, help overlay) | **done** |
| M7 | Polish + perf + release | **done** |
| M7.1 | Collection interchange (JSON import/export, in-TUI curl paste/copy) | **done** |
| M7.2 | Quick-jump pickers (requests + workspaces) | **done** |
| M7.3 | Environments & vars editor | **done** |
| M7.4 | Request sequences (E2E testing) | **done** |
| M7.5 | Concurrent requests (throttle / load testing) | **done** |
| M7.5.3 | Clipboard cross-platform compat (native + OSC-52 passthrough) | **done** |
| **D1** | Demo-stabilize (regression fixes) | **done** |
| **R0** | Cheap-P0 durability (atomic writes · load-runner memory bound) | **done** |
| **M7.10** | Navigation & keymap unification (design-first) | **done** |
| M7.7 | Response formatting + help search (+ control-char sanitize) | planned |
| **R1** | Durability & concurrency (reserved-names · merge-comments · SQLite WAL · pruning · buffer/channel bounds) | planned |
| M7.6 | Interchange parity (churl-native JSON import) | planned |
| **R2** | Cross-platform proof (CI macOS+Windows · MSRV · cargo-deny · install.ps1 · Wayland) | planned |
| M7.8 | Lifecycle & distribution (version pin, self-update, uninstall) | planned |
| **R3** | Secret & request safety (placeholder-gate · redirect policy · env grandfather+warn) | planned |
| M7.9 | Unified creation flow (`<leader>n`: collection/endpoint/sequence; endpoint from curl) | planned |
| M8 | Cookies + proxy | planned |
| M9 | Plugin system | planned |

> **Re-planning 2026-07-09 — hardening pivot.** Owner + agent ran a full-codebase regression sweep (6 parallel audits) plus an owner demo drive-test. Roadmap re-sequenced to bake **regression (R)** and **debug (D)** milestones in as first-class, scheduled work instead of reactive patch-batches — see the "Milestone taxonomy & re-sequence" section below for the F/R/D model, standing rules, and the D1 → R0 → M7.10 → interleaved order. M7.6–M7.9 keep their scope; only their ordering changed.

> M7.1–M7.5 scheduled 2026-07-07 (owner priority: "these features need to be ready before the plugin feature, right after release") — fractional numbers per the M6.5 precedent so baked-in M8/M9 references stay valid. They run after Ship 0.1 and before M8/M9; order within the block is a proposal, adjustable per session. **The M7.1–M7.5 block is complete (all five done, 2026-07-08)** — M7.5 (concurrent load testing) closed it.

> M7.6–M7.8 added 2026-07-08 from owner feature notes: interchange parity (churl-native JSON import, symmetric with export), a response-formatting/help UX round, and an install-lifecycle cluster (version pinning + self-update + uninstall). Same fractional scheme, same adjustable ordering, still before M8/M9. The M7.3 session also folds in the manifest-in-collection crash fix (see M7.3 deliverables).

> Renumbered after the M3 plan review (2026-07-05): Auth was promoted from the post-release backlog to its own milestone M5; the former M5 (themes/templating) and M6 (polish/release) shifted to M6/M7. Sections below M4 use the new numbers.

---

## Milestone taxonomy & re-sequence (2026-07-09 hardening pivot)

Until now hardening was reactive (M7.5.1/.2/.3 patch-batches squeezed between features) and durability was invisible until it bit. This pivot makes three milestone **classes** explicit and interleaves them by rule:

- **F — feature.** New user-facing capability (M7.6, M7.7, …).
- **R — regression / hardening.** Durability, concurrency, cross-platform, security-correctness. Scheduled up front, never a reactive patch.
- **D — debug / drive-test.** Owner drives the freshly-built binary; the findings *are* the milestone scope.

**Standing rules (the structural change):**
1. **F → D always** — every feature milestone is followed by an owner drive-test + fix pass before the next feature.
2. **R cadence** — ≥1 hardening milestone per 2–3 features; R work is planned, not discovered under fire.
3. **Same-commit docs + tests** — every milestone updates ARCHITECTURE/DECISIONS/MILESTONES/CLAUDE **and** adds regression tests for what it touched, in the milestone commit. (The doc drift found in the 07-09 sweep is the tax on skipping this.)
4. **Living regression checklist** — the demo drive-script becomes a committed, growing checklist so "full regression" is repeatable, not re-discovered each round.
5. **Demo is rebuilt from master in every D** — the 07-09 sweep found the owner drove a *stale* demo binary (picker Up/Down "broken" was actually fixed on master, pre-#23). Rebuild + re-sign is a checklist step, not a ritual to remember.

**Linear sequence:** `D1 → R0 → M7.10 → M7.7 →(D)→ R1 → M7.6 →(D)→ R2 → M7.8 →(D)→ R3 → M7.9 →(D)→ M8 →(D)→ M9`.

### Provenance — the 07-09 regression sweep

Six read-only audit agents (core security/correctness · persistence/data-integrity · concurrency/memory · TUI state-machine · TUI rendering · cross-cutting conventions/tests), each finding verified in the main session. Headline findings and where they land:

| Finding | Severity | Lands in |
|---|---|---|
| Load runner retains every response body (`total × body` → OOM; `on_load_result` builds a full `ResponseView` per row) | **P0** | R0 |
| Saves are non-atomic (`persistence.rs` `std::fs::write` truncate-in-place → crash mid-write destroys the source-of-truth file) | **P0** | R0 |
| Reserved-name collisions (`create_endpoint`/`create_collection` slug to `churl.toml`/`folder.toml`/`sequences/` → written but invisible, no error) | **P0** | R1 |
| SQLite has no `busy_timeout`/WAL + migration loop can race across processes | **P0** | R1 |
| `merge_tables` replaces an array-of-tables wholesale on length change → drops `# comments` on surviving siblings (breaks the format-preserving promise) | P1 | R1 |
| CI is Linux-only; Windows/macOS binaries shipped untested | **P0** | R2 |
| `is_template_placeholder` accepts `{{a}}secret{{b}}`; secret-marker list misses `cookie`/`session`/`signature`/`pwd` | P1 | R3 |
| Redirects followed 10 hops by default; custom-header / query api-keys may forward cross-origin | P1 (verify vs reqwest) | R3 |
| Minified single-line body re-materialized every frame (perf cliff — the M7.7 formatter issue) + unsanitized ANSI/control chars + tab-width-0 | P1 | M7.7 |
| Unbounded app channel · no history pruning · buffers never evicted (long-session growth) | P1 | R1 |
| Global `[keys]` bind silently shadowed by pane overlays; `[keys] space=…` silently dead; leader submenus/leader-key/vim-motions not remappable | P1 | M7.10 |
| Docs drift: `interchange.rs` absent from module tables; ARCHITECTURE clipboard section stale (says "no native dep" post-arboard) | P2 | folded into R1/M7.7 commits |

Clean bill on: churl-core has zero TUI-dep leakage · zero `unwrap`/`panic` in production code (all in test modules) · `cargo audit` in CI · reused HTTP client (no FD churn) · defensive geometry (no panic on 1×1) · well-tested stats/template/fold/search/clipboard-framing.

### Locked owner decisions (2026-07-09 re-planning)

1. **Sequencing** = cheap-P0s → nav → rest (R0 before the feature stream; the two catastrophic-but-cheap P0s first).
2. **Env-editor secret gate** = **grandfather + warn**: churl never *writes* a new literal secret (stays hard-blocked), but a pre-existing hand-authored literal no longer dead-ends an unrelated save — it saves with a warning + an inline `!` marker on the offending row so it's findable. (Fixes the demo dead-end where editing one field was refused over a pre-existing `vars.bearer` literal.)
3. **Keymap scope** = **full data-driven**: churl's *entire* binding surface (leader key + submenu keys/membership + pane + global) becomes remappable via `[keys.*]`, with loud conflict/shadow warnings at load and a fully-generated which-key + `churl keymaps` effective-binding view. **Excludes** edtui's in-editor vim motions (`f`/`t`/`w` inside the Body editor — forking edtui's key register is out of scope).
4. **Sequences pane** = symmetric peek/zoom **exactly like Request/Response**. The current impl fully *hides* the Sequences sub-pane when Explorer is focused (asymmetric bug); fix = it collapses to a peeking header both directions, never disappears. **Explorer zoomed by default** (Sequences peeking). Zero-sequences workspace shows a peeking `Sequences · <leader>s a to add` affordance.
5. **`<leader>S` removed** — once Sequences is always visible + reachable by the uniform `f` jump, a show/hide toggle has no job. (`<leader>s` submenu stays.)
6. **Tab / focus model** = **4 regions**, matching what's actually a distinct visible region: `Left column → URL bar → Request → Response → (wrap)`, Shift-Tab reverses. Returning to the left column restores the last-active sub-pane + its zoom. Rationale: the right column is 3 simultaneously-visible rows (3 stops), the left column shows one sub-pane at a time (1 stop) — Tab stops map to visible regions, not to every focusable pane.
7. **Within-region cycling** = a first-class region-aware action `cycle-region-fwd`/`back` (left = sub-panes, right = buffers/tabs), **shipped unbound by default**, mapped later once a portable key is chosen. `Ctrl-Tab` deliberately **not** hardcoded — it is terminal-unreliable (same class as the Ctrl-J picker caveat: needs the Kitty keyboard protocol; dead on Terminal.app/Windows) and iTerm2/browsers/tmux already claim it. Enhanced-keyboard-protocol support (crossterm `PushKeyboardEnhancementFlags`, which also unlocks reliable Ctrl-J) is a **tracked sub-item of M7.10**, layered over portable defaults — never the only path.

---

## D1 — Demo-stabilize (regression fixes)

**Scope**: The pure bugs + cheap consistency fixes from the owner demo drive-test, so further drive-testing runs on an honest binary. Small; no design session. **Shipped 2026-07-09.**

**Deliverables** (all done):
- **Sequences pane peek-symmetry** — `sequences_shown` now defaults on so the left column always splits; the sub-pane not holding `left_active` collapses to a peeking stub (never vanishes), symmetric with Request/Response; **Explorer zoomed by default**. `<leader>S` is repointed to an interim focus-switch (delegates to `focus_sequences_toggle`) so it can never hide the pane; a zero-sequence workspace's stub shows the `<leader>s a to add` affordance. (Full nav model — `f`-jump, `<leader>S` removal, Tab regions — is M7.10; this is just the render/collapse + interim-keymap fix.)
- **`<leader>s r` routes to a chooser** — new `RunSequencePick` action + `sequence_pick_runs` one-shot intent (mirrors `load_runner_after_pick`); the picker accept path runs the chosen sequence instead of editing it. `run_selected_sequence` stays reachable via in-pane `r` + palette.
- **Cancelled load requests show time-to-cancel** — `cancel_load_run` reads `started` out of `InFlight` and records `row.timing = started.elapsed()` for launched rows; never-launched pending rows keep `timing = None` (no fabricated zero).
- **Verify picker Up/Down on master + rebuild the demo** — no picker code change needed (Up/Down/Ctrl-p/n/j/k already route through `handle_overlay_key`); the demo binary was stale. The rebuild + re-sign is a main-session step. Rebuild-from-master is now item 1 of the regression checklist.
- **Env-editor interim message** — the literal-secret refusal now names the offending var(s) and signals they're pre-existing ("…not saved: {names} — move them to env (grandfathering coming soon)"). The full grandfather+warn/`!`-marker behavior is R3.
- **Living regression checklist** — seeded `docs/REGRESSION.md` from the demo drive-script (grouped by Explorer/Sequences · Picker · Load runner · Env editor · Response · Import/export · Clipboard, with a "record the built commit" header).

**Next**: R0

---

## R0 — Cheap-P0 durability (atomic writes · load-runner memory bound)

**Scope**: The two catastrophic-but-invisible P0s from the sweep — both self-contained, both foundational, ~1 session.

**Deliverables** (all done, 2026-07-09):
- **Atomic saves** — the single `std::fs::write` in `save_value` now funnels through `persistence::atomic_write`: sibling temp file → `write_all` → `sync_all` → atomic `rename` → parent-dir fsync (degrades gracefully where dir-fsync is unsupported), with best-effort temp cleanup on any error. One helper covers `save_endpoint`/`save_sequence`/`save_workspace_manifest`/`save_collection_meta` (they all route through `save_value`). Error contract unchanged (`PersistenceError::Write { path, source }`). Kills the crash-mid-write data-loss on the sole source of truth.
- **Load-runner memory bound** — retention is now O(concurrency + K). `LoadRunnerState` keeps a full `ResponseState::Done { view }` only for a bounded window (last **K=16** completions + the selected row, which is never evicted); an overflowing completion downgrades the oldest non-selected retained row to a new `ResponseState::Dropped { status, timing, size }` that the viewer renders honestly ("response body not retained (memory-bounded)"). Dropped rows keep no body bytes and are **not** reconstructable (accepted load-test tradeoff). Per-row status/timing and the outcomes-driven stats are untouched.
- Tests: `persistence::tests::torn_write_never_replaces_a_good_file` (a failed write leaves the good file byte-intact and no stray temp) + `atomic_write_replaces_content_and_leaves_no_temp`; the format-preserving corpus stays byte-identical. `load_runner::tests::high_total_run_bounds_retained_views` (500 completions → retained `Done` count ≤ K+1, every row Done-or-Dropped, stats correct over all 500) + `selected_row_is_never_evicted`.

**Next**: M7.10

---

## M7.10 — Navigation & keymap unification (design-first)

**Scope**: The coherent navigation model the owner circled across rounds 2–6, plus the full data-driven keymap. Absorbs the demo nav findings and the deferred nav notes. **Design session first.**

**Staging** (three reviewable PRs — A ships alone as the riskiest refactor):
- **Stage A — keymap (dynamic submenus + conflict warnings): DONE** (branch `feat/m7.10a-keymap`). Removed the closed `LeaderMenu` enum + hardcoded `sub_*` fields for data-driven `submenus: HashMap<String, Submenu>` + `LeaderEntry::Submenu(String)`; built-in three seeded as defaults (default config byte-identical); `[keys.leader.<name>]` creates/extends any submenu. Added `KeyMap::validate` load-time conflict/shadow warnings (5 defect classes, genuine defects only) surfaced in three non-blocking channels (stderr pre-raw-mode · first-frame toast · `churl keymaps` `⚠ Conflicts` section). Added `<leader>s f` sequence finder (aliases `s o`). Verified + tested picker uniformity (no silent run-last). See the 2026-07-09 M7.10 Stage A ADRs in DECISIONS.md.
- **Stage B — nav model: DONE** (branch `feat/m7.10b-nav`). Refined the 4-region Tab so the Explorer stop = "left-column region" (Tab restores `left_active`'s sub-pane + zoom, no Endpoints force-reset; `skip_hidden_explorer` for a collapsed left column preserved). Added `cycle-region-fwd`/`back` actions shipped **unbound** (left column ⇒ Endpoints⇄Sequences, right column ⇒ buffer ring). Removed `<leader>S` + the `ToggleSequencesPane` action + `toggle_sequences_pane` fn; retired the `sequences_shown` field (sub-pane always peek-visible; no dead-end — reachable via `s` overlay, `f`-jump `s`, `<leader>s f`). Made `f`-jump pane-only (5 regions, zero row labels; mnemonics e/s/u/r/**p**, `s`=Sequences, Response→`p`). Added `hovered_endpoint()` fallback wired into copy-as-curl + the direct load runner (`send_request` stays loaded-only). See the 2026-07-09 M7.10 Stage B ADRs in DECISIONS.md.
- **Stage C — legibility: DONE** (branch `feat/m7.10c-legibility`). Adopted a symmetric sequence-surface header convention on both the editor and runner: the title now carries only `Sequence · <name>` (+ dirty `●`); the mode shows in a top row inside the pane (`Mode: EDIT` / `Mode: RUN`); the `^R run` / `^R edit` face-flip hint moved to the footer alongside the other key hints. Reorged the load-runner top-left block to **name → url → config → stats**, one row each (title reduced to `Load`). Added a dim one-line purpose hint to the sequence editor, sequence runner, and load panes so a first-time user reads each surface at a glance. Render-only — no keymap/nav/focus behavior changed. See the 2026-07-09 M7.10 Stage C ADR in DECISIONS.md.

**M7.10 complete** (all three stages merged). Milestone status → done; roadmap advances to **M7.7**.

**M7.10 follow-up fixes** (owner drive-test 2026-07-10, branch `feat/m7.10-followup-fixes`) — five small UX corrections surfaced by a live drive-test after the nav/keymap unification landed. See the 2026-07-10 ADRs in DECISIONS.md.
1. **Find/open pickers on the leader key** — `<leader><leader>` (endpoints), `<leader>s <leader>` (sequences), `<leader>l <leader>` (load) replace the old `<leader>f`/`<leader>s o`+`s f`/`<leader>l f`; `f` freed at root for jump-mode. Validator stays clean (Space-as-continuation is not a conflict). Also collapses the "two ways to open a sequence" into one.
2. **Load-runner run = Ctrl-R** — matches the sequence surface; plain `r` no longer runs. Sequence editor/runner deliberately left untouched.
3. **`f`-jump `e` lands on the endpoints tree** — the Explorer label now resets `left_active = Endpoints`, so `f e` from a focused Sequences sub-pane works instead of appearing to do nothing.
4. **Response `[h]` headers-hint is focus-gated** — shows only when its response pane (main / sequence-runner / load-runner) is focused.
5. **Arrow keys navigate the explorer** — `Up`/`Down`/`Left`/`Right` bound in the Explorer overlay (mirroring `k`/`j`/`h`/`l`), the last pane where arrows didn't already work.

**Deliverables**:
- **4-region Tab model** (decision 6): `Left column → URL → Request → Response`, Shift-Tab reverses; left column is one stop showing the active sub-pane; returning restores last-active sub-pane + zoom.
- **`f` jump-to-pane addresses all five** (Explorer, Sequences, URL, Request, Response) — precise access including the peeking Sequences pane (reverses the PR-2b deferral in DECISIONS.md).
- **`cycle-region-fwd`/`back` action** (decision 7) — region-aware within-region cycling, shipped **unbound**; plus **enhanced-keyboard-protocol** support as a tracked sub-item (unlocks a layered Ctrl-Tab / reliable Ctrl-J over portable defaults).
- **Remove `<leader>S`** (decision 5); keep left-column sub-pane switch on `s`/`f`.
- **Picker semantics unified** — `<leader>s o`/`s r` behave like `<leader>f`/`<leader>l f` (always choose via picker, never silently run-last).
- **Full data-driven keymap** (decision 3) — leader key + submenu keys/membership + pane + global all in `[keys.*]`; **loud conflict/shadow warnings** at load; which-key fully generated; `churl keymaps` shows effective binds. (Excludes edtui in-editor motions.) This is the M9 plugin-command foundation.
- **Hover-vs-selection resolution** (round-6): one-shot open-y actions fall back to the hovered endpoint when nothing is loaded.
- **Sequence-surface header convention** — top row `Mode: EDIT/RUN`, shortcuts moved to the footer (currently `EDIT (^R run)` in the header with shortcuts at the bottom).
- **Runner/sequence pane legibility** — load-runner top-left reorg (name→url→config→stats), pane spacing + a one-line functionality description on the sequence + load panes (the deferred #4D crowding).
- Tests: Tab-cycle order + region focus-restore, peek-symmetry both directions, conflict-warning on a shadowed bind, `f`-jump reaches Sequences, picker-choose paths.

**Next**: M7.7

---

## R1 — Durability & concurrency (persistence + state)

**Scope**: The rest of the durability backbone from the sweep (R0 took the two cheapest P0s).

**Deliverables**:
- **Reserved-name guards** — `create_endpoint`/`create_collection`/`rename_*` reject/suffix a slug equal to `MANIFEST_FILENAME`/`FOLDER_FILENAME`/`SEQUENCES_DIRNAME` (fixes the silent "written but invisible" data-loss).
- **Merge-comment preservation** — array-of-tables merge does a longest-common-prefix element-wise merge (append/truncate the tail) instead of wholesale replace, so `# comments` on surviving siblings survive a length change.
- **SQLite concurrency** — `busy_timeout` + `journal_mode=WAL` on open; wrap the migration loop in `BEGIN IMMEDIATE` (guards the cross-process migration race); + a concurrent-writer test.
- **History pruning** — age/count-capped retention prune for `history`/`load_batches` (mirrors the vault's `runs prune`).
- **Buffer eviction + bounded channel** — LRU-cap open buffers (or drop non-active buffer bodies); bound (or body-strip) the app channel so a fast load can't flood it with 10 MB `Response` messages.
- Docs: add `interchange` to the ARCHITECTURE/CLAUDE module tables; fix the stale clipboard section.
- Tests: reserved-name collision, comment-survival on array add/remove, concurrent SQLite writers, pruning, long-session buffer growth.

**Next**: M7.6

---

## R2 — Cross-platform proof

**Scope**: Make "works on any machine" *true*, not assumed. Mostly CI/infra; can overlap other work.

**Deliverables**:
- **CI matrix** adds `macos-latest` + `windows-latest` to the fmt/clippy/test job (Windows binary is shipped but never tested today).
- **MSRV pin** (`rust-version`) + an MSRV CI job (edition 2024 already needs a recent compiler).
- **`cargo-deny`** (license + banned-source + duplicate-dep gating) alongside the existing `cargo audit`.
- **`install.ps1`** — a PowerShell installer so Windows has a one-liner path (parity with `install.sh`).
- **Wayland clipboard** — enable arboard `wayland-data-control` (or document the OSC-52 fallback in the README platform matrix, not just DECISIONS).
- **Cross-platform path tests** — Windows separators / drive-letter absolutes through persistence + sequence path guards.

**Next**: M7.8

---

## R3 — Secret & request safety

**Scope**: The security-correctness findings + the full env-editor secret policy.

**Deliverables**:
- **Tighten `is_template_placeholder`** — accept only exactly-one-well-formed-placeholder (reuse the template parser), closing `{{a}}secret{{b}}`; **broaden secret markers** (`cookie`, `session`, `signature`, `sig`, `pwd`, `access_key`) + a value-entropy fallback for unknown-named secrets.
- **Redirect policy** — verify reqwest 0.13's cross-origin header-strip list; if custom-header / query-placed api-keys forward across origin, tighten the default (limited hops / drop on cross-origin) + a test per auth placement.
- **Env-editor grandfather+warn** (decision 2) — save proceeds over pre-existing literals with a warning + inline `!` marker; only *new* churl-written literals are hard-blocked.
- Tests: placeholder-gate rejects mixed literal+placeholder, marker negative-coverage, cross-origin redirect auth-forwarding per placement, grandfathered save + new-literal block.

**Next**: M7.9

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

## M6.5 — UX review round 1

**Scope**: The quick-fix batch from the owner's first live drive-test (2026-07-06). Fractional number: M7–M9 references are baked into code comments and docs, so inserting milestones must not renumber them (decision recorded in DECISIONS.md).

**Deliverables** (owner notes, verbatim intent):
- **Layout**: two columns — Explorer (left) | column B stacked: Request (top) / Response (bottom) — more width for readability and editing
- **Statusline reset**: transient status messages (send outcome, cancel, warnings) auto-expire back to the key-hint guide; they currently stick forever
- **Profile message dedup**: switching profiles must not emit a status message duplicating the persistent `profile:` indicator (the indicator is the single source of truth)
- **In-flight visibility**: while a request is in flight the statusline shows `sending… (ctrl-c cancels)` and the response pane's in-flight state is unmistakable (spinner/elapsed)
- **Profile picker marks the active profile** (e.g. `●` prefix)

**Verified by**: `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all` (199 tests) all green; `cargo run -p churl -- --version` works. New tests (5): `status_expires_after_4s` (backdated struct construction to test expiry), `in_flight_statusline_message_derives_from_state` (render-time derivation), `profile_picker_marks_active` (● markers and filtering), `placeholder_count_in_url` (urlbar unit), `url_bar_shows_indicators_for_auth_and_placeholders` (snapshot with auth + placeholder indicators).

**Notes**:
- Column B is three rows: URL bar (3 lines, display-only in M6.5), Request (50% of remaining), Response (50% of remaining). The URL bar renders `METHOD  url` + right-aligned `auth:<kind>` and `{{N}}` placeholder-count indicators. Not focusable/editable — M6.6 adds that. New component: `tui::components::urlbar`.
- `status: Option<String>` changed to `status: Option<TransientStatus>` — a private struct holding the message and `set_at: Instant`. Expiry checked on the 250 ms tick. Tests backdate using direct struct construction (private struct, same module).
- In-flight statusline message is derived from `app.in_flight.is_some()` at render time, not stored as a `TransientStatus` — appears/disappears atomically with the send/response.
- Spinner frame derived from `app.tick_count % 8` (braille characters `⠋⠙⠹⠸⠼⠴⠦⠧`). Tests use `tick_count = 0` implicitly (default) for deterministic first frame `⠋`.
- Profile picker active marker is display-only (in labels only, not in `profile_choices`). Nucleo fuzzy-matching on `"● dev"` still matches the query `"dev"` because the marker is a prefix — no stripping needed.
- All layout snapshots updated: three-row column B visible at 80×24 (URL bar / Request / Response).

**Deviations from the pinned design**:
- None. Every fix implemented exactly as specified (including the mid-session §1 update to the three-row layout).

- Main-session review fix: the explorer column is `Length(30)`, not `Min(24)`+`Fill` — ratatui distributes excess into `Min`, which grew the explorer to half the screen; the owner prompt says *narrow* column.

**Next**: M6.6

---

## M6.6 — Request editing UX

**Scope**: In-app request authoring — the gap called out by the owner's drive-test ("no way of creating/editing requests"). Was deferred in M2's notes ("full editing UX matures in later milestones") but never assigned a milestone — rescued from that limbo by the owner's 2026-07-06 review. Ships **before** the release milestone: 0.1 must be a client you can author requests in.

**UX north star (owner, 2026-07-06)**: ease of use and quick actions are what make churl better — judge every design decision by keystroke count for the common loops. Target: *jump to bar → tweak URL → send* and *switch method → resend* each in 3–4 keystrokes.

**Deliverables** (design session first — Postman-familiar, terminal-native):
- **URL bar becomes first-class focusable** (owner requirement 2026-07-06): joins the Tab cycle and jump-mode labels; when focused, type to edit the URL inline and switch the method with a single quick action (cycle key or one-keystroke menu — decide in the design session against real keybinding ergonomics). The M6.5 display-only bar (`tui::components::urlbar`) is the base; indicators (auth kind, `{{n}}`, unsaved dot once CRUD lands) stay right-aligned
- **Content tabs** in the Request pane: Params / Headers / Auth / Body, switchable (owner screenshots on file); each tab editable (add/remove/toggle rows; auth kind + fields; body via the existing edtui editor)
- **In-app CRUD**: create endpoint (into a collection, `seq` auto-assigned), create collection, rename, delete (with confirm); all writes through the existing format-preserving persistence + secrets refusal
- Save flow: explicit save action (statusline dirty indicator) — never auto-write on every keystroke
- Tests: tab state machine, URL-bar edit round-trip, CRUD persistence integration, snapshots per tab

**Open questions** (for the M6.6 design session):
- ~~Keybinding scheme for tab switching and row editing~~ **Resolved**: `[`/`]` cycle + `1`–`4` direct jump (Request overlay); rows `j`/`k`/`a`/`d`/`Space`/`Enter`/`i`. All remappable.
- ~~Delete confirmation UX~~ **Resolved**: `y/n` for endpoints, typed collection name for collections (risk-proportional).

**Verified by**: `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all` (246 tests, from the 199 baseline) all green; `cargo run -p churl -- --version` works; `churl keymaps` prints the global map plus per-pane overlay sections. New tests (47): churl-core CRUD persistence (`create_endpoint` slug/seq/empty-collection/collision/empty-name, `rename_endpoint` file+name atomicity + secrets-refusal-leaves-file, `delete_endpoint`, `create_collection` dir-without-folder-toml + refuse-existing, `rename_collection` moves contents, `delete_collection` recursive); `Method::cycle` wrap; config `[keys.*]` overlay split; `KeyMap` overlay precedence + override parse + unknown-table/action errors + overlay iter/combos; `LineEditor` (insert/backspace/delete/motion/unicode/control-keys); `RequestTabs` state machine (cycle, direct jump, per-tab selection persistence, clamp, edit-cancel-on-switch); `method_menu` label resolution; palette curated-allowlist guard + every-entry-dispatches; TUI integration (URL edit→commit→save writes file, row add+toggle serialization with `enabled = false`, discard-changes switches without persisting); snapshots (URL bar focused / editing / dirty-dot; Params/Headers/Body/Auth tabs; method menu; new-endpoint prompt; delete + discard confirms; curated palette). Existing layout/jump snapshots updated for the tab bar, focusable URL bar, four-pane jump labels, and the `w save` statusline hint.

**Notes**:
- **Contextual keymaps**: `KeyMap` gains per-pane overlays (`PaneCtx ∈ {Explorer, UrlBar, Request, Response}`); `lookup_ctx(key, ctx)` = overlay-then-global. `handle_key` routes through the focused pane's `ctx()`. Config `[keys.<pane>]` sub-tables parse fail-loud; `churl keymaps` groups overlays under headers. See DECISIONS.
- **Focusable URL bar**: `Pane::UrlBar` joins the Tab cycle (`Explorer→UrlBar→Request→Response`) and jump-mode (now 4 pane labels `a/s/d/f`, rows from `g`). `i`/`Enter` edit the URL inline via `LineEditor` (Enter commits, Esc reverts); `m` cycles method, `M` (shift-m) opens a one-key method menu (`g`et `p`ost `u`t… ). Indicators (`●` dirty dot, `auth:<kind>`, `{{n}}`) are right-aligned and recompute from the live request.
- **Request tabs** (`RequestTabs` state on `App`, `RequestTab ∈ {Params, Headers, Auth, Body}`): tab bar with active highlight + row counts; `]`/`[` cycle, `1`–`4` jump. Params/Headers are row-list editors (`j`/`k` move, `a` add+edit, `d` delete, `Space` toggle `enabled`, `Enter`/`i` edit; name→value field edit via `LineEditor`, Tab/Enter advance, Esc cancels). Auth tab: kind row opens the None/Basic/Bearer/ApiKey picker (default-empty fields on switch); field rows edit like params; ApiKey `placement` toggles with `Space` or `Enter`; literal secret values render masked (`*****`), the save-time refusal surfaces on the statusline. Body is the unchanged edtui editor (M4 Ctrl-S/Ctrl-C interception stands, now gated on the Body tab being active). Send/save read the live in-memory request.
- **In-app CRUD** through new `churl-core::persistence` seams (see DECISIONS): Explorer overlay `n` new endpoint / `N` new collection / `r` rename / `d` delete. Prompts (`Mode::Prompt`) and confirms (`Mode::Confirm`) are new overlay modes; after any op the explorer reloads (preserving expansion + cursor) and selects the created/renamed item.
- **Dirty tracking + explicit save**: derived by comparing the live request (incl. body) against `loaded_snapshot`; never auto-written. `w` (or the palette "save request") saves format-preserving, refreshes the snapshot, and reports "Saved <name>". Switching endpoints while dirty raises the discard-changes confirm.
- **Curated command palette** (owner mid-flight addition, §6b): explicit context-free allowlist replacing `Action::all()`; new Actions never auto-appear. CRUD/send from the palette act on the explorer selection or surface a statusline error.

**Keystroke-count audit** (owner north star: common loops in 3–4 keystrokes):
- *Jump to bar → tweak URL → send*: `f` (jump) + `s` (UrlBar label) + `i` (edit) + …type… + `Enter` (commit) + `Ctrl-S` (send) = **4 control keystrokes** counting the edit itself as free (f, s, i, Enter with Ctrl-S) — within budget. From an already-focused bar it is `i` + Enter + Ctrl-S.
- *Switch method → resend*: `f` + `s` (jump to bar) + `m` (cycle) + `Ctrl-S` (send) = **4**. Via the menu: `f`, `s`, `M`, label, `Ctrl-S` = 5 (the menu trades one keystroke for a direct pick rather than repeated cycling).
- Both loops meet the 3–4 target; the bar being in the Tab cycle also gives a no-jump path (Tab×N + i/m) for keyboards without the jump key.

**Deviations from the pinned design**:
- **Auth field edits are single-field, not name→value**: auth rows have fixed labels (username/password/token/name/value/placement), so `row_edit` on an Auth row edits the one value directly (seeded on `EditField::Value`, committed on Enter) rather than the name→value two-step used for Params/Headers rows. The design described the two-step generically; applying it to fixed-label auth fields would let the user "edit" an immutable label. No behavioural loss — every auth value is still editable; kind/placement change via the picker/`Space`.
- **`Config` `[keys]` split via an untagged `KeyEntry` enum + post-load partition**, rather than a custom `Deserialize`. serde cannot write two struct fields from one table, so the raw `[keys]` table deserializes into `raw_keys: BTreeMap<String, KeyEntry>` (`KeyEntry = Action(String) | Overlay(map)`) and `split_key_overlays` partitions it into `keys` + `key_overlays` in `load_config`. Same observable config surface; keeps the flat `keys` map for existing callers.
- **Initial build also violated the design in two places the review caught** (fixed in the review round below): Enter on the ApiKey placement row was a silent no-op (design: Space *and* Enter toggle — now both do), and the DiscardChanges guard only covered the explorer-Enter path (design intent: never lose edits silently — see the guarded-seam fix).

**Review round (2026-07-06, findings #1–#9 fixed; #10 deferred)**:
- **#1 Body-tab routing**: `i`/`a`/`d`/`Space` on the Body tab forward to edtui instead of being eaten by the Request overlay's Row* actions (there are no rows on the Body tab). Regression test `body_tab_row_keys_reach_edtui`.
- **#2/#7 Data loss — unguarded switch paths**: the search overlay, jump-mode row labels, and CRUD reselects (new endpoint, rename) bypassed the discard-changes guard and silently discarded dirty edits. All endpoint-switch paths now funnel through one `guarded_load(PendingLoad::{Row,File})` seam; the pending target parks on `App::pending_load` behind `Confirm(DiscardChanges)` (payload-free now). See DECISIONS. Renaming the *loaded* endpoint updates file+name in place — edits survive their own rename with no confirm.
- **#3 Data loss — save-then-switch on a failed save**: `s` now switches only when the save actually took; a secrets refusal keeps the user on the dirty endpoint with the error visible.
- **#4/#5 Stale indices after explorer reload**: `reload_explorer` remaps `selected`'s collection index from its file path (name-sorted siblings shift indices — the resolver read the *wrong* collection's `folder.toml` vars) and clears a vanished selection; collection rename repoints the loaded endpoint's file into the new directory (next save no longer fails NotFound). See DECISIONS.
- **#6 Placement row**: Enter toggles header/query, same as Space (per the pinned design).
- **#8 Ghost rows**: Esc on a field edit of a freshly-added, still-empty row removes the row (it would otherwise serialize nameless).
- **#9 Vacuous tests strengthened**: `every_palette_command_dispatches` asserts a concrete non-no-op effect per command (statusline error / focus change / picker / quit); `discard_changes_switches_endpoint` asserts the endpoint actually switched.
- **#10 Deferred to M7** (owner-north-star triage): no horizontal scroll in URL-bar/prompt inline editing (typing blind past the right edge) and no vertical scroll in row lists — carried in M7's scope.
- New tests (9): `body_tab_row_keys_reach_edtui`, `reload_remaps_selected_collection_index_for_resolver`, `search_switch_while_dirty_is_guarded`, `jump_switch_while_dirty_guards_and_saves`, `save_failure_blocks_discard_changes_switch`, `rename_collection_repoints_loaded_endpoint_file`, `placement_row_enter_toggles`, `new_endpoint_while_dirty_is_guarded`, `ghost_row_removed_on_escape` — total now 256 (incl. the main session's `rename_endpoint_same_slug_keeps_filename`).

**Next**: M6.7

---

## M6.7 — UX round 2 (owner drive-test 2026-07-06, second pass)

**Scope**: Second owner drive-test of the M6.6 build surfaced four discoverability failures (features that exist but couldn't be found), two dropped requirements, and one design miss. This milestone makes the keymap self-teaching (leader + which-key + help overlay), fixes the real gaps (zoom, explorer toggle, inline-edit scrolling, URL→params sync), and removes the digit-key collision. Ships before M7 — a release you can't discover isn't releasable.

**Deliverables** (in build order — later items render leader/help content from earlier infra):

1. **Leader key + which-key popup**: `Space` becomes the global leader. Pressing it enters a pending-leader state and (immediately) shows a small floating panel listing the bound continuations (which-key style); any unbound key or Esc dismisses. Initial leader map: `<leader>e` toggle explorer, `<leader>s` send (fallback alias for Ctrl-S), `<leader>c` cancel in-flight request (fallback alias), `<leader>p` switch profile, `<leader>q` quit. Leader is inert during text edits (LineEditor/edtui) — Space types a space. The Request-pane row-toggle rebinds `Space` → `t` (freeing Space everywhere). Config: leader key remappable; `[keys.leader]` sub-table for continuations, same fail-loud parsing as pane overlays; `churl keymaps` prints a Leader section.
2. **Drop global `1`/`2`/`3` pane-focus binds**. Navigation is Tab/Shift-Tab + `f` jump-mode only; `1`–`4` remain solely as Request-overlay tab jumps. **Root-cause first**: the owner observed digits "mostly jumping to Request" regardless of focus — the keymap as written doesn't explain that; find out whether `focus.ctx()`/dispatch has a real bug before deleting the binds (a dispatch bug would affect other overlay keys too). Record the finding in the milestone notes.
3. **URL→Params sync on edit-commit**: committing a URL edit (Enter) strips any query string from the URL and merges it into the Params tab; the bar thereafter shows the base URL; send composes base URL + enabled params (existing behavior). Merge policy, per committed `name=value` pair, in order: (a) exact name+value row exists → ensure enabled, no duplicate; (b) name exists with different value → first row with that name gets the new value + enabled; (c) name absent → append enabled row; (d) duplicate names within the URL itself (`?tag=a&tag=b`) map positionally onto existing rows of that name, extras appended (multi-value params preserved). Statusline reports the merge ("params: A updated, B added") — never silent. Marks the request dirty (normal save flow). DECISIONS.md entry: this scopes the M4 "query stays in the URL, lossless" rule to *unedited imports* — first edit-commit explodes the query into params and the next save rewrites the TOML accordingly (editing is intentional change).
4. **Pane zoom** (`z`, focused-pane-only — tmux prefix-z model): `z` in the Request overlay zooms Request, collapsing Response to its stats line; `z` in the Response overlay zooms Response, collapsing Request to its tab bar. Invariant: **a collapsed pane cannot hold focus** — Tab/jump-mode/focus actions targeting the collapsed pane auto-unzoom first. `z` again restores the split. No global variant.
5. **Explorer sidebar toggle** (`<leader>e`, global — original kickoff-prompt requirement "collapsible explorer", dropped from every milestone until now): hides the 30-column explorer, right column takes the full width. Same invariant as zoom: any action that would focus the explorer (Tab cycle, jump label, palette "focus explorer") auto-reopens it. State is session-only (not persisted).
6. **Inline-edit scrolling** (pulled forward from M7, M6.6 review finding #10): horizontal viewport scrolling in `LineEditor` renders (URL bar + prompts) — the view follows the cursor, with truncation indicators (`…`) at the clipped edge(s); typing past the right edge must never go blind. Vertical scrolling in the Params/Headers row lists (mirror the explorer's `scroll_to_fit`).
7. **URL vim-popup editor**: `e` on the URL bar opens a centered floating editor (edtui — already in-tree for Body) seeded with the URL, constrained to a single logical line; vim mode indicator (NORMAL/INSERT) in the popup border/footer — the chrome the inline bar lacks. Enter commits (running the deliverable-3 param merge), Esc in normal mode cancels. Config `url_edit = "inline" | "popup"` selects what `i`/`Enter` on the bar opens (default `inline`); `e` always opens the popup.
8. **`?` help overlay** (pulled forward from M7): floating pane rendering the *effective* keymap from the live `KeyMap` (never a hardcoded list — it cannot drift), sectioned **Global / Explorer / URL bar / Request / Response / Leader**, scrollable, dismissed with `?`/Esc/`q`. Available from any pane outside text-edit modes.
9. **Dedicated message row** (owner requirement 2026-07-07): action/transient messages (saves, merges, errors, CRUD results) move out of the statusline into their own row rendered directly *above* it — they must never cover statusline content (the statusline may become owner-customizable later; keep the two components decoupled). The row appears only while a message is live and disappears after expiry — default lifetime **6 s** (a named constant, config-knob-ready), replacing today's shorter `TransientStatus` expiry; a newer message replaces the current one. The statusline keeps only persistent state (focus/endpoint/dirty/profile/in-flight spinner). Expiry still checked on the existing 250 ms tick.

**Keystroke audit** (north star unchanged — common loops in 3–4 keystrokes): *tweak URL → send* and *switch method → resend* are untouched (Ctrl-S stays primary send; leader aliases are fallbacks, not replacements). Zoom-and-read is `z` from the focused pane; explorer toggle is 2 keystrokes from anywhere.

**Tests**: leader state machine (pending → dispatch/dismiss, inert during edits, which-key popup snapshot); keymap Leader-section parsing + `churl keymaps` output; digit-bind removal (1–4 only act in Request); URL-commit merge policy unit tests covering rules a–d + statusline message + dirty flag; TOML rewrite round-trip after explode; zoom state machine incl. focus-collapsed-pane auto-unzoom; explorer toggle incl. auto-reopen paths; LineEditor viewport (cursor kept in view, edge indicators, unicode widths); row-list vertical scroll; popup editor commit/cancel + single-line constraint + `url_edit` config; help overlay renders every bound action (guard test: no section missing) + snapshots per section; message row (appears above statusline, 6 s expiry via backdated set, replacement by newer message, statusline content untouched while a message is live) + snapshots with/without an active message.

**Verified by**: `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all` all green — **301 tests** (up from the 256 baseline).

**Notes**:
- **Digit-key root cause (deliverable 2)**: investigated before deleting the binds. There was **no dispatch bug** — `lookup_ctx` correctly consults the focused pane's overlay before the global map, and it worked for every overlay key. The owner's "digits mostly jump to Request" was the *dual meaning* itself: `1`–`3` were global pane-focus binds *and* `1`–`4` were Request-tab jumps in the Request overlay, so the behaviour flipped with focus with no visible cue. A dispatch bug would have affected other overlay keys (`]`/`[`/`a`/`d`), which behaved correctly — confirming the collision, not a bug, was the discoverability failure. Fix: dropped the global `1`/`2`/`3` binds entirely; `1`–`4` now act *only* as Request-overlay tab jumps. Pane focus is Tab/Shift-Tab + `f` jump-mode.
- **Space→leader / row-toggle→`t`**: making Space the global leader required freeing it in the Request overlay, so row-toggle rebound to `t` (updated the row-toggle snapshot test accordingly).
- **Statusline is now persistent-state-only**: transient messages moved to the dedicated row (deliverable 9); the statusline shows focus · workspace · profile · dirty ● · in-flight spinner (or the key hints when idle). Every existing pane snapshot's last row changed and was re-accepted.
- **`unicode-width` added** (workspace dep) for the `LineEditor` viewport's cell-accurate cursor tracking; already transitively in-tree, so no new external crate.
- **URL→Params merge** decodes `+`/`%XX` in query values with a minimal inline decoder (no new dep; core has an encoder but no decoder).

**Open questions**: none — design fixed with the owner 2026-07-06 (this conversation supersedes the M7 "full-screen response toggle (`F` key)" line, which is replaced by the mutual-zoom design here).

**Review round 2 (owner drive-test 2026-07-07, findings #1–#7; #8 deferred to M7 with a milestone entry)**:
1. **Which-key popup anchors bottom-right**, not bottom-center — directly above the message/status rows, right-aligned to the terminal edge.
2. **Query-param hygiene on URL commit**: names/values are whitespace-trimmed before the merge; decoding stays single-pass (never double-decode already-decoded text); *send-time composition must percent-encode* params properly (verify the `http.rs` path — if query assembly is manual string concat, encode there; if it goes through reqwest's `.query()`, confirm and add a test with a space + `&` in a param value proving the wire URL is correctly encoded).
3. **Zoom stubs were built as bare 1-row borders — deviation from the pinned design (deliverable 4) that review round 1 missed**: a collapsed pane must render its promised one-line summary as *content*, not a border fragment. Collapsed Request = its tab bar line (`Params(n) Headers(n) Auth Body`); collapsed Response = its stats line (`status · time · size · N hdrs`, or `no response yet`). No full block chrome in the 1-row state.
4. **Help overlay half-page scroll**: `d`/`u` (and `Ctrl-d`/`Ctrl-u`) scroll the help overlay half a page down/up, consistent with the response viewer.
5. **Unsaved-changes indicator made explicit** (owner refinement 2026-07-07): the bare statusline `●` reads as decoration. Three steady, consistent markers while dirty (no flashing — steady accent over animation): (a) statusline becomes verbal — `● unsaved · w save`, theme-accented; (b) the URL-bar `●` indicator gets the same accent colour instead of default fg; (c) the loaded endpoint's row in the explorer tree gains an accent `●` suffix (the editor modified-file convention) that clears on save/discard.
6. **Help overlay styling**: dim the shortcut-key column's background (subtle/dimmed key style instead of a loud highlight block).
7. **Response stats move to the top-right corner** of the Response pane (right-aligned block title), not top-left.
8. *Deferred with a milestone entry (never a bare "later")*: response-pane copy-to-clipboard + richer navigation — added to M7 deliverables.
9. **Numbered tab titles while Request is focused** (owner mid-flight addition): tab titles render their jump digit as a prefix — `(1) Params (n) · (2) Headers (n) · (3) Auth · (4) Body` — when the Request pane is focused (where `1`–`4` are live); unfocused, titles stay clean. Self-documents the tab-jump keys in place.

**Review round 2 outcomes (agent build, 2026-07-07)**:
- **#1 done**: `Flex::End` for horizontal layout in `leader_popup::render` — popup now anchors bottom-right.
- **#2 done**: `split_query` trims whitespace from name/value before `percent_decode`; `http.rs` already uses reqwest `.query()` (reqwest handles encoding); added wiremock test `param_with_space_and_ampersand_is_encoded_correctly_on_wire` + unit test `split_query_trims_whitespace`.
- **#3 done**: `request::collapsed_summary` and `response::collapsed_summary` added; render in app.rs detects zoom state and renders the collapsed summary `Paragraph` instead of the full pane for the non-zoomed pane. Snapshot tests `zoom_request_collapsed_summary` and `zoom_response_collapsed_summary` added.
- **#4 done**: `help_viewport_height: usize` field on `App` (default 10); `help::render` now returns `RenderOutcome { total, viewport_height }` and the render call stores the height. `handle_help_key` handles `KeyCode::Char('d')` and `'u'` for half-page scroll (Ctrl-d/Ctrl-u handled by the same code path since both yield same char code).
- **#5 done (incl. owner refinement)**: new `accent` theme slot (dark: yellow, light: magenta; overridable via `[theme_colors]` like every slot). (a) Statusline unsaved marker is a theme-accented `Span` — `· ● unsaved · w save` — not plain string concat. (b) URL-bar `●` dirty dot split out of the dim indicator string into its own accent-styled span (auth/placeholder indicators stay dim). (c) The loaded endpoint's explorer row gains an accent ` ●` suffix while dirty, matched by **file path** (`ExplorerState::row_endpoint_file`, never by index) via a new `dirty_file: Option<&Path>` render param threaded from app.rs; clears on save/discard. Test `explorer_row_dirty_marker_clears_on_save` asserts all three markers while dirty (snapshot) and their absence after `w` save; `url_bar_dirty_dot` snapshot re-accepted with the explorer marker + verbal statusline.
- **#6 done**: `help_lines` changed to `Style::default().add_modifier(Modifier::DIM)` for the key column instead of `theme.jump_label`.
- **#7 done**: `response::render` embeds stats as a right-aligned block title (`Line::from(...).right_aligned()`) for the Done state; Done state uses the full inner area as body (no status_area split). Non-Done states keep the existing status_area layout.
- **#9 done**: `tab_bar` / `tab_bar_line` in `request.rs` accept a `focused: bool` parameter; when focused, tabs are prefixed with `(N)`. `collapsed_summary` always passes `focused=false`. Snapshot test `request_tab_bar_shows_digit_prefixes_when_focused` added; existing unfocused snapshots stayed byte-identical.

**Verified by**: `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all` — **307 tests** (up from 301 baseline), all green.

**Next**: M7

---

## M7 — Polish + perf + release

**Scope**: Performance validation, final UX touches, release preparation.

**Deliverables**:
- Cold-start benchmark: `hyperfine 'churl --help'` < 100 ms on reference hardware
- JSON folding in response viewer
- **Response headers view**: toggle between body and full headers in the response pane (closes the M3 open question; count-only until then)
- **Wrap toggle** in the response viewer (closes the M3 horizontal-scroll open question — wrap chosen over horizontal scroll)
- **Response body search** (owner request 2026-07-05): `/`-style incremental search within the response viewer with match navigation — the explorer `/` fuzzy search never covered response bodies, and search beats folding for large payloads
- **Response copy + richer navigation** (owner request 2026-07-07, deferred from the M6.7 review round 2): copy response body (and selected line/section) to the system clipboard; extend response-pane navigation (word/line motions, jump to top/bottom already exist — evaluate what's missing against real use)
- Highlight micro-nits from the M3 review: skip re-enqueueing a highlight job already in flight for the same viewport hash; strip `\r` from CRLF bodies in the line index
- README: install, quickstart, feature matrix, screenshot
- `cargo publish` dry-run passes for both crates
- GitHub release action (tag-triggered), building per-platform binaries: macOS arm64 + x86_64, Linux x86_64 **musl static** + aarch64, Windows x86_64 (owner requirement 2026-07-06: installable without Rust — rustls + bundled SQLite already make the binary self-contained)
- **`curl | sh` installer** (owner request 2026-07-06): `install.sh` in the repo, served via the release — detects OS/arch, downloads the matching release binary, installs to `~/.local/bin` (prompting/`--to` for override). `cargo install churl` remains the Rust-user path
- **`churl tutorial` onboarding** (owner request 2026-07-06): scaffolds a demo workspace (commented `churl.toml` with a profile + vars, one collection with `folder.toml`, an example endpoint against a public echo API) so a first-time user sends a request in under a minute; README quickstart mirrors it

Delivered in two waves (committed separately): **wave 1** = response-viewer features (folding, headers, wrap, search, copy, richer nav, highlight micro-nits); **wave 2** = release infra (cargo metadata, README, tutorial, release workflow, installer, cold-start benchmark).

### Wave 1 — response-viewer features (done)

The response viewer gained a display pipeline and vim-like navigation. All keys live in the configurable `[keys.response]` overlay and appear in the `?` help overlay (the every-bound-action-appears guard test stays green).

- **Display pipeline** (`components/response.rs`): logical lines (body or headers, CRLF-stripped) → fold filter (JSON-only) → wrap expansion (optional) → viewport slice. Cursor and scroll are display-row indices (post-fold, post-wrap); search matches are stored against logical lines and mapped through the pipeline. All pipeline stages are pure fns, snapshot-tested without a runtime.
- **Cursor line** — `j`/`k` move a vim-like cursor (scroll follows to keep it in view); `g`/`G` first/last; `Ctrl-d`/`Ctrl-u` half-viewport. Cursor row uses `theme.selection` (no new slot).
- **Headers view** — `h` (Response overlay, shadows global Collapse) toggles Body↔Headers, rendered through the same pipeline; stats title gains `· headers`. Closes the M3 count-only open question.
- **Wrap toggle** — `W` soft-wraps at the pane width via a `unicode-width`-aware display-row index (rebuilt on toggle/resize/fold/mode change); stats title gains `· wrap`. Closes the M3 horizontal-scroll open question (wrap chosen over h-scroll). **Fallback taken**: wrapped mode renders unhighlighted plain text — slicing highlighted spans at wrap boundaries was deferred (see DECISIONS).
- **JSON folding** — `o` folds/unfolds the innermost region at the cursor; `O` collapses all top-level regions or expands all. Regions scanned once per response by a string-aware scanner (`components/fold.rs`); a folded region renders `<opener> ⋯ N lines` (dim). Non-JSON responses no-op with a `folding: JSON responses only` notice.
- **Body search** — `/` opens a literal, smart-case incremental search in the message-row position (shared `LineEditor`, new `Mode::BodySearch`; shadows global fuzzy `/`). `n`/`N` cycle matches (wrapping); each nav scrolls the match into view and **auto-unfolds** its region. Matches highlighted (current = reversed, others = dim+underline); feedback via `k/N matches` in the stats title while typing and `match k/N` in the message row on `n`/`N`. New response or view toggle clears the search.
- **Copy via OSC 52** — `y` copies the current view's full text, `Y` the cursor's logical line; message row confirms `copied 4.1 KB` / `copied line`, with a `(truncated)` note for capped bodies and a `copied first 1.0 MB of 4.2 MB` note when the 1 MB OSC 52 payload cap kicks in. No native clipboard dep (`tui/clipboard.rs`).
- **Highlight micro-nits** — duplicate-enqueue guard (`pending_highlight: Option<u64>` on `App`; a job whose hash is in flight is not re-sent, cleared when its result lands); CRLF `\r` stripped once where logical lines are materialised, so fold/wrap/search byte ranges stay consistent.

**Verified by**: `cargo test --all` — 355 tests (307 baseline + 48), all green. `cargo fmt --all --check` clean; `cargo clippy --all-targets --all-features -D warnings` clean. New coverage: fold scanner (nested/strings-with-braces/arrays/truncated-no-panic/mismatched-bracket-kinds), wrap index (unicode wide chars, exact-width lines), smart-case matcher (incl. per-char length-shifting case folds via an offset-mapping table), OSC 52 framing, CRLF line index; TestBackend snapshots for headers view, wrap on, folded `⋯ N lines`, cursor row, stats markers; behaviour tests for search nav/wrap/no-match/esc-clears/auto-unfold-while-typing, copy message row, view-toggle reset, headers-view fold notice, zoom stub unchanged.

**Wave 1 known edges / weak spots**: the cursor-row *style* (`theme.selection`) is not asserted (symbol-only snapshots don't capture styles); syntax-highlighting under wrap is unhighlighted by design (fallback above); with wrap on, `n`/`N` scroll to the match's *logical-line start* row, not the exact wrapped sub-row containing the match (accepted).

### Wave 2 — release infra (done)

- **B1 Cargo metadata**: workspace `repository` → `https://github.com/AlsubaieAli/churl`; both crates gain `description`, `keywords` (5), `categories`; churl-core gets `readme = "README.md"`; churl gets `readme = "../../README.md"`; churl depends on churl-core with `version = "0.1.0"` in addition to path. License files `LICENSE-MIT` and `LICENSE-APACHE` added.
- **B2 README.md**: repo root — hero one-liner, CI badge, Install (`curl|sh`, prebuilt binaries table, `cargo install`), Quickstart (mirrors `churl tutorial`), Feature matrix, Screenshot placeholder (`docs/screenshot.png` + TODO), Configuration pointer, License line.
- **B3 `churl tutorial`**: `churl tutorial [--dir DIR]` — scaffolds `./churl-tutorial/` with `churl.toml` (workspace vars + `dev` profile, both pointing at `https://httpbingo.org`), `examples/` collection with `folder.toml`, and 3 endpoints (Get Anything, Post JSON, Bearer Auth with `{{token}}`). All files generated through real persistence seams — no hand-written TOML strings for endpoint or folder files. Refuses non-empty dir. Implemented in `crates/churl/src/tutorial.rs`; 3 CLI integration tests in `tests/cli_tutorial.rs`.
- **B4 Release workflow** (`.github/workflows/release.yml`): tag-triggered (`v*`), `taiki-e/upload-rust-binary-action@v1`, 5-target matrix: `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `x86_64-pc-windows-msvc`; SHA-256 checksums attached. Uses dtolnay/rust-toolchain + Swatinem/rust-cache per-target.
- **B5 `install.sh`**: POSIX sh, OS+arch detection → release target triple, `curl`/`wget` download, SHA-256 verification, extracts to `~/.local/bin` (`--to DIR` override, `--force`, `--dry-run`). Tested with `--dry-run` on darwin/arm64 → correctly resolves `aarch64-apple-darwin`. shellcheck + actionlint installed and run during the main-session review (see below).
- **B6 Cold-start benchmark**: `cargo build --release && hyperfine --warmup 3 'target/release/churl --help'` → **6.3 ms ± 6.3 ms** on darwin/arm64 (well under the 100 ms budget). Clap startup + syntect lazy init keep startup near-zero; no action needed.

**Main-session review round (2026-07-07)**:
- `actionlint` (installed for the review) caught a real workflow bug: the `macos-13` runner label is retired on GitHub-hosted runners — the tag build would have failed. x86_64 darwin now cross-compiles on `macos-latest` (arm64) via `--target`, which the taiki-e action handles.
- `shellcheck` now clean on `install.sh` (one intentional SC2016 literal-`$PATH` hint carries a disable directive).
- Tutorial teachability fix: the `dev` profile originally overrode `base_url` with an *identical* value, demonstrating nothing. Now the workspace defines `greeting = "hello"`, the `dev` profile overrides it to `"hello-from-dev"`, and Get Anything sends `greeting={{greeting}}` — switching profiles visibly changes the echo. Integration test asserts both values.
- **Default `User-Agent` added** (`churl/<version>` in `build_client`) — the live tutorial E2E got **402 from httpbingo.org** because reqwest sends no UA and the service rejects UA-less requests; curl parity confirmed the diagnosis. An enabled user `User-Agent` header still wins per-request. Two new wiremock tests (default sent, user override wins). Found only by live E2E — a real release blocker.
- Live tutorial PTY E2E (7/7 checks): scaffold → open with `--profile dev` → profile indicator → select Get Anything → real send → **200 OK with `hello-from-dev` echoed on the wire** → clean exit.

**Verified by**: `cargo fmt --all --check` clean; `cargo clippy --all-targets --all-features -- -D warnings` clean; `cargo test --all` — **360 tests** (355 baseline + 3 tutorial integration + 2 user-agent wiremock), all green; `cargo publish --dry-run -p churl-core` passes; `cargo package -p churl --no-verify --list` — 70+ files, no fixture bloat; `install.sh --dry-run` on darwin/arm64 prints `aarch64-apple-darwin`; `cargo publish --dry-run -p churl` cannot pass until churl-core is published to crates.io (path+version dep resolves against the registry at verify time).

**Limitation (verbatim)**: `cargo publish --dry-run -p churl` cannot pass until churl-core is published to crates.io — the path+version dep resolves against the registry at verify time, and churl-core is not yet on crates.io. `cargo package -p churl --no-verify --list` passes and the file list is sane. The full publish dry-run will pass once the owner runs `cargo publish -p churl-core` first.

### Review round 3 (owner drive-test 2026-07-07, pre-release — 6 findings, all fixed same-session)

1. **Help overlay ordering**: entries were sorted alphabetically by config name, scattering related keys (`g`/`Shift-g` far apart, `h`/`j`/`k`/`l` split). Now renders in `ACTION_TABLE` order, and the table's movement block was reordered to vim `h/j/k/l`, then `Enter`, `g`/`G`, paging. (The table is also the palette order — the same grouping benefits both.)
2. **Jump-mode pane labels made mnemonic** (owner choice; they were home-row-sequential `a/s/d/f`): `e`xplorer, `u`rl bar, `r`equest, re`s`ponse (`PANE_LABELS` in `jump.rs`); explorer rows use the home-row alphabet minus those four (guard test asserts disjointness).
3. **Collapsed zoom stubs keep their pane chrome** (supersedes round 2's "no full block chrome in the 1-row state"): a collapsed pane is now a 3-row bordered stub — unfocused border + title (jump label included) around the tab-bar/stats summary line (`render_collapsed_stub` in app.rs).
4. **Jump-mode bypassed the zoom invariant** (real bug): jump dispatch assigned `self.focus` directly, so jumping into the collapsed pane didn't auto-unzoom. Now routed through `set_focus` (which also auto-reopens a hidden explorer on `e`). Regression test `jump_into_collapsed_pane_auto_unzooms`.
5. **Focused tab-title shortcut prefixes**: `(1) Params` → `[1] Params` (brackets read as keys; parens stay for row counts).
6. **URL vim-popup footer**: hints moved to the popup's bottom-right and the `NORMAL ·` prefix dropped — edtui's own status line inside the popup already shows the mode; one mode indicator, not two. New snapshot `url_popup_editor` guards against the duplicate.

**Verified by**: `cargo fmt --all --check` clean; `cargo clippy --all-targets --all-features -- -D warnings` clean; `cargo test --all` — **363 tests** (360 baseline + jump-disjointness guard + popup snapshot; jump tests rewritten for the mnemonics), all green; PTY drive of the real binary (jump labels, zoom→jump auto-unzoom, popup footer). Repo URLs corrected `ali-subaie` → `AlsubaieAli` (actual GitHub account) across Cargo.toml, README, install.sh, core README.

**Next**: ship 0.1, then M7.1–M7.5 (owner priority 2026-07-07: interchange/env-editor/pickers/sequences/concurrency before M8/M9)

---

### Review round 4 (owner drive-test 2026-07-07 — vim motions in the edtui editors)

Findings from driving the two edtui editors (URL vim-popup + Body tab), all on edtui 0.11.3:

1. **Missing vim motions**: edtui does not implement `W`, `B`, `f<char>`, `F<char>`, `t<char>`, `T<char>`, and binds first-non-blank only as `_` (not `^`). These are now implemented churl-side in `components/vim_ext.rs` as Normal-mode cursor mutations on `EditorState`, applied uniformly to both edtui editors (an `f`/`F`/`t`/`T` pending-find state is held per-editor: `App.url_popup_vim` / `App.editor_vim`, reset when the popup opens / an endpoint loads). Cols are char positions (`Jagged<char>` is char-indexed) so unicode needs no byte math; cursor col stays clamped to the row's last char.
2. **URL popup swallowed `/`-search** (real bug): `handle_url_popup_key` committed on *any* Enter regardless of edtui mode, so edtui's `/`-search (Enter = FindFirst → jump to match → Normal) could never run. The handler is now mode-aware: in `EditorMode::Search` everything (incl. Enter/Esc) goes to edtui (Enter executes the search, Esc cancels it, never commits); otherwise Enter commits as before, Esc-in-Normal cancels, and Normal-mode vim motions are consulted before edtui fall-through. Accepted edge: Enter while an `f`/`F`/`t`/`T` find is pending still commits.
3. **Body tab**: in Request/Body/Normal, `vim_ext` is consulted before the leader and keymap steps, so `W`/`B`/`^`/`F`/`t`/`T` (all unbound today) work as motions and `f` becomes find-char *inside* the Body editor, shadowing the global Jump key there (M6.6 shadowing precedent; jump stays reachable from every other pane). Precedence matters: a pending find's next char must reach `vim_ext` even when it's Space (leader) or a mapped key. `w` (Save) and `/` (endpoint search) keep their Body-tab meaning — untouched.

**Main-session review fixes (2 real edges the build missed)**: (a) modifier discipline — `Ctrl-f` matched `KeyCode::Char('f')` and armed a pending find, and a pending find resolved `Ctrl-s` as a search for `'s'` (swallowing Send on the Body tab); `vim_ext` now treats only bare/shifted chars as motion input, modified keys abort a pending find. (b) Popup Esc ordering — Esc while a find was pending closed the whole popup (discarding edits) because the Esc-cancel check preceded the `vim_ext` call; reordered so Esc aborts the pending find first (vim), a second Esc cancels.

**Verified by**: `cargo fmt --all --check` clean; `cargo clippy --all-targets --all-features -- -D warnings` clean; `cargo test --all` — **387 tests** (363 baseline + 17 `vim_ext` module units + 4 app-level + 3 review-fix regressions: modified-char discipline, shifted-char works, popup-Esc-aborts-find), all green; PTY drive of the real binary 9/9 (search Enter jumps with popup open, `^`+`f<c>` proven by marker insert, Esc abort vs cancel, Esc revert, Body-tab `f` without jump-mode, clean exit).

---

## M7.1 — Collection interchange (JSON import/export + in-TUI curl paste/copy)

**Scope**: Getting collections in and out of churl (owner requests 2026-07-05 + clarifications 2026-07-07). churl becomes adoptable by people with existing Postman/JSON collections, and requests become shareable without leaving the TUI.

**Deliverables** (all shipped):
- ✅ **JSON collection import** — Postman Collection v2.1 → churl endpoints via the new `churl_core::interchange` module (hand-rolled `serde_json::Value` mapping — no strict schema crate, so messy real-world exports still import). Two entry points: (a) in the TUI — palette `import collection (JSON)` + a path prompt; (b) at launch — global `churl --import-collection <file.json>`, which writes into the cwd workspace, prints a summary, then continues into the TUI. Both share `interchange::write_import`. Imported requests land as normal file-per-endpoint TOML — a one-time conversion, not a second storage format.
- ✅ **Export** — scope-selectable (one **collection** or the whole **workspace**) with a selectable **dialect**: Postman v2.1 (round-trips) or churl-native JSON (lossless). Palette actions + a path prompt pre-filled with `exports/<slug>.json`; writes are guarded to stay inside the workspace root (`..`/absolute escapes refused). Secrets policy carries over: `{{var}}` placeholders exported verbatim, literal secret auth refused (reuses `auth_secret_violations`).
- ✅ **In-TUI curl paste**: palette `paste curl as new endpoint` → prompt → `churl_core::import::import_curl` → new endpoint in the selected collection.
- ✅ **In-TUI copy-as-curl**: `C` in the Request pane (and palette) copies the loaded request as a curl one-liner via `export::export_curl` + OSC 52. Default copies `{{var}}` verbatim; a separate palette variant resolves vars first (explicit secrets-caution opt-in).
- ✅ Tests: Postman import fixtures (nested folders, each auth kind, each body mode incl. an unsupported one, disabled headers, `{{var}}` URL), Postman round-trip (churl → Postman JSON → churl structural equality), native-export validity, secret-refusal on export, `write_import` folder-flattening, CLI parse + fail-loud paths, export path-escape guard, and the palette/keymap TUI wiring.

**Decisions made** (see DECISIONS.md 2026-07-08):
- **Export dialect is user-selectable** (Postman v2.1 for round-trip, churl-native JSON for lossless) — resolves the design-session open question.
- **Nested Postman folders flatten on import** — `folder_path` is joined with `" / "` into a single collection name (nested collection directories are a post-release backlog item). Root-level requests land in a collection named after the import.
- **Postman body-mode limits**: `raw` (Text/Json by `options.raw.language`) and `urlencoded` (→ Form) import; `formdata`/`file`/`graphql` drop the body with a warning. Postman collection `variable[]` is not imported (warned).
- **Query stays in the URL** on Postman import (matches curl-import behaviour — no explosion into params).

**Next**: M7.2

---

## M7.2 — Quick-jump pickers (requests + workspaces)

**Scope**: Telescope/snacks-style navigation (owner addition 2026-07-07) — type-to-search pickers on logical, remappable hotkeys, building on the existing shared picker overlay + nucleo-matcher (M2).

**Deliverables** (all shipped):
- **Request picker** (`<leader>f`, remappable via `[keys.leader]`) — ✅ **reuses the existing endpoint-search overlay** (`Action::OpenSearch` / `Mode::Search` / `explorer.all_endpoints()`) rather than rebuilding a picker: a new `Action::QuickJumpRequests` dispatches to the same `open_search()` code path, so Enter opens the endpoint through the existing dirty-guarded load. The `/` search bind stays as a second entry point. An explicit alternative to `f`-jump navigation.
- **Workspace picker** (`<leader>w`, remappable) — ✅ new `Action::QuickJumpWorkspaces` opens a picker over recently-opened workspaces. Recency lives in the SQLite **state DB** (new `workspaces(path UNIQUE, last_opened_ms)` table, migration 2 — `touch_workspace`/`recent_workspaces` on `HistoryStore`), never in workspace files. Paths are stored canonical/absolute (deduped); `$HOME` collapses to `~` in the display. Enter routes the switch through `guarded_load` via a new `PendingLoad::Workspace(path)` variant (always treated as "other", so a dirty switch defers to the discard-changes confirm). `install_runtime` seeds recency with the launch workspace.
- **Runtime workspace switch** — ✅ new `App::switch_workspace` opens the new manifest (fail-loud on error, current state preserved), rebuilds the explorer against it, and resets every endpoint/workspace-scoped field: `selected`, `loaded_snapshot`, editor body, `active_profile`, response state (+ scroll/cursor/highlight cache), tabs, url editor/popup, in-flight request (aborted), pending load, zoom, and focus (→ Explorer). Empty/absent recency shows a message instead of an empty picker.
- ✅ Both honour the M6.6 dirty guards and, being labelled leader actions, appear automatically in the `?` help overlay + which-key popup (enforced by the help-overlay guard test).
- ✅ Tests: core recency (insert/upsert/order/limit/dedup), `<leader>f` opens search, workspace picker open + empty-list guard, `switch_workspace` state reset + tree reload + recency write, dirty-switch defers to confirm, updated which-key snapshot.

**Decisions**: request picker reuses the endpoint-search overlay (no separate picker); workspace recency lives in SQLite state (never in workspace files); the runtime switch resets endpoint-scoped state through the dirty guard. See DECISIONS.md.

**Next**: M7.3

---

## M7.3 — Environments & vars editor

**Scope**: In-app editing for the M6 three-scope var system (owner request 2026-07-05, clarified 2026-07-07). Today vars/profiles are file-edit-only; this makes them first-class in the TUI.

**Deliverables** (design session first — same simplicity principles as the rest of the TUI, owner requirement):
- **Own layout or large popover pane with split view** (owner's words): left pane selects the scope — whole workspace, a collection, or a profile; right pane edits the selected scope's vars (add / edit / delete).
- Explicit **save mechanism** (owner requirement): `w` to save through the existing format-preserving toml_edit persistence; dirty tracking + discard guard reusing the M6.6 `guarded_load` seams.
- Profile management in the same view: create/rename/delete profiles, edit their vars; active profile stays visible.
- Secrets rules unchanged: name-marker enforcement on save, masked display.
- Resolver chain (cli > profile > collection > workspace > env) is *displayed*, not changed — the editor should make the winning value for a var name visible so precedence stops being a file-reading exercise.
- Tests: editor state machine, save round-trip per scope, dirty-guard paths, snapshots.
- **Crash bugfix (folded in, owner 2026-07-08)**: opening a workspace aborts if a collection dir contains a `churl.toml`. `Collection::endpoints()` (persistence.rs) skips `FOLDER_FILENAME` (`folder.toml`) but **not** `MANIFEST_FILENAME` (`churl.toml`), so a nested workspace manifest is parsed as an `Endpoint` (`missing field 'request'`) and `load_endpoint(&path)?` propagates, killing the whole TUI load (surfaced at `app.rs:2821`). Repro: `churl --import-collection X.json` from a dir whose subdirs are themselves churl workspaces (the demo root). Fix: (1) skip `MANIFEST_FILENAME` in endpoint enumeration like `FOLDER_FILENAME`; (2) robustness — degrade a single unparseable endpoint file to a warning instead of aborting the load, and/or don't treat a subdir that is itself a workspace as a collection. Regression test on the nested-workspace layout.

**Delivered** (2026-07-08):
- **Crash fix + load resilience** — `Collection::endpoints()` now also skips `MANIFEST_FILENAME`; a new `endpoints_lenient() -> CollectionLoad { endpoints, warnings }` degrades a single unparseable endpoint to a warning instead of aborting. The TUI explorer/search load paths use the lenient variant and surface skipped files in the M6.7 message row (never crash, never silent). `read_dir` IO errors stay hard errors. Regression + resilience tests in `churl-core/tests/persistence.rs`.
- **Save-prune correctness gate** — verified the format-preserving `merge_tables` already **prunes** keys/profiles absent from the saved struct (deleted var key gone, deleted `[[profiles]]` entry gone, renamed old name gone, emptied scope's `[vars]` table gone); no save-path change was needed. Proven end-to-end by new round-trip tests that re-read the file and assert the removal, with surviving keys' comments intact.
- **Environments & variables editor** — a near-full-screen split-view modal (`<leader>v` / palette "environments & vars"): left column selects the scope (workspace / each collection / each profile, grouped, active profile marked `●`); right column edits its vars (`a` add, `enter`/`i` value, `r` name, `d` delete). Profile CRUD in the same view (`n` new, `r` rename, `d` delete, `x` set active). Explicit `w` save through the core persistence writers, dirty marker + discard confirm (`s`/`d`/`esc`). Secret-named literals are masked (`••••••`) and refused on save (placeholders pass). Live **precedence display**: each row is tagged ` ✓` (this scope wins) or ` → profile dev` (shadowed), with the full chain in the footer. Save live-refreshes the workspace + explorer so edits take effect without restart. State lives entirely in `churl` (`components/env_editor.rs`); `churl-core` gained nothing UI. Deviations recorded in DECISIONS.md.

**Next**: M7.4

---

## M7.4 — Request sequences (API E2E testing)

**Scope**: Run endpoints in a defined order for end-to-end API testing (owner request from day one, 2026-07-05; promoted from backlog 2026-07-07). Intuitive TUI is a hard requirement (owner 2026-07-07) — same simplicity principles, design session first.

**Deliverables**:
- Sequence definitions live in the workspace as TOML (same file-per-unit, `seq`-ordered philosophy).
- **Value extraction**: pull values out of a step's response (JSONPath or similar) into variables consumed by later steps — extracted values enter the same `{{var}}` chain (single resolver seam, M9 guardrail).
- **Run view**: per-step status/timing as the sequence executes; a failed step halts (continue-on-error as a knob); results inspectable per step in the normal response viewer.
- TUI: create/edit/reorder sequence steps in-app, run on a keybind; sequences visible in the explorer.
- Depends on M3 execution + M6 templating (both done — this was the blocker note in the backlog entry).
- Tests: wiremock multi-step suites (extraction chain, halt-on-fail, var precedence with extracted values), sequence TOML round-trip.

**Status**: **done** (2026-07-08).

**Delivered** (2026-07-08):
- **Core run engine (`churl-core`, UI-free)** — `sequence.rs` holds the run primitives shared by the wiremock-tested `run_sequence` *and* the live TUI driver, so their semantics can't drift: `prepare_step` (resolves an endpoint against the workspace root, rejecting `..`/absolute traversal, and builds the resolver with an ephemeral **`extracted` scope prepended at highest precedence** — the single resolver seam, never forked), `extract_step`, `classify_response`/`classify_step` (the one classify+extract point), `ordered_steps`. `model.rs` gains `Sequence`/`SequenceStep`/`OnError`; `persistence.rs` gains `SEQUENCES_DIRNAME` (excluded from `collections()`), `OpenWorkspace::sequences()` (lenient — one bad file → warning), and `load/save/create/rename/delete_sequence`.
- **Extraction subset grammar** (dependency-free, no JSONPath crate; documented in `sequence.rs` + DECISIONS): `status` → the numeric status; `header:<Name>` → a response header value (case-insensitive; absent = error); `$.a.b[0].c` / `a.b[2]` (leading `$.` optional) → a JSON path walked over `serde_json::Value` (`.key` on objects, `[n]` on arrays). Leaf coercion: string → its value, number/bool → display, object/array → compact JSON; a `null` leaf, missing key, out-of-range index, type mismatch, non-JSON body, or malformed expression are each a distinct `ExtractError`. Any rule failing fails the step (subject to `on_error`).
- **Run view** (`components/sequence_runner.rs`, `Mode::SequenceRunner`) — a large modal (mirrors the env-editor wiring) that drives the run live via the core primitives + an `AppMsg::SequenceStep { run_generation, index, outcome }` handler (stale generations dropped; abort handle for cancel/re-run). Left column = per-step glyph + `METHOD endpoint` + timing + masked extracted values (`token=••••••`); right column = the SELECTED step's response in the **real** `response::render`/`ResponseView`. Keys: `j/k` select, `Tab` focus the viewer (`h` headers · `W` wrap · `o`/`O` fold · scroll), `r` re-run, `Ctrl-C` cancel, `q`/`Esc` close (confirm while running). Trigger: `Action::RunSequence` (`<leader>r` + palette + Enter/jump on a sequence row auto-runs).
- **Explorer integration** — a dim `SEQUENCES` section (`RowKind::Sequence`) shown when the workspace has sequences; Enter/jump opens the runner; load warnings flow through the M7.3 `take_warnings()` → message-row path.
- **In-app editor** (`components/sequence_editor.rs`, `Mode::SequenceEditor`, `Action::EditSequence` = `<leader>a`) — create (name prompt → `create_sequence` → editor), add step (self-contained substring endpoint picker), delete, reorder (`K`/`J` or `[`/`]`, renumbering `seq`), toggle `on_error`, and full per-step extraction-rule CRUD (add/edit-expr/rename/delete) via the shared `LineEditor`; explicit `w` save through `save_sequence`, derived dirty + discard guard. **The §4 editor shipped complete** — the design's extraction-rule-editing escape hatch was not needed.
- **Non-negotiables honoured**: `churl-core` stays UI-free; `execute()` is the only HTTP chokepoint (the runner goes through it); resolver stays the single seam; no new deps; never panics on malformed input / traversal / empty steps / unicode; cancel aborts in-flight work. **Sequence-step executions write no per-endpoint history in v1** (documented; batch history is M7.5).

**Next**: M7.5

---

## M7.5 — Concurrent requests (throttle / load testing)

**Scope**: Fire N copies of one endpoint (or several endpoints) concurrently to expose rate limits and race bugs (owner request from day one, 2026-07-05; promoted from backlog 2026-07-07). Intuitive TUI required — design session first.

**Deliverables**:
- Concurrency runner on M3's task-per-request + `AbortHandle` architecture: N copies, configurable concurrency/pacing (burst vs paced — knobs decided in design session), cancellable as a batch.
- **Results-comparison view**: per-request status/timing side by side; summary line (success/fail counts, min/median/max latency); individual responses inspectable.
- History: batch runs recorded in SQLite without flooding the per-endpoint history view (grouping decided in design).
- Guardrail: this is a testing aid, not a load-cannon — sane default caps, loud confirmation above them.
- Tests: wiremock concurrency suites (N-copy fan-out, cancel mid-batch, timing capture), results-view snapshots.

**Status**: **done** (2026-07-08). **This closes the M7.1–M7.5 block.**

**Delivered** (2026-07-08):
- **Core load engine (`churl-core::load`, UI-free)** — `run_load(client, request, cfg, options)` fires `cfg.total` copies of an *already-resolved* request through the single `execute()` chokepoint, bounded to `cfg.concurrency` in flight via `futures`' `buffer_unordered` and paced by an **absolute-target** delay (copy `i` never launches before `start + i·interval`, so the run's wall-clock has a guaranteed lower bound). Pure `stats()` computes ok/failed/errored counts + **nearest-rank** min/median/p95/max/mean over completed-request timings (all-`None` on an empty/all-errored batch — no panic). `classify()` is the single `Ok`/`Failed`(≥400)/`Error`(transport) seam; `check_config()`/`LoadCaps`/`LoadCheck` classify a config against the guardrail caps. Wiremock-proven: exact N-copy fan-out with timings, **bounded concurrency never exceeds the cap** (an arrival-interval overlap gauge in the responder), 500→Failed, transport→Error, interval-pacing floor, total=0 no-op.
- **History migration 3 — separate `load_batches` table (structural non-flooding)** — a completed (or cancelled) run writes exactly ONE `LoadBatchSummary` row to a NEW table via `insert_load_batch`, never to `history`. Load runs therefore **cannot** flood the per-endpoint history view (proven: after batches are recorded, the history query still returns only history rows). Migration applies cleanly from a v2 DB.
- **`[load]` config caps** — an optional `[load]` table (`warn_total`/`warn_concurrency`/`max_total`/`max_concurrency`) folds over `LoadCaps::default()` (100/20/10 000/200) via `Config::load_caps()`; a malformed value fails the whole config parse loudly, like every other knob.
- **TUI load runner** (`components/load_runner.rs`, `Mode::LoadRunner`, `Action::OpenLoadRunner` = `<leader>l` + palette "load test endpoint (concurrent)") — a large modal with an **editable config header** (total/concurrency/interval, edited via the M7.3 `LineEditor` field-row pattern, digits-only + clamp), a live **O(viewport)** results list (glyphs `·`/`◐`/`✓`/`✗`/`⚠`), the reused `response::render` viewer for any selected copy, and a live stats line (`12/50 done · 44 ok · 6 failed · min … p50 … p95 … max … mean …`). Opens with the request resolved **once** exactly like an interactive send (endpoint clone + body editor + `build_resolver`/`substitute_request`); never auto-runs. `r` runs, `Ctrl-C` cancels, `q`/`Esc` closes (confirm while running).
- **Single-launcher fan-out + real batch cancel** — `App` owns ONE launcher task whose `buffer_unordered` mirrors `run_load`; aborting its `AbortHandle` drops the fan-out and every in-flight reqwest future (there is **no** detached per-request `tokio::spawn` to escape cancellation). A per-run `run_generation` drops stale results after cancel/re-run. Proven live: a multi-thread wiremock test cancels mid-batch and asserts the server saw only a handful of the 20 copies (the un-launched ones never fired).
- **Guardrail** — `check_config` Refuse blocks the run with a message; Warn shows a loud confirm **naming the target URL and count** (`Fire N requests at concurrency C against <url>?`), `y` proceeds / `n` aborts; Ok runs immediately. Caps are `[load]`-overridable.
- **Non-negotiables honoured**: `churl-core` stays UI-free; `execute()` is the only HTTP chokepoint (both `run_load` and the TUI launcher go through it); the request is resolved once and cloned for all N copies; bounded concurrency actually bounds; cancel aborts all in-flight; stale results dropped by generation; never panics (total 0/1, huge N refused by the cap, all-errored, percentile of empty, unicode URL); the results list renders O(viewport), not O(N).
- **Deviations** (recorded in DECISIONS.md): `buffer_unordered` instead of a manual `FuturesUnordered`+`Semaphore` (same bound, less code); absolute-target pacing; an added `LoadStarted` launcher message so a running copy shows its glyph honestly; a `cancelled` column added to `load_batches` (the DDL in the design omitted it but §1d/§2 require marking a cancelled run); `tokio`+`futures` promoted to `churl-core` direct deps (used only inside `run_load`; `execute` stays runtime-agnostic).
- **Fix round (pre-merge polish)**: all three batch-interrupt paths (Ctrl-C cancel, `r` re-run mid-batch, close mid-batch) now route through one `interrupt_running_batch` seam that records the partial cancelled summary before aborting — a partial run is never lost from `load_batches`. `LoadStats.mean` is surfaced in the stats line and persisted via **migration 4** (`ALTER TABLE load_batches ADD COLUMN mean_ms` — appended, not by editing migration 3, so it lands on already-v3 DBs too; regression-tested). See DECISIONS.md "M7.5 fix round".

**Next**: M7.6

---

## M7.5.3 — Clipboard cross-platform compat (native + OSC-52 passthrough)

**Scope**: Bug-fix milestone (owner report). Copy-as-curl (`C`) and response copy (`y`/`Y`) never reached the system clipboard for tmux users: the M7-wave-1 path emitted a *raw* OSC 52 escape (which tmux/screen swallow without passthrough-wrapping) and reported success unconditionally. Owner requirement: copy must work across almost all terminals, multiplexers, and operating systems (macOS, Windows, Linux X11 natively; pure Wayland via the OSC 52 fallback — native Wayland is a follow-up).

**Deliverables**:
- ✅ **Layered clipboard** (`tui/clipboard.rs`): `copy(payload, out) -> CopyOutcome` tries the **native OS clipboard first** (`arboard` — cross-OS), falling back to **OSC 52 wrapped for the active multiplexer** (tmux/screen DCS passthrough, else raw). `MAX_COPY_BYTES` cap + char-boundary truncation preserved.
- ✅ **Honesty**: the silent `let _ =` is gone. The run loop reports the real outcome — "copied…" only when a path succeeded, "copy failed" otherwise. Resolved-vars secret warning preserved.
- ✅ **Dep**: `arboard = { version = "3.6", default-features = false }` on the churl bin only (never churl-core). Already a transitive dep via edtui, so no new packages and no `ci.yml` change (its Linux backend `x11rb` is pure-Rust, no system libs).
- ✅ **Tests**: pure logic only (OSC 52 framing, tmux/screen passthrough wrapping, multiplexer detection, payload capping, `CopyOutcome`) — `cargo test` never calls `arboard` (headless CI has no clipboard). Copy messages asserted via `App::pending_copy_message`.

**Reverses** the M7-wave-1 "OSC 52, no native dep" decision (DECISIONS.md).

**Next**: M7.6

---

## M7.6 — Interchange parity (churl-native JSON import)

**Scope**: Close the import/export asymmetry from M7.1 (owner note 2026-07-08). M7.1 shipped Postman v2.1 JSON import + selectable-dialect export (incl. a churl-native JSON dialect), but import only accepts Postman — a file `churl export` produces in the native dialect can't be re-imported. Import should accept **every dialect export can produce**.

**Deliverables**:
- **churl-native JSON import** alongside the existing Postman v2.1 importer; dialect detected from the payload (or an explicit flag), routed through the shared collection-build seam from M7.1.
- Symmetry contract: any file emitted by `churl export` (collection or workspace scope, any dialect) round-trips back through import. Add a round-trip corpus test asserting export→import→export stability, mirroring the M4 curl round-trip corpus.
- Reuse the M7.1 F1/F4/F5 guarantees (writes a `churl.toml` so the TUI shows the import; slug-collision handling is explicit, not silent-merge).

**Next**: R2 (per the 2026-07-09 re-sequence)

---

## M7.7 — Response formatting + help search (UX / viewer round)

**Scope**: A focused viewer/overlay UX round (owner notes 2026-07-08). Two independent polish items that both live in the response viewer / overlay code, grouped like the earlier M6.x UX rounds.

**Deliverables** (design session first):
- **Response pretty-printer / reformatter**: re-indent structured bodies for readability — not just soft-wrap. Minified/single-line JSON (e.g. httpbin's) renders on one line today and `W` wrap doesn't help. Raw↔pretty toggle; pretty-by-default for known content-types. Covers **JSON**, **HTML**, and **XML**. Must interact cleanly with the existing display pipeline (syntect highlighting, JSON folding `o`/`O`, wrap, body search) — reformat is a transform *before* the fold/wrap/viewport stages, keeping the single content-type→formatter mapping point (`SyntaxToken::from_content_type`, M7-viewer plugin guardrail) rather than forking per-format paths.
- **Help-overlay `/` quick-search**: `/` inside the `?` help pane filters/jumps to a binding, reusing the response body-search pattern (`/` `n` `N`, smart-case). Keeps the live-KeyMap-driven help from M6.7.
- Tests: reformat round-trip + idempotence per content-type, pretty↔raw toggle preserves search/fold state, help-search filter/jump snapshots.
- Also fold in (from the 07-09 sweep): **control-char / ANSI sanitize** + explicit tab-width in the viewer, and the **horizontal-window slice** for unwrapped long lines (the minified-line perf cliff — reformat addresses the common case, the window slice bounds the rest).

**Next**: R1 (per the 2026-07-09 re-sequence)

---

## M7.8 — Lifecycle & distribution (version pinning, self-update, uninstall)

**Scope**: Install/version/lifecycle cluster (owner notes 2026-07-08; `churl update` elevated from the post-release backlog). Three items that cohere around "what happens to an installed binary over time" and pair with the M7 installer.

**Deliverables** (design session first):
- **Per-project version pinning**: `churl.toml` declares a compatible churl version (a `churl_version`/compat field). On workspace open, compare against the running binary — **older** pin → warn and run (assume backward-compatible); **newer** pin → prompt to update (or refuse to open). Manifest field + open-time check; masks nothing about the resolver/persistence behaviour.
- **`churl update` / `churl upgrade` self-update**: update the installed binary in place (fetch the latest release artifact matching the platform, verify, atomic-replace). Pairs with the version-pin prompt and the M7 `install.sh` release layout.
- **Uninstall path**: a `churl uninstall` subcommand (and/or an `uninstall.sh`) that removes the binary and optionally state/config in the OS data dir, with a README "Uninstalling" section. Counterpart to `install.sh`.
- Tests: manifest compat parse + older/newer/equal decision matrix, self-update artifact-resolution + replace (mocked), uninstall dry-run enumeration.
- Note: package-manager distribution (Homebrew tap / AUR) stays in the backlog — this milestone is the built-in lifecycle commands, not third-party channels.
- Note: `churl update` self-update must reuse R0's atomic-replace helper.

**Next**: R3 (per the 2026-07-09 re-sequence)

---

## M7.9 — Unified creation flow (`<leader>n`)

**Scope**: Owner notes 2026-07-09. Consolidate creating a collection / endpoint / sequence under one `<leader>n` submenu (builds on the nested-leader infra shipped in the leader-submenus PR), and make endpoint creation richer. Replaces today's scattered creation entry points (`n`/`N` in the explorer, `<leader>s a` for sequences) with one discoverable, noun-first menu.

**Deliverables** (design session first):
- **`<leader>n` creation submenu**: opens a which-key submenu to create a **collection**, an **endpoint**, or a **sequence**.
- **Endpoint creation picks a collection**: creating an endpoint prompts for a target collection (reuse the picker); if none is chosen, create it at the **workspace root**.
- **Endpoint from curl**: creating an endpoint can start from a **pasted curl command** — parse it into an endpoint (reuse the `churl-core::import` curl parser + the in-TUI curl-paste seam from M7.1).
- Tests: submenu routing; endpoint-create in a chosen collection vs. root; curl-paste → endpoint round-trip.

**Next**: M8

---

## M8 — Cookies + proxy

**Scope**: Session and network-environment support (promoted from the backlog, owner decision 2026-07-05; since 2026-07-07 it follows the M7.1–M7.5 block — owner priority puts those before everything else post-release).

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

Not yet scheduled into milestones; each becomes a milestone (or folds into an existing one) when picked up.

> Requirements-drop #5 (2026-07-07): JSON collection import/export, in-TUI curl paste/copy-as-curl, and in-app environment editing were owner requests from the project's start but never reached this backlog or any milestone — the owner caught it at ship-readiness review. All three are now scheduled (M7.1–M7.3) alongside the two backlog promotions (M7.4/M7.5).

- ~~Auth types~~ → **promoted to milestone M5** in the 2026-07-05 plan review (OAuth2 client-credentials remains here as backlog).
- ~~Request sequences (API E2E testing)~~ → **promoted to milestone M7.4** (owner priority 2026-07-07: before plugins, right after release).
- ~~Concurrent requests (throttle / race-condition testing)~~ → **promoted to milestone M7.5** (owner priority 2026-07-07).
- ~~Cookies / sessions~~ → **promoted to milestone M8** (owner decision 2026-07-05).
- ~~Proxy configuration + per-request TLS-skip~~ → **promoted to milestone M8** (owner decision 2026-07-05).
- **Multipart (`-F`) bodies** (approved, owner decision 2026-07-06 — resolves the M4 open question): multipart/form-data as a model feature — multi-part body (fields + file refs), TUI body-type editing, reqwest multipart execution, `-F` import/export remap replacing the hard `Unsupported` error. Slot after M8.
- **Nested folders inside collections** (owner question 2026-07-07 — surfaced as requirements-drop #4: promised in the kickoff prompt *and* M2's deliverable line "collection → folder → endpoint navigation", deferred in M2's notes "until persistence grows folders", never rescheduled): folder = subdirectory inside a collection dir (each with its own `folder.toml` vars, extending the resolver chain one level), lazy loading, explorer tree gains the folder level, CRUD (create/rename/delete folder), `seq` ordering within folders. Recommended slot: first post-release, ahead of or with M8 (model surgery — keep it out of the M7 release run). Owner to confirm placement.

### Deferred nits (from M2/M3 reviews)

- ~~Explorer pane has no scroll offset — a tree taller than the pane runs off-screen.~~ **Fixed in M3** (`ExplorerState::scroll_to_fit` keeps the selection in the viewport).
- ~~Ctrl-C/Ctrl-S consumed by edtui in insert mode.~~ → **Scheduled into M4** (owner decision 2026-07-05: intercept both before edtui — they are not text-input keys).
- Highlight job re-enqueued while an identical job is in flight; CRLF bodies keep `\r` in the line index → **scheduled into M7** polish.
