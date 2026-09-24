---
type: ChangeRequest
kind: refactor
title: Cut per-request type-model churn and index duplication on large projects
description: Stop rebuilding the declared-type union by copy on every request and stop storing each index entry twice, so memory and per-request work stop scaling with workspace size.
state: done
priority: high
tags: [dev, refactor, performance, memory]
owner: felix
verified:
  by: cargo test --all-targets (278 passed) + cargo clippy --lib (15 warnings,
    unchanged) + java-lsp-bench --files 500 --methods-per-class 10 (peak RSS
    51504 -> 43648 KiB, post-warm-up hover RTT 0.730 -> 0.163 ms, warm-up hover
    max RTT 1.152 -> 0.256 ms)
  at: 2026-09-25T18:23:30Z
---

# Context

The declared-type union is not accidental — three earlier requests forced its shape,
and this refactor keeps all three:

- **Cross-file resolution** — a query on one file must resolve a type declared in
  another, so the model a query sees has to cover every workspace source
  (`external-change-detection` D4).
- **Unsaved-edit freshness** — an open buffer, or a file a watcher re-read, must
  override its on-disk (warm-up) model so an edited signature is visible everywhere
  without a re-scan — the dirty overlay (`external-change-detection` D5).
- **Cheap, correct deletion** — the model is stored one per source URI _because_ a
  merged, name-keyed model cannot forget a single file; deletion is a map removal,
  never a re-scan (`deleted-file-stays-in-type-model` D1/D4).

