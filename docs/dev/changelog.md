---
type: Changelog
title: Changelog
description: Shipped changes, newest first.
tags: [dev, changelog]
status: draft
---

Entries that say "Verified by N tests" quote the whole suite's size at that
point (the cumulative `cargo test` total), not the number of tests covering the
named change.

## 2026-10-03

- **The engine bus is the engine hub** — `bus.rs` is `hub.rs`, `BusClient` is
  `HubClient`, the received-message enum `Bus` is `Hub`, and the `hub` module,
  `crate::hub`/`java_lsp::hub` paths, the `hub()` accessor and the
  `RUST_LOG=java_lsp::hub=debug` log target all follow; no `bus` name remains.

- **The document store is a hub client** — `DocumentStore` moves out of the shell
  into the document module (`document.rs`, serving `Module::Document`): it
  subscribes to `DocumentOpened`/`DocumentClosed`, applies the editor's
  incremental edits on a `DocumentChange` request (returning the new text), and
  answers a `DocumentText` request. The shell holds no document state;
  `didChange` awaits the module before notifying, so an edit still lands before
  the query the client sends at the new cursor. Verified by 299 library tests and
  29 harness tests (the loopback-socket `sources` cases and one harness case
  cannot bind in the sandbox).

## 2026-10-02

- **Every module owns its own hub subscription** — the six warm-up drivers
  (`project`, `dependency`, `source`, `jar`, `jdk`, `download`) are now their own
  modules (`project.rs`, `resolve.rs`, `scan.rs`, `jars.rs`, `jdk.rs`,
  `sources.rs`), each `spawn` labels itself, subscribes, and starts its own
  thread or task, and `engine.rs::start` is pure wiring (no labels, no
  `subscribe`/`serve`, no driver bodies). Verified by 299 library tests, 29
  harness tests, and the stdio smoke test. See
  [Every module handles its own hub subscription](backlog/module-owned-hub-subscriptions.md).

## 2026-10-01

