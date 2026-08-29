use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use syn::visit::{self, Visit};
use syn::{Attribute, File, Item, Path as RustPath, UseTree};
use usagi_core::infrastructure::store::issue::IssueStore;

const FACES: [(&str, &[&str]); 4] = [
    ("core", &[]),
    ("cli", &["usagi-core"]),
    ("daemon", &["usagi-core"]),
    ("tui", &["usagi-core"]),
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

#[test]
fn committed_issue_sources_are_parseable_and_unambiguous() {
    IssueStore::new(workspace_root())
        .validate_source_set()
        .expect("committed .usagi/issues Markdown must have valid unique identities");
}

fn manifest_usagi_dependencies(path: &Path) -> BTreeSet<String> {
    let source = fs::read_to_string(path).expect("manifest is readable");
    let manifest: toml::Value = toml::from_str(&source).expect("manifest is valid TOML");
    let mut found = BTreeSet::new();
    collect_dependency_tables(&manifest, &mut found);
    found
}

fn collect_dependency_tables(manifest: &toml::Value, found: &mut BTreeSet<String>) {
    let Some(table) = manifest.as_table() else {
        return;
    };
    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(dependencies) = table.get(section).and_then(toml::Value::as_table) {
            found.extend(
                dependencies
                    .keys()
                    .filter(|name| name.starts_with("usagi-"))
                    .cloned(),
            );
        }
    }
    if let Some(targets) = table.get("target").and_then(toml::Value::as_table) {
        for target in targets.values() {
            collect_dependency_tables(target, found);
        }
    }
}

#[test]
fn workspace_manifests_keep_faces_independent() {
    let root = workspace_root();
    for (face, expected) in FACES {
        let actual =
            manifest_usagi_dependencies(&root.join("crates").join(face).join("Cargo.toml"));
        let expected = expected
            .iter()
            .map(|dependency| (*dependency).to_owned())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            actual, expected,
            "crates/{face} may depend only on the documented usagi crates"
        );
    }

    assert_eq!(
        manifest_usagi_dependencies(&root.join("Cargo.toml")),
        ["usagi-cli", "usagi-core", "usagi-daemon", "usagi-tui"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        "the composition root owns all face dependencies"
    );
}

#[test]
fn manifest_guard_includes_test_build_and_target_specific_dependencies() {
    let fixture = tempfile::tempdir().expect("fixture directory");
    let manifest = fixture.path().join("Cargo.toml");
    fs::write(
        &manifest,
        r#"
[package]
name = "fixture"
version = "0.0.0"

[dependencies]
usagi-core = "0"

[dev-dependencies]
usagi-tui = "0"

[target.'cfg(unix)'.build-dependencies]
usagi-daemon = "0"
"#,
    )
    .expect("fixture manifest");

    assert_eq!(
        manifest_usagi_dependencies(&manifest),
        ["usagi-core", "usagi-daemon", "usagi-tui"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    );
}

fn rust_sources(root: &Path, sources: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(root).expect("source directory is readable") {
        let entry = entry.expect("source entry is readable");
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_none_or(|name| name != "tests") {
                rust_sources(&path, sources);
            }
        } else if path.extension().is_some_and(|extension| extension == "rs")
            && path.file_stem().is_none_or(|name| name != "tests")
        {
            sources.push(path);
        }
    }
}

fn cfg_test(attributes: &[Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        attribute.path().is_ident("cfg")
            && attribute
                .meta
                .require_list()
                .is_ok_and(|list| list.tokens.to_string() == "test")
    })
}

struct DependencyVisitor<'a> {
    forbidden: &'a BTreeSet<&'a str>,
    violations: BTreeSet<String>,
}

impl DependencyVisitor<'_> {
    fn inspect<'a>(&mut self, segments: impl IntoIterator<Item = &'a str>) {
        let segments = segments.into_iter().collect::<Vec<_>>();
        let local = segments
            .first()
            .is_some_and(|segment| matches!(*segment, "crate" | "self" | "super"));
        if local {
            for segment in segments {
                if self.forbidden.contains(segment) {
                    self.violations.insert(segment.to_owned());
                }
            }
        }
    }

    fn inspect_use(&mut self, tree: &UseTree, segments: &mut Vec<String>) {
        match tree {
            UseTree::Path(path) => {
                segments.push(path.ident.to_string());
                self.inspect_use(&path.tree, segments);
                segments.pop();
            }
            UseTree::Name(name) => {
                segments.push(name.ident.to_string());
                self.inspect(segments.iter().map(String::as_str));
                segments.pop();
            }
            UseTree::Rename(rename) => {
                segments.push(rename.ident.to_string());
                self.inspect(segments.iter().map(String::as_str));
                segments.pop();
            }
            UseTree::Glob(_) => self.inspect(segments.iter().map(String::as_str)),
            UseTree::Group(group) => {
                for item in &group.items {
                    self.inspect_use(item, segments);
                }
            }
        }
    }
}

