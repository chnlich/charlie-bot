//! Workspace structure check: the group of each crate, the dependencies between groups, the runtime layer order, and
//! the module cycles inside each crate.
//!
//! A crate's group comes from its directory under `rust/crates`: `infra/<name>` is `infra`, `runtime/<name>` is
//! `runtime`, `backends/<name>` is `backends.<name>`, `features/<name>` is `features.<name>`, and `app/<name>` is
//! `app`. Any other directory fails.
//!
//! Directions: infra depends on infra only. runtime depends on runtime and infra. A backend or a feature depends on
//! itself, runtime and infra; the backends in `BACKEND_VARIANTS` may also depend on backends.claude_code, and the
//! features in `CHAT_CHANNELS` may also depend on features.chat_threads. app depends on every group. Normal and build
//! dependencies count; a dev-dependency does not, because test code follows no direction.
//!
//! Inside runtime, every crate is one layer. A runtime crate depends only on its own layer or a lower layer. The table
//! must cover every runtime crate and name no missing one.
//!
//! Each crate's module graph covers every target: its lib (or its main bin), each other bin, test, example and bench.
//! A node is the target root, a file module or an inline `mod` block. An edge A -> B means A's own code holds a path
//! that resolves to an item defined in B, through `use` trees, expressions, types, patterns and the tokens of macro
//! calls. A `mod` declaration is no edge, and a `pub use` that only re-exports a name is no edge until the module's own
//! code uses that name. A cycle is a strongly connected component of two or more modules.
//!
//! Every module cycle fails: a cycle is a defect of the code, not a recorded exception.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use proc_macro2::{Spacing, TokenStream, TokenTree};
use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::{Attribute, Item, ItemMacro, ItemMod, ItemUse, Meta, UseTree};

const BACKEND_VARIANTS: [&str; 2] = ["anthropic_compatible", "openai_compatible"];
const CHAT_CHANNELS: [&str; 2] = ["slack", "discord"];

/// The runtime layers, keyed by the crate's directory name under `rust/crates/runtime`.
const RUNTIME_LAYERS: [(&str, usize); 7] =
  [("store", 1), ("registry", 2), ("usage", 3), ("agents", 4), ("tasks", 5), ("triggers", 6), ("session-api", 7)];

const DEPEND_ON_THE_RUNTIME: &str = "Depend on the registry: the imported crate registers its implementation, and the \
                                    importer looks it up there.";
const FIX_THE_UPWARD_IMPORT: &str = "Fix it in one of four ways. Give the lower module a narrow protocol that the \
                                     higher module satisfies. Give the owning module an instance accessor. Move the \
                                     shared code to the lower module. Delete the forwarder.";

// ---------------------------------------------------------------------------------------------------------------------
// The workspace, read from `cargo metadata`.

struct Workspace {
  root: PathBuf,
  crates: Vec<Crate>,
  path_failures: Vec<String>,
}

struct Crate {
  name: String,
  manifest_rel: String,
  dir_rel: String,
  group: Option<String>,
  /// Workspace packages this crate's normal and build dependencies name.
  deps: BTreeSet<String>,
  targets: Vec<Target>,
}

struct Target {
  /// `crate` for the crate's root target; `<kind>.<name>` for every other target.
  label: String,
  root: bool,
  modules: BTreeMap<Vec<String>, Module>,
}

#[derive(Default)]
struct Module {
  file: PathBuf,
  children: BTreeSet<String>,
  defs: BTreeSet<String>,
  reexports: BTreeMap<String, Vec<String>>,
  globs: Vec<Vec<String>>,
  uses: Vec<UseRecord>,
  paths: Vec<PathRecord>,
  macro_paths: Vec<PathRecord>,
  /// Identifiers the module's own code names, outside `use` declarations.
  idents: BTreeSet<String>,
}

struct UseRecord {
  path: Vec<String>,
  name: Option<String>,
  glob: bool,
  is_pub: bool,
  line: usize,
}

struct PathRecord {
  segs: Vec<String>,
  line: usize,
}

struct TargetMeta {
  kind: String,
  name: String,
  src: PathBuf,
}

fn load_workspace(manifest_dir: &Path) -> Workspace {
  let cargo = std::env::var("CARGO").expect("CARGO names the cargo program for a test process");
  let manifest = manifest_dir.join("Cargo.toml");
  let output = Command::new(&cargo)
    .args(["metadata", "--no-deps", "--offline", "--format-version", "1", "--manifest-path"])
    .arg(&manifest)
    .output()
    .unwrap_or_else(|error| panic!("cannot run {cargo} metadata for {}: {error}", manifest.display()));
  assert!(
    output.status.success(),
    "cargo metadata for {} failed: {}",
    manifest.display(),
    String::from_utf8_lossy(&output.stderr)
  );
  let json: serde_json::Value = serde_json::from_slice(&output.stdout)
    .unwrap_or_else(|error| panic!("cargo metadata for {} is not JSON: {error}", manifest.display()));
  let root = PathBuf::from(json["workspace_root"].as_str().expect("cargo metadata holds workspace_root"));

  let mut crates = Vec::new();
  let mut path_failures = Vec::new();
  for package in json["packages"].as_array().expect("cargo metadata holds packages") {
    let name = package["name"].as_str().expect("a package has a name").to_string();
    let manifest_path = PathBuf::from(package["manifest_path"].as_str().expect("a package has a manifest path"));
    let dir = manifest_path.parent().expect("a manifest has a parent directory").to_path_buf();
    let dir_rel = rel(&root, &dir);
    let group = group_of_dir(&dir_rel);
    let deps = workspace_deps(package);
    let metas = target_metas(package);
    let root_target = root_target_index(&metas);
    let mut targets = Vec::new();
    for (index, meta) in metas.iter().enumerate() {
      let is_root = root_target == Some(index);
      targets.push(build_target(&root, &dir, meta, is_root, &mut path_failures));
    }
    crates.push(Crate { name, manifest_rel: rel(&root, &manifest_path), dir_rel, group, deps, targets });
  }
  crates.sort_by(|left, right| left.manifest_rel.cmp(&right.manifest_rel));
  Workspace { root, crates, path_failures }
}

