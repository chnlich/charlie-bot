//! Workspace structure check: the group of each crate, the dependencies between groups, the runtime layer order, and
//! the module cycles inside each crate.
//!
//! A crate's group comes from its directory under `rust/crates`. The names map as `infra/<name>` to `infra`,
//! `runtime/<name>` to `runtime`, `backends/<name>` to `backends.<name>`, `features/<name>` to `features.<name>`,
//! and `app/<name>` to `app`. Any other directory fails.
//!
//! Directions: infra depends on infra only. runtime depends on runtime and infra. A backend or a feature depends on
//! itself, runtime and infra. The backends in `BACKEND_VARIANTS` may also depend on backends.claude_code, and the
//! features in `CHAT_CHANNELS` may also depend on features.chat_threads. app depends on every group. Normal and
//! build dependencies count; a dev-dependency does not, because test code follows no direction.
//!
//! Inside runtime, every crate is one layer. A runtime crate depends only on its own layer or a lower layer. The table
//! must cover every runtime crate and name no missing one.
//!
//! Each crate's module graph covers every target: its lib (or its main bin), each other bin, test, example and bench.
//! A node is the target root, a file module or an inline `mod` block. An edge A -> B means A's own code holds a path
//! that resolves to an item defined in B. The path reaches through `use` trees, expressions, types, patterns and the
//! tokens of macro calls. A `mod` declaration is no edge, and a `pub use` that only re-exports a name is no edge until
//! the module's own code uses that name. A cycle is a strongly connected component of two or more modules.
//!
//! Every module cycle fails: a cycle is a defect of the code, not a recorded exception.
//!
//! A name that two crates must agree on has one owner: the owner table names the owner crate, and no other crate's
//! production code may spell the name. A class of names, such as `CHARLIEBOT_` environment variables, must be spelled
//! in one crate. Both rules read the string literals of each production module; a literal with whitespace is prose and
//! names nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use proc_macro2::{Delimiter, Spacing, TokenStream, TokenTree};
use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::{
  Arm, Attribute, Expr, ExprCall, ExprLit, ExprMethodCall, Field, FieldValue, ImplItem, ImplItemFn, Item, ItemFn,
  ItemMacro, ItemMod, ItemUse, Lit, LitStr, Local, Meta, StmtMacro, TraitItemFn, UseTree,
};

const BACKEND_VARIANTS: [&str; 2] = ["anthropic_compatible", "openai_compatible"];
const CHAT_CHANNELS: [&str; 2] = ["slack", "discord"];

/// The runtime layers, keyed by the crate's directory name under `rust/crates/runtime`.
const RUNTIME_LAYERS: [(&str, usize); 8] = [
  ("store", 1),
  ("registry", 2),
  ("user_text_checks", 3),
  ("usage", 3),
  ("agents", 4),
  ("tasks", 5),
  ("triggers", 6),
  ("session-api", 7),
];

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
  /// The string literals of the module's production code; empty for a test module.
  literals: Vec<Literal>,
  /// The production functions of the module's own items; empty for a test module.
  functions: Vec<FunctionRecord>,
  /// The production statics of the module's own items and macro invocations; empty for a test module.
  statics: Vec<StaticRecord>,
  /// The production `use` items that import a process-environment writer; empty for a test module.
  environment_uses: Vec<EnvironmentUse>,
}

