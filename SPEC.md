# bookmark-check Technical Specification

Status: Draft · Target: `bookmark-check` v0.1.0 · Language: Rust (latest stable, edition 2021)

## 1. Purpose

`bookmark-check` is a CLI tool that reads Markdown files, extracts URLs, checks
them concurrently over HTTP, and reports broken or redirected links as
human-readable text or machine-readable JSON. It deliberately does **not**
implement a full Markdown parser — link extraction is a minimal, pragmatic
scan of the file (see §4).

## 2. Scope

In scope:

- URL extraction from Markdown files (minimal approach, no full parser)
- Malformed-link reporting
- Concurrent HTTP checks
- Status-code and redirect reporting
- Per-request timeouts
- JSON output
- GitHub Actions CI (already present on `main`)

Out of scope:

- Full Markdown parsing (no CommonMark compliance, no inline-code context
  awareness guarantees beyond simple exclusion rules)
- Authentication-protected URL checking (no cookies, no credentials)
- HTML page content crawling (we check the target URL's own response only)
- Link checking of relative/local file references

## 3. CLI Interface

```
bookmark-check [OPTIONS] <FILE>...
```

Positional arguments:

- `<FILE>...` — one or more Markdown file paths. `-` reads a single Markdown
  document from stdin.

Options:

| Flag | Meaning |
|------|---------|
| `--json` | Emit machine-readable JSON (§8) instead of text |
| `--timeout <SECONDS>` | Per-request timeout, default `10`, clamped to `1..=120` |
| `--concurrency <N>` | Max parallel HTTP requests, default `16`, clamped to `1..=128` |
| `--user-agent <STRING>` | Override the default `bookmark-check/0.1` UA |
| `--follow-redirects / --no-follow-redirects` | Default: follow (report final destination + redirect chain, §7) |
| `--fail-fast` | Stop checking after the first broken link (skip remaining checks, still print what was found) |
| `--quiet` | Suppress the per-link summary; print only the exit-code-visible failures |

Exit codes:

- `0` — all links healthy (or no links found)
- `1` — one or more broken/redirect-problem links
- `2` — usage error (no file, unknown flag, invalid flag value, file not found/unreadable)

Text output (default): one line per checked URL, e.g.

```
OK       200 https://example.com
REDIRECT 301 -> 200 https://old.example -> https://new.example
BROKEN   404 https://example.com/missing
ERROR    connection timed out https://10.0.0.1/
MALFORMED bad url "htp:/x" (docs.md:12)
```

## 4. URL Extraction

Extraction is a minimal, line-based scan — **not** a Markdown parser.

Extraction rules:

1. **Inline links**: `[text](URL)` — the URL is everything inside the first
   parenthesised group up to the closing paren, after stripping an optional
   whitespace-separated title `("title")`.
2. **Autolinks**: `<https://example.com>` — the URL is the inner text.
3. **Reference-style definitions**: `[label]: URL` at line start — the URL is
   the remainder of the line up to the first whitespace (or optional
   `<...>`-wrapped form).
4. **Bare URLs**: `http://` and `https://` substrings terminated by
   whitespace, `<>`, `)`, or a trailing punctuation character from
   `. , ; : ! ?` (a trailing punctuation char is stripped only when it does
   not make the URL invalid).

Exclusions (do not extract or report):

- URLs inside fenced code blocks (``` or ~~~) and inline code spans
  (`` ` `` … `` ` ``).
- Non-HTTP(S) schemes: `mailto:`, `ftp:`, `file:`, `#fragment`-only anchors,
  and relative paths. These are silently skipped (except `mailto:` etc. are
  not malformed — just out of scope).

Each extracted link carries its source location `(file, line, column)` for
reporting. Duplicate URLs across files are checked once, but each occurrence
is reported.

### Malformed links

A link is *malformed* (and reported as such, never silently dropped) when it
matches extraction shape (§4 rules) but fails URL parsing:

- empty URL inside `[]()`
- a URL that does not parse per the `url` crate (e.g. `htp:/x`, `http://`)
- `http://` / `https://` prefix with no host
- unmatched brackets or unbalanced title quotes

Malformed links are collected and reported but never sent over HTTP. They do
not affect the process exit code as a failure unless `--fail-fast` is set.

## 5. Concurrency Model

- A bounded work-stealing pool (or `std::thread::scoped`-style semaphore) of
  at most `--concurrency` simultaneous HTTP requests. Default 16.
- Extraction is sequential per file; all files are scanned first, producing a
  flat list of `Check { url, location }` items.
- Checks fan out over the pool; results are collected into a
  `Vec<CheckResult>` and sorted by `(file, line, column)` before printing so
  output is deterministic.
- Cancellation: on `--fail-fast`, remaining queued items are dropped; in-flight
  requests are allowed to finish (bounded by timeout).

No async runtime is used; blocking HTTP with a small thread pool keeps
dependencies and complexity minimal for a CLI of this size.

## 6. HTTP Behavior

Client: `reqwest` blocking client, per-request configuration:

- **Redirects**: follow up to 10 hops by default (matches browser behaviour).
  When following, record the redirect chain (status + location per hop).
  With `--no-follow-redirects`, a 3xx response is reported as
  `REDIRECT (unfollowed)` with the `Location` header, and is treated as
  neither broken nor healthy for exit-code purposes unless the redirect
  target is also broken.
- **Timeout**: connect timeout of 5s and total-request timeout of
  `--timeout` seconds (default 10). Timed-out requests are reported as
  `ERROR timeout`.
- **Method**: `HEAD` first. If the server responds with `405 Method Not
  Allowed`, `400 Bad Request`, or `501 Not Implemented`, retry once with
  `GET` and discard the body (`Range: bytes=0-0` is not used; the body is
  simply not read beyond what is needed to receive the status).
- **User-Agent**: `bookmark-check/<version>` unless overridden.
- **Status interpretation**:
  - `2xx` → OK
  - `3xx` → REDIRECT (see above)
  - `4xx` → BROKEN (404/410 always; other 4xx also broken — some servers
    return 403 for bots; we report it plainly rather than heuristically)
  - `5xx` → BROKEN
  - Anything else or transport error (DNS failure, TLS error, connection
    refused, timeout) → ERROR
- Only `http` and `https` schemes are checked (see §4 exclusions).

## 7. JSON Schema

`--json` emits a single object to stdout:

```json
{
  "version": 1,
  "files": ["docs.md", "guide.md"],
  "summary": { "total": 42, "ok": 35, "broken": 4, "redirects": 2, "errors": 1, "malformed": 0 },
  "checks": [
    {
      "url": "https://example.com",
      "location": { "file": "docs.md", "line": 12, "column": 5 },
      "status": "ok",
      "http_status": 200,
      "redirect_chain": [],
      "error": null
    }
  ]
}
```

`status` is one of `"ok" | "broken" | "redirect" | "error" | "malformed"`.
`http_status` is `null` when no HTTP response was received. `redirect_chain`
is a list of `{ "status": 301, "url": "https://old.example" }` hops in order,
empty when no redirect occurred. `error` is a human-readable string for
transport failures, else `null`. The `version` field allows evolution of this
schema without breaking parsers.

## 8. Error Handling

- **Usage errors** (missing file, bad flag value): message on stderr, exit 2.
- **Unreadable files**: message on stderr, exit 2 (before any HTTP is done —
  all files are opened first).
- **Per-link errors** (timeout, DNS, TLS, refused): recorded on the
  `CheckResult`, printed, and counted as `errors` in the summary. They are
  not process failures (exit 0 with only errors is allowed, since
  unreachable servers are common); `--fail-fast` treats the first broken
  link as terminal.
- **Malformed links**: reported, never sent over HTTP, do not fail the
  process.
- **Panic-free**: HTTP and parse errors are all mapped to typed results; no
  `unwrap`/`expect` on fallible paths (tests enforce `#![deny(clippy::unwrap_used)]`
  style invariants via clippy on CI).

## 9. Testing Strategy

- **Unit tests** (`#[cfg(test)]` in-module, matching the scaffold's style):
  extraction rules (each §4 rule + exclusions + malformed cases), status
  classification, JSON serialisation of a fixed result set.
- **Integration tests** (`tests/cli.rs`): run the binary as a subprocess
  against fixture Markdown files in `tests/fixtures/`, asserting text and
  JSON output and exit codes. Use a tiny in-test TCP listener (std
  `TcpListener`) serving canned responses for status/redirect/timeout cases —
  no external network in tests.
- **CI** (already configured): `cargo fmt --check`, `cargo clippy
  --all-targets --all-features -- -D warnings`, `cargo build --locked`,
  `cargo test --locked`.

## 10. Dependencies

Registry-only, minimal set:

| Crate | Purpose |
|-------|---------|
| `reqwest` (blocking feature, `default-tls`→`native-tls` or `rustls-tls`) | HTTP client |
| `url` | URL parsing/validation (already a transitive dep of reqwest; used directly) |
| `serde` + `serde_json` | JSON output |
| `clap` (derive) | CLI argument parsing |
| `anyhow` (or bare `Result<(), String>` matching scaffold) | Error context at boundaries — keep minimal; may be omitted initially |

Dev-dependencies: none beyond the std-lib test harness initially (assertions
via `assert_eq!`/`assert!`).

TLS backend choice: prefer `rustls` (pure-Rust, avoids system OpenSSL
variability) — `reqwest = { version = "0.12", default-features = false, features = ["blocking", "rustls-tls", "json"] }`.

All dependency additions go into `Cargo.toml` and are committed with an
updated `Cargo.lock` (`--locked` builds in CI enforce this).

## 11. Project Layout

```
src/
  main.rs      — arg parsing, wiring, exit codes
  extract.rs   — URL extraction + malformed-link detection
  check.rs     — HTTP checking, status classification
  report.rs    — text and JSON output
tests/
  cli.rs       — end-to-end subprocess tests
  fixtures/    — sample Markdown files
```