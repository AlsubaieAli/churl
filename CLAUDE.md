# churl — Claude Code Instructions

## Build / test / run

```sh
# Build (debug)
cargo build

# Build (release)
cargo build --release

# Run all tests (workspace)
cargo test --all

# Canonical CI checks: see CONTRIBUTING.md → Local checks (fmt / clippy / test).

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
                           #   Sequence/SequenceStep/OnError — request-chain execution
      auth.rs              # apply_auth(&Auth) -> AuthWire: the single dispatch point on auth kinds
                           #   (plugin guardrail); execute/export apply effects, never match Auth
      persistence.rs       # toml_edit load/save (format-preserving, deletion-pruning merge; atomic_write: temp→fsync→rename→dir-fsync), lazy OpenWorkspace/Collection;
                           #   M7.9: workspace root IS the root collection — OpenWorkspace::root_collection()
                           #   (root endpoints + churl.toml [vars] as root meta), Collection::sub_collections()
                           #   recurses to arbitrary depth (sequences/ reserved at ROOT only);
                           #   Collection::endpoints() strict + endpoints_lenient() -> CollectionLoad (skip one bad file →
                           #   warning; both skip folder.toml AND churl.toml at every level);
                           #   CollectionMeta (folder.toml [vars] + seq: u32 skip-if-0; loaders sort (seq,name),
                           #   all-default corpus stays byte-identical → no migration); CRUD seams:
                           #   create/rename/delete_endpoint + create/rename/delete_collection (slug+seq,
                           #   baseline-aware secret gate on every save path — save_*_checked(policy) return
                           #   SecretDecision, plain save_* wrap them at Strict; SecretsRefused only on a
                           #   newly-authored name-anchored literal, reserved-name disambiguation);
                           #   M7.12 tree CRUD (relocate.rs/reorder.rs/refs.rs): move/copy/duplicate_endpoint +
                           #   _collection (atomic claim + -N, append-at-dest seq), reorder_{endpoint,collection,
                           #   sequence} (dense-renumber then swap; ReorderOutcome edge report),
                           #   retarget_sequence_steps (rewrite step endpoint on rename/move; copy never rewrites);
                           #   SEQUENCES_DIRNAME (excluded from collections()), OpenWorkspace::sequences() →
                           #   SequenceLoad (lenient), load/save/create/rename/delete/duplicate_sequence
      sequence.rs          # run engine (UI-free): extract_value (status/header:/JSON-path subset, no
                           #   jsonpath dep), prepare_step (resolver + prepended extracted scope), extract_step,
                           #   classify_step (single classify+extract seam), ordered_steps, run_sequence
                           #   (wiremock-tested); rejects ../absolute step endpoints, never panics
      load.rs              # concurrent-load runner (UI-free): run_load (N copies through execute(),
                           #   bounded by futures' buffer_unordered + absolute-target pacing), classify (single
                           #   Ok/Failed/Error seam), pure stats (nearest-rank percentiles), check_config/LoadCaps
                           #   guardrail; run_load is the wiremock-tested twin the TUI launcher mirrors
      template.rs          # {{var}} Resolver: ordered Scope list + env fallback (the single plugin seam);
                           #   substitute / substitute_request; sequences prepend a highest-precedence
                           #   `extracted` scope — resolution never forked; unresolved_placeholders
                           #   fails loud: names any {{var}} still present after substitution
                           #   (reuses parse_placeholder; no literal-brace escape) so the 3 send paths refuse;
                           #   contains_placeholder: does a value embed any {{token}} (used by the save-gate for
                           #   header/url/param values like `Bearer {{token}}`, vs config's whole-value check)
      config.rs            # global config.toml loading (incl. [keys] overrides, theme + [theme_colors],
                           #   timeout_secs, max_body_bytes, the [load] guardrail caps → Config::load_caps(),
                           #   secret_policy → Config::secret_policy() = Strict|Warn fail-loud)
                           #   + secret-name/placeholder primitives (looks_like_secret_name,
                           #   is_template_placeholder tightened to a single well-formed {{token}},
                           #   secret_violations incl. workspace [vars], collection_secret_violations,
                           #   auth_secret_violations)
      secrets.rs           # save-time secret engine: Severity{Block,Warn}, SecretFinding, SecretPolicy,
                           #   scan_endpoint/scan_workspace/scan_collection (auth + header/url/param/body +
                           #   env-var names AND secret-shaped values via looks_like_secret_value), and
                           #   decide(new, baseline, policy) → SecretDecision{refusals, warnings}
                           #   (novelty by location → grandfather pre-existing, block new name-anchored)
      history.rs           # rusqlite HistoryStore, user_version migrations (append-only); WAL + busy_timeout +
                           #   BEGIN IMMEDIATE migration lock; a SEPARATE load_batches table (LoadBatchSummary) —
                           #   load runs write one summary row there, never to history (structural non-flooding);
                           #   prune-on-insert row caps
      http.rs              # reqwest+rustls execute(client, request, &ExecuteOptions); streamed body cap →
                           #   Response.truncated; build_client(timeout); runtime-agnostic (no AbortHandle in core);
                           #   applies AuthWire effects (enabled user header with the same name wins)
      import.rs            # curl command → Endpoint (shlex + strict flag map; unknown flag = hard error)
      export.rs            # Endpoint → curl command (shlex::try_quote; round-trip contract with import)
      pin.rs               # optional `.churl-version` workspace pin (pure: discover/parse/compare,
                           #   semver-aware w/ exact-string fallback); warn-only, the bin displays it
    tests/
      persistence.rs       # comment-preservation corpus, manifest+secrets, lazy loading
      roundtrip_prop.rs    # proptest Endpoint round-trip
      curl_roundtrip.rs    # ≥20-command curl import→export→import corpus
      http.rs              # wiremock execution suite incl. body-size cap
      sequence.rs          # wiremock chain/halt/continue/precedence/traversal + sequence TOML round-trip
      fixtures/            # comment-bearing endpoint TOML fixtures
  churl/                   # binary crate + thin lib for integration tests
    src/
      lib.rs               # pub mod tui (re-export for tests)
      main.rs              # Cli (clap derive): global --var/--profile, subcommands (import, keymaps, tutorial,
                           #   update, uninstall) | TUI; #[tokio::main]
      tutorial.rs          # churl tutorial subcommand: scaffold demo workspace via real persistence seams
      update.rs            # churl update: verified self-replace from GitHub releases (self_replace crate);
                           #   pure target→asset/version-compare/checksum fns, network+swap bin-only
      uninstall.rs         # churl uninstall: binary by default, config+state behind --purge (pure removal_plan)
      tui.rs               # terminal init/restore + run(cli_vars, profile) entry point (thin);
                           #   warns once on a `.churl-version` mismatch at workspace load
      tui/
        app/               # App state + orchestration (directory module; mod.rs is the spine)
          mod.rs           # App state, Pane (incl. UrlBar)/Mode (incl. Jump/MethodMenu/Prompt/Confirm/EnvEditor/
                           #   Sequence/LoadRunner — each mode owns its state in-variant)/AppMsg (incl. SequenceStep,
                           #   LoadStarted/LoadResult); Picker enum (data-carrying, one variant per picker kind);
                           #   RequestTabs, loaded_snapshot (derived dirty), inline LineEditor edit; key routing via
                           #   lookup_ctx; tokio::select! loop + event loop; send-time {{var}} resolution,
                           #   profile switching, Theme; send/cancel, history, highlight cache
          handlers/        # per-concern key/action handlers: buffers, crud, editing, env_editor, help,
                           #   load_runner, response, send, sequence, vars, workspace (+ mod.rs)
          render.rs        # the render layer (render + leader popup / collapsed-stub / prompt / confirm helpers)
          state.rs         # pure state types
          pure.rs          # self-free helpers (query split/decode, auth-tab + export-path helpers)
          tests.rs         # App-level unit + state-machine tests
        events/            # Action enum + key mapping (directory module)
          mod.rs           # Action enum (+ urlbar/tab/row/CRUD/save actions), crokey KeyMap with per-pane
                           #   overlays (PaneCtx, lookup_ctx, [keys.<pane>] config) + load-time conflict validator
          action.rs        # Action definitions + the config-name action table
          fuzzy.rs         # nucleo-matcher FuzzyFinder
          tests.rs
        theme.rs           # Theme (named style slots) parsed from core strings; dark/light built-ins + [theme_colors]
        highlight.rs       # off-thread syntect worker (std::thread + mpsc), viewport-only, theme-aware; spawn degrades to None
        clipboard.rs       # native (arboard) clipboard primary + OSC-52-with-passthrough (tmux/screen) fallback, 1 MB cap
        components/        # explorer, urlbar (focusable, inline edit + dirty dot), line_editor (shared 1-line editor),
                           #   response/ (virtualised viewer pipeline: pretty/sort/headers/wrap/fold/search/copy/line-numbers;
                           #     sanitize_for_display strips ANSI/controls + expands tabs (TAB_WIDTH 4) + h_scroll horizontal window;
                           #     ResponseState::Dropped memory-bound placeholder: status/timing/size, no body; geometry cache),
                           #   request (tab bar + Params/Headers/Auth rows + edtui Body), request_tabs (tab/row state),
                           #   tab_strip (filled-chip buffer strip), fold (JSON fold-region scanner),
                           #   env_editor/ (environments & vars editor: split-view over workspace/collection/profile scopes
                           #     + a read-only masked Session group — profile CRUD, dirty/discard guard, secret mask+refuse,
                           #     live precedence display; core stays UI-free; `p` ephemeral peek reveals a masked value + `y` copies it),
                           #   picker, method_menu, prompt (CRUD prompt + confirm overlays),
                           #   search, palette (curated command allowlist), jump (pane-only jump labels), statusline,
                           #   vim_ext (Normal-mode W/B/^/f/F/t/T motions edtui lacks, for both edtui editors),
                           #   sequence_runner (run view: live per-step status/timing + reused response viewer,
                           #     masked extracted values; UI-only, App drives via core primitives),
                           #   sequence_editor (steps + extraction-rule CRUD + reorder + on_error + Session-persist toggle, save_sequence),
                           #   load_runner (editable config header + live O(viewport) results; memory bound —
                           #     retains full Done views only for last K=16 + selected row, older→Dropped
                           #     list + reused response viewer + stats line; UI-only, App owns the single
                           #     buffer_unordered launcher + load_abort + generation guard + [load] guardrail)
    tests/
      tui_snapshot.rs      # insta snapshots via TestBackend: panes, overlays, empty state, truncated status line
      cli_import.rs        # `churl import` integration tests against the real binary
      cli_m6.rs            # CLI integration tests: churl keymaps, --var, --profile
      cli_tutorial.rs      # `churl tutorial` integration tests: scaffold, refuse-overwrite, workspace load
docs/
  ARCHITECTURE.md
  DECISIONS.md
  ROADMAP.md
README.md                  # install, quickstart, feature matrix, screenshot placeholder, license
install.sh                 # curl|sh installer: OS+arch detection, sha256 verify, ~/.local/bin, --dry-run
.github/workflows/ci.yml
.github/workflows/release.yml  # tag-triggered; taiki-e/upload-rust-binary-action; 5 targets + sha256
```

