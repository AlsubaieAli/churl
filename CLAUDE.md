# churl — Claude Code Instructions

## Build / test / run

```sh
# Build (debug)
cargo build

# Build (release)
cargo build --release

# Run all tests (workspace)
cargo test --all

# Format check
cargo fmt --all --check

# Lint (must be warning-free)
cargo clippy --all-targets --all-features -- -D warnings

# Run the TUI
cargo run -p churl

# Run with a subcommand
cargo run -p churl -- import "curl https://example.com"

# Accept new insta snapshots, then re-run clean
INSTA_UPDATE=always cargo test --all
cargo test --all
```

## Workspace layout

```
Cargo.toml                 # workspace root, shared deps / package metadata
crates/
  churl-core/              # pure library — zero TUI deps, ever
    src/
      lib.rs               # pub const VERSION + module exports
      model.rs             # Method, Endpoint, Request, Response, Header, Param, Profile, Workspace
      persistence.rs       # toml_edit load/save (format-preserving merge), lazy OpenWorkspace/Collection
      config.rs            # global config.toml loading (incl. [keys] overrides, timeout_secs, max_body_bytes)
                           #   + secrets heuristics
      history.rs           # rusqlite HistoryStore, user_version migrations
      http.rs              # reqwest+rustls execute(client, request, &ExecuteOptions); streamed body cap →
                           #   Response.truncated; build_client(timeout); runtime-agnostic (no AbortHandle in core)
      import.rs            # curl command → Endpoint (shlex + strict flag map; unknown flag = hard error)
      export.rs            # Endpoint → curl command (shlex::try_quote; round-trip contract with import)
    tests/
      persistence.rs       # comment-preservation corpus, manifest+secrets, lazy loading
      roundtrip_prop.rs    # proptest Endpoint round-trip
      curl_roundtrip.rs    # ≥20-command curl import→export→import corpus
      http.rs              # wiremock execution suite incl. body-size cap
      fixtures/            # comment-bearing endpoint TOML fixtures
  churl/                   # binary crate + thin lib for integration tests
    src/
      lib.rs               # pub mod tui (re-export for tests)
      main.rs              # Cli (clap derive) → subcommand | TUI; #[tokio::main]
      tui.rs               # terminal init/restore + run() entry point (thin)
      tui/
        app.rs             # App state, Pane/Mode/AppMsg, key routing, tokio::select! loop, render;
                           #   send/cancel (AbortHandle + generation counter), history writes, highlight cache
        events.rs          # Action enum, crokey KeyMap (+config overrides), nucleo-matcher FuzzyFinder
        highlight.rs       # off-thread syntect worker (std::thread + mpsc), viewport-only, returns Highlighted
        components/        # explorer, request (edtui), response (virtualised viewer), picker, search, palette, statusline
    tests/
      tui_snapshot.rs      # insta snapshots via TestBackend: panes, overlays, empty state, truncated status line
      cli_import.rs        # `churl import` integration tests against the real binary
docs/
  ARCHITECTURE.md
  DECISIONS.md
  MILESTONES.md
.github/workflows/ci.yml
```

## Conventions

- **Commits**: conventional commits — `feat:`, `fix:`, `chore:`, `docs:`, `refactor:`, `test:`.
- **Edition**: 2024 throughout.
- **Error handling**:
  - Libraries (`churl-core` and any future `churl-*` libs): `thiserror` typed errors.
  - Binary / integration glue (`churl` crate, `main.rs`): `color-eyre` for context-rich reporting.
- **`churl-core` discipline**: never add TUI deps (ratatui, crossterm, …) to `churl-core`. Model + persistence + HTTP live there; rendering never does.
- **Snapshots**: committed `.snap` files live in `crates/churl/tests/snapshots/`. When a snapshot changes intentionally, run `INSTA_UPDATE=always cargo test --all` then review and commit the updated `.snap` files.
- **Async**: tokio since M2 (`tokio::select!` over crossterm `EventStream`, tick, app mpsc channel). Since M3, HTTP requests run as `tokio::spawn`ed tasks whose `AbortHandle` + a monotonic generation counter live on `App` (cancellation is task-level; `churl-core::http` stays runtime-agnostic). Results land as `AppMsg::Response`. The render path stays sync and pure — snapshot tests construct `App` (without `install_runtime`, so `client`/`history`/highlight worker are `None`) and call `render` without a runtime; never make rendering depend on tokio.

## Milestone workflow

1. Read `docs/MILESTONES.md` to understand scope and current milestone.
2. Implement the milestone's deliverables.
3. Update `docs/MILESTONES.md`, `docs/ARCHITECTURE.md`, `docs/DECISIONS.md`, and this file **before** the milestone commit.
4. Verify: fmt + clippy + test all green, `cargo run -p churl -- --version` works.
5. Commit: `chore(m<N>): <summary>`.
