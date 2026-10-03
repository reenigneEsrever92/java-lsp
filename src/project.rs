//! The Maven project model: pom discovery, source roots per module, and the
//! fallback to a plain whole-root workspace when no `pom.xml` exists.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tower_lsp::lsp_types::Url;

use crate::hub::{next_notification, HubClient};
use crate::messages::{DriverMessage, ProgressUpdate, Stage};
use crate::resolve::{local_repository, EffectivePom, Resolver};

/// One Maven module (or the whole workspace in fallback mode).
#[derive(Debug, Clone)]
pub struct Module {
    pub directory: PathBuf,
    pub effective: Option<EffectivePom>,
    /// Existing source roots (main and test); empty for aggregator poms.
    pub source_roots: Vec<PathBuf>,
}

/// What the server knows about the workspace layout.
#[derive(Debug, Clone, Default)]
pub struct ProjectModel {
    pub modules: Vec<Module>,
    /// False when no `pom.xml` was found and the whole root is scanned.
    pub maven: bool,
}

impl ProjectModel {
    /// Every source root in the workspace, deduplicated and sorted.
    pub fn source_roots(&self) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = self
            .modules
            .iter()
            .flat_map(|module| module.source_roots.iter().cloned())
            .collect();
        roots.sort();
        roots.dedup();
        roots
    }
}

/// Discovers the project model under `root`: every `pom.xml` (hidden and
/// `target`/`build` trees skipped) becomes a module with its effective pom
/// and source roots; with no poms at all the whole root is one module.
pub fn discover(root: &Path, resolver: &mut Resolver) -> ProjectModel {
    let mut modules: HashMap<PathBuf, Module> = HashMap::new();
    for pom_path in find_poms(root) {
        let Some(directory) = pom_path.parent().map(Path::to_path_buf) else {
            continue;
        };
        let effective = crate::resolve::assemble(&pom_path, resolver, 0);
        let source_roots = source_roots(&directory, effective.as_ref());
        modules.insert(
            directory.clone(),
            Module {
                directory,
                effective,
                source_roots,
            },
        );
    }

    if modules.is_empty() {
        return ProjectModel {
            modules: vec![Module {
                directory: root.to_path_buf(),
                effective: None,
                source_roots: vec![root.to_path_buf()],
            }],
            maven: false,
        };
    }

    let mut modules: Vec<Module> = modules.into_values().collect();
    modules.sort_by(|a, b| a.directory.cmp(&b.directory));
    ProjectModel {
        modules,
        maven: true,
    }
}

/// Main and test source roots of `directory`, honoring `<build>` overrides,
/// keeping only roots that exist on disk.
fn source_roots(directory: &Path, effective: Option<&EffectivePom>) -> Vec<PathBuf> {
    let Some(effective) = effective else {
        return Vec::new();
    };
    let main = effective
        .source_directory
        .clone()
        .unwrap_or_else(|| "src/main/java".to_string());
    let test = effective
        .test_source_directory
        .clone()
        .unwrap_or_else(|| "src/test/java".to_string());
    [main, test]
        .into_iter()
        .map(|relative| {
            let candidate = Path::new(&relative);
            if candidate.is_absolute() {
                candidate.to_path_buf()
            } else {
                directory.join(candidate)
            }
        })
        .filter(|root| root.is_dir())
        .collect()
}

/// Every `pom.xml` under `root`, hidden directories and build outputs skipped.
fn find_poms(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    find_poms_recursive(root, &mut out, 0);
    out
}

fn find_poms_recursive(dir: &Path, out: &mut Vec<PathBuf>, depth: u32) {
    if depth > 32 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with('.') || name == "target" || name == "build" {
            continue;
        }
        if path.is_dir() {
            find_poms_recursive(&path, out, depth + 1);
        } else if name == "pom.xml" {
            out.push(path);
        }
    }
}

/// Walks the workspace: discovers the Maven model and collects the `.java` files
/// under its source roots. The project driver runs this off the request path.
pub(crate) fn walk_project(root: &Path) -> (ProjectModel, Vec<PathBuf>) {
    let mut resolver = Resolver::new(local_repository());
    let model = discover(root, &mut resolver);
    let mut files = Vec::new();
    for source_root in model.source_roots() {
        collect_java_files(&source_root, &mut files);
    }
    files.sort();
    (model, files)
}

