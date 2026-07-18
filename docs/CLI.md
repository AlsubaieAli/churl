# CLI & headless (M8.2)

churl's CLI treats an AI agent / CI pipeline as a first-class consumer, on equal footing with a human at the terminal. This document is the frozen machine-output contract: a compatibility surface, versioned like an on-disk format. Treat every shape here as load-bearing — a breaking change requires bumping `schema_version`.

## Commands

- `churl run <endpoint>` — executes a saved endpoint from the cwd workspace. `<endpoint>` is a `collection/sub/endpoint name` display path (root-level: just the endpoint's name) — the same addressing the TUI explorer shows, so what you see there is what you type here. Quote the argument when the endpoint name has spaces.
- `churl send [-X METHOD] [-H 'Name: Value']... [-d BODY] <URL>` — an ad-hoc one-shot request from inline flags. No saved endpoint, no workspace required. Accepts curl-mnemonic flags (`-X`/`-H`/`-d`/`--url`) and churl-native aliases (`--method`/`--header`/`--body`) — they're the same field, just two names, so either flag style works.
- `churl import <curl-command>` — parses a curl command into an endpoint. Default: writes it into the cwd workspace (`churl init` first if there isn't one yet). `--stdout` prints the endpoint TOML instead (the pre-M8.2 default). `--out FILE` writes an arbitrary file, bypassing the workspace entirely.

Global flags that apply to `run`/`send` (and the rest of the CLI): `--var k=v` (repeatable), `--profile NAME`, `--proxy URL`, `-k`/`--insecure`, and `--json` (switches to the machine envelope described below).

## The `--json` envelope

Every `--json` invocation of `run`/`send`/`import` prints **exactly one** JSON object to stdout:

```json
{ "schema_version": 1, "ok": true, "command": "send", "data": { ... }, "error": null }
```

- `schema_version` — integer, currently `1`. Bumps only on a breaking change (a field removed, renamed, or its meaning changed). A new optional field, or a new `error.kind` value, is additive and never bumps it.
- `ok` — mirrors the process exit code exactly: `true` ⟺ exit `0`.
- `command` — the emitting subcommand's name (`"run"`, `"send"`, `"import"`).
- `data` — the success payload; `null` on any hard failure.
- `error` — `null` on success; on failure, `{ "kind": "<stable-slug>", "message": "<human text>", "detail": {...}? }`. `kind` is a **closed enum** — branch on it, never on `message` (free text, may reword at any time).

**stdout carries only the envelope** in `--json` mode. Every log, warning, and human nicety goes to stderr. `--json` mode never colors output, never shows a spinner/prompt, and never enables bracketed paste.

## `run`/`send` payload (`data` on success)

```json
{
  "request":  { "method": "GET", "url": "...", "headers": [{"name": "...", "value": "..."}], "body_present": false },
  "response": { "status": 200, "headers": [...], "body": "...", "body_encoding": "utf8",
                "truncated": false, "timing_ms": { "total": 123 } },
  "assertions": null
}
```

`data.trace` (M8.3) is omitted from this shape by default — see "Debug trace (`-v`)" below; it appears only when `-v/--verbose` is given under `--json`.

- `request.url` is the request's resolved `url` field (after `{{var}}` substitution), **with secrets masked** (see "Secret masking" below) — it does **not** include enabled query params or a query-placement auth effect appended by `churl_core::http::execute` (those are wire-only effects the current schema doesn't echo; a future schema version may add a dedicated `params` field).
- `request.headers` lists only *enabled* headers (a disabled header is never sent, so it's never echoed), **with secrets masked**.
- `response.headers` are echoed **verbatim, unmasked** — including `Set-Cookie`. This is intentional: response data is the whole point of showing the response (matches `curl -i`). Only the *request* echo is redacted (it can carry a resolved credential the caller supplied); the response is what the server chose to send back.
- `response.body_encoding` is `"utf8"` when the response body is valid UTF-8, else `"base64"` (`body` is then base64-encoded). Never lossy — deterministic decoding for an agent.
- `response.truncated` mirrors the body-size cap flag.
- `assertions` is `null` when no assertions were given (unchanged since M8.2 — the reserved shape shipped early to avoid a schema-version bump); otherwise it holds the populated `AssertionReport` object described in "Assertions" below.
- Multi-request runs (sequences/load) are out of scope for M8.2 and will stream NDJSON (one JSON object per line) rather than reuse this single-object envelope — that's a distinct, future contract; a `run`/`send` invocation is always exactly one request, one object.

## Exit codes (frozen forever)

| Code | Meaning |
|---|---|
| 0 | success |
| 1 | assertion failure — a `run`/`send` whose request succeeded but whose assertions did not (see "Assertions" below) |
| 2 | usage error — owned entirely by clap's own parser (missing/conflicting/unknown flags to `churl` itself). This module never constructs a JSON envelope for a band-2 failure, even under `--json` — "clap default, don't remap." |
| 3 | workspace/resolution error |
| 4 | request/transport error |
| 5 | input/import error |

Every non-zero exit accompanies `ok: false` and a populated `error.kind`, **except** band 2 (clap's own usage errors print clap's own text to stderr and are envelope-free by design) **and band 1** (an assertion failure is the sole exception: the request succeeded, so the envelope stays `ok: true` with `data` populated — branch on `data.assertions.passed`, never on `ok`, to detect it).

## `error.kind` → exit code

| `kind` | Band | Meaning |
|---|---|---|
| `no-workspace` | 3 | No `churl.toml` at the cwd (`run`'s only mode; `import`'s default write mode; `send`'s workspace-read I/O failure) |
| `endpoint-not-found` | 3 | A `run` endpoint path didn't resolve in the open workspace |
| `unresolved-var` | 3 | The resolved request still carries a `{{var}}` placeholder no scope (nor the process env) resolved — refused rather than shipped literally |
| `unknown-profile` | 3 | `--profile NAME` named a profile the workspace manifest doesn't define |
| `config-error` | 3 | The global config couldn't be loaded/parsed (unreadable, malformed TOML, or an invalid knob value such as `redirect`), or the current working directory couldn't be determined — a pre-flight resolution failure that occurs before the request is shaped |
| `invalid-url` | 4 | The request URL couldn't be parsed (message + `detail.url` are secret-masked) |
| `timeout` | 4 | The request timed out |
| `transport-error` | 4 | Any other transport failure (DNS, connect, TLS, protocol) — message + `detail.url` are secret-masked |
| `not-a-curl-command` | 5 | `import`'s input didn't parse as a curl command — covers a tokenize failure, a missing/duplicate URL, an unknown flag, an unsupported construct, an invalid `-X` method, **and** the non-interactive stdin guard (no curl given and stdin isn't piped) |
| `import-write-failed` | 5 | The curl command parsed, but writing the endpoint failed (e.g. a newly-authored literal secret was refused, or a disk error) |
| `invalid-assertion` | 5 | A `--assert` flag did not parse: an unknown operator, a value-requiring operator (everything but `exists`/`absent`) with no value, or an empty target (M8.4) |

Implementation: `crates/churl/src/output.rs` (`ErrorKind::exit_code`).

## Assertions

`run`/`send` accept a repeatable `--assert <EXPR>` flag that checks a value in the response and, on any failure, exits **1** (see "Exit codes" above) while still printing the normal success envelope.

### Syntax

```
<target> <op> <value>      # e.g. status == 200
<target> exists|absent     # no value
```

- **`target`** is an extraction expression — the same grammar `sequence.rs`/`docs/ARCHITECTURE.md` documents for sequence-step extraction rules: `status` (the numeric HTTP status), `header:<Name>` (case-insensitive), or a JSON path (`$.a.b[0]`, leading `$.` optional). It is always a single whitespace-free token.
- **`value`** is everything after the operator token, including embedded spaces (e.g. `$.data.msg contains hello world` compares against `"hello world"`). `exists`/`absent` take no value.

### Operators

| Op | Aliases | Meaning |
|---|---|---|
| `==` | `eq` | Exact string equality |
| `!=` | `ne` | Exact string inequality |
| `contains` | | Substring match |
| `exists` | | The target extracts successfully (a `null` leaf or a missing header/key/index does **not** count as existing) |
| `absent` | | The target's extraction fails with a not-found reason (missing header/key/index, or a `null` leaf) — a malformed expression or non-JSON body does **not** count as absent |
| `<`, `>`, `<=`, `>=` | | Numeric comparison; both sides are parsed as `f64` — a non-numeric side fails the assertion with a clear reason rather than falling back to string comparison |

A target that fails to extract (e.g. a missing header) fails every value-comparing operator (`==`/`!=`/`contains`/`<`/`>`/`<=`/`>=`) with the extractor's own error surfaced as the reason — never a fabricated empty-string/zero comparison.

### Effective assertion set

- `churl run <endpoint>`: the endpoint's persisted `[[assertions]]` (below), **then** its `--assert` flags, in that order (append, never replace).
- `churl send`: no persisted endpoint, so `--assert` flags are the whole set.

An empty set (no persisted assertions, no `--assert` flags) is unchanged M8.2 behaviour: exit 0, `data.assertions` stays `null`.

### Populated `data.assertions` shape

```json
{
  "passed": false,
  "total": 2,
  "failed": 1,
  "results": [
    { "target": "status", "op": "==", "expected": "200", "actual": "200", "pass": true },
    { "target": "$.data.id", "op": "exists", "pass": false, "error": "extract \"$.data.id\": no such key \"id\"" }
  ]
}
```

`op` is always the canonical operator string (`==`, `contains`, `exists`, …), never the Rust variant name. `expected`/`actual` are omitted (not `null`) when not applicable (`expected` for `exists`/`absent`; either when extraction failed). `error` is present only on a failed result.

### Persisted endpoint assertions

An endpoint TOML file may carry a top-level `[[assertions]]` array-of-tables (sibling of `[request]`):

```toml
[[assertions]]
target = "status"
op = "=="
value = "200"

[[assertions]]
target = "$.data.id"
op = "exists"
```

`op` round-trips as its canonical string; `value` is omitted on disk for `exists`/`absent`.

### Invalid `--assert`

A flag that fails to parse (unknown operator, a value-requiring operator with no value, or an empty target) is `error.kind: "invalid-assertion"`, exit **5** — a usage/input mistake caught before any request runs, distinct from an assertion that ran and failed (exit 1).

### Human (non-`--json`) mode

After the usual response echo, a checklist prints to **stderr** — one line per assertion (`✓`/`✗` + `target op [value]`, with the failure reason appended after `✗`) — followed by a summary line, then the process exits 1 if any failed:

```
✓ status == 200
✗ $.data.id exists — extract "$.data.id": no such key "id"
1 passed, 1 failed
```

`--json` mode never prints this — the checklist is a human-only rendering of the same `data.assertions` object.

## Debug trace (`-v`)

`run`/`send` accept `-v`/`--verbose`. In human mode this has always printed a request/response trace to stderr. As of M8.3, under `--json` it additionally adds a `data.trace` object to the envelope — the same underlying capture (`churl_core::debug::DebugTrace`) that backs the TUI's Inspector overlay. Omitting `-v` omits `data.trace` entirely (never `"trace": null`); this is a purely additive field — see "Schema versioning" below.

```json
{
  "resolved_display": {
    "method": "GET",
    "url": "https://api.example.com/x?api_key=••••••",
    "headers": [{ "name": "Authorization", "value": "••••••", "enabled": true }],
    "body_present": false
  },
  "var_steps": [
    { "name": "host", "scope": "cli", "value_masked": "api.example.com" },
    { "name": "api_key", "scope": "profile", "value_masked": "••••••" }
  ],
  "redirect_hops": [
    { "from": "https://a.example/x", "to": "https://b.example/x", "status": 302,
      "cross_origin": true, "stripped_headers": ["authorization"] }
  ],
  "decisions": { "auth_injected": "Authorization", "cookie_used": false, "proxy": null }
}
```

- **`resolved_display`** — the final (post-`{{var}}`) request churl actually sent, masked exactly like `data.request` (see "Secret masking" below). `headers` is omitted when empty.
- **`var_steps`** — every `{{var}}` placeholder that resolved, in substitution order: the name, the scope that supplied it (`"cli"`, `"profile"`, `"collection"`; absent means the process-environment fallback), and the resolved value masked the same dual-anchor way header values are (a secret-*named* var is masked even at low entropy; any secret-*shaped* value is masked under any name). Omitted when empty.
- **`redirect_hops`** — one entry per redirect hop followed, in hop order: masked `from`/`to`, the hop's status, `method_change` (omitted when the method was preserved), whether the hop crossed origin, and which header names were stripped before it (only populated on a cross-origin hop under the default `strip` redirect policy). Omitted when empty.
- **`decisions`** — `auth_injected` names the auth-bearing header/query churl added (omitted when none, e.g. a user header of the same name already won); `cookie_used`/`proxy` (masked) reflect the `ClientConfig` `run`/`send` built its client from for this invocation.
- An `error` field exists on the underlying `DebugTrace` type (for a failed send) but never appears in `data.trace`: `run_execution` only attaches a trace to `ExecData` on a *successful* exchange (a transport/resolution failure returns before `ExecData` exists at all, mirroring every other field of the envelope). The Inspector overlay (TUI) surfaces the failure case separately.

**Schema versioning.** `data.trace` is a new optional field on an existing object — additive per the frozen bump rule (`crates/churl/src/output.rs` module docs): "`SCHEMA_VERSION` bumps ONLY on a breaking change (a field removed, renamed, or its meaning changed); adding a new optional field never bumps it." `schema_version` stays `1`.

## Secret masking (request echo)

The echoed `request` is redacted before it reaches stdout/stderr, so a resolved `{{secret}}` (or a caller-supplied credential) never round-trips back out even though the real outgoing request carried it. Two surfaces, both masked:

- **`request.headers`** — a header value is replaced with `••••••` when its *name* is `authorization`/`cookie` or looks secret-named (`churl_core::config::looks_like_secret_name`), or when its *value* looks secret-shaped (`churl_core::secrets::looks_like_secret_value`). Same dual-anchor policy as the cross-origin redirect `strip` policy (DECISIONS.md).
- **`request.url`** — masked by `churl_core::secrets::mask_url` (the redaction twin of the `scan_url` scanner): the `user:PASSWORD@` userinfo password and each secret query value (a secret-*named* key's literal value, or a secret-*shaped* value under any key). A `{{placeholder}}` span and non-secret pairs are untouched.

Both the **success** surface (`data.request` in the envelope, and the `-v` stderr trace) and the **failure** surface apply this masking: the `invalid-url` `error.message` + `error.detail.url`, **and** the `transport-error` message + `detail.url` (a connection/DNS/TLS failure is the common case, and the underlying client's error text embeds the request URL — query string included).

**Known limitation (best-effort, not a guarantee).** This is heuristic redaction shared with R3's codebase-wide secret detection: an opaque header/query *name* whose *value* is low-entropy enough to trip neither the name anchor nor the value-shape anchor still echoes. `mask_url` mirrors `scan_url`'s spans exactly — **userinfo password + query values** — so a secret placed in a **path segment** (`…/tokens/ghp_…`) or a **`#fragment`** is *not* masked (paths and fragments aren't a known credential position; masking high-entropy path segments would also clobber ordinary resource IDs / UUIDs, degrading the echo agents rely on). Closing the name/value gap fully needs value-*provenance* tracking (which resolved value came from a `{{secret}}`) — a codebase-wide change beyond M8.2, not a masking bug here. `response.headers` (incl. `Set-Cookie`) are echoed unmasked **intentionally** — response data is the point of showing the response (`curl -i`).

## Non-interactive guarantees

`--json`/headless subcommands never block on a TTY. `import`'s stdin-read path (no curl argument given) refuses immediately with `not-a-curl-command` when stdin is an interactive terminal, rather than hanging on a Ctrl-D that will never come. Same inputs → the same envelope key-set for a given `schema_version` — deterministic for a script.

## Deliberate scope cuts (M8.2)

Recorded here so a later milestone can pick them back up deliberately rather than rediscover them:

- **`request.url` excludes appended query params/auth-query effects.** The frozen payload has no separate `params` field; adding one is additive and can land in a later schema version without breaking `1`.
- **`-X`/`--method` is validated against churl's closed `Method` enum** (`GET`/`POST`/`PUT`/`PATCH`/`DELETE`/`HEAD`/`OPTIONS`), unlike curl's own free-form `-X` (any string). An unsupported value is a clap usage error (exit 2).
- **No cookie jar for `run`/`send`.** A one-shot headless process builds exactly one client and exits; the persistent per-workspace jar (`churl cookies list|clear`) is a separate, already-headless surface.
- **Multi-request sequence/load runs, assertions, and the debug inspector** are out of scope for M8.2 entirely (see ROADMAP.md 0.7 vs. 0.8).