struct Literal {
  value: String,
  line: usize,
  /// The literal is the argument of `.join(..)`, `Path::new(..)` or `PathBuf::from(..)`.
  path_arg: bool,
  /// The literal is the argument of `.strip_prefix(..)` or `.starts_with(..)`, a prefix that claims routes.
  prefix_arg: bool,
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
    let literals = if test { Vec::new() } else { production_literals(items) };
    let functions = if test { Vec::new() } else { production_functions(items) };
    let (statics, environment_uses) = if test { (Vec::new(), Vec::new()) } else { production_global_state(items) };
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
        literals,
        functions,
        statics,
        environment_uses,
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
  /// The crate's directory under `rust/crates`, such as `infra/core`; the owner table names crates this way.
  fn key(&self) -> &str {
    self.dir_rel.strip_prefix("crates/").unwrap_or(&self.dir_rel)
  }

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
// The owner rule.

/// A name that one owner crate defines. A production literal that spells the name in another crate fails.
#[derive(Clone, Copy)]
struct OwnerRow {
  name: &'static str,
  /// The owner crate's directory under `rust/crates`.
  owner: &'static str,
  /// Names the owner's const or function that the other crates use instead.
  hint: &'static str,
}

/// The names with an owner. A task that fixes the sites of a name adds the name's row in the same commit, so the rule
/// stays green at every commit.
const OWNER_TABLE: &[OwnerRow] = &[
  OwnerRow {
    name: "agent.raw.ndjson",
    owner: "runtime/store",
    hint: "Use `charliebot_store::run_records::RAW_LOG_NAME`.",
  },
  OwnerRow {
    name: "events.jsonl",
    owner: "runtime/store",
    hint: "Use `charliebot_store::run_records::RUN_EVENTS_NAME`.",
  },
  OwnerRow { name: "threads", owner: "runtime/store", hint: "Use `charliebot_store::layout::threads_dir`." },
  OwnerRow { name: "master_runs", owner: "runtime/store", hint: "Use `charliebot_store::layout::master_runs_dir`." },
  OwnerRow { name: "archives", owner: "runtime/store", hint: "Use `charliebot_store::layout::archives_dir`." },
  OwnerRow {
    name: "launch_prompt.md",
    owner: "runtime/tasks",
    hint: "Use `charliebot_tasks::prompts::LAUNCH_TEXT_FILENAME`.",
  },
  OwnerRow {
    name: "prompt_snapshot.json",
    owner: "runtime/tasks",
    hint: "Use `charliebot_tasks::prompts::SNAPSHOT_FILENAME`.",
  },
  OwnerRow {
    name: "prompts/worker.md",
    owner: "runtime/tasks",
    hint: "Use `charliebot_tasks::spawner_prompt::WORKER_PROMPT_PATH`.",
  },
  OwnerRow {
    name: "stdout.log",
    owner: "runtime/agents",
    hint: "Use `charliebot_agents::transport::final_stdout::STDOUT_LOG_NAME`.",
  },
  OwnerRow {
    name: "backend_from_pool",
    owner: "runtime/agents",
    hint: "Use `charliebot_agents::backend_pool::BACKEND_FROM_POOL_FIELD`.",
  },
  OwnerRow {
    name: "AGENTS.md",
    owner: "runtime/registry",
    hint: "Use `charliebot_registry::backend::AGENTS_INSTRUCTIONS_FILE_NAME`.",
  },
  OwnerRow { name: "task.md", owner: "backends/charlie_code", hint: "Use `charliebot_charlie_code::TASK_FILE_NAME`." },
  OwnerRow {
    name: "PreToolUse",
    owner: "backends/claude_code",
    hint: "Use `charliebot_claude_code::claude_launch::PRE_TOOL_USE_HOOK_EVENT`.",
  },
  OwnerRow {
    name: "SessionStart",
    owner: "backends/claude_code",
    hint: "Use `charliebot_claude_code::claude_launch::SESSION_START_HOOK_EVENT`.",
  },
  OwnerRow {
    name: "cli_binary",
    owner: "backends/claude_code",
    hint: "Use `charliebot_claude_code::options::CLI_BINARY_KEY`.",
  },
  OwnerRow {
    name: "plans.json",
    owner: "features/artifacts",
    hint: "Read the plan registry through `Registry::plan_registry_reader`, which `features/artifacts` registers.",
  },
  OwnerRow { name: "config.yaml", owner: "infra/core", hint: "Use `charliebot_core::config::CONFIG_FILENAME`." },
  OwnerRow {
    name: "credentials.yaml",
    owner: "infra/core",
    hint: "Use `charliebot_core::config::CREDENTIALS_FILENAME`.",
  },
  OwnerRow { name: "config.d", owner: "infra/core", hint: "Use `charliebot_core::config::CONFIG_D_DIR_NAME`." },
  OwnerRow { name: "sessions", owner: "infra/core", hint: "Use `charliebot_core::config::sessions_dir_in`." },
  OwnerRow {
    name: "America/Los_Angeles",
    owner: "infra/core",
    hint: "Use `charliebot_core::pacific::ZONE`, the house time zone.",
  },
  OwnerRow {
    name: "metadata.json",
    owner: "infra/core",
    hint: "Use `charliebot_core::metadata_slot::METADATA_FILE_NAME`, or `charliebot_store::layout::metadata_path` for a \
           session.",
  },
  OwnerRow { name: "data", owner: "runtime/store", hint: "Use `charliebot_store::layout::data_dir`." },
  OwnerRow { name: "runs", owner: "runtime/store", hint: "Use `charliebot_store::layout::runs_dir`." },
  OwnerRow { name: "triggers", owner: "runtime/store", hint: "Use `charliebot_store::layout::triggers_dir`." },
  OwnerRow {
    name: "chat_events.jsonl",
    owner: "runtime/store",
    hint: "Use `charliebot_store::layout::chat_events_path`.",
  },
  OwnerRow { name: "schedule_cron", owner: "runtime/store", hint: "Use `charliebot_store::metadata::SCHEDULE_CRON`." },
  OwnerRow {
    name: "schedule_enabled",
    owner: "runtime/store",
    hint: "Use `charliebot_store::metadata::SCHEDULE_ENABLED`.",
  },
  OwnerRow {
    name: "schedule_next_run",
    owner: "runtime/store",
    hint: "Use `charliebot_store::metadata::SCHEDULE_NEXT_RUN`.",
  },
  OwnerRow {
    name: "schedule_timezone",
    owner: "runtime/store",
    hint: "Use `charliebot_store::metadata::SCHEDULE_TIMEZONE`.",
  },
  OwnerRow {
    name: "schedule_project",
    owner: "runtime/store",
    hint: "Use `charliebot_store::metadata::SCHEDULE_PROJECT`.",
  },
  OwnerRow {
    name: "schedule_allow_failure",
    owner: "runtime/store",
    hint: "Use `charliebot_store::metadata::SCHEDULE_ALLOW_FAILURE`.",
  },
];

/// The event names that the output formats of Claude Code, Codex and Charlie Code also use. A backend spelling one
/// of them may be writing that output, so the owner rule cannot tell the two apart, and these stay unprotected.
const EXTERNAL_EVENT_NAMES: [&str; 17] = [
  "tool_use",
  "tool_result",
  "input_tokens",
  "output_tokens",
  "cache_read_input_tokens",
  "cache_creation_input_tokens",
  "total_cost_usd",
  "agent_message",
  "compact_boundary",
  "compact_metadata",
  "pre_tokens",
  "post_tokens",
  "rate_limit_event",
  "rate_limit_info",
  "context_tokens",
  "context_full",
  "command_progress",
];

/// The owner rows for the event names: the production literal values of `infra/core`'s `event_types.rs`.
/// Each name is owned by `infra/core`, so a new const there is protected with no table edit.
/// A value without `_` stays unprotected: it is a single word with other meanings in the same code.
fn event_name_rows(workspace: &'static Workspace) -> Vec<OwnerRow> {
  const EVENT_NAME_HINT: &str = "Reference the const in `charliebot_core::event_types` that holds this name.";
  let krate = workspace
    .crates
    .iter()
    .find(|item| item.key() == "infra/core")
    .unwrap_or_else(|| panic!("infra/core holds the event names; the owner rule has no owner without it"));
  let module = krate
    .targets
    .iter()
    .flat_map(|target| target.modules.values())
    .find(|module| module.file.ends_with("event_types.rs"))
    .unwrap_or_else(|| panic!("infra/core holds event_types.rs; the owner rule has no owner without it"));
  let mut names: Vec<&str> = module
    .literals
    .iter()
    .map(|literal| literal.value.as_str())
    .filter(|value| {
      !value.contains(char::is_whitespace) && value.contains('_') && !EXTERNAL_EVENT_NAMES.contains(value)
    })
    .collect();
  names.sort_unstable();
  names.dedup();
  names.into_iter().map(|name| OwnerRow { name, owner: "infra/core", hint: EVENT_NAME_HINT }).collect()
}

/// A class of names that one crate must hold. A production literal that `name_of` maps to a name counts toward the
/// crates that spell that name; a name spelled in two or more crates fails.
struct SpellingClass {
  name_of: fn(&Site) -> Option<String>,
  hint: &'static str,
}

const ENVIRONMENT_HINT: &str = "Name it once, as a const in its owner module (infra/core for CHARLIEBOT_ names), and \
                                reference that const from the other crates.";

const ROUTE_HINT: &str = "Export the path as a `pub const` from the crate that registers its route, and reference that \
                          const from the other crates. `charliebot_cli_common::route::fill` fills its `{}` parameters.";

/// The environment variable names: `CHARLIEBOT_` followed by one or more of `A-Z`, `0-9` and `_`. The route paths: the
/// part of a literal from `/api/` on, with each `{...}` parameter written `{}`.
const SPELLING_CLASSES: &[SpellingClass] = &[
  SpellingClass { name_of: environment_name, hint: ENVIRONMENT_HINT },
  SpellingClass { name_of: route_name, hint: ROUTE_HINT },
];

/// A production string literal and where it sits.
struct Site {
  krate: String,
  file: String,
  line: usize,
  value: String,
  path_arg: bool,
  prefix_arg: bool,
}

impl Site {
  fn location(&self) -> String {
    format!("{}:{}", self.file, self.line)
  }
}

/// Every production literal of the workspace that holds no whitespace, sorted by file and line.
fn production_sites(workspace: &Workspace) -> Vec<Site> {
  let mut sites = Vec::new();
  for item in &workspace.crates {
    for target in &item.targets {
      for module in target.modules.values() {
        let file = rel(&workspace.root, &module.file);
        for literal in &module.literals {
          if !literal.value.contains(char::is_whitespace) {
            sites.push(Site {
              krate: item.key().to_string(),
              file: file.clone(),
              line: literal.line,
              value: literal.value.clone(),
              path_arg: literal.path_arg,
              prefix_arg: literal.prefix_arg,
            });
          }
        }
      }
    }
  }
  sites.sort_by(|left, right| (&left.file, left.line).cmp(&(&right.file, right.line)));
  sites.dedup_by(|left, right| left.file == right.file && left.line == right.line && left.value == right.value);
  sites
}

/// Whether a site spells `name`. A name with `.`, `_` or `/` matches a literal whose `/`-separated segments hold the
/// name's segments in sequence. A name holding an uppercase letter is a protocol identifier, so only a literal equal to
/// it matches. A plain word matches only in a path form: an argument of `.join(..)`, `Path::new(..)` or
/// `PathBuf::from(..)` equal to it, or a segment of a relative path literal. A literal that holds `/api/` is a route
/// path, which its router owns, so it spells no name in any form.
fn spells(site: &Site, name: &str) -> bool {
  if site.value.contains("/api/") {
    return false;
  }
  if name.contains(['.', '_', '/']) {
    let wanted: Vec<&str> = name.split('/').collect();
    let segments: Vec<&str> = site.value.split('/').collect();
    return segments.windows(wanted.len()).any(|window| window == wanted.as_slice());
  }
  if name.bytes().any(|byte| byte.is_ascii_uppercase()) {
    return site.value == name;
  }
  let relative = site.value.contains('/') && !site.value.starts_with('/') && !site.value.contains("://");
  (site.path_arg && site.value == name) || (relative && site.value.split('/').any(|segment| segment == name))
}

/// The `CHARLIEBOT_[A-Z0-9_]+` name a literal spells, whole or before its first `=`.
fn environment_name(site: &Site) -> Option<String> {
  let name = site.value.split_once('=').map_or(site.value.as_str(), |(name, _)| name);
  let suffix = name.strip_prefix("CHARLIEBOT_")?;
  let spelled =
    !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_');
  spelled.then(|| name.to_string())
}

/// The route path a literal spells: the part from `/api/` up to the first `?`, with each `{...}` parameter written `{}`,
/// so `{session_id}` and `{id}` name one path. A literal that is the argument of `strip_prefix(..)` or `starts_with(..)`
/// is a prefix that claims routes, so it names no path.
fn route_name(site: &Site) -> Option<String> {
  if site.prefix_arg {
    return None;
  }
  let start = site.value.find("/api/")?;
  let path = site.value[start..].split('?').next().unwrap_or_default();
  Some(parameters_as_empty(path))
}

/// The path with each `{...}` parameter replaced by `{}`.
fn parameters_as_empty(path: &str) -> String {
  let mut named = String::with_capacity(path.len());
  let mut in_parameter = false;
  for character in path.chars() {
    match (in_parameter, character) {
      (false, '{') => {
        in_parameter = true;
        named.push_str("{}");
      }
      (true, '}') => in_parameter = false,
      (false, _) => named.push(character),
      (true, _) => {}
    }
  }
  named
}

/// The owner rule's failures: each owner row's name spelled outside its owner, and each spelling class's name spelled
/// in two or more crates.
fn owner_failures(workspace: &Workspace, rows: &[OwnerRow], classes: &[SpellingClass]) -> Vec<String> {
  let sites = production_sites(workspace);
  let mut failures = Vec::new();
  for row in rows {
    let outside: Vec<&Site> = sites.iter().filter(|site| site.krate != row.owner && spells(site, row.name)).collect();
    if !outside.is_empty() {
      let heading = format!("{}: owner {}; spelled outside it at:", row.name, row.owner);
      failures.push(site_failure(&heading, &outside, row.hint));
    }
  }
  for class in classes {
    let mut by_name: BTreeMap<String, Vec<&Site>> = BTreeMap::new();
    for site in &sites {
      if let Some(name) = (class.name_of)(site) {
        by_name.entry(name).or_default().push(site);
      }
    }
    for (name, group) in by_name {
      let crates: BTreeSet<&str> = group.iter().map(|site| site.krate.as_str()).collect();
      if crates.len() > 1 {
        let heading = format!("{name}: spelled in {} crates at:", crates.len());
        failures.push(site_failure(&heading, &group, class.hint));
      }
    }
  }
  failures
}

fn site_failure(heading: &str, sites: &[&Site], hint: &str) -> String {
  let mut lines = vec![heading.to_string()];
  lines.extend(sites.iter().map(|site| format!("  {}", site.location())));
  lines.push(hint.to_string());
  lines.join("\n")
}

// ---------------------------------------------------------------------------------------------------------------------
// The format rule.

/// A file kind that one owner crate reads and writes. A production function outside the owner that names one of the
/// kind's `names` and makes a decode or an encode call in its body fails.
struct FormatRow {
  /// The file kind, named for a reader.
  kind: &'static str,
  /// The owner crate's directory under `rust/crates`.
  owner: &'static str,
  /// The owner's accessor functions and file-name consts for the kind.
  names: &'static [&'static str],
  /// The owner's read and write functions that a caller uses instead.
  hint: &'static str,
}