So the storage is deliberately per-file and the query view deliberately spans all of
it. The cost is only in _how_ that view is assembled today — `overlay_model()`
materializes the union by deep-copying every layer, on every call. It read as free in
the bench (`deleted-file-stays-in-type-model`: "the bench shows this is invisible
(post-warm-up hover RTT 0.1 ms), so no cache was added") because the fixture opens one
small document. The layered view keeps the design and removes the copying.

# Problem

Two independent structures make memory — and the work done per request — scale badly
on large workspaces. Both were invisible in the bench because its fixture opens one
small document.

**The declared-type union is rebuilt by copy on every request.** Type-aware features
need the union of the non-source base, every workspace source file's model, and the
dirty override. Rather than reference those layers, `TreeSitterEngine::overlay_model`
(`src/analysis.rs`) allocates a fresh `TypeModel` and deep-copies every source file's
`TypeInfo`s — and their members, parameters, types, and strings — into it, once per
request. `hover`, `completions`/`member_items`, `signature_help`, `inlay_hints`,
`diagnostics`, `code_actions`, `definition`, `implementation`, and `resolve_target`
all pay it; and `collect_occurrences` (`TargetKind::Local`) plus
`collect_member_occurrences`/`collect_constructor_occurrences` call it **once per
candidate file**, so one find-references or rename rebuilds the whole union O(files)
times. That transient allocation is simultaneously the peak-RSS and the CPU cost on a
large project — the opposite of keeping response times down.

**Every index entry is stored twice, each copy with its own allocations.**
`IndexState` (`src/index.rs`) keeps `files` and `by_name`, both holding full
`SymbolEntry` clones. Each entry owns `uri: Url` — the same file path re-allocated for
every declaration in the file — plus `package: Option<String>` (the same package
repeated across the whole file), `container: Vec<String>`, and `name: String`.
`query_name`/`query_prefix`/`all_symbols` additionally deep-clone entries per call, and
`query_name` is called many times per file inside the diagnostics pass.

# Proposal

Keep the design and fix the cost. Store the workspace types once and let queries read
them through a **lazy layered view**: a `TypeLookup` that holds references to the base
`Arc<TypeModel>`, each source file's `Arc<TypeModel>`, and the dirty override, and
answers `find_unique`/`find_in_package`/`contains` in precedence order while allocating
only itself — no copy of the data, per request or per file. The per-file split (so a
deleted file is forgotten by a map removal) and the dirty overlay (so unsaved edits
win) are untouched. Separately, share the index's repeated per-file data — `uri` as
`Arc<Url>`, `package` as `Option<Arc<str>>`, `container` as `Arc<[String]>` — and keep
one shared entry in both maps, so the queries hand back handles instead of deep copies.

# Decisions

- **D1 — Do both changes, view first.** Option A (the layered type view) removes the
  largest latency and peak-RSS cost and is the prerequisite for referencing rather than
  merging the `Arc<TypeModel>` layers; option B (the index footprint) removes the
  steady-state RAM that dominates on a very large workspace. They touch disjoint files
  (`analysis.rs` vs `index.rs`), so they compose; A lands first, B follows in the same
  request.

- **D2 — A lazy layered view, not a cached merged model.** A cached merged `TypeModel`
  would remove the per-call cost but add a second retained copy of all workspace source
  types, raising steady-state memory — contrary to the goal. The view allocates only
  small handles and keeps the layers as the single copy. This is possible without
  copying anything: every layer already lives behind an `Arc<TypeModel>` (a stable
  address), so the view can hold the `Arc`s and return `&TypeInfo` borrowed straight
  from a layer. `Arc<TypeModel>` alone is not sufficient on its own, though: the
  per-file models are already `Arc`-wrapped, but `merge` copies their _contents_, so
  the benefit only materializes by referencing the layers instead of merging them.

- **D3 — Preserve the per-file split and the dirty overlay.** Storage stays one model
  per URI (so a deletion is still one map removal, never a re-scan — the
  `deleted-file-stays-in-type-model` D1/D4 rules) and the dirty entries still override
  their URI's warm-up model so unsaved edits are visible cross-file
  (`external-change-detection` D4/D5). Only the assembly changes, not the storage.

- **D4 — Answers and precedence are preserved exactly.** The view reproduces
  `TypeModel::find_unique`/`find_in_package`/`contains` over the layers with the same
  overriding rules: a later layer wins a slot by `(name, package, kind, nested)`, and
  `TypeQuery` still prefers the overlay over the base. This is a storage/assembly
  refactor; no query answers may change.

- **D5 — Share the index's repeated data; `SymbolEntry` stays crate-internal.**
  `uri: Arc<Url>` (one allocation per file, shared by its declarations),
  `package: Option<Arc<str>>` (one per package), `container: Arc<[String]>`, and one
  `Arc<SymbolEntry>` stored in both `files` and `by_name`. `SymbolEntry` is not public
  API (only the crate and its tests construct it), so this is contained; the
  `WorkspaceIndex` query signatures may hand back shared handles instead of owned
  clones.

- **D6 — Make the measurement portable.** The bench reads `/proc/<pid>/status`
  (`src/bin/java-lsp-bench.rs`), which is Linux-only, so it reports `memory: n/a` on
  macOS and the improvement could not be recorded here. Add a non-Linux RSS read (so
  before/after numbers can be captured on this machine) while keeping the Linux path.

- **D7 — No LSP-surface change.** Everything here is crate-internal; the shell, the
  protocol, and the advertised capabilities are untouched.

- **D8 — Document the "why" at the view.** The layered view (and the engine helper
  that builds it) carries a short comment stating why the union is a view over the
  per-file layers — cross-file resolution, dirty override, forgettable per-file
  storage (see `# Context`) — so the rationale survives being read in isolation and the
  view is not "simplified" back into a merge later.

# Acceptance criteria

- `cargo test --all-targets` is green, and the layered view answers **identically** to
  the current merged model — equivalence tests cover `find_unique`, `find_in_package`,
  and `contains`, including a same-name/different-package collision, a
  same-name-in-one-file ambiguity, a dirty entry overriding its warm-up source, and a
  dirty-only file created after warm-up.

- No per-request model materialization remains: the merge-on-call in `overlay_model` is
  gone, and find-references/rename no longer rebuild the union per candidate file
  (asserted structurally, not just by timing).

- Index memory holds each entry once and shares the repeated `uri`/`package`/`container`,
  so `query_name`/`query_prefix` return shared handles rather than deep clones.

- Bench before/after on a large fixture (`java-lsp-bench --files 500
--methods-per-class 10`): steady-state and peak RSS are lower than the recorded
  baseline, with **no regression** in per-feature first response or warm-up hover RTT.

- The bench reports memory on macOS (no longer `n/a`), and both the before and after
  reports are recorded in the changelog entry.

- Existing behaviour is preserved: a deleted file is still forgotten, unsaved edits are
  still visible cross-file, and diagnostics still republish for referring documents.

# Docs to update

- `docs/architecture.md` — the type-layer bullet (the model is read through a layered
  view over the per-file models rather than re-merged per request) and the
  workspace-index bullet (entries are shared, not stored twice).
- `docs/dev/backlog/perf-benchmarks.md` — its "memory via `/proc/<pid>/status` ...
  Linux-specific ... `n/a` elsewhere" note now has a portable path.
- `docs/dev/backlog/index.md` — this request's row.
- `docs/dev/changelog.md` — an entry at implementation, quoting the before/after bench
  numbers.

# Implementation plan

## Approach

Two independent changes plus a measurement change, all crate-internal (D7). A lands
first, then B, so each is verified on its own.

**Bench memory read (D6).** `src/bin/java-lsp-bench.rs` reads `/proc/<pid>/status`, which
does not exist on macOS, so `read_memory` returns `None` there. Add a non-Linux branch
that reads the child's RSS with `ps -o rss= -p <pid>`, and a lightweight sampler thread
that polls RSS (both platforms) and keeps the maximum, so a portable peak is reported
alongside the Linux `VmHWM`. Run the bench before the code changes to capture the
baseline.

**A — lazy layered type view (`types.rs`, `analysis.rs`).** Add a `ModelLayers` view in
`types.rs` holding `Vec<Arc<TypeModel>>` (layers in increasing precedence) that
implements `TypeLookup` by consulting the layers directly: `contains` short-circuits on
any layer, and `find_unique`/`find_in_package` delegate to the single layer that declares
a name, falling back to a slot-deduplicated candidate set only when several layers
declare it — reproducing `TypeModel`'s logic exactly (D4). It exposes `types()` for
`implementation`, deduplicating across layers. `TypeQuery`'s `overlay` becomes
`&'a dyn TypeLookup`. In `analysis.rs`, `dirty` becomes `HashMap<Url, Arc<TypeModel>>`
and `overlay_model` becomes `type_layers() -> ModelLayers`, which selects each source
file's dirty model if present else its warm-up model, appends dirty-only files, and
returns the view — no merge, no copy (D2/D3). `TypeQuery::new(base, &overlay)` call sites
are unchanged; `semantic_diagnostics` takes `&dyn TypeLookup`; `implementation` uses
`layers.types()`.

**B — shared index entries (`index.rs` and consumers).** `SymbolEntry.uri` becomes
`Arc<Url>`, `package` `Option<Arc<str>>`, `container` `Arc<[String]>`; `IndexState.files`
and `by_name` hold `Arc<SymbolEntry>` (one allocation per entry, shared by both maps);
`query_name`/`query_prefix` return `Vec<Arc<SymbolEntry>>`. `extract_entries` shares one
`Arc<Url>` per file and one `Arc<str>` package across its entries; the class-file and JDK
producers share their `jar_url`/package the same way. `all_symbols` (the test hook) keeps
its owned return. Consumers (`analysis.rs`, tests) switch `Vec<SymbolEntry>` to
`Vec<Arc<SymbolEntry>>`; field reads are unchanged through `Deref`.

## Steps

- [x] Bench (`src/bin/java-lsp-bench.rs`): add a `ps`-based RSS read and a max-RSS
      sampler for non-Linux, and report a portable peak; verify memory prints on macOS.
      (D6)
- [x] Record the "before" bench report (RSS + latency) at `--files 500
  --methods-per-class 10` as the baseline. (AC)
- [x] `types.rs`: add `ModelLayers` (`TypeLookup` over `Vec<Arc<TypeModel>>`, plus
      `types()`); change `TypeQuery::overlay` to `&dyn TypeLookup`; add equivalence
      tests against `TypeModel`. (D2/D4)
- [x] `analysis.rs`: `dirty` to `HashMap<Url, Arc<TypeModel>>`; `overlay_model` →
      `type_layers() -> ModelLayers`; `semantic_diagnostics` takes `&dyn TypeLookup`;
      `implementation` uses `layers.types()`. (D2/D3/D8)
- [x] `cargo test --all-targets` green and `cargo clippy` clean after A. (AC)
- [x] `index.rs`: `SymbolEntry` fields `Arc`-shared; store `Arc<SymbolEntry>` in
      `files`/`by_name`; `query_name`/`query_prefix` return `Vec<Arc<SymbolEntry>>`;
      share one uri/package per file in `extract_entries`. (D5)
- [x] `classfile.rs`/`jdk.rs`/`sources.rs`: share the `jar_url`/package across a jar's
      entries. (D5)
- [x] `analysis.rs` + tests: adapt consumers to `Arc<SymbolEntry>`. (D5)
- [x] `cargo test --all-targets` green and `cargo clippy` clean after B; run the "after"
      bench and compare against the baseline. (AC)
- [x] `docs/architecture.md`: the type-layer and workspace-index bullets (layered view;
      shared entries). (Docs)
- [x] `docs/dev/backlog/perf-benchmarks.md`: note the portable memory read. (Docs)
- [x] `docs/dev/backlog/index.md` row → `done`, and a `docs/dev/changelog.md` entry with
      the before/after numbers. (Docs)

## Implementation notes

- The bench's `ps`-based RSS read had to be dropped: this environment's sandbox
  denies executing `ps` (`Operation not permitted`), so macOS peak RSS comes from
  `getrusage(RUSAGE_CHILDREN)` instead — a syscall, no subprocess. It is the
  kernel's peak for the reaped child, so it is read after `wait` (the
  pre-shutdown read sees no reaped child yet). Final/steady RSS stays Linux-only.
