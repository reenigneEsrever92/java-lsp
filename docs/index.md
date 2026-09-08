# java-lsp

A Java language server written in Rust: fast from project open, light on
resources, and with no JVM at runtime. It starts as a syntax-level server built
on tree-sitter and grows toward full type-aware Java support behind a pluggable
semantic-engine interface.

- [Overview](overview.md) — what the project is, the problem it solves, and what is deliberately out of scope.
- [Requirements](requirements.md) — users, use cases, technology choices, and the numbered requirement list with the initial milestone.
- [Architecture](architecture.md) — crate layout, the LSP shell and `SemanticEngine` seam, and the data flow.
- [Development](dev/index.md) — the change-driven workflow: the backlog of change requests and the changelog.
