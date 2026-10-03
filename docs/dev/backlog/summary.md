---
type: Reference
title: Backlog summary
description: Condensed records of the finished change requests that once lived in the backlog, newest first.
tags: [dev, backlog]
status: stable
---

# Backlog summary

The [backlog](index.md) holds open change requests. Once a request is finished —
`done`, `rejected`, or `superseded` — and has sat a while, its full document is
retired here as a compact section and the original is deleted; the
[changelog](../changelog.md) links every shipped request into its section below.

## Cache each archive's parse in its own file, read on demand

`kind: improvement` · `state: done` · `priority: high` · `owner: felix` · finished 2026-10-01

**Problem** — The class-file base cache was one JSON file per kind, loaded whole and rewritten whole each run (`jars.json` reached 3.7 GB).

**Proposal** — Replace it with `base_cache::ArchiveStore`, one small versioned file per archive under `<cache>/base/<kind>/<hash>.json`, `get`ting on demand and `insert`ing only on (re)parse.

**Decisions**

- One file per archive keyed by an identity hash — read only what is needed, write only what changed.
- Each file carries the version and identity — a schema bump or hash collision is a miss, not a wrong parse.
- `insert` only on a miss — a warm run writes nothing.
- The JDK keeps one key for the whole JDK — it is small and indexed as a unit.
- Drop the legacy `<kind>.json` on first use — it could be 3.7 GB and is no longer read.

**Acceptance criteria** — No code path loads or rewrites a whole-kind cache (a warm run writes nothing); a hit is guarded by version + identity, a miss reparses to unchanged answers; build/`fmt --check` clean and `base_cache`/`hub`/`sources` tests pass.

## Keep the hub debug log readable and emit progress in steps

`kind: improvement` · `state: done` · `priority: medium` · `owner: felix` · finished 2026-10-01

**Problem** — The hub logged every per-item notification at `debug` (thousands of lines on a large workspace), and the dependency-source pass emitted a `Progress` update per archive.

**Proposal** — Log the high-cardinality per-item notifications at `trace` and everything else at `debug`, and emit extraction progress ~5% of the way instead of per archive.

**Decisions**

- Bulk = the per-item data notifications (`SourceFile`, `BaseArtifact`, `RemoveBase`, `SourceEntries`, `SourceRemoved`, `SourceTypes`, `DirtyTypes`, `DirtyTypesDropped`, `Progress`) — their cardinality is proportional to the corpus.
- Keep the messages, just at `trace` — they are real index-feeding messages; only their logging moves down a level.
- Progress in ~5% steps (`progress_step = (total / 20).max(1)`, plus the final total) — enough to see it move, ~20 lines not thousands.
- Timing `Log` lines stay at `debug` — they are `Log`, not `Progress`, so the per-phase split stays visible.

**Acceptance criteria** — `RUST_LOG=java_lsp::hub=debug` no longer prints a line per source file/artifact/progress tick while the flow and timing lines remain; `=trace` still prints every message; the extraction pass emits ~20 progress updates; build/`fmt --check` clean and `hub`/`sources` tests pass.

## Hub requests return an awaitable reply receiver

`kind: refactor` · `state: done` · `priority: low` · `owner: felix` · finished 2026-10-01

**Problem** — `HubClient` had two callback-built request paths plus per-method `_async` copies, and a boxed reply deliverer that made the request flow hard to read.

**Proposal** — Every `HubClient` request returns a `Reply<R>`, a newtype over a `tokio::sync::oneshot::Receiver<R>` implementing `Future` with a `blocking_recv()` for synchronous callers; `ReplyHandle` carries the oneshot sender and the client builds requests directly.

**Decisions**

- Synchronous callers stay synchronous (option A) — the deep recursive tree-sitter walks already run off the runtime's workers and just `blocking_recv` at the call site; making them `async` was a large rewrite for no gain.
- A thin `Reply<R>` wrapper, not a bare receiver — keeps the hub contract that an unanswered request or gone hub resolves to `R::default()`.
- The reply still rides the hub — `ReplyHandle::send` posts through the hub (logging and timing it) before the oneshot, keeping type-erased `Inbound::Reply` delivery.
- The `_async` methods are gone — `code_actions`, `all_symbols`, and `ready` serve both caller kinds, behaviour unchanged.

**Acceptance criteria** — `HubClient` has no blocking request method, no `request_async`, and no `_async` methods; every request returns `Reply<R>`; `ReplyHandle` holds a `oneshot::Sender<R>` (std mpsc delivery gone); async callers await, sync modules `blocking_recv`; existing tests pass unchanged in behaviour.

## Every participant subscribes to the hub through its HubClient

`kind: refactor` · `state: done` · `priority: medium` · `owner: felix` · finished 2026-10-01

**Problem** — The hub's routing table was fixed at `spawn_router` start, forcing `engine::spawn` and the server to pre-create a channel per module and driver and thread receivers through every spawn function.

**Proposal** — Start the hub empty (`hub::spawn_hub() -> HubClient`) and let participants register at runtime via `client.subscribe()` / `client.serve(Module::X)`; one `engine::start(lsp_client) -> JavaLanguageServer` builds the hub and every participant, server included.

**Decisions**

- One message type for everyone — drivers receive `Hub` like modules and match `Hub::Notify` (a driver never serves a module).
- One setup function builds everything, the server included — `engine::start` starts the hub, builds the server (which subscribes on its own client), then the modules and drivers; `JavaLanguageServer::new` takes the LSP client and its `HubClient`.
- No message is lost — every participant subscribes synchronously in its spawn function over the hub's FIFO inbound channel before sending; the listening drivers subscribe, the JDK driver does not.
- A second owner is a wiring bug — log an error and keep the first owner; the newcomer still receives notifications (a panic would kill the hub thread).
- Dropped receivers are pruned — a failed send or closed owner is removed, and its requests then resolve to the default reply.
- Behaviour is unchanged — same participants, labels, and FIFO ordering; each subscription logs at `debug`.

**Acceptance criteria** — `spawn_router`, `hub::channel`, and every module/driver `rx` parameter are gone (`spawn_module` takes only a client, plus the runtime handle for analysis); `HubClient::subscribe`/`serve` exist with first-owner-wins and pruning; `engine::start` is the single setup path and `JavaLanguageServer::new` no longer starts the engine; `cargo build --all-targets` warning-free and `cargo test` passes.

## Do not rewrite dependency sources already extracted to the cache

`kind: improvement` · `state: done` · `priority: medium` · `owner: felix` · finished 2026-10-01

**Problem** — `index_one` called `create_dir_all` once per file and rewrote every extracted `.java` on every start, hundreds of thousands of redundant writes and directory ops.

**Proposal** — Create each package directory once per artifact (a `HashSet<PathBuf>`) and write a file only when missing or its length differs (`metadata().len() != text.len()`); the parse still uses the in-memory jar text.

**Decisions**

- Skip by existence + length, not existence alone — one `stat` instead of a `write`, and a truncated/partial file is rewritten.
- Directory creation deduped per artifact — distinct artifacts extract to distinct `<group>/<id>/<version>` subtrees, so per-artifact dedupe is complete.
- The `write` phase timing now shows the saving — `Timers::write` covers the (usually skipped) directory + file I/O.

**Acceptance criteria** — On a warm cache the `write` phase is ~0 versus seconds cold, with extracted files still present and correct; build/`fmt --check` clean and `sources`/`hub` tests pass.

## Batch and cache the diagnostics pass's index queries

`kind: improvement` · `state: done` · `priority: medium` · `owner: felix` · finished 2026-10-01

**Problem** — The semantic diagnostics pass issued an `IndexQueryName` hub round trip per lookup, several per type reference, repeated for every open document and every keystroke-triggered sweep.

**Proposal** — Give the pass a per-sweep name cache with batched prefetch (`Request::IndexQueryNames`, `Request::IndexHasPackages`) and fetch `ready`/`type_model`/`type_layers` once per sweep; diagnostics output unchanged.

**Decisions**

- The cache lives for one sweep, shared across all open documents — the index is already read non-atomically, and common names are then fetched once per sweep.
- Batch with `IndexQueryNames { names: Vec<String> }` → `HashMap<String, Vec<Arc<SymbolEntry>>>` and `IndexHasPackages { packages: Vec<String> }` → `HashSet<String>` — one round trip and two `count=N` log lines per sweep.
- The prefetch walk collects type-identifier base names, symbolic identifiers, import/static-import simple names, and wildcard-import packages — over-collecting is harmless, a miss only falls back.
- `ready`, `type_model`, `type_layers` are fetched once per sweep and passed into each document's pass — they are per-index, not per-document, state.
- `import_edit` takes a trait abstraction (`query_name`) instead of `&IndexHandle` — diagnostics reads through the cache while completion call sites keep the handle unchanged.
- Out of scope — re-checking only the changed document, filtering non-Java `FileEvent`s, and moving these requests to `trace`.
- Noted, not fixed — a burst of edits triggers one sweep per message (the module does not drain the queue), contradicting the architecture's collapsed-sweep claim.
- Measured by a hub-log test using the `tracing`-capture approach — no new benchmark.

**Acceptance criteria** — Existing diagnostics and quick-fix tests pass unchanged; a two-document sweep emits exactly one `IndexQueryNames`, at most one `IndexHasPackages`, no repeated `IndexQueryName`, and one `IndexReady`/`IndexTypeModel`/`IndexTypeLayers`, while a semantic-off sweep sends no name/ready/type-model requests; the new requests log as `count=N` and their replies log like any other request; build/`fmt --check`/`test` clean.

## Log what the index holds, so its size can be targeted

`kind: improvement` · `state: done` · `priority: low` · `owner: felix` · finished 2026-10-01

**Problem** — Nothing reported the index's composition (the base cache alone had reached 3.7 GB), so any size reduction would be guesswork.

**Proposal** — `WorkspaceIndex::composition()` returns a one-line summary of entries by kind, names, base types/members, and approximate bytes, logged at the base `Ready` and the closing `StageDone(Downloads)` milestones.

**Decisions**

- Counts by kind plus approximate bytes, not exact — the split between entries and the type model is what matters.
- Logged by the index module at `target: "java_lsp::hub"` — the module has no hub client and the hub must not block on its own request, so it logs directly under the existing debug filter.
- At `Ready` and `StageDone(Downloads)` — the delta shows how much extracted library sources add on top of jars/JDK.
- Measurement only, no behaviour change.

**Acceptance criteria** — A warm-up logs the composition line at `Ready` and after downloads, each naming entry counts by kind, base type/member counts, and approximate bytes; build/`fmt --check` clean and `index` tests pass.

## Stop storing import declarations for library sources

`kind: improvement` · `state: done` · `priority: low` · `owner: felix` · finished 2026-10-01

**Problem** — Import entries were ~2.9M of the flat index (~31%, ~0.39 GB) yet no feature read an `Import` entry.

**Proposal** — Add `index::drop_import_entries(&mut Vec<SymbolEntry>)` and call it from `sources::index_one` and `jdk::src_zip_entries` before storing extracted entries; workspace extraction keeps its imports.

**Decisions**

- Drop only in the library passes, not in `extract_entries` — workspace imports are few, two `index` tests pin them, and this keeps the blast radius small.
- A shared helper on `index.rs`, not an inline `retain` — both library passes need it and its doc records why imports are safe to drop.
- `kind: improvement` — it removes wasted work/storage without changing what any feature answers.

**Acceptance criteria** — The dependency-source and JDK passes store no `IndexKind::Import` entries while workspace extraction still does; the two workspace-import `index` tests pass unchanged; build/`fmt --check` clean and `index`/`hub`/`base_cache`/non-network `sources` tests pass with `tests/example_features.rs` green; the composition line's `imports=` count drops from ~2.9M toward zero with other counts unchanged.