/// Every `.java` file under `dir` (hidden directories skipped).
fn collect_java_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let hidden = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with('.'));
            if !hidden {
                collect_java_files(&path, out);
            }
        } else if path.extension().is_some_and(|ext| ext == "java") {
            out.push(path);
        }
    }
}

// -- the project driver -----------------------------------------------------

/// Starts the project driver on the hub: on an added folder it walks for the
/// Maven model and the source inventory, and once the core stages and the
/// downloader have reported it flips `ready` and closes the progress item.
pub fn spawn(hub: &HubClient) {
    let client = hub.labeled("project");
    let mut rx = client.subscribe();
    tokio::spawn(async move {
        let mut root: Option<Url> = None;
        let mut started: Option<std::time::Instant> = None;
        let mut maven = false;
        let mut counts: HashMap<Stage, usize> = HashMap::new();
        let mut ready_sent = false;
        let mut summary_sent = false;
        while let Some(message) = next_notification(&mut rx).await {
            match message {
                DriverMessage::FolderAdded { uri } if root.is_none() => {
                    root = Some(uri.clone());
                    started = Some(std::time::Instant::now());
                    let _ = client.notify(DriverMessage::Progress(ProgressUpdate::Begin {
                        title: "java-lsp".to_string(),
                        message: "Indexing workspace".to_string(),
                    }));
                    let client = client.clone();
                    let _ = tokio::task::spawn_blocking(move || match uri.to_file_path() {
                        Ok(path) => {
                            let (model, files) = walk_project(&path);
                            let _ = client.notify(DriverMessage::ProjectModel {
                                model: Arc::new(model),
                            });
                            let _ = client.notify(DriverMessage::SourceInventory {
                                files: Arc::new(files),
                            });
                        }
                        Err(_) => {
                            let _ = client.notify(DriverMessage::ProjectModel {
                                model: Arc::new(ProjectModel::default()),
                            });
                            let _ = client.notify(DriverMessage::SourceInventory {
                                files: Arc::new(Vec::new()),
                            });
                        }
                    })
                    .await;
                }
                DriverMessage::ProjectModel { model } => {
                    maven = model.maven;
                }
                DriverMessage::StageDone { stage, count } => {
                    counts.insert(stage, count);
                    if !ready_sent
                        && counts.contains_key(&Stage::Sources)
                        && counts.contains_key(&Stage::Jars)
                        && counts.contains_key(&Stage::Jdk)
                    {
                        ready_sent = true;
                        let _ = client.notify(DriverMessage::Ready);
                    }
                    if ready_sent && !summary_sent && counts.contains_key(&Stage::Downloads) {
                        summary_sent = true;
                        let files = counts.get(&Stage::Sources).copied().unwrap_or(0);
                        let jars = counts.get(&Stage::Jars).copied().unwrap_or(0);
                        let jdk_classes = counts.get(&Stage::Jdk).copied().unwrap_or(0);
                        let elapsed_ms = started
                            .map(|at| at.elapsed().as_millis() as u64)
                            .unwrap_or(0);
                        let root = root.as_ref().map(Url::to_string).unwrap_or_default();
                        let _ = client.notify(DriverMessage::Summary {
                            root,
                            files,
                            jars,
                            jdk_classes,
                            maven,
                            elapsed_ms,
                        });
                        let _ = client.notify(DriverMessage::Progress(ProgressUpdate::End {
                            message: Some(format!(
                                "Indexed {files} files, {jars} dependency jars, {jdk_classes} JDK classes"
                            )),
                        }));
                    }
                }
                _ => {}
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "java-lsp-project-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn write(&self, relative: &str, content: &str) -> PathBuf {
            let path = self.root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
            path
        }

        fn dir(&self, relative: &str) -> PathBuf {
            let path = self.root.join(relative);
            std::fs::create_dir_all(&path).unwrap();
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn without_poms_the_whole_root_is_one_module() {
        let fixture = Fixture::new("fallback");
        fixture.dir("src");
        fixture.dir("target"); // would be excluded in Maven mode
        let mut resolver = Resolver::new(std::env::temp_dir().join("nonexistent"));
        let model = discover(&fixture.root, &mut resolver);

        assert!(!model.maven);
        assert_eq!(model.modules.len(), 1);
        assert_eq!(model.source_roots(), vec![fixture.root.clone()]);
    }

    #[test]
    fn multi_module_poms_yield_per_module_source_roots() {
        let fixture = Fixture::new("multimodule");
        fixture.write(
            "pom.xml",
            "<project><groupId>demo</groupId><artifactId>root</artifactId><version>1.0</version><packaging>pom</packaging><modules><module>app</module><module>lib</module></modules></project>",
        );
        fixture.write(
            "app/pom.xml",
            "<project><artifactId>app</artifactId><parent><groupId>demo</groupId><artifactId>root</artifactId><version>1.0</version></parent></project>",
        );
        fixture.write(
            "lib/pom.xml",
            "<project><artifactId>lib</artifactId><parent><groupId>demo</groupId><artifactId>root</artifactId><version>1.0</version></parent></project>",
        );
        let app_src = fixture.dir("app/src/main/java");
        let app_test = fixture.dir("app/src/test/java");
        let lib_src = fixture.dir("lib/src/main/java");

        let mut resolver = Resolver::new(std::env::temp_dir().join("nonexistent"));
        let model = discover(&fixture.root, &mut resolver);

        assert!(model.maven);
        assert_eq!(model.modules.len(), 3);
        let roots = model.source_roots();
        // Sorted, deduplicated, exactly the existing roots.
        assert_eq!(
            roots,
            vec![app_src, app_test, lib_src]
                .into_iter()
                .map(|root| root)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn build_directory_overrides_and_missing_roots_are_honored() {
        let fixture = Fixture::new("overrides");
        fixture.write(
            "pom.xml",
            "<project><groupId>demo</groupId><artifactId>root</artifactId><version>1.0</version>
             <build><sourceDirectory>java-src</sourceDirectory></build></project>",
        );
        let overridden = fixture.dir("java-src");
        // src/main/java and src/test/java deliberately not created.

        let mut resolver = Resolver::new(std::env::temp_dir().join("nonexistent"));
        let model = discover(&fixture.root, &mut resolver);

        assert!(model.maven);
        assert_eq!(model.source_roots(), vec![overridden]);
    }

    #[test]
    fn build_output_and_hidden_trees_are_not_scanned_for_poms() {
        let fixture = Fixture::new("skipdirs");
        fixture.write(
            "pom.xml",
            "<project><groupId>demo</groupId><artifactId>root</artifactId><version>1.0</version></project>",
        );
        // A pom inside target/ must not become a module.
        fixture.write(
            "target/generated/pom.xml",
            "<project><artifactId>generated</artifactId></project>",
        );
        fixture.write(
            ".hidden/pom.xml",
            "<project><artifactId>hidden</artifactId></project>",
        );

        let mut resolver = Resolver::new(std::env::temp_dir().join("nonexistent"));
        let model = discover(&fixture.root, &mut resolver);

        assert_eq!(model.modules.len(), 1);
    }

    /// The committed `example/` project must keep working: parent
    /// inheritance via `relativePath`, xmlns-bearing poms, source roots per
    /// module, and managed dependency versions.
    #[test]
    fn the_committed_example_project_discovers_three_modules() {
        let example = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("example");
        let mut resolver = Resolver::new(std::env::temp_dir().join("nonexistent"));
        let model = discover(&example, &mut resolver);

        assert!(model.maven);
        assert_eq!(model.modules.len(), 3);
        let roots = model.source_roots();
        assert!(roots.contains(
            &example
                .join("greeting-app")
                .join("src")
                .join("main")
                .join("java")
        ));
        assert!(roots.contains(
            &example
                .join("greeting-lib")
                .join("src")
                .join("test")
                .join("java")
        ));
        assert!(!roots
            .iter()
            .any(|root| root.starts_with(example.join("target"))));

        let app = model
            .modules
            .iter()
            .find(|module| module.directory.ends_with("greeting-app"))
            .expect("app module");
        let effective = app.effective.as_ref().expect("app effective pom");
        // Inherited from the parent via relativePath.
        assert_eq!(effective.group_id, "com.example");
        assert_eq!(effective.version, "1.0.0");
        // Declared dependency with its own version.
        let gson = effective
            .dependencies
            .iter()
            .find(|dep| dep.artifact_id == "gson")
            .expect("gson dependency");
        assert_eq!(gson.version.as_deref(), Some("2.10.1"));
        // Managed by the parent: no version on the declaration itself.
        let junit = effective
            .dependencies
            .iter()
            .find(|dep| dep.artifact_id == "junit")
            .expect("junit dependency");
        assert_eq!(junit.version, None);
        assert_eq!(
            effective.managed().get(&("junit", "junit")),
            Some(&"4.13.2"),
        );
    }
}