## Conventions

Working conventions (code structure, comments, process, commits) are canonical in `CONTRIBUTING.md` → **Working conventions** — they bind you. Below are the churl-specific operational notes.

- **`churl-core` discipline**: never add TUI deps (ratatui, crossterm, …) to `churl-core`. Model + persistence + HTTP live there; rendering never does. (Full rule + error-handling split in CONTRIBUTING.)
- **Snapshots**: committed `.snap` files live in `crates/churl/tests/snapshots/`. When a snapshot changes intentionally, run `INSTA_UPDATE=always cargo test --all` then review and commit the updated `.snap` files.
- **Async**: tokio (`tokio::select!` over crossterm `EventStream`, tick, app mpsc channel). HTTP requests run as `tokio::spawn`ed tasks whose `AbortHandle` + a monotonic generation counter live on `App` (cancellation is task-level; `churl-core::http` stays runtime-agnostic). Results land as `AppMsg::Response`. The render path stays sync and pure — snapshot tests construct `App` (without `install_runtime`, so `client`/`history`/highlight worker are `None`) and call `render` without a runtime; never make rendering depend on tokio.

## Milestone workflow

1. Read `docs/ROADMAP.md` for scope and the current milestone. The roadmap is repo-authoritative; the maintainers' vault hub mirrors it milestone-grain.
2. Implement the milestone's deliverables.
3. Update `docs/ROADMAP.md`, `docs/ARCHITECTURE.md`, `docs/DECISIONS.md`, and this file **before** the milestone commit.
4. Verify: fmt + clippy + test all green, `cargo run -p churl -- --version` works.
5. Commit with the appropriate Conventional Commit type (see CONTRIBUTING.md).