/// The file kinds with an owner. A file kind adds its row in the change that makes its owner the only reader and writer
/// of the file.
const FORMAT_TABLE: &[FormatRow] = &[
  FormatRow {
    kind: "chat event log",
    owner: "runtime/store",
    names: &[
      "chat_events_path",
      "archives_dir",
      "is_chat_events_archive",
      "CHAT_EVENTS_FILE_NAME",
      "session_log_paths",
      "archive_files",
    ],
    hint: "Read it through `charliebot_store::chat_events` (`read_lines`, `read_universal_text`, \
           `take_complete_lines`), get its paths from `session_log_paths` or `archive_files`, decode a line with \
           `LogLine::decode`, and append with `append_event`.",
  },
  FormatRow {
    kind: "session metadata",
    owner: "runtime/store",
    names: &["metadata_path", "SESSION_METADATA_NAME", "METADATA_FILE_NAME"],
    hint: "Read it through `charliebot_store::metadata` (`read_metadata_value`, `read_metadata_object`, \
           `read_session_metadata`, `read_metadata`), and write it through the store's `persist_*` saves.",
  },
  FormatRow {
    kind: "thread metadata",
    owner: "runtime/store",
    names: &["thread_metadata_path", "METADATA_FILE_NAME"],
    hint: "Read it through `Store::read_thread_metadata` or `charliebot_store::metadata::read_metadata_object`, and \
           write it through `Store::write_thread_metadata`.",
  },
  FormatRow {
    kind: "run metadata",
    owner: "runtime/store",
    names: &["RUN_METADATA_NAME", "METADATA_NAME", "run_metadata_path"],
    hint: "Read it through `charliebot_store::run_records` (`read_run_metadata`, `read_run_record_file`), and write it \
           through `write_run_metadata` or the store's run-record save.",
  },
  FormatRow {
    kind: "run event log",
    owner: "runtime/store",
    names: &["RUN_EVENTS_NAME", "EVENTS_LOG_NAME"],
    hint: "Encode a line with `charliebot_store::run_records::event_line`, decode a line with `parse_event_line` or \
           `decode_event_line`, read the log with `read_event_values` or `read_event_lines_lossy`, and read it \
           newest first with `visit_events_from_end` or `NewestFirstEvents`.",
  },
  FormatRow {
    kind: "trigger file",
    owner: "runtime/store",
    names: &["triggers_dir", "trigger_path", "iter_trigger_file_stats", "pending_trigger_state"],
    hint: "Read it through `charliebot_store::trigger_files` (`read_trigger`, `read_trigger_value`), and write it \
           through `write_trigger`.",
  },
  FormatRow {
    kind: "improve loop state",
    owner: "features/improve",
    names: &["STATE_FILE", "read_loop_state_value", "load_loop_state", "save_loop_state"],
    hint: "Read it through `charliebot_improve::command` (`read_loop_state_value`, `load_loop_state`), and write it \
           through `save_loop_state`.",
  },
  FormatRow {
    kind: "OpenCode server record",
    owner: "backends/opencode",
    names: &[
      "SERVER_DIRECTORY_NAME",
      "server_directory",
      "write_server_record",
      "read_server_record",
      "write_server_exit_reason",
    ],
    hint: "Read it through `relay::read_server_record`, and write it through `relay::write_server_record` or \
           `write_server_exit_reason`.",
  },
];

/// A production function of a module, with the names its body uses and the decode, encode and environment-write calls
/// it makes.
struct FunctionRecord {
  name: String,
  line: usize,
  /// The identifiers the body names: path segments, method names and the identifiers in macro tokens.
  names: BTreeSet<String>,
  /// The decode calls, by callee path.
  decodes: BTreeSet<String>,
  /// The encode calls, by callee path.
  encodes: BTreeSet<String>,
  /// The calls to a process-environment writer in the body.
  environment_writes: Vec<EnvironmentWrite>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Codec {
  Decode,
  Encode,
}

impl FunctionRecord {
  /// Record a call by its callee path when the call decodes or encodes.
  fn note_call(&mut self, path: &[String]) {
    let joined = path.join("::");
    match codec_of_call(path) {
      Some(Codec::Decode) => {
        self.decodes.insert(joined);
      }
      Some(Codec::Encode) => {
        self.encodes.insert(joined);
      }
      None => {}
    }
  }

  /// Record a call by its callee path when the path ends in a process-environment writer.
  fn note_environment_write(&mut self, path: &[String], line: usize) {
    if let Some(writer) = path.last().filter(|writer| ENVIRONMENT_WRITERS.contains(&writer.as_str())) {
      self.environment_writes.push(EnvironmentWrite { writer: writer.clone(), line });
    }
  }
}

/// The codec of a call by its callee path. A serde decoder or encoder is named through `serde_json` or `serde_yaml`,
/// a Python-style decoder through `json_loads`, and a bare name, or a method name, counts as the name alone.
fn codec_of_call(path: &[String]) -> Option<Codec> {
  let last = path.last()?.as_str();
  let serde = path.iter().any(|segment| segment == "serde_json" || segment == "serde_yaml");
  let json_loads = path.iter().any(|segment| segment == "json_loads");
  let bare = path.len() == 1;
  match last {
    "lines" | "split_inclusive" | "split_terminator" | "read_until" => Some(Codec::Decode),
    "from_str" | "from_slice" | "from_reader" if serde || bare => Some(Codec::Decode),
    "loads" | "loads_text" if json_loads || bare => Some(Codec::Decode),
    "json_dumps" => Some(Codec::Encode),
    _ if last.starts_with("to_") && serde => Some(Codec::Encode),
    _ => None,
  }
}

/// Collects the production functions of one module: each item and impl item that is not `#[cfg(test)]`, with the
/// names its body uses and the codec calls in it. An inline module is its own scope and stays out.
#[derive(Default)]
struct FunctionCollector {
  functions: Vec<FunctionRecord>,
  /// The functions whose bodies the visitor is inside, innermost last.
  open: Vec<FunctionRecord>,
}

impl FunctionCollector {
  fn enter(&mut self, name: &syn::Ident) {
    self.open.push(FunctionRecord {
      name: name.to_string(),
      line: name.span().start().line,
      names: BTreeSet::new(),
      decodes: BTreeSet::new(),
      encodes: BTreeSet::new(),
      environment_writes: Vec::new(),
    });
  }

