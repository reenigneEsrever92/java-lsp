# Directory log

## 2026-10-03

- **Architecture doc restructured**: Rewrote the `## Components and data flow`
  section of `architecture.md` — the verbose per-module prose is now a
  hub-and-spoke diagram (the hub emphasised) plus a module table giving each
  component's description and responsibilities; the original detail is
  preserved verbatim in a new `## Implementation notes` section.
- **Engine bus renamed to hub; document store moved onto the hub**: `bus.rs` →
  `hub.rs` (`HubClient`, `Hub`, the `hub()` accessor and the `java_lsp::hub` log
  target), and `DocumentStore` moved out of the shell into the document module —
  a hub client that applies edits and answers text requests. `architecture.md`
  and the changelog were updated to match.

## 2026-09-08

- **Syntax features verified**: All syntax-feature acceptance criteria
  verified by automated tests (document symbols, folding ranges, semantic
  tokens, parse-error diagnostics that clear on fix, per-document trees).
  In-editor Zed verification (outline panel, folds, squiggles on `Hello.java`)
  is left to the user — restart the language server to pick up the new
  release binary.
- **LSP shell skeleton smoke test**: Drove the new `java-lsp` binary over
  stdio with raw JSON-RPC (`scripts/stdio-smoke.py`) — initialize advertised
  incremental sync, didOpen/didChange produced publishDiagnostics, hover
  answered null from the stub engine, and shutdown/exit exited cleanly.
- **Creation**: Initialized the project as an OKF bundle — discovered the
  project kind, users, use cases, and technologies; recorded the requirement
  analysis in `requirements.md`; and seeded the backlog with the initial
  change requests.
