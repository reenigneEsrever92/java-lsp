---
type: Overview
title: java-lsp — a JVM-free Java language server
description: A Java language server written in Rust, responsive immediately on project open, with a pluggable semantic engine.
tags: [java, lsp, rust]
status: draft
---

Java language servers today run on a JVM: Eclipse JDT-LS and similar tools
bring a heavy install, slow project startup, and high memory use. This project
builds the alternative: a Java language server written in Rust, shipped as
native binaries, with **no JVM at runtime**, that feels responsive the moment a
project opens. Its primary user is any Java developer working in any
LSP-capable editor (Zed, Neovim, VS Code, Helix, Emacs).

The project is greenfield. Its shape is an LSP shell in Rust plus a pluggable
semantic engine behind a trait. The first engine work is pure Rust: syntax
features from tree-sitter-java and an in-process workspace symbol index that
powers completions and navigation without any type resolution. Full type-aware
semantics — a javac-based analysis daemon compiled with GraalVM Native Image
versus a pure-Rust type checker — is a deliberate, postponed decision recorded
in the requirements and backlog, not a built component.

Deliberately out of scope for now: full compiler parity, Maven/Gradle
project-model auto-detection, and refactoring features such as rename.

Next: the [requirements](requirements.md) trace users and use cases to numbered
requirements; the [architecture](architecture.md) describes how the code is
shaped; the [development section](dev/index.md) holds the backlog that turns
the requirements into work.
