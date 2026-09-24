---
type: ChangeRequest
kind: improvement
title: Publish each dependency's sources as one base artifact, not one per file
description: The source downloader emits a BaseArtifact per extracted .java file — flooding the bus, the hub log, and the base with thousands of messages/layers; emit one per artifact, as the documented model and the jar/JDK indexers already do.
state: done
priority: low
tags: [dev, improvement, indexing, observability, sources]
owner: felix
verified:
  by: cargo build clean; cargo fmt --check clean; the new `sources::tests::a_sources_jar_is_published_as_one_base_artifact` passes (a two-file jar publishes exactly one `BaseArtifact`, with both files' entries present), and the existing `bus::tests` still pass. The other `sources` tests could not run here: they bind a loopback `TestServer`, which this sandbox forbids (the known environmental `PermissionDenied` failures noted in earlier requests); they are otherwise unaffected (only the `index_extracted` call was updated to the sink form).
  at: 2026-09-30T00:00:00Z
---

# Problem

`src/sources.rs::index_extracted` publishes a `DriverMessage::BaseArtifact` **once
per extracted `.java` file**, keyed by that file's URI:

```rust
for (uri, entries, infos) in files {
    files_indexed += 1;
    let mut types = crate::types::TypeModel::new();
    types.extend(infos);
    let _ = bus.notify(DriverMessage::BaseArtifact {
        uri,                                   // the source file's URI
        entries: Arc::new(entries),
        types: Arc::new(types),
    });
}
```

Fetching one dependency's sources (guava, say) therefore produces one bus message,
one base layer, and — since the hub logs every message at `debug` — one
`sender=download notify BaseArtifact <file> entries=N` line **per source file**.
That contradicts the documented model in two places: the `WorkspaceIndex.base`
doc (`src/index.rs`) and `docs/architecture.md` both say the non-source base is
"one layer per artifact URI", and the sibling producers `index_jars` and
`index_jdk` emit exactly one `BaseArtifact` per archive. `index_extracted` is the
outlier.

Impact:

- **The hub log floods.** An observer turns on `java_lsp::bus=debug` to watch the
  message flow and instead gets thousands of per-file lines, hiding the flow it
  exists to show. (This predates naming the sender; naming it just makes the
  origin obvious.)
- **Every message is fanned out.** The hub clones each `BaseArtifact` to every
  module (index, diagnostics, quickfix).
- **The base grows a layer per file.** `type_model()` assembles a
  `SourceLayerIndex` across all of them — thousands of layers for one dependency
  with sources.

# Proposal

Accumulate every extracted file's entries and declared-type infos and publish a
**single `BaseArtifact` for the whole artifact**, keyed by the **sources-jar URI**;
the per-artifact `RemoveBase` for the class-jar layer is still emitted first.
Entries keep their per-file source URIs, so `definition`, hover, and inlay hints
are unchanged. This drops the messages, hub lines, and base layers from one per
file to one per artifact.

While here, give `index_extracted` the same message sink as the other producers
(`index_jars`, `index_jdk` take `&mut dyn FnMut(DriverMessage)`) so the
granularity is observable in a test.

# Decisions

- **D1 — One layer per artifact, keyed by the sources-jar URI.** The class-jar
  URI cannot be the key: the downloader calls `remove_base_layer` for it, which
  records it in `BaseLayers::superseded`, and `add_base_layer` then _drops_ any
  layer under that URI. The sources-jar path is a stable, unique artifact identity
  that is not superseded. Reason: matches the documented "one layer per artifact
  URI" and the jar/JDK indexers.
- **D2 — Entries keep their per-file URIs.** Only the layer key changes; each
  `SymbolEntry` still carries its extracted file's URI, so navigation and hover
  resolve to the exact file. Reason: preserves the user-visible behaviour.
- **D3 — Entries and type infos are merged across the artifact's files.** One
  layer needs one `Vec<SymbolEntry>` and one `TypeModel`; within-artifact order is
  the archive's file order, which is what the previous per-file layer order was.
- **D4 — `index_extracted` takes a message sink.** It becomes
  `&mut dyn FnMut(DriverMessage)` like `index_jars`/`index_jdk`; `index_sources`
  passes a bus-backed closure. Reason: consistency with the other producers, and
  it lets a test count the published messages.
- **D5 — No architecture/doc change.** `docs/architecture.md` and the
  `WorkspaceIndex.base` doc already say "one layer per artifact URI"; this brings
  the code in line with the doc rather than changing documented behaviour. Only
  the changelog is added.
- **D6 — Out of scope.** The per-file `SourceFile` notifications from the
  workspace warm-up scan are a different producer (one per workspace file, a far
  smaller and bounded set) and are left as they are.

# Acceptance criteria

1. `index_extracted` publishes exactly one `BaseArtifact` per artifact that has
   extracted sources (and none when a jar yields no files), keyed by the
   sources-jar URI, with one `RemoveBase` for its class-jar layer first.
2. The published entries are unchanged: same per-file `uri`, `dependency` and
   `library_source` flags, and ranges; a library type still resolves to its
   extracted source file.
3. A test drives `index_extracted` over a sources jar with several files and
   asserts exactly one `BaseArtifact` message is published.
4. `cargo build` and `cargo fmt --check` are clean and the `sources` tests pass.

# Docs to update

- `docs/dev/backlog/index.md` — this request's row.
- `docs/dev/changelog.md` — an entry at implementation.
- (`docs/architecture.md` needs no change — it already says one layer per
  artifact URI; this change makes the code match it.)

# Implementation plan

## Approach

`src/sources.rs` only.

**The granularity (`index_extracted`).** Collect each file's entries and infos as
today, but publish one merged layer per artifact:

```rust
if files.is_empty() {
    continue;
}
let Ok(sources_uri) = Url::from_file_path(sources_jar_path(repo, group, id, version)) else {
    continue;
};
if let Ok(class_uri) = Url::from_file_path(class_jar_path(repo, group, id, version)) {
    let _ = sink(DriverMessage::RemoveBase { uri: class_uri });
}
let mut entries_all: Vec<SymbolEntry> = Vec::new();
let mut types = crate::types::TypeModel::new();
for (entries, infos) in files {
    files_indexed += 1;
    entries_all.extend(entries);
    types.extend(infos);
}
let _ = sink(DriverMessage::BaseArtifact {
    uri: sources_uri,
    entries: Arc::new(entries_all),
    types: Arc::new(types),
});
artifacts_indexed += 1;
```

The `files` vec drops its now-unused outer `Url` (each entry already carries its
file URI), becoming `Vec<(Vec<SymbolEntry>, Vec<TypeInfo>)>`.

**The sink (`index_extracted` signature).** Change the parameter from
`bus: &crate::bus::BusClient` to `sink: &mut dyn FnMut(DriverMessage)` and replace
every `bus.notify(m)` with `sink(m)`. `index_sources` builds the closure:

```rust
let extract_bus = bus.clone();
let _ = tokio::task::spawn_blocking(move || {
    let mut sink = |message| {
        let _ = extract_bus.notify(message);
    };
    index_extracted(&mut sink, &available, &repo, &cache);
})
.await;
```

**The test.** Update `downloads_extracts_and_indexes_sources` (and the other
`sources` tests that call `index_extracted`) to pass a sink that both forwards to
the standalone bus and records the messages, then assert exactly one
`BaseArtifact` for the one-file fixture — and, in the multi-file case, that the
entries of every file are present in the single layer.

## Steps

- [x] `src/sources.rs`: change `index_extracted` to a `&mut dyn FnMut(DriverMessage)`
      sink; publish one merged `BaseArtifact` per artifact keyed by the
      sources-jar URI.
- [x] `src/sources.rs`: update `index_sources` to pass a bus-backed sink.
- [x] `src/sources.rs`: update the `index_extracted` tests to the sink form and
      add the one-`BaseArtifact`-per-artifact assertion and a multi-file merge
      assertion.
- [x] Docs: no `architecture.md` change (D5); update `docs/dev/backlog/index.md`
      and `docs/dev/changelog.md`.
- [x] Build and test: `cargo build`, `cargo fmt --check`, and the `sources` tests.