fn workspace_deps(package: &serde_json::Value) -> BTreeSet<String> {
  let mut deps = BTreeSet::new();
  for dep in package["dependencies"].as_array().expect("a package holds dependencies") {
    if dep["kind"].as_str() == Some("dev") {
      continue;
    }
    deps.insert(dep["name"].as_str().expect("a dependency has a name").to_string());
  }
  deps
}

fn target_metas(package: &serde_json::Value) -> Vec<TargetMeta> {
  let mut metas = Vec::new();
  for target in package["targets"].as_array().expect("a package holds targets") {
    let kinds: Vec<&str> = target["kind"]
      .as_array()
      .expect("a target holds kinds")
      .iter()
      .map(|kind| kind.as_str().expect("a target kind is a string"))
      .collect();
    let kind = if kinds.iter().any(|kind| matches!(*kind, "lib" | "proc-macro" | "cdylib" | "staticlib" | "dylib")) {
      "lib"
    } else if kinds.contains(&"bin") {
      "bin"
    } else if kinds.contains(&"test") {
      "test"
    } else if kinds.contains(&"example") {
      "example"
    } else if kinds.contains(&"bench") {
      "bench"
    } else {
      continue;
    };
    metas.push(TargetMeta {
      kind: kind.to_string(),
      name: target["name"].as_str().expect("a target has a name").to_string(),
      src: PathBuf::from(target["src_path"].as_str().expect("a target has a source path")),
    });
  }
  metas.sort_by(|left, right| (&left.kind, &left.name).cmp(&(&right.kind, &right.name)));
  metas
}

/// The lib is the root target; a crate without a lib uses its main bin. Every other target is prefixed by its kind.
fn root_target_index(metas: &[TargetMeta]) -> Option<usize> {
  if let Some(index) = metas.iter().position(|meta| meta.kind == "lib") {
    return Some(index);
  }
  if let Some(index) =
    metas.iter().position(|meta| meta.kind == "bin" && meta.src.file_name().is_some_and(|name| name == "main.rs"))
  {
    return Some(index);
  }
  metas.iter().position(|meta| meta.kind == "bin").or(if metas.is_empty() { None } else { Some(0) })
}

fn build_target(
  workspace_root: &Path,
  crate_dir: &Path,
  meta: &TargetMeta,
  is_root: bool,
  path_failures: &mut Vec<String>,
) -> Target {
  let label = if is_root { "crate".to_string() } else { format!("{}.{}", meta.kind, meta.name) };
  let test_like = matches!(meta.kind.as_str(), "test" | "example" | "bench");
  let source = fs::read_to_string(&meta.src)
    .unwrap_or_else(|error| panic!("cannot read target root {}: {error}", meta.src.display()));
  let parsed = syn::parse_file(&source).unwrap_or_else(|error| panic!("cannot parse {}: {error}", meta.src.display()));
  let logical_dir = meta.src.parent().expect("a target root has a parent directory").to_path_buf();
  let mut builder = Builder { workspace_root, crate_dir, modules: BTreeMap::new(), path_failures };
  builder.process_scope(Vec::new(), &meta.src, &logical_dir, &parsed.items, test_like, false);
  Target { label, root: is_root, modules: builder.modules }
}

struct Builder<'a> {
  workspace_root: &'a Path,
  crate_dir: &'a Path,
  modules: BTreeMap<Vec<String>, Module>,
  path_failures: &'a mut Vec<String>,
}

impl Builder<'_> {
  /// Walk one module's own items. Inline `mod` blocks become child modules and their items stay out of this module.
  fn process_scope(
    &mut self,
    path: Vec<String>,
    file: &Path,
    logical_dir: &Path,
    items: &[Item],
    test: bool,
    inline: bool,
  ) {
    let mut collector = Collector::default();
    for item in items {
      collector.visit_item(item);
    }
    let child_mods = collector.child_mods;
    let children = child_mods.iter().map(|node| node.ident.to_string()).collect();
    self.modules.insert(
      path.clone(),
      Module {
        file: file.to_path_buf(),
        children,
        defs: collector.defs,
        reexports: collector.reexports,
        globs: collector.globs,
        uses: collector.uses,
        paths: collector.paths,
        macro_paths: collector.macro_paths,
        idents: collector.idents,
      },
    );

    for node in child_mods {
      let name = node.ident.to_string();
      let child_test = test || is_cfg_test(&node.attrs);
      let mut child_path = path.clone();
      child_path.push(name.clone());
      match &node.content {
        Some((_, child_items)) => {
          let child_logical = logical_dir.join(&name);
          self.process_scope(child_path, file, &child_logical, child_items, child_test, true);
        }
        None => {
          let found = self.resolve_module_file(file, logical_dir, inline, &name, &node.attrs, child_test);
          let child_logical = module_logical_dir(&found);
          let source = fs::read_to_string(&found).unwrap_or_else(|error| {
            panic!("cannot read {} for module {name} declared in {}: {error}", found.display(), file.display())
          });
          let parsed =
            syn::parse_file(&source).unwrap_or_else(|error| panic!("cannot parse {}: {error}", found.display()));
          self.process_scope(child_path, &found, &child_logical, &parsed.items, child_test, false);
        }
      }
    }
  }

  fn resolve_module_file(
    &mut self,
    file: &Path,
    logical_dir: &Path,
    inline: bool,
    name: &str,
    attrs: &[Attribute],
    test: bool,
  ) -> PathBuf {
    if let Some((path_text, line)) = path_attr(attrs) {
      let base = if inline { logical_dir } else { file.parent().expect("a source file has a parent directory") };
      let target = normalize(&base.join(path_text));
      if !test && !is_inside(self.crate_dir, &target) {
        self.path_failures.push(format!(
          "{}:{}: #[path] names {}, which is outside the crate directory {}; move the file into the crate.",
          rel(self.workspace_root, file),
          line,
          rel(self.workspace_root, &target),
          rel(self.workspace_root, self.crate_dir)
        ));
      }
      return target;
    }
    let plain = logical_dir.join(format!("{name}.rs"));
    if plain.is_file() {
      return plain;
    }
    let nested = logical_dir.join(name).join("mod.rs");
    if nested.is_file() {
      return nested;
    }
    panic!(
      "no source file for module {name} declared in {}: expected {} or {}",
      file.display(),
      plain.display(),
      nested.display()
    );
  }
}

