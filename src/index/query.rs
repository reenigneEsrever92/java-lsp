//! Index query — reading the index's data structures.
//!
//! The reads over the index's data: the declared-type queries and the
//! [`TypeModel`] / [`SourceLayerIndex`] / [`ModelLayers`] methods they are built
//! from. The writes — the symbol table's upsert and removal, the declared-type
//! store's layer updates, and the context rebuild — live with the Java reading
//! that produces what is stored, in [`super::analyze`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use tower_lsp::lsp_types::Url;

use super::types::{
    IndexKind, Member, ModelLayers, SourceLayerIndex, SymbolEntry, Ty, TypeInfo, TypeModel,
    WorkspaceIndex,
};
use super::util::params_key;
/// The single type named `name`, preferring `package`; `None` when absent or
/// ambiguous. The workspace overlay wins over the non-source base.
pub fn find_unique<'a>(
    index: &'a WorkspaceIndex,
    name: &str,
    package: Option<&str>,
) -> Option<&'a TypeInfo> {
    index
        .context
        .overlay
        .find_unique(name, package)
        .or_else(|| index.context.base.find_unique(name, package))
}

/// The single type declared with this simple name in exactly `package`. Unlike
/// [`find_unique`], a name that only matches a type in another package yields
/// `None` rather than a unique-match fallback.
pub fn find_in_package<'a>(
    index: &'a WorkspaceIndex,
    name: &str,
    package: Option<&str>,
) -> Option<&'a TypeInfo> {
    index
        .context
        .overlay
        .find_in_package(name, package)
        .or_else(|| index.context.base.find_in_package(name, package))
}

/// True when any type is declared with this simple name.
pub fn contains(index: &WorkspaceIndex, name: &str) -> bool {
    index.context.overlay.contains(name) || index.context.base.contains(name)
}

/// Every declared type the index holds, overlay first, with same-slot duplicates
/// collapsed. The name-keyed queries cannot express "all types nested in `X`", so
/// callers that need such a scan use this (and [`nested_types`], built on it).
pub fn all_types(index: &WorkspaceIndex) -> Vec<&TypeInfo> {
    let mut seen: HashSet<(&str, Option<&str>, IndexKind, Option<&str>)> = HashSet::new();
    let mut out: Vec<&TypeInfo> = Vec::new();
    for info in index
        .context
        .overlay
        .all_types()
        .into_iter()
        .chain(index.context.base.all_types())
    {
        let slot = (
            info.name.as_str(),
            info.package.as_deref(),
            info.kind,
            info.nested.as_deref(),
        );
        if seen.insert(slot) {
            out.push(info);
        }
    }
    out
}

/// The types directly nested in the type `owner` declared in `package`, in no
/// particular order. A nested type records its enclosing chain in
/// `TypeInfo::nested`, so a direct child's innermost `$`-segment names it.
pub fn nested_types<'a>(
    index: &'a WorkspaceIndex,
    owner: &str,
    package: Option<&str>,
) -> Vec<&'a TypeInfo> {
    all_types(index)
        .into_iter()
        .filter(|info| {
            info.package.as_deref() == package
                && info
                    .nested
                    .as_deref()
                    .and_then(|chain| chain.rsplit('$').next())
                    == Some(owner)
        })
        .collect()
}

/// The top-level types declared directly in `package`. Like [`nested_types`],
/// this is a scan over [`all_types`] for a query the name index cannot express
/// (a package's members); nested types are excluded, since a package-qualified
/// reference to one names its owner chain, not the package alone.
pub fn types_in_package<'a>(index: &'a WorkspaceIndex, package: &str) -> Vec<&'a TypeInfo> {
    all_types(index)
        .into_iter()
        .filter(|info| info.nested.is_none() && info.package.as_deref() == Some(package))
        .collect()
}