- **Every participant subscribes through its HubClient** — the hub starts
  empty (`hub::spawn_hub`) and learns its participants at runtime:
  `HubClient::subscribe()` for every notification, `serve(Module)` to also own
  a module's requests. Modules and drivers take only their client; the drivers
  now receive `Hub` like the modules. `engine::start` is the one setup path,
  building the hub and every participant, the server included. A second owner
  is logged and ignored, and dropped receivers are pruned. `spawn_router`,
  `hub::channel` and the hand-wired channels are gone. Verified by 299 library
  tests, 29 harness tests, and the stdio smoke test. See
  [Every participant subscribes to the hub through its HubClient](backlog/summary.md#every-participant-subscribes-to-the-hub-through-its-hubclient).

- **Hub requests return an awaitable reply** — every `HubClient` request now
  returns a `Reply<R>` (a thin wrapper over a tokio oneshot receiver): the shell
  and tokio tests `.await` it, the synchronous analysis, diagnostics and
  quick-fix code calls `blocking_recv()`. The closure-built requests, the boxed
  reply deliverer, the `std`-channel blocking path and the `_async` method twins
  are gone. Verified by 297 library tests, 29 harness tests, and the stdio smoke
  test. See [Hub requests return an awaitable reply receiver](backlog/summary.md#hub-requests-return-an-awaitable-reply-receiver).

- **The LSP shell is just another hub client** — `JavaLanguageServer` holds a
  `HubClient`: it notifies `DocumentOpened/Changed/Closed`, `FolderAdded`,
  `FileEvent`, and `ClientCapabilities`, awaits `Request::Analysis(…)` queries
  answered by the new analysis module (`analysis::spawn_module`), and renders
  `Diagnostics`/`Progress`/`Notice` from its own hub channel; `Command`,
  `EngineHandle`, `EngineEvent`, the dispatcher, and `messages::translate` are
  gone, the diagnostics sweep runs on the new `AnalysisUpdated`, and the close
  clear comes from the diagnostics module. Verified by 297 library tests, 29
  harness tests, and the stdio smoke test. See
  [Make the LSP shell just another hub client](backlog/summary.md#make-the-lsp-shell-just-another-hub-client).

- **Go-to-declaration** — `initialize` now advertises `declarationProvider` and
  `textDocument/declaration` is answered by the go-to-definition resolution
  (Java has no declaration/definition split), so the editor's "Go to
  Declaration" command no longer does nothing. Verified by the harness
  capability and definition tests.

- **Diagnostics sweeps read the index in one batch** — `src/diagnostics.rs` reads
  the index through a per-sweep `SweepIndex`: `type_layers`/`type_model`/`ready`
  once per sweep, and every name and wildcard package the open documents' passes
  look up prefetched via the new `IndexQueryNames`/`IndexHasPackages` requests
  (logged as `count=N`) into a cache, with a single cached `IndexQueryName` as
  the fallback; `import_edit` takes `&dyn NameLookup`. See
  [Batch and cache the diagnostics pass's index queries](backlog/summary.md#batch-and-cache-the-diagnostics-passs-index-queries).

- **Jar and JDK members share one container `Arc` per type** — `src/classfile.rs`
  `class_entries` built `Arc::from(member_container.clone())` for every member,
  re-allocating the container vector and `Arc` per member; the jar and JDK paths
  both route through it, so over ~6M members this was a large, repeated
  allocation. One `Arc<[String]>` is now built per type and shared. See
  [Shrink the indexed entry and type model](backlog/index-entry-size.md).
- **Library sources no longer index their import declarations** — `index.rs`
  gains `drop_import_entries`, called by the dependency-source (`sources.rs`) and
  JDK (`jdk.rs`) passes, so the two library passes store no `Import` entries. No
  feature reads them (`definition` filters them out, `completion_kind`/
  `symbol_kind` answer `None`, `ambiguous_names` skips them, and `references`
  parses workspace files) — they were ~2.9M of ~0.39 GB over the real corpus.
  Workspace files keep their imports. See
  [Stop storing import declarations for library sources](backlog/summary.md#stop-storing-import-declarations-for-library-sources).
- **End-to-end feature test over the `example/` workspace** — `tests/example_features.rs`
  drives the real binary over stdio and asserts `definition`, `documentSymbol`,
  `hover`, `.`-completion, `references`, and `workspace/symbol` against the
  two-module example (~1 s, no JDK, offline). See
  [Shrink the indexed entry and type model](backlog/index-entry-size.md).
- **The warm-up logs what the index holds** — `index.rs` logs an
  `index composition:` line (entries by kind, names, files, approximate bytes;
  base layers/types/members; source models) at `Ready` and after the downloads
  stage, so its size can be targeted rather than guessed. See
  [Log what the index holds, so its size can be targeted](backlog/summary.md#log-what-the-index-holds-so-its-size-can-be-targeted).
- **The class-file base cache is one file per archive, read on demand** —
  `base_cache::ArchiveStore` replaces the whole-kind `jars.json`/`jdk.json` (the
  jars file had reached 3.7 GB and was loaded and re-written whole each run) with
  one guarded file per archive, read only when that archive is needed and written
  only when it is reparsed; the legacy file is dropped on first use. See
  [Cache each archive's parse in its own file, read on demand](backlog/summary.md#cache-each-archives-parse-in-its-own-file-read-on-demand).
- **The hub debug log no longer floods with per-item lines** — `src/hub.rs` logs
  the high-cardinality per-item notifications (`SourceFile`, `BaseArtifact`,
  `Progress`, …) at `trace` and the rest at `debug`, and the extraction progress
  is emitted ~5 % of the way instead of per archive. See
  [Keep the hub debug log readable and emit progress in steps](backlog/summary.md#keep-the-hub-debug-log-readable-and-emit-progress-in-steps).
- **Dependency sources already extracted are not rewritten** — `src/sources.rs`'s
  `index_one` now creates each package directory once per artifact and writes a
  `.java` only when it is missing or the wrong length, so a warm cache does no
  write work (measured: the `write` phase 14294 ms → 42 ms; ~15 % wall clock, as
  the writes overlapped with the parse). See
  [Do not rewrite dependency sources already extracted to the cache](backlog/summary.md#do-not-rewrite-dependency-sources-already-extracted-to-the-cache).
- **The dependency-source pass logs per-phase timings** — `src/sources.rs` logs
  the fetch (`dependency source fetch: N of M archives available in Xms`) and the
  per-phase extract totals (`read/inflate/write/parse/entries/types`), every 200
  archives and at the end, so the bottleneck is measured on the real corpus.
  Measured over 40 real jars: parse 32 %, cache write 23 %, types 18 %, entries
  17 %, inflate 5 %, read 5 %. See
  [Log per-phase timings for the dependency-source pass](backlog/summary.md#log-per-phase-timings-for-the-dependency-source-pass).

## 2026-09-30

- **Indexing is no longer quadratic in file size** — byte offsets now convert to
  LSP positions through a per-file `LineIndex` (`analysis.rs`) built once and
  binary-searched, instead of `lsp_position` rescanning the text per position.
  The dependency-source pass on a 30-jar sample fell from 373.6 s to 18.7 s. See
  [Make byte-offset to LSP position conversion linear, not quadratic, per file](backlog/summary.md#make-byte-offset-to-lsp-position-conversion-linear-not-quadratic-per-file).
- **Dependency sources are extracted and parsed across worker threads** —
  `sources.rs::index_extracted` fans the artifacts across `available_parallelism`
  workers (one parser each), the calling thread emitting. See
  [Extract and parse dependency sources across worker threads](backlog/summary.md#extract-and-parse-dependency-sources-across-worker-threads).
- **Dependency-source extraction reports per-archive progress** — `src/sources.rs`'s
  `index_extracted` emits a `Progress` update per archive,
  `Parsed x/N dependency source archives (F source files)`, so the extraction
  phase is legible instead of looking stuck between `Parsing N …` and
  `Indexed M …`. See
  [Report progress while dependency source archives are extracted and indexed](backlog/summary.md#report-progress-while-dependency-source-archives-are-extracted-and-indexed)
  and its [follow-up](backlog/summary.md#drive-the-dependency-source-extraction-progress-by-counts-with-the-parsed-source-file-count).
- **Dependency sources are one base artifact per jar** — `src/sources.rs`'s
  `index_extracted` now publishes a single `BaseArtifact` for a whole sources jar
  (keyed by the sources-jar URI) instead of one per extracted `.java` file, so the
  hub, the hub log, and the index base no longer grow per file. See
  [Publish each dependency's sources as one base artifact, not one per file](backlog/summary.md#publish-each-dependencys-sources-as-one-base-artifact-not-one-per-file).
- **The hub no longer logs a reply for an unanswered request** — the reply line in
  `src/hub.rs` is emitted only for a real reply (`deliver.is_some()`), not for a
  dropped request, and `docs/architecture.md`'s index bullet now describes the
  request/reply round-trip accurately. See
  [Stop the hub logging a reply for an unanswered request](backlog/summary.md#stop-the-hub-logging-a-reply-for-an-unanswered-request).
- **The hub log names the sender and times replies** — each `debug` line from
  the hub (`src/hub.rs`) is prefixed `sender=<label>` (the core, the dispatcher,
  the six drivers, the diagnostics, and the quick fixes each name themselves),
  and a request's reply is routed back through the hub and logged as
  `sender=<owner> reply to=<requester> … elapsed=<ms>`. See
  [Identify the sender, replies, and reply latency in the hub log](backlog/summary.md#identify-the-sender-replies-and-reply-latency-in-the-hub-log).
- **The hub logs every hub message** — `src/hub.rs` logs each notification and
  request passing through the hub at `debug` (`RUST_LOG=java_lsp::hub=debug`),
  with a concise description (identifiers and counts, never a payload). See
  [One engine hub with broadcast notifications and hub-routed request/response](backlog/summary.md#one-engine-hub-with-broadcast-notifications-and-hub-routed-requestresponse).

## 2026-09-29

- **One engine hub** — every module (the drivers, the index, the diagnostics, the
  quick fixes, and the core) now speaks a single hub (`src/hub.rs`): notifications
  are broadcast to every module, and a request is routed by the hub thread to the
  module that owns it. The per-module `IndexHandle`, `DiagnosticsHandle`,
  `QuickFixHandle`, and the filesystem driver are gone. See
  [One engine hub with broadcast notifications and hub-routed request/response](backlog/summary.md#one-engine-hub-with-broadcast-notifications-and-hub-routed-requestresponse).
- **A quick-fix subsystem, a queryable diagnostics cache, and a prefix-ordered
  index** — `src/quickfix.rs` now generates the create/import/rename fixes from
  its own parse, querying the symbol index and a new diagnostics cache
  (`DiagnosticsHandle::diagnostics`); the index's `by_name` is a `BTreeMap`, so
  `query_prefix` (completions, `workspace/symbol`) is a range scan. See
  [A quick-fix subsystem, a queryable diagnostics cache, and a prefix-ordered index](backlog/summary.md#a-quick-fix-subsystem-a-queryable-diagnostics-cache-and-a-prefix-ordered-index).
- **The diagnostics engine is its own subsystem** — `src/diagnostics.rs` owns a
  parser and the open buffers' text, computes the syntax and unresolved-symbol
  pass itself, and reports it to the hub; the engine's `DiagnosticsPublisher` is
  gone, and the declared-type overlay moved into the index subsystem. See
  [Make the index and the diagnostics engine their own subsystems](backlog/summary.md#make-the-index-and-the-diagnostics-engine-their-own-subsystems).
- **Cache the class-file base across warm-ups** — the dependency-jar and JDK
  class-file parses are now cached under the sources cache dir (`base/jars.json`,
  `base/jdk.json`), keyed by a schema version and each archive's path, size, and
  mtime, so a restart re-parses only what changed; a missing, corrupt, or
  version-mismatched cache falls back to a full parse. See
  [Cache the class-file base across warm-ups](backlog/summary.md#cache-the-class-file-base-across-warm-ups).
- **The index is its own subsystem** — `src/index.rs` now owns the
  `WorkspaceIndex` on a dedicated thread behind a message-based `IndexHandle`;
  the analysis core and the engine hub reach it only through messages, and the
  hub no longer mutates the index itself. See
  [Make the index and the diagnostics engine their own subsystems](backlog/summary.md#make-the-index-and-the-diagnostics-engine-their-own-subsystems).
- **Size the runtime's thread stacks** — `src/main.rs` now builds the tokio
  runtime with `thread_stack_size(256 MiB)`, applied to both worker and
  blocking-pool threads, so a deeply recursive analysis pass on a large workspace
  no longer overflows the default 2 MiB stack and aborts the server. See
  [Deep analysis recursion overflows the engine runtime's default thread stack on large workspaces](backlog/summary.md#deep-analysis-recursion-overflows-the-engine-runtimes-default-thread-stack-on-large-workspaces).

## 2026-09-28

- **Complete member access through dotted receivers** — a member access after `.`
  now recovers the receiver at the dot before the typed prefix, so `.`-completion
  works for a partially typed name (`SumType.T…`, `new SumType.T…`), for a dotted
  nested-type receiver (`Greeter.Inner.`, `Greeter.Inner.CONST`), and for a package
  qualifier (`java.util.Li` → `List`); `all_types` also keys a type by its name so
  no same-kind sibling is dropped, and `src/messages.rs` compiles again. See
  [Member completion after a dot is empty for dotted nested-type receivers and partial type names](backlog/summary.md#member-completion-after-a-dot-is-empty-for-dotted-nested-type-receivers-and-partial-type-names).

- **Route every subsystem through one engine-owned message hub** — all messages
  now live in `src/messages.rs`, the `Reporter` indirection is gone, and every
  subsystem (filesystem, project walk, dependency resolution, source scan, jar and
  JDK indexing, source download) is a driver spawned at start that speaks only
  `DriverMessage`; the engine hub applies the index-affecting ones and is the sole
  emitter of the warm-up's client events, so discovery is message-based and
  nothing else mutates the index or talks to the editor. See
  [Make every subsystem a message-driven driver on one engine-owned hub](backlog/summary.md#make-every-subsystem-a-message-driven-driver-on-one-engine-owned-hub).

- **Model a source enum's members** — an enum's constants and the fields,
  methods, and constructors in its `;`-introduced declaration section are now
  indexed (kind `EnumConstant`) and modelled, so `DataType.` offers its
  constants as `enumMember`, `DataType.TYPE_1` no longer reports "cannot
  resolve", and definition/references/rename target a constant. See
  [Source enum members (constants and declared members) are not modelled](backlog/summary.md#source-enum-members-constants-and-declared-members-are-not-modelled-so-completion-navigation-and-member-resolution-miss-them).

- **Resolve dotted nested-type references** — `Outer.Inner` is resolved
  nested-first (the prefix as a type, then its nested type) instead of being read
  as package `Outer` plus `Inner`, and `.`-completion on a type receiver lists
  its nested types, so `new Greeter.Inner()` type-resolves and `i.` completes
  its members. See
  [Dotted nested-type references are misread as package-qualified names](backlog/summary.md#dotted-nested-type-references-outerinner-are-misread-as-package-qualified-names).

- **Make indexing an incremental, message-based pipeline** — indexing is now a
  driver that spawns independent producers (project discovery, the workspace
  source scan, the dependency jars, the JDK, and the dependency-source
  downloader) which emit `IndexMessage`s to one indexing task; the declared-type
  base grows append-only per artifact (the downloader's whole-model clone is
  gone), the downloader runs concurrently with the source scan, and the warm-up's
  progress, notices, and log lines all flow through that boundary. See
  [Make indexing an incremental, message-based pipeline of producer modules](backlog/summary.md#make-indexing-an-incremental-message-based-pipeline-of-producer-modules).

- **Index the JDK from jmods** — a jmod is a ZIP prefixed by a 4-byte `JM`
  magic whose central-directory offsets are relative to the byte after it; the
  reader treated them as absolute, so every jmod yielded zero entries and the
  standard library was never indexed (`jdk_classes=0` on all JDK 9+ installs) —
  no `java.*`/`javax.*` types or members completed, hovered, or navigated.
  `jdk::class_archive_entries` now strips the prefix (Temurin 21.0.8 indexes
  73310 entries), and the jmod test fixture carries the magic. See
  [Index the JDK from jmods so standard-library members complete](backlog/summary.md#index-the-jdk-from-jmods-so-standard-library-members-complete).

- **Publish the type base before the source scan** — `scan_workspace_core` now
  indexes dependency jars and the JDK (and publishes the base type model) before
  the workspace source scan, so library types resolve while a large tree is
  still being indexed (previously the base was published only at the end, so
  `StringUtils.`/`Math.` completed nothing for the whole scan). Semantic
  diagnostics stay gated on `ready`, so a not-yet-scanned workspace type is
  never flagged unresolved. Interim step toward
  [Make indexing an incremental, message-based pipeline of producer modules](backlog/summary.md#make-indexing-an-incremental-message-based-pipeline-of-producer-modules).

- **Responsive request path on large workspaces** — diagnostics now run on a
  coalescing publisher task instead of inline on the engine dispatcher, the
  document store is snapshotted so no handler holds its lock across a semantic
  pass, a references/rename search parses with a private parser from a pool
  instead of holding the shared parse mutex, and the declared-type base is read
  through a cached, name-indexed layer view rather than rebuilt per request.
  Verified by `cargo test --all-targets` (255 lib, 28 harness, and 1 stdio test
  passing; the only failures are 4 `sources` lib tests and 1 harness test that
  need a loopback socket this sandbox forbids). On `java-lsp-bench --files 10000
--methods-per-class 10 --open-docs 20 --edits 10 --references` (20 open
  documents, JDK indexed): edit → publish 12.4 ms, edit → hover 4.3 ms, edit →
  definition 4.3 ms, and `references` on a member every file uses 3.2 s for
  10001 locations (bounded and off the typing path). See
  [Keep the request path responsive while diagnostics and references run on a large workspace](backlog/summary.md#keep-the-request-path-responsive-while-diagnostics-and-references-run-on-a-large-workspace).

## 2026-09-25

- **Lower memory and per-request cost on large projects** — the declared-type
  layer is now read through a lazy `ModelLayers` view that borrows the per-file
  `Arc<TypeModel>`s instead of deep-copying the whole workspace union on every
  request (and once per candidate file in references/rename, now built once per
  search), and index entries are allocated once and shared between the name and
  file maps, with a file's URI, package, and container chain shared across its
  entries. On the 500-file fixture (`java-lsp-bench --files 500
--methods-per-class 10`) peak RSS fell from 51504 KiB to 43648 KiB and
  post-warm-up hover RTT from 0.730 ms to 0.163 ms; the bench now reports memory
  on macOS via `getrusage` `ru_maxrss`. Verified by `cargo test --all-targets`
  (278 tests). See
  [Cut per-request type-model churn and index duplication on large projects](backlog/summary.md#cut-per-request-type-model-churn-and-index-duplication-on-large-projects).

- **Go-to-implementation** — `textDocument/implementation` now answers a cursor
  on a type with the workspace types whose supertype closure reaches it
  (sub-interfaces and abstract intermediates included) and a cursor on a member
  with the workspace subtypes that override it, matched by name and parameter
  types; a library/JDK type may be the contract, while only workspace
  declarations are returned. Verified by `cargo test --all-targets` (274 tests).
  See
  [Go to implementation for types and members](backlog/summary.md#go-to-implementation-for-types-and-members).

- **Lombok annotation support** — a source type carrying Lombok annotations now
  exposes the members Lombok would generate, synthesized statically (no
  annotation processor, no `lombok.config`): `@Getter`/`@Setter`/`@With`,
  `@Data`/`@Value`, `@Accessors` (fluent/chain), `@Builder` (a nested
  `TBuilder` with `builder()`/`toBuilder()`), the log-field family, and the
  constructor annotations. Generated members reach `.`-completion, hover,
  signature help, inlay hints, and stop the unresolved-member diagnostic from
  crying wolf; they are indexed as `synthetic` entries anchored at the field
  they derive from, so `definition` and `references` reach it while
  `workspace/symbol`, ordinary completion, and `rename` leave them alone.
  Verified by `cargo test --all-targets` (267 tests). See
  [Lombok annotation support](backlog/summary.md#lombok-annotation-support).

- **Constructor modelling and `new T(...)` resolution** — the declared-type layer
  now carries a type's constructors, kept out of `.`-completion and
  `workspace/symbol`: `collect_members` reads `constructor_declaration`s, a
  record's canonical constructor and a class's implicit no-arg one are
  synthesized from the source, and a public `<init>` becomes a constructor for a
  jar/JDK type while `<clinit>`, private, and synthetic members stay skipped. A
  `new T(...)` now answers signature help with the created type's constructor
  overloads, and resolves go-to-definition (falling back to the type for an
  implicit or library constructor), find-references, and parameter-name inlay
  hints, its overload chosen by the call's argument types, then arity, then name.
  Verified by `cargo test --all-targets` (253 tests). See
  [Model constructors so new T(...) resolves](backlog/summary.md#model-constructors-so-new-t-resolves).

## 2026-09-24

- **Forget a deleted source file in the type model** — `WorkspaceIndex` now
  splits the declared-type model into a non-source base (dependency jars and the
  JDK) plus one model per workspace source file keyed by URI, so deleting a file
  drops its types along with its index entries and no model-based feature
  (member completion, hover, signature help, inlay hints) resolves it any more.
  Verified by `cargo test --all-targets` (239 tests). See
  [Forget a deleted source file in the type model](backlog/summary.md#forget-a-deleted-source-file-in-the-type-model).

- **External file changes and a fresh analysis model** — the shell registers a
  `workspace/didChangeWatchedFiles` watcher for `**/*.java` (when the client
  supports dynamic registration), re-indexes a created/changed file and drops a
  deleted one, layers every open buffer's declared types over the warm-up model,
  and republishes diagnostics for every open document after an open, change,
  close, or watched event. Verified by `cargo test --all-targets` (236 tests).
  See
  [External file change detection and a fresh analysis model](backlog/summary.md#detect-external-file-changes-and-keep-the-analysis-model-fresh).

- **Create-symbol quick fixes** — unresolved symbols now offer full create
  actions: class / interface / enum / record (a scaffolded file), and a method,
  field, or local variable with the signature inferred from the usage
  (parameter types from the call's arguments, the return type from the
  assignment/declaration/`return` context), including on a workspace receiver's
  type. Verified by `cargo test --all-targets` (229 tests). See
  [Create-symbol quick fixes for unresolved symbols](backlog/summary.md#create-symbol-quick-fixes-for-unresolved-symbols).

- **Unresolved-symbol diagnostics and quick fixes** — the server now reports
  unresolved types, members, bare identifiers, and imports as `ERROR`
  diagnostics (gated on a clean parse and an indexed `java.lang`, disabled by
  `JAVA_LSP_SEMANTIC_DIAGNOSTICS=0`), and serves `textDocument/codeAction`
  (`codeActionProvider`, kind `quickfix`) offering "Add import", a did-you-mean
  rename, and create-stub actions — the create-type fix a `CreateFile` resource
  operation, withheld unless the client advertises it. Verified by
  `cargo test --all-targets` (225 tests). See
  [Unresolved-symbol diagnostics and quick fixes](backlog/summary.md#unresolved-symbol-diagnostics-and-quick-fixes).

## 2026-09-23

- **Float literals and parameter hints honour overloads** — a floating literal
  is now typed by its suffix (`5.0f` is `float`, `5.0` is `double`), so
  `data.test(5.0f)` selects the `float` (or widening `double`) overload instead
  of falling back to the first same-arity one; and inlay parameter hints select
  the callee from the argument types, emitting no hint rather than a wrong name
  when the overload cannot be pinned down. Verified by
  `cargo test --all-targets` (213 tests). See
  [Float literals and parameter hints ignore overload resolution](backlog/summary.md#float-literals-and-parameter-hints-ignore-overload-resolution).

- **Overload-aware completions and signature help** — method completions list
  each overload of a method as its own item, labelled with the full signature and
  inserting `name(`, in both `.`-member completion and the workspace-index
  source; the shell advertises `signatureHelpProvider` and serves
  `textDocument/signatureHelp` from the type layer, marking the argument the
  cursor is in as `activeParameter`. Verified by `cargo test --all-targets`
  (203 tests). See
  [Overload-aware completions and signature help](backlog/summary.md#overload-aware-completions-and-signature-help).

- **Argument-aware overload resolution for navigation** — go-to-definition and
  find-references now select the overload a call targets from its argument types
  (arity when the types are inconclusive): a new `assignable` relation in the
  type layer plus `member_for_arguments` narrow a callee's overloads, and a
  reference is attributed only when its call accepts the selected overload's
  parameters. `rename` stays name-group-wide. Verified by
  `cargo test --all-targets` (208 tests). See
  [Argument-type overload resolution for navigation](backlog/summary.md#argument-type-overload-resolution-for-navigation).

- **Warm-up and source-fetch progress** — the server now reports the background
  warm-up to the client: one work-done progress item (`window/workDoneProgress/
create` + `$/progress`) titled `java-lsp` whose message names each phase with
  counts (`Indexed N source files`, `Indexed N dependency jars`, `Indexed N JDK
classes`, `Fetching N dependency sources` with a rising percentage), plus a
  single `window/showMessage` notice when `JAVA_LSP_OFFLINE` disables source
  fetching on a workspace that has dependencies. Gated on the client's
  `window.workDoneProgress` capability; emitted through a new `Reporter` and the
  `Progress`/`Message` engine events. Verified by `cargo test --all-targets`
  (198 tests). See
  [Warm-up and source-fetch progress reporting](backlog/summary.md#warm-up-and-source-fetch-progress-reporting).

- **Message-based engine boundary** — the shell and the engine now meet at a
  `tokio` channel boundary (`src/engine.rs`: `Command`, `EngineEvent`,
  `EngineHandle`, and a dispatcher task) instead of a read-locked
  `Box<dyn SemanticEngine>` trait. Mutations are applied inline in arrival
  order while read-only queries run on spawned tasks, and diagnostics arrive as
  `EngineEvent`s the shell's drain task publishes. The `SemanticEngine` trait,
  `SyntaxOnlyEngine`, and `src/engine/` are gone; the analysis core moved to
  `src/analysis.rs` unchanged. Verified by `cargo test --all-targets` (193
  tests). See [Message-based engine boundary](backlog/summary.md#message-based-engine-boundary).

- **Maven dependency sources** — the server now fetches each resolved
  dependency's `<a>-<v>-sources.jar` from a Maven repository (Maven Central by
  default; `JAVA_LSP_MAVEN_CENTRAL_URL` overrides, `JAVA_LSP_OFFLINE=1` opts
  out), writes it into the local Maven repository, extracts it under
  `JAVA_LSP_SOURCES_CACHE` (default `~/.cache/java-lsp/sources`), and indexes
  the Java sources — so hover and `.`-completion carry real signatures and
  parameter names and **go-to-definition opens the extracted source file**. A
  new `library_source` entry flag admits those declarations to `definition`
  while `WorkspaceIndex::source_files` excludes the cache, so references and
  rename never read or edit it. Verified by `cargo test --all-targets` (193
  tests). See
  [Maven source download and library navigation](backlog/summary.md#maven-source-download-and-library-navigation).

## 2026-09-22

- **Dot receiver recovery and wider `var` inference** — an incomplete
  `receiver.` at the end of a line now keeps its receiver (the dot can parse
  into the next token, so `gson.` before a `var` line read `gson.var` as a
  scoped type identifier and completed to nothing), and `var` bindings infer
  from more initializer shapes: an enhanced-for iterable's element type, a
  try-with-resources binding, and a conditional, array creation, `instanceof`,
  or `switch` expression. Inherited `java.lang.Object` methods now resolve for
  typing, hover, and `var` inference without rejoining `.`-completion listings.
  Verified by `cargo test --all-targets` (184 tests). See
  [A dot at line end loses its receiver, and `var` locals frequently infer no type](backlog/summary.md#a-dot-at-line-end-loses-its-receiver-and-var-locals-frequently-infer-no-type).

- **Record components as accessors** — a source record's header components are
  now modelled and indexed. `.`-completion on a record value offers each
  component as its accessor (`x()`), narrowed by the typed prefix, and a record
  with no components adds nothing; hover renders the accessor's signature and,
  on the declaration, the component list (`record Point(int x, int y)`); and
  go-to-definition, references, rename, and `workspace/symbol` target the
  component at its position in the header, while the bare component name
  resolves inside the record. Verified by `cargo test --all-targets` (167
  tests). See
  [Record components are not modelled, so member completion on a record is empty](backlog/summary.md#record-components-are-not-modelled-so-member-completion-on-a-record-is-empty).

- **Type-aware review follow-ups** — fixes the defects found reviewing the
  type-aware work. References and rename now identify a member's declaring type
  by simple name _and_ package, so renaming `a.Widget.run` no longer touches
  `b.Widget.run`; they report a declaration only when `include_declaration`
  asks, target nothing on a non-terminal import segment, count a
  fully-qualified use as visibility, and refuse (rather than drop a reference)
  when a candidate file cannot be read or parsed. A qualified supertype and a
  nested class-file type (`Map$Entry`) now resolve, and an `implements` list is
  diagnosed once per unresolved name instead of twice. A call whose argument
  count matches no overload gets no parameter hint, hints never fall outside
  the requested range, `rename` rejects restricted identifiers
  (`var`/`record`/`yield`), the harness no longer prints debug output, and the
  committed `example/` is back to valid Java with no build artifacts in the
  change. Verified by `cargo test --all-targets` (157 tests); bench with a real
  JDK indexed (Temurin 25, `java-lsp-bench --files 5`, release): warm-up ≈ 6.2 s,
  peak RSS ≈ 197 MB, warm-up hover RTT ≤ 1.1 ms, post-warm-up hover RTT 1.2 ms.
  See [Type-aware review follow-ups](backlog/summary.md#type-aware-review-follow-ups).

- **Generic type-argument inference** — `receiver_type` now binds and substitutes
  type arguments for calls: a method's own type parameters from its argument
  types (`List.of(5)` renders `List<Integer>`, primitives boxed) and the
  receiver's parameters from its own arguments (`List<String> l; l.get(0)`
  renders `String`), with fields substituted the same way. Calls pick their
  overload by arity (a new `member_for_call`), which also gives parameter hints
  the right names, and literals now carry a type. A call that cannot be pinned
  down keeps the written form; erased class-file signatures stay as they are.
  Verified by 141 tests and against a real JDK (`: List<Integer>`,
  `: String`). See
  [Generic type-argument inference](backlog/summary.md#generic-type-argument-inference).
- **Separate completions for same-named symbols** — workspace-index completions
  no longer collapse every symbol that shares a simple name into one item. The
  dedupe key is now the insert text plus the import the item carries, so typing
  `Li` offers both `class of java.awt` and `interface of java.util`, each with
  its own `import` edit, while duplicate entries for one symbol and a local
  shadowing a same-named field still collapse. Previously the surviving item was
  arbitrary — accepting `List` inserted `import java.awt.List;`. Verified by 137
  tests. See
  [Separate completions for same-named symbols](backlog/summary.md#separate-completions-for-same-named-symbols).
- **Import-aware type-name resolution** — `resolve_name` now resolves a simple
  type name through the file's imports (an exact single-type import, then the
  package, then a wildcard import, then a unique match) and returns a
  package-qualified reference; `TypeLookup::lookup`/`members`/`member_owner`
  honour that qualifier, and `receiver_type` qualifies a local's or field's
  declared type name. This fixes names shared across packages (a real JDK indexes
  both `java.util.List` and `java.awt.List`), so hover, `.`-completions, and
  inlay hints work for them — `var x = List.of(3);` now hints. Verified by 136
  tests. See
  [Import-aware type-name resolution](backlog/summary.md#import-aware-type-name-resolution).
- **Type and parameter inlay hints** — the shell advertises `inlayHintProvider`
  and `SemanticEngine` gains `inlay_hints` (defaulting to none), answered from
  the open document's tree plus the type model for the client's requested range
  only. Three families: variable type hints (a `var` local's initializer is
  inferred through `receiver_type`, which also types `var` locals in the scope
  hover and completions read), parameter-name hints at call sites, and the
  return types of intermediate method-chain links. `Member` now carries
  parameter names for source-declared methods, so hover signatures include them
  while class-file members stay name-less and produce no parameter hint.
  Verified by 128 tests. See
  [Type and parameter inlay hints](backlog/summary.md#type-and-parameter-inlay-hints).
- **Find references and rename** — `SemanticEngine` gains `references` and
  `rename` (defaulting to an empty list and `None`), and the shell advertises
  `referencesProvider`/`renameProvider`. Resolution reuses the type layer —
  including a member's declaring type from the receiver's type — then searches
  the workspace's `.java` files (substring-prefiltered, parsed on demand) for
  occurrences it can attribute with confidence: types only in files that can
  see them, member accesses only where the receiver resolves the name to the
  same declaring type, unqualified names only inside the declaring type's span,
  and locals only when their method declares the name once. `rename` refuses
  with `null` for an unresolved or ambiguous target, a library declaration, or
  an invalid identifier. Verified by 120
  tests. See [Find references and rename](backlog/summary.md#find-references-and-rename).
- **Library type signatures from JVM descriptors** — `classfile.rs` now parses
  each member's descriptor into a type (so a jar/JDK type carries
  `String getName()`, not a bare name) and records the superclass and
  interfaces (skipping the implicit `java.lang.Object`), and the JDK's
  `lib/src.zip` path feeds the model through `collect_type_infos`; library
  receivers therefore offer inherited members exactly like workspace types, and
  each jar is read once via the new `jar_outputs`. Costs ~14 MB more peak RSS
  with a JDK indexed (192 MB vs 178 MB) and ~0.8 s more warm-up. Verified by 113
  tests. See [JVM member descriptors](backlog/summary.md#jvm-member-descriptors-for-library-type-signatures).
- **Type-aware engine (pure Rust)** — a new `src/types.rs` models declared
  types, members, and hierarchies, built during warm-up from the source trees
  (full signatures) over name-only types synthesized from the indexed jars and
  JDK. It now backs `TreeSitterEngine::hover` (previously hardcoded `None`, now
  rendering a member's signature, a type declaration, or a local's type),
  member completions after `.` (the receiver's inferred type, inherited members
  included, still empty for an uninferrable receiver), and conservative
  `type X cannot be resolved` warnings that never fire without an indexed JDK or
  for imported, same-package, or type-parameter names. `SemanticEngine` and the
  LSP shell are unchanged. Costs ~13 MB more peak RSS with a JDK indexed
  (178 MB vs 165 MB). Verified by 109 tests; the bench's post-warm-up hover
  probe now asserts resolved content. See
  [Type-aware semantic engine](backlog/summary.md#type-aware-semantic-engine).

## 2026-09-09

- **Standard library indexing** — the installed JDK's `java.*`/`javax.*`
  declarations are indexed during warm-up from `jmods` class files,
  `lib/src.zip` sources (tree-sitter parsed), or `rt.jar`, discovered via
  `$JAVA_LSP_JDK`/`$JAVA_HOME`/SDKMAN and read with a streaming ZIP walk;
  JDK types appear in completions with auto-import edits (none for
  `java.lang`), stay out of navigation, and a missing JDK is a graceful
  no-op. Real-JDK baseline (Temurin 25 via src.zip, ~4.2k files): warm-up
  5.5 s, peak RSS 165 MB, hover RTT during warm-up still ≤ 1.6 ms. Verified
  by 92 tests. See [Standard library indexing](backlog/summary.md#standard-library-jdk-indexing).
- **Auto-import on completion accept** — the index now records each symbol's
  package (from the source file's `package_declaration` or the jar class's
  internal name), and completion items from other packages, modules, or jars
  carry an `additionalTextEdits` adding the `import` line (after the last
  import, else after the package statement, else at the top); never-worsen
  rules suppress the edit when the name already resolves or would conflict.
  Verified by 86 tests. See
  [Auto-import on completion accept](backlog/summary.md#auto-import-on-completion-accept).
- **Maven project model and dependency indexing** — the warm-up now builds a
  Maven model from statically parsed `pom.xml` files (multi-module, `<build>`
  directory overrides) and scans exactly the declared source roots, then
  resolves each module's full dependency closure offline (parent chains,
  property interpolation, `dependencyManagement` with import-scoped BOMs,
  transitive walk with nearest-wins mediation, exclusions, optionals) from
  `$MAVEN_REPO`/`~/.m2` and indexes the jars' class files via a minimal
  reader; dependency types are offered in completions while definition and
  `workspace/symbol` stay source-only. Verified by 79 tests; the bench's new
  `--maven` mode keeps all five features under 1 ms during a 500-file scan.
  See [Maven project model and dependency indexing](backlog/summary.md#maven-project-model-and-dependency-indexing).

## 2026-09-08

- **Performance benchmark harness** — `src/bin/java-lsp-bench.rs` generates a
  configurable fixture Java workspace, drives the real `java-lsp` binary over
  stdio JSON-RPC, and reports per-feature first-response time, hover RTT
  during index warm-up (detected via the server's readiness log line), and
  peak RSS (`VmHWM`); v0.1 baseline at `--files 500 --methods-per-class 10`
  on aarch64 / 8 cores: first response 0.6–0.7 ms from didOpen (≤1.6 ms from
  process start) for all five features, warm-up 116 ms (5 hover samples,
  max 0.1 ms), post-warm-up RTT 0.1 ms, peak RSS 11.8 MB. See
  [Performance benchmark harness](backlog/summary.md#performance-benchmark-harness).
- **Go-to-definition and workspace symbols** — `TreeSitterEngine::definition`
  now resolves the word under the cursor through the workspace index (import
  targets via the dotted path's last segment, declaration-name lookups
  narrowed by the node kind at the cursor, same-file overload collapse,
  `null` on multi-URI ambiguity) and the shell serves `workspace/symbol`
  from a new `SemanticEngine::workspace_symbols` prefix query with
  `workspaceSymbolProvider` advertised. Verified by 52 tests. See
  [Navigation v1](backlog/summary.md#navigation-v1).
- **Type-free completions** — `TreeSitterEngine::completions` now merges three
  ranked sources (`sort_text` `0`/`1`/`2`): Java keywords, names in scope from
  the open document's tree (parameters, locals, enclosing fields), and
  workspace index symbols (members labeled `Container.name`, inserted under
  their simple name); results are partial rather than blocked during index
  warm-up, and a completion directly after `.` returns an empty list. See
  [Completions v1](backlog/summary.md#completions-v1).
- **Workspace symbol index** — `TreeSitterEngine` now owns a `WorkspaceIndex`
  (`src/index.rs`): a background scan of the workspace's `*.java` files
  extracts declarations and imports into in-memory entries, edits update only
  the edited file's entries, and `index_ready()` gates index-backed features
  during warm-up without ever blocking the request path. Verified by 31
  tests. See [Non-blocking workspace symbol index](backlog/summary.md#non-blocking-workspace-symbol-index).
- **Syntax features via tree-sitter** — `TreeSitterEngine` now parses each
  open document with tree-sitter-java and publishes parse-error diagnostics
  (which clear on fix), hierarchical document symbols, folding ranges, and
  semantic tokens behind the `SemanticEngine` seam; the shell advertises the
  three new capabilities. Verified by 22 tests. See
  [Syntax features via tree-sitter](backlog/summary.md#syntax-features-via-tree-sitter).
- **Zed extension** — `zed-java-lsp/` provides Java support in Zed around
  java-lsp: tree-sitter-java grammar, the Java language definition (queries
  vendored from `zed-extensions/java`), and the java-lsp binary as language
  server, resolved from settings override, `$PATH`, or the repository's
  `target/` builds. See [Zed extension](zed-java-lsp/README.md) and
  [LSP shell skeleton](backlog/summary.md#lsp-shell-skeleton).
- **LSP shell skeleton** — `java-lsp` now builds a tower-lsp server over
  stdio with incremental versioned text sync, `SemanticEngine`/`SyntaxOnlyEngine`
  seam, and a `LspService` test harness; verified with 12 unit/integration
  tests and a stdio JSON-RPC smoke test. See
  [LSP shell skeleton](backlog/summary.md#lsp-shell-skeleton).