/// The group of a crate directory relative to the workspace root: `crates/<group>/<name>`.
fn group_of_dir(dir_rel: &str) -> Option<String> {
  let parts: Vec<&str> = dir_rel.split('/').collect();
  if parts.len() != 3 || parts[0] != "crates" {
    return None;
  }
  match parts[1] {
    "infra" | "runtime" | "app" => Some(parts[1].to_string()),
    "backends" | "features" => Some(format!("{}.{}", parts[1], parts[2])),
    _ => None,
  }
}

/// The groups `group` may import besides itself; None for app, which may import every group.
fn allowed_groups(group: &str) -> Option<BTreeSet<String>> {
  let (kind, name) = match group.split_once('.') {
    Some((kind, name)) => (kind, name),
    None => (group, ""),
  };
  let mut allowed = BTreeSet::new();
  match kind {
    "app" => return None,
    "infra" => {}
    "runtime" => {
      allowed.insert("infra".to_string());
    }
    "backends" => {
      allowed.insert("infra".to_string());
      allowed.insert("runtime".to_string());
      if BACKEND_VARIANTS.contains(&name) {
        allowed.insert("backends.claude_code".to_string());
      }
    }
    "features" => {
      allowed.insert("infra".to_string());
      allowed.insert("runtime".to_string());
      if CHAT_CHANNELS.contains(&name) {
        allowed.insert("features.chat_threads".to_string());
      }
    }
    _ => panic!("unknown group {group}"),
  }
  Some(allowed)
}

// ---------------------------------------------------------------------------------------------------------------------
// The group, direction and layer checks.

fn group_failures(workspace: &Workspace) -> Vec<String> {
  workspace
    .crates
    .iter()
    .filter(|item| item.group.is_none())
    .map(|item| {
      format!(
        "{}: sits outside the groups rust/crates/infra, rust/crates/runtime, rust/crates/backends/<name>, \
         rust/crates/features/<name>, rust/crates/app; move it into one.",
        item.manifest_rel
      )
    })
    .collect()
}

fn direction_failures(workspace: &Workspace) -> Vec<String> {
  let by_name: BTreeMap<&str, &Crate> = workspace.crates.iter().map(|item| (item.name.as_str(), item)).collect();
  let mut failures = Vec::new();
  for item in &workspace.crates {
    let Some(group) = &item.group else { continue };
    let allowed = allowed_groups(group);
    for dep_name in &item.deps {
      let Some(dep) = by_name.get(dep_name.as_str()) else { continue };
      let Some(dep_group) = &dep.group else { continue };
      if dep_group == group {
        continue;
      }
      if let Some(allowed) = &allowed
        && !allowed.contains(dep_group.as_str())
      {
        let mut may = allowed.clone();
        may.insert(group.clone());
        failures.push(format!(
          "{}: group {} depends on {} (group {}), and {} may depend only on {}. {}",
          item.manifest_rel,
          group,
          dep.name,
          dep_group,
          group,
          may.into_iter().collect::<Vec<_>>().join(", "),
          DEPEND_ON_THE_RUNTIME
        ));
      }
    }
  }
  failures
}

fn layer_of(item: &Crate) -> Option<(&'static str, usize)> {
  if item.group.as_deref() != Some("runtime") {
    return None;
  }
  let dir_name = item.dir_rel.rsplit('/').next().unwrap_or(&item.dir_rel);
  RUNTIME_LAYERS.iter().find(|(name, _)| *name == dir_name).map(|(name, layer)| (*name, *layer))
}

fn layer_break_failures(workspace: &Workspace) -> Vec<String> {
  let by_name: BTreeMap<&str, &Crate> = workspace.crates.iter().map(|item| (item.name.as_str(), item)).collect();
  let mut failures = Vec::new();
  for item in &workspace.crates {
    let Some((source_name, source_layer)) = layer_of(item) else { continue };
    for dep_name in &item.deps {
      let Some(dep) = by_name.get(dep_name.as_str()) else { continue };
      let Some((target_name, target_layer)) = layer_of(dep) else { continue };
      if target_layer > source_layer {
        failures.push(format!(
          "{}: layer {} ({}) crate {} depends on {} (layer {}, {}); layer {} may depend only on layers 1 to {}. {}",
          item.manifest_rel,
          source_layer,
          source_name,
          item.name,
          dep.name,
          target_layer,
          target_name,
          source_layer,
          source_layer,
          FIX_THE_UPWARD_IMPORT
        ));
      }
    }
  }
  failures
}

fn unplaced_runtime_failures(workspace: &Workspace) -> Vec<String> {
  workspace
    .crates
    .iter()
    .filter(|item| item.group.as_deref() == Some("runtime") && layer_of(item).is_none())
    .map(|item| {
      format!(
        "{}: no layer in the runtime layer table; add it to the layer of the modules it serves.",
        item.manifest_rel
      )
    })
    .collect()
}

fn stale_layer_failures(workspace: &Workspace) -> Vec<String> {
  RUNTIME_LAYERS
    .iter()
    .filter(|(name, _)| {
      !workspace
        .crates
        .iter()
        .any(|item| item.group.as_deref() == Some("runtime") && item.dir_rel.rsplit('/').next() == Some(*name))
    })
    .map(|(name, _)| format!("{name}: names no runtime module; remove it from the layer table."))
    .collect()
}

// ---------------------------------------------------------------------------------------------------------------------
// The module graph.

const MAX_RESOLVE_DEPTH: usize = 32;

impl Crate {
  fn target(&self, target: usize) -> &Target {
    &self.targets[target]
  }

  /// The module a path written in `cur` resolves to, or None when its first segment names no module.
  fn resolve(&self, target: usize, cur: &[String], segs: &[String]) -> Option<Vec<String>> {
    self.resolve_at(target, cur, segs, 0)
  }