/// The type a reference points at. A dotted name with no `$` is resolved
/// nested-first — its prefix as a type, then the last segment among that type's
/// nested types — falling back to reading the prefix as a package; a name with
/// no `.` resolves with `package` breaking ties.
pub fn lookup<'a>(
    index: &'a WorkspaceIndex,
    ty: &Ty,
    package: Option<&str>,
) -> Option<&'a TypeInfo> {
    match ty {
        Ty::Array(element) => lookup(index, element, package),
        Ty::Ref { name, .. } | Ty::Var(name) => {
            // A dotted name with no `$` may name a nested type (`Outer.Inner`)
            // or a package-qualified one (`java.util.List`); Java gives no
            // syntactic signal, so the in-scope type reading is tried first and
            // the package reading is the fallback.
            if !name.contains('$') {
                if let Some((owner_path, simple)) = name.rsplit_once('.') {
                    if let Some(owner) = lookup(index, &Ty::reference(owner_path), package) {
                        if let Some(nested) =
                            nested_types(index, &owner.name, owner.package.as_deref())
                                .into_iter()
                                .find(|info| info.name == simple)
                        {
                            return Some(nested);
                        }
                    }
                    return find_in_package(index, simple, Some(owner_path));
                }
            }
            let simple = name.rsplit(['.', '$']).next().unwrap_or(name);
            match ty.qualified_package() {
                Some(qualifier) => find_in_package(index, simple, Some(qualifier)),
                None => find_unique(index, simple, package),
            }
        }
        _ => None,
    }
}

/// The type's own constructors. Constructors are not inherited, so unlike
/// [`members`] this does not walk the hierarchy.
pub fn constructors(index: &WorkspaceIndex, ty: &Ty, package: Option<&str>) -> Vec<Member> {
    lookup(index, ty, package)
        .map(|info| info.constructors.clone())
        .unwrap_or_default()
}

/// The members of `ty`, including inherited ones. Supertype names resolve with
/// the subtype's own package as context; cycles are guarded and the first
/// declaration of a name wins. Same-named overloads collapse to the first — see
/// [`members_with_overloads`] to keep them apart.
pub fn members(index: &WorkspaceIndex, ty: &Ty, package: Option<&str>) -> Vec<Member> {
    let mut out = Vec::new();
    let mut seen: HashSet<(String, bool)> = HashSet::new();
    for member in members_with_overloads(index, ty, package) {
        let is_method = member.kind == IndexKind::Method;
        if seen.insert((member.name.clone(), is_method)) {
            out.push(member);
        }
    }
    out
}

/// The members of `ty`, including inherited ones, with same-named overloads kept
/// separate. Unlike [`members`] — which collapses a name to a single member for
/// typing — this deduplicates by name, kind, and parameter types, so an override
/// still collapses but genuinely distinct overloads remain. Supertype resolution
/// and the cycle guard match `members`.
pub fn members_with_overloads(
    index: &WorkspaceIndex,
    ty: &Ty,
    package: Option<&str>,
) -> Vec<Member> {
    if matches!(ty, Ty::Array(_)) {
        return Vec::new();
    }
    // The root resolves exactly when its name is qualified, and with `package`
    // breaking ties otherwise.
    let Some(root) = lookup(index, ty, package) else {
        return Vec::new();
    };
    let root_name = root.name.clone();
    let root_context = root.package.clone();
    let mut out = Vec::new();
    let mut seen: HashSet<(String, bool, String)> = HashSet::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<(String, Option<String>)> = VecDeque::new();
    queue.push_back((root_name, root_context));
    while let Some((name, context)) = queue.pop_front() {
        if !visited.insert(name.clone()) {
            continue;
        }
        let Some(info) = find_unique(index, &name, context.as_deref()) else {
            continue;
        };
        for member in info.members() {
            let is_method = member.kind == IndexKind::Method;
            let key = (member.name.clone(), is_method, params_key(&member.params));
            if seen.insert(key) {
                out.push(member.clone());
            }
        }
        let super_context = info.package.clone().or(context);
        for supertype in &info.supertypes {
            // A qualified supertype (every class-file edge) resolves in its own
            // package, so a base whose simple name is shared across packages
            // still finds the right type; a source-written simple name falls
            // back to the subtype's own context as before.
            if supertype.qualified_package().is_some() {
                if let Some(resolved) = lookup(index, supertype, None) {
                    queue.push_back((resolved.name.clone(), resolved.package.clone()));
                }
            } else if let Some(super_name) = supertype.simple_name() {
                queue.push_back((super_name.to_string(), super_context.clone()));
            }
        }
    }
    out
}

