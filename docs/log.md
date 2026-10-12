# Directory log

## 2026-10-12

- **Architecture doc reconciled with the navigation extraction**: Navigation is
  no longer the index's own — `src/index/navigate.rs` moved to a top-level
  `src/navigate.rs`, a hub module (`navigate`) that serves definition,
  declaration, and references by reading source files itself and the index only
  over the hub, and the index answers a coarse, batched read API
  (`IndexRequest`/`IndexBatchRequest`) while the shared Java-reading helpers live
  in `src/parser.rs`. Updated `architecture.md` to match: the Layout
  (`index/navigate.rs` gone; `navigate.rs`, `parser.rs`, and `index/read.rs` added;
  the shell routes navigation to the navigate module), the module table (shell
  and index rows, plus a new navigate row), the data-flow diagram (a navigate
  node reading the index), the `WorkspaceIndex` and navigation implementation
  notes, and the "index is its own subsystem" decision.

## 2026-10-11

- **Architecture doc reconciled with the hub rework**: The hub's request/response
  mechanics now live in `hub.rs` — a request is routed to the module that owns it
  (`owner_of`), the reply rides the hub back to its one requester
  (`HubClient::{request, respond}` around an awaitable `Reply`), each client sorts
  what it receives through a per-client dispatcher (`subscribe`/`serve`), and the
  hub logs every message it forwards (`info`). Updated `architecture.md` to
  match: the Layout (`hub.rs` and `messages.rs`), the **Hub and spoke**
  paragraph, the Hub and Engine-wiring table rows, the shell-on-the-hub bullet
  (register/subscribe/serve, `respond`, and the empty reply for a request with no
  owner), and the "one hub" and "index is its own subsystem" decisions
  (registration is `Hub::register`, logging is `info`, replies are logged without
  timing). The rework note now records both landed passes and marks the fuller
  vocabulary the reintroduced modules will bring (`DriverMessage`,
  `AnalysisRequest`, `IndexHandle`) as intended design.

## 2026-10-10

- **Architecture doc reconciled with the source-scan fold**: The workspace
  source scanner is no longer a separate `scan.rs` driver — the index owns its
  own scan (`index.rs` walks the workspace root and parses each `.java` file into
  the index). Updated `architecture.md`: the Layout (dropped `scan.rs`; recast
  `index.rs` as owning the source scan; split the declared-type model in
  `index/types.rs` from the binding engine in `types.rs`), the components diagram
  and module table, the implementation notes, and the "index is its own
  subsystem"/warm-up-driver decisions to match, plus a note that the hub rework
  leaves several modules commented out of `lib.rs`.

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