  fn resolve_at(&self, target: usize, cur: &[String], segs: &[String], depth: usize) -> Option<Vec<String>> {
    if depth > MAX_RESOLVE_DEPTH || segs.is_empty() {
      return if segs.is_empty() { Some(cur.to_vec()) } else { None };
    }
    match segs[0].as_str() {
      "crate" => self.lookup(target, &[], &segs[1..], depth),
      "self" | "super" => {
        let mut base = cur.to_vec();
        let mut index = 0;
        while index < segs.len() && (segs[index] == "self" || segs[index] == "super") {
          if segs[index] == "super" {
            base.pop()?;
          }
          index += 1;
        }
        self.lookup(target, &base, &segs[index..], depth)
      }
      _ => self.lookup(target, cur, segs, depth),
    }
  }

  /// Follow `segs` from `base` through child modules, re-exports and globs to the module that defines the item.
  fn lookup(&self, target: usize, base: &[String], segs: &[String], depth: usize) -> Option<Vec<String>> {
    if depth > MAX_RESOLVE_DEPTH {
      return None;
    }
    let module = self.target(target).modules.get(base)?;
    if segs.is_empty() {
      return Some(base.to_vec());
    }
    let first = &segs[0];
    if module.children.contains(first) {
      let mut child = base.to_vec();
      child.push(first.clone());
      if segs.len() == 1 {
        return Some(child);
      }
      return self.lookup(target, &child, &segs[1..], depth);
    }
    if let Some(path) = module.reexports.get(first) {
      let resolved = self.resolve_at(target, base, path, depth + 1)?;
      return self.lookup(target, &resolved, &segs[1..], depth + 1);
    }
    if module.defs.contains(first) {
      return Some(base.to_vec());
    }
    for glob in &module.globs {
      if let Some(resolved) = self.resolve_at(target, base, glob, depth + 1)
        && let Some(glob_module) = self.target(target).modules.get(&resolved)
        && (glob_module.children.contains(first)
          || glob_module.reexports.contains_key(first)
          || glob_module.defs.contains(first))
      {
        return self.lookup(target, &resolved, segs, depth + 1);
      }
    }
    None
  }
}

fn module_name(target: &Target, path: &[String]) -> String {
  if target.root {
    if path.is_empty() { "crate".to_string() } else { path.join("::") }
  } else if path.is_empty() {
    target.label.clone()
  } else {
    format!("{}::{}", target.label, path.join("::"))
  }
}

/// The modules `module`'s own code points at, with the line of the first reference to each.
fn module_outgoing(item: &Crate, target: usize, path: &[String], module: &Module) -> Vec<(Vec<String>, usize)> {
  let mut outgoing = Vec::new();
  for record in &module.uses {
    if record.is_pub
      && !record.glob
      && let Some(name) = &record.name
      && !module.idents.contains(name)
    {
      continue;
    }
    if let Some(resolved) = item.resolve(target, path, &record.path) {
      outgoing.push((resolved, record.line));
    }
  }
  for record in &module.paths {
    if let Some(resolved) = item.resolve(target, path, &record.segs) {
      outgoing.push((resolved, record.line));
    }
  }
  for record in &module.macro_paths {
    if let Some(resolved) = item.resolve(target, path, &record.segs) {
      outgoing.push((resolved, record.line));
    }
  }
  outgoing
}

struct Cycle {
  package: String,
  members: Vec<String>,
  /// `(from, to, file relative to the workspace root, line)`.
  edges: Vec<(String, String, String, usize)>,
}

impl Cycle {
  fn message(&self) -> String {
    let mut lines = Vec::with_capacity(self.edges.len() + 2);
    lines.push(format!(
      "module cycle in {} among {} modules: {}",
      self.package,
      self.members.len(),
      self.members.join(", ")
    ));
    for (from, to, file, line) in &self.edges {
      lines.push(format!("  {file}:{line} {from} -> {to}"));
    }
    lines.push("Move the shared definitions to a leaf module that both use, or merge the modules.".to_string());
    lines.join("\n")
  }
}

fn detect_cycles(workspace: &Workspace) -> Vec<Cycle> {
  let mut cycles = Vec::new();
  for item in &workspace.crates {
    let mut nodes = BTreeSet::new();
    let mut edges: BTreeMap<(String, String), (String, usize)> = BTreeMap::new();
    for (target_index, target) in item.targets.iter().enumerate() {
      for (path, module) in &target.modules {
        let from = module_name(target, path);
        nodes.insert(from.clone());
        for (resolved, line) in module_outgoing(item, target_index, path, module) {
          if resolved == *path {
            continue;
          }
          let to = module_name(target, &resolved);
          nodes.insert(to.clone());
          let file = rel(&workspace.root, &module.file);
          edges
            .entry((from.clone(), to))
            .and_modify(|(retained_file, retained_line)| {
              if line < *retained_line {
                *retained_file = file.clone();
                *retained_line = line;
              }
            })
            .or_insert((file, line));
        }
      }
    }
    let mut graph: BTreeMap<String, Vec<String>> = nodes.into_iter().map(|node| (node, Vec::new())).collect();
    for (from, to) in edges.keys() {
      graph.get_mut(from).expect("an edge starts at a known module").push(to.clone());
    }
    for neighbors in graph.values_mut() {
      neighbors.sort();
      neighbors.dedup();
    }
    for component in strongly_connected(&graph) {
      let members: BTreeSet<&String> = component.iter().collect();
      let mut cycle_edges: Vec<(String, String, String, usize)> = edges
        .iter()
        .filter(|((from, to), _)| members.contains(from) && members.contains(to))
        .map(|((from, to), (file, line))| (from.clone(), to.clone(), file.clone(), *line))
        .collect();
      cycle_edges.sort();
      cycles.push(Cycle { package: item.name.clone(), members: component, edges: cycle_edges });
    }
  }
  cycles.sort_by(|left, right| (&left.package, &left.members).cmp(&(&right.package, &right.members)));
  cycles
}