## Make the LSP shell just another hub client

`kind: refactor` · `state: done` · `priority: medium` · `owner: felix` · finished 2026-10-01

**Problem** — The shell still reached the engine over bespoke `EngineHandle`/`Command`/`EngineEvent` channels and the `engine::dispatch` task, leaving it the one component that was not a hub module and putting editor knowledge in the hub.

**Proposal** — Give the shell a `HubClient` labeled `server`, register it as a module, and send its lifecycle/root/watched-file/capability notifications and all queries as hub messages; add a new `Module::Analysis` wrapping `TreeSitterEngine` that owns the query requests, and delete `Command`, `EngineHandle`, `EngineEvent`, `dispatch`, and `translate`.

**Decisions**

- The shell is a peer module and the core is a request-owning module — one vocabulary and one mechanism for every component; `DocumentStore` stays in the shell as turning incremental LSP edits into text is LSP-specific. Superseded 2026-10-03: `DocumentStore` moved into the document module, a hub client (`document.rs`), so the shell holds no document state; see the [changelog](../changelog.md).
- Async requests on `HubClient` — handlers are async and the harness runs a current-thread runtime, so the shell awaits a `tokio` oneshot while blocking callers are kept.
- The Analysis module runs on its own thread with queries on the blocking pool — matches index/diagnostics, and ordering holds via the FIFO hub and module channel.
- The hub becomes neutral — no `events` sender, no `translate`; the hub renders `Log`/`Summary`, while the shell renders `Diagnostics`, `Progress`, and `Notice`.
- The diagnostics module clears a closed document — it emits `Diagnostics { version: None }` on `DocumentClosed` and the shell publishes no clear.
- Naming and test hooks — `engine.rs` keeps its name as wiring + drivers, `engine()` becomes `hub()`, the dead `TreeSitterEngine.events`/`set_events` is deleted.
- Behaviour preserved — LSP surface, diagnostics (including clear on close and republish), progress, notices, watcher registration, and warm-up are unchanged.
- The diagnostics sweep follows `AnalysisUpdated` — the analysis module notifies it after applying an edit so the sweep reads an index holding the edit.
- The analysis thread gets the runtime's stack size — `RUNTIME_STACK_SIZE` moves to `lib.rs` to avoid reintroducing the stack-overflow abort.

**Acceptance criteria** — `JavaLanguageServer` holds a `HubClient` and hub channel with no `EngineHandle`; `Command`, `EngineHandle`, `EngineEvent`, `dispatch`, and `translate` no longer exist and `spawn_router` takes no editor sender; `Module::Analysis` owns the query requests with the hub naming `analysis` as responder; `HubClient` offers both blocking and async requests; closing a document clears diagnostics via the diagnostics module; `TreeSitterEngine.events`/`set_events` is gone; build is warning-free and library, harness, and `stdio_smoke` tests pass; an edit immediately followed by a query at the new position sees the edit.

## Log per-phase timings for the dependency-source pass

`kind: improvement` · `state: done` · `priority: low` · `owner: felix` · finished 2026-10-01

**Problem** — The dependency-source pass gave no clue where its time went, so its cost was guessed rather than measured.

**Proposal** — `index_sources` logs the fetch line and `index_extracted` accumulates and logs per-phase wall clock (read, inflate, write, parse, entries, types), each one `Info` line per warm-up.

**Decisions**

- Phase counters, not per-file logging — one `AtomicU64` of nanoseconds per phase summed across workers and printed once.
- `inflate` is measured as `for_each_zip_entry` wall time minus callback time — the callback-owned phases are direct, the remainder is the reader.
- Always on — a handful of `Instant::now` per file is negligible against what it measures.
- Logged periodically, not only at the end — every 200 archives and once at the end, each line named `(attempted/total archives)`.
- The lines are `DriverMessage::Log` — they surface on `java_lsp::hub` debug and `java_lsp::messages` info (note `RUST_LOG=java_lsp=debug` is needed to see both).

**Acceptance criteria** — A warm-up logs the fetch line and the extract phase line periodically (every 200 archives) and once at the end, with phases summing to roughly the pass wall clock times the worker count; build/`fmt --check` clean and `sources`/`hub` tests pass.

## Extract and parse dependency sources across worker threads

`kind: improvement` · `state: done` · `priority: medium` · `owner: felix` · finished 2026-09-30

**Problem** — `index_extracted` processed every artifact on a single blocking-pool thread, leaving the other cores idle and dominating the download driver's time on large closures.

**Proposal** — Fan artifacts across `available_parallelism` scoped worker threads, each with its own `java_parser()`, with the calling thread emitting all messages.

**Decisions**

- Parallelize across artifacts, one jar per work item — matches the many-dependencies workload and bounds memory, deferring file-level fan-out until a single dominant dependency is the bottleneck.
- Use `thread::scope` + `available_parallelism`, one parser per worker, no `rayon` — `Parser` is `Send` but not `Sync`.
- Workers return their finished layer; the calling thread emits — the `sink` is not `Send`, and emission on one thread preserves per-artifact message order.
- Extract the per-artifact body into `index_one` returning an `Extracted` enum — safe to run off the calling thread.
- Worker count is `min(available_parallelism(), artifacts.len())` — a one-artifact run spawns no thread.

**Acceptance criteria** — artifacts extract/parse concurrently with one parser per worker and emission only from the calling thread; published messages are unchanged (`RemoveBase` + `BaseArtifact` per artifact, same progress/indexing sequence); the 30-jar measurement drops from ~18.7 s to a few seconds; `cargo build`/`fmt --check` clean and `sources`/`hub` tests pass.

## Make byte-offset to LSP position conversion linear, not quadratic, per file

`kind: bug` · `state: done` · `priority: high` · `owner: felix` · finished 2026-09-30

**Problem** — `analysis.rs::lsp_position` rescanned the whole text from byte 0 on every call, making per-file indexing O(declarations × file size) — ~110 ms/file and 96% of the dependency-source pass.

**Proposal** — Add `analysis.rs::LineIndex` built once per text and binary-searched per position, threaded through the entry-collection path.

**Decisions**

- A `LineIndex` built once per file and binary-searched per position (`new`/`position`/`range`) — gives `O(n + k log n)` instead of `O(n · sum(offsets))` at the call site with one text and many offsets.
- Thread the index through the entry collectors rather than change `lsp_position` — fixes the measured 96% with a small, low-risk diff.
- Out of scope (follow-up): make `lsp_position`/`lsp_range` themselves take a `&LineIndex` and update all 134 call sites — a broad mechanical change with no behaviour change.
- Behaviour is identical — positions are byte-for-byte the same.

**Acceptance criteria** — `extract_entries` no longer rescans per declaration and the 30-jar pass drops from 373.6 s to <20 s; extracted `SymbolEntry` ranges unchanged and `sources`/`build`/`fmt` clean; no `java-lsp-bench` regression.

## Stop the hub logging a reply for an unanswered request

`kind: bug` · `state: done` · `priority: low` · `owner: felix` · finished 2026-09-30

**Problem** — The hub logged a fabricated `reply` line with a bogus latency for a request whose `ReplyHandle` was dropped unanswered, contradicting its own contract; the architecture doc's "no round-trip through the hub" claim was stale; the new log contract was untested.

**Proposal** — Log the reply line only for real replies (`deliver.is_some()`), correct the architecture sentence, and add a test pinning the contract.

**Decisions**

- Scope is the `message-hub-log-sender` work only — the wider `unified-hub`/`quickfix-subsystem` refactor is already captured elsewhere.
- Kind is `bug` — the dominant finding asserts something false.
- One request covers findings 1–3 — the doc correction and test are caused by the same behaviour.
- Detect a cancellation by `deliver` (`is_some()`), not a new flag — the distinction already exists in the protocol.
- Extract the line into a pure `reply_log(done, answered)` helper the test targets — avoids a process-global `tracing` subscriber leaking across the parallel test binary.

**Acceptance criteria** — an answered request logs exactly one `sender=<owner> reply to=<requester> <desc> elapsed=<ms>ms` line and an unanswered one logs none and leaves no `pending` entry; `docs/architecture.md` describes the actual routed request/reply flow with no synchronous hub-side index call; a test fails before the fix; build/fmt clean and touched tests pass.

## Identify the sender, replies, and reply latency in the hub log

`kind: improvement` · `state: done` · `priority: low` · `owner: felix` · finished 2026-09-30

**Problem** — Hub log lines named only the message, not its sender (all clients shared one identity-less `HubClient`), and replies bypassed the hub entirely, so the request/response path was invisible and unmeasured.

**Proposal** — Give every `HubClient` a name, wrap the hub's inbound in an envelope, and route replies back through the hub to log each request/reply pair with round-trip time.

**Decisions**

- The label is a free-form `String` set by each client — lets a client name itself without enumerating senders in the hub type.
- Every sender is distinct (`core`, `dispatch`, the six drivers, `diagnostics`, `quickfix`), splitting the core and dispatcher that shared one client — makes each origin attributable.
- The hub inbound channel is an envelope (`Notify`/`Request`/`Reply`) while the module-facing `Hub` is unchanged — leaves all module match arms untouched.
- Log shape prefixed `sender=<label>`, replies as `sender=<owner> reply to=<requester> <desc> elapsed=<ms>` — still debug-gated, target `java_lsp::hub`, never a payload.
- Route replies through the hub and time them there — makes the hub the single observer/timer, at the cost of a second hop per request even when logging is off.
- Change each `Request`'s `reply` field to `ReplyHandle<R>` whose `send(self, value)` consumes the handle — call sites stay textually unchanged.
- A dropped `Reply` posts a cancellation so the hub drops the pending entry — keeps the pending map from leaking.
- Labels are not asserted in tests — the improvement is verified by their presence and correctness.
- Out of scope: shell `Command`s and per-message payload rendering.

**Acceptance criteria** — every hub line carries a `sender=<label>`, with distinct labels for the ten senders; each request/reply yields a matching reply line with elapsed time and unanswered requests yield none; the module-facing `Hub`, `channel()`, and match arms are unchanged; logging stays debug-gated and payload-free; existing behaviour and the test suite are preserved.

## Publish each dependency's sources as one base artifact, not one per file

`kind: improvement` · `state: done` · `priority: low` · `owner: felix` · finished 2026-09-30

**Problem** — `index_extracted` emitted one `BaseArtifact` per extracted `.java` file, flooding the hub, hub log, and base with thousands of messages/layers for one dependency, contradicting the documented "one layer per artifact URI" model.

**Proposal** — Accumulate each artifact's entries and type infos into a single `BaseArtifact` keyed by the sources-jar URI, with the class-jar `RemoveBase` still emitted first.

**Decisions**

- One layer per artifact keyed by the sources-jar URI — the class-jar URI is superseded by `remove_base_layer` and would be dropped by `add_base_layer`.
- Entries keep their per-file URIs — preserves navigation and hover behaviour.
- Entries and type infos are merged across the artifact's files, in archive order — one layer needs one `Vec<SymbolEntry>` and one `TypeModel`.
- `index_extracted` takes a `&mut dyn FnMut(DriverMessage)` sink like `index_jars`/`index_jdk` — consistent, and lets a test count published messages.
- No architecture/doc change — the code is brought in line with the docs' existing "one layer per artifact URI".
- Out of scope: the workspace warm-up's per-file `SourceFile` notifications (a different, bounded producer).

**Acceptance criteria** — exactly one `BaseArtifact` per artifact with extracted sources (none for empty jars), keyed by the sources-jar URI with its class-jar `RemoveBase` first; published entries unchanged; a test asserts exactly one `BaseArtifact` over a multi-file sources jar; build/fmt clean and `sources` tests pass.

