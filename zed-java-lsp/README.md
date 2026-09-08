# zed-java-lsp

A [Zed](https://zed.dev) extension that provides Java support built around
[java-lsp](../) — the JVM-free Java language server written in Rust.

The extension is self-contained: it registers the `tree-sitter-java` grammar
and the Java language (config + highlighting, indentation, folds, outline,
brackets, text objects — vendored from `zed-extensions/java`), and starts the
`java-lsp` binary as the language server. No JDK, no jdtls, no downloads.

The extension ships no binaries, though. It resolves the `java-lsp` binary in
this order:

1. A user-configured binary: `lsp."java-lsp".binary.{path, arguments, env}` in
   Zed settings.
2. A `java-lsp` binary on the worktree's `$PATH` (e.g. installed with
   `cargo install --path <java-lsp-repo>`).
3. A build inside the java-lsp repository itself
   (`<worktree>/target/release/java-lsp`, then `target/debug/java-lsp`), which
   makes it work out of the box when the java-lsp checkout is the open project.

If none resolve, Zed shows an error explaining the options above.

## Install (development)

1. Build or install the server from the repository root:

   ```sh
   cargo build --release          # → target/release/java-lsp
   # or, to put it on your PATH:
   cargo install --path .
   ```

2. If you previously installed Zed's official `java` extension, uninstall it —
   it also defines the "Java" language (and pulls in jdtls, a JVM-based
   server). Two definitions of the same language conflict.

3. Install this extension as a dev extension: run
   `zed: install dev extension` from Zed's command palette and select this
   `zed-java-lsp` directory. Zed compiles the Rust extension to WebAssembly
   (`wasm32-wasip2`) and clones/builds the tree-sitter-java grammar (a C
   compiler is needed for that). Re-run the command after changing extension
   code.

4. Open a `.java` file (e.g. [`Hello.java`](../Hello.java)). `java-lsp` starts
   for Java buffers; check `zed: open log` if it does not.

## Pointing Zed at a specific binary

```json
{
  "lsp": {
    "java-lsp": {
      "binary": {
        "path": "/absolute/path/to/java-lsp",
        "arguments": []
      }
    }
  }
}
```

## Notes

- The server currently advertises incremental text sync, hover, go-to-definition
  and completions, all answering empty from the `SyntaxOnlyEngine` stub.
- Syntax highlighting, outline, folds, etc. come from the vendored
  tree-sitter queries, not from the server (that is the `syntax-features`
  backlog request).
- If you had added the `"language_servers": ["java-lsp", "!jdtls"]` pin from
  an earlier troubleshooting step, you can remove it — with this extension
  there is no jdtls to exclude.