/// The groups of two or more modules that all reach each other (Tarjan).
fn strongly_connected(graph: &BTreeMap<String, Vec<String>>) -> Vec<Vec<String>> {
  struct State<'a> {
    graph: &'a BTreeMap<String, Vec<String>>,
    index: BTreeMap<String, usize>,
    low: BTreeMap<String, usize>,
    stack: Vec<String>,
    on_stack: BTreeSet<String>,
    next: usize,
    components: Vec<Vec<String>>,
  }

  fn visit(node: &str, state: &mut State<'_>) {
    let number = state.next;
    state.index.insert(node.to_string(), number);
    state.low.insert(node.to_string(), number);
    state.next += 1;
    state.stack.push(node.to_string());
    state.on_stack.insert(node.to_string());
    let neighbors = state.graph.get(node).cloned().unwrap_or_default();
    for neighbor in neighbors {
      if !state.index.contains_key(&neighbor) {
        visit(&neighbor, state);
        let lower = state.low[node].min(state.low[&neighbor]);
        state.low.insert(node.to_string(), lower);
      } else if state.on_stack.contains(&neighbor) {
        let lower = state.low[node].min(state.index[&neighbor]);
        state.low.insert(node.to_string(), lower);
      }
    }
    if state.low[node] == state.index[node] {
      let mut component = Vec::new();
      loop {
        let member = state.stack.pop().expect("the stack holds the node itself");
        state.on_stack.remove(&member);
        component.push(member.clone());
        if member == node {
          break;
        }
      }
      if component.len() > 1 {
        component.sort();
        state.components.push(component);
      }
    }
  }

  let mut state = State {
    graph,
    index: BTreeMap::new(),
    low: BTreeMap::new(),
    stack: Vec::new(),
    on_stack: BTreeSet::new(),
    next: 0,
    components: Vec::new(),
  };
  for node in graph.keys() {
    if !state.index.contains_key(node) {
      visit(node, &mut state);
    }
  }
  state.components.sort();
  state.components
}

fn cycle_failures(workspace: &Workspace) -> Vec<String> {
  detect_cycles(workspace).iter().map(Cycle::message).collect()
}

// ---------------------------------------------------------------------------------------------------------------------
// The real workspace.

fn real_workspace() -> &'static Workspace {
  static WORKSPACE: OnceLock<Workspace> = OnceLock::new();
  WORKSPACE.get_or_init(|| load_workspace(Path::new(env!("CARGO_MANIFEST_DIR"))))
}