## Drive the dependency-source extraction progress by counts, with the parsed source-file count

`kind: improvement` · `state: done` · `priority: low` · `owner: felix` · finished 2026-09-30

**Problem** — The per-archive extraction progress reported an archive count plus a percentage that was redundant and collided with the fetch phase's already-complete bar, and it never surfaced the real unit of work (files parsed).

**Proposal** — Report the running parsed source-file count per archive and drop the percentage: `Parsed 7/312 dependency source archives (1840 source files)`.

**Decisions**

- Count-driven, no percentage — avoids conflation with the fetch phase's bar, which hits 100% before extraction begins.
- Include the running parsed source-file count (`files_indexed`) — the pass's real unit of work.
- Only the per-archive update changes — the fetch percentages and the start/end messages stay as they are.

**Acceptance criteria** — each per-archive update reads `Parsed x/N dependency source archives (F source files)` with `x` running `1..=N` and no percentage; start/end messages unchanged; the existing `sources` test asserts the new text; build/fmt clean and `sources` tests pass.

## Report progress while dependency source archives are extracted and indexed

`kind: improvement` · `state: done` · `priority: low` · `owner: felix` · finished 2026-09-30

**Problem** — The extraction/indexing phase (the slow one) emitted no per-artifact progress, so the work-done bar sat still and the hub log looked like an endless download with no way to tell "still extracting" from "stuck".

**Proposal** — Have `index_extracted` emit a `Progress(Update)` per archive, `Parsed x/N dependency source archives`, so the bar advances through extraction and completion is unmistakable.

**Decisions**

- Per-archive progress in the extraction loop, in the same `x/N` + percentage shape as `Fetched x/N` — the two phases should look alike and the count reaches N/N.
- Emitted after each archive, with skip paths moved into a labeled block — keeps the bar honest and fires the line for every archive.
- No new message types or hub changes — progress rides the existing `DriverMessage::Progress`.
- Out of scope: the fetch phase's already-adequate reporting and the per-artifact `debug` lines.

**Acceptance criteria** — one `Progress(Update)` per artifact, `Parsed x/N …`, with `x` running `1..=N` and the percentage reaching 100 on the last; start/end messages unchanged; a server-free test asserts the per-archive update; build/fmt clean and `sources` tests pass.

## Make the index and the diagnostics engine their own subsystems

`kind: refactor` · `state: done` · `priority: medium` · `owner: felix` · finished 2026-09-29

**Problem** — The `WorkspaceIndex` was owned and mutated directly by the core and hub, and diagnostics were computed inside the core rather than exchanged as messages, so the "one boundary, one hub" claim did not hold for the components the request path leans on hardest.

**Proposal** — Give the index and the diagnostics engine each their own message-driven subsystem, leaving the engine as the hub that binds them, with all index access through `IndexHandle`.

**Decisions**

- The index is a subsystem on its own dedicated thread — the hub makes synchronous index calls during dispatch, so a task on the same runtime would deadlock; each call blocks for its reply, returning shared `Arc`s.
- `IndexHandle` mirrors every `WorkspaceIndex` method the core or a driver needs — call sites change type only, not shape.
- Queries are messages to the subsystem, not round-trips through the hub — routing through the hub would deadlock, so the hub only owns the wiring.
- The hub stops touching the index — `apply_to_index` moves into the subsystem as `IndexHandle::apply`.
- Behaviour is preserved — same index results, warm-up ordering, and diagnostics gating.
- Two subsystems, one boundary: each parses its own source text, and the declared-type overlay lives with the index subsystem and is read via the handle.

**Acceptance criteria** — the `WorkspaceIndex` is owned only by the index subsystem and reached only through `IndexHandle` with no `apply_to_index` in the hub; diagnostics are computed by a diagnostics subsystem that parses buffers itself and reports them as messages with `DiagnosticsPublisher` gone; the declared-type overlay lives in the index subsystem; the suite is green apart from the known sandbox loopback-bind failures.

## A quick-fix subsystem, a queryable diagnostics cache, and a prefix-ordered index

`kind: refactor` · `state: done` · `priority: medium` · `owner: felix` · finished 2026-09-29

**Problem** — Quick fixes still lived in the core reading its document and index directly; the diagnostics pass published but kept no result so anything wanting it had to recompute; and `query_prefix` scanned every name in the workspace rather than those that matched.

**Proposal** — Give the quick fixes their own subsystem querying the symbol index and a new queryable diagnostics cache, and make the index's name map prefix lookups range scans.

**Decisions**

- `quickfix.rs` is a subsystem owning its own parser and open-buffer text, with a `QuickFixHandle` on its own thread, dispatched on the blocking pool — a blocking handle never starves a runtime worker.
- It queries the two indexes (via `IndexHandle` and `DiagnosticsHandle`) and holds no index/diagnostics state, falling back to the cached pass when a request carries none — keeps state with its owner.
- The diagnostics cache is queryable, keeping the latest `(version, Arc<Vec<Diagnostic>>)` per open document and dropped on close — queries share the `Arc`, not clone.
- The index's `by_name` becomes a `BTreeMap` and `query_prefix` a `range` scan — key set unchanged, so the index does not grow.
- Behaviour is preserved — the fix builders moved verbatim and the core's entry points remain thin delegators.

**Acceptance criteria** — `quickfix.rs` generates fixes from its own parse and `analysis.rs` no longer holds the builders; the diagnostics subsystem keeps a queryable cache cleared on close; `query_prefix` is an ordered range scan with an unchanged key set; the library's code-action/quick-fix tests pass.

## Deep analysis recursion overflows the engine runtime's default thread stack on large workspaces

`kind: bug` · `state: done` · `priority: high` · `owner: felix` · finished 2026-09-29

**Problem** — Deep-but-finite analysis recursion exceeded the 2 MiB default worker/blocking-pool stack of the runtime built by `Runtime::new()`, fatally aborting the server on large Maven workspaces.

**Proposal** — Build the runtime with `Builder::new_multi_thread().thread_stack_size(N)` so workers and the blocking pool both get a larger stack, without depending on `RUST_MIN_STACK`.

**Decisions**

- Fix with an explicit `thread_stack_size` rather than first chasing the offending walk — removes the abort at the observed depth with a small, low-risk change.
- One setting covers both workers and the blocking pool — the pinned tokio copies `thread_stack_size` into every blocking thread.
- The binary must not depend on `RUST_MIN_STACK` — relying on it leaves the 2 MiB default in editor sessions that do not export it.
- Size from the verified-good 256 MiB, then halve to the smallest working power of two, recording the value and rationale — bounds the reservation while guaranteeing the reported case passes.
- Out of scope: making the deep recursion iterative, and the separate `collect_java_files` walk that lacks a depth cap and symlink-cycle guard.

**Acceptance criteria** — `src/main.rs` builds the runtime via `Builder` with `.thread_stack_size(..)` that applies with `RUST_MIN_STACK` unset; the reported reproduction no longer aborts and analysis still answers; `cargo test --all-targets` stays green; the chosen size and rationale are commented at the construction.

## One engine hub with broadcast notifications and hub-routed request/response

`kind: refactor` · `state: done` · `priority: high` · `owner: felix` · finished 2026-09-29

**Problem** — After the subsystem extractions, modules still communicated through bespoke channels and cross-module handles, so "each module encapsulated, communicating by message" held only in spirit and every new interaction meant new wiring.

**Proposal** — Replace the per-module channels and handles with a single hub: the hub broadcasts notifications to every module and routes each request to the one module that owns it, with the reply on the same hub.

**Decisions**

- One message module, one client, one router — `Hub`, `Request`, and `translate` in `messages.rs`; `HubClient` and `spawn_router` in `hub.rs`.
- The hub is a thread, not a task — a blocking request from a runtime worker would deadlock a current-thread runtime (the harness).
- The index is a hub module — `IndexHandle` becomes `HubClient`; its applying code stays with the module.
- Diagnostics and quick fixes are hub modules owning their own parser and buffers, answering their requests; `DiagnosticsHandle`, `QuickFixHandle`, and `FsInput` are gone.
- File events and the root become notifications, removing the filesystem driver and `FsInput`.
- The core keeps its synchronous algorithms, with only its index access moved onto the hub — safe because the hub is a thread.
- Behaviour preserved — the same index, diagnostics, and quick fixes, with targeted tests passing.

**Acceptance criteria** — one `HubClient`/router used by all drivers, modules, and the core; notifications reach every module and each request is answered only by its owner; subsystems hold no other module's handle and the retired handles/driver are gone; `cargo build` clean and targeted tests pass.

## Cache the class-file base across warm-ups

`kind: improvement` · `state: done` · `priority: high` · `owner: felix` · finished 2026-09-29

**Problem** — The resolved dependency-jar and JDK class-file base was re-read and re-parsed from scratch on every server start, so warm-up scaled with total class-file count and was paid in full even when nothing changed.

**Proposal** — Persist the parsed non-source base under the existing cache dir, keyed by a schema version and each archive's identity, and re-index only what changed.

**Decisions**

- Cache only the non-source base (jars + JDK), not workspace sources — class-file archives are the repeated, expensive, stable part.
- The cached content is exactly what the parse produced — the cache changes build speed, never the index.
- Serialize with `serde`/`serde_json` (already in the tree), with `rc` for the `Arc`-shared fields — reuses an available dependency.
- Best-effort and safe by default — a key mismatch, corrupt/partial file, or version change falls back to a full parse, with writes renamed into place.

**Acceptance criteria** — a second warm-up over an unchanged workspace re-parses no class files and yields an identical index; a changed archive or schema version is re-parsed and never yields a stale/partial base; a missing/corrupt/partial cache degrades to a full parse without error; the suite is green apart from known sandbox failures and large-fixture warm-up is faster.

## Member completion after a dot is empty for dotted nested-type receivers and partial type names

`kind: bug` · `state: done` · `priority: high` · `owner: felix` · finished 2026-09-28

**Problem** — `.`-completion returned nothing when the receiver was a dotted nested-type name (`Outer.Inner.`) or when the name being typed parsed as a type (`SumType.T…`, `new SumType.T…`), and the tree at HEAD also failed to compile.

**Proposal** — Fix the build, recover the receiver at the dot rather than the cursor, resolve a dotted receiver as a type when it names no member, key `all_types` by name, and complete package-qualified names.

**Decisions**

- All points ship as one request — the `ProjectModel` build fix is a prerequisite, not a separate change.
- The two completion symptoms are one defect family (a dotted name read as a type) split across receiver lookup and type resolution; both are fixed.
- Package-qualified completion is in scope, sharing the dotted-receiver lookup without regressing the nested-type path.
- Refusal stays the failure mode for a receiver whose type genuinely cannot be inferred.
- A type's name is part of its identity in a scan — `all_types`' key omitted it and collapsed same-kind siblings.
- The example's package/directory mismatch is left as-is, being unrelated to the defect.

**Acceptance criteria** — Build and tests compile cleanly; `Greeter.Inner.` offers statics (`CONST`) and `Greeter.Inner.CON…` narrows while `i.` still offers instance members; while typing `Greeter.Inn`, `SumType.T…`, and their `new` forms the qualifier's nested types/members are offered narrowed by prefix; `SumType.`/`SumType.T…`/`new SumType.T…` offer `Type1..Type4` and an instance offers `val`; `java.util.Li` offers `List`; existing controls stay green; two same-kind nested types are both offered; regression tests cover each in `src/analysis.rs` and `src/types.rs`.

## Make every subsystem a message-driven driver on one engine-owned hub