  fn exit(&mut self) {
    let record = self.open.pop().expect("a function is entered before it is left");
    self.functions.push(record);
  }

  /// The function whose body the visitor is inside, when it is inside one.
  fn current(&mut self) -> Option<&mut FunctionRecord> {
    self.open.last_mut()
  }
}

impl<'ast> Visit<'ast> for FunctionCollector {
  fn visit_item(&mut self, node: &'ast Item) {
    if !is_cfg_test(item_attrs(node)) {
      syn::visit::visit_item(self, node);
    }
  }

  fn visit_item_mod(&mut self, _node: &'ast ItemMod) {
    // An inline module is its own scope, and `process_scope` reads it.
  }

  fn visit_impl_item(&mut self, node: &'ast ImplItem) {
    if !is_cfg_test(impl_item_attrs(node)) {
      syn::visit::visit_impl_item(self, node);
    }
  }

  fn visit_item_fn(&mut self, node: &'ast ItemFn) {
    self.enter(&node.sig.ident);
    syn::visit::visit_item_fn(self, node);
    self.exit();
  }

  fn visit_impl_item_fn(&mut self, node: &'ast ImplItemFn) {
    self.enter(&node.sig.ident);
    syn::visit::visit_impl_item_fn(self, node);
    self.exit();
  }

  fn visit_trait_item_fn(&mut self, node: &'ast TraitItemFn) {
    self.enter(&node.sig.ident);
    syn::visit::visit_trait_item_fn(self, node);
    self.exit();
  }

  fn visit_expr(&mut self, node: &'ast Expr) {
    if !is_cfg_test(expr_attrs(node)) {
      syn::visit::visit_expr(self, node);
    }
  }

  fn visit_local(&mut self, node: &'ast Local) {
    if !is_cfg_test(&node.attrs) {
      syn::visit::visit_local(self, node);
    }
  }

  fn visit_stmt_macro(&mut self, node: &'ast StmtMacro) {
    if !is_cfg_test(&node.attrs) {
      syn::visit::visit_stmt_macro(self, node);
    }
  }

  fn visit_path(&mut self, node: &'ast syn::Path) {
    if let Some(record) = self.current() {
      record.names.extend(path_segments(node));
    }
    syn::visit::visit_path(self, node);
  }

  fn visit_expr_call(&mut self, node: &'ast ExprCall) {
    if let Expr::Path(callee) = &*node.func
      && let Some(record) = self.current()
    {
      let path = path_segments(&callee.path);
      record.note_call(&path);
      record.note_environment_write(&path, callee.path.span().start().line);
    }
    syn::visit::visit_expr_call(self, node);
  }

  fn visit_expr_method_call(&mut self, node: &'ast ExprMethodCall) {
    if let Some(record) = self.current() {
      let method = node.method.to_string();
      record.names.insert(method.clone());
      record.note_call(std::slice::from_ref(&method));
    }
    syn::visit::visit_expr_method_call(self, node);
  }

  fn visit_macro(&mut self, node: &'ast syn::Macro) {
    if let Some(record) = self.current() {
      scan_macro_tokens(node.tokens.clone(), record);
    }
  }
}

/// Record the identifiers of a macro call's tokens and the calls among them. Syn does not parse a macro's tokens, so a
/// path that a parenthesized group follows counts as a call.
fn scan_macro_tokens(tokens: TokenStream, record: &mut FunctionRecord) {
  let trees: Vec<TokenTree> = tokens.into_iter().collect();
  for (index, tree) in trees.iter().enumerate() {
    match tree {
      TokenTree::Ident(ident) => {
        record.names.insert(ident.to_string());
        if let Some(TokenTree::Group(group)) = trees.get(index + 1)
          && group.delimiter() == Delimiter::Parenthesis
        {
          let path = call_path_before(&trees, index);
          record.note_call(&path);
          record.note_environment_write(&path, ident.span().start().line);
        }
      }
      TokenTree::Group(group) => scan_macro_tokens(group.stream(), record),
      TokenTree::Punct(_) | TokenTree::Literal(_) => {}
    }
  }
}

/// The callee path that ends at `trees[index]`: the identifiers joined by `::` before it.
fn call_path_before(trees: &[TokenTree], index: usize) -> Vec<String> {
  let mut segments = vec![trees[index].to_string()];
  let mut at = index;
  while at >= 3 && is_path_separator(&trees[at - 2], &trees[at - 1]) {
    let TokenTree::Ident(previous) = &trees[at - 3] else { break };
    segments.insert(0, previous.to_string());
    at -= 3;
  }
  segments
}

/// The production functions of a module's items, each with the names and codec calls its body holds.
fn production_functions(items: &[Item]) -> Vec<FunctionRecord> {
  let mut collector = FunctionCollector::default();
  for item in items {
    collector.visit_item(item);
  }
  collector.functions
}

/// The format rule's failures: each production function outside a file kind's owner that names one of the kind's
/// `names`. The function also makes a decode or an encode call in its body. Each failure gives the function's
/// `file:line`, the kind, the calls and the owner's hint.
fn format_failures(workspace: &Workspace, table: &[FormatRow]) -> Vec<String> {
  let mut failures = BTreeSet::new();
  for item in &workspace.crates {
    for target in &item.targets {
      for module in target.modules.values() {
        let file = rel(&workspace.root, &module.file);
        for function in &module.functions {
          for row in table {
            let named = function.names.iter().any(|name| row.names.contains(&name.as_str()));
            let codecs = !function.decodes.is_empty() || !function.encodes.is_empty();
            if item.key() != row.owner && named && codecs {
              failures.insert(format_failure(&file, function, row));
            }
          }
        }
      }
    }
  }
  failures.into_iter().collect()
}

/// One failure line: the function's `file:line`, its verb and the calls it makes, the file kind and the owner's hint.
fn format_failure(file: &str, function: &FunctionRecord, row: &FormatRow) -> String {
  let verb = match (function.decodes.is_empty(), function.encodes.is_empty()) {
    (false, true) => "decodes",
    (true, false) => "encodes",
    (false, false) => "decodes and encodes",
    (true, true) => unreachable!("a function fails the format rule only when it makes a codec call"),
  };
  let calls: BTreeSet<&String> = function.decodes.union(&function.encodes).collect();
  let calls: Vec<String> = calls.into_iter().map(|call| format!("`{call}`")).collect();
  let calls = calls.join(", ");
  format!("{file}:{}: `{}` {verb} the {} itself ({calls}). {}", function.line, function.name, row.kind, row.hint)
}

// ---------------------------------------------------------------------------------------------------------------------
// The global-state rule.

/// The process-environment writers. A production call whose callee path ends in one of these names changes the
/// environment, and so does a production `use` that imports one.
const ENVIRONMENT_WRITERS: [&str; 6] = ["set_var", "remove_var", "setenv", "unsetenv", "putenv", "clearenv"];

/// The one file whose production code may change the process environment.
const ENVIRONMENT_OWNER: &str = "runtime/agents/src/agent_environment.rs";

const PRIVATE_STATIC_HINT: &str = "Remove the visibility modifier: the module exposes mutable state through \
                                   functions, and shared data that never changes is a `const` or an access function.";

const OWNED_ENVIRONMENT_HINT: &str = "Change the process environment in `replace_process_environment`.";

/// A production `static` declaration: its name, its line and whether it carries a visibility modifier.
struct StaticRecord {
  name: String,
  line: usize,
  has_visibility: bool,
}

/// A production `use` that imports a process-environment writer.
struct EnvironmentUse {
  writer: String,
  line: usize,
}

/// A call in a production function whose callee path ends in a process-environment writer.
struct EnvironmentWrite {
  writer: String,
  line: usize,
}

/// The global-state rule's failures: each production `static` with a visibility modifier, and each production place
/// outside the owner file that changes the process environment, sorted by file and line.
fn global_state_failures(workspace: &Workspace) -> Vec<String> {
  let mut failures: Vec<(String, usize, String)> = Vec::new();
  for item in &workspace.crates {
    for target in &item.targets {
      for module in target.modules.values() {
        let file = rel(&workspace.root, &module.file);
        for record in &module.statics {
          if record.has_visibility {
            failures.push((
              file.clone(),
              record.line,
              format!(
                "{file}:{}: `{}` is a production `static` with a visibility modifier. {PRIVATE_STATIC_HINT}",
                record.line, record.name
              ),
            ));
          }
        }
        if file.ends_with(ENVIRONMENT_OWNER) {
          continue;
        }
        for record in &module.environment_uses {
          failures.push((
            file.clone(),
            record.line,
            format!(
              "{file}:{}: a `use` imports the process-environment writer `{}`. {OWNED_ENVIRONMENT_HINT}",
              record.line, record.writer
            ),
          ));
        }
        for function in &module.functions {
          for write in &function.environment_writes {
            failures.push((
              file.clone(),
              write.line,
              format!(
                "{file}:{}: `{}` calls the process-environment writer `{}`. {OWNED_ENVIRONMENT_HINT}",
                write.line, function.name, write.writer
              ),
            ));
          }
        }
      }
    }
  }
  failures.sort_by(|left, right| (&left.0, left.1).cmp(&(&right.0, right.1)));
  failures.into_iter().map(|(_, _, message)| message).collect()
}

/// Collects a module's production statics and the production `use` items that import a process-environment writer.
#[derive(Default)]
struct GlobalStateCollector {
  statics: Vec<StaticRecord>,
  environment_uses: Vec<EnvironmentUse>,
}

impl<'ast> Visit<'ast> for GlobalStateCollector {
  fn visit_item(&mut self, node: &'ast Item) {
    if !is_cfg_test(item_attrs(node)) {
      syn::visit::visit_item(self, node);
    }
  }

