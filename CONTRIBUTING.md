# Contributing to churl

## TL;DR

1. Branch off `master`, open a PR.
2. Give the PR a **conventional commit title** — CI rejects anything else.
3. Get it green and merged (squash merge; the PR title becomes the commit).
4. That's it. Releases happen automatically from your PR title.

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

### Dev builds

Need a binary from an unreleased branch? Comment `/build` on the PR
(collaborators only) or run the **Dev build** workflow from the Actions tab —
binaries appear as workflow artifacts with 14-day retention.

## Release machinery map

| Piece | Role |
|---|---|
| `.github/workflows/pr-title.yml` | Enforces conventional PR titles |
| `.github/workflows/pr-label.yml` | Auto-labels PRs from the title type |
| `release-plz.toml` | Version/changelog/tag policy (single release train) |
| `.github/workflows/release-plz.yml` | Release PR + crates.io publish + tag push |
| `.github/workflows/release.yml` | Binaries + GitHub release on `v*` tags |
| `.github/workflows/dev-build.yml` | Tester binaries from any ref |
| `install.sh` | End-user installer (attached to every release) |

## Local checks

CI runs exactly these — green locally means green in CI:

```sh
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
```