impl<'ast> Visit<'ast> for DependencyVisitor<'_> {
    fn visit_item(&mut self, node: &'ast Item) {
        let attributes: &[syn::Attribute] = match node {
            Item::Const(item) => &item.attrs,
            Item::Enum(item) => &item.attrs,
            Item::ExternCrate(item) => &item.attrs,
            Item::Fn(item) => &item.attrs,
            Item::ForeignMod(item) => &item.attrs,
            Item::Impl(item) => &item.attrs,
            Item::Macro(item) => &item.attrs,
            Item::Mod(item) => &item.attrs,
            Item::Static(item) => &item.attrs,
            Item::Struct(item) => &item.attrs,
            Item::Trait(item) => &item.attrs,
            Item::TraitAlias(item) => &item.attrs,
            Item::Type(item) => &item.attrs,
            Item::Union(item) => &item.attrs,
            Item::Use(item) => &item.attrs,
            _ => &[],
        };
        if !cfg_test(attributes) {
            visit::visit_item(self, node);
        }
    }

    fn visit_item_use(&mut self, node: &'ast syn::ItemUse) {
        self.inspect_use(&node.tree, &mut Vec::new());
    }

    fn visit_path(&mut self, node: &'ast RustPath) {
        self.inspect(
            node.segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect::<Vec<_>>()
                .iter()
                .map(String::as_str),
        );
        visit::visit_path(self, node);
    }
}

fn layer_violations(root: &Path, forbidden: &[&str]) -> BTreeMap<PathBuf, BTreeSet<String>> {
    let forbidden = forbidden.iter().copied().collect::<BTreeSet<_>>();
    let mut sources = Vec::new();
    rust_sources(root, &mut sources);
    sources
        .into_iter()
        .filter_map(|path| {
            let source = fs::read_to_string(&path).expect("Rust source is readable");
            let syntax: File = syn::parse_file(&source).expect("Rust source parses");
            let mut visitor = DependencyVisitor {
                forbidden: &forbidden,
                violations: BTreeSet::new(),
            };
            visitor.visit_file(&syntax);
            (!visitor.violations.is_empty()).then_some((path, visitor.violations))
        })
        .collect()
}

#[test]
fn source_guard_reads_syntax_and_ignores_non_production_references() {
    let fixture = tempfile::tempdir().expect("fixture directory");
    let source = fixture.path().join("layer.rs");
    fs::write(
        &source,
        r"
//! A doc link to [`crate::presentation`] is not a dependency.
#[cfg(test)]
mod tests {
    use crate::presentation::TestView;
}
#[cfg(test)]
fn test_only_adapter() {
    crate::infrastructure::run();
}
use crate::{
    presentation::View as RenamedView,
};
fn call_adapter() {
    crate::infrastructure::run();
}
",
    )
    .expect("fixture source");

    assert_eq!(
        layer_violations(fixture.path(), &["presentation", "infrastructure"]),
        [(
            source,
            ["infrastructure".to_owned(), "presentation".to_owned()]
                .into_iter()
                .collect(),
        )]
        .into_iter()
        .collect()
    );
}

#[test]
fn source_layers_follow_the_documented_dependency_matrix() {
    let root = workspace_root();
    let mut violations = BTreeMap::new();

    violations.extend(layer_violations(
        &root.join("crates/core/src/domain"),
        &["usecase", "infrastructure", "presentation"],
    ));
    violations.extend(layer_violations(
        &root.join("crates/core/src/infrastructure"),
        &["presentation"],
    ));
    for face in ["daemon", "tui"] {
        violations.extend(layer_violations(
            &root.join("crates").join(face).join("src/usecase"),
            &["infrastructure", "presentation"],
        ));
        violations.extend(layer_violations(
            &root.join("crates").join(face).join("src/infrastructure"),
            &["presentation"],
        ));
    }

    assert!(
        violations.is_empty(),
        "source dependency matrix violations:\n{violations:#?}"
    );
}

#[test]
fn daemon_tenant_control_stays_out_of_the_socket_and_lifecycle_composition_module() {
    let root = workspace_root();
    let composition = fs::read_to_string(root.join("src/runtime/daemon.rs"))
        .expect("daemon composition source is readable");
    let tenant = fs::read_to_string(root.join("src/runtime/daemon/tenant_control.rs"))
        .expect("tenant control source is readable");

    assert!(composition.contains("mod tenant_control;"));
    assert!(composition.contains("tenant_control::dispatch("));
    assert!(!composition.contains("fn dispatch_tenant("));
    for source in [&composition, &tenant] {
        assert!(
            !source
                .lines()
                .take(10)
                .any(|line| line.trim_start().starts_with("#![coverage(off)]")),
            "a module split must not remove production composition from coverage"
        );
    }
    assert!(
        tenant.lines().count() <= 250,
        "tenant control composition grew beyond its reviewable boundary"
    );
    assert!(tenant.contains("pub(super) fn dispatch("));
    assert!(tenant.contains("fn inventory("));
    assert!(tenant.contains("fn retire("));
}