impl TypeModel {
    /// `(types, members, approximate bytes)` across the model, for the index
    /// composition log: the struct sizes plus the owned strings and types.
    pub(crate) fn stats(&self) -> (usize, usize, usize) {
        let mut types = 0usize;
        let mut members = 0usize;
        let mut bytes = 0usize;
        for info in self.by_name.values().flatten() {
            types += 1;
            members += info.fields.len() + info.methods.len() + info.constructors.len();
            bytes += info.bytes();
        }
        (types, members, bytes)
    }

    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    pub fn insert(&mut self, info: TypeInfo) {
        let bucket = self.by_name.entry(info.name.clone()).or_default();
        // A later insert for the same (name, package, kind) replaces the
        // earlier one, so source-derived types overlay name-only ones.
        if let Some(existing) = bucket.iter_mut().find(|candidate| {
            candidate.package == info.package
                && candidate.kind == info.kind
                && candidate.nested == info.nested
        }) {
            *existing = info;
        } else {
            bucket.push(info);
        }
    }

    pub fn extend(&mut self, infos: impl IntoIterator<Item = TypeInfo>) {
        for info in infos {
            self.insert(info);
        }
    }

    /// Adds every type of `other`, replacing same-slot entries (by name,
    /// package, kind, and enclosing chain). Used to union per-file overlays.
    pub fn merge(&mut self, other: &TypeModel) {
        for infos in other.by_name.values() {
            for info in infos {
                self.insert(info.clone());
            }
        }
    }

    /// Every type declared with this simple name.
    pub fn find(&self, name: &str) -> &[TypeInfo] {
        self.by_name.get(name).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Every declared type in the model, in no particular order.
    pub fn types(&self) -> impl Iterator<Item = &TypeInfo> {
        self.by_name.values().flatten()
    }

    /// Builds a name-only model from index entries: every top-level type
    /// becomes a `TypeInfo`, and each method/field entry is attached to the
    /// type named by the innermost container. Used for jars and the JDK, whose
    /// descriptors the flat index does not carry.
    pub fn from_entries(entries: &[SymbolEntry]) -> TypeModel {
        let mut model = TypeModel::new();
        model.add_entries(entries);
        model
    }

    /// Adds every name-only type and member described by `entries` to this
    /// model (see [`TypeModel::from_entries`]); entries are not kept, so this
    /// can be fed one archive at a time without cloning them first.
    pub fn add_entries(&mut self, entries: &[SymbolEntry]) {
        let mut members: HashMap<String, Vec<Member>> = HashMap::new();
        for entry in entries {
            if !matches!(
                entry.kind,
                IndexKind::Method | IndexKind::Field | IndexKind::EnumConstant
            ) {
                continue;
            }
            let Some(container) = entry.container.last() else {
                continue;
            };
            members.entry(container.clone()).or_default().push(Member {
                name: entry.name.to_string(),
                kind: entry.kind,
                ty: Ty::Unknown,
                params: Vec::new(),
                type_params: Vec::new(),
                is_static: false,
            });
        }
        for entry in entries {
            if !entry.kind.is_type() {
                continue;
            }
            // Nested types are indexed with a container chain; key them by
            // their innermost name so `Outer.Inner` is reachable as `Inner`.
            let name = entry
                .container
                .last()
                .cloned()
                .unwrap_or_else(|| entry.name.to_string());
            let mut info = TypeInfo::new(
                name.clone(),
                entry.package.as_deref().map(str::to_string),
                entry.kind,
            );
            if let Some(declared) = members.get(&name) {
                for member in declared {
                    if member.kind == IndexKind::Method {
                        info.methods.push(member.clone());
                    } else {
                        info.fields.push(member.clone());
                    }
                }
            }
            self.insert(info);
        }
    }
}

impl TypeModel {
    fn find_unique(&self, name: &str, package: Option<&str>) -> Option<&TypeInfo> {
        let candidates = self.find(name);
        match candidates.len() {
            0 => None,
            1 => Some(&candidates[0]),
            _ => {
                let mut preferred = candidates
                    .iter()
                    .filter(|info| info.package.as_deref() == package);
                let first = preferred.next()?;
                if preferred.next().is_some() {
                    None
                } else {
                    Some(first)
                }
            }
        }
    }

