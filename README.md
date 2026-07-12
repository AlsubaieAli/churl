# churl

**A fast terminal API client — craft, send, and inspect HTTP requests from your terminal.**

[![CI](https://github.com/AlsubaieAli/churl/actions/workflows/ci.yml/badge.svg)](https://github.com/AlsubaieAli/churl/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/churl.svg)](https://crates.io/crates/churl)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

---

## Install

### curl | sh (macOS and Linux)

```sh
curl -fsSL https://github.com/AlsubaieAli/churl/releases/latest/download/install.sh | sh
```

The script detects your OS and architecture, downloads the matching binary,
verifies its SHA-256 checksum, and installs to `~/.local/bin`. To pass options
through the pipe, use `sh -s --`:

```sh
curl -fsSL https://github.com/AlsubaieAli/churl/releases/latest/download/install.sh \
  | sh -s -- --to ~/bin --force
```

| Option | Effect |
|---|---|
| `--to DIR` | Install to `DIR` instead of `~/.local/bin` |
| `--tag TAG` | Install a specific release, including betas (e.g. `--tag v0.2.0-beta.1`) |
| `--force` | Overwrite an existing `churl` binary |
| `--dry-run` | Print the resolved URL and target, download nothing |

### PowerShell (Windows)

```powershell
irm https://github.com/AlsubaieAli/churl/releases/latest/download/install.ps1 | iex
```

The script downloads the Windows binary, verifies its SHA-256 checksum, and
installs to `%LOCALAPPDATA%\Programs\churl`. To pass options, download and run
it directly:

```powershell
irm https://github.com/AlsubaieAli/churl/releases/latest/download/install.ps1 -OutFile install.ps1
pwsh install.ps1 -To C:\Tools\churl -Force
```

| Option | Effect |
|---|---|
| `-To DIR` | Install to `DIR` instead of `%LOCALAPPDATA%\Programs\churl` |
| `-Tag TAG` | Install a specific release, including betas (e.g. `-Tag v0.2.0-beta.1`) |
| `-Force` | Overwrite an existing `churl` binary |
| `-DryRun` | Print the resolved URL and target, download nothing |

### Prebuilt binaries

Download the archive for your platform from the
[latest release](https://github.com/AlsubaieAli/churl/releases/latest):

| Platform | Archive |
|---|---|
| macOS (Apple Silicon) | `churl-aarch64-apple-darwin.tar.gz` |
| macOS (Intel) | `churl-x86_64-apple-darwin.tar.gz` |
| Linux x86\_64 (musl, static) | `churl-x86_64-unknown-linux-musl.tar.gz` |
| Linux aarch64 (musl, static) | `churl-aarch64-unknown-linux-musl.tar.gz` |
| Windows x86\_64 | `churl-x86_64-pc-windows-msvc.zip` |

Each archive includes a `churl` binary and a `.sha256` checksum file.

### cargo install

Builds from source (needs a Rust toolchain):

```sh
cargo install churl
```

### From git (bleeding edge)

Builds the tip of `master` — unreleased features, no stability promises:

```sh
cargo install --git https://github.com/AlsubaieAli/churl churl
```

### Updating

Updating uses the same command as installing — check what you're running
with `churl --version`:

| Installed via | Update with |
|---|---|
| curl \| sh | Re-run the installer with `--force` (always resolves the latest stable) |
| Prebuilt binary | Download the newer archive and replace the binary |
| `cargo install churl` | `cargo install churl` — cargo rebuilds when a newer version is published |
| git | `cargo install --git … churl --force` (`--force` reinstalls even if the version number hasn't changed) |

There's no self-update command yet (`churl update` is a roadmap candidate),
and no package-manager distribution (Homebrew/AUR) so far.

### Beta releases

Pre-releases (tags like `v0.2.0-beta.1`) ship the same binaries as stable
releases but are never picked up by `releases/latest` or a plain
`cargo install churl` — you opt in explicitly:

```sh
# installer
curl -fsSL https://github.com/AlsubaieAli/churl/releases/latest/download/install.sh \
  | sh -s -- --tag v0.2.0-beta.1 --force

# or cargo
cargo install churl --version 0.2.0-beta.1
```

---

## Quickstart

The fastest way to get started is `churl tutorial`, which scaffolds a demo workspace
with example endpoints, a profile, and template variables:

```sh
churl tutorial          # scaffolds ./churl-tutorial/
cd churl-tutorial
churl                   # opens the TUI — select an endpoint and press Ctrl-S to send
```

Or scaffold to a custom directory:

```sh
churl tutorial --dir ~/my-api
```

The demo workspace targets [httpbingo.org](https://httpbingo.org) — a public HTTP
echo service — so your first request works immediately without any sign-up.

---

<!-- TODO: capture a screenshot to docs/screenshot.png, then restore:
## Screenshot
![screenshot](docs/screenshot.png)
-->

## Feature matrix

| Feature | Notes |
|---|---|
| Collections + endpoints | TOML files, one endpoint per file, comment-preserving edits |
| Profiles + template vars | `{{base_url}}`, `{{token}}` placeholders; CLI `--var`, profiles, collection and workspace vars |
| Auth | Basic, Bearer, API key (header or query); secrets via `{{var}}` placeholders |
| curl import / export | `churl import "curl …"` converts a curl command; round-trip stable |
| Themes | Dark (default) and light built-ins; per-slot `[theme_colors]` overrides |
| Keymaps | Fully remappable via `[keys]` + per-pane `[keys.response]` etc.; `churl keymaps` prints the effective map |
| Vim navigation | `j`/`k`, `g`/`G`, `Ctrl-d`/`Ctrl-u`, jump-mode (`f`), Space-leader |
| Response viewer | Virtualised scroll; syntax highlighting (JSON, YAML, HTML, …) |
| Response search | `/` incremental smart-case search, `n`/`N` navigation, auto-unfold |
| Response wrap | `W` soft-wraps at pane width |
| Response headers | `h` toggles between body and full headers |
| JSON folding | `o`/`O` fold/unfold regions at the cursor |
| Copy to clipboard | `y` copies the view, `Y` copies the cursor line (OSC 52 — works over SSH/tmux) |
| Request history | Every request written to SQLite; browse via the history picker |

---

## Configuration

`~/.config/churl/config.toml` — global settings:

```toml
theme = "dark"           # "dark" (default) or "light"
timeout_secs = 30        # request timeout in seconds
max_body_bytes = 10485760  # response body cap (10 MB default)

[theme_colors]
# Override individual theme slots with named ANSI colours or #rrggbb hex.
title = "cyan"
accent = "#ffcc00"

[keys]
# Remap any action globally.
"ctrl-p" = "open-palette"

[keys.response]
# Override keys in the response pane overlay.
"ctrl-f" = "open-body-search"
```

Print the effective keymap (defaults + your overrides) at any time:

```sh
churl keymaps
```

---

## What's next

Roughly in order — see [docs/ROADMAP.md](docs/ROADMAP.md) for the authoritative roadmap:

- **Collection interchange** — JSON collection import/export, plus curl paste/copy directly inside the TUI
- **Environments & variables editor** — manage profiles, collection, and workspace vars without leaving the app
- **Quick-jump pickers** — fuzzy-jump straight to any request or workspace
- **Request sequences** — chain requests into end-to-end flows with shared state
- **Concurrent requests** — fire throttled batches for smoke and light load testing
- **Cookies & proxy** — cookie jar persistence and HTTP(S) proxy support
- **Plugin system** — extend auth schemes, body types, and viewers
- Also on the backlog: multipart bodies (`curl -F`) and nested folders in the explorer

---

## Development

```sh
git clone https://github.com/AlsubaieAli/churl
cd churl
cargo test --all       # full suite
cargo run -p churl     # run the TUI from source
```

CI runs fmt, clippy (`-D warnings`), the test suite, and a security audit on
every push and pull request. Releases are fully automated: PR titles follow
[Conventional Commits](https://www.conventionalcommits.org), and merging the
bot-maintained release PR publishes to crates.io, tags, writes the changelog,
and builds binaries for all five supported targets. The full workflow —
conventions, release train, betas, dev builds — is in
[CONTRIBUTING.md](CONTRIBUTING.md).

Need a binary from an unreleased branch? Collaborators can comment `/build`
on a pull request (or run the **Dev build** workflow from the Actions tab) to
get macOS/Linux/Windows binaries as workflow artifacts.

---

## License

Licensed under either of

- [MIT License](LICENSE-MIT)
- [Apache License, Version 2.0](LICENSE-APACHE)

at your option.