`kind: refactor` · `state: done` · `priority: high` · `owner: felix` · finished 2026-09-28

**Problem** — Discovery, the `Reporter`, and the filesystem each sat outside the engine's claimed universal message boundary.

**Proposal** — Make every subsystem a driver spawned at start that speaks only one engine-owned message module, broadcast by the engine, which alone turns messages into editor reporting.

**Decisions**

- One engine-owned module defines the whole vocabulary (`Command`, hub messages, `EngineEvent`, reporting types) and `Reporter` is deleted — the boundary is defined and changed in one place.
- The engine is a dumb broadcast hub, not a router; each driver self-selects by variant — adding a driver changes neither the engine nor other drivers.
- The engine is the sole translator to the editor, with `apply_message` moved from `src/index.rs` into the engine module — only the engine sends reporting.
- Discovery dissolves: the project driver walks and derives, the dependency driver resolves, and model/files/artifacts ride the hub — removing the last non-message path and the one index mutation outside the writer.
- The filesystem driver emits the root as an added-folder message and relays client watched-file events, never enumerating or OS-watching — no new dependency and the project driver owns the walk.
- The model is dynamic: a folder added after start recomputes and updates its dependents — a correctness property to preserve, not an optimization to chase.
- Behaviour is preserved: identical final index/model, `ready`, summary line, progress/notice surface, and diagnostics gating.
- Drivers run on the runtime; the no-runtime inline path is replaced, leaving the ~160 core unit tests untouched.
- `warmup-throughput` is folded in where subsumed (its scheduling/progress goals) and trimmed to its orthogonal class-file base cache.
- Out of scope: query results and diagnostics algorithm, Maven resolver semantics, request-path latency, OS-level watching, multi-root workspaces.

**Acceptance criteria** — One engine-owned module defines every message with `Reporter` gone; only the engine emits `EngineEvent` and no subsystem talks to the editor; no synchronous `discover_workspace` or direct `set_model` (the index is mutated only by the engine applying a message); the seven drivers are spawned at start; the filesystem driver relays without enumerating; final index/model, `ready`, summary line, progress/notice surface, and diagnostics gating are unchanged; `cargo test --all-targets` green apart from known sandbox failures.

## Source enum members (constants and declared members) are not modelled, so completion, navigation, and member resolution miss them

`kind: bug` · `state: done` · `priority: high` · `owner: felix` · finished 2026-09-28

**Problem** — A source enum's constants and its members declared after the `;` were ignored by the index and type model, so `DataType.` offered nothing and `DataType.TYPE_1` was reported unresolved.

**Proposal** — Model a source enum's members as members of the enum in all three declaration-modelling places (index, type model, scope).

**Decisions**

- A dedicated `IndexKind::EnumConstant` — constants complete as `ENUM_MEMBER` with their own icon rather than being conflated with `Field`.
- A constant's type is its enum and is static — so `DataType.TYPE_1.rank()` chains, matching Java.
- Qualified and unqualified — both `DataType.TYPE_1` and a bare `TYPE_1` inside the enum body resolve.
- Arguments and bodies are kept, indexing a constant by its name only.
- The declaration section is walked — `collect_members` descends into `enum_body_declarations` so an enum's own fields/methods/constructors are modelled.
- Implicit `values()`/`valueOf(String)` are out of scope (synthesized methods, not constants).
- Document-symbol outline entries for constants are out of scope.
- Source enums only — jar/JDK enums already yield members via the class file; `src/classfile.rs` is untouched.

**Acceptance criteria** — `DataType.` offers `TYPE_1`/`TYPE_2` as `ENUM_MEMBER` narrowed by prefix; `DataType.TYPE_1` clears the false diagnostic while `DataType.NOPE` still reports; definition/references/rename and `workspace/symbol` target the constant; a bare constant inside its enum resolves; an argumented/body constant still resolves by name; a member after the `;` completes on an enum value; regression tests cover each in `src/index.rs`, `src/types.rs`, and `src/analysis.rs`.

## Make indexing an incremental, message-based pipeline of producer modules

`kind: refactor` · `state: done` · `priority: high` · `owner: felix` · finished 2026-09-28

**Problem** — Indexing was one monolithic blocking function that published the whole base type model once at the end, with the dependency-source downloader cloning and re-publishing it after the fact.

**Proposal** — Refactor each indexing step (source scan, jar indexer, JDK indexer, dependency-source downloader) into an independent producer feeding an append-only, per-artifact index through the engine's existing message boundary.

**Decisions**

- This is `message-based-engine` applied to indexing — the refactor reuses the existing `EngineHandle`/`EngineEvent` boundary rather than inventing one.
- Producers are discovery, workspace sources, dependency jars, the JDK, and dependency-source download; the interim jars+JDK-before-sources reorder becomes an explicit scheduling decision.
- The base grows append-only per artifact (`add_base_layer`), replacing `set_types`, which also removes the O(total-types) clone — the final index/model must be identical to today's.
- Semantic diagnostics stay gated on an indexed `java.lang` plus a completed source scan, so partial indexing never yields false positives; other features serve partial results.
- The LSP surface and observable end state are preserved (`ready` still means all producers finished); only progress events may be finer-grained.
- This supersedes the interim "publish the base before the source scan" stopgap, making it structural.

**Acceptance criteria** — Indexing is driven by producer messages with the downloader concurrent with the source scan; the base grows per artifact with no whole-model clone in `index_extracted`; library types resolve mid-scan; final index/model identical to before (equivalence test); semantic diagnostics never fire from partial indexing; suite green with no bench regression.

## Index the JDK from jmods so standard-library members complete

`kind: bug` · `state: done` · `priority: high` · `owner: felix` · finished 2026-09-28

**Problem** — Every jmod's ZIP offsets are relative to the byte after its 4-byte `JM` magic, but the reader used them as absolute offsets, so no jmod yielded entries and the whole standard library was silently absent.

**Proposal** — Strip the 4-byte `JM` magic before parsing a jmod so its ZIP offsets index the sliced buffer, detect the magic generically, and make the fixtures realistic.

**Decisions**

- Fix at the jmod reader by slicing off the prefix, not special-casing offsets everywhere — `for_each_zip_entry` is unchanged and prefix-free archives are unaffected.
- Detect by magic prefix (`starts_with(b"JM")`), not by call site — the reader is robust to how it is called.
- Make the tests realistic (fixture carries `JM\x01\x00`) — a magic-less fixture is exactly what hid the bug.
- No behavior change beyond the fix — `java.*`/`javax.*` filtering, entry shapes, and the missing-JDK no-op are unchanged.

**Acceptance criteria** — A JDK 9+ with jmods reports a large `jdk_classes` (was 0); `.`-completion on a standard-library receiver offers its members; suite green apart from known sandbox failures; the jmod unit test uses a `JM`-magic fixture.

## Dotted nested-type references (Outer.Inner) are misread as package-qualified names

`kind: bug` · `state: done` · `priority: high` · `owner: felix` · finished 2026-09-28

**Problem** — A source-written nested type reference like `Outer.Inner` was split as package `Outer` plus type `Inner`, so the type did not resolve (no member completion, no nested-type completion, lost navigation).

**Proposal** — Teach resolution that a dotted name may be a nested type (nested-first, package fallback) and teach completion to list a type's nested types.

**Decisions**

- Nested-first for a dotted name with the package reading as fallback — Java gives no syntactic signal and outer-as-in-scope-type is Java's own rule.
- Reuse the recorded `nested` chain — the model already stores each nested type's enclosing chain, introducing no new identity.
- `$` names keep their current meaning — only `.`-dotted names gain the new path.
- Completion lists nested types on a type receiver (`Greeter.`) but not on an instance receiver, since the receiver is known to be a type (`static_only`).
- Out of scope: local and anonymous classes, and static imports of nested types.

**Acceptance criteria** — `i.getVal()` with `i` of type `Outer.Inner` completes and resolves through the receiver; `Greeter.` offers `Inner` plus the enclosing type's statics; a dotted-nested variable declaration and object creation type-resolve; a dotted jar/JDK nested reference (`Map.Entry`) resolves; plain package-qualified and `$`-form names are unchanged; regression tests cover each in `src/types.rs` and `src/analysis.rs`.

## Keep the request path responsive while diagnostics and references run on a large workspace

`kind: bug` · `state: done` · `priority: high` · `owner: felix` · finished 2026-09-28

**Problem** — On a ~10k-file workspace with many open buffers, typing stalled completion and navigation, diagnostics took seconds, and references was slow or never returned because four defects compounded on the single dispatcher and shared locks.

**Proposal** — Run diagnostics off the dispatcher with per-URI coalescing, stop holding the `documents`/`parser` locks across analysis and whole-workspace searches, and cache the per-request type-layer view.

**Decisions**

- Split responsiveness from warm-up throughput — the scan duration itself is a separate improvement touching different code.
- Fix the causes, not the symptom — all four defects (inline diagnostics, `documents` hold, `parser` hold, `O(N)` per-request overlay) are addressed.
- Preserve behaviour exactly — the published diagnostics and references sets are unchanged, keeping the cross-file republish and rename's refuse-when-incomplete rule.
- Bound references work — no unbounded work while holding any shared lock, via candidate prefiltering and per-file lock release.
- No LSP-surface change — capabilities and protocol are untouched.

**Acceptance criteria** — With `K` open documents and a large index, `didChange` and a concurrent `hover`/`definition` each respond within a bounded time and no handler is blocked for a sweep; a references search on a shared name completes bounded without blocking a concurrent `didChange`; answers are identical to today; suite green with no bench regression; the bench/harness can express the multi-document, typing, and references scenarios.

## Model constructors so new T(...) resolves

`kind: feature` · `state: done` · `priority: high` · `owner: felix` · finished 2026-09-25

**Problem** — `new T(...)` was never resolved because constructors were absent from the declared-type model and dropped from class files.

**Proposal** — Model constructors as first-class parts of a declared type and resolve `new T(...)` against them for signature help, hover, definition, references, and parameter hints.

**Decisions**

- Constructors are a separate collection, not methods — member completion reads methods, so a dedicated collection prevents offering `t.T()` while every other feature treats a constructor as an overloaded member.
- Implicit constructors are synthesized — a no-arg for a class with none, a record's canonical from its components; interfaces and enums contribute none.
- Class-file constructors are modelled, but jar/JDK navigation is not — `<init>` gives library types real signatures, but a library declaration is still never a definition target.
- Diagnostics are out of scope — reporting unknown/wrong-arity constructors is a deliberate follow-up.
- Known limitation: inner-class construction is not fully modelled (the implicit enclosing-instance parameter is not matched), and is documented rather than guessed at.
- Lombok's constructor-generating annotations depend on this and must be implemented after it.

**Acceptance criteria** — Signature help inside `new T(...)` lists overloads and marks the active argument (or `None` when unresolved); definition lands on a source `constructor_declaration` and references returns call sites; implicit no-arg and record canonical constructors resolve, with overload selection by argument types then arity then name; source constructors render parameter-name hints while library ones do not; constructors never appear in `.`-completion, `workspace/symbol`, or as callable; library-constructor definition returns no location; unit tests cover each.

## Go to implementation for types and members

`kind: feature` · `state: done` · `priority: medium` · `owner: felix` · finished 2026-09-25

**Problem** — The server answered most navigation requests but not `textDocument/implementation`, so a developer could not jump from a contract to its workspace implementations.

**Proposal** — Add the implementation provider end to end, reusing the existing `subtype_of`/overlay-model hierarchy machinery.

**Decisions**