    fn contains(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    fn find_in_package(&self, name: &str, package: Option<&str>) -> Option<&TypeInfo> {
        let mut matches = self
            .find(name)
            .iter()
            .filter(|info| info.package.as_deref() == package);
        let first = matches.next()?;
        matches.next().is_none().then_some(first)
    }

    fn all_types(&self) -> Vec<&TypeInfo> {
        self.types().collect()
    }
}

impl SourceLayerIndex {
    /// Builds the index from `(uri, model)` layers already in increasing
    /// precedence (a later layer wins), preserving that order. Used for the
    /// non-source base, whose precedence is the order the producing modules
    /// added their artifacts in.
    pub fn ordered(layers: Vec<(Url, Arc<TypeModel>)>) -> Self {
        let mut models: Vec<Arc<TypeModel>> = Vec::with_capacity(layers.len());
        let mut uris: Vec<Url> = Vec::with_capacity(layers.len());
        let mut index_of: HashMap<Url, usize> = HashMap::with_capacity(layers.len());
        let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, (uri, model)) in layers.into_iter().enumerate() {
            index_of.insert(uri.clone(), i);
            uris.push(uri);
            models.push(model);
        }
        // Highest precedence first, so each name's layer positions are recorded
        // in precedence order. Within one layer a name's entries are contiguous
        // (one bucket per name), so the last position pushed is that layer.
        for i in (0..models.len()).rev() {
            for info in models[i].types() {
                let positions = by_name.entry(info.name.clone()).or_default();
                if positions.last() != Some(&i) {
                    positions.push(i);
                }
            }
        }
        Self {
            layers: models,
            uris,
            index_of,
            by_name,
        }
    }

    /// Builds the index from `(uri, model)` layers, normalising the order to
    /// URI-ascending (so the caller need not pre-sort). Used for the workspace
    /// source models, whose union is order-independent.
    pub fn new(mut layers: Vec<(Url, Arc<TypeModel>)>) -> Self {
        layers.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        Self::ordered(layers)
    }

    fn uri_of(&self, index: usize) -> &Url {
        &self.uris[index]
    }

    fn contains_uri(&self, uri: &Url) -> bool {
        self.index_of.contains_key(uri)
    }

    /// The layers that declare `name`, highest precedence first.
    fn declaring(&self, name: &str) -> Vec<&TypeModel> {
        let mut out = Vec::new();
        if let Some(positions) = self.by_name.get(name) {
            for &i in positions {
                if let Some(layer) = self.layers.get(i) {
                    if (**layer).contains(name) {
                        out.push(&**layer);
                    }
                }
            }
        }
        out
    }
}

/// The types named `name` across `declaring` (highest precedence first), each
/// slot (`package`, `kind`, enclosing chain) taken from its highest-precedence
/// layer — the same slot-dedup [`ModelLayers`] applies over its own layers.
fn layer_candidates<'a>(declaring: &[&'a TypeModel], name: &str) -> Vec<&'a TypeInfo> {
    let mut seen: HashSet<(Option<&str>, IndexKind, Option<&str>)> = HashSet::new();
    let mut out: Vec<&'a TypeInfo> = Vec::new();
    for layer in declaring {
        for info in layer.find(name) {
            let slot = (info.package.as_deref(), info.kind, info.nested.as_deref());
            if seen.insert(slot) {
                out.push(info);
            }
        }
    }
    out
}

impl SourceLayerIndex {
    pub(crate) fn find_unique(&self, name: &str, package: Option<&str>) -> Option<&TypeInfo> {
        let declaring = self.declaring(name);
        let first = *declaring.first()?;
        if declaring.len() == 1 {
            return first.find_unique(name, package);
        }
        let candidates = layer_candidates(&declaring, name);
        match candidates.len() {
            0 => None,
            1 => Some(candidates[0]),
            _ => {
                let mut preferred = candidates
                    .iter()
                    .filter(|info| info.package.as_deref() == package);
                let first = preferred.next()?;
                if preferred.next().is_some() {
                    None
                } else {
                    Some(*first)
                }
            }
        }
    }