#[test]
fn every_crate_is_in_a_group() {
  let failures = group_failures(real_workspace());
  assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn dependencies_follow_the_group_directions() {
  let failures = direction_failures(real_workspace());
  assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn no_runtime_crate_depends_on_a_higher_layer() {
  let failures = layer_break_failures(real_workspace());
  assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn every_runtime_crate_has_a_layer() {
  let failures = unplaced_runtime_failures(real_workspace());
  assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn every_layer_names_a_runtime_crate() {
  let failures = stale_layer_failures(real_workspace());
  assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn no_crate_has_a_module_cycle() {
  let failures = cycle_failures(real_workspace());
  assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn no_production_path_attribute_reaches_outside_its_crate() {
  let failures = &real_workspace().path_failures;
  assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// ---------------------------------------------------------------------------------------------------------------------
// The synthetic workspaces.

fn write_tree<S: AsRef<str>>(root: &Path, files: &[(S, String)]) {
  for (relative, content) in files {
    let path = root.join(relative.as_ref());
    fs::create_dir_all(path.parent().expect("a source file has a parent directory")).expect("create a directory");
    fs::write(&path, content).expect("write a file");
  }
}

fn workspace_manifest(members: &[&str]) -> String {
  let mut text = String::from("[workspace]\nresolver = \"2\"\nmembers = [\n");
  for member in members {
    text.push_str(&format!("  \"{member}\",\n"));
  }
  text.push_str("]\n");
  text
}

fn crate_manifest(name: &str, deps: &[(&str, &str)], dev_deps: &[(&str, &str)]) -> String {
  let mut text = format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n");
  if !deps.is_empty() {
    text.push_str("\n[dependencies]\n");
    for (dep, path) in deps {
      text.push_str(&format!("{dep} = {{ path = \"{path}\" }}\n"));
    }
  }
  if !dev_deps.is_empty() {
    text.push_str("\n[dev-dependencies]\n");
    for (dep, path) in dev_deps {
      text.push_str(&format!("{dep} = {{ path = \"{path}\" }}\n"));
    }
  }
  text
}

fn cycle_of<'a>(cycles: &'a [Cycle], package: &str) -> Option<&'a Cycle> {
  cycles.iter().find(|cycle| cycle.package == package)
}

/// One test builds small synthetic workspaces and checks one failure per case. A check that skips a form of break
/// would let that break into the workspace unseen.
#[test]
fn the_checks_find_each_break() {
  let groups_dir = tempfile::tempdir().expect("create a temporary directory");
  let groups_root = groups_dir.path();
  let groups_members = [
    "crates/features/alpha",
    "crates/features/beta",
    "crates/runtime/store",
    "crates/runtime/registry",
    "crates/runtime/usage",
    "crates/runtime/agents",
    "crates/runtime/tasks",
    "crates/runtime/triggers",
    "crates/runtime/mystery",
    "crates/misc/loose",
  ];
  let mut groups_files: Vec<(String, String)> = vec![("Cargo.toml".to_string(), workspace_manifest(&groups_members))];
  for member in groups_members {
    groups_files.push((format!("{member}/src/lib.rs"), String::new()));
  }
  groups_files.extend(
    [
      ("crates/features/alpha/Cargo.toml", crate_manifest("alpha", &[("beta", "../beta")], &[])),
      ("crates/features/beta/Cargo.toml", crate_manifest("beta", &[], &[])),
      ("crates/runtime/store/Cargo.toml", crate_manifest("store-crate", &[("triggers-crate", "../triggers")], &[])),
      ("crates/runtime/registry/Cargo.toml", crate_manifest("registry-crate", &[], &[])),
      ("crates/runtime/usage/Cargo.toml", crate_manifest("usage-crate", &[], &[])),
      ("crates/runtime/agents/Cargo.toml", crate_manifest("agents-crate", &[], &[])),
      ("crates/runtime/tasks/Cargo.toml", crate_manifest("tasks-crate", &[], &[])),
      ("crates/runtime/triggers/Cargo.toml", crate_manifest("triggers-crate", &[], &[])),
      ("crates/runtime/mystery/Cargo.toml", crate_manifest("mystery-crate", &[], &[])),
      ("crates/misc/loose/Cargo.toml", crate_manifest("loose-crate", &[], &[])),
    ]
    .map(|(path, content)| (path.to_string(), content)),
  );
  write_tree(groups_root, &groups_files);
  let groups = load_workspace(groups_root);

  assert_eq!(
    direction_failures(&groups),
    vec![
      "crates/features/alpha/Cargo.toml: group features.alpha depends on beta (group features.beta), and \
       features.alpha may depend only on features.alpha, infra, runtime. Depend on the registry: the imported crate \
       registers its implementation, and the importer looks it up there."
        .to_string()
    ],
    "case 1: a feature that depends on another feature"
  );
  assert_eq!(
    layer_break_failures(&groups),
    vec![format!(
      "crates/runtime/store/Cargo.toml: layer 1 (store) crate store-crate depends on triggers-crate (layer 6, \
       triggers); layer 1 may depend only on layers 1 to 1. {FIX_THE_UPWARD_IMPORT}"
    )],
    "case 2: a runtime crate that depends on a higher layer"
  );
  assert_eq!(
    group_failures(&groups),
    vec![
      "crates/misc/loose/Cargo.toml: sits outside the groups rust/crates/infra, rust/crates/runtime, \
       rust/crates/backends/<name>, rust/crates/features/<name>, rust/crates/app; move it into one."
        .to_string()
    ],
    "case 3: a crate outside every group"
  );
  assert_eq!(
    unplaced_runtime_failures(&groups),
    vec![
      "crates/runtime/mystery/Cargo.toml: no layer in the runtime layer table; add it to the layer of the modules it \
       serves."
        .to_string()
    ],
    "case 4: the layer table misses a runtime crate"
  );
  assert_eq!(
    stale_layer_failures(&groups),
    vec!["session-api: names no runtime module; remove it from the layer table.".to_string()],
    "case 5: the layer table names a crate that does not exist"
  );

  let modules_dir = tempfile::tempdir().expect("create a temporary directory");
  let modules_root = modules_dir.path();
  let mut files = vec![(
    "Cargo.toml",
    workspace_manifest(&[
      "crates/infra/file_cycle",
      "crates/infra/inline_cycle",
      "crates/infra/macro_cycle",
      "crates/infra/reexport_only",
      "crates/features/path_outer",
      "crates/features/path_inner",
      "crates/features/dev_dep",
      "crates/features/another_feature",
    ]),
  )];
  files.push(("crates/infra/file_cycle/Cargo.toml", crate_manifest("file-cycle", &[], &[])));
  files.push(("crates/infra/file_cycle/src/lib.rs", "mod a;\nmod b;\n".to_string()));
  files.push((
    "crates/infra/file_cycle/src/a.rs",
    "use crate::b::B;\n\npub struct A;\n\npub fn touch(_: B) {}\n".to_string(),
  ));
  files.push((
    "crates/infra/file_cycle/src/b.rs",
    "use crate::a::A;\n\npub struct B;\n\npub fn touch(_: A) {}\n".to_string(),
  ));
  files.push(("crates/infra/inline_cycle/Cargo.toml", crate_manifest("inline-cycle", &[], &[])));
  files.push((
    "crates/infra/inline_cycle/src/lib.rs",
    "mod helper;\n\n#[cfg(test)]\nmod tests {\n  use crate::helper::Helper;\n  pub struct Thing;\n}\n".to_string(),
  ));
  files.push((
    "crates/infra/inline_cycle/src/helper.rs",
    "use crate::tests::Thing;\n\npub struct Helper;\n\npub fn touch(_: Thing) {}\n".to_string(),
  ));
  files.push(("crates/infra/macro_cycle/Cargo.toml", crate_manifest("macro-cycle", &[], &[])));
  files.push(("crates/infra/macro_cycle/src/lib.rs", "mod m;\nmod n;\n".to_string()));
  files.push((
    "crates/infra/macro_cycle/src/m.rs",
    "pub struct M;\n\npub fn f() {\n  some_macro!(crate::n::N);\n}\n".to_string(),
  ));
  files.push((
    "crates/infra/macro_cycle/src/n.rs",
    "pub struct N;\n\npub fn g() {\n  let _ = crate::m::M;\n}\n".to_string(),
  ));
  files.push(("crates/infra/reexport_only/Cargo.toml", crate_manifest("reexport-only", &[], &[])));
  files.push(("crates/infra/reexport_only/src/lib.rs", "pub mod a;\npub mod b;\n".to_string()));
  files.push((
    "crates/infra/reexport_only/src/a.rs",
    "pub use crate::b::Thing;\n\npub const SOMETHING: u32 = 1;\n".to_string(),
  ));
  files.push((
    "crates/infra/reexport_only/src/b.rs",
    "use crate::a::SOMETHING;\n\npub struct Thing;\n\npub fn touch() {\n  let _ = SOMETHING;\n}\n".to_string(),
  ));
  files.push(("crates/features/path_outer/Cargo.toml", crate_manifest("path-outer", &[], &[])));
  files.push((
    "crates/features/path_outer/src/lib.rs",
    "#[path = \"../../path_inner/src/shared.rs\"]\nmod shared;\n\npub fn f() -> u32 {\n  shared::VALUE\n}\n"
      .to_string(),
  ));
  files.push(("crates/features/path_inner/Cargo.toml", crate_manifest("path-inner", &[], &[])));
  files.push(("crates/features/path_inner/src/lib.rs", String::new()));
  files.push(("crates/features/path_inner/src/shared.rs", "pub const VALUE: u32 = 7;\n".to_string()));
  files.push((
    "crates/features/dev_dep/Cargo.toml",
    crate_manifest("dev-dep", &[], &[("another-feature", "../another_feature")]),
  ));
  files.push(("crates/features/dev_dep/src/lib.rs", String::new()));
  files.push(("crates/features/another_feature/Cargo.toml", crate_manifest("another-feature", &[], &[])));
  files.push(("crates/features/another_feature/src/lib.rs", String::new()));
  write_tree(modules_root, &files);
  let modules = load_workspace(modules_root);
  let cycles = detect_cycles(&modules);

  assert_eq!(
    cycle_of(&cycles, "file-cycle").map(Cycle::message),
    Some(
      "module cycle in file-cycle among 2 modules: a, b\n  \
       crates/infra/file_cycle/src/a.rs:1 a -> b\n  \
       crates/infra/file_cycle/src/b.rs:1 b -> a\n\
       Move the shared definitions to a leaf module that both use, or merge the modules."
        .to_string()
    ),
    "case 6: a cycle of file modules"
  );
  assert_eq!(
    cycle_of(&cycles, "inline-cycle").map(Cycle::message),
    Some(
      "module cycle in inline-cycle among 2 modules: helper, tests\n  \
       crates/infra/inline_cycle/src/helper.rs:1 helper -> tests\n  \
       crates/infra/inline_cycle/src/lib.rs:5 tests -> helper\n\
       Move the shared definitions to a leaf module that both use, or merge the modules."
        .to_string()
    ),
    "case 7: a cycle through an inline #[cfg(test)] module"
  );
  assert_eq!(
    cycle_of(&cycles, "macro-cycle").map(Cycle::message),
    Some(
      "module cycle in macro-cycle among 2 modules: m, n\n  \
       crates/infra/macro_cycle/src/m.rs:4 m -> n\n  \
       crates/infra/macro_cycle/src/n.rs:4 n -> m\n\
       Move the shared definitions to a leaf module that both use, or merge the modules."
        .to_string()
    ),
    "case 8: a cycle through a path inside a macro call"
  );
  assert!(cycle_of(&cycles, "reexport-only").is_none(), "case 9: a module that only re-exports adds no cycle");
  assert_eq!(
    modules.path_failures,
    vec![
      "crates/features/path_outer/src/lib.rs:1: #[path] names crates/features/path_inner/src/shared.rs, which is \
       outside the crate directory crates/features/path_outer; move the file into the crate."
        .to_string()
    ],
    "case 10: a production #[path] that names a file in another crate"
  );
  assert!(
    !direction_failures(&modules).iter().any(|failure| failure.contains("crates/features/dev_dep/Cargo.toml")),
    "case 11: a dev-dependency across groups passes"
  );
}

// ---------------------------------------------------------------------------------------------------------------------
// Parsing helpers.

#[derive(Default)]
struct Collector {
  defs: BTreeSet<String>,
  reexports: BTreeMap<String, Vec<String>>,
  globs: Vec<Vec<String>>,
  uses: Vec<UseRecord>,
  paths: Vec<PathRecord>,
  macro_paths: Vec<PathRecord>,
  idents: BTreeSet<String>,
  child_mods: Vec<ItemMod>,
}

impl Collector {
  fn record_use(&mut self, node: &ItemUse) {
    let is_pub = !matches!(node.vis, syn::Visibility::Inherited);
    let line = node.use_token.span.start().line;
    let mut flattened = Vec::new();
    flatten_use(&node.tree, &mut Vec::new(), &mut flattened);
    for item in flattened {
      match item {
        FlatUse::Name { path, name } => {
          self.reexports.insert(name.clone(), path.clone());
          self.uses.push(UseRecord { path, name: Some(name), glob: false, is_pub, line });
        }
        FlatUse::Glob { path } => {
          self.globs.push(path.clone());
          self.uses.push(UseRecord { path, name: None, glob: true, is_pub, line });
        }
      }
    }
  }
}

impl<'ast> Visit<'ast> for Collector {
  fn visit_item_mod(&mut self, node: &'ast ItemMod) {
    // The `mod` declaration is no edge, and an inline block is its own module.
    self.child_mods.push(node.clone());
  }

  fn visit_visibility(&mut self, _node: &'ast syn::Visibility) {
    // `pub(crate)` and `pub(super)` hold a path that names no module reference.
  }

  fn visit_item_use(&mut self, node: &'ast ItemUse) {
    self.record_use(node);
  }

  fn visit_path(&mut self, path: &'ast syn::Path) {
    let segs = path_segments(path);
    if !segs.is_empty() {
      self.idents.extend(segs.iter().cloned());
      self.paths.push(PathRecord { segs, line: path.span().start().line });
    }
    syn::visit::visit_path(self, path);
  }

  fn visit_macro(&mut self, node: &'ast syn::Macro) {
    let line = node.path.span().start().line;
    scan_tokens(&node.tokens, line, &mut self.macro_paths, &mut self.idents);
    syn::visit::visit_macro(self, node);
  }

  fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
    self.defs.insert(node.sig.ident.to_string());
    syn::visit::visit_item_fn(self, node);
  }

  fn visit_item_struct(&mut self, node: &'ast syn::ItemStruct) {
    self.defs.insert(node.ident.to_string());
    syn::visit::visit_item_struct(self, node);
  }

  fn visit_item_enum(&mut self, node: &'ast syn::ItemEnum) {
    self.defs.insert(node.ident.to_string());
    syn::visit::visit_item_enum(self, node);
  }

  fn visit_item_trait(&mut self, node: &'ast syn::ItemTrait) {
    self.defs.insert(node.ident.to_string());
    syn::visit::visit_item_trait(self, node);
  }

  fn visit_item_trait_alias(&mut self, node: &'ast syn::ItemTraitAlias) {
    self.defs.insert(node.ident.to_string());
    syn::visit::visit_item_trait_alias(self, node);
  }

  fn visit_item_type(&mut self, node: &'ast syn::ItemType) {
    self.defs.insert(node.ident.to_string());
    syn::visit::visit_item_type(self, node);
  }

  fn visit_item_const(&mut self, node: &'ast syn::ItemConst) {
    self.defs.insert(node.ident.to_string());
    syn::visit::visit_item_const(self, node);
  }

  fn visit_item_static(&mut self, node: &'ast syn::ItemStatic) {
    self.defs.insert(node.ident.to_string());
    syn::visit::visit_item_static(self, node);
  }

  fn visit_item_union(&mut self, node: &'ast syn::ItemUnion) {
    self.defs.insert(node.ident.to_string());
    syn::visit::visit_item_union(self, node);
  }

  fn visit_item_macro(&mut self, node: &'ast ItemMacro) {
    if let Some(ident) = &node.ident {
      self.defs.insert(ident.to_string());
    }
    syn::visit::visit_item_macro(self, node);
  }

  fn visit_foreign_item_fn(&mut self, node: &'ast syn::ForeignItemFn) {
    self.defs.insert(node.sig.ident.to_string());
    syn::visit::visit_foreign_item_fn(self, node);
  }

  fn visit_foreign_item_static(&mut self, node: &'ast syn::ForeignItemStatic) {
    self.defs.insert(node.ident.to_string());
    syn::visit::visit_foreign_item_static(self, node);
  }

  fn visit_foreign_item_type(&mut self, node: &'ast syn::ForeignItemType) {
    self.defs.insert(node.ident.to_string());
    syn::visit::visit_foreign_item_type(self, node);
  }
}

enum FlatUse {
  Name { path: Vec<String>, name: String },
  Glob { path: Vec<String> },
}

fn flatten_use(tree: &UseTree, prefix: &mut Vec<String>, out: &mut Vec<FlatUse>) {
  match tree {
    UseTree::Path(path) => {
      prefix.push(path.ident.to_string());
      flatten_use(&path.tree, prefix, out);
      prefix.pop();
    }
    UseTree::Name(name) => {
      if name.ident == "self" {
        if let Some(last) = prefix.last() {
          out.push(FlatUse::Name { path: prefix.clone(), name: last.clone() });
        }
      } else {
        let mut path = prefix.clone();
        path.push(name.ident.to_string());
        out.push(FlatUse::Name { path, name: name.ident.to_string() });
      }
    }
    UseTree::Rename(rename) => {
      let path = if rename.ident == "self" {
        prefix.clone()
      } else {
        let mut path = prefix.clone();
        path.push(rename.ident.to_string());
        path
      };
      if !path.is_empty() {
        out.push(FlatUse::Name { path, name: rename.rename.to_string() });
      }
    }
    UseTree::Glob(_) => out.push(FlatUse::Glob { path: prefix.clone() }),
    UseTree::Group(group) => {
      for item in &group.items {
        flatten_use(item, prefix, out);
      }
    }
  }
}

fn path_segments(path: &syn::Path) -> Vec<String> {
  path.segments.iter().map(|segment| segment.ident.to_string()).collect()
}

/// Collect every `crate::`, `super::` and `self::` sequence in a macro call's tokens; syn expands no macro.
fn scan_tokens(tokens: &TokenStream, line: usize, paths: &mut Vec<PathRecord>, idents: &mut BTreeSet<String>) {
  let trees: Vec<TokenTree> = tokens.clone().into_iter().collect();
  let mut index = 0;
  while index < trees.len() {
    match &trees[index] {
      TokenTree::Ident(ident) => {
        let text = ident.to_string();
        idents.insert(text.clone());
        let dollar_prefixed =
          index > 0 && matches!(&trees[index - 1], TokenTree::Punct(punct) if punct.as_char() == '$');
        if !dollar_prefixed && matches!(text.as_str(), "crate" | "super" | "self") {
          let mut segs = vec![text];
          let mut cursor = index + 1;
          while cursor + 2 < trees.len() && is_path_separator(&trees[cursor], &trees[cursor + 1]) {
            let TokenTree::Ident(next) = &trees[cursor + 2] else { break };
            idents.insert(next.to_string());
            segs.push(next.to_string());
            cursor += 3;
          }
          if segs.len() > 1 {
            paths.push(PathRecord { segs, line });
          }
        }
      }
      TokenTree::Group(group) => scan_tokens(&group.stream(), line, paths, idents),
      _ => {}
    }
    index += 1;
  }
}

fn is_path_separator(first: &TokenTree, second: &TokenTree) -> bool {
  matches!(first, TokenTree::Punct(punct) if punct.as_char() == ':' && punct.spacing() == Spacing::Joint)
    && matches!(second, TokenTree::Punct(punct) if punct.as_char() == ':')
}

fn is_cfg_test(attrs: &[Attribute]) -> bool {
  attrs.iter().any(|attr| {
    attr.path().is_ident("cfg")
      && match &attr.meta {
        Meta::List(list) => list.tokens.to_string().replace(' ', "") == "test",
        _ => false,
      }
  })
}

fn path_attr(attrs: &[Attribute]) -> Option<(String, usize)> {
  for attr in attrs {
    if !attr.path().is_ident("path") {
      continue;
    }
    let Meta::NameValue(name_value) = &attr.meta else { continue };
    let syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(value), .. }) = &name_value.value else {
      continue;
    };
    return Some((value.value(), attr.path().span().start().line));
  }
  None
}

// ---------------------------------------------------------------------------------------------------------------------
// Path helpers.

/// The logical directory a module's child declarations resolve against: a `mod.rs` file owns its own directory, and
/// any other module file owns the directory named after it.
fn module_logical_dir(file: &Path) -> PathBuf {
  if file.file_name().is_some_and(|name| name == "mod.rs") {
    return file.parent().expect("a module file has a parent directory").to_path_buf();
  }
  let stem = file.file_stem().expect("a module file has a stem");
  file.parent().expect("a module file has a parent directory").join(stem)
}

fn is_inside(directory: &Path, file: &Path) -> bool {
  normalize(file).starts_with(normalize(directory))
}

fn normalize(path: &Path) -> PathBuf {
  let mut normalized = PathBuf::new();
  for component in path.components() {
    match component {
      Component::CurDir => {}
      Component::ParentDir => {
        normalized.pop();
      }
      other => normalized.push(other.as_os_str()),
    }
  }
  normalized
}

fn rel(root: &Path, path: &Path) -> String {
  path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/")
}