- Both a type and a member cursor are supported; a field, constructor, or local cursor answers nothing since those have no implementations.
- A library/JDK type may be the contract owner (e.g. `Runnable`) — every result is still a workspace source declaration, preserving the rule that a jar/JDK declaration is never a navigation target.
- The hierarchy match is name-based, as the model already is — a shared simple name could over-report, accepted rather than re-plumbing edges to carry packages.
- The result is the whole transitive closure — sub-interfaces and abstract intermediates count, only the contract itself is excluded.
- A member override is matched by name and parameter types when the overload is pinned, name-only otherwise; an inherited-only member contributes nothing.
- Dependency and JDK declarations are never results.
- The response is an array of locations or `null` — no `LocationLink`, no partial results.

**Acceptance criteria** — `initialize` advertises `implementationProvider` and requests are answered; a type contract returns every transitively implementing/extending workspace type (excluding the contract itself); a member contract returns every overriding subtype matching the selected overload, excluding inherited-only ones; a library contract returns workspace bindings or `null`; field/constructor/local and unresolvable cursors return `null`; dependency/JDK declarations never appear; unit, model, and harness tests cover each.

## Cut per-request type-model churn and index duplication on large projects

`kind: refactor` · `state: done` · `priority: high` · `owner: felix` · finished 2026-09-25

**Problem** — The declared-type union was deep-copied on every request (and once per candidate file in references/rename), and every index entry was stored twice with its own allocations, so memory and per-request work scaled badly.

**Proposal** — Store workspace types once and read them through a lazy layered `TypeLookup` view over the per-file `Arc<TypeModel>` layers, and share the index's repeated per-file data (`uri`, `package`, `container`) behind `Arc`s.

**Decisions**

- Do both changes, view first — the layered view removes the largest latency/RSS cost and is the prerequisite for referencing rather than merging layers; the index footprint change touches disjoint files and composes.
- A lazy layered view, not a cached merged model — a cache would add a second retained copy, contrary to the goal; the view holds `Arc`s and returns `&TypeInfo` borrowed from a layer.
- Preserve the per-file split and dirty overlay — deletion stays one map removal and unsaved edits still win; only the assembly changes.
- Answers and precedence are preserved exactly — the view reproduces `find_unique`/`find_in_package`/`contains` with the same slot-override rules.
- Share the index's repeated data — `uri: Arc<Url>`, `package: Option<Arc<str>>`, `container: Arc<[String]>`, one `Arc<SymbolEntry>` in both maps; `SymbolEntry` stays crate-internal.
- Make the measurement portable — add a non-Linux RSS read so the improvement can be recorded on macOS.
- No LSP-surface change — everything is crate-internal.
- Document the "why" at the view so the rationale survives and it is not "simplified" back into a merge.

**Acceptance criteria** — Suite green with the layered view answering identically to the merged model (equivalence tests incl. collisions, ambiguity, dirty override, dirty-only file); no per-request model materialization remains and references/rename no longer rebuild the union per file (asserted structurally); index memory holds each entry once and shares repeated data; bench shows lower steady-state/peak RSS with no latency regression; the bench reports memory on macOS and before/after reports are recorded; existing deletion, dirty-override, and diagnostics-republish behaviour is preserved.

## Lombok annotation support

`kind: feature` · `state: done` · `priority: high` · `owner: felix` · finished 2026-09-25

**Problem** — Lombok-generated members did not exist as far as the pure-Rust server was concerned while editing a source type, breaking completion, hover, the unresolved-member diagnostic, and navigation.

**Proposal** — Statically and syntactically synthesize Lombok-generated members (accessors, setters, builders, log fields, constructors) into the source type model and flat index, with no annotation processor.

**Decisions**

- Syntactic detection by simple annotation name — no classpath or `lombok.config` check, since offline resolution could silently disengage the feature and `lombok.config` is ignored and documented.
- One shared synthesis helper in `src/types.rs` — the model and index must agree exactly, so both `collect_type_infos` and `collect_entries` call it.
- Builders are in scope — `@Builder` synthesizes `T.TBuilder`, `builder()`/`toBuilder()`, and an implied all-args constructor; `@SuperBuilder` is deferred.
- Constructors are in scope but depend on the constructors request, so this must land after it.
- Navigation is definition and references, not rename — a generated member has no source of its own, so `rename` keeps refusing.
- `equals`/`hashCode`/`toString` are not synthesized — the model already resolves them via `Object` and keeps them out of completion.
- Annotations that generate no member (`@NonNull`, `@SneakyThrows`, `@Getter(lazy = true)`, etc.) contribute none.

**Acceptance criteria** — Accessors/setters follow field type and `is`/`get` naming; `@Data`/`@Value`/`@With` generate their members; `@Accessors(fluent/chain)` behaves; `@Builder` offers `builder()`, per-field builder setters, and `build()`; `@Slf4j` and friends offer a static `log`; the unresolved-member diagnostic clears on generated members but still fires on unknown ones; definition/references resolve a generated accessor to its field while `rename` refuses; constructor annotations synthesize once constructors lands; an unannotated class is unchanged; unit tests cover each.

## Create-symbol quick fixes for unresolved symbols

`kind: feature` · `state: done` · `priority: medium` · `owner: felix` · finished 2026-09-24

**Problem** — The shipped create-stub fixes were minimal: an unknown type offered only "Create class", bare-identifier stubs ignored the call's arguments and expected type, and there was no way to create a member on a known receiver or a local variable.

**Proposal** — Turn the create fixes into a full set of create-symbol actions: four type-kind actions, a member whose signature is inferred from the usage, and local/field creation with an inferred type.

**Decisions**

- One separate action per type kind (class/interface/enum/record) with a client picker — the language gives no signal for which kind is intended.
- Signatures inferred from usage (argument types for parameters, context for the return type), falling back to `Object`/`void` — a stub that matches its call sites is the difference between a useful action and a chore.
- Parameter names reused from bare-identifier arguments, else `arg1…` — reads better with a deterministic fallback.
- A value use offers local variable (preferred) and field, the local inserted at the innermost block start — the block start precedes every use.
- Create-on-receiver inserts into the workspace type's file as an unversioned `changes` edit — the file is often closed and the rename path already accepts on-disk edits.
- Library (jar/JDK) receivers and packages are excluded — those files are not the user's to edit.
- Create-type actions withheld unless the client advertises `CreateFile` — a new type is a new file and LSP has no other way.
- A create diagnostic carries the fix marker, name, and value/local-field flag; the handler re-reads the node and context to infer the signature — keeps the diagnostics pass cheap and the fix current.
- Enum and record stubs are minimal but valid (`public enum X {}`, `public record X() {}`) — enough to compile; inventing members would be guessing.
- Constructors and enum constants are out of scope — both need context that would be speculative here.

**Acceptance criteria** — Four type actions create a file in the file's own package (absent without `CreateFile`); `foo(a, b)` yields a method with argument-typed, named parameters; an assignment context yields the right return type; a value use offers local (preferred) and field; a workspace-receiver member is created in `T`'s file with no action for library types; every stub is valid Java and clears its diagnostic; unit tests per action and harness coverage of the four type actions.

## Forget a deleted source file in the type model

`kind: bug` · `state: done` · `priority: high` · `owner: felix` · finished 2026-09-24

**Problem** — Deleting a source file dropped its index entries and overlay but left its types merged into the immutable warm-up `TypeModel`, so member completion, hover, signature help, and inlay hints still resolved the deleted type.

**Proposal** — Split the declared-type model into a non-source base (jars and JDK) plus per-source-file models keyed by URI, make the overlay the union overriding per URI, and drop the URI from both on delete.

**Decisions**

- Split into a non-source base plus per-source-file models — only a per-file split can forget one file.
- The overlay is the union of current sources with the dirty entry overriding per URI — create/change/delete all reduce to one map operation.
- A delete removes the URI from both the source models and `dirty` — index and model drifting is exactly this bug.
- No re-scan on delete, just a map delete — a re-scan is O(workspace) per deletion and would block.
- The base narrows to "jars and JDK" — one obvious place for a file's types.
- Watcher and republish rules unchanged — orthogonal and already tested.

**Acceptance criteria** — After deleting a warm-up source, `index.query_name` is empty and no model-based feature resolves its members (completion, hover, signature help, inlay hints); a dirty-only file deleted leaves nothing behind; prior cross-file visibility and republish behaviour preserved; suite green, no new clippy warnings, no bench regression.

## Detect external file changes and keep the analysis model fresh

`kind: bug` · `state: done` · `priority: high` · `owner: felix` · finished 2026-09-24

**Problem** — Files the editor never opened were invisible (no file watcher) and only the changed document was re-analysed, while model-based features read a warm-up-only type model, so cross-file edits reached definition but not completion.

**Proposal** — Register a `**/*.java` file watcher, republish diagnostics for every open document on any index/model change, and build the analysis overlay from all open buffers plus tracked dirty sources.

**Decisions**

- A file watcher (`workspace/didChangeWatchedFiles` for `**/*.java`) is the mechanism for on-disk changes — LSP gives no other signal for unopened files.
- Honour only source-root paths and never override an open buffer — avoids indexing stray files and fighting the editor.
- Republish diagnostics for all open documents on an index/model change — the referring document does not change when another file gains the symbol.
- The overlay covers all open documents, replacing `local_model` — model-based features must agree with definition.
- Track dirty source types per URI; the model is the warm-up base plus the union of dirty sources — replaces one file's contribution without a full re-scan.
- Skip the watcher when the client lacks the capability; the rest of the fix stands — not every client supports it.
- No new dependency and no protocol beyond stock LSP — the fix is protocol plumbing.

**Acceptance criteria** — A watched file's arrival/change clears a referring document's diagnostic without editing it (and yields the import diagnostic when packages differ); a cross-module member becomes completable; other open files' edits are visible to completion/hover/signature help/hints; an open buffer is never overridden; without the capability behaviour is unchanged plus open/close republishing; suite green with no new clippy warnings or bench regression.

## Unresolved-symbol diagnostics and quick fixes

`kind: feature` · `state: done` · `priority: high` · `owner: felix` · finished 2026-09-24

**Problem** — Semantic diagnostics covered only unresolved unqualified type names as warnings, so unresolved members, bare identifiers, and imports went unreported.

**Proposal** — Widen to four `ERROR`-severity families (type references, member accesses, bare identifiers, imports), each carrying `code`/`data` so the shell can offer add-import, did-you-mean, and create-stub quick fixes.

**Decisions**

- All four families at `ERROR` severity — a symbol that does not resolve is as fatal as a syntax error.
- Four resolution outcomes (unknown / known-but-not-visible / unresolved member / unresolvable import) drive the message and fix — the fix follows the cause.
- Trust gate (clean parse plus a model vouching for `java.lang`) carried over — a broken file or missing JDK must never become false positives.
- Unresolved dependencies flagged, not skipped — an honest report is wanted.
- The failed import is flagged once and per-usage diagnostics suppressed — one error, one squiggle.
- Opt-out via `JAVA_LSP_SEMANTIC_DIAGNOSTICS` (`0`/`false` disables) — consistent with the project's env-var configuration.
- New `codeActionProvider`/`textDocument/codeAction` surface with `Command::CodeActions`; diagnostics carry `code`/`data` — keeps the fix aligned with what was reported.
- Add-import offered per candidate, ambiguity left to the client picker, edits from the existing `import_edit` — never guesses a package.
- Create-stub writes a type file or member, gated on the client advertising `CreateFile` — a new type must be a new file.
- Did-you-mean picks the nearest member by edit distance, quiet when none is close — mirrors IDE behaviour.
- Imports checked best-effort, including wildcard and static forms — the user asked for unresolved imports to be red.
- Diagnostics stay off the request path, moved to a spawned task if the bench regresses — R6 is a standing constraint.
- Message wording (`cannot resolve type/member/symbol/import`, `source: "java-lsp"`) fixed as contract — tests and users key on the text.