- `ModelLayers` reproduces `TypeModel::find_unique`/`find_in_package` exactly:
  `contains` short-circuits on any layer, and `find_unique`/`find_in_package`
  delegate to the one declaring layer in the common case and fall back to a
  slot-deduplicated candidate set only when several layers declare a name. Four
  equivalence tests compare it against a merged model (same name across packages,
  later-layer shadowing, and a dirty-only layer).
- The references/rename path built the model once per candidate file; it now
  builds the view once per search (`collect_occurrences`) and passes
  `&dyn TypeLookup` to `collect_member_occurrences`/`collect_constructor_occurrences`
  (which gained an `#[allow(clippy::too_many_arguments)]`, matching the existing
  `collect_*_nodes` helpers). `TypeModel::merge` is now used only by tests.
- `SymbolEntry` is crate-internal, so the `Arc` fields are contained;
  `query_name`/`query_prefix` return `Vec<Arc<SymbolEntry>>`, while `all_symbols`
  (the test hook) still returns owned entries.
- Bench before/after at `--files 500 --methods-per-class 10` on this machine: peak
  RSS 51504 -> 43648 KiB; post-warm-up hover RTT 0.730 -> 0.163 ms; warm-up hover
  max RTT 1.152 -> 0.256 ms. No JDK was indexed here, so the base model is empty
  and the source layers dominate — the case this change targets.
- Implementation is **uncommitted**, left for review; no commit or branch was
  created, so no commit is linked in the body.