    pub(crate) fn find_in_package(&self, name: &str, package: Option<&str>) -> Option<&TypeInfo> {
        let declaring = self.declaring(name);
        let first = *declaring.first()?;
        if declaring.len() == 1 {
            return first.find_in_package(name, package);
        }
        let mut matches = layer_candidates(&declaring, name)
            .into_iter()
            .filter(|info| info.package.as_deref() == package);
        let first = matches.next()?;
        matches.next().is_none().then_some(first)
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    pub(crate) fn all_types(&self) -> Vec<&TypeInfo> {
        let mut seen: HashSet<(&str, Option<&str>, IndexKind, Option<&str>)> = HashSet::new();
        let mut out: Vec<&TypeInfo> = Vec::new();
        // Highest precedence first (layers are in increasing precedence), each
        // declared type taken once — the name is part of the identity, so two
        // types sharing a package, kind, and owner both survive.
        for layer in self.layers.iter().rev() {
            for info in layer.types() {
                let slot = (
                    info.name.as_str(),
                    info.package.as_deref(),
                    info.kind,
                    info.nested.as_deref(),
                );
                if seen.insert(slot) {
                    out.push(info);
                }
            }
        }
        out
    }
}

/// A URI for a layer built with no URI of its own (the `ModelLayers::new` test
/// path). Such a layer can never be overridden, because the dirty overlay has
/// no entry that matches it.
fn synthetic_layer_uri(index: usize) -> Url {
    Url::parse(&format!("java-lsp-layer://{index}")).expect("valid synthetic layer URI")
}

impl Default for ModelLayers {
    /// An empty view: no layers and no dirty overlay.
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl ModelLayers {
    /// A view over `layers` (increasing precedence) with an empty dirty
    /// overlay, and no URI to override.
    pub fn new(layers: Vec<Arc<TypeModel>>) -> Self {
        let indexed = layers
            .into_iter()
            .enumerate()
            .map(|(i, model)| (synthetic_layer_uri(i), model))
            .collect();
        Self {
            base: Arc::new(SourceLayerIndex::new(indexed)),
            dirty: Arc::new(HashMap::new()),
        }
    }

    /// A view over a cached base and the engine's dirty overlay: a dirty entry
    /// replaces the base layer with the same URI, and a dirty-only URI (a file
    /// created after warm-up) ranks above every base layer.
    pub fn layered(base: Arc<SourceLayerIndex>, dirty: Arc<HashMap<Url, Arc<TypeModel>>>) -> Self {
        Self { base, dirty }
    }

    pub fn is_empty(&self) -> bool {
        let base_empty = self.base.layers.iter().enumerate().all(|(i, layer)| {
            match self.dirty.get(self.base.uri_of(i)) {
                Some(dirty) => dirty.is_empty(),
                None => layer.is_empty(),
            }
        });
        base_empty
            && self
                .dirty
                .iter()
                .filter(|(uri, _)| !self.base.contains_uri(uri))
                .all(|(_, model)| model.is_empty())
    }

    /// Every declared type across the layers, highest precedence first, with
    /// same-slot duplicates collapsed (a later layer's copy wins).
    pub fn types(&self) -> impl Iterator<Item = &TypeInfo> + '_ {
        let mut seen: HashSet<(&str, Option<&str>, IndexKind, Option<&str>)> = HashSet::new();
        let mut out: Vec<&TypeInfo> = Vec::new();
        for layer in self.effective_layers() {
            for info in layer.types() {
                if seen.insert((
                    info.name.as_str(),
                    info.package.as_deref(),
                    info.kind,
                    info.nested.as_deref(),
                )) {
                    out.push(info);
                }
            }
        }
        out.into_iter()
    }

    /// The effective layers in decreasing precedence: the dirty-only layers
    /// (by URI) first, then each base layer with its dirty override in place.
    fn effective_layers(&self) -> Vec<&TypeModel> {
        let mut out: Vec<&TypeModel> = Vec::new();
        for uri in self.dirty_only_uris() {
            if let Some(model) = self.dirty.get(uri) {
                out.push(&**model);
            }
        }
        for i in (0..self.base.layers.len()).rev() {
            out.push(self.effective_base(i));
        }
        out
    }