  fn visit_item_mod(&mut self, _node: &'ast ItemMod) {
    // An inline module is its own scope, and `process_scope` reads it.
  }

  fn visit_item_static(&mut self, node: &'ast syn::ItemStatic) {
    self.statics.push(StaticRecord {
      name: node.ident.to_string(),
      line: node.ident.span().start().line,
      has_visibility: !matches!(node.vis, syn::Visibility::Inherited),
    });
  }

  fn visit_item_use(&mut self, node: &'ast ItemUse) {
    let line = node.use_token.span.start().line;
    let mut flattened = Vec::new();
    flatten_use(&node.tree, &mut Vec::new(), &mut flattened);
    for item in flattened {
      let FlatUse::Name { path, .. } = item else { continue };
      if let Some(writer) = path.last().filter(|writer| ENVIRONMENT_WRITERS.contains(&writer.as_str())) {
        self.environment_uses.push(EnvironmentUse { writer: writer.clone(), line });
      }
    }
  }

  fn visit_macro(&mut self, node: &'ast syn::Macro) {
    collect_macro_statics(node.tokens.clone(), &mut self.statics);
    syn::visit::visit_macro(self, node);
  }
}

/// The `static` declarations that a macro invocation's tokens spell with a visibility modifier. Syn expands no macro,
/// so a `thread_local!` block's `pub static` is read from its tokens.
fn collect_macro_statics(tokens: TokenStream, statics: &mut Vec<StaticRecord>) {
  let trees: Vec<TokenTree> = tokens.into_iter().collect();
  for (index, tree) in trees.iter().enumerate() {
    match tree {
      TokenTree::Group(group) => collect_macro_statics(group.stream(), statics),
      TokenTree::Ident(ident) if ident == "static" && has_visibility_before(&trees, index) => {
        if let Some(TokenTree::Ident(name)) = trees.get(index + 1) {
          statics.push(StaticRecord { name: name.to_string(), line: ident.span().start().line, has_visibility: true });
        }
      }
      TokenTree::Ident(_) | TokenTree::Punct(_) | TokenTree::Literal(_) => {}
    }
  }
}

/// Whether the tokens before `trees[index]` hold a visibility modifier: `pub`, or `pub` and its `(..)` group.
fn has_visibility_before(trees: &[TokenTree], index: usize) -> bool {
  let before = index.checked_sub(1).and_then(|position| trees.get(position));
  let group_before = index.checked_sub(2).and_then(|position| trees.get(position));
  match (before, group_before) {
    (Some(TokenTree::Ident(ident)), _) => ident == "pub",
    (Some(TokenTree::Group(group)), Some(TokenTree::Ident(ident))) => {
      group.delimiter() == Delimiter::Parenthesis && ident == "pub"
    }
    _ => false,
  }
}

/// The production statics and process-environment imports of a module's items.
fn production_global_state(items: &[Item]) -> (Vec<StaticRecord>, Vec<EnvironmentUse>) {
  let mut collector = GlobalStateCollector::default();
  for item in items {
    collector.visit_item(item);
  }
  (collector.statics, collector.environment_uses)
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

#[test]
fn no_name_is_spelled_outside_its_owner() {
  let workspace = real_workspace();
  let rows: Vec<OwnerRow> = OWNER_TABLE.iter().copied().chain(event_name_rows(workspace)).collect();
  let failures = owner_failures(workspace, &rows, SPELLING_CLASSES);
  assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn every_file_kind_is_read_and_written_by_its_owner() {
  let failures = format_failures(real_workspace(), FORMAT_TABLE);
  assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn no_global_state_outside_its_owner() {
  let failures = global_state_failures(real_workspace());
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
    vec![
      "user_text_checks: names no runtime module; remove it from the layer table.".to_string(),
      "session-api: names no runtime module; remove it from the layer table.".to_string(),
    ],
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

/// The owner rule on a synthetic workspace. The owner crate spells its names; the writer crate spells them outside it,
/// and spells one environment name that the reader crate also spells. Each break fails with its sites and its hint.
/// Each form the rule allows passes: an owner literal, a named const, a JSON key, prose, and a `#[cfg(test)]` item.
/// A route path that holds a layout name and a test target also pass. Each line number below is the line of the
/// fixture text.
#[test]
fn the_owner_rule_finds_each_break() {
  let owner_dir = tempfile::tempdir().expect("create a temporary directory");
  let owner_root = owner_dir.path();
  let owner_lib = r##"pub const SESSIONS: &str = "sessions";
pub const EVENTS_FILE: &str = "chat_events.jsonl";
pub const RUN_EVENTS: &str = "events.jsonl";
pub const SESSION_ID_ENV: &str = "CHARLIEBOT_SESSION_ID";
"##;
  let writer_lib = r##"use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[cfg(test)]
use std::fmt;

pub struct Probe {
  #[cfg(test)]
  pub scratch: u8,
  #[serde(rename = "chat_events.jsonl")]
  pub name: &'static str,
  pub label: &'static str,
}

pub fn sessions_dir(home: &Path) -> PathBuf {
  home.join(owner_crate::SESSIONS)
}

pub fn archive(home: &Path) -> PathBuf {
  home.join("sessions")
}

pub fn event_file(home: &Path) -> PathBuf {
  home.join("data/chat_events.jsonl")
}

pub fn archive_path() -> &'static str {
  "archive/sessions/x.json"
}

pub fn lookup(map: &HashMap<&str, u8>) -> u8 {
  map["data"]
}

pub fn probe() -> Probe {
  Probe {
    #[cfg(test)]
    name: "chat_events.jsonl",
    label: "chat_events.jsonl",
  }
}

pub fn token_names() -> [&'static str; 1] {
  ["CHARLIEBOT_RUN_TOKEN"]
}

pub fn help() -> &'static str {
  "Session ID (optional; taken from the CHARLIEBOT_SESSION_ID the server writes)"
}

pub fn trace(home: &Path) {
  #[cfg(test)]
  let _probe = home.join("sessions");
  let _ = home.join("sessions");
  #[cfg(test)]
  drop(Path::new("chat_events.jsonl"));
}

#[cfg(test)]
fn fixture() -> &'static str {
  "chat_events.jsonl"
}

#[cfg(test)]
mod tests {
  pub const NAME: &str = "sessions";
}
"##;
  let reader_lib = r##"pub fn token_env() -> &'static str {
  "CHARLIEBOT_RUN_TOKEN="
}

pub fn routes() -> [&'static str; 2] {
  ["/api/sessions/{session_id}/events.jsonl", "/api/sessions/{session_id}/data/chat_events.jsonl"]
}

#[cfg(test)]
mod tests {
  pub const TOKEN: &str = "CHARLIEBOT_RUN_TOKEN";
}
"##;
  let writer_fixture = r##"pub const FIXTURE: [&str; 2] = ["CHARLIEBOT_RUN_TOKEN", "chat_events.jsonl"];
"##;
  write_tree(
    owner_root,
    &[
      ("Cargo.toml", workspace_manifest(&["crates/infra/owner", "crates/features/writer", "crates/features/reader"])),
      ("crates/infra/owner/Cargo.toml", crate_manifest("owner-crate", &[], &[])),
      ("crates/infra/owner/src/lib.rs", owner_lib.to_string()),
      ("crates/features/writer/Cargo.toml", crate_manifest("writer", &[("owner-crate", "../../infra/owner")], &[])),
      ("crates/features/writer/src/lib.rs", writer_lib.to_string()),
      ("crates/features/writer/tests/fixture.rs", writer_fixture.to_string()),
      ("crates/features/reader/Cargo.toml", crate_manifest("reader", &[], &[])),
      ("crates/features/reader/src/lib.rs", reader_lib.to_string()),
    ],
  );
  let workspace = load_workspace(owner_root);
  let rows = [
    OwnerRow { name: "chat_events.jsonl", owner: "infra/owner", hint: "Use `owner_crate::EVENTS_FILE`." },
    OwnerRow { name: "sessions", owner: "infra/owner", hint: "Use `owner_crate::SESSIONS`." },
    OwnerRow { name: "events.jsonl", owner: "infra/owner", hint: "Use `owner_crate::RUN_EVENTS`." },
  ];

  assert_eq!(
    owner_failures(&workspace, &rows, SPELLING_CLASSES),
    vec![
      [
        "chat_events.jsonl: owner infra/owner; spelled outside it at:",
        "  crates/features/writer/src/lib.rs:10",
        "  crates/features/writer/src/lib.rs:24",
        "  crates/features/writer/src/lib.rs:39",
        "Use `owner_crate::EVENTS_FILE`.",
      ]
      .join("\n"),
      [
        "sessions: owner infra/owner; spelled outside it at:",
        "  crates/features/writer/src/lib.rs:20",
        "  crates/features/writer/src/lib.rs:28",
        "  crates/features/writer/src/lib.rs:54",
        "Use `owner_crate::SESSIONS`.",
      ]
      .join("\n"),
      [
        "CHARLIEBOT_RUN_TOKEN: spelled in 2 crates at:",
        "  crates/features/reader/src/lib.rs:2",
        "  crates/features/writer/src/lib.rs:44",
        ENVIRONMENT_HINT,
      ]
      .join("\n"),
    ],
    "the owner rule finds each break with its sites and hint, and passes the forms it allows"
  );
}

/// The route rule on a synthetic workspace. A path that two crates spell fails once, with each site and the hint. Two
/// spellings that differ only in a parameter's name count as one path, and a `strip_prefix` or `starts_with` argument
/// names no path, so the reader crate passes.
#[test]
fn the_route_rule_finds_each_break() {
  let dir = tempfile::tempdir().expect("create a temporary directory");
  let root = dir.path();
  let owner_lib = r##"pub const THING: &str = "/api/things/{id}";
"##;
  let writer_lib = r##"pub fn send() -> &'static str {
  "/api/things/{thing_id}?verbose=1"
}
"##;
  let reader_lib = r##"pub fn claims(path: &str) -> bool {
  path.strip_prefix("/api/things/").is_some() || path.starts_with("/api/things/")
}
"##;
  write_tree(
    root,
    &[
      ("Cargo.toml", workspace_manifest(&["crates/infra/owner", "crates/features/writer", "crates/features/reader"])),
      ("crates/infra/owner/Cargo.toml", crate_manifest("owner-crate", &[], &[])),
      ("crates/infra/owner/src/lib.rs", owner_lib.to_string()),
      ("crates/features/writer/Cargo.toml", crate_manifest("writer", &[], &[])),
      ("crates/features/writer/src/lib.rs", writer_lib.to_string()),
      ("crates/features/reader/Cargo.toml", crate_manifest("reader", &[], &[])),
      ("crates/features/reader/src/lib.rs", reader_lib.to_string()),
    ],
  );
  let workspace = load_workspace(root);

  assert_eq!(
    owner_failures(&workspace, &[], SPELLING_CLASSES),
    vec![
      [
        "/api/things/{}: spelled in 2 crates at:",
        "  crates/features/writer/src/lib.rs:2",
        "  crates/infra/owner/src/lib.rs:1",
        ROUTE_HINT,
      ]
      .join("\n"),
    ],
    "the route rule fails the path that two crates spell, and passes the prefix arguments"
  );
}