**Acceptance criteria** — Four families reported at `ERROR`; add-import per candidate; create-class gated on `CreateFile`; did-you-mean rename; a failed import flagged once without double-flagging its uses; syntax/trust/opt-out gating holds; fixes clear their diagnostics on the next change; no bench regression; unit and harness coverage plus `codeActionProvider` (kind `quickfix`) advertised.

## Float literals and parameter hints ignore overload resolution

`kind: bug` · `state: done` · `priority: high` · `owner: felix` · finished 2026-09-23

**Problem** — Inlay parameter hints chose the callee by arity only, and a float literal was typed `double`, so `data.test(5.0f)` could be hinted as and navigated to the wrong overload.

**Proposal** — Type a floating literal by its `f`/`F` suffix, and select the hint's callee from the call's argument types, emitting no hint when the overload cannot be pinned down.

**Decisions**

- The float-literal fix is in scope — the literal's mis-typing is the root cause of the failed resolution, not a separate concern.
- Hints refuse rather than guess — "no result beats a wrong result"; a single same-arity candidate is still used and an unmatchable arity still yields no hint.
- Navigation keeps its arity fallback — this change only makes the types decisive for float literals.
- No new dependency and no shell change — both fixes are inside the type layer and the hint path.

**Acceptance criteria** — With `test(int)`/`test(float)`, `data.test(5.0f)` resolves to `test(float number)` with the hint `number:` while `data.test(5)` still selects `test(int count)`; with no `float` overload it widens to `test(double)`; an inconclusive several-same-arity call gets no hint; `5.0f` is typed `float` and `5.0` `double`; existing navigation and completion are unchanged.

## Maven source download and library navigation

`kind: feature` · `state: done` · `priority: high` · `owner: felix` · finished 2026-09-23

**Problem** — Dependencies were indexed from `.class` files only, so a library symbol had no openable declaration — `definition` and `workspace_symbols` dropped every `dependency` entry and `rename` refused library targets.

**Proposal** — On the background warm-up, reuse or download each dependency's `<a>-<v>-sources.jar` into the local Maven repository, extract and index the sources through the tree-sitter path, and let `definition` open the extracted file.

**Decisions**

- On by default, opt-out via `JAVA_LSP_OFFLINE=1` — the request is that the analyzer fetch sources "on its own".
- All artifacts in the closure eligible — a classpath missing most of a starter's sources is as useless as for completions.
- Local repository first, downloading only what is missing and writing it back in Maven's layout — keeps the cache reusable by `mvn` and other tools.
- Extract to a java-lsp cache (`JAVA_LSP_SOURCES_CACHE`, else `$XDG_CACHE_HOME/java-lsp/sources`) — a definition location must be an ordinary openable `file://` URI.
- Async HTTP with `reqwest` (rustls-tls, default features off) and bounded concurrency, plus `sha1` — downloads must never stall the warm-up thread.
- Repository URL configurable via `JAVA_LSP_MAVEN_CENTRAL_URL`; no credential or `settings.xml` mirror handling in v1 — an unreachable artifact is skipped with a warning.
- Verify the sibling `.sha1` when present — never index corrupted bytes; a missing checksum is not fatal.
- Source entries replace class entries per artifact — otherwise `definition` sees two declarations and, by "no result beats a wrong result", answers nothing.
- A distinct "navigable" flag on `SymbolEntry`, and library-sourced files excluded from `source_files()` — references and rename can never read or rewrite the cache.
- Navigation scope is `definition`, not references or rename — there is nothing useful to search or rewrite in third-party sources.
- Readiness flips before downloads — first open stays as responsive and the existing log line stays comparable for the bench baseline.
- Tests and the bench stay hermetic (local HTTP fixture, `JAVA_LSP_OFFLINE=1` default) — no test reaches Maven Central.
- Supersedes three earlier decisions (offline-only, jars-for-completions-only, and "no external services") — the docs record the change.
- Maven only; Gradle stays out of scope — as in `maven-project-model`.

**Acceptance criteria** — Sources are fetched/extracted/indexed entirely off the request path; source-served dependencies carry real signatures and parameter names; go-to-definition returns an openable location in the cache; references/rename never touch the cache and rename still refuses library targets; offline and no-sources cases degrade to today's behaviour with warnings; the warm-up log line is undelayed and the suite passes with no network.

## Message-based engine boundary

`kind: refactor` · `state: done` · `priority: high` · `owner: felix` · finished 2026-09-23

**Problem** — The shell talked to the analysis backend through a one-directional `SemanticEngine` trait (17 methods) behind a lock, so the engine could never push to the shell — hence pulled diagnostics and an invented progress seam — and it carried the now-unused `SyntaxOnlyEngine`.

**Proposal** — Replace the trait with a `Command`/`EngineEvent` message boundary and an `EngineHandle`, moving the core to a concrete `TreeSitterEngine` in a flattened `src/analysis.rs`.

**Decisions**

- Dispatcher plus concurrent reads, not a strict actor — a strict actor would serialize every request behind the slowest and regress R6.
- `EngineHandle` is the new seam; the trait and stub are removed — the isolation is now expressed by the boundary, the shape a future out-of-process engine would ship across.
- The core stays a plain synchronous type, unit-tested directly — keeps ~160 unit tests meaningful.
- Events use an unbounded channel, commands a bounded one — emitting an event must never stall the dispatcher.
- No behaviour change; a dropped reply yields the empty result the trait used to default to.
- Out of scope: a separate daemon process, request cancellation, and multi-root workspaces.
- Flattened layout (`src/engine.rs`, `src/analysis.rs`), no new dependencies.
- Supersedes the trait-seam decision recorded in `docs/architecture.md`.

**Acceptance criteria** — `SemanticEngine`, `SyntaxOnlyEngine`, and `src/engine/` are gone, the shell holds an `EngineHandle`, every existing test passes, diagnostics are still published on open/change and cleared on close (now via `EngineEvent::Diagnostics`), a slow query cannot block a mutation, the core stays synchronous and directly unit-tested, and the architecture doc describes the boundary with no dangling trait reference.

## Overload-aware completions and signature help

`kind: feature` · `state: done` · `priority: high` · `owner: felix` · finished 2026-09-23

**Problem** — Completion collapsed same-named methods by `(name, is_method)`, so overloads were indistinguishable, and the server never advertised `textDocument/signatureHelp`.

**Proposal** — List each distinct overload separately with its full signature (`name(` on accept) in both the member and workspace-index completion paths, and serve `textDocument/signatureHelp` with `activeParameter`.

**Decisions**

- Both completion paths (member and workspace-index source) list overloads — the index holds no parameter list, so it resolves each overload in the type model by the entry's container type.
- Add an overload-preserving member listing that dedupes by name, kind, and parameter signature — a true duplicate (an override) still collapses while distinct overloads stay separate.
- `label` = `Member::signature()`, `filter_text` = bare name, `insert_text` = `name(`, `detail` = declaring type — accepting opens the argument list so signature help engages, and typing still filters.
- Fields and non-overloaded members unchanged — one item, label = name.
- Ordering stays ranked, same-name items ordered by parameter count — a stable list.
- Signature help is conservative (rendered from the type model; no guess when the callee or receiver is unresolvable), with `activeParameter` counted from the argument list and triggers on `(` and `,`.
- No `SemanticEngine` trait to touch — it rides the channel boundary like every other query.

**Acceptance criteria** — A receiver with `add(int)` and `add(int, int)` offers two full-signature items that insert `add(`; the index source likewise expands overloads; an inherited/own same-signature duplicate still collapses; fields and non-overloaded methods stay single; `signatureHelp` returns the overloads with the correct `activeParameter` (nothing when unresolved) and the shell advertises `signatureHelpProvider`.

## Argument-type overload resolution for navigation

`kind: feature` · `state: done` · `priority: medium` · `owner: felix` · finished 2026-09-23

**Problem** — Definition and references ignored a call's arguments, so same-file overloaded methods collapsed to the first declaration and name-based references reported every overload's call sites together.

**Proposal** — Add an assignability relation to the type layer, select the callee's overload from the call's argument types with arity as the backstop, and use it for definition and references.

**Decisions**

- Argument types first, arity as the backstop — an inconclusive type match keeps the arity-level answer rather than returning nothing.
- `assignable` is a conservative approximation (identity, widening, boxing, `null`, subtyping, arrays, erasure) — an unconfirmable candidate is never preferred over a determinate one.
- Ambiguity refuses, as everywhere else — definition keeps its best-candidate behaviour and references its name-based set, never inventing a location.
- Definition is unified with references through `resolve_target`/`member_target`, then narrowed by the selected overload — the two finally answer the same question the same way.
- `rename` is deliberately left name-group-wide — a partial rename that misses a call site silently breaks code, and method references have no argument list.
- No index schema change — an occurrence's arguments are read from the tree already parsed for the search.

**Acceptance criteria** — `x.add(1)` resolves to `add(int)` and `x.add("a")` to `add(String)`, with references on `add(int)` returning only its call sites; unresolvable arguments fall back to arity and then to the current conservative results; a name spread across files still returns nothing; rename still renames every overload; unit tests cover the conversions and each navigation outcome, with a harness test over an overloaded fixture.

## Warm-up and source-fetch progress reporting

`kind: improvement` · `state: done` · `priority: medium` · `owner: felix` · finished 2026-09-23

**Problem** — Warm-up and the dependency-source fetch reported only to stderr, so an editor showed nothing while the index was built or sources downloaded, and no one could tell whether a long warm-up was progressing.

**Proposal** — Report the background job via `window/workDoneProgress` plus `$/progress` (one status-bar item) and a single `window/showMessage` (Info) when sources are disabled by `JAVA_LSP_OFFLINE` and dependencies resolved, with stderr logging unchanged.

**Decisions**

- Work-done progress, not `showMessage`, for the background job — it is purpose-built for a status-bar item, as rust-analyzer uses; `showMessage` is reserved for the one notice.
- One progress item for the whole job, message per phase, percentage only during the download pass — the job is short-lived and one item is what a user reads.
- Emitted through the message boundary via a small `Reporter` — no new trait or sink abstraction.
- Gated on the client's `window.workDoneProgress` capability — absent or false means nothing is sent.
- The `window/workDoneProgress/create` request is not awaited (fire-and-forget) — awaiting would deadlock a client that never replies.
- Not cancellable in v1 (`cancellable: false`) — the job is not abortable.
- Fetch failures stay in logs, not user-visible messages — avoids nagging on every open.
- The offline notice is narrow (flag set and at least one dependency resolved) — otherwise nothing is shown.
- No behaviour change beyond messaging — same warm-up, results, and non-blocking guarantees.

**Acceptance criteria** — With the capability, opening a workspace produces exactly one progress item (create, begin/report/end with phase counts, a rising download percentage); without it no progress is sent; the offline notice appears only when warranted; stderr logging is unchanged; the full suite passes.

## A dot at line end loses its receiver, and `var` locals frequently infer no type

`kind: bug` · `state: done` · `priority: high` · `owner: felix` · finished 2026-09-22

**Problem** — Completing `receiver.` returned nothing when the next line began a new statement (e.g. a following `var` line), and `var` bindings inferred no type for several common initializers (`Object`-inherited calls, enhanced-for, ternary, array creation, lambda, `switch`, `instanceof`), plus try-with-resources bindings were never collected.

**Proposal** — Recover the dot's receiver from the source text rather than only the tree, teach `collect_locals` the enhanced-for and try-with-resources bindings, extend `receiver_type` to the unhandled initializer shapes, and resolve `java.lang.Object`'s methods for typing without adding them to completion listings.

