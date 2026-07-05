# churl — Decision Log (ADRs)

Entries are append-only; one line per decision, date-prefixed. Superseding decisions reference the one they replace.

---

## 2026-07-05

- **Name: churl** — "curl" in a TUI shell; short, memorable, not taken on crates.io at decision time.
- **Default branch: master** — matches the host platform's default; no organisational convention overrides it for this personal project.
- **HTTP client: reqwest 0.13 + rustls** — async-native, pure-Rust TLS, no OpenSSL dep. Coarse timing (connect + total) is sufficient for M0–M3; a custom hyper connector (DNS/connect/TLS split timing) is documented as the later upgrade path when fine-grained waterfall data is wanted.
- **TOML library: toml_edit** — format-preserving; users edit endpoint files by hand and expect their comments and ordering to survive a round-trip through churl. `toml` (serde-only) would silently destroy formatting.
- **File-per-endpoint + explicit `seq` field** — learned from Bruno: monolithic collection files (Slumber, Insomnia) create merge conflicts and make git history noisy. One file per endpoint scales to large collections; `seq` provides stable ordering without relying on filesystem order.
- **Templating: hand-rolled `{{var}}` substitution** — no JS runtime (ATAC carries a ~10 MB binary bloat for Deno); no Tera/Handlebars (overkill for URL/header/body variable injection). Precedence: CLI `--var` flag → named profile → process environment. Secrets are process-env only; never written to synced files.
- **Secrets hygiene: never in workspace files** — workspace = a git repo users commit and sync. Secrets go in process env or a local `.env` file the user adds to their `.gitignore`. churl never reads, writes, or suggests committing secrets.
- **Local state: rusqlite (bundled)** — history, cookies, and cached sessions are high-churn and relational. `sled` (considered) was abandoned upstream. SQLite with bundled compilation avoids a system dep and is universally available. Schema migrations run at startup.
- **Syntax highlighting: syntect + two-face, lazy, off-thread, viewport-only** — ~23 ms per viewport is the measured cold-start risk; background thread + viewport hash cache keeps the render loop smooth. Whole-document highlighting of a 1 MB response is never done.
- **Vim-modal editing: edtui** — `tui-textarea` has been unmaintained since 2024; `edtui` is actively developed and ships modal vim bindings out of the box.
- **Keybindings: crokey + semantic Key→Action map** — follows gitui/helix model; users remap actions, not raw keys. Prevents keybinding spaghetti as the feature set grows.
- **Fuzzy search: nucleo** — same engine as Helix; outperforms skim/fuzzy-matcher at scale; async-native.
- **HTTP mocking in tests: wiremock** — spins a real HTTP server in-process; tests exercise the full reqwest stack without network access.
- **Snapshot tests: insta + ratatui TestBackend** — deterministic pixel-level assertions for TUI rendering; catch accidental layout regressions.
- **No HTTP-semantic caching** — churl is a development/testing tool. Returning a cached response when the user hits "send" is a footgun. History (SQLite) covers the recall use case.
- **Async runtime: tokio** (arrives M2) — not added in M0. Plain synchronous `crossterm::event::read()` is sufficient for the placeholder TUI; the full `tokio::select!` event loop comes with request execution in M2/M3.