/// The event names of `event_types.rs` become owner rows with no table edit. The derived set holds the values with
/// `_`, but not the names the backend output formats also use, nor any single word. A writer crate spelling the
/// protected name fails with its `file:line` and hint; its spellings of the unprotected ones pass.
#[test]
fn the_event_names_of_event_types_are_protected() {
  let dir = tempfile::tempdir().expect("create a temporary directory");
  let root = dir.path();
  write_tree(
    root,
    &[
      ("Cargo.toml", workspace_manifest(&["crates/infra/core", "crates/features/writer"])),
      ("crates/infra/core/Cargo.toml", crate_manifest("core-crate", &[], &[])),
      ("crates/infra/core/src/lib.rs", "pub mod event_types;\n".to_string()),
      (
        "crates/infra/core/src/event_types.rs",
        "pub const CHILD_REPORT: &str = \"child_report\";\n\
         pub const TOOL_USE: &str = \"tool_use\";\n\
         pub const USER: &str = \"user\";\n"
          .to_string(),
      ),
      ("crates/features/writer/Cargo.toml", crate_manifest("writer", &[], &[])),
      (
        "crates/features/writer/src/lib.rs",
        "pub fn reported() -> &'static str {\n  \"child_report\"\n}\n\n\
         pub fn tool_output() -> &'static str {\n  \"tool_use\"\n}\n\n\
         pub fn word() -> &'static str {\n  \"user\"\n}\n"
          .to_string(),
      ),
    ],
  );
  // The rows borrow the literal values, so the workspace outlives the test the way the real one does.
  let workspace: &'static Workspace = Box::leak(Box::new(load_workspace(root)));
  let rows = event_name_rows(workspace);

  assert_eq!(
    rows.iter().map(|row| row.name).collect::<Vec<_>>(),
    ["child_report"],
    "the value with `_` that the backend output formats do not use is the only protected name"
  );
  assert_eq!(
    owner_failures(workspace, &rows, &[]),
    vec![
      "child_report: owner infra/core; spelled outside it at:\n  crates/features/writer/src/lib.rs:2\n\
       Reference the const in `charliebot_core::event_types` that holds this name."
        .to_string()
    ],
    "the owner rule fails the spelled name with its file:line and hint, and passes the unprotected words"
  );
}

