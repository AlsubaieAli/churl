# Contributing to churl

## TL;DR

1. Branch off `master`, open a PR.
2. Give the PR a **conventional commit title** — CI rejects anything else.
3. Get it green and merged (squash merge; the PR title becomes the commit).
4. That's it. Releases happen automatically from your PR title.

## Branch naming

Branches follow `<type>/<milestone>-<slug>`:

```
<type>/<milestone>-<short-kebab-slug>
```

- **`<type>`** — the same Conventional-Commit type as the PR title
  (`feat`, `fix`, `perf`, `refactor`, `docs`, `test`, `build`, `ci`, `chore`,
  `revert`), so branch, PR title, and squash commit stay consistent.
- **`<milestone>`** — the milestone id when the work belongs to one
  (`d1`, `r0`, `m7`, `m7.10`, …). **Drop it** for milestone-less work.
- **`<slug>`** — a short kebab-case description.

Examples:

```
feat/d1-demo-stabilize        # milestone work
fix/m7-clipboard-tmux
refactor/r0-atomic-saves
ci/parallel-check-jobs         # no milestone → type/slug
docs/contributing-conventions
```

## PR title convention

PR titles follow [Conventional Commits](https://www.conventionalcommits.org):

```
<type>(<optional scope>): <summary in imperative mood>
```

| Type | Use for | Version impact | Auto-label |
|---|---|---|---|
| `feat` | New user-facing capability | minor bump | `enhancement` |
| `fix` | Bug fix | patch bump | `bug` |
| `perf` | Performance improvement | patch bump | `performance` |
| `refactor` | Code change with no behaviour change | none | `refactor` |
| `docs` | Documentation only | none | `documentation` |
| `test` | Tests only | none | `tests` |
| `build` / `ci` | Build system / CI changes | none | `ci` |
| `chore` | Everything else (deps, tooling) | none | `chore` |
| `revert` | Reverting a previous change | matches reverted change | `revert` |

A `!` after the type (`feat!:`) or a `BREAKING CHANGE:` footer marks a breaking
change → major bump (while churl is 0.x, cargo semver treats this as a minor
bump, which is still breaking for 0.x users).

Labels are applied automatically from the title when a PR is opened, retitled,
or reopened (`pr-label.yml`); `!` additionally applies `breaking`. Retitling
adds the new type's label but doesn't remove the old one — drop stale labels
by hand.

Examples:

```
feat(viewer): fold XML regions like JSON
fix(import): keep query encoding on curl round-trip
feat!: drop TOML v1 workspace layout
```

Scopes are free-form but prefer existing ones (`viewer`, `import`, `auth`,
`tui`, `core`, `installer`, `release`).

## Working conventions

How we keep pace, focus, and style consistent — for any contributor, human or agent. (Agents: these bind you; `CLAUDE.md` points here.)

### Code structure
- **File-size discipline.** Source files have a soft ceiling of ~800 lines. Past it, ask *"one responsibility, or several?"* — several clusters with no cross-dependencies → split into sibling modules; one cohesive state-machine → keep it (splitting state from its render adds indirection, not clarity). Splits are pure-move and behaviour-preserving: snapshots stay byte-identical.
- **Illegal states unrepresentable.** Model state so invalid combinations can't be constructed — per-mode data lives inside its `Mode`/`Picker` enum variant, not in parallel `Option` fields guarded at every use site. If you're writing a defensive `match` for a state the types already rule out, fix the type instead.
- **`churl-core` purity.** No TUI dependencies (ratatui, crossterm, …) in `churl-core`, ever. Model, persistence, and HTTP live there; rendering never does.
- **Errors.** `thiserror` typed errors in libraries (`churl-core`); `color-eyre` for context-rich reporting in the binary (`churl`, `main.rs`).

### Comments
- Comments explain **present-tense behaviour and rationale** — the *why*, invariants, safety/security predicates, protocol/wire-format notes, subtle concurrency reasoning.
- **No milestone/PR archaeology in code** (`(M7.4)`, `PR #58`, `// ---- M6.7 … ----`), no "previously… now…" narratives, no restating what the code plainly says. Provenance belongs in git history and the decision log, not inline.

### Process — how work moves
- **Non-trivial changes get an independent review** — a reviewer other than the author looks for the most likely bug and for tests that only appear to test something.
- **A green build is not proof.** Exercise UI changes against the real binary, not just the test suite.
- **No silent deferrals.** Every "later" gets an explicit roadmap or backlog entry when you defer it.
- **Docs move with the code.** Update `ROADMAP.md`, `ARCHITECTURE.md`, `DECISIONS.md`, and `CLAUDE.md` in the same commit as the change; keep `CLAUDE.md`'s layout map factually current.

### Commit hygiene
- Conventional Commits (`feat:`/`fix:`/`refactor:`/`docs:`/`chore:`/…) — CI enforces the PR title (see the table above).
- **Keep the subject clean:** the subject line is user-facing release-note copy (the changelog is generated from it), so no milestone/PR/note archaeology in it — put context in the commit body if it's worth keeping.

## How releases work

Nobody pushes tags or runs `cargo publish` by hand:

1. Merging PRs to `master` makes [release-plz](https://release-plz.dev)
   open (or update) a **release PR** — it computes the next version from the
   merged conventional commits and writes the matching `CHANGELOG.md` section.
2. **Merging the release PR is the release.** release-plz publishes
   `churl-core` then `churl` to crates.io and pushes the `v<version>` tag.
3. The tag triggers `release.yml`, which builds binaries for the five
   supported targets and creates the GitHub release, using the new
   `CHANGELOG.md` section as release notes.

Batching is free: leave the release PR open while more PRs merge and it
keeps updating itself; merge it when the release feels ready.

### Beta releases

Betas are cut manually, outside the release train:

```sh
git tag v0.2.0-beta.1 && git push origin v0.2.0-beta.1
```

`release.yml` marks `-suffix` tags as prereleases, so `releases/latest`, the
installer, and plain `cargo install churl` keep serving stable. Publish a beta
to crates.io only if testers need `cargo install --version`.

A tag is an immutable pointer to a commit, so a beta persists on its own —
moving `master` forward never disturbs it, and testers keep installing it with
`install.sh --tag v0.2.0-beta.1`. When a beta proves good and you want the
**stable** release to be byte-for-byte what testers vetted, cut stable from the
**same commit** the beta was tagged on (rather than a newer `master`).

### Forcing a release (installer / infra changes)

release-plz versions the **crate** — it hashes what `cargo publish` ships. A
change to `install.sh` or the workflows doesn't touch the crate, so release-plz
correctly won't cut a release for it. But the installer reaches users through
**GitHub release assets**, which *do* need a new release to update. That gap is
filled by the **Force release** workflow (`force-release.yml`):

- **Automatically**: a push to master that changes only `install.sh` (no crate
  source) force-cuts a **patch** release — installer fixes ship themselves.
- **Manually**: Actions tab → **Force release** → Run workflow, choosing
  `patch`/`minor`/`major` and a changelog line. Use this for any on-demand
  release (e.g. re-releasing after a release-infra fix).

It bumps the workspace version + changelog and commits to master; from there the
normal pipeline takes over (release-plz publishes + tags → `release.yml` builds
binaries, uploads the installer, smoke-tests it, and finalizes). Everything runs
on CI — releases never depend on uploading assets from a developer machine.

If a PR changes `install.sh` **and** crate code together, release-plz handles it
as a normal crate release and the new installer rides along — the force path
stays out of the way.

### Rolling back a release

There is no single "undo" — rollback is per-surface, because each ships
differently. In order of preference:

1. **Roll forward (preferred).** Releases are cheap and CI-only, so the cleanest
   fix for a bad release is to revert the offending commit and cut a new patch —
   via a normal PR, or the **Force release** workflow for an on-demand bump. The
   bad version stays in history; the new one supersedes it everywhere.

2. **Repoint the installer / `Latest` (fast stop-gap).** The `curl | sh`
   installer follows `releases/latest`. Run the **Rollback release** workflow
   (Actions tab → **Rollback release** → Run workflow) with the last-good tag
   (and optionally the bad tag to demote). It re-marks the good release `Latest`
   and demotes the bad one to a prerelease. It is **metadata-only** (`gh release
   edit`, no asset upload), so it runs even when `uploads.github.com` is
   unreachable (Twingate). It verifies the good release actually has binaries
   first, so it can't point `Latest` at an assetless release.

3. **crates.io (`cargo install churl`).** crates.io is **immutable** — a
   published version can never be overwritten or deleted, only *yanked*:

   ```sh
   cargo yank --version 0.3.1        # hide from new dependency resolution
   cargo yank --version 0.3.1 --undo # reverse it
   ```

   Yank leaves existing lockfiles working; it only stops new picks. To actually
   replace the code, publish a higher patch (roll forward). The Rollback workflow
   does **not** touch crates.io — yank is a separate, deliberate call.

### Dev builds

Need a binary from an unreleased branch? Comment `/build` on the PR
(collaborators only) or run the **Dev build** workflow from the Actions tab —
binaries appear as workflow artifacts with 14-day retention.

## Workflow map

| Piece | Role |
|---|---|
| `.github/workflows/ci.yml` | Format · Lint · Test · Security audit (parallel PR gate) |
| `.github/workflows/pr-title.yml` | Enforces conventional PR titles |
| `.github/workflows/pr-label.yml` | Auto-labels PRs from the title type |
| `release-plz.toml` | Version/changelog/tag policy (single release train) |
| `.github/workflows/release-plz.yml` | Release PR + crates.io publish + tag push |
| `.github/workflows/release.yml` | Binaries + GitHub release on `v*` tags; installer smoke-test gate |
| `.github/workflows/force-release.yml` | Cuts a release for installer/infra changes release-plz skips |
| `.github/workflows/rollback.yml` | Repoints `Latest`/installer to a known-good release (metadata-only rollback) |
| `.github/workflows/dev-build.yml` | Tester binaries from any ref |
| `install.sh` | End-user installer (attached to every release) |

## Continuous integration

`ci.yml` runs four **independent, parallel** jobs on every push and PR to
`master` — each is its own status check, so any can be marked *required* in
branch protection:

| Check | Command |
|---|---|
| **Format** | `cargo fmt --all --check` |
| **Lint** | `cargo clippy --all-targets --all-features -- -D warnings` |
| **Test** | `cargo test --all` (with `INSTA_UPDATE=no` — CI never writes snapshots) |
| **Security audit** | `cargo audit` |

They run in parallel with no `needs:` between them (none depends on another);
`needs:` is reserved for genuine pipelines like the release flow.

### Local checks

Green locally means green in CI — run the first three before pushing:

```sh
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
```

## Semantic code navigation (Serena)

This repo is configured for [Serena](https://github.com/oraios/serena) — an MCP
toolkit that gives coding agents LSP-backed **semantic** navigation
(`find_symbol`, `find_referencing_symbols`, `get_symbols_overview`, symbolic
edits) instead of grep-and-read-whole-files. rust-analyzer is the language
server (auto-detected). The versioned config is `.serena/project.yml`; the symbol
cache and local overrides (`.serena/cache/`, `.serena/project.local.yml`) are
gitignored by `.serena/.gitignore`.

It's optional — nothing in the build/test/release path depends on it. To enable
it for Claude Code (per-user), from the repo root:

```sh
claude mcp add serena -- \
  uvx --from git+https://github.com/oraios/serena \
  serena start-mcp-server --context claude-code --project-from-cwd
```

Optionally pre-build the symbol index for faster first responses (writes to the
gitignored cache):

```sh
uvx --from git+https://github.com/oraios/serena serena project index
```

Requires [`uv`](https://docs.astral.sh/uv/) and a Rust toolchain (rust-analyzer
comes via `rustup component add rust-analyzer` if not already present).
