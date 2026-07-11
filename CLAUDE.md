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

## Code navigation (Serena)

This repo is [Serena](https://github.com/oraios/serena)-enabled (`.serena/project.yml`,
Rust via rust-analyzer). When the Serena MCP tools are available, prefer them for
navigating the codebase — `find_symbol`, `find_referencing_symbols`,
`get_symbols_overview` — over bulk file reads and grep. Setup is in
CONTRIBUTING.md → "Semantic code navigation (Serena)".

## Workspace layout

```
Cargo.toml                 # workspace root, shared deps / package metadata
crates/
  churl-core/              # pure library — zero TUI deps, ever
    src/
      lib.rs               # pub const VERSION + module exports
      model.rs             # Method, Endpoint, Request, Response, Header, Param, Profile, Workspace,
                           #   Auth/ApiKeyPlacement (internally-tagged [request.auth]);
                           #   Sequence/SequenceStep/OnError (M7.4 request sequences)
      auth.rs              # apply_auth(&Auth) -> AuthWire: the single dispatch point on auth kinds
                           #   (M9 plugin guardrail); execute/export apply effects, never match Auth
      persistence.rs       # toml_edit load/save (format-preserving, deletion-pruning merge; atomic_write: temp→fsync→rename→dir-fsync, R0), lazy OpenWorkspace/Collection;
                           #   Collection::endpoints() strict + endpoints_lenient() -> CollectionLoad (skip one bad file →
                           #   warning; both skip folder.toml AND churl.toml — M7.3 crash fix);
                           #   CollectionMeta (folder.toml [vars]) load/save (M6); CRUD seams (M6.6):
                           #   create/rename/delete_endpoint + create/rename/delete_collection (slug+seq,
                           #   secrets refusal on every save path);
                           #   SEQUENCES_DIRNAME (excluded from collections()), OpenWorkspace::sequences() →
                           #   SequenceLoad (lenient), load/save/create/rename/delete_sequence (M7.4)
      sequence.rs          # M7.4 run engine (UI-free): extract_value (status/header:/JSON-path subset, no
                           #   jsonpath dep), prepare_step (resolver + prepended extracted scope), extract_step,
                           #   classify_step (single classify+extract seam), ordered_steps, run_sequence
                           #   (wiremock-tested); rejects ../absolute step endpoints, never panics
      load.rs              # M7.5 concurrent-load runner (UI-free): run_load (N copies through execute(),
                           #   bounded by futures' buffer_unordered + absolute-target pacing), classify (single
                           #   Ok/Failed/Error seam), pure stats (nearest-rank percentiles), check_config/LoadCaps
                           #   guardrail; run_load is the wiremock-tested twin the TUI launcher mirrors
      template.rs          # {{var}} Resolver: ordered Scope list + env fallback (the single M9 seam);
                           #   substitute / substitute_request (M6); sequences prepend a highest-precedence
                           #   `extracted` scope (M7.4) — resolution never forked; unresolved_placeholders
                           #   (note #4b) fails loud: names any {{var}} still present after substitution
                           #   (reuses parse_placeholder; no literal-brace escape) so the 3 send paths refuse
      config.rs            # global config.toml loading (incl. [keys] overrides, theme + [theme_colors],
                           #   timeout_secs, max_body_bytes, the M7.5 [load] guardrail caps → Config::load_caps())
                           #   + secrets heuristics (looks_like_secret_name,
                           #   is_template_placeholder, secret_violations incl. workspace [vars],
                           #   collection_secret_violations, auth_secret_violations)
      history.rs           # rusqlite HistoryStore, user_version migrations (append-only); migration 3 (M7.5)
                           #   adds a SEPARATE load_batches table (LoadBatchSummary) — load runs write one
                           #   summary row there, never to history (structural non-flooding); migration 4 ALTERs
                           #   in the mean_ms column
      http.rs              # reqwest+rustls execute(client, request, &ExecuteOptions); streamed body cap →
                           #   Response.truncated; build_client(timeout); runtime-agnostic (no AbortHandle in core);
                           #   applies AuthWire effects (enabled user header with the same name wins)
      import.rs            # curl command → Endpoint (shlex + strict flag map; unknown flag = hard error)
      export.rs            # Endpoint → curl command (shlex::try_quote; round-trip contract with import)
    tests/
      persistence.rs       # comment-preservation corpus, manifest+secrets, lazy loading
      roundtrip_prop.rs    # proptest Endpoint round-trip
      curl_roundtrip.rs    # ≥20-command curl import→export→import corpus
      http.rs              # wiremock execution suite incl. body-size cap
      sequence.rs          # M7.4 wiremock chain/halt/continue/precedence/traversal + sequence TOML round-trip
      fixtures/            # comment-bearing endpoint TOML fixtures
  churl/                   # binary crate + thin lib for integration tests
    src/
      lib.rs               # pub mod tui (re-export for tests)
      main.rs              # Cli (clap derive): global --var/--profile, subcommands (import, keymaps, tutorial) | TUI; #[tokio::main]
      tutorial.rs          # churl tutorial subcommand: scaffold demo workspace via real persistence seams
      tui.rs               # terminal init/restore + run(cli_vars, profile) entry point (thin)
      tui/
        app.rs             # App state, Pane (incl. UrlBar)/Mode (incl. Jump/MethodMenu/Prompt/Confirm/EnvEditor/
                           #   SequenceRunner/SequenceEditor — M7.4 / LoadRunner — M7.5)/AppMsg (incl. SequenceStep,
                           #   LoadStarted/LoadResult),
                           #   RequestTabs, loaded_snapshot (derived dirty), inline LineEditor edit; key routing via
                           #   lookup_ctx; in-app CRUD via core seams; tokio::select! loop, render; send-time {{var}}
                           #   resolution, profile switching, Theme; send/cancel, history, highlight cache
        events.rs          # Action enum (+Jump/SwitchProfile + M6.6 urlbar/tab/row/CRUD/save actions), crokey KeyMap
                           #   with per-pane overlays (PaneCtx, lookup_ctx, [keys.<pane>] config), nucleo-matcher FuzzyFinder
        theme.rs           # Theme (named style slots) parsed from core strings; dark/light built-ins + [theme_colors]
        highlight.rs       # off-thread syntect worker (std::thread + mpsc), viewport-only, theme-aware, returns Highlighted
        clipboard.rs       # OSC 52 clipboard writes (no native dep; works over SSH/tmux), 1 MB cap
        components/        # explorer, urlbar (focusable, inline edit + dirty dot), line_editor (shared 1-line editor),
                           #   response also carries ResponseState::Dropped (R0 memory-bound placeholder: status/timing/size, no body),
                           #   request (tab bar + Params/Headers/Auth rows + edtui Body), request_tabs (tab/row state),
                           #   response (virtualised viewer + M7 pipeline: cursor/headers/wrap/fold/search/copy;
                           #     M7.7 sanitize_for_display (strip ANSI/controls, tab-width 4) + h_scroll horizontal window),
                           #   fold (JSON fold-region scanner), env_editor (M7.3 environments & vars editor:
                           #     EnvEditorState split-view over workspace/collection/profile scopes — profile CRUD,
                           #     dirty/discard guard, secret mask+refuse, live precedence display; core stays UI-free;
                           #     `p` ephemeral peek reveals a masked row's resolved value in place (note #3) + `y` copies it —
                           #     re-masks on any move/mode-change/6s-timeout, one row at a time, view-state-only never-persisted),
                           #   picker, method_menu, prompt (CRUD prompt + confirm overlays),
                           #   search, palette (curated command allowlist), jump, statusline,
                           #   vim_ext (Normal-mode W/B/^/f/F/t/T motions edtui lacks, for both edtui editors),
                           #   sequence_runner (M7.4 run view: live per-step status/timing + reused response viewer,
                           #     masked extracted values; UI-only, App drives via core primitives),
                           #   sequence_editor (M7.4 §4: steps + extraction-rule CRUD + reorder + on_error, save_sequence),
                           #   load_runner (M7.5 Mode::LoadRunner: editable config header + live O(viewport) results;
                           #     R0 memory bound — retains full Done views only for last K=16 + selected row, older→Dropped
                           #     list + reused response viewer + stats line; UI-only, App owns the single
                           #     buffer_unordered launcher + load_abort + generation guard + [load] guardrail)
    tests/
      tui_snapshot.rs      # insta snapshots via TestBackend: panes, overlays, empty state, truncated status line
      cli_import.rs        # `churl import` integration tests against the real binary
      cli_m6.rs            # M6 CLI integration tests (keymaps, --var, --profile)
      cli_tutorial.rs      # `churl tutorial` integration tests: scaffold, refuse-overwrite, workspace load
docs/
  ARCHITECTURE.md
  DECISIONS.md
  MILESTONES.md
README.md                  # install, quickstart, feature matrix, screenshot placeholder, license
install.sh                 # curl|sh installer: OS+arch detection, sha256 verify, ~/.local/bin, --dry-run
.github/workflows/ci.yml
.github/workflows/release.yml  # tag-triggered; taiki-e/upload-rust-binary-action; 5 targets + sha256
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