/// A name holding an uppercase letter on a synthetic workspace. The writer crate fails on a literal equal to the name,
/// and passes on a longer literal that only holds its letters.
#[test]
fn a_camel_case_owner_name_matches_an_exact_literal_only() {
  let dir = tempfile::tempdir().expect("create a temporary directory");
  let root = dir.path();
  write_tree(
    root,
    &[
      ("Cargo.toml", workspace_manifest(&["crates/infra/owner", "crates/features/writer"])),
      ("crates/infra/owner/Cargo.toml", crate_manifest("owner-crate", &[], &[])),
      ("crates/infra/owner/src/lib.rs", "pub const HOOK_EVENT: &str = \"PreToolUse\";\n".to_string()),
      ("crates/features/writer/Cargo.toml", crate_manifest("writer", &[], &[])),
      (
        "crates/features/writer/src/lib.rs",
        "pub fn event_name() -> &'static str {\n  \"PreToolUse\"\n}\n\n\
         pub fn longer_name() -> &'static str {\n  \"PreToolUseLater\"\n}\n"
          .to_string(),
      ),
    ],
  );
  let workspace = load_workspace(root);
  let rows = [OwnerRow { name: "PreToolUse", owner: "infra/owner", hint: "Use `owner_crate::HOOK_EVENT`." }];

  assert_eq!(
    owner_failures(&workspace, &rows, &[]),
    vec![
      "PreToolUse: owner infra/owner; spelled outside it at:\n  crates/features/writer/src/lib.rs:2\n\
       Use `owner_crate::HOOK_EVENT`."
        .to_string()
    ],
    "the owner rule fails the exact literal and passes the longer one"
  );
}

/// The format rule on a synthetic workspace. The owner decodes its own file, which passes. The writer's function that
/// decodes the file fails, and so does the one that encodes a line next to the path inside a macro call. A function
/// that only passes the path on passes, and so does a test-only function. Each failure gives its `file:line`, the kind
/// and the hint.
#[test]
fn the_format_rule_finds_each_break() {
  let dir = tempfile::tempdir().expect("create a temporary directory");
  let root = dir.path();
  let owner_lib = r##"use std::path::{Path, PathBuf};

pub fn log_path(home: &Path) -> PathBuf {
  home.join("chat_events.jsonl")
}

pub fn read_lines(home: &Path) -> Vec<String> {
  std::fs::read_to_string(log_path(home)).unwrap().lines().map(str::to_string).collect()
}
"##;
  let writer_lib = r##"use std::path::{Path, PathBuf};

pub fn decode_break(home: &Path) -> usize {
  let text = std::fs::read_to_string(owner_crate::log_path(home)).unwrap();
  text.lines().count()
}

pub fn encode_break(home: &Path) -> String {
  let event = serde_json::json!({"type": "task_closed"});
  format!("{}\n{}", owner_crate::log_path(home).display(), serde_json::to_string(&event).unwrap())
}

pub fn pass_on(home: &Path) -> PathBuf {
  owner_crate::log_path(home)
}

pub fn count_at(path: &Path) -> usize {
  std::fs::read_to_string(path).unwrap().lines().count()
}

pub fn pass_through_helper(home: &Path) -> usize {
  count_at(&owner_crate::log_path(home))
}

#[cfg(test)]
fn fixture_count(home: &Path) -> usize {
  std::fs::read_to_string(owner_crate::log_path(home)).unwrap().lines().count()
}
"##;
  write_tree(
    root,
    &[
      ("Cargo.toml", workspace_manifest(&["crates/infra/owner", "crates/features/writer"])),
      ("crates/infra/owner/Cargo.toml", crate_manifest("owner-crate", &[], &[])),
      ("crates/infra/owner/src/lib.rs", owner_lib.to_string()),
      ("crates/features/writer/Cargo.toml", crate_manifest("writer", &[], &[])),
      ("crates/features/writer/src/lib.rs", writer_lib.to_string()),
    ],
  );
  let workspace = load_workspace(root);
  let table = [FormatRow {
    kind: "owner file",
    owner: "infra/owner",
    names: &["log_path"],
    hint: "Read it through `owner_crate::read_lines`.",
  }];

  assert_eq!(
    format_failures(&workspace, &table),
    vec![
      "crates/features/writer/src/lib.rs:3: `decode_break` decodes the owner file itself (`lines`). Read it through \
       `owner_crate::read_lines`."
        .to_string(),
      "crates/features/writer/src/lib.rs:8: `encode_break` encodes the owner file itself (`serde_json::to_string`). \
       Read it through `owner_crate::read_lines`."
        .to_string(),
    ],
    "the format rule fails each decode and encode outside the owner, and passes the owner, a pass-on and a test fn"
  );
}

