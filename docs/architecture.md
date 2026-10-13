---
type: Architecture
title: Architecture
description: Crate layout, the engine hub every component (the LSP shell included) is a client of, and the data flow inside java-lsp, including the warm-up that fills the index.
tags: [architecture, lsp, rust]
status: draft
---

# Architecture

java-lsp is a single cargo crate. A workspace split into shell/engine crates is
deliberately deferred — the shell is already just another client on the engine
hub (notifications and requests out, notifications in), so a future split is a
transport change rather than a redesign.

> **Rework in progress.** The engine modules are reintroduced one at a time, so
> several modules listed below are deprecated: commented out of `lib.rs` and kept
> out of the build under `src/deprecated/` (only `buffers`, `cli`, `completion`,
> `hub`, `index`, `maven`, `messages`, `model`, `navigate`, `parser`, `server`,
> `util`, and `workspace` are built). This section describes the intended design;
> where a pass has already landed, the text below reflects it.
> Six have landed. The **hub mechanics** are rebuilt — a request is routed to
> the one module that owns it and the reply rides the hub back to the one
> requester (`HubClient::{request, respond}` around an awaitable `Reply`) — with
> `messages.rs` holding the current vocabulary (`Envelope`, `Message`, the
> `Request`/`Response` pairs, and `Notification`). The workspace source scan
> is now the index's own in `index/` rather than a separate `scan.rs` driver. And
> **navigation** is now its own hub module (`navigate.rs`): it serves the shell's
> navigation and reads the index over the hub through a coarse, batched read
> vocabulary (`IndexRequest`/`IndexBatchRequest`), so the index neither contains
> nor serves navigation and the shared Java-reading helpers live in `parser.rs`.
> And **completion** is its own hub module (`completion.rs`): the shell routes
> `textDocument/completion` to it, and it resolves the names offerable at a cursor
> — keywords, the names in scope, the workspace symbols starting with the typed
> prefix (each with an auto-import edit), and, after a `.`, the receiver's members
> through the shared receiver step (the reintroduced `util.rs`). And the **buffers** module
> (`buffers.rs`) now owns the editor's unsaved text: the shell notifies
> `DocumentOpened`/`DocumentChanged`/`DocumentClosed`, and a consumer that needs a
> file's current text reads it there (`buffers::current_text`: the open buffer, or
> the file on disk). And **workspace symbol search** is its own hub module
> (`workspace.rs`): the shell advertises `workspaceSymbolProvider` and routes
> `workspace/symbol` to it, and it maps the index's `EntriesByPrefix` matches to
> `SymbolInformation`. And the **maven** module (`maven/`) parses Maven projects,
> resolves their dependencies offline, fetches the dependency sources into the
> home-folder cache, and publishes the workspace source roots and the extracted
> library roots to the index as two `FoldersDiscoverred` batches with origins,
> superseding the `project.rs` walk. The fuller vocabulary the reintroduced modules bring — the
> `DriverMessage` set beyond those document events, `AnalysisRequest`, the index's
> `IndexHandle` — appears below only as intended design.

## Layout

```
Cargo.toml
Hello.java              — example Java file to open by hand while trying the server
src/
  main.rs     — thin stdio binary: parses the command line (`cli.rs`), tracing
                setup (stderr), tokio runtime, tower-lsp server
  lib.rs      — library root so integration tests can drive the shell; the
                analysis threads' stack size (RUNTIME_STACK_SIZE)
  cli.rs      — the launch configuration: the `clap` flags (`Cli`) and the
                resolved module toggles (`Modules`) the engine consumes
  server.rs   — JavaLanguageServer: the LSP shell (all editor-facing handlers),
                a hub client that routes the navigation queries to the navigate
                module and renders diagnostics, progress, and notices
  hub.rs      — the engine hub: Hub (the inbound loop — it broadcasts
                notifications, routes each request to the module that owns it,
                routes the reply back to its one requester, and logs a compact
                summary of every message) and HubClient (register, notify, subscribe, serve,
                request → Reply, respond)
  index/      — the index subsystem, split by role: `mod.rs` declares the
                submodules, re-exports their operations, and owns `spawn` (the
                workspace source scan and the coarse read server);
                `types.rs` holds index-owned data (WorkspaceIndex — the workspace
                symbol entries, the append-only declared-type base, and the
                per-source models — plus the symbol table and layered views);
                `analyze.rs` reads Java (`extract_entries`,
                `collect_type_infos`) and writes the index (the symbol-table
                upsert, the declared-type store's layer updates, and the context
                rebuild); `query.rs` reads the index; `read.rs` answers the coarse
                reads a consumer makes over the hub; `util.rs` holds index-specific
                helpers shared by analysis and query
  model.rs    — shared Java declaration vocabulary (`SymbolKind`, `Ty`, `Prim`,
                `Param`, `Member`, `TypeInfo`) and its value behaviour; index
                types re-export the old paths for compatibility
  parser.rs   — the shared, stateless Java reading the index, `navigate`, and
                `completion` use: the tree-sitter parser, a file's line index,
                the position conversions and the identifier run at an offset, a
                type node as a `Ty`, and the file's package
  util.rs     — the shared receiver step `navigate` and `completion` use: turns a
                receiver expression before a `.` (`this` or an implicit `this`, a
                field, a parameter or local — a `var` binding through its
                initializer's inferred type — `new T()`, or a `Type.staticMember`)
                into a type over the hub's declared-type reads, and recovers the
                receiver before an incomplete `.`
  navigate.rs — the navigation hub module (`navigate`): serves the shell's
                definition, declaration, implementation, and references
                requests, reading the cursor's file and each candidate source
                from disk itself and the index only over the hub
  completion.rs — the completion hub module (`completion`): serves the shell's
                completion and signature-help requests, reading the requested
                file (its open buffer, else disk) and the index only over the
                hub, and offering keywords, in-scope names, workspace symbols
                (with auto-imports), the receiver's members via `util.rs` after
                a `.`, and a call's overloads with the cursor's active argument
  buffers.rs  — the buffers hub module (`buffers`): owns the editor's unsaved
                text (DocumentStore) — it applies the document lifecycle
                notifications and answers the text read a consumer makes for a
                file's current text
  workspace.rs — the workspace symbol module (`workspace`): serves the shell's
                `workspace/symbol` — the workspace's declarations whose simple
                name matches a query — reading the index only over the hub
  maven/      — the maven module: parses Maven projects, resolves their
                dependencies, and publishes the source roots to the index.
                `mod.rs` owns the spawn (reads `ServerStarted`, drives the
                pass, publishes the two `FoldersDiscoverred` batches with
                origins); `settings.rs` (user Maven settings and the
                local-repository / download-base resolution, including new
                `~/.m2/settings.xml` support), `resolve.rs` (effective poms
                and the offline dependency closure, reworked from the
                deprecated `resolve.rs`), `model.rs` (pom discovery, modules,
                source roots, and the no-pom whole-root fallback — superseding
                the live `project.rs` walk), and `sources.rs` (sources-jar
                reuse/download/extract into the home-folder cache, reworked
                from the deprecated `sources.rs`)
  messages.rs — the hub vocabulary: the Envelope wrapper and Message
                (Request/Response/Notification), the Request/Response pairs, and
                the broadcast Notification variants
  types.rs    — the deprecated binding/resolution engine (R7): name resolution,
                receiver types, overload selection, over the shared model
  deprecated/ — engine modules kept out of the build while the rest are
                reintroduced one at a time; reintroduce one by moving its file
                back under `src/` and declaring it in `lib.rs`
    jars.rs     — the dependency-jar indexer driver: parses resolved jars into
                the index (the `Jars` stage)
    base_cache.rs — the cross-run cache of the class-file base (jars, JDK): one
                guarded file per archive, read on demand
    classfile.rs— minimal jar (ZIP) + class-file reader for dependency indexing
    jdk.rs      — standard-library indexing (JDK discovery, jmods/src.zip/rt.jar)
                and the JDK indexer driver
    startup.rs  — the warm-up coordinator: walks the project model, emits `Ready`,
                the summary, and progress for the enabled stages, and warns when
                dependency sources are disabled
    engine.rs   — the one setup path (`start_with`: the hub plus every enabled
                participant's `spawn`, the shell included; `start` runs
                everything the environment enables)
    diagnostics.rs — the diagnostics subsystem: its own parser and open-document
                state, computing syntax and unresolved-symbol diagnostics and
                reporting them to the hub, plus a queryable cache of the latest
                pass per open document
    quickfix.rs — the quick-fix subsystem: its own parser and open-document text,
                generating the create/import/rename fixes from the symbol index
                and the diagnostics cache
    analysis.rs — the engine core (TreeSitterEngine): parse trees, symbols,
                folding, semantic tokens, completions, navigation, inlay hints;
                and the analysis module that owns it on the hub
tests/
  harness.rs      — drives JavaLanguageServer through tower_lsp::LspService
  stdio_smoke.rs  — drives the real binary over stdio with raw LSP JSON-RPC
example/          — sample multi-module Maven project (see example/README.md)
zed-java-lsp/           — Zed extension: Java language + tree-sitter-java grammar
                          + java-lsp language server (queries vendored from zed-extensions/java)
```

