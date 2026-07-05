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
      config.rs            # global config.toml loading + secrets heuristics
      history.rs           # rusqlite HistoryStore, user_version migrations
    tests/
      persistence.rs       # comment-preservation corpus, manifest+secrets, lazy loading
      roundtrip_prop.rs    # proptest Endpoint round-trip
      fixtures/            # comment-bearing endpoint TOML fixtures
  churl/                   # binary crate + thin lib for integration tests
    src/
      lib.rs               # pub mod tui (re-export for tests)
      main.rs              # Cli (clap derive) → subcommand | TUI
      tui.rs               # ratatui + crossterm placeholder TUI
    tests/
      tui_snapshot.rs      # insta snapshot via TestBackend
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
- **Async**: not in M0. Tokio arrives in M2/M3 — don't add it earlier.

## Milestone workflow

1. Read `docs/MILESTONES.md` to understand scope and current milestone.
2. Implement the milestone's deliverables.
3. Update `docs/MILESTONES.md`, `docs/ARCHITECTURE.md`, `docs/DECISIONS.md`, and this file **before** the milestone commit.
4. Verify: fmt + clippy + test all green, `cargo run -p churl -- --version` works.
5. Commit: `chore(m<N>): <summary>`.
