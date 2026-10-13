# Directory log

## 2026-10-13

- **Architecture doc reconciled with workspace symbol search**: `workspace/symbol`
  is served again by a new `workspace` hub module (`src/workspace.rs`) — the shell
  advertises `workspaceSymbolProvider` and routes the search to it, and it maps
  the index's existing `EntriesByPrefix` read to `SymbolInformation` (the entry's
  `selection_range` as the location, its innermost enclosing type as
  `container_name`, the mapped LSP kind), dropping imports, constructors,
  Lombok-synthetic members, and `Dependency` class-file entries. Updated
  `architecture.md`: the rework note (a sixth landed pass), the Layout
  (`workspace.rs` added), the data-flow diagram (a workspace node reading the
  index), the module table (the shell row's capabilities and a new workspace row),
  and a workspace implementation note (with the `workspace_symbols` prose
  reworded). Also moved the constructor predicate to `SymbolEntry::is_constructor`
  (`index/util.rs`), shared by `completion` and `workspace`.

- **Architecture doc reconciled with the compact hub message log**: the hub's
  per-message `info` line now renders a compact `summary()` of each `Envelope`
  (kind, sender, uri, counts, and text payloads as byte lengths) instead of the
  full `Debug`, so buffer-text responses, `DocumentOpened`, and full-text
  `DocumentChanged` no longer dump the file body into the log. Updated
  `architecture.md`: the Layout (`hub.rs`), the module table (the hub row), and
  the hub decision bullet.

- **Architecture doc reconciled with var-local receiver inference**: the shared
  receiver step (`src/util.rs`) now infers a `var` local's type from its
  initializer — `local_type` hands the named declarator's initializer node up to
  `identifier_type`, which classifies it through the existing depth-guarded
  receiver machinery (`new T()`, method-call returns, field accesses, chains)
  instead of reading the written `var` token as a type — so member completion and
  signature help after a `var`-declared receiver resolve, and an initializer that
  resolves nothing still yields nothing. Updated `architecture.md`: the Layout's
  `util.rs` row, the navigation limitation prose, and the completion
  implementation note.

- **Completion note names the receiver shapes it does not resolve**: for symmetry
  with the `navigate` module doc, the completion implementation note in
  `architecture.md` now lists the shapes the shared receiver step cannot type — a
  nested-type or package qualifier, `super`, a generic type argument, and a lambda
  or anonymous-class scope — where it previously said only "a receiver that cannot
  be typed".

- **Architecture doc reconciled with the completed completion work**: the deprecated
  engine-core note no longer carries the live completion module's vocabulary — the
  signature-help prose moved into the completion implementation note (now the
  single home for completion and signature help), and the `MembersOf` message doc
  and `navigate` module doc now state that the read keeps same-named overloads
  separate and that a call- or field-access receiver resolves recursively, a chain
  resolving left to right while every link's receiver can be typed. No shipped
  behaviour changed. Updated `architecture.md`: the engine-core and completion
  implementation notes.

- **Architecture doc reconciled with field-access receivers**: the shared receiver
  step (`src/util.rs`) now resolves a `field_access` receiver to the accessed
  field's declared type — symmetric to the `method_invocation` arm, behind the
  same depth guard — so completion after a field access (`a.field.`) and
  navigation of a member reached through one resolve, and a chain through a field
  (`a.field.c()`) resolves left to right. Updated `architecture.md`: the
  navigation limitation prose (a receiver may be a call or a field access; a chain
  resolves while every link's receiver can be typed) and the completion
  implementation note.

- **Architecture doc reconciled with signature help**: `textDocument/signatureHelp`
  is now served by the `completion` module — the call at the cursor resolves its
  callee's receiver through the shared step (`src/util.rs`) and reads the receiver
  type's same-named methods over the hub's overload-preserving `MembersOf`
  (statics only for a `Type.` qualifier), rendering each overload's signature and
  the cursor's active argument; a `new T(…)` answers the created type's
  constructors over a new `ConstructorsOf` read, and the shell advertises
  `signatureHelpProvider` with `(` and `,` retriggers. Updated `architecture.md`:
  the Layout (`completion.rs`), the module table (the completion and shell rows),
  and the completion implementation note. Updated `requirements.md`: the v0.3
  traceability note.

- **Architecture doc reconciled with call-site overload completion**: the
  workspace-index completion path now lists a method's overloads — resolved from
  the entry's declaring type over the hub's overload-preserving `MembersOf` read
  — each labelled with its full signature and inserting `name(`, and carries the
  entry's auto-import edit on each; a method whose declaring type does not resolve
  keeps the single name-only item. Updated `architecture.md`: the completion
  implementation note (both paths list overloads; the stale name-only limitation
  removed).

- **Architecture doc reconciled with chained-call receivers**: the shared receiver
  step (`src/util.rs`) now resolves a `method_invocation` receiver to the callee's
  declared return type — recursing into the receiver before the callee so a longer
  chain resolves left to right, behind a depth guard — so completion after a call
  (`a.foo().`) and navigation of a member reached through a chain both resolve.
  Updated `architecture.md`: the navigation limitation prose (a chained call drops
  from the untyped-receiver list) and the completion implementation note.

- **Architecture doc reconciled with member completion**: completion after a `.`
  is type-aware again — the `completion` module resolves the receiver's declared
  type through a new shared `src/util.rs` (the receiver step moved out of
  `navigate.rs`) and offers the type's members over the hub's `MembersOf` read
  (now overload-preserving). Updated `architecture.md`: the rework note, the
  Layout (`util.rs` added; the `completion.rs` description), the module table (the
  completion and navigate rows), and the completion implementation note.

- **Architecture doc reconciled with the buffers extraction**: The editor's
  unsaved text now lives in its own `buffers` module — `src/buffers.rs` owns the
  `DocumentStore`, applies the `DocumentOpened`/`DocumentChanged`/`DocumentClosed`
  notifications the shell forwards, and answers a text read; completion and
  navigation read a file's current text through `buffers::current_text` (the open
  buffer, else disk). The old `document.rs`/`DocumentText`/`DocumentChange`
  vocabulary is gone. Updated `architecture.md`: the rework note (a fifth landed
  pass), the Layout (`buffers.rs` added; `document.rs` dropped from deprecated),
  the module table (the document-store row becomes the buffers row, and the shell
  and completion/navigate rows now say "through the buffers module"), and a
  buffers implementation note.

- **Architecture doc reconciled with the completion extraction**: Completion is
  now its own hub module — `src/completion.rs` serves the shell's
  `textDocument/completion` as a `CompletionRequest`, reads the requested file
  itself, and reaches the index only over the hub through a new coarse
  `EntriesByPrefix` read, offering keywords, in-scope names, and workspace
  symbols with auto-imports (the R4 slice — names without types). The
  `byte_offset`/`word_prefix`/`word_at` helpers moved from `src/navigate.rs` into
  the shared `src/parser.rs`. Updated `architecture.md` to match: the rework note
  (a fourth landed pass), the Layout (`completion.rs` added; `parser.rs` now
  serves `completion` too), the module table (shell and a new completion row),
  the shell row's capabilities, and a completion implementation note.

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