## Components and data flow

```mermaid
graph TD
    C[Editor client] -- LSP over stdio --> S[JavaLanguageServer]

    S -- "input notifications, query requests" --> HUB(("hub.rs — the hub"))
    HUB -- "Diagnostics, Progress, Notice" --> S

    HUB -- "document events, text requests" --> DOC{{document module}}
    DOC -- owns --> D[(DocumentStore)]
    DOC -- "text" --> HUB

    HUB -- "document events, queries" --> AN{{analysis module}}
    AN -- "index updates, AnalysisUpdated" --> HUB
    AN -- owns --> TS[analysis.rs: TreeSitterEngine]

    HUB -- "document events, AnalysisUpdated" --> DSUB{{diagnostics subsystem}}
    DSUB -- Diagnostics --> HUB

    HUB -- "documents, code actions" --> QF{{quick-fix subsystem}}

    HUB -- "index messages, coarse reads" --> IDX{{index subsystem}}
    IDX -- owns --> WI[(WorkspaceIndex)]
    IDX -- "base + source types" --> TL[(type base + source models)]
    IDX -- "walks + parses .java" --> WS[workspace .java files]

    HUB -- "navigation queries" --> NAV{{navigate module}}
    NAV -- "coarse index reads" --> IDX
    NAV -- "reads .java" --> WS
    NAV -- "locations" --> HUB

    HUB -- "workspace symbol searches" --> SYM{{workspace module}}
    SYM -- "coarse index reads" --> IDX
    SYM -- "symbols" --> HUB

    HUB -- "every DriverMessage" --> DR{{startup + drivers: startup, dependency, jar, JDK, download}}
    DR -- DriverMessage --> HUB
    DR -- "class files" --> JV[local repo jars]
    DR -- "archives" --> ARCH[JDK jmods / src.zip]
    DR -- "download + extract" --> SR[sources cache]

    HUB -- "tracing logs (sender, latency), Log, Summary" --> E[(stderr)]
```

**Hub and spoke.** The hub is the single **hub**; every component attached to it — the LSP shell included — is a **spoke**, a hub client that talks only to the hub and never to another spoke directly. A spoke **notifies** (a broadcast to every subscriber) or **requests** (routed to exactly one owning module — named by `owner_of` — and answered as an awaitable `Reply`, the reply riding the hub back to that one requester), and every notification, request, and reply passes through the hub. The shell is the only spoke that knows about the editor; the hub carries no editor knowledge. `messages.rs` holds the hub vocabulary — `Notification`s and the `Request`/`Response` pairs — that every spoke speaks.

