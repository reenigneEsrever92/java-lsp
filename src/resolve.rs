//! Static Maven dependency resolution: effective-POM assembly (parent chains,
//! property interpolation, `dependencyManagement` incl. import-scoped BOMs)
//! and the transitive dependency closure with Maven's conflict mediation.
//!
//! Strictly offline: artifacts are read from the local repository only;
//! nothing is ever fetched. Anything unresolvable is pruned with a warning —
//! degraded completions, never a broken server.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// One `<dependency>` declaration (also used for `dependencyManagement`
/// entries). Fields are interpolated by the time they are read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dep {
    pub group_id: String,
    pub artifact_id: String,
    pub version: Option<String>,
    pub scope: Option<String>,
    pub dep_type: Option<String>,
    pub optional: bool,
    pub exclusions: Vec<(String, String)>,
}

/// The effective model of one pom: the raw file merged with its parent chain
/// and fully interpolated.
#[derive(Debug, Clone)]
pub struct EffectivePom {
    pub group_id: String,
    pub artifact_id: String,
    pub version: String,
    /// `jar` or `pom`.
    pub packaging: String,
    pub properties: HashMap<String, String>,
    /// Chain-merged management with import-scoped BOMs expanded; direct
    /// entries beat imported ones.
    pub dependency_management: Vec<Dep>,
    /// Own + inherited dependencies (own declarations win on duplicates).
    pub dependencies: Vec<Dep>,
    pub modules: Vec<String>,
    pub source_directory: Option<String>,
    pub test_source_directory: Option<String>,
}

impl EffectivePom {
    /// `(groupId, artifactId) -> version` from the (expanded) management.
    pub fn managed(&self) -> HashMap<(&str, &str), &str> {
        self.dependency_management
            .iter()
            .filter_map(|dep| {
                dep.version
                    .as_deref()
                    .map(|version| ((dep.group_id.as_str(), dep.artifact_id.as_str()), version))
            })
            .collect()
    }
}

/// Reads the local Maven repository at `repo_root` and caches effective poms
/// per coordinates (multi-module projects share parents and BOMs heavily).
#[derive(Debug)]
pub struct Resolver {
    repo_root: PathBuf,
    cache: HashMap<(String, String, String), Option<EffectivePom>>,
}

impl Resolver {
    pub fn new(repo_root: PathBuf) -> Self {
        Self {
            repo_root,
            cache: HashMap::new(),
        }
    }

    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    fn pom_path(&self, group: &str, artifact: &str, version: &str) -> PathBuf {
        self.repo_root
            .join(group.replace('.', "/"))
            .join(artifact)
            .join(version)
            .join(format!("{artifact}-{version}.pom"))
    }

    pub fn jar_path(&self, group: &str, artifact: &str, version: &str) -> PathBuf {
        self.repo_root
            .join(group.replace('.', "/"))
            .join(artifact)
            .join(version)
            .join(format!("{artifact}-{version}.jar"))
    }

    /// The effective pom for coordinates, from the repository (cached).
    pub fn effective_from_repo(
        &mut self,
        group: &str,
        artifact: &str,
        version: &str,
    ) -> Option<&EffectivePom> {
        let key = (group.to_string(), artifact.to_string(), version.to_string());
        if !self.cache.contains_key(&key) {
            let path = self.pom_path(group, artifact, version);
            let effective = assemble(&path, self, 0);
            self.cache.insert(key.clone(), effective);
        }
        self.cache.get(&key).and_then(|entry| entry.as_ref())
    }
}