**Decisions**

- The dot failure is general, not `var`-specific — fixed at the receiver lookup for any following statement.
- `Object` members are resolve-only — available for typing and `var` inference but excluded from `.`-completion lists.
- Refusal stays the failure mode — a genuinely uninferrable initializer yields no type/completion.
- Everything the report surfaced is in scope — enhanced-for, try-with-resources, and the unhandled initializer shapes are one defect.

**Acceptance criteria** — `gson.` with a following `var` (or any new statement) offers `Gson`'s members like the same-line form; `var x = gson.toString();` infers `String` (hint and `x.` members with a JDK); `for (var w : ws) { w. }` infers the element type; both try-with-resources forms offer members; the newly handled initializer shapes infer where a single answer exists; `.`-completion gains no `Object` members; regression tests in `syntax.rs`/`types.rs`.

## Generic type-argument inference

`kind: feature` · `state: done` · `priority: medium` · `owner: felix` · finished 2026-09-22

**Problem** — Resolved generic types rendered without readable arguments (`var x = List.of(3);` showed `List` or `List<E>`, never `List<Integer>`), since `type-aware-engine` deferred substitution.

**Proposal** — Infer and substitute generic type arguments for common cases when rendering a resolved type or looking up a receiver's members.

**Decisions**

- Scope settled to method calls and field reads through a receiver — applied in `receiver_type`; hover/completion keep the written form.
- Overloads picked by arity — new `member_for_call` prefers the overload matching the call's argument count.
- Reuse the existing representation — `Ty::Ref` keeps arguments; work is inference/substitution only.
- Conservative — substitute only a single provable argument type, else keep written/erased form.
- Class-file sources are erased — inference cannot recover `E` from a descriptor; only source/`src.zip` allows substitution.

**Acceptance criteria** — `var x = List.of(3);` renders `List<Integer>` with a `src.zip` or workspace JDK; an uninferrable argument leaves the written form; documented in `docs/architecture.md` and the changelog.

## Import-aware type-name resolution

`kind: bug` · `state: done` · `priority: high` · `owner: felix` · finished 2026-09-22

**Problem** — `resolve_name` ignored the file's imports, so a simple name shared across packages (e.g. `List`, indexed as both `java.util.List` and `java.awt.List`) resolved to nothing, breaking hover, `.`-completions, and hints; qualified references also lost their package in member lookup.

**Proposal** — Resolve simple type names through the compilation unit's imports, and honour a qualified reference's own package during member lookup.

**Decisions**

- Java's resolution order, conservative at the end — exact import, own package, wildcard import, then a unique model match; two ambiguous wildcards stay unresolved.
- Resolved references carry their package — `resolve_name` returns a package-qualified `Ty`, `Ty::display` still shows the simple name.
- Receiver's declared type name qualified through the same rules — so an imported local's members resolve.
- No change when a name is already unambiguous — context-package and unique-match fallbacks preserved.

**Acceptance criteria** — `var x = List.of(3);` with `import java.util.List;` produces a hint resolving to `java.util.List` despite `java.awt.List`; wildcard imports resolve, conflicting wildcards do not; qualified references find their members; an unimported ambiguous name stays unresolved; verified by `src/types.rs` and harness tests over two-package and fake-`src.zip` workspaces.

## JVM member descriptors for library type signatures

`kind: feature` · `state: done` · `priority: medium` · `owner: felix` · finished 2026-09-22

**Problem** — Library (jar/JDK) types entered the model name-only, so hovering a library member showed a bare name and member completion omitted inherited members.

**Proposal** — Parse member descriptors and `super_class`/`interfaces` in `classfile.rs` so library types carry real signatures and supertypes, and route `lib/src.zip` JDK sources through `collect_type_infos`.

**Decisions**

- Follow-up to `type-aware-engine` with the model shape unchanged — only library population changes.
- `java.lang.Object` not recorded as a universal supertype — avoids `toString`/`equals`/etc. noise; explicit edges kept.
- Public class surface preserved — `methods`/`fields` derived from parsed members, `entries_from_jar` a thin wrapper over one combined reader.
- Private/synthetic members stay filtered — a library type never offers unusable members.
- No changes to the LSP shell or trait.
- Per-member signature storage raises peak RSS — measured and recorded.

**Acceptance criteria** — Library members render typed signatures (`String getName()`, `int size`); library member completion includes inherited members; no implicit `Object` supertype; private/synthetic/`<init>`/`<clinit>` members absent; existing index/navigation/tests unchanged; documented with measured memory cost.

## Record components are not modelled, so member completion on a record is empty

`kind: bug` · `state: done` · `priority: high` · `owner: felix` · finished 2026-09-22

**Problem** — A source record's components were ignored by the type model and index, so `.`-completion, hover, and navigation on a record exposed no components or accessors.

**Proposal** — Treat each record component as a member (accessor method to outside receivers, private-field view inside the record) in `src/types.rs` and index each component at its header position in `src/index.rs`, with declaration hover showing the component list.

**Decisions**

- Components are accessors for outside access, not fields — `.`-completion/hover show `x()`, never a private `x` field.
- Full surface in scope — completion, hover, and navigation/rename, not completion alone.
- Source records only — jar/JDK records already work via real class-file accessors.
- `Object`/`Record` members stay out of completion lists — adds only the header's components.

**Acceptance criteria** — Completing `p.` on `record Point(int x, int y)` offers `x`/`y` and narrows by prefix; a component-less record adds nothing; hover of `p.x()` shows a signature and of the declaration shows components; definition/references/rename/`workspace/symbol` target the header component; an unqualified in-record component name resolves; regression tests in `syntax.rs`, `types.rs`, and `index.rs`.

## Find references and rename

`kind: feature` · `state: done` · `priority: medium` · `owner: felix` · finished 2026-09-22

**Problem** — The server could navigate to a declaration but could not answer the reverse query or safely rename, the remaining gap to a refactor-capable server.

**Proposal** — Add `textDocument/references` and `textDocument/rename` over workspace sources: resolve the cursor target (name, kind, declaring type), substring-prefilter then parse candidate `.java` files, filter genuine occurrences via kind rules and the type layer, and build a `WorkspaceEdit` only when the rename is provably safe.

**Decisions**

- Refusal is the failure mode for destructive edits — unresolved/ambiguous target, library declaration, or invalid identifier all refuse.
- Types renamed only where visible — declaring file, same package, or an exact/wildcard import.
- Member references verified through the type layer — receiver's inferred type must resolve the name to the same declaring type.
- Library declarations never renamed — their files cannot be written.
- Locals/parameters file-local, only when exactly one such local is declared.
- New `SemanticEngine::references`/`rename` with defaults, shell advertises both providers.
- `WorkspaceEdit` uses plain `changes` — no versioning guarantee for unopened files.
- On-demand synchronous parsing accepted — user-initiated, rare, prefiltered.

**Acceptance criteria** — `references` returns occurrences (declaration only when `include_declaration`), empty when unpinnable; `rename` returns one `TextEdit` per reference and `None` for library/unresolved/invalid cases; no cross-package or unrelated-type collateral (negative tests); file-local locals correct; shell advertises both providers with the stub conforming; docs and changelog updated.

## Separate completions for same-named symbols

`kind: bug` · `state: done` · `priority: high` · `owner: felix` · finished 2026-09-22

**Problem** — The completion loop deduplicated by insert text alone, so symbols sharing a simple name collapsed into one arbitrary item with a wrong import (`java.awt.List` instead of `java.util.List`), and same-named members on different types likewise merged.

**Proposal** — Keep symbols that share a simple name but are imported differently as separate completion items, each labeled with its owning package.

**Decisions**

- Deduplicate by symbol, not insert text — key on insert text plus carried import, so distinct imports stay separate and one symbol's duplicates collapse.
- A shared name is labeled with its owner — detail names the owner (`interface of java.util`); unambiguous names keep their detail.
- A local still shadows a same-named workspace symbol — preserving existing ranking.
- No guessing — offer the candidates with enough information to tell them apart.

**Acceptance criteria** — Two same-named types in different packages appear as two items with their own import edits and owner details; duplicate index entries collapse; a local and a same-named field still yield one item (negative test); verified by a unit test and against a real JDK.

## Type-aware semantic engine

`kind: feature` · `state: done` · `priority: medium` · `owner: felix` · finished 2026-09-22

**Problem** — Type-aware hover, member completions, and accurate diagnostics had no real type resolution, only syntax and an index.

**Proposal** — A pure-Rust type layer (`src/types.rs`) behind `SemanticEngine` with no LSP-shell changes, delivering type-aware hover, `.`-member completions, and conservative unresolved-type diagnostics.

**Decisions**

- Pure-Rust type checker, rejecting the javac-daemon/GraalVM option — avoids reintroducing a Java codebase and Lombok risk.
- No JVM at runtime — new Rust modules in the existing single crate.
- Conservative by default — no result beats a wrong result; ambiguous/unknown yields nothing.
- Phased with declared non-goals — generics substitution, overload resolution, `var` inference, lambdas, casts, static-import members deferred; type variables opaque.
- Library signatures name-only in this slice — synthesized from flat index entries; descriptor parsing deferred.
- Model built during existing warm-up, off the request path — per-request work bounded to the open document.
- Model keys by simple name with a per-type member list — memory proportional to declared members.

**Acceptance criteria** — Hover renders resolved signatures for workspace symbols (none when unresolved); `.`-completions infer receivers via locals/params/fields/`this`/`super`/`new T()`/chains and offer inherited members; only provably-unresolvable type names warn (imports, same package, type parameters, `java.lang`, indexed workspace/jars/JDK exempt); no `server.rs` changes; verified by tests plus the bench harness; docs updated.

## Type-aware review follow-ups

`kind: bug` · `state: done` · `priority: high` · `owner: felix` · finished 2026-09-22

**Problem** — Reviewing the uncommitted type-aware work found two correctness defects (cross-package member conflation in references/rename, duplicate semantic diagnostics) plus behavioural gaps, coverage holes, and hygiene issues across the type layer, hints, class-file model, harness, and example.

**Proposal** — Fix the two correctness defects first (package-qualified owner identity; single `type_list` expansion), then close the remaining behavioural gaps, add a negative test per fix, harden conservative scope and harness hygiene, clean the example fixtures, and correct the changelog.

**Decisions**

- Kind `bug` — the dominant nature is correcting shipped behaviour despite carrying test/doc/hygiene work.
- Scope: all twenty findings, nothing deferred — splitting or deferring was considered and dropped.
- Refusal stays the failure mode — `rename` returns `None` when the search cannot complete.
- One nested-type naming convention applied to both model and descriptor parsing.
- Noisy build artifacts leave the change — regenerated `target/` and surefire output excluded.
- No new dependencies.

**Acceptance criteria** — Twenty numbered outcomes: no stray test output; no cross-package member conflation; one diagnostic per unresolved interface; `include_declaration` honoured; non-terminal import segments yield no target; nested library types resolve and render in source form without key collisions; `rename` refuses on unreadable/unparsable files; fully-qualified uses counted; shared-simple-name supertypes inherited; exact `: List<Integer>` and chain-hint assertions; remaining coverage gaps closed; valid example with artifacts restored; strict parameter arity; in-range hints only; initializer/iterable not treated as declaration names; diagnostics scope broadened or documented; Java-correct identifier validation; dead arm removed; harness env/temp hygiene; changelog corrections.

## Type and parameter inlay hints

`kind: feature` · `state: done` · `priority: medium` · `owner: felix` · finished 2026-09-22