| Module                                                                                       | Description                                                                                                          | Responsibilities                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| -------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Hub** (`hub.rs`)                                                                           | The engine's central mediator — the hub of the hub-and-spoke.                                                        | Owns the FIFO inbound channel; registers clients and sorts what each receives through a per-client dispatcher; broadcasts notifications to every subscriber; routes each request to the module that owns it (`owner_of`) and the reply back to its one requester; resolves an unowned request to the empty reply; logs a compact summary of every message it forwards. Carries no editor knowledge.                                                                     |
| **LSP shell** (`server.rs`)                                                                  | The `tower_lsp::LanguageServer` implementation — a hub client like any other, never touching the core directly.      | Serves every editor-facing method; advertises capabilities in `initialize` (incremental text sync, completion, signature help, definition, declaration, implementation, references, workspace symbols); routes the navigation queries — `textDocument/definition`, `textDocument/declaration`, `textDocument/implementation`, and `textDocument/references` — to the navigate module as `NavigateRequest::FindDeclaration`/`FindReferences`/`FindImplementation` hub requests and `textDocument/completion` and `textDocument/signatureHelp` to the completion module as `CompletionRequest`s, rendering each reply; notifies input (`DocumentOpened`/`DocumentChanged`/`DocumentClosed`) for the buffers module; renders `Diagnostics`, `Progress` (as `$/progress`), and `Notice` (as `window/showMessage`) on its own drain task. Its hub channel is unbounded, so a slow client never stalls the hub.                                      |
| **Buffers module / DocumentStore** (`buffers.rs`)                                            | The editor's unsaved text — the canonical open-buffer store, a hub client, not owned by the shell.                   | Subscribes to `DocumentOpened` (full text), `DocumentChanged` (applies the incremental changes itself), and `DocumentClosed`; answers `BuffersRequest::Text` — a file's current text, or `None` when it is not open — and offers `buffers::current_text` (the open buffer, else the file on disk) as the read a consumer calls; converts LSP positions (line + UTF-16 code units) to byte offsets, the only place position semantics are handled. Its `select!` is biased, so a queued document event is always applied before a read is served. |
| **Engine wiring** (`engine.rs`, `cli.rs`)                                                    | The single `start_with` path that builds the hub and boots every participant the launch configuration enables.       | Calls each enabled participant's `spawn` — shell first, then modules, then the startup coordinator and drivers — each of which registers under its own name (`Hub::register`), subscribes for the notifications it renders, serves the requests it owns (`serve()`), and starts its own thread or task. `start` runs everything the environment enables. Pure wiring: no channel, name, or receive loop.                                           |
| **Analysis module / engine core** (`analysis.rs`, `TreeSitterEngine`)                        | Owns the open documents' parse trees and answers the shell's queries.                                                | Parses each open document with `tree-sitter-java`; serves document symbols, folding ranges, semantic tokens, completions, hover, definition/declaration, implementation, references, rename, and inlay hints; applies editor input inline in arrival order and runs each query on the blocking pool, so a slow search never delays typing; notifies `AnalysisUpdated` after each applied document or file event (it does not compute diagnostics). |
| **Type layer** (`model.rs`, `index/types.rs`, deprecated `types.rs`)                                                | Shared Java declarations (`model.rs`), index-owned storage/views (`index/types.rs`), and the deprecated binding/resolution engine (`types.rs`).                           | Models types, members, hierarchies, generics, records, enums, and syntactically-read Lombok members; infers receiver types; resolves names in Java precedence order; selects overloads by argument types. Backs completion, hover, signature help, inlay hints, and semantic diagnostics through a cached, layered, dirty-overlay view of the base and per-source models.                                                                          |
| **Diagnostics subsystem** (`diagnostics.rs`)                                                 | A self-contained parser and open-document state, separate from the core.                                             | Sweeps on `AnalysisUpdated` (the just-edited file first, then every open document) for syntax errors and unresolved symbols; reads the index and type layer through `IndexHandle` with per-sweep caches; publishes one `Diagnostics` per open document and keeps a queryable cache of each document's latest pass.                                                                                                                                 |
| **Quick-fix subsystem** (`quickfix.rs`)                                                      | A self-contained parser and open-document text.                                                                      | Turns unresolved-symbol diagnostics into `CodeAction`s — add an import, change to a near member, or create a stub type/member; reads the symbol index (`IndexHandle`) and the diagnostics cache (`DiagnosticsForDocument`); answers the shell's `codeAction` request as an awaited hub request.                                                                                                                                                    |
| **Index subsystem / WorkspaceIndex** (`index/`)                                            | A workspace-wide in-memory symbol index owned by a dedicated task.                                                   | Stores flat `SymbolEntry`s (name, kind, package, container chain, ranges, `origin`/`synthetic`); owns the workspace source scan — it walks the reported folders on `FoldersDiscoverred`, parses each `.java` file into entries (`extract_entries`) and its declared-type model (`collect_type_infos`); answers a coarse, navigation-shaped read vocabulary over the hub (`IndexRequest`/`IndexBatchRequest` — entries by name, one file's symbols, the workspace files, a type's supertype closure, a type's members, whether a type resolves, and the workspace types reaching a contract); grows the declared-type base append-only, one layer per artifact.          |
| **Navigate module** (`navigate.rs`) | Cursor navigation — the declaration a symbol names, every reference to it, and its implementations. | Its own hub client, labelled `navigate`; the shell routes `textDocument/definition`, `textDocument/declaration`, `textDocument/implementation`, and `textDocument/references` to it as `NavigateRequest`s. Reads the cursor's file and each candidate source through the buffers module (`buffers::current_text` — the open buffer, else disk) and reaches the index only through the hub's coarse reads (`IndexRequest`/`IndexBatchRequest`); resolves the word under the cursor by node kind and, for a member, through the receiver's declared type (the shared receiver step in `util.rs`). |
| **Completion module** (`completion.rs`) | Autocomplete — the names offerable at a cursor, and a receiver's members after a `.` — plus signature help, a call's overloads and active argument. | Its own hub client, labelled `completion`; the shell routes `textDocument/completion` and `textDocument/signatureHelp` to it as `CompletionRequest`s. Reads the requested file through the buffers module (`buffers::current_text` — the open buffer, else disk) and reaches the index only through the hub's `EntriesByPrefix`, `MembersOf`, and `ConstructorsOf` reads; offers Java keywords, the locals/parameters/fields in scope, and the workspace symbols starting with the typed prefix, each workspace symbol carrying an auto-import edit when the file does not already resolve the name; after a `.` it resolves the receiver's declared type through the shared receiver step (`util.rs`) and offers that type's members — fields first, then one item per method overload with its full signature — inherited members included, statics only for a type qualifier, and nothing for a receiver it cannot type; a call the cursor sits in (or a `new T(…)`) is answered as signature help — the callee's same-named methods (`MembersOf`, statics only for a type qualifier) or the created type's constructors (`ConstructorsOf`) rendered with each overload's signature and the active parameter counted from the cursor's argument, and nothing when the callee or its receiver cannot be resolved. |
| **Workspace module** (`workspace.rs`) | Workspace symbol search — the workspace's declarations whose simple name matches a query. | Its own hub client, labelled `workspace`; the shell advertises `workspaceSymbolProvider` and routes `workspace/symbol` to it as a `SymbolRequest::Search`. Reads the index only through the hub's `EntriesByPrefix` read (an empty query matches every symbol) and shapes each entry as a `SymbolInformation` — the entry's `selection_range` as the location, its innermost enclosing type as `container_name`, its kind mapped to the LSP kind — dropping imports, constructors, Lombok-synthetic members, and `Dependency` class-file entries. |
| **Warm-up coordinator** (`startup.rs`)                                                       | Drives the warm-up sequence and shapes its events to the launch configuration.                                       | On `FolderAdded` it walks the project model and drives the warm-up stages — the index's own source scan included; tracks `StageDone` for the enabled stages only; emits `Ready`, the summary, and the progress end; and warns when dependency sources are disabled. The warm-up drivers stay configuration-free.                                                                                                                                   |
| **Maven module** (`maven/`) | Parses Maven projects and publishes their source roots — the workspace's own and the extracted dependency sources — to the index. | Its own hub client, labelled `maven`; on `ServerStarted` it builds the project model (`model.rs`: every `pom.xml` becomes a module with its effective pom and declared source roots, whole-root fallback when none exist), resolves each module's dependency closure strictly offline from the local repository (`resolve.rs`), fetches each resolved artifact's `-sources.jar` into the local repository and extracts it under the sources cache (`sources.rs`), honoring `~/.m2/settings.xml` (`settings.rs`: `localRepository`, mirrors, `offline`; `$MAVEN_REPO` overrides the local repository), then publishes two `FoldersDiscoverred` batches — the workspace source roots with `origin: Workspace` and the extracted dependency roots with `origin: LibrarySources` — which the index scans and tags `FileOrigin::Workspace` / `FileOrigin::LibrarySource` (the latter completable and openable, never a references/rename candidate). Never invokes `mvn`. `--no-dependencies`/`--no-downloads`/`$JAVA_LSP_OFFLINE` gate the resolution and sourcing. |
| **JDK indexer** (`jdk.rs`)                                                                   | Indexes the installed JDK's standard library.                                                                        | Discovers the JDK (`$JAVA_LSP_JDK`, `$JAVA_HOME`, common locations incl. SDKMAN) and indexes `jmods/*.jmod`, `lib/src.zip`, or `rt.jar` with streaming per-entry reads; keeps only `java.*`/`javax.*`; a missing JDK is a graceful no-op.                                                                                                                                                                                                          |
| **Jar indexer** (`jars.rs`)                                                                  | Warm-up driver that fills the index. The source scan that used to sit beside it is now the index's own (`index/`). | Parses resolved jars into the index (the `Jars` stage); speaks only `DriverMessage`s.                                                                                                                                                                                                                                                                                                                                                              |

## Implementation notes

Detailed behaviour, edge cases, and the documented v1 limitations behind each module in the table above.

Every arrow into or out of the hub is a hub message: the analysis, diagnostics,
quick-fix, navigate, completion, and workspace modules also read the index (and
the quick-fix module the diagnostics cache) through hub-routed requests.

- **LSP shell** (`server.rs`): implements `tower_lsp::LanguageServer`.
  `initialize` advertises incremental text sync, hover, definition,
  declaration, implementation, completions, signature help, document symbols, workspace
  symbols, folding
  ranges, semantic tokens, references, rename, code actions (kind `quickfix`),
  and inlay hints.
  `didOpen`/`didChange`/`didClose` notify the hub
  (`DocumentOpened`/`DocumentChanged`/`DocumentClosed`, the full text shared
  behind an `Arc`); `didChange` first asks the document module for the new text,
  since the shell holds no store; the diagnostics come back as `Diagnostics`
  notifications on the shell's own hub channel, which the shell's drain task
  publishes. `initialized` also registers
  `workspace/didChangeWatchedFiles` for `**/*.java` when the client advertises
  `workspace.didChangeWatchedFiles.dynamicRegistration` — a fire-and-forget
  `client/registerCapability`, so the handshake never waits on the client — and
  the `didChangeWatchedFiles` handler notifies one `FileEvent` per
  created/changed/deleted file; without the capability the watcher is simply
  skipped and everything else stands (D6).
  Query handlers await their hub request's `Reply`. That
  same drain task renders the `Progress` notifications as
  `window/workDoneProgress/create` plus `$/progress` — one status-bar item
  titled `java-lsp`, whose message names the current phase and whose percentage
  rises during the source download — and each `Notice` as a single
  `window/showMessage`. Progress and notices are gated on the client having
  advertised `window.workDoneProgress` in `initialize`; without it they are
  dropped silently. A `window/workDoneProgress/cancel` is ignored — the
  background job is not cancellable.
- **Document module** (`document.rs`): owns the canonical `DocumentStore`, a hub
  client like any other — the shell holds no document state. It keeps UTF-8 bytes
  per URI with the client version, subscribes to `DocumentOpened`/`DocumentClosed`,
  and applies the editor's incremental `TextDocumentContentChangeEvent`s in order
  when the shell sends a `DocumentChange` request, returning the new text; a
  `DocumentText` request returns the current text and version. LSP positions
  (line + UTF-16 code units) are converted to byte offsets here — bytes are what
  tree-sitter consumes, and UTF-16 conversion is the only place position semantics
  are handled on the way in.
- **The shell on the hub** (`server.rs`, `engine.rs`, `analysis.rs`): the shell
  never touches the core directly — it is just another hub client.
  `engine::start` (the `LspService` constructor) starts the hub and calls each
  participant's `spawn`: the shell first, then the modules, then the drivers.
  Each `spawn` registers under its own name (`Hub::register`), subscribes for the
  notifications it renders, and serves the requests it owns (`serve()`), before
  starting its own thread or task, so `engine.rs` is pure wiring (no channel, no
  name, no receive loop) and the shell's client logs as `server`. The shell
  **notifies** the editor's input — `FolderAdded` (the root, in
  `initialized`), `ClientCapabilities`, `DocumentOpened`/`DocumentChanged`/
  `DocumentClosed`, and `FileEvent` — and **requests** each query as a
  `Request::Analysis(AnalysisRequest::…)`, which the hub routes to the
  **analysis module**; code actions go to the quick-fix module. Every
  `HubClient` request returns a `Reply` — a thin wrapper over a `tokio` oneshot
  receiver — and the owning module answers it by id with `HubClient::respond`.
  A query handler awaits it, so no runtime worker blocks; the
  synchronous module code (the core, diagnostics, quick-fix), which runs on its
  own threads or the blocking pool, calls `Reply::blocking_recv` instead. The
  analysis module is a thread that owns the core and
  applies the editor's notifications **inline, in arrival order**: one sender, a
  FIFO hub, and a FIFO module channel mean an edit always lands before the query
  the client sends at the new cursor. It hands each query to the runtime's
  **blocking pool**, so a slow `references` never delays typing. It does **not**
  compute diagnostics: after applying a document or file event (and sending the
  index the core's updates) it notifies `AnalysisUpdated`, and the **diagnostics
  subsystem** sweeps on that, so an edit and the queries that follow it are never
  queued behind a sweep, while the sweep still reads an index that holds the edit.
  Each pass covers **every** open document, the just-edited one first, so a
  referring file's squiggles clear without an edit of its own (D3); a close
  clears the closed document with an empty `Diagnostics` and republishes the rest.
  From the background warm-up come `Progress(ProgressUpdate)` (begin/update/end
  with a phase message, a count, and an optional download percentage) and
  `Notice { level, text }` notifications, which the shell renders, and `Log` and
  `Summary`, which the hub renders as `tracing` lines — the hub carries no editor
  knowledge. The shell's hub channel is unbounded, so a slow client can never
  stall the hub. A request with no registered owner, or a reply the module never
  sends, resolves to the empty reply (`Response::default()`) rather than an error.
  The core itself:
- **Diagnostics subsystem** (`diagnostics.rs`): owns a `tree-sitter-java` parser
  and the open documents' text, parses each open buffer itself, and computes the
  pass — syntax errors, or the unresolved-symbol checks — reading the workspace
  symbol index and declared-type layer from the **index subsystem** through
  `IndexHandle`. A sweep reads the index through one `SweepIndex`: the
  declared-type overlay, the workspace layer, and the readiness gate are fetched
  once per sweep (not per document), and the names every open document's pass
  will look up — type names, unbound symbolic identifiers, import simple names
  and static-import owners — are prefetched in one `IndexQueryNames` request,
  with wildcard-import packages checked in one `IndexHasPackages`. Lookups are
  served from that per-sweep cache; a name the prefetch missed costs one cached
  `IndexQueryName`. A new sweep starts from empty caches, so index changes are
  seen on the next sweep. It reports one `DriverMessage::Diagnostics` per open document,
  which the shell publishes. A document event records the text and version; a
  close drops the document and notifies an empty `Diagnostics` (version `None`)
  that clears it; the sweep itself runs on the analysis module's
  `AnalysisUpdated`, which follows every applied document or watched-file event.
  The latest pass per
  open document is kept in a **cache** that the quick-fix subsystem queries
  (`DiagnosticsForDocument`), so a fix never recomputes it.
- **Quick-fix subsystem** (`quickfix.rs`): owns a `tree-sitter-java` parser and
  the open documents' text, parses the buffer itself, and turns the
  unresolved-symbol diagnostics into `CodeAction`s — add an import, change to a
  near member, or create a stub type/member. It reads the **symbol index**
  through `IndexHandle` and the **diagnostics cache** through a
  `DiagnosticsForDocument` request
  (falling back to it when a request carries no diagnostics), so it holds no
  index or diagnostics state. The shell's `codeAction` is a
  `QuickFixForDocument` request routed to it.
- **Engine core** (`analysis.rs`, `TreeSitterEngine`): parses each open document with
  `tree-sitter-java` and keeps one tree (plus the text) per URI, for the query
  features. Declaration nodes become hierarchical
  document symbols (class/interface/enum/record/constructor/method/field);
  declarations and brace blocks spanning multiple lines become folding
  ranges; a curated node-kind mapping produces delta-encoded semantic tokens
  (declaration names, types, strings, comments, numbers — legend exported as
  `SEMANTIC_TOKEN_TYPES` for the shell's capability). Point columns from
  tree-sitter are byte-based and converted to UTF-16 at the LSP boundary.
  Completions merge three ranked sources as `sort_text` prefixes `0`/`1`/`2`
  (clients re-sort alphabetically): Java keywords; names in scope at the
  cursor from the open document's tree — the innermost enclosing
  method/constructor's parameters and locals plus the enclosing type's fields
  (nameable unqualified, so no wrong membership is claimed); and workspace
  index symbols matched by `query_prefix` on the simple name. A method is
  offered as one item per overload (its declaring type resolved in the type
  layer), labelled with its full signature and inserted under its bare name
  followed by `(` so accepting it opens the argument list; a field keeps its
  `Container.name` label and a type its plain name, and a method whose
  overloads cannot be resolved in the model falls back to the plain name.
  Symbols that share a simple name but are imported differently stay separate
  items — each carrying its own import edit and labeled with its owner
  (`class of java.awt` alongside `interface of java.util`) — while entries for
  one symbol, or a name with the same import, collapse to one. The index query
  takes only a brief read
  lock, so while the workspace scan is warming up completions simply contribute
  whatever is indexed so far — partial, never blocking (R6). Member access is
  answered from the type layer (see the `types.rs` bullet): the receiver's type
  is inferred and each of its members is offered — every overload of a method
  keeping its own item, labelled with the full signature and inserting `name(`,
  fields keeping their name and their type in `detail` (inherited members
  included for workspace types), an enum's constants offered as enum members,
  and — when the receiver names a type — its nested types offered alongside its
  static members, and a receiver that names a package its top-level types
  (`java.util.Li` → `List`) — while an uninferrable receiver still returns an
  empty list, claiming nothing a type-free engine
  cannot verify. The receiver's members come from the model layered with
  **all** open buffers (see the `types.rs` bullet), so a member added to
  another open file — or to a file a watcher event re-read — is offered without
  reopening this one. The receiver of a `.` is the expression ending at the dot
  before the name being typed — normally the tree's member access, but a partial
  name the parser reads as a type (`Greeter.Inn` in a declaration, `new
SumType.T…`) would otherwise hide its qualifier, and an incomplete `receiver.`
  at the end of a line can parse the dot into the next token (a following `var`
  line reads `gson.var` as a scoped type identifier) — so it is recovered from
  the source as the expression ending at the last non-whitespace byte before that
  dot, and the same members are offered whether or not the name is already typed.
  Auto-import: every index-sourced item whose symbol lives outside the open
  file's package carries an `additionalTextEdits` inserting
  `import <fqcn>;` (after the last import, else after the `package`
  statement, else at the top); the index stores each entry's package so the
  fully qualified name is known at completion time. The never-worsen rules
  suppress the edit when the name already resolves — same file, same
  package, an equal explicit import, a matching `.*` wildcard — or when it
  would conflict (the simple name already imported from another package, or
  declared in the file's own package); a suppressed edit leaves the plain
  insert text, never a compile error. Keywords and locals never carry
  edits, and jar members import their enclosing type.
  Navigation works off the same index. Definition resolves the full word
  under the cursor: inside an import declaration the dotted path's last
  segment is looked up (a `.*` wildcard or an unindexed JDK/library import
  honestly yields nothing; a `static` import also admits method/field
  targets); otherwise an exact-name lookup over declaration entries is
  narrowed by the node kind at the cursor — a `type_identifier` restricts to
  type declarations, a method-invocation or field-access name to methods and
  fields. A member call is resolved further through the receiver's declared type
  — the expression before the `.` (`this` or an implicit `this`, a field of the
  enclosing type, a parameter or local of the enclosing method, `new T(…)`, or a
  `Type.staticMember` qualifier) turned into a type through the declared-type
  model, the name narrowed to the most-derived declaration the receiver's
  supertype closure provides, a name with no such receiver keeping the name-only
  answer. The index reports the first candidate by position among what remains;
  argument-type overload selection (landing `x.add(1)` on `add(int)` rather than
  the first same-named declaration) and `new T(...)` constructor selection are the
  type layer's, not this name-and-receiver lookup's (see the `types.rs` bullet).
  `textDocument/declaration` is answered by the same resolution: Java has no
  declaration/definition split, so both requests land on the same place.
  Workspace declarations and source-backed library declarations (see
  the dependency-sources bullet) both qualify; a class-file jar declaration does
  not, since its location is not openable. A single candidate — or several sharing one file (method overloads,
  same-file repeats), resolved to the first by position — is an answer;
  anything spread across multiple files returns no location rather than a
  wrong one (no result beats a wrong result). A type name is first narrowed to
  the types the cursor's file can see — its own, its package's, or an import —
  so a same-named type elsewhere is never mistaken for ambiguity. The exception
  is a cursor on a declaration's own name, which resolves to that declaration,
  or, when the declaration overrides a supertype member, to the nearest
  declaration it overrides in the type's supertype closure; only a use keeps the
  cross-file ambiguity policy. **Workspace symbol search** — the `workspace`
  module's `Search` — maps the index's `EntriesByPrefix` matches to
  `SymbolInformation`: import entries, constructors, Lombok-synthetic members,
  and `Dependency` class-file entries are dropped, the innermost container is the
  `container_name`, and the selection range is the location, with an empty query
  returning everything indexed for the client to filter. Like completions, both
  serve partial results during warm-up and never block (R6). **Go to implementation** — the `navigate` module's
  `ImplementationsOf` read — answers `textDocument/implementation` from the same
  binding and the declared-type
  hierarchy: a cursor on a type lists every workspace source type whose
  supertype closure reaches it — sub-interfaces and abstract intermediates
  included, the contract itself excluded — and a cursor on a method lists every
  workspace subtype that declares an override of the same name and parameter
  types (a subtype that only inherits the member is not listed). The contract
  may be a library or JDK type — a cursor on `Runnable` lists the workspace
  classes that implement it — since every result is a workspace declaration;
  dependency and JDK declarations are never returned, and a field, constructor,
  or local cursor, or a contract with no workspace implementation, yields
  nothing. **Hover** resolves the symbol under the cursor through the
  type layer and renders its declaration as Markdown — a member's signature, a
  type's declaration, or a local's declared type — returning nothing when the
  symbol is unresolved or ambiguous. Documented v1 limitations: a bare name is
  still a pure lookup with no receiver-type resolution (a member call is
  resolved through its receiver and arguments, above), so an unqualified `foo`
  may hit a same- or cross-file declaration by name or return nothing; overload
  selection covers only the assignability relation's conversions and refuses
  when they are inconclusive; and a watched file's indexed ranges reflect its
  last re-read, not a live document.
  **References and rename** resolve the symbol under the cursor the way hover
  does — a member's declaring type coming from the receiver's type, identified
  by simple name _and_ package so a same-named type elsewhere is never touched —
  and then search the workspace's `.java` files (each pre-filtered by a
  substring check before being parsed, on demand, on the request path, with a
  private parser taken from a small pool rather than the shared parse mutex, so
  a search never blocks typing or another search) for
  occurrences that can be attributed with confidence: a type only in files that
  can see it (its own file, its package, or an import), a
  member access only where the receiver's declared type — or a supertype of it —
  resolves the name back to the same declaring type, an unqualified member name
  only inside the declaring
  type's own span, and a local only inside its method and only when that method
  declares the name once. An overloaded method is narrowed to the selected
  overload: an occurrence counts only when its call's argument types accept that
  overload's parameters (a method reference, with no argument list, is left
  out), and only the selected declaration is reported. `rename` deliberately
  stays name-group-wide — it renames every overload of the name, never one
  overload that could leave a call site behind. A declaration is reported only when
  `include_declaration` asks for it: the search skips declaration names, and the
  index (or, for a local, its own recorded location) supplies them. A cursor on
  a non-terminal segment of an import path targets nothing. `rename` builds one
  `TextEdit` per reference and **refuses** (`null`) otherwise — an unresolved or
  ambiguous target, a library declaration, an invalid identifier, or a search
  that could not read and parse every candidate file.
  Trees are rebuilt from the full text per edit — one file per change, never
  the workspace; `InputEdit`-based incremental reparsing (needs edit ranges
  forwarded from the shell) is a later optimization.
- **Type layer** (`model.rs`, `index/types.rs`, deprecated `types.rs`): the
  shared declared-type vocabulary (`model.rs`), the index's storage and layered
  views (`index/types.rs`), and the deprecated binding/resolution engine
  (`types.rs`), chosen for R7. It models
  declared types — primitives, `void`, `null`, arrays, named references with
  their generic arguments kept as written, and type variables — and keeps each
  type's package, kind, supertypes (`extends`/`implements`), fields, and
  methods with their declared types (source-declared methods also keep their
  parameter names, which class-file descriptors cannot supply), and its
  constructors. A constructor is a member of its declaring type but is kept
  apart from `methods`, so it never appears in a `.`-completion listing; a
  record's canonical constructor and a class's implicit no-arg one are
  synthesized from the source. A source declaration's Lombok annotations are read
  syntactically (by simple annotation name — no annotation processor, and
  `lombok.config` is ignored): `@Getter`/`@Setter`/`@With`, `@Data`/`@Value`,
  `@Accessors`, `@Builder` (a nested `TBuilder`), the log-field family, and the
  constructor annotations append the members Lombok would generate, so
  `.`-completion, hover, signature help, inlay hints, and the unresolved-member
  diagnostic see them; `equals`/`hashCode`/`toString` are deliberately not
  synthesized. A source
  record's components are modelled as accessor methods — the component's type
  with no parameters — so an outside receiver sees `x()` and never the private
  backing field, while inside the record the bare component name resolves as
  that field would. A source enum's constants are modelled as static members
  typed as the enum (kind `EnumConstant`, so they complete as `enumMember` and a
  chained `TYPE_1.rank()` resolves), and the enum's `;`-introduced declaration
  section is walked, so its own fields, methods, and constructors are members
  too. The model has two parts. A **non-source base** holds the
  resolved jars, the JDK, and extracted dependency sources as `TypeInfo`s with
  real signatures and supertypes, parsed from their class files (or, for a
  source-only JDK, from `lib/src.zip` through the same tree-sitter extractor);
  it grows append-only, one layer per artifact URI, as each producer's jars, JDK
  archives, and extracted sources land, and is read through a cached,
  name-indexed view. Each workspace source file keeps its own
  model, built from the tree the scan already parses, so one file can be
  forgotten by dropping its own model rather than rebuilding the base. Member
  lookup walks supertypes breadth-first, cycle-guarded, first declaration
  winning, so inherited members are found. The type layer is not frozen at
  warm-up: the engine reads the base **through a cached layered view**
  (`ModelLayers`) over the current per-source models, with a **dirty** overlay —
  every open buffer, plus any file a watcher event or a close re-read from
  disk — replacing its file's warm-up model per URI. The base view is a
  name-indexed `SourceLayerIndex` cached by `WorkspaceIndex` behind a
  source-model generation, so it is rebuilt only when the sources change, not
  per request; a lookup consults only the layers that declare the name rather
  than scanning all of them. The view holds the per-file models behind their
  `Arc`s and answers in precedence order, so a request never re-merges (copies)
  the workspace model; the per-file split
  is what keeps a deleted file forgettable by a single map removal
  (`large-project-memory`). So completion, hover, signature help, inlay hints, and
  semantic diagnostics answer from unsaved edits to any open file (the same
  edits `definition` reaches through the index), and a file deleted on disk is
  forgotten from the index and the model at once (D4, D5).
  **Binding** turns a cursor position into an answer: it collects the names
  visible there (locals and parameters with their declared types — an
  enhanced-for binding written `var` taking its iterable's element type and a
  try-with-resources `resource` binding collected like a local — the enclosing
  type's fields, type parameters, imports including `.*`, and the file's
  package) and then resolves a simple name in Java's precedence order, or
  infers the type of a `.`-receiver (`identifier`, `this`/`super`, a dotted
  nested or package-qualified type name such as `Outer.Inner` or `java.util.List`,
  `new T(...)`, chained field/method access, and — as a `var` initializer — a
  conditional
  (a lone `null` yielding the other branch), array creation, `instanceof`, or
  `switch` expression). Everything the layer cannot pin to a single
  answer is `Unknown`, and callers treat that as "no answer".
  **Semantic diagnostics** report unresolved symbols at `ERROR` severity across
  four families: a simple type name in a declaration, `new`, a cast, `extends`,
  or `implements`; a member access on a receiver whose type is known; a bare
  identifier that resolves nowhere; and an `import` whose target the index
  cannot supply (a single type, a best-effort `.*` wildcard, or a `static`
  import). A name known in the index but not visible here (no import, another
  package) is reported on its _usage_, with a quick fix to add the import; a
  name nowhere in the index offers a create-stub fix; an unresolved member
  offers a did-you-mean rename. All of it is gated on the model actually
  vouching for `java.lang` (an indexed JDK) and on the file parsing cleanly, so
  neither a missing JDK nor a broken file turns into a wall of false positives;
  a missing dependency is reported (imports and usages go red) rather than
  silently skipped. Qualified names, generic arguments, `var`, type parameters,
  and any name bound anywhere in the file are left alone rather than guessed at
  (the scope collector does not model every binding kind, so a lambda parameter
  or catch binding is never reported), and the check is switched off wholesale
  by `JAVA_LSP_SEMANTIC_DIAGNOSTICS`.
  Known approximations, documented rather than hidden: a simple type name
  resolves in Java's order — an exact single-type import, the file's own
  package, a wildcard import, then a unique model match — so a name shared
  across packages (e.g. `java.util.List` vs `java.awt.List`) is resolved only
  when the file pins it down; a nested type is keyed by its innermost simple
  name with its enclosing chain recorded (so two `Inner`s in one package
  coexist rather than overwrite each other); a dotted reference with no `$`
  (`Outer.Inner`) is resolved nested-first — the prefix as an in-scope type,
  then the last segment among that type's nested types — before falling back to
  reading the prefix as a package (`java.util.List`), and a class file's
  `Outer$Inner` descriptor reaches the type as `Inner`; a supertype is
  looked up in the package the name carries, so a class-file base whose simple
  name is shared across packages still resolves; a call's type arguments are
  inferred from its arguments (a method's own type parameters and the
  receiver's, with primitives boxed) and substituted into the result, while a
  call the layer cannot pin down keeps the written form; a floating literal is
  `float` when written with an `f`/`F` suffix and `double` otherwise; overloads
  are chosen by argument types where an `assignable` relation (identity,
  primitive widening, boxing/unboxing, `null` to a reference, subtyping through
  the hierarchy, arrays, erasure) decides, by arity when a single candidate
  shares it, and by name alone elsewhere — `member_for_arguments` for the loose
  navigation answer and `member_for_arguments_confirmed` for callers that must
  not name the wrong overload (inlay hints); and the implicit
  `java.lang.Object` is not recorded as a supertype (matching source-extracted
  types), so its members never appear in a `.`-completion listing. They do
  resolve, though, as a fallback when the hierarchy walk finds nothing — so an
  inherited `toString()`/`equals()`/... types a call, hover, and `var`
  inference — and only for a known reference-ish receiver (`Ref`/`Var`/`Array`),
  never an `Unknown` one.
  Overload selection by argument types is now
  available to definition, find-references, completions, and inlay hints (see
  the type layer's
  `assignable` relation); full Java overload resolution, lambdas, casts, and
  static-import member resolution remain follow-up work (R7 remainder, R8).
  Each semantic diagnostic carries a `code` (`unresolved-type`,
  `unresolved-member`, `unresolved-symbol`, `unresolved-import`) and `data`
  (`{ name, fixes: [...] }`), which the shell's code-action handler turns into
  quick fixes: one "Add import" per candidate (from `import_edit`), a "Change to
  `x`" rename for a near member, or create-symbol stubs. The create-type fix
  offers four actions (class / interface / enum / record), each a `CreateFile`
  resource operation plus a `TextDocumentEdit` for a new file under the source
  root of the file's own package, offered only when the client advertises
  `workspace.workspaceEdit.resourceOperations` including `CreateFile` (read in
  `initialize` and forwarded to the engine). A created method, field, or local
  takes its signature from the usage — parameter types from the call's
  arguments (names from bare identifiers), the return type from the
  assignment/declaration/`return` context, the local's or field's type from its
  initializer — falling back to `Object`/`void`. An unresolved member on a
  _workspace_ receiver type offers "Create method/field in `T`", which inserts
  the stub into `T`'s file (the open buffer, else disk) as an unversioned
  `changes` edit; a jar/JDK receiver offers nothing, since its source is not the
  user's to edit.
- **Inlay hints** (`analysis.rs`, R9): computed on demand for the range
  the client requests — the tree walk is pruned to that range, so cost scales
  with the visible text rather than the file. Three families: variable type
  hints (`: Type` after a local or field name), parameter name hints at a call
  site (`name:` before an argument), and chained-call hints (the return type of
  each intermediate link — an invocation immediately dereferenced by a further
  `.field`/`.method()` — while the outermost call and standalone calls are left
  alone). A local declared with `var` has its initializer's type inferred
  through `receiver_type` — including a call's type arguments, so
  `List.of(5)` is `List<Integer>` — and that inference also feeds the scope, so a
  `var` local completes and hovers like an explicitly typed one; an
  enhanced-for binding written `var` shows its iterable's element type the same
  way. Parameter names
  come from the type model: source-declared methods keep them (each `Member`
  parameter carries a name and a type) and the overload is selected from the
  call's argument types — arity only when a single candidate shares it — so
  class-file members — every jar/JDK method
  indexed from bytecode — have name-less parameters and therefore produce no
  parameter hint, and a call whose overload cannot be pinned down (its argument
  types inconclusive among several same-arity candidates, or an argument count
  matching no overload) gets no parameter hint rather than the wrong names. Conservative as everywhere: an
  unresolved type or callee yields no hint, and only hints whose position lies
  inside the requested range are returned, so a node straddling the range
  boundary cannot leak a hint outside it. No `inlayHint/resolve` and no
  server-push refresh; a hint
  computed while the model is still warming simply appears on the client's next
  request (R6).
- **WorkspaceIndex** (`index/`): a workspace-wide, in-memory index of Java
  declarations and imports, owned by the index subsystem thread and reached only
  through its `IndexHandle`. The subsystem is split by role: `types.rs` holds the
  data, `analyze.rs` reads Java and writes the index, `query.rs` reads it,
  `read.rs` answers the coarse, navigation-shaped reads a consumer makes over the
  hub, and `util.rs` holds the value behaviour and small helpers; the shared,
  stateless Java reading both the index and the `navigate` module use lives in
  `parser.rs`. Entries are flat
  `SymbolEntry`s (name, kind, package, enclosing-type container chain,
  ranges, `dependency` flag) — no trees, no text — so memory stays
  proportional to workspace size. Each entry is allocated once and shared
  (`Arc`) between the per-file and per-name maps, and a file's URI, package, and
  container chain are shared across its entries, so the name lookups hand back
  shared handles rather than clones (`large-project-memory`). A source record's header components are
  indexed too, as method entries at their declared positions with the record as
  their container, so definition, references, rename, and `workspace/symbol`
  can target the accessor, and an enum's constants are indexed as
  `EnumConstant` entries — a member-like kind, not a type — with the enum as
  their container, so a constant resolves like any member. Lombok-generated members are indexed as
  `synthetic` entries anchored at the field they derive from (or the type's own
  name): they resolve `definition` and `references` for a generated member, are
  filtered out of `workspace/symbol` and ordinary completion, and `rename`
  refuses them. The package (from the file's
  `package_declaration`, or the class's internal name for jars) is what
  lets completions auto-import accepted symbols. The shell captures `rootUri` (or the first workspace
  folder) in `initialize` and notifies it as `FolderAdded` in `initialized`;
  the analysis module records it and the project driver walks it. Every
  subsystem is a **driver** spawned at start — the project walker, the dependency
  resolver, the jar indexer, the JDK indexer, and the source
  downloader — and each speaks only the hub's `DriverMessage`s. The workspace
  source scan is no longer a driver of its own: the index (`index/`) owns it,
  walking the root on `FolderAdded` and parsing each `.java` file into the index.
  The index is
  itself a subsystem (`index/`): the hub broadcasts it the
  index-affecting messages and routes it the coarse index reads, broadcasts every
  message to every driver, and renders the warm-up's log lines; the shell renders
  its progress and notices — the request path is never
  involved (R6).
  The base grows append-only, one layer per artifact, so library types resolve
  while the source scan is still running; the warm-up coordinator flips `ready`
  once the jar and JDK stages report done and the index's own source scan
  completes.
  The jar and JDK class-file parses are cached across runs (`base_cache.rs`),
  one file per archive keyed by a schema version and the archive's path, size,
  and mtime, read only when that archive is indexed and written only when it is
  reparsed, so a restart re-parses only what changed; a missing, corrupt, or
  version-mismatched
  cache falls back to a full parse (best-effort, never a source of a wrong or
  partial base).
  Edits to open
  documents re-extract that file's entries from its already parsed tree; on
  `didClose`, a file inside any source root is re-read from disk (disk truth
  wins), anything else drops its entries. A watched-file event does the same
  for a file the editor never opened: a created or changed `.java` file inside
  a source root is re-read and re-indexed from disk, a deleted one loses its
  entries, and a file the editor currently has open is skipped because its
  `didChange` is authoritative (D2). Every such change also refreshes the
  declared-type model and republishes the open documents' diagnostics (see the
  shell-on-the-hub and type-layer bullets). `index_ready()` flips when warm-up
  completes so index-backed features can report themselves briefly
  unavailable during warm-up instead of blocking; completions consume the
  index via `query_prefix` (see the engine-core bullet above); the `navigate`
  module reads exact names through the index's `EntriesByName` read for
  go-to-definition, and `workspace/symbol` is backed by `query_prefix` — with
  dependency-jar entries filtered out of both (see the project-model bullet).
  Known v1 limitation: a watched file's indexed
  ranges reflect its last re-read rather than a live document (the index
  changes only when the client reports an event). A file deleted on disk is
  forgotten from both the index and the model: the watcher drops its entries
  and its per-source type model, so no model-based feature resolves it any more
  (see the type-layer bullet). Extracted dependency
  sources (see the dependency-sources bullet) are indexed as ordinary `.java`
  files under the cache, but their entries carry `library_source`;
  `source_files()` — the candidate set for references and rename — excludes
  them, so a search never reads the cache and a rename never edits it.
- **Navigation is its own hub module** (`navigate.rs`) — the shell routes
  `textDocument/definition`, `textDocument/declaration`,
  `textDocument/implementation`, and `textDocument/references` to the `navigate`
  module as `NavigateRequest::FindDeclaration`/`FindReferences`/
  `FindImplementation` hub requests (declaration reuses the definition request —
  Java has no declaration/definition split).
  Navigation reads the cursor's file and each candidate source through the
  buffers module (`buffers::current_text` — the open buffer, else disk) and
  reaches the index only through the hub, over a
  coarse read vocabulary (`IndexRequest`/`IndexBatchRequest` — entries by name,
  one file's symbols, the workspace files, a type's supertype closure, a type's
  members, whether a type resolves, and the workspace types reaching a contract).
  Resolution is name-based, matching the
  extraction: the cursor resolves
  to the declarations of the word it sits on, narrowed by the node kind (a
  `type_identifier` names a type, a method/field-access name a member), and a name
  spread across files is no answer (no result beats a wrong result) — the one
  exception is a cursor on a declaration's own name, which resolves to that
  declaration; only a use keeps the cross-file ambiguity policy. Type
  references are found soundly — every `Type` occurrence of a type the file can
  see (its own file, its package, or an import). Member references are found where
  the receiver's declared type (or a supertype) resolves the name to the same
  declaring type — `this.member`, a field, a parameter or local (a `var` binding
  through its initializer's inferred type), `new T(…)`, or a
  `Type.staticMember` qualifier — or, for a bare name, inside the declaring type's
  own span. A member receiver may itself be a call or a field access,
  resolved recursively through the shared receiver step, so a chain through calls
  or fields (`a.b().c.member`, `a.field.member`) resolves left to right while every
  link's receiver can be typed. **Documented v1 limitation:** a receiver that
  cannot be typed from its file — a generic type argument, a lambda or
  anonymous-class scope, `super` — is not resolved through, so a chain breaks at
  such a link; and receiver-typed overload selection in definition still awaits the
  full type layer.
- **Completion is its own hub module** (`completion.rs`) — the shell routes
  `textDocument/completion` and `textDocument/signatureHelp` to the `completion`
  module as `CompletionRequest`s (`Complete`, `SignatureHelp`). Completion reads
  the requested file through the buffers module (`buffers::current_text` — the
  open buffer, else disk) and reaches the index only through the hub's
  `EntriesByPrefix` read (the name-ordered symbol table range-scanned for the
  typed prefix). It merges three ranked sources as `sort_text` prefixes
  `0`/`1`/`2` (clients re-sort alphabetically): Java keywords; the locals,
  parameters, and fields nameable unqualified at the cursor (read from the parsed
  tree); and the workspace symbols whose simple name starts with the prefix — a
  method among them listing each overload of its declaring type (resolved from the
  entry's innermost container), over the hub's `MembersOf` read, labelled with its
  full signature and inserting `name(`, so a bare call site offers the same shape
  the member path does, falling back to a single name-only item when the declaring
  type does not resolve rather than dropping the symbol. A
  workspace symbol declared outside the file carries an auto-import edit when the
  file does not already resolve the name (never-worsen: the same file, the same
  package, `java.lang`, an equal or `.*` import, or a conflicting same-package
  simple name all suppress it); each overload item carries the entry's edit. A
  member access after a `.` resolves the
  receiver's declared type through the shared receiver step (`util.rs`) — a
  parameter or local resolving to its declared type (a local written `var` to
  its initializer's inferred type, through the same receiver machinery), a
  `method_invocation` receiver resolving recursively to the callee's declared
  return type and a `field_access` receiver to the accessed field's declared type,
  so a chain through calls or fields (`a.b().c().`, `a.field.c().`) resolves left
  to right while every link's receiver can be typed — and offers that
  type's members over the hub's `MembersOf` read (now overload-preserving) —
  inherited members included, fields and enum constants first, then one item per
  method overload (`label` the full signature, `insert_text` `name(`), a type
  qualifier offering only its statics, and a receiver it cannot type — a nested-type
  or package qualifier, `super`, a generic type argument, a lambda or
  anonymous-class scope, or a `var` initializer that resolves nothing — yielding
  nothing rather than a guess. Both completion
  paths therefore list a method as one item per overload.
  **Signature help** serves `textDocument/signatureHelp` from the same receiver
  step: for the call the cursor sits in it resolves the callee's receiver through
  `util.rs` and reads the receiver type's same-named methods over the hub's
  `MembersOf` (statics only for a type qualifier), rendering each overload's
  `signature()` and marking the argument the cursor is in as `activeParameter`,
  returning nothing when the callee or its receiver cannot be resolved. A
  `new T(...)` is answered from the created type's constructors over
  `ConstructorsOf`, its overloads listed with `constructor_signature()` and the
  active argument marked the same way. The shell advertises
  `signatureHelpProvider` with `(` and `,` as the retrigger characters.
- **Workspace symbol search is its own hub module** (`workspace.rs`) — the shell
  advertises `workspaceSymbolProvider` and routes `workspace/symbol` to it as a
  `SymbolRequest::Search`. It reads the index only through the hub's
  `EntriesByPrefix` read (an empty query matches every symbol) and shapes each
  entry as a `SymbolInformation` — the entry's `selection_range` as the location,
  its innermost enclosing type as `container_name`, its kind mapped to the LSP
  kind. A `Dependency` class-file entry (nothing openable), an import (a path,
  not a declaration), a constructor, and a Lombok-synthetic member (no declaration
  of its own) are dropped.
- **Buffers is its own hub module** (`buffers.rs`) — the editor's unsaved text is
  the one source of truth, so a consumer that needs a file's current text reads it
  here rather than from disk. The shell notifies `DocumentOpened` (full text),
  `DocumentChanged` (the incremental changes), and `DocumentClosed`; the module's
  `DocumentStore` applies them and answers `BuffersRequest::Text` (`None` when the
  document is not open). `buffers::current_text` is the read a consumer calls —
  the open buffer's text when the editor has it open, otherwise the file on disk —
  so completion and navigation see unsaved edits and untitled buffers. The
  module's `select!` is biased, so a queued document event is always applied
  before a read is served; the two arrive on separate channels, and random
  polling could otherwise serve a read against pre-edit text.
- **Maven module** (`maven/`): on the shell's `ServerStarted` the module
  builds the project model — it walks the root (hidden dirs, `target/`, `build/`
  skipped), and every `pom.xml` becomes a module whose source roots are the
  standard `src/main/java`/`src/test/java` unless `<build>` overrides them;
  roots that don't exist are dropped, and a workspace with no poms falls back to
  scanning the whole root (non-Maven projects keep working) — and publishes
  those roots as a `FoldersDiscoverred` batch with the `Workspace` origin, which
  the index scans as `FileOrigin::Workspace` sources. It then resolves each
  module's dependency closure and, unless disabled, fetches and extracts the
  dependency sources, publishing the extracted roots as a second batch with the
  `LibrarySources` origin, which the index scans as `FileOrigin::LibrarySource`
  — completable and openable (`definition` reaches a library declaration) but
  never a references/rename candidate (`source_files()` omits them; rename
  refuses).
  **Dependency resolution** (`resolve.rs`) is static and strictly offline —
  it reads the local repository (`$MAVEN_REPO` if set, else the settings
  `localRepository`, else `~/.m2/repository`) and never invokes `mvn` or the
  network. Each pom is assembled into an effective model (parent chain via
  `relativePath` or the repository, `${...}` interpolation with the
  `project.*` built-ins, parent-first merge; unknown properties interpolate to
  nothing and the artifact is pruned with a warning). `dependencyManagement`
  merges down the parent chain (child wins) and expands `import`-scoped BOMs
  from the repository (direct management beats imported, first imported wins
  among BOMs). The dependency closure is a breadth-first walk over each
  artifact's effective pom with Maven's mediation — nearest depth wins, first
  declaration wins at equal depth (implemented as first-encountered-in-BFS) —
  path-scoped exclusions, and optional/test transitive pruning; a missing pom
  prunes its branch with a warning while the rest of the closure continues.
  Known v1 limitations: no profile activation, no plugin-contributed roots or
  dependencies, no transitive version-range handling, `-SNAPSHOT` metadata is
  ignored (the local file is used as-is), and dependency scopes are ignored at
  indexing time (test-scope types may be offered in main sources). The bench's
  `--maven` mode measures open-to-responsive on a Maven-layout fixture.
- **Standard library** (`jdk.rs`): the installed JDK's `java.*`/`javax.*`
  declarations are indexed at start by the JDK driver — concurrently with the
  rest, through the same dependency path — offered in
  completions with auto-import edits, filtered out of definition and
  `workspace/symbol`.
  Discovery order: `$JAVA_LSP_JDK` (an explicit override pointing at an
  unusable home disables JDK indexing entirely — deterministic opt-out),
  `$JAVA_HOME`, then common locations including SDKMAN's
  `~/.sdkman/candidates/java/current`. Three archive layouts are supported:
  `jmods/*.jmod` class files (JDK 9+, ZIPs with a magic prefix — the
  tail-scanning reader handles that), `lib/src.zip` sources (tree-sitter
  parsed; some distributions ship sources without jmods), and `rt.jar`
  (JDK 8). Only class/source entries under `java.*`/`javax.*` are indexed —
  `jdk.*`, `com.sun.*`, `sun.*`, `module-info`, and `package-info` stay out —
  and archives are read with a streaming per-entry callback so multi-
  hundred-megabyte content never materializes at once. `java.lang.*` symbols
  never carry an import edit (implicitly imported in Java). No JDK found is
  a graceful no-op. Measured impact (Temurin 25, src.zip, ~4.2k files):
  warm-up ≈ 5.5 s and peak RSS ≈ 165 MB in release — both on the background
  task; hover RTT during warm-up stayed ≤ 1.6 ms (R6 holds; see the bench
  baseline in the changelog).
- **Dependency sources** (`maven/sources.rs`): for each resolved artifact a
  `-sources.jar` already in the local repository is reused; otherwise it is
  downloaded into the repository at Maven's standard path, with the published
  `.sha1` verified when present. The base URL is `$JAVA_LSP_MAVEN_CENTRAL_URL`
  (default `https://repo1.maven.org/maven2`) unless a `~/.m2/settings.xml`
  mirror matches (`central`/`*`/`external:*`), which replaces it; the env var
  overrides the mirror. Downloads run on the runtime with bounded concurrency
  (a semaphore caps at 8); extraction runs on the blocking pool, so the
  request path is never involved (R6). Each sources jar is unpacked under
  `$JAVA_LSP_SOURCES_CACHE` (default `$XDG_CACHE_HOME/java-lsp/sources`, else
  `~/.cache/java-lsp/sources`), one directory per artifact
  (`<cache>/<group>/<artifact>/<version>`), and the module publishes those
  directories as the `LibrarySources` `FoldersDiscoverred` batch — the index
  parses the `.java` files itself through the same tree-sitter path as the
  workspace sources, so hover, `.`-completion, and inlay hints gain real
  signatures and parameter names, and `definition` resolves a library
  declaration to its extracted source — an ordinary openable `file://` URI.
  `references` and `rename` are unchanged for libraries: `source_files()` omits
  the cache, so neither reads nor edits it. On by default; `--no-downloads` or
  `$JAVA_LSP_OFFLINE` (any non-empty value) disables the network work, and
  `--no-dependencies` skips resolution and sourcing entirely. An unreachable
  repository, an artifact with no published sources, or a checksum mismatch
  each degrades to nothing for that artifact with a warning.

## Decisions and constraints

- **`tower-lsp` over stdio** — ergonomics first (decision recorded in
  `requirements.md`); revisit if it blocks cancellation or backpressure control.
- **The shell is just another hub client** (`server.rs`, `engine.rs`) — the
  shell notifies the editor's input and requests the editor's queries on the
  same hub every module uses, and consumes the editor-facing notifications on
  its own channel; there is no separate command/event boundary and no
  dispatcher. The core is the **analysis module**, which applies mutations
  inline for ordering while queries run concurrently on the blocking pool
  (preserving R6); a strict single-task actor was rejected because it would
  serialize every request behind the slowest one (`message-based-engine`). This
  supersedes the earlier `Command`/`EngineEvent` boundary and the
  `EngineHandle` dispatcher (`server-as-hub-client`).
- **Every subsystem is a module or driver on one hub** (`messages.rs`,
  `hub.rs`, `engine.rs`) — all messages live in `messages.rs`: the notification
  set and the `Request`/`Response` pairs. Each participant is a module that owns
  its whole hub life — its name, its subscription and served requests, and its
  own thread or task — so `engine::start_with` only builds the hub and calls each
  enabled `spawn` (`engine::start` runs everything the environment enables). The
  launch
  configuration (`cli.rs`) decides which participants to build and which stages
  the startup coordinator waits on, so the drivers themselves take no
  configuration. The warm-up drivers live with the work they drive
  (`maven/`, `jars.rs`, `jdk.rs`, `sources.rs`), and each reacts
  to the messages it cares about: the startup coordinator (`startup.rs`) walks
  for the model on `FolderAdded` and owns `ready` and the summary;
  the dependency driver resolves the jars; the jar indexer, JDK
  indexer, and source downloader produce the index data, and the workspace source
  scan is the index's own (`index/`) rather than a driver. The hub starts empty:
  every participant registers its client (`Hub::register`) before its thread or
  task starts, and the registration is in place before any later message — the
  client's inbound channel is installed as `register` returns — so a later
  notification, request, or reply can never miss it. The hub broadcasts every
  notification and routes every request to its owner (the navigate, index,
  analysis, diagnostics, or quick-fix module), carrying no editor knowledge; the
  shell renders the editor-facing notifications. A request with no registered
  owner resolves to the empty reply rather than waiting. The hub logs a compact
  summary of every message it forwards (`RUST_LOG=java_lsp::hub=info`), so the
  whole flow is attributable from one place.
  This supersedes the earlier "one driver that discovers, then spawns producers"
  shape and the `Reporter` indirection it threaded through the producers
  (`incremental-indexing-pipeline`).
- **The index is its own subsystem** (`index/`) — the symbol index is
  owned by a dedicated subsystem thread and reached only through `IndexHandle`, a
  cheap, message-based handle: each call is one message to the subsystem (and a
  reply for a query), so no component holds the index state directly and the
  subsystem is its single reader and writer. It also owns its own workspace
  source scan — it walks the root on `FolderAdded` and parses each `.java` file
  into the index, so the scan is not a separate driver. It no longer serves
  navigation: the `navigate` module reads it over the hub through a coarse,
  batched read vocabulary (`IndexRequest`/`IndexBatchRequest`). The hub hands it
  the index-affecting
  `DriverMessage`s; the analysis core uses the handle for its symbol lookups and
  edits. A query is an ordinary hub request: the hub routes it to the index
  subsystem and that subsystem's reply is routed back through the hub (which logs
  it), so the hub never calls the index synchronously and cannot deadlock on
  it.
- **The diagnostics engine is its own subsystem** (`diagnostics.rs`) — it owns a
  parser and the open documents' text and computes the pass itself, reading the
  index and declared-type layer from the index subsystem only through
  `IndexHandle` and reporting to the hub as messages. The hub broadcasts the
  document events to it, and its sweep follows the analysis module's
  `AnalysisUpdated`; the shell publishes its reports; a retired
  `DiagnosticsPublisher` no longer reaches into the core. It keeps a queryable
  cache of each open document's latest pass.
- **The quick-fix subsystem is its own subsystem** (`quickfix.rs`) — it generates
  the create/import/rename fixes from its own parse of the buffer, querying the
  symbol index (`IndexHandle`) and the diagnostics cache (`DiagnosticsForDocument`).
  The shell's `codeAction` reaches it as an awaited hub request, so a fix never
  blocks a runtime worker.
- **The index is ordered by name for prefix queries** — `by_name` is a
  `BTreeMap`, so `query_prefix` (completions on every keystroke, and
  `workspace/symbol`) is a range scan over the matching names rather than a scan
  of every name; the key set is unchanged, so the index does not grow.
- **Analysis threads are built with an explicit stack size** — analysis is
  deeply recursive (the warm-up source scan, the open-document parse/extract on
  the analysis module's thread, and the diagnostics sweep all walk the AST and the type model
  recursively), and on a large workspace that overflows the standard library's
  default 2 MiB per thread, aborting the process with `has overflowed its stack`.
  `main.rs` builds the runtime with `Builder::thread_stack_size`, which tokio
  applies to worker threads and the blocking pool, and the analysis module's
  thread is spawned with the same `RUNTIME_STACK_SIZE` (`lib.rs`), so the server is safe by
  default without depending on the `RUST_MIN_STACK` environment variable the
  editor extension does not set (`runtime-stack-overflow`).
- **Process lifecycle**: `shutdown`/`exit` stop the service (further requests
  are rejected with `ExitedError`); the process itself terminates when the
  client closes stdin (EOF), which is tower-lsp's transport semantics and what
  every editor does after `exit`.
- **Method params are validated**: sending a `params` field for methods that
  take none (`shutdown`, `exit`) is rejected with -32602 — clients and tests
  must omit the field entirely.
- **Logging goes to stderr.** stdout is the LSP transport; any stray stdout
  write corrupts the protocol. `tracing-subscriber` writes to
  `std::io::stderr`, filtered by `RUST_LOG` (default `java_lsp=info`).
- **`rename` returns a plain `changes` edit.** The shell tracks versions only
  for open documents, so a versioned `documentChanges` edit covering unopened
  files would claim a guarantee the server cannot make; a rename applies to the
  on-disk content the scan read. Edits to open documents are still subject to
  the client's own version checks.
- **Out-of-range change positions** are skipped (with a warning) rather than
  failing the whole `didChange` batch; the document keeps its last good text.
- **Test harnesses must drain the client socket**: tower-lsp's client channel
  has capacity 1, so a test that triggers more than a couple of
  `publishDiagnostics` notifications without reading the `ClientSocket`
  blocks the server's handlers (seen with the workspace-index integration
  test; it now drains the socket in a spawned task).

## Zed integration

`zed-java-lsp/` is a self-contained Zed extension (`zed_extension_api` 0.7,
compiled to `wasm32-wasip2`): it registers the `tree-sitter-java` grammar and
the Java language (config + highlighting/indent/fold/outline/bracket/textobject
queries, vendored from `zed-extensions/java`) and starts `java-lsp` as the
language server — no JDK, no jdtls. It downloads nothing:
`language_server_command` resolves the binary in order — user override
(`lsp."java-lsp".binary.path`), `$PATH` lookup, or the repository's own
`target/release` (then `target/debug`) build — testing host paths through
`sh -c '[ -f ... ]'` because the WASM sandbox cannot stat them directly — and
errors with build/install instructions otherwise. Install it as a dev
extension via `zed: install dev extension`; see `zed-java-lsp/README.md`.

## Verifying without an editor

Two harnesses drive the server with raw JSON-RPC, both plain `cargo test`:

- `tests/harness.rs` feeds requests through `tower_lsp::LspService` and asserts
  on responses and document-store state.
- `tests/stdio_smoke.rs` spawns the built binary (via
  `CARGO_BIN_EXE_java-lsp`) and drives the real stdio transport through
  initialize → didOpen → didChange → hover → shutdown → exit. Note: methods
  without params in the LSP spec (`shutdown`, `exit`) must be sent without a
  `params` field — tower-lsp rejects `{}` with -32602.
- `src/bin/java-lsp-bench.rs` is the performance harness (see
  `docs/dev/backlog/perf-benchmarks.md`): one command —
  `cargo run --release --bin java-lsp-bench -- --files 500
--methods-per-class 10` — generates a fixture Java workspace in a temp dir,
  spawns the real `java-lsp` binary (resolved from `--server`, `$JAVA_LSP_BIN`,
  then `target/{release,debug}/java-lsp`, with an auto `cargo build --bin
java-lsp` when run under cargo), and drives it over raw stdio JSON-RPC. It
  measures per-feature first-response time after `didOpen`, hover RTT sampled
  on a 25 ms tick during index warm-up (detected via the server's own
  "workspace index warm-up complete" log line on stderr), and peak RSS from
  `/proc/<pid>/status` `VmHWM`, then prints a text table (or `--json`) and
  cleans up (unless `--keep`). Other flags: `--fields-per-class F`. Baselines
  are recorded per milestone in `docs/dev/changelog.md`.
