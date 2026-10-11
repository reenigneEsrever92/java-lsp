# AGENTS.md

## Use the LSP MCP tools

Zed exposes the editor's language server to agents as **LSP MCP tools**. They
answer questions semantically — by symbol, type, and reference — so they are more
accurate and far cheaper than `grep`, reading whole files, or guessing paths.
Reach for them whenever you navigate, understand, or refactor code: jump to a
definition, find every caller, outline a file, inspect a type, or apply a fix.

They work on any file whose language has a running server — Rust in this repo via
(`rust-analyzer`, and Java via this project's own `java-lsp` when it is the server
registered for Java (try it on [`example/`](../example/README.md)).

**Positions are 1-based** — line and column, both passed in and returned. This
differs from raw LSP, which is 0-based; an off-by-one lands you in the wrong
context and the tool will answer honestly for the wrong symbol.

| Tool                  | Use it to                                                                                               |
| --------------------- | ------------------------------------------------------------------------------------------------------- |
| `workspaceSymbol`     | Find a symbol by name across the workspace when you know the name but not the file.                     |
| `documentSymbol`      | Outline a single file — every symbol, its kind, and its position — instead of reading it top to bottom. |
| `definition`          | Jump from a use site to where a symbol is declared.                                                     |
| `references`          | List every reference to a symbol; check all call sites before changing an API.                          |
| `hover`               | Read a symbol's type, signature, and docs without opening its definition.                               |
| `diagnostics`         | Get errors and warnings project-wide, or for one file, after an edit.                                   |
| `lsp_mcp_diagnostics` | Get the merged diagnostics for a single file.                                                           |
| `codeAction`          | List the fixes and refactors offered at a position (add import, create symbol, …).                      |
| `applyCodeAction`     | Apply one of those actions.                                                                             |
| `rename`              | Rename a symbol correctly across the workspace, not just the current file.                              |
| `format`              | Format a file or a range with the language's formatter.                                                 |

### Pattern

1. **Map the area** — `workspaceSymbol` by name, or `documentSymbol` for a file.
2. **Trace it** — `definition` to find where it lives, `references` to find who
   uses it.
3. **Confirm** — `hover` for the type and signature.
4. **Edit** — let the server do the mechanical work: `rename` for renames,
   `codeAction` + `applyCodeAction` for fixes, `format` for whitespace.
5. **Verify** — `diagnostics` (or `lsp_mcp_diagnostics`) to catch what the edit
   broke.