    /// The model in force for base layer `index`: its dirty override if it has
    /// one, else the warm-up layer.
    fn effective_base(&self, index: usize) -> &TypeModel {
        match self.dirty.get(self.base.uri_of(index)) {
            Some(dirty) => dirty,
            None => &self.base.layers[index],
        }
    }

    /// The dirty overlay's URIs the base does not carry, highest precedence
    /// (URI-descending) first.
    fn dirty_only_uris(&self) -> Vec<&Url> {
        let mut extras: Vec<&Url> = self
            .dirty
            .keys()
            .filter(|uri| !self.base.contains_uri(uri))
            .collect();
        extras.sort_by(|a, b| b.as_str().cmp(a.as_str()));
        extras
    }

    /// The layers that declare `name`, highest precedence first.
    fn declaring_layers(&self, name: &str) -> Vec<&TypeModel> {
        let mut out: Vec<&TypeModel> = Vec::new();
        for uri in self.dirty_only_uris() {
            if let Some(model) = self.dirty.get(uri) {
                if model.contains(name) {
                    out.push(&**model);
                }
            }
        }
        if let Some(positions) = self.base.by_name.get(name) {
            for &i in positions {
                let model = self.effective_base(i);
                if model.contains(name) {
                    out.push(model);
                }
            }
        }
        out
    }

    /// The candidate types named `name` across `declaring` (highest precedence
    /// first), each slot taken from its highest-precedence layer.
    fn candidates<'a>(declaring: &[&'a TypeModel], name: &str) -> Vec<&'a TypeInfo> {
        let mut seen: HashSet<(Option<&str>, IndexKind, Option<&str>)> = HashSet::new();
        let mut out: Vec<&'a TypeInfo> = Vec::new();
        for layer in declaring {
            for info in layer.find(name) {
                let slot = (info.package.as_deref(), info.kind, info.nested.as_deref());
                if seen.insert(slot) {
                    out.push(info);
                }
            }
        }
        out
    }
}

impl ModelLayers {
    pub(crate) fn find_unique(&self, name: &str, package: Option<&str>) -> Option<&TypeInfo> {
        // The common case is one declaring layer, whose own answer is the merged
        // answer — then nothing is allocated at all.
        let declaring = self.declaring_layers(name);
        let first = *declaring.first()?;
        if declaring.len() == 1 {
            return first.find_unique(name, package);
        }
        // Several layers declare the name: reproduce `TypeModel::find_unique`
        // over the slot-deduplicated candidates.
        let candidates = Self::candidates(&declaring, name);
        match candidates.len() {
            0 => None,
            1 => Some(candidates[0]),
            _ => {
                let mut preferred = candidates
                    .iter()
                    .filter(|info| info.package.as_deref() == package);
                let first = preferred.next()?;
                if preferred.next().is_some() {
                    None
                } else {
                    Some(*first)
                }
            }
        }
    }

    pub(crate) fn find_in_package(&self, name: &str, package: Option<&str>) -> Option<&TypeInfo> {
        let declaring = self.declaring_layers(name);
        let first = *declaring.first()?;
        if declaring.len() == 1 {
            return first.find_in_package(name, package);
        }
        let mut matches = Self::candidates(&declaring, name)
            .into_iter()
            .filter(|info| info.package.as_deref() == package);
        let first = matches.next()?;
        matches.next().is_none().then_some(first)
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        if let Some(positions) = self.base.by_name.get(name) {
            for &i in positions {
                match self.dirty.get(self.base.uri_of(i)) {
                    Some(dirty) => {
                        if dirty.contains(name) {
                            return true;
                        }
                    }
                    None => return true,
                }
            }
        }
        self.dirty
            .iter()
            .any(|(uri, model)| !self.base.contains_uri(uri) && model.contains(name))
    }

    pub(crate) fn all_types(&self) -> Vec<&TypeInfo> {
        self.types().collect()
    }
}