/// The global-state rule on a synthetic workspace. Each public static fails: the one whose type is a type alias of a
/// `Mutex`, the plain `&str` one, and the `pub(crate)`, `pub(super)` and `pub(in ..)` forms. The `pub static` inside
/// `thread_local!` fails too. The production calls to `set_var` and `setenv` fail, and so does the aliased `use`.
/// A private static and a `#[cfg(test)]` function pass. Each line number below is the line of the fixture text.
#[test]
fn the_global_state_rule_finds_each_break() {
  let dir = tempfile::tempdir().expect("create a temporary directory");
  let root = dir.path();
  let lib = r##"use std::sync::{Mutex, OnceLock};

pub type Shared = Mutex<u8>;

pub static SHARED: OnceLock<Shared> = OnceLock::new();

pub(crate) static PLAIN: &str = "synthetic";

pub(super) static SUPER: &str = "synthetic";

pub(in crate) static IN_CRATE: &str = "synthetic";

static PRIVATE: OnceLock<u8> = OnceLock::new();

thread_local! {
  pub static STEPS: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

pub fn write_std() {
  std::env::set_var("SYNTHETIC_NAME", "synthetic");
}

pub fn write_libc() {
  unsafe { libc::setenv("SYNTHETIC_NAME", "synthetic", 1) };
}

use std::env::set_var as other_name;

#[cfg(test)]
fn test_write() {
  std::env::set_var("SYNTHETIC_NAME", "synthetic");
}
"##;
  write_tree(
    root,
    &[
      ("Cargo.toml", workspace_manifest(&["crates/infra/owner"])),
      ("crates/infra/owner/Cargo.toml", crate_manifest("owner-crate", &[], &[])),
      ("crates/infra/owner/src/lib.rs", lib.to_string()),
    ],
  );
  let workspace = load_workspace(root);
  let file = "crates/infra/owner/src/lib.rs";

  assert_eq!(
    global_state_failures(&workspace),
    vec![
      format!("{file}:5: `SHARED` is a production `static` with a visibility modifier. {PRIVATE_STATIC_HINT}"),
      format!("{file}:7: `PLAIN` is a production `static` with a visibility modifier. {PRIVATE_STATIC_HINT}"),
      format!("{file}:9: `SUPER` is a production `static` with a visibility modifier. {PRIVATE_STATIC_HINT}"),
      format!("{file}:11: `IN_CRATE` is a production `static` with a visibility modifier. {PRIVATE_STATIC_HINT}"),
      format!("{file}:16: `STEPS` is a production `static` with a visibility modifier. {PRIVATE_STATIC_HINT}"),
      format!("{file}:20: `write_std` calls the process-environment writer `set_var`. {OWNED_ENVIRONMENT_HINT}"),
      format!("{file}:24: `write_libc` calls the process-environment writer `setenv`. {OWNED_ENVIRONMENT_HINT}"),
      format!("{file}:27: a `use` imports the process-environment writer `set_var`. {OWNED_ENVIRONMENT_HINT}"),
    ],
    "the global-state rule fails each public static and each environment write, and passes a private static and a \
     cfg(test) function"
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

/// The string literals of a module's production items. Each `#[cfg(test)]` item is skipped, and an inline module is
/// read in its own scope, so the visitor does not enter it.
#[derive(Default)]
struct LiteralCollector {
  literals: Vec<Literal>,
  /// The start positions of the literals that are the argument of `.join(..)`, `Path::new(..)` or `PathBuf::from(..)`.
  path_args: BTreeSet<(usize, usize)>,
  /// The start positions of the literals that are the argument of `.strip_prefix(..)` or `.starts_with(..)`.
  prefix_args: BTreeSet<(usize, usize)>,
}

fn production_literals(items: &[Item]) -> Vec<Literal> {
  let mut collector = LiteralCollector::default();
  for item in items {
    collector.visit_item(item);
  }
  collector.literals
}

/// The start positions of the string literals among `args`.
fn literal_positions<'a>(args: impl Iterator<Item = &'a Expr> + 'a) -> impl Iterator<Item = (usize, usize)> + 'a {
  args.filter_map(|arg| match arg {
    Expr::Lit(ExprLit { lit: Lit::Str(value), .. }) => {
      let start = value.span().start();
      Some((start.line, start.column))
    }
    _ => None,
  })
}

impl<'ast> Visit<'ast> for LiteralCollector {
  fn visit_item(&mut self, node: &'ast Item) {
    if !is_cfg_test(item_attrs(node)) {
      syn::visit::visit_item(self, node);
    }
  }

  fn visit_item_mod(&mut self, _node: &'ast ItemMod) {
    // An inline module is its own scope, and `process_scope` reads it.
  }

  fn visit_impl_item(&mut self, node: &'ast ImplItem) {
    if !is_cfg_test(impl_item_attrs(node)) {
      syn::visit::visit_impl_item(self, node);
    }
  }

  fn visit_field(&mut self, node: &'ast Field) {
    if !is_cfg_test(&node.attrs) {
      syn::visit::visit_field(self, node);
    }
  }

  fn visit_field_value(&mut self, node: &'ast FieldValue) {
    if !is_cfg_test(&node.attrs) {
      syn::visit::visit_field_value(self, node);
    }
  }

  fn visit_arm(&mut self, node: &'ast Arm) {
    if !is_cfg_test(&node.attrs) {
      syn::visit::visit_arm(self, node);
    }
  }

  fn visit_local(&mut self, node: &'ast Local) {
    if !is_cfg_test(&node.attrs) {
      syn::visit::visit_local(self, node);
    }
  }

  fn visit_stmt_macro(&mut self, node: &'ast StmtMacro) {
    if !is_cfg_test(&node.attrs) {
      syn::visit::visit_stmt_macro(self, node);
    }
  }

  fn visit_expr(&mut self, node: &'ast Expr) {
    if !is_cfg_test(expr_attrs(node)) {
      syn::visit::visit_expr(self, node);
    }
  }

  fn visit_attribute(&mut self, node: &'ast Attribute) {
    // A `doc` attribute holds documentation text, not code.
    if node.path().is_ident("doc") {
      return;
    }
    match &node.meta {
      Meta::Path(_) => {}
      Meta::List(list) => collect_token_literals(list.tokens.clone(), &mut self.literals),
      Meta::NameValue(_) => syn::visit::visit_attribute(self, node),
    }
  }

  fn visit_macro(&mut self, node: &'ast syn::Macro) {
    // Syn does not parse a macro call's tokens, so their literals are read from the tokens.
    collect_token_literals(node.tokens.clone(), &mut self.literals);
  }

  fn visit_expr_method_call(&mut self, node: &'ast ExprMethodCall) {
    if node.method == "join" {
      self.path_args.extend(literal_positions(node.args.iter()));
    } else if node.method == "strip_prefix" || node.method == "starts_with" {
      self.prefix_args.extend(literal_positions(node.args.iter()));
    }
    syn::visit::visit_expr_method_call(self, node);
  }

  fn visit_expr_call(&mut self, node: &'ast ExprCall) {
    if let Expr::Path(callee) = &*node.func
      && is_path_constructor(&callee.path)
    {
      self.path_args.extend(literal_positions(node.args.iter()));
    }
    syn::visit::visit_expr_call(self, node);
  }

  fn visit_lit_str(&mut self, node: &'ast LitStr) {
    let start = node.span().start();
    let position = (start.line, start.column);
    let path_arg = self.path_args.contains(&position);
    let prefix_arg = self.prefix_args.contains(&position);
    self.literals.push(Literal { value: node.value(), line: start.line, path_arg, prefix_arg });
  }
}

/// The string literals in a token stream, nested groups included.
fn collect_token_literals(tokens: TokenStream, literals: &mut Vec<Literal>) {
  for tree in tokens {
    match tree {
      TokenTree::Group(group) => collect_token_literals(group.stream(), literals),
      TokenTree::Literal(literal) => {
        let parsed: Lit = syn::parse_str(&literal.to_string()).expect("a literal token parses as a syn literal");
        if let Lit::Str(value) = parsed {
          literals.push(Literal {
            value: value.value(),
            line: literal.span().start().line,
            path_arg: false,
            prefix_arg: false,
          });
        }
      }
      TokenTree::Ident(_) | TokenTree::Punct(_) => {}
    }
  }
}

/// Whether a callee is `Path::new` or `PathBuf::from`, by its last two segments.
fn is_path_constructor(path: &syn::Path) -> bool {
  match path_segments(path).as_slice() {
    [.., owner, function] => matches!((owner.as_str(), function.as_str()), ("Path", "new") | ("PathBuf", "from")),
    _ => false,
  }
}

/// The attributes of an item. Syn keeps them on each variant; a variant it adds later fails here, not silently.
fn item_attrs(item: &Item) -> &[Attribute] {
  match item {
    Item::Const(node) => &node.attrs,
    Item::Enum(node) => &node.attrs,
    Item::ExternCrate(node) => &node.attrs,
    Item::Fn(node) => &node.attrs,
    Item::ForeignMod(node) => &node.attrs,
    Item::Impl(node) => &node.attrs,
    Item::Macro(node) => &node.attrs,
    Item::Mod(node) => &node.attrs,
    Item::Static(node) => &node.attrs,
    Item::Struct(node) => &node.attrs,
    Item::Trait(node) => &node.attrs,
    Item::TraitAlias(node) => &node.attrs,
    Item::Type(node) => &node.attrs,
    Item::Union(node) => &node.attrs,
    Item::Use(node) => &node.attrs,
    Item::Verbatim(_) => &[],
    _ => panic!("syn added an Item variant; the owner rule must read its attributes"),
  }
}

/// The attributes of an impl item; see `item_attrs`.
fn impl_item_attrs(item: &ImplItem) -> &[Attribute] {
  match item {
    ImplItem::Const(node) => &node.attrs,
    ImplItem::Fn(node) => &node.attrs,
    ImplItem::Type(node) => &node.attrs,
    ImplItem::Macro(node) => &node.attrs,
    ImplItem::Verbatim(_) => &[],
    _ => panic!("syn added an ImplItem variant; the owner rule must read its attributes"),
  }
}

/// The attributes of an expression; see `item_attrs`. A statement's attributes sit on its expression.
fn expr_attrs(expr: &Expr) -> &[Attribute] {
  match expr {
    Expr::Array(node) => &node.attrs,
    Expr::Assign(node) => &node.attrs,
    Expr::Async(node) => &node.attrs,
    Expr::Await(node) => &node.attrs,
    Expr::Binary(node) => &node.attrs,
    Expr::Block(node) => &node.attrs,
    Expr::Break(node) => &node.attrs,
    Expr::Call(node) => &node.attrs,
    Expr::Cast(node) => &node.attrs,
    Expr::Closure(node) => &node.attrs,
    Expr::Const(node) => &node.attrs,
    Expr::Continue(node) => &node.attrs,
    Expr::Field(node) => &node.attrs,
    Expr::ForLoop(node) => &node.attrs,
    Expr::Group(node) => &node.attrs,
    Expr::If(node) => &node.attrs,
    Expr::Index(node) => &node.attrs,
    Expr::Infer(node) => &node.attrs,
    Expr::Let(node) => &node.attrs,
    Expr::Lit(node) => &node.attrs,
    Expr::Loop(node) => &node.attrs,
    Expr::Macro(node) => &node.attrs,
    Expr::Match(node) => &node.attrs,
    Expr::MethodCall(node) => &node.attrs,
    Expr::Paren(node) => &node.attrs,
    Expr::Path(node) => &node.attrs,
    Expr::Range(node) => &node.attrs,
    Expr::RawAddr(node) => &node.attrs,
    Expr::Reference(node) => &node.attrs,
    Expr::Repeat(node) => &node.attrs,
    Expr::Return(node) => &node.attrs,
    Expr::Struct(node) => &node.attrs,
    Expr::Try(node) => &node.attrs,
    Expr::TryBlock(node) => &node.attrs,
    Expr::Tuple(node) => &node.attrs,
    Expr::Unary(node) => &node.attrs,
    Expr::Unsafe(node) => &node.attrs,
    Expr::Verbatim(_) => &[],
    Expr::While(node) => &node.attrs,
    Expr::Yield(node) => &node.attrs,
    _ => panic!("syn added an Expr variant; the owner rule must read its attributes"),
  }
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