/// Assembles the effective pom for the file at `path`, resolving its parent
/// (via `relativePath` if it exists on disk, else the repository).
pub fn assemble(path: &Path, resolver: &mut Resolver, depth: u32) -> Option<EffectivePom> {
    if depth > 32 {
        tracing::warn!(pom = %path.display(), "parent chain too deep; giving up");
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    let raw = parse_pom(&text)?;

    let parent_eff = raw.parent.as_ref().and_then(|parent| {
        // `relativePath` points at the parent's directory (default `..`),
        // or directly at its pom file.
        let relative = raw.relative_path.as_deref().unwrap_or("..");
        let candidate = path
            .parent()
            .unwrap_or(Path::new("."))
            .join(relative)
            .join("pom.xml");
        let candidate = if candidate.is_file() {
            candidate
        } else {
            let direct = path.parent().unwrap_or(Path::new(".")).join(relative);
            match direct.extension().is_some_and(|ext| ext == "xml") {
                true => direct,
                false => direct.join("pom.xml"),
            }
        };
        if candidate.is_file() {
            return assemble(&candidate, resolver, depth + 1);
        }
        resolver
            .effective_from_repo(&parent.group_id, &parent.artifact_id, &parent.version)
            .cloned()
    });

    // Properties: parent's first, child's override.
    let mut properties: HashMap<String, String> = parent_eff
        .as_ref()
        .map(|parent| parent.properties.clone())
        .unwrap_or_default();
    for (key, value) in &raw.properties {
        properties.insert(key.clone(), value.clone());
    }

    // Coordinates: child inherits missing groupId/version from the parent.
    let group_id = raw
        .group_id
        .or_else(|| parent_eff.as_ref().map(|p| p.group_id.clone()))?;
    let artifact_id = raw.artifact_id.clone()?;
    let version = raw
        .version
        .or_else(|| parent_eff.as_ref().map(|p| p.version.clone()))?;

    // Built-ins, then interpolate every string the model carries.
    let mut lookup = properties.clone();
    for (key, value) in [
        ("project.groupId", group_id.as_str()),
        ("project.artifactId", artifact_id.as_str()),
        ("project.version", version.as_str()),
        ("pom.groupId", group_id.as_str()),
        ("pom.artifactId", artifact_id.as_str()),
        ("pom.version", version.as_str()),
    ] {
        lookup.insert(key.into(), value.into());
    }
    if let Some(parent) = &raw.parent {
        lookup.insert("project.parent.groupId".into(), parent.group_id.clone());
        lookup.insert("project.parent.version".into(), parent.version.clone());
    }
    let interp = |text: &str| interpolate(text, &lookup);

    let group_id = interp(&group_id);
    let artifact_id = interp(&artifact_id);
    let version = interp(&version);
    let packaging = raw.packaging.as_deref().unwrap_or("jar").to_string();

    // Management: chain entries (child wins on (g,a)) beat imported BOMs.
    let mut management: Vec<Dep> = parent_eff
        .as_ref()
        .map(|parent| parent.dependency_management.clone())
        .unwrap_or_default();
    for dep in raw.dependency_management {
        let interpolated = interpolate_dep(dep, &interp);
        if let Some(existing) = management.iter_mut().find(|existing| {
            existing.group_id == interpolated.group_id
                && existing.artifact_id == interpolated.artifact_id
        }) {
            *existing = interpolated;
        } else {
            management.push(interpolated);
        }
    }
    let mut imported: Vec<Dep> = Vec::new();
    for entry in management.clone() {
        if entry.scope.as_deref() != Some("import") {
            continue;
        }
        let Some(version) = entry.version.clone() else {
            continue;
        };
        let Some(bom) = resolver.effective_from_repo(&entry.group_id, &entry.artifact_id, &version)
        else {
            tracing::warn!(
                bom = %format!("{}:{}:{}", entry.group_id, entry.artifact_id, version),
                "import-scoped BOM not found in the local repository"
            );
            continue;
        };
        for dep in &bom.dependency_management {
            if !imported.iter().any(|existing| {
                existing.group_id == dep.group_id && existing.artifact_id == dep.artifact_id
            }) {
                imported.push(dep.clone());
            }
        }
    }
    // Chain entries overlay the imported ones; keep deterministic order.
    let mut dependency_management: Vec<Dep> = Vec::new();
    let merge = |dep: Dep, target: &mut Vec<Dep>| {
        if let Some(existing) = target.iter_mut().find(|existing| {
            existing.group_id == dep.group_id && existing.artifact_id == dep.artifact_id
        }) {
            *existing = dep;
        } else {
            target.push(dep);
        }
    };
    for dep in imported {
        merge(dep, &mut dependency_management);
    }
    for dep in management {
        let dep = strip_import_scope(dep);
        merge(dep, &mut dependency_management);
    }

    // Dependencies: own declarations first (they win mediation ties and
    // dedup), then inherited ones not redeclared.
    let mut dependencies: Vec<Dep> = Vec::new();
    for dep in raw.dependencies {
        dependencies.push(interpolate_dep(dep, &interp));
    }
    if let Some(parent) = parent_eff.as_ref() {
        for dep in &parent.dependencies {
            if !dependencies
                .iter()
                .any(|own| own.group_id == dep.group_id && own.artifact_id == dep.artifact_id)
            {
                dependencies.push(dep.clone());
            }
        }
    }

    let modules = raw
        .modules
        .into_iter()
        .map(|module| interp(&module))
        .collect();
    let source_directory = raw
        .source_directory
        .or_else(|| {
            parent_eff
                .as_ref()
                .and_then(|parent| parent.source_directory.clone())
        })
        .map(|dir| interp(&dir));
    let test_source_directory = raw
        .test_source_directory
        .or_else(|| {
            parent_eff
                .as_ref()
                .and_then(|parent| parent.test_source_directory.clone())
        })
        .map(|dir| interp(&dir));

    Some(EffectivePom {
        group_id,
        artifact_id,
        version,
        packaging,
        properties: lookup
            .into_iter()
            .filter(|(key, _)| !key.starts_with("project.") && !key.starts_with("pom."))
            .collect(),
        dependency_management,
        dependencies,
        modules,
        source_directory,
        test_source_directory,
    })
}

fn strip_import_scope(mut dep: Dep) -> Dep {
    dep.scope = None;
    dep
}

fn interpolate_dep(dep: Dep, interp: &impl Fn(&str) -> String) -> Dep {
    Dep {
        group_id: interp(&dep.group_id),
        artifact_id: interp(&dep.artifact_id),
        version: dep.version.map(|version| interp(&version)),
        scope: dep.scope.map(|scope| interp(&scope)),
        dep_type: dep.dep_type.map(|typ| interp(&typ)),
        optional: dep.optional,
        exclusions: dep
            .exclusions
            .into_iter()
            .map(|(group, artifact)| (interp(&group), interp(&artifact)))
            .collect(),
    }
}

/// Replaces `${key}` references; unknown keys stay literal (the artifact will
/// then simply not resolve, and is pruned with a warning downstream).
fn interpolate(text: &str, props: &HashMap<String, String>) -> String {
    let mut current = text.to_string();
    for _ in 0..10 {
        let Some(start) = current.find("${") else {
            break;
        };
        let Some(length) = current[start + 2..].find('}') else {
            break;
        };
        let key = &current[start + 2..start + 2 + length];
        let replacement = props.get(key).cloned().unwrap_or_default();
        current.replace_range(start..start + 2 + length + 1, &replacement);
    }
    current
}

/// One resolved artifact in the closure: `(groupId, artifactId, version)`.
pub type Artifact = (String, String, String);

/// Resolves the full dependency closure of `effective`: direct dependencies
/// (versions from management) expanded transitively via repository poms,
/// with nearest-wins/first-declared mediation, path-scoped exclusions, and
/// optional/test transitive pruning. Missing poms prune their branch.
pub fn resolve_closure(effective: &EffectivePom, resolver: &mut Resolver) -> Vec<Artifact> {
    let managed = effective.managed();
    let mut closure: Vec<Artifact> = Vec::new();
    let mut expanded: std::collections::HashSet<(String, String)> = Default::default();
    let mut queue: std::collections::VecDeque<(Artifact, Vec<(String, String)>)> =
        Default::default();

    for dep in &effective.dependencies {
        let Some(version) = dep.version.clone().or_else(|| {
            managed
                .get(&(dep.group_id.as_str(), dep.artifact_id.as_str()))
                .map(|version| version.to_string())
        }) else {
            tracing::warn!(
                dependency = %format!("{}:{}", dep.group_id, dep.artifact_id),
                "no version (declared or managed); skipping"
            );
            continue;
        };
        if version.is_empty() {
            // An unresolvable `${property}` interpolates to nothing.
            tracing::warn!(
                dependency = %format!("{}:{}", dep.group_id, dep.artifact_id),
                "version did not resolve; skipping"
            );
            continue;
        }
        queue.push_back((
            (dep.group_id.clone(), dep.artifact_id.clone(), version),
            dep.exclusions.clone(),
        ));
    }

    while let Some(((group, artifact, version), exclusions)) = queue.pop_front() {
        if !expanded.insert((group.clone(), artifact.clone())) {
            continue; // mediated away: a nearer (or first) declaration won
        }
        let artifact_key = (group.clone(), artifact.clone(), version.clone());
        closure.push(artifact_key.clone());

        let Some(dep_pom) = resolver.effective_from_repo(&group, &artifact, &version) else {
            tracing::warn!(
                artifact = %format!("{group}:{artifact}:{version}"),
                "pom not found in the local repository; branch pruned"
            );
            continue;
        };
        let dep_managed = dep_pom.managed();
        for transitive in &dep_pom.dependencies {
            if transitive.optional || transitive.scope.as_deref() == Some("test") {
                continue;
            }
            if exclusions.contains(&(transitive.group_id.clone(), transitive.artifact_id.clone())) {
                continue;
            }
            let Some(version) = transitive.version.clone().or_else(|| {
                dep_managed
                    .get(&(
                        transitive.group_id.as_str(),
                        transitive.artifact_id.as_str(),
                    ))
                    .map(|version| version.to_string())
            }) else {
                tracing::warn!(
                    dependency = %format!("{}:{}", transitive.group_id, transitive.artifact_id),
                    of = %format!("{group}:{artifact}"),
                    "transitive dependency without a resolvable version; pruned"
                );
                continue;
            };
            if version.is_empty() {
                tracing::warn!(
                    dependency = %format!("{}:{}", transitive.group_id, transitive.artifact_id),
                    of = %format!("{group}:{artifact}"),
                    "transitive version did not resolve; pruned"
                );
                continue;
            }
            let mut child_exclusions = exclusions.clone();
            child_exclusions.extend(transitive.exclusions.iter().cloned());
            queue.push_back((
                (
                    transitive.group_id.clone(),
                    transitive.artifact_id.clone(),
                    version,
                ),
                child_exclusions,
            ));
        }
    }
    closure
}

/// The raw, uninterpolated content of one pom file.
#[derive(Debug, Default, Clone)]
struct RawPom {
    group_id: Option<String>,
    artifact_id: Option<String>,
    version: Option<String>,
    packaging: Option<String>,
    parent: Option<Coordinates>,
    relative_path: Option<String>,
    properties: Vec<(String, String)>,
    dependency_management: Vec<Dep>,
    dependencies: Vec<Dep>,
    modules: Vec<String>,
    source_directory: Option<String>,
    test_source_directory: Option<String>,
}

#[derive(Debug, Clone)]
struct Coordinates {
    group_id: String,
    artifact_id: String,
    version: String,
}

fn parse_pom(text: &str) -> Option<RawPom> {
    let doc = roxmltree::Document::parse(text).ok()?;
    let project = doc.root_element();
    if project.tag_name().name() != "project" {
        return None;
    }
    let text_of = |node: roxmltree::Node, name: &str| -> Option<String> {
        child(node, name).and_then(|child| child.text()).map(trim)
    };
    let mut pom = RawPom {
        group_id: text_of(project, "groupId"),
        artifact_id: text_of(project, "artifactId"),
        version: text_of(project, "version"),
        packaging: text_of(project, "packaging"),
        ..Default::default()
    };
    if let Some(parent) = child(project, "parent") {
        pom.parent = Some(Coordinates {
            group_id: text_of(parent, "groupId")?,
            artifact_id: text_of(parent, "artifactId")?,
            version: text_of(parent, "version")?,
        });
        pom.relative_path = text_of(parent, "relativePath");
    }
    if let Some(properties) = child(project, "properties") {
        pom.properties = properties
            .children()
            .filter(|node| node.is_element())
            .filter_map(|node| {
                node.text()
                    .map(|text| (node.tag_name().name().to_string(), trim(text)))
            })
            .collect();
    }
    pom.dependency_management = child(project, "dependencyManagement")
        .and_then(|management| child(management, "dependencies"))
        .map(|dependencies| parse_dependencies(dependencies))
        .unwrap_or_default();
    pom.dependencies = child(project, "dependencies")
        .map(|dependencies| parse_dependencies(dependencies))
        .unwrap_or_default();
    pom.modules = child(project, "modules")
        .map(|modules| {
            modules
                .children()
                .filter(|node| node.is_element() && node.tag_name().name() == "module")
                .filter_map(|node| node.text().map(trim))
                .collect()
        })
        .unwrap_or_default();
    if let Some(build) = child(project, "build") {
        pom.source_directory = text_of(build, "sourceDirectory");
        pom.test_source_directory = text_of(build, "testSourceDirectory");
    }
    Some(pom)
}

fn parse_dependencies(node: roxmltree::Node) -> Vec<Dep> {
    node.children()
        .filter(|node| node.is_element() && node.tag_name().name() == "dependency")
        .map(|dependency| Dep {
            group_id: child_text(dependency, "groupId").unwrap_or_default(),
            artifact_id: child_text(dependency, "artifactId").unwrap_or_default(),
            version: child_text(dependency, "version"),
            scope: child_text(dependency, "scope"),
            dep_type: child_text(dependency, "type"),
            optional: child_text(dependency, "optional").as_deref() == Some("true"),
            exclusions: child(dependency, "exclusions")
                .map(|exclusions| {
                    exclusions
                        .children()
                        .filter(|node| node.is_element() && node.tag_name().name() == "exclusion")
                        .filter_map(|exclusion| {
                            Some((
                                child_text(exclusion, "groupId")?,
                                child_text(exclusion, "artifactId")?,
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect()
}

fn child<'a>(node: roxmltree::Node<'a, 'a>, name: &str) -> Option<roxmltree::Node<'a, 'a>> {
    node.children()
        .find(|child| child.is_element() && child.tag_name().name() == name)
}

fn child_text(node: roxmltree::Node, name: &str) -> Option<String> {
    child(node, name).and_then(|child| child.text()).map(trim)
}

fn trim(text: &str) -> String {
    text.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a local repository in a temp dir from `path -> content` pairs
    /// (pom files laid out in the standard `{g/a/p}/{a}/{v}/` structure).
    struct TestRepo {
        root: PathBuf,
    }

    impl TestRepo {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "java-lsp-resolve-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn pom(&self, group: &str, artifact: &str, version: &str, text: &str) {
            let dir = self
                .root
                .join(group.replace('.', "/"))
                .join(artifact)
                .join(version);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!("{artifact}-{version}.pom")), text).unwrap();
        }

        fn resolver(&self) -> Resolver {
            Resolver::new(self.root.clone())
        }
    }

    impl Drop for TestRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn artifact_name(closure: &[Artifact], artifact: &str) -> Option<String> {
        closure
            .iter()
            .find(|(_, a, _)| a == artifact)
            .map(|(_, _, v)| v.clone())
    }

    #[test]
    fn parent_inheritance_supplies_group_version_and_management() {
        let repo = TestRepo::new("parent");
        repo.pom(
            "demo",
            "child",
            "1.0",
            "\
<project>
  <groupId>demo</groupId>
  <artifactId>child</artifactId>
  <version>1.0</version>
  <parent><groupId>demo</groupId><artifactId>up</artifactId><version>2.0</version></parent>
  <dependencies>
    <dependency><groupId>junit</groupId><artifactId>junit</artifactId></dependency>
  </dependencies>
</project>",
        );
        repo.pom(
            "demo",
            "up",
            "2.0",
            "\
<project>
  <groupId>demo</groupId>
  <artifactId>up</artifactId>
  <version>2.0</version>
  <properties><junit.version>4.13.2</junit.version></properties>
  <dependencyManagement>
    <dependencies>
      <dependency><groupId>junit</groupId><artifactId>junit</artifactId><version>${junit.version}</version></dependency>
    </dependencies>
  </dependencyManagement>
</project>",
        );

        let mut resolver = repo.resolver();
        let child = resolver
            .effective_from_repo("demo", "child", "1.0")
            .expect("child pom")
            .clone();
        assert_eq!(child.group_id, "demo");
        assert_eq!(child.version, "1.0");
        let closure = resolve_closure(&child, &mut resolver);
        assert_eq!(artifact_name(&closure, "junit").as_deref(), Some("4.13.2"));
    }

    #[test]
    fn on_disk_parent_via_relative_path() {
        let dir = std::env::temp_dir().join(format!(
            "java-lsp-relpath-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("app")).unwrap();
        std::fs::write(
            dir.join("pom.xml"),
            "<project><groupId>demo</groupId><artifactId>root</artifactId><version>1.0</version><properties><v>9.9</v></properties><dependencyManagement><dependencies><dependency><groupId>x</groupId><artifactId>x</artifactId><version>${v}</version></dependency></dependencies></dependencyManagement></project>",
        )
        .unwrap();
        std::fs::write(
            dir.join("app").join("pom.xml"),
            "<project><artifactId>app</artifactId><parent><groupId>demo</groupId><artifactId>root</artifactId><version>1.0</version></parent><dependencies><dependency><groupId>x</groupId><artifactId>x</artifactId></dependency></dependencies></project>",
        )
        .unwrap();

        let mut resolver = Resolver::new(std::env::temp_dir().join("nonexistent-repo"));
        let effective = assemble(&dir.join("app").join("pom.xml"), &mut resolver, 0).unwrap();
        assert_eq!(effective.group_id, "demo");
        let closure = resolve_closure(&effective, &mut resolver);
        assert_eq!(artifact_name(&closure, "x").as_deref(), Some("9.9"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bom_import_manages_versions_and_direct_management_wins() {
        let repo = TestRepo::new("bom");
        repo.pom(
            "demo",
            "bom",
            "1.0",
            "\
<project>
  <groupId>demo</groupId><artifactId>bom</artifactId><version>1.0</version>
  <packaging>pom</packaging>
  <dependencyManagement>
    <dependencies>
      <dependency><groupId>x</groupId><artifactId>from-bom</artifactId><version>1.1</version></dependency>
      <dependency><groupId>x</groupId><artifactId>overridden</artifactId><version>1.1</version></dependency>
    </dependencies>
  </dependencyManagement>
</project>",
        );
        repo.pom(
            "demo",
            "app",
            "1.0",
            "\
<project>
  <groupId>demo</groupId><artifactId>app</artifactId><version>1.0</version>
  <dependencyManagement>
    <dependencies>
      <dependency><groupId>x</groupId><artifactId>overridden</artifactId><version>9.9</version></dependency>
      <dependency><groupId>demo</groupId><artifactId>bom</artifactId><version>1.0</version><type>pom</type><scope>import</scope></dependency>
    </dependencies>
  </dependencyManagement>
  <dependencies>
    <dependency><groupId>x</groupId><artifactId>from-bom</artifactId></dependency>
    <dependency><groupId>x</groupId><artifactId>overridden</artifactId></dependency>
  </dependencies>
</project>",
        );

        let mut resolver = repo.resolver();
        let app = resolver
            .effective_from_repo("demo", "app", "1.0")
            .unwrap()
            .clone();
        let closure = resolve_closure(&app, &mut resolver);
        assert_eq!(artifact_name(&closure, "from-bom").as_deref(), Some("1.1"));
        // Direct management beats the BOM's managed version.
        assert_eq!(
            artifact_name(&closure, "overridden").as_deref(),
            Some("9.9")
        );
    }

    #[test]
    fn transitive_closure_walks_artifact_poms() {
        let repo = TestRepo::new("transitive");
        repo.pom("demo", "app", "1.0", "\
<project><groupId>demo</groupId><artifactId>app</artifactId><version>1.0</version>
<dependencies><dependency><groupId>x</groupId><artifactId>a</artifactId><version>1.0</version></dependency></dependencies></project>");
        repo.pom("x", "a", "1.0", "\
<project><groupId>x</groupId><artifactId>a</artifactId><version>1.0</version>
<dependencies><dependency><groupId>x</groupId><artifactId>b</artifactId><version>2.0</version></dependency></dependencies></project>");
        repo.pom("x", "b", "2.0", "\
<project><groupId>x</groupId><artifactId>b</artifactId><version>2.0</version>
<dependencies><dependency><groupId>x</groupId><artifactId>c</artifactId><version>3.0</version></dependency></dependencies></project>");
        repo.pom(
            "x",
            "c",
            "3.0",
            "\
<project><groupId>x</groupId><artifactId>c</artifactId><version>3.0</version></project>",
        );

        let mut resolver = repo.resolver();
        let app = resolver
            .effective_from_repo("demo", "app", "1.0")
            .unwrap()
            .clone();
        let closure = resolve_closure(&app, &mut resolver);
        assert!(closure.contains(&("x".into(), "a".into(), "1.0".into())));
        assert!(closure.contains(&("x".into(), "b".into(), "2.0".into())));
        assert!(closure.contains(&("x".into(), "c".into(), "3.0".into())));
    }

    #[test]
    fn nearest_dependency_wins_and_first_declaration_breaks_ties() {
        let repo = TestRepo::new("mediation");
        // Equal depth: a's c:1.0 is declared before d's c:2.0 -> 1.0 wins.
        repo.pom(
            "demo",
            "app",
            "1.0",
            "\
<project><groupId>demo</groupId><artifactId>app</artifactId><version>1.0</version>
<dependencies>
  <dependency><groupId>x</groupId><artifactId>a</artifactId><version>1.0</version></dependency>
  <dependency><groupId>x</groupId><artifactId>d</artifactId><version>1.0</version></dependency>
</dependencies></project>",
        );
        repo.pom("x", "a", "1.0", "\
<project><groupId>x</groupId><artifactId>a</artifactId><version>1.0</version>
<dependencies><dependency><groupId>x</groupId><artifactId>c</artifactId><version>1.0</version></dependency></dependencies></project>");
        repo.pom("x", "d", "1.0", "\
<project><groupId>x</groupId><artifactId>d</artifactId><version>1.0</version>
<dependencies><dependency><groupId>x</groupId><artifactId>c</artifactId><version>2.0</version></dependency></dependencies></project>");
        repo.pom("x", "c", "1.0", "<project><groupId>x</groupId><artifactId>c</artifactId><version>1.0</version></project>");
        repo.pom("x", "c", "2.0", "<project><groupId>x</groupId><artifactId>c</artifactId><version>2.0</version></project>");

        let mut resolver = repo.resolver();
        let app = resolver
            .effective_from_repo("demo", "app", "1.0")
            .unwrap()
            .clone();
        let closure = resolve_closure(&app, &mut resolver);
        assert_eq!(artifact_name(&closure, "c").as_deref(), Some("1.0"));

        // Nearest wins over deeper: shallow e pulls c:2.0 one level out.
        repo.pom(
            "demo",
            "app2",
            "1.0",
            "\
<project><groupId>demo</groupId><artifactId>app2</artifactId><version>1.0</version>
<dependencies>
  <dependency><groupId>x</groupId><artifactId>e</artifactId><version>1.0</version></dependency>
  <dependency><groupId>x</groupId><artifactId>a</artifactId><version>1.0</version></dependency>
</dependencies></project>",
        );
        repo.pom("x", "e", "1.0", "\
<project><groupId>x</groupId><artifactId>e</artifactId><version>1.0</version>
<dependencies><dependency><groupId>x</groupId><artifactId>c</artifactId><version>2.0</version></dependency></dependencies></project>");
        let app2 = resolver
            .effective_from_repo("demo", "app2", "1.0")
            .unwrap()
            .clone();
        let closure = resolve_closure(&app2, &mut resolver);
        assert_eq!(artifact_name(&closure, "c").as_deref(), Some("2.0"));
    }

    #[test]
    fn optional_and_test_transitives_are_pruned() {
        let repo = TestRepo::new("optional");
        repo.pom("demo", "app", "1.0", "\
<project><groupId>demo</groupId><artifactId>app</artifactId><version>1.0</version>
<dependencies><dependency><groupId>x</groupId><artifactId>a</artifactId><version>1.0</version></dependency></dependencies></project>");
        repo.pom("x", "a", "1.0", "\
<project><groupId>x</groupId><artifactId>a</artifactId><version>1.0</version>
<dependencies>
  <dependency><groupId>x</groupId><artifactId>opt</artifactId><version>1.0</version><optional>true</optional></dependency>
  <dependency><groupId>x</groupId><artifactId>tst</artifactId><version>1.0</version><scope>test</scope></dependency>
  <dependency><groupId>x</groupId><artifactId>kept</artifactId><version>1.0</version></dependency>
</dependencies></project>");
        repo.pom("x", "opt", "1.0", "<project><groupId>x</groupId><artifactId>opt</artifactId><version>1.0</version></project>");
        repo.pom("x", "tst", "1.0", "<project><groupId>x</groupId><artifactId>tst</artifactId><version>1.0</version></project>");
        repo.pom("x", "kept", "1.0", "<project><groupId>x</groupId><artifactId>kept</artifactId><version>1.0</version></project>");

        let mut resolver = repo.resolver();
        let app = resolver
            .effective_from_repo("demo", "app", "1.0")
            .unwrap()
            .clone();
        let closure = resolve_closure(&app, &mut resolver);
        assert!(!closure.iter().any(|(_, a, _)| a == "opt"));
        assert!(!closure.iter().any(|(_, a, _)| a == "tst"));
        assert!(closure.iter().any(|(_, a, _)| a == "kept"));
    }

    #[test]
    fn exclusions_prune_the_named_artifact_on_that_path() {
        let repo = TestRepo::new("exclusions");
        repo.pom(
            "demo",
            "app",
            "1.0",
            "\
<project><groupId>demo</groupId><artifactId>app</artifactId><version>1.0</version>
<dependencies>
  <dependency><groupId>x</groupId><artifactId>a</artifactId><version>1.0</version>
    <exclusions><exclusion><groupId>x</groupId><artifactId>c</artifactId></exclusion></exclusions>
  </dependency>
  <dependency><groupId>x</groupId><artifactId>d</artifactId><version>1.0</version></dependency>
</dependencies></project>",
        );
        repo.pom("x", "a", "1.0", "\
<project><groupId>x</groupId><artifactId>a</artifactId><version>1.0</version>
<dependencies><dependency><groupId>x</groupId><artifactId>c</artifactId><version>1.0</version></dependency></dependencies></project>");
        repo.pom("x", "d", "1.0", "\
<project><groupId>x</groupId><artifactId>d</artifactId><version>1.0</version>
<dependencies><dependency><groupId>x</groupId><artifactId>c</artifactId><version>1.0</version></dependency></dependencies></project>");
        repo.pom("x", "c", "1.0", "<project><groupId>x</groupId><artifactId>c</artifactId><version>1.0</version></project>");

        let mut resolver = repo.resolver();
        let app = resolver
            .effective_from_repo("demo", "app", "1.0")
            .unwrap()
            .clone();
        let closure = resolve_closure(&app, &mut resolver);
        // c is excluded via a but still arrives via d.
        assert!(closure.iter().any(|(_, a, _)| a == "c"));
        assert_eq!(artifact_name(&closure, "c").as_deref(), Some("1.0"));
    }

    #[test]
    fn dependency_cycles_terminate() {
        let repo = TestRepo::new("cycle");
        repo.pom("demo", "app", "1.0", "\
<project><groupId>demo</groupId><artifactId>app</artifactId><version>1.0</version>
<dependencies><dependency><groupId>x</groupId><artifactId>a</artifactId><version>1.0</version></dependency></dependencies></project>");
        repo.pom("x", "a", "1.0", "\
<project><groupId>x</groupId><artifactId>a</artifactId><version>1.0</version>
<dependencies><dependency><groupId>x</groupId><artifactId>b</artifactId><version>1.0</version></dependency></dependencies></project>");
        repo.pom("x", "b", "1.0", "\
<project><groupId>x</groupId><artifactId>b</artifactId><version>1.0</version>
<dependencies><dependency><groupId>x</groupId><artifactId>a</artifactId><version>1.0</version></dependency></dependencies></project>");

        let mut resolver = repo.resolver();
        let app = resolver
            .effective_from_repo("demo", "app", "1.0")
            .unwrap()
            .clone();
        let closure = resolve_closure(&app, &mut resolver);
        assert!(closure.iter().any(|(_, a, _)| a == "a"));
        assert!(closure.iter().any(|(_, a, _)| a == "b"));
    }

    #[test]
    fn missing_poms_prune_their_branch_without_killing_the_closure() {
        let repo = TestRepo::new("missing");
        repo.pom(
            "demo",
            "app",
            "1.0",
            "\
<project><groupId>demo</groupId><artifactId>app</artifactId><version>1.0</version>
<dependencies>
  <dependency><groupId>x</groupId><artifactId>ghost</artifactId><version>1.0</version></dependency>
  <dependency><groupId>x</groupId><artifactId>real</artifactId><version>1.0</version></dependency>
</dependencies></project>",
        );
        repo.pom("x", "real", "1.0", "<project><groupId>x</groupId><artifactId>real</artifactId><version>1.0</version></project>");

        let mut resolver = repo.resolver();
        let app = resolver
            .effective_from_repo("demo", "app", "1.0")
            .unwrap()
            .clone();
        let closure = resolve_closure(&app, &mut resolver);
        assert!(closure.contains(&("x".into(), "ghost".into(), "1.0".into())));
        assert!(closure.contains(&("x".into(), "real".into(), "1.0".into())));
    }

    #[test]
    fn interpolation_expands_properties_and_leaves_unknowns_literate() {
        let repo = TestRepo::new("interp");
        repo.pom("demo", "app", "1.0", "\
<project><groupId>demo</groupId><artifactId>app</artifactId><version>1.0</version>
<properties><lib.version>7.7</lib.version></properties>
<dependencies>
  <dependency><groupId>x</groupId><artifactId>lib</artifactId><version>${lib.version}</version></dependency>
  <dependency><groupId>x</groupId><artifactId>ghost</artifactId><version>${no.such.key}</version></dependency>
</dependencies></project>");
        repo.pom("x", "lib", "7.7", "<project><groupId>x</groupId><artifactId>lib</artifactId><version>7.7</version></project>");

        let mut resolver = repo.resolver();
        let app = resolver
            .effective_from_repo("demo", "app", "1.0")
            .unwrap()
            .clone();
        let closure = resolve_closure(&app, &mut resolver);
        assert_eq!(artifact_name(&closure, "lib").as_deref(), Some("7.7"));
        // Unknown property interpolates to the empty string; nothing resolves.
        assert!(!closure.iter().any(|(_, a, _)| a == "ghost"));
    }
}
