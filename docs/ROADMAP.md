# churl — Roadmap

churl is a terminal-native API client: explore, build, debug, test, and load-test
HTTP APIs without leaving the keyboard. This is the public roadmap — the path to a
stable **1.0** and beyond.

## Who churl is for

- **Terminal-first developers** building and debugging APIs from the keyboard.
- **QA** testing staging and production APIs — both individual cases and load.
- **CI pipelines** running automated API tests headlessly.
- **AI agents** driving API development, debugging, and testing.
- **Anyone** who wants a fast, scriptable, hackable alternative to heavyweight API clients.

## Legend

✅ shipped · 🚧 in progress · ⏳ planned · 🔭 after 1.0

---

## Shipped ✅

### 0.5.0
- Multi-line curl import — paste a browser's "Copy as cURL" straight into the new-request
  prompt (bracketed paste), with imported secrets captured into a masked session variable.
- Response viewer — a visible cursor line so keyboard scrolling always tracks where you are.

### 0.4.0
- **Cookies, proxy & insecure-TLS** — an opt-in persistent cookie jar, an HTTP(S) proxy
  (CLI / config / env, credentials never persisted), and a session insecure-TLS toggle with a
  loud warning badge. All live from an in-TUI **Options overlay**.
- **Per-endpoint insecure-TLS** — a durable per-endpoint opt-in, so a dev endpoint can skip
  cert verification while its siblings stay verified.
- **Release operations** — one-button rollback and force-release workflows, and a documented
  rollback runbook.

### 0.3.0
- **Nested collections & root-level endpoints** — the workspace is one recursive collection
  tree: collections nest to any depth, endpoints can live at the root, and variables inherit
  down the tree (child overrides parent). Existing workspaces keep working unchanged.
- **Unified creation & tree CRUD** — one gesture to create collections, endpoints, or sequences
  anywhere in the tree (a pasted curl is auto-detected and imported); reorder, move, copy, and
  duplicate across the tree; a writable in-memory session-variable group.
- **Lifecycle & distribution** — self-update, uninstall, and version pinning.
- **Secret & request safety** — a request-wide secret save-gate, and a cross-origin redirect
  policy that drops auth headers when a redirect crosses origin.

### 0.2.0
- **Collection interchange** — Postman and churl-native JSON import/export (dialect
  auto-detected), plus curl paste/copy inside the TUI.
- **Environments & variables editor** — workspace, collection, and profile vars with live
  precedence display and masked secrets.
- **Request sequences** — chain requests into end-to-end flows, extracting values from each
  response to feed the next.
- **Concurrent load testing** — throttled batches with live stats and bounded, memory-safe
  retention.
- **Response viewer** — JSON pretty-printing, fold/wrap, structural node navigation, in-viewer
  search, line-number gutter, control-char/ANSI sanitizing.
- **Navigation & keymap** — a 4-region layout, jump-to-pane, and a fully remappable,
  data-driven keymap with load-time conflict warnings.
- **Durability & cross-platform** — atomic saves, SQLite WAL, bounded growth; macOS + Windows
  CI, `cargo-deny`, native Wayland clipboard.

### 0.1.x
- The core TUI: three-pane layout, vim-style navigation, request execution, virtualized
  response rendering, and history.
- curl import/export with a strict round-trip corpus.
- First-class auth (basic / bearer / API-key), themes, keymaps, jump-mode, and `{{var}}`
  templating.
- Prebuilt binaries, a `curl | sh` installer, and an automated release pipeline.

---

## The road to 1.0

churl is already a full request workbench. The path to 1.0 closes the edges — a first-class
CLI, real testing, and the polish to launch — grouped by target release.

### 0.6 — Quick wins 🚧
- [x] Response structural navigation — jump between collapsible nodes in the response viewer.
- [x] Editable paste-curl — review and adjust a pasted curl before importing.

### 0.7 — CLI & headless (agent-first) ✅
- [x] `churl send` / `churl run <endpoint>` — headless execution for scripting, CI, and agents,
      with structured JSON output, clean exit codes, and no interactive prompts. Frozen contract:
      [`docs/CLI.md`](CLI.md).
- [x] `churl init`, refined `--help`, shell completions, and man pages.
- [x] Import redesign — `churl import` creates an endpoint in the current workspace by default.

### 0.8 — Debugging & testing ⏳
- [ ] Debug inspector — resolved request, redirect / variable / auth traces,
      copy-as-resolved-curl, logs, and a session traffic feed (opt-in, off by default).
- [x] Assertions & tests — status / header / body-JSONPath pass-fail checks (`--assert`,
      persisted `[[assertions]]`) with machine-readable results (`data.assertions`, exit 1 on
      failure) for CI and agents. Frozen contract: [`docs/CLI.md`](CLI.md), "Assertions". Regex
      `matches` deferred — see `docs/DECISIONS.md`'s Backlog section.
- [x] Headless sequence runs — `run-seq <name>` runs a saved sequence end-to-end with no TUI,
      chaining extracted values step-to-step in one process and gating each step on its
      endpoint's persisted `[[assertions]]`. Under `--json` it streams NDJSON (one object per
      step + a terminal summary line); exit 1 on any failed assertion or broken extraction
      chain, a transport/resolution band (3/4/5) winning over it. Frozen contract:
      [`docs/CLI.md`](CLI.md), "Sequence runs (`run-seq`)".

### 0.9 — Coverage & settings ⏳
- [ ] Centralized settings panel.
- [ ] Multipart / file upload.
- [ ] Response output — save response bodies to file, HTML/XML pretty-printing.

### 0.10 — Reports & interop ⏳
- [ ] Saved reports — persist, export, and reload completed sequence and load runs.
- [ ] Storage retention & maintenance — reports lifecycle and a `churl db` maintenance command.
- [ ] Interop — OpenAPI, Postman, and `.http` / REST-Client import/export.
- [ ] Timing depth — a request waterfall (DNS / connect / TLS / TTFB / download).

### 1.0 — Polish & launch ⏳
- [ ] Final quality and security sweep, refreshed docs, and a rewritten README.
- [ ] Homebrew and AUR distribution.

---

## After 1.0 🔭

- **Plugins** — community extensibility for auth schemes, body types, viewers, and
  import/export formats. churl ships the well-known formats; the plugin system is how the
  community adds the rest. OAuth and GraphQL land here first, then graduate to core.
- **Streaming** — Server-Sent Events, then WebSocket.
- **Agent tool server** — an MCP server exposing churl's send / run / test as callable tools,
  so any agent can drive churl directly.
