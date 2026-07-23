# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.10.0](https://github.com/AlsubaieAli/churl/compare/v0.9.1...v0.10.0) - 2026-07-23

### Added

- response output — save to file + HTML/XML pretty-print (M8.7) ([#118](https://github.com/AlsubaieAli/churl/pull/118))
- *(tui)* request body browse/edit gate + de-globalize body-type (M8.6.1) ([#117](https://github.com/AlsubaieAli/churl/pull/117))
- multipart / file upload (M8.6) ([#115](https://github.com/AlsubaieAli/churl/pull/115))

## [0.9.1](https://github.com/AlsubaieAli/churl/compare/v0.9.0...v0.9.1) - 2026-07-21

### Fixed

- *(tui)* settings save-fidelity + drive-test polish (M8.5.3) ([#113](https://github.com/AlsubaieAli/churl/pull/113))

## [0.9.0](https://github.com/AlsubaieAli/churl/compare/v0.8.0...v0.9.0) - 2026-07-20

### Added

- *(tui)* settings panel UX polish (J/K adjust, leader-key capture, MB units, inline descriptions) ([#112](https://github.com/AlsubaieAli/churl/pull/112))
- cookie model — SameSite + manual add/edit (M8.5.1) ([#111](https://github.com/AlsubaieAli/churl/pull/111))
- centralized settings panel + config writer ([#109](https://github.com/AlsubaieAli/churl/pull/109))

## [0.8.0](https://github.com/AlsubaieAli/churl/compare/v0.7.0...v0.8.0) - 2026-07-19

### Added

- U3 — endpoint-level extraction into session vars ([#108](https://github.com/AlsubaieAli/churl/pull/108))
- U6 — curl-import naming standard + collision prompt ([#107](https://github.com/AlsubaieAli/churl/pull/107))
- U2 — Ctrl-s sends the hovered endpoint when none is open ([#105](https://github.com/AlsubaieAli/churl/pull/105))
- U-track Batch A — response/load-runner presentation polish (U4/U5/U7/U8) ([#104](https://github.com/AlsubaieAli/churl/pull/104))
- headless load runs — load fires N concurrent copies and asserts on aggregate stats ([#103](https://github.com/AlsubaieAli/churl/pull/103))
- headless sequence runs — run-seq streams NDJSON, per-step assertions, exit 1 on failure ([#102](https://github.com/AlsubaieAli/churl/pull/102))
- debugging shell — Inspector, Log panel, Traffic feed, headless -v trace, advanced settings ([#101](https://github.com/AlsubaieAli/churl/pull/101))
- response assertions — --assert, exit 1, machine-readable pass/fail ([#99](https://github.com/AlsubaieAli/churl/pull/99))

### Other

- U1 — route sequence add-step through the shared picker ([#106](https://github.com/AlsubaieAli/churl/pull/106))

## [0.7.0] - 2026-07-17

- feat: CLI & headless (agent-first, human-second) — M8.2 (#97)

## [0.6.0](https://github.com/AlsubaieAli/churl/compare/v0.5.0...v0.6.0) - 2026-07-16

### Added

- quick-wins batch — non_exhaustive, response J/K nav, two-stage curl prompt ([#96](https://github.com/AlsubaieAli/churl/pull/96))

### Other

- sync roadmap to locked 1.0 plan; enforce Serena; tidy config ([#94](https://github.com/AlsubaieAli/churl/pull/94))

## [0.5.0](https://github.com/AlsubaieAli/churl/compare/v0.4.0...v0.5.0) - 2026-07-16

### Fixed

- multi-line curl paste import + response-cursor visibility ([#92](https://github.com/AlsubaieAli/churl/pull/92))

## [0.4.0](https://github.com/AlsubaieAli/churl/compare/v0.3.0...v0.4.0) - 2026-07-15

### Added

- per-endpoint insecure-TLS with cookie, proxy, and curl-import hardening ([#90](https://github.com/AlsubaieAli/churl/pull/90))
- cookie jar, proxy, and insecure-TLS session controls with an in-TUI Options overlay ([#88](https://github.com/AlsubaieAli/churl/pull/88))

## [0.3.0](https://github.com/AlsubaieAli/churl/compare/v0.2.0...v0.3.0) - 2026-07-14

### Added

- unified creation, tree CRUD, and writable session vars (M7.12) ([#87](https://github.com/AlsubaieAli/churl/pull/87))
- hierarchy model — nested collections + root-level endpoints (M7.9) ([#86](https://github.com/AlsubaieAli/churl/pull/86))
- *(tui)* <leader>r reloads the workspace from disk ([#85](https://github.com/AlsubaieAli/churl/pull/85))
- *(http)* cross-origin redirect policy that strips auth headers by default ([#83](https://github.com/AlsubaieAli/churl/pull/83))
- *(secrets)* baseline-aware save-gate with grandfathering, request-wide coverage, and secret_policy ([#82](https://github.com/AlsubaieAli/churl/pull/82))
- lifecycle & distribution — self-update, uninstall, version pinning ([#81](https://github.com/AlsubaieAli/churl/pull/81))
- Windows installer + native Wayland clipboard, proven on a cross-platform CI matrix ([#80](https://github.com/AlsubaieAli/churl/pull/80))
- *(import)* churl-native JSON import with auto-detect dispatch ([#79](https://github.com/AlsubaieAli/churl/pull/79))

### Fixed

- *(tui)* reliability nits — in-flight-on-quit history, sticky history-fail indicator, highlight-spawn degrade (R1.5 B) ([#72](https://github.com/AlsubaieAli/churl/pull/72))

### Other

- strip milestone/PR archaeology from code comments ([#78](https://github.com/AlsubaieAli/churl/pull/78))
- *(tui)* extract response viewer state + geometry into child modules ([#77](https://github.com/AlsubaieAli/churl/pull/77))
- restructure project docs for public release ([#73](https://github.com/AlsubaieAli/churl/pull/73))
- *(tui)* cache response-view geometry to stop per-frame full-body recompute (R1.5 A3) ([#71](https://github.com/AlsubaieAli/churl/pull/71))
- *(tui)* consolidate picker into a data-carrying Picker enum (R1.5 A2/H3) ([#70](https://github.com/AlsubaieAli/churl/pull/70))
- *(tui)* fold sequence state into Mode::Sequence variant (R1.5 A2) ([#69](https://github.com/AlsubaieAli/churl/pull/69))
- *(tui)* fold env_editor + load_runner state into Mode variants (R1.5 A2) ([#68](https://github.com/AlsubaieAli/churl/pull/68))
- *(core,tui)* final module trim — app state/vars/workspace, env_editor edit, persistence merge/naming (M7.11) ([#66](https://github.com/AlsubaieAli/churl/pull/66))
- *(tui)* split response/env_editor/events into child modules (M7.11) ([#65](https://github.com/AlsubaieAli/churl/pull/65))
- *(tui)* extract send/sequence/editing/crud handlers to app/handlers/ (M7.11) ([#64](https://github.com/AlsubaieAli/churl/pull/64))
- *(tui)* extract buffers/help/env_editor/load_runner/response handlers to app/handlers/ (M7.11) ([#63](https://github.com/AlsubaieAli/churl/pull/63))
- *(tui)* split app.rs foundation — app/mod.rs + app/pure.rs + app/render.rs (M7.11) ([#62](https://github.com/AlsubaieAli/churl/pull/62))
- *(tests)* extract inline test modules to sibling files (M7.11 Phase 0) ([#60](https://github.com/AlsubaieAli/churl/pull/60))

### Added

- `churl update` — verified, reversible in-place self-update from the latest GitHub release
- `churl uninstall` — remove the binary, with `--purge` to also delete churl's config and local state
- optional `.churl-version` workspace pin — warns when the running binary differs, never blocks

## [0.2.0](https://github.com/AlsubaieAli/churl/compare/v0.1.3...v0.2.0) - 2026-07-11

### Added

- *(tui)* ephemeral peek+copy for masked secret values (note #3) ([#56](https://github.com/AlsubaieAli/churl/pull/56))
- *(tui)* refine tab chips + <leader>t <n> numbered jump (note #5) ([#57](https://github.com/AlsubaieAli/churl/pull/57))
- *(tui)* delete a sequence from the Sequences pane (note #6) ([#54](https://github.com/AlsubaieAli/churl/pull/54))
- extraction rule → in-memory Session variable (note #6) ([#50](https://github.com/AlsubaieAli/churl/pull/50))
- *(tui)* extraction-grammar guidance in the sequence rule editor (note #5) ([#49](https://github.com/AlsubaieAli/churl/pull/49))
- *(tui)* Ctrl-j/Ctrl-k reorder steps in the sequence editor (note #4) ([#48](https://github.com/AlsubaieAli/churl/pull/48))
- *(tui)* filled-chip tab style with borders + gaps (note #7) ([#46](https://github.com/AlsubaieAli/churl/pull/46))
- *(tui)* · sorted marker when A→Z key sort is active (note #1) ([#45](https://github.com/AlsubaieAli/churl/pull/45))
- *(tui)* line-number gutter in the response viewer (default on) ([#43](https://github.com/AlsubaieAli/churl/pull/43))
- *(tui)* M7.7 close-out — viewer sanitize, tab-width, horizontal-window slice ([#42](https://github.com/AlsubaieAli/churl/pull/42))
- *(tui)* / search in the help overlay (M7.7 stage B) ([#40](https://github.com/AlsubaieAli/churl/pull/40))
- *(tui)* optional A→Z key-sort toggle in pretty JSON view (M7.7) ([#39](https://github.com/AlsubaieAli/churl/pull/39))
- *(tui)* JSON response reformatter + pretty toggle (M7.7 stage A) ([#38](https://github.com/AlsubaieAli/churl/pull/38))
- *(tui)* sequence Mode header + runner/load pane legibility (M7.10 stage C) ([#36](https://github.com/AlsubaieAli/churl/pull/36))
- *(tui)* 4-region Tab, pane-only f-jump, cycle-region, remove <leader>S (M7.10 stage B) ([#35](https://github.com/AlsubaieAli/churl/pull/35))
- *(keymap)* dynamic leader submenus + load-time conflict warnings (M7.10 stage A) ([#34](https://github.com/AlsubaieAli/churl/pull/34))
- R0 cheap-P0 durability — atomic saves + load-runner memory bound ([#33](https://github.com/AlsubaieAli/churl/pull/33))
- *(tui)* D1 demo-stabilize — peek-symmetry, sequence run-chooser, cancel timing, env msg ([#29](https://github.com/AlsubaieAli/churl/pull/29))
- *(tui)* multi-endpoint tabs/buffers (stage 2) ([#27](https://github.com/AlsubaieAli/churl/pull/27))
- *(tui)* sequences sub-pane — toggle-able, out of the explorer tree, mutually-exclusive zoom ([#25](https://github.com/AlsubaieAli/churl/pull/25))
- *(tui)* leader submenus (sequences/load) + unified sequence surface with edit⇄run switcher ([#24](https://github.com/AlsubaieAli/churl/pull/24))
- *(tui)* picker vim-nav (ctrl-j/k), proportional picker, copy-as-curl on <leader>y ([#23](https://github.com/AlsubaieAli/churl/pull/23))
- load-runner UX polish (response pane, value steppers, grouped stats, headers hint) ([#20](https://github.com/AlsubaieAli/churl/pull/20))
- concurrent request load testing with bounded concurrency and live stats ([#18](https://github.com/AlsubaieAli/churl/pull/18))
- request sequences for end-to-end API testing ([#17](https://github.com/AlsubaieAli/churl/pull/17))
- environments & variables editor + collection-manifest crash fix ([#16](https://github.com/AlsubaieAli/churl/pull/16))
- quick-jump request + workspace pickers ([#13](https://github.com/AlsubaieAli/churl/pull/13))
- collection interchange — Postman JSON import/export, curl paste/copy ([#14](https://github.com/AlsubaieAli/churl/pull/14))

### Fixed

- *(durability)* R1 — reserved-name guards, SQLite WAL+migration lock, comment-preserving array merge, memory/disk bounds ([#59](https://github.com/AlsubaieAli/churl/pull/59))
- *(tui)* D2 drive-test fixes — env copy regression, runner→edit focus, global {/} tab nav, s discoverability ([#58](https://github.com/AlsubaieAli/churl/pull/58))
- *(tui)* session-marker contrast + env g/G nav + no silent copy no-ops (notes #1/#2) ([#55](https://github.com/AlsubaieAli/churl/pull/55))
- *(tui)* honest, yankable failed-response row in the unified viewer ([#53](https://github.com/AlsubaieAli/churl/pull/53))
- *(core)* fail loudly on unresolved {{var}} instead of shipping a literal ([#52](https://github.com/AlsubaieAli/churl/pull/52))
- *(tui)* focus + inform the empty sequences pane on f-jump (note #3) ([#47](https://github.com/AlsubaieAli/churl/pull/47))
- *(tui)* M7.10 drive-test follow-ups — nav, keymap, response hint ([#37](https://github.com/AlsubaieAli/churl/pull/37))
- *(tui)* Tab skips a collapsed explorer instead of reopening it ([#32](https://github.com/AlsubaieAli/churl/pull/32))
- clipboard copy reaches the system clipboard (native arboard + OSC 52 passthrough fallback) ([#22](https://github.com/AlsubaieAli/churl/pull/22))
- gate [h] headers hint on focus + correct load-runner idle copy ([#21](https://github.com/AlsubaieAli/churl/pull/21))
- unify pane navigation laws across zoom and the load runner ([#19](https://github.com/AlsubaieAli/churl/pull/19))

### Other

- *(tui)* unify runner response viewers with the main pane (note #2) ([#44](https://github.com/AlsubaieAli/churl/pull/44))
- codebase-wide comment cleanup + M7.11 modularization milestone ([#41](https://github.com/AlsubaieAli/churl/pull/41))
- *(comments)* condense verbose comments (first pass — response, persistence, env editor) ([#28](https://github.com/AlsubaieAli/churl/pull/28))
- *(tui)* per-endpoint Buffer model (stage 1 of tabs/buffers) ([#26](https://github.com/AlsubaieAli/churl/pull/26))

## [0.1.3] - 2026-07-07

- fix(ci): authenticate the force-release git push (#11)
- ci: force-release workflow for installer/infra changes (#10)
- fix(installer): correct checksum asset name (404'd every install) (#9)
- ci: serialize release-plz runs and claim latest on finalize (#8)

## [0.1.2](https://github.com/AlsubaieAli/churl/compare/v0.1.1...v0.1.2) - 2026-07-07

### Other

- auto-label PRs by title type + document install/update paths ([#4](https://github.com/AlsubaieAli/churl/pull/4))

## [0.1.1](https://github.com/AlsubaieAli/churl/compare/v0.1.0...v0.1.1) - 2026-07-07

### Other

- *(release)* automated release train — release-plz + conventional-commit PR titles