**Problem** — The type layer computed inferred/declared types but nothing surfaced them as inlay hints, which Java's `var`, diamond, and chains make valuable.

**Proposal** — Add `SemanticEngine::inlay_hints` (variable type, parameter name, and chained-call return-type hints) served on demand from the open document, with an `inlayHintProvider` capability.

**Decisions**

- All three hint families in scope — "type hints" read as `textDocument/inlayHint`.
- `var`/diamond inference pulled forward — reusing `receiver_type` on the initializer, fixing the latent `Ty::reference("var")` behaviour.
- Variable hints cover locals and fields, not parameters — parameter types are already written.
- Conservative — unresolved targets yield no hint.
- `Member.params` becomes `Vec<Param>` with optional names for source-derived types — JVM descriptors carry no names.
- Chained-call hints only for intermediate links — avoids restating every call.
- Range-scoped, on demand, never blocking (R6) — walks pruned to the requested range.
- No lazy resolve, no cache — the inference is the cost, not the deferred tooltip/edits.
- New trait method with a default; shell advertises `inlayHintProvider`.
- Plain labels (`: Type`, `name:`) with no `text_edits`/tooltips.
- No server-push refresh; `workspace/inlayHint/refresh` deferred.

**Acceptance criteria** — `var`/diamond locals show inferred types, explicit locals/fields show declared types, unresolved yields none; source-declared call arguments get parameter names, jar/classfile callees do not; intermediate chain links show return types, outermost/standalone do not; no family requires an indexed JDK; range scoping bounds inference; shell advertises and routes the provider with the stub conforming; unit tests per family plus a harness test.

## Auto-import on completion accept

`kind: feature` · `state: done` · `priority: high` · `owner: felix` · finished 2026-09-09

**Problem** — Completions offered cross-package, cross-module, and jar symbols by bare name with no import, forcing the user to hand-write it.

**Proposal** — Recorded each symbol's package in the index and attached `additionalTextEdits` inserting `import <fqcn>;` to completion items whose symbol lives outside the current file's package.

**Decisions**

- Completion-time only via `additionalTextEdits` — no new client capability; "add import" quick-fix deferred.
- Workspace symbols across packages and modules are first-class — both flow through the same FQCN machinery.
- FQCNs recorded at index time (package per source file, internal name per jar class) — not recomputed per request.
- Never worsen — a conflicting simple name suppresses the edit entirely.
- `.*` wildcard imports suppress the edit; a `static` import does not make a type nameable.

**Acceptance criteria** — Accepting a cross-package/module/jar completion adds the correct import at the correct position (after last import, else package, else top) with unchanged insert text; no edit for keywords, locals, same-file, same-package, already-imported, wildcard-covered, conflicting, or default-package names; identical for jar and workspace entries with no new client capability; architecture doc updated.

## Standard library (JDK) indexing

`kind: feature` · `state: done` · `priority: high` · `owner: felix` · finished 2026-09-09

**Problem** — The standard library (`String`, `List`, `Map`, `ArrayList`) was invisible to completions, recorded as "JDK/library imports are not indexed".

**Proposal** — Indexed `java.*`/`javax.*` declarations from the installed JDK's `$JAVA_HOME/jmods/*.jmod` (with `rt.jar` fallback) through the existing jar pipeline, offered in completions with auto-import edits.

**Decisions**

- Class files from jmods (rt.jar fallback), not src.zip — the class-file reader already exists and declaration fidelity matches jar handling.
- `java.*`/`javax.*` scope — excludes internals (`jdk.*`, `com.sun.*`, `sun.*`) and module/package-info.
- `$JAVA_HOME` with heuristic + `$JAVA_LSP_JDK` override — zero configuration with a documented no-op fallback.
- `java.lang` needs no import — implicitly imported (never-worsen extension).
- Measured, not guessed — bench records memory/warm-up impact.

**Acceptance criteria** — JDK types and members appear in completions with imports for non-`java.lang` symbols and none for `java.lang`; internals and module/package-info absent; missing/unusable JDK degrades to today's behaviour with no client errors; definition/`workspace/symbol` still return nothing for JDK symbols; warm-up time and peak RSS measured with request latency non-blocking (R6); architecture doc updated.

## Maven project model and dependency indexing

`kind: feature` · `state: done` · `priority: high` · `owner: felix` · finished 2026-09-09

**Problem** — The server treated the workspace as an unstructured pile of `.java` files with no source roots, modules, or external dependencies.

**Proposal** — Added a statically parsed Maven project model: `pom.xml` discovery computing module source roots, plus offline resolution of each module's full dependency closure and indexing of the resolved jars' `.class` files.

**Decisions**

- Maven only; Gradle deliberately excluded — one build system done honestly.
- Static parsing only, never invoke `mvn` — keeps the server JVM-free and startup instant.
- Full dependency resolution from day one (parents, properties, depMgmt/BOM imports, transitivity, mediation, exclusions, optionals) — a partial classpath makes completions useless.
- Offline, local repository only (`~/.m2/repository`, path overridable) — missing artifacts pruned with warnings.
- Dependency scopes ignored in v1 — accepted noise.
- Jars feed completions only; definition and `workspace/symbol` stay source-only — jar locations are not openable.
- Hand-rolled minimal class-file reader; new deps `flate2` (pure-Rust) and `roxmltree` — minimal surface.
- No file watching — `pom.xml`/jar changes apply on restart.
- Opens milestone v0.2; type-aware engine (R7) pushed behind it.

**Acceptance criteria** — Scan covers exactly the declared source roots (multi-module, `<build>` overrides honored, `target/` excluded); full declared closure resolves statically with Maven-style mediation and exclusions/optionals; declared dependency types appear in completions; unresolvable artifacts skipped with warnings and nothing fetched; build tool never invoked; indexing off the request path (R6, bench-verified on a Maven fixture); requirements/architecture docs updated.

## Completions v1

`kind: feature` · `state: done` · `priority: high` · `owner: felix` · finished 2026-09-08

**Problem** — Typing support is the most-wanted day-to-day feature, and v0.1 could deliver a useful version without type resolution (R4, UC2).

**Proposal** — Implemented completions as three ranked sources: language keywords, locals/parameters in scope from the document tree, and workspace symbols from the index, with correct kinds and insert text.

**Decisions**

- No member-access resolution in v1 — completions after `x.` deferred until a type-aware engine exists.
- Sources labeled "type-free" — user expectations match reality.
- Ranking encoded in `sort_text` prefixes — clients re-sort alphabetically.

**Acceptance criteria** — Completion inside a method body offers keywords, in-scope locals/parameters, and matching workspace symbols; results arrive quickly and never block on warm-up (empty/partial while warming); no wrong-membership claims after a dot.

## LSP shell skeleton

`kind: feature` · `state: done` · `priority: high` · `owner: felix` · finished 2026-09-08

**Problem** — Nothing existed; every feature needed a server speaking LSP over stdio, tracking document state, and separating the editor layer from a future analysis backend (R1, R2).

**Proposal** — Created the cargo crate and skeleton: a `tower-lsp` stdio server with lifecycle and incremental document sync, plus the `SemanticEngine` trait and an empty `SyntaxOnlyEngine` stub.

**Decisions**

- Rust with `tower-lsp` over `lsp-server` — ergonomics first, revisit if cancellation/backpressure suffer.
- stdio transport only — universal client support.
- Trait seam from the first commit — agreed architecture principle.

**Acceptance criteria** — Starts as a stdio binary and completes the LSP handshake with Zed/Neovim/VS Code; incremental `didChange` applied to a versioned store; stub queries respond empty; graceful shutdown on `shutdown`/`exit`; test harness drives the server without an editor.

## Navigation v1

`kind: feature` · `state: done` · `priority: medium` · `owner: felix` · finished 2026-09-08

**Problem** — A developer needs to jump to declarations, but without type resolution navigation must still be accurate and never silently wrong (R5, UC3).

**Proposal** — Implemented go-to-definition over the workspace index (same-file declarations, import targets, same-name matches) and `workspace/symbol` as an index prefix query.

**Decisions**

- Honest limitation: type-inference-dependent references (e.g. `x.foo()`) may be unresolved in v1 — deferred to `type-aware-engine`.
- Ambiguity policy: no result beats a wrong result.
- Best-candidate narrowing (node kind, single-URI) — makes "best candidate or no result" concrete.
- No gating on `index_ready()` — warm-up queries contribute partial results, never block (R6).

**Acceptance criteria** — Go-to-definition works for class names, import targets, and reachable method/field declarations; `workspace/symbol` finds types and members by prefix across the workspace; ambiguous cases return no location and the limitation is documented.

## Performance benchmark harness

`kind: feature` · `state: done` · `priority: medium` · `owner: felix` · finished 2026-09-08

**Problem** — "Project open → responsive" is a standing requirement (R6) with no numeric targets or instrument, so regressions would only be noticed by feel.

**Proposal** — Built a generator for a configurable-size fixture Java project and a harness that starts the real server against it, measuring first-response times, warm-up latency, and memory, recording per-milestone baselines.

**Decisions**

- No fixed numeric targets — measure, document baselines, keep regressions visible.
- Generated fixture — any workspace size exercisable.
- Separate bin, not a `[[bench]]` target — preserves the agreed single-command shape.
- Real stdio binary, not in-process `LspService` — includes process startup, framing, and dispatch.
- Warm-up detection via the server's own readiness log line — deterministic, yields scan stats.
- Memory via the child's peak RSS (`VmHWM`/`getrusage`) — no external tool, `n/a` where unavailable.
- No `criterion` — end-to-end cold-start numbers, not hot-loop microbenchmarks.
- Duplicated minimal JSON-RPC client — tests can't import a bin target; revisit if a third driver appears.

**Acceptance criteria** — A single command produces a timing/memory report for a chosen fixture size; a baseline report exists per shipped milestone and is recorded in the changelog; syntax features demonstrably respond while indexing runs.

## Syntax features via tree-sitter

`kind: feature` · `state: done` · `priority: high` · `owner: felix` · finished 2026-09-08

**Problem** — The server needed immediate per-file value independent of any semantic engine, with parse errors the cheapest available diagnostics (R3, UC1, UC4).

**Proposal** — Integrated `tree-sitter` with `tree-sitter-java`, keeping one tree per open document to serve document symbols, folding ranges, semantic tokens, and parse-error diagnostics.

**Decisions**

- `tree-sitter-java` over a hand-written parser — mature, incremental, error-recovering; custom lossless parser deferred.
- Per-document trees only, no workspace-wide reparsing on a single edit.
- Tree cache with full reparse per change — incremental `InputEdit` parsing deferred; full file parses are microseconds.

**Acceptance criteria** — Opening a Java file yields correct document symbols, folding ranges, and semantic tokens; parse errors appear as diagnostics and clear when fixed; edits update only the affected document.

## Non-blocking workspace symbol index

`kind: feature` · `state: done` · `priority: high` · `owner: felix` · finished 2026-09-08

**Problem** — Completions and navigation need workspace-wide symbols, but per-request scanning would be O(project) and block the request path (R4, R5, R6).

**Proposal** — A background indexer walking `.java` files, extracting declarations from tree-sitter ASTs into an in-memory index with per-file incremental updates, reporting features unavailable until warm-up completes.

**Decisions**

- Pure Rust, in process — this is the v0.1 engine.
- Declarations only, no type resolution — type-aware work postponed (see `type-aware-engine`).
- Built off the request path — opening a project never waits for the scan.
- Close re-reads the file from disk — disk truth wins; no file watcher in v1.

**Acceptance criteria** — Index construction never delays sync or any request (verified with `perf-benchmarks`); edits update only the affected file's symbols; memory stays proportional to workspace size.
