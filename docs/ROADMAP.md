# churl — Roadmap

The authoritative roadmap. Detailed build tracking lives with the maintainers.

## Legend

**Milestone classes** — **F** feature · **R** regression-hardening · **D** debug/drive-test · **refactor** behaviour-preserving.

**Status** — ✅ shipped · 🚧 in progress · ⏳ planned · 🔭 exploring.

## Shipped ✅

**0.2.0** — the TUI grows into a full request workbench.
- Collection interchange — Postman JSON import/export, plus curl paste/copy inside the TUI.
- Interchange parity — churl-native JSON import, symmetric with the existing export, with dialect auto-detected from the file's envelope.
- Environments & variables editor — manage workspace, collection, and profile vars, with live precedence display and masked secrets.
- Quick-jump pickers for requests and workspaces.
- Request sequences — chain requests into end-to-end flows, extracting values from each response to feed the next (with an in-memory Session store).
- Concurrent load testing — fire throttled batches with live stats and bounded, memory-safe retention.
- Response viewer polish — JSON pretty-printing, fold/wrap, in-viewer search, line-number gutter, control-char/ANSI sanitizing.
- Navigation & keymap unification — a 4-region Tab model, jump-to-pane, and a fully data-driven remappable keymap with load-time conflict warnings.
- Durability hardening — atomic saves, SQLite WAL + migration locking, comment-preserving TOML merges, reserved-name guards, and bounded memory/disk growth.
- Cross-platform proof — macOS + Windows CI matrix, `cargo-deny`, `install.ps1`, native Wayland clipboard.

**0.1.x** — first public release and the automated release train.
- The core TUI: three-pane layout, vim-style navigation, request execution, virtualized response rendering, and history.
- curl import/export with a strict round-trip corpus.
- First-class auth (basic / bearer / API-key), themes, keymaps, jump-mode, and `{{var}}` templating.
- In-app request editing and collection CRUD.
- Prebuilt binaries + `curl | sh` installer; automated release-plz + conventional-commit pipeline.

## In progress 🚧

- **Lifecycle & distribution** — self-update, uninstall, version pinning. (F)

## Planned ⏳

- **Secret & request safety hardening** — tighter placeholder gating, broadened secret markers, cross-origin redirect policy. (R)
- **Unified creation flow** — one `<leader>n` gesture to create a collection, endpoint, or sequence. (F)
- **Cookies + proxy** — cookie-jar persistence and HTTP(S) proxy support. (F)

## Exploring 🔭

- **Plugin system** — community extensibility for auth schemes, body types, and viewers. (F)
