# Example: greeting-demo

A small multi-module Maven project to try java-lsp against. It exercises
everything the server's project model understands:

- an **aggregator/parent pom** (`pom.xml`) with two modules inheriting
  `groupId`/`version` and a managed `junit` version from it,
- per-module **source roots** (`src/main/java` + `src/test/java`), so
  `target/` outputs are never indexed,
- a **sibling-module dependency** (`greeting-app` → `greeting-lib`) and a
  **third-party dependency** (`gson`), both resolved statically and offline
  from your local repository (`~/.m2/repository`, or `$MAVEN_REPO` if set) —
  the server never invokes `mvn` and never touches the network.

## Layout

```
pom.xml                        aggregator + parent (dependencyManagement)
greeting-lib/
  src/main/java/com/example/greeting/Greeter.java
  src/test/java/com/example/greeting/GreeterTest.java
greeting-app/
  src/main/java/com/example/app/Main.java
```

## Trying it

1. Build once with Maven so the dependency jars land in your local
   repository — `mvn install` in this directory. Without it, the server
   still works: it scans the source roots and skips the dependencies with a
   warning (features degrade gracefully, nothing breaks).
2. Open **this `example/` directory** as the workspace root in an editor
   with the java-lsp extension (see `zed-java-lsp/README.md`), or drive the
   binary directly (`RUST_LOG=java_lsp=info` shows the warm-up line with
   `maven=true` and the indexed jar count).
3. What you should see:
   - document symbols/folding in every file immediately, while indexing
     runs in the background;
   - completions inside `Main.java` offering `Greeter`, `greet`,
     `Lib`-style members from the indexed jars (`Greeter.getName`,
     Gson's classes) — labelled with their enclosing type;
   - `target/` contents and files outside source roots absent from the
     index; `workspace/symbol` finds only workspace sources.
