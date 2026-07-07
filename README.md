# churl

**A fast terminal API client — craft, send, and inspect HTTP requests from your terminal.**

![CI](https://github.com/ali-subaie/churl/actions/workflows/ci.yml/badge.svg)

<!-- TODO: add a crates.io badge once published -->

---

## Install

### curl | sh (macOS and Linux)

```sh
curl -fsSL https://github.com/ali-subaie/churl/releases/latest/download/install.sh | sh
```

The script detects your OS and architecture, downloads the matching binary,
verifies its SHA-256 checksum, and installs to `~/.local/bin` (add a `--to DIR`
override or `--force` to overwrite an existing binary). Run with `--dry-run`
to preview the resolved URL without downloading.

### Prebuilt binaries

Download the archive for your platform from the
[latest release](https://github.com/ali-subaie/churl/releases/latest):

| Platform | Archive |
|---|---|
| macOS (Apple Silicon) | `churl-aarch64-apple-darwin.tar.gz` |
| macOS (Intel) | `churl-x86_64-apple-darwin.tar.gz` |
| Linux x86\_64 (musl, static) | `churl-x86_64-unknown-linux-musl.tar.gz` |
| Linux aarch64 (musl, static) | `churl-aarch64-unknown-linux-musl.tar.gz` |
| Windows x86\_64 | `churl-x86_64-pc-windows-msvc.zip` |

Each archive includes a `churl` binary and a `.sha256` checksum file.

### cargo install

```sh
cargo install churl
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

## Screenshot

<!-- TODO: capture a screenshot and save to docs/screenshot.png -->
![screenshot](docs/screenshot.png)

---

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

## License

Licensed under either of

- [MIT License](LICENSE-MIT)
- [Apache License, Version 2.0](LICENSE-APACHE)

at your option.
