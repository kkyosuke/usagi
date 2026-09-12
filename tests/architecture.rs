use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use syn::visit::{self, Visit};
use syn::{Attribute, Expr, File, Item, Pat, Path as RustPath, Stmt, UseTree};
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
    required_root: Option<&'a str>,
    violations: BTreeSet<String>,
}

impl DependencyVisitor<'_> {
    fn inspect<'a>(&mut self, segments: impl IntoIterator<Item = &'a str>) {
        let segments = segments.into_iter().collect::<Vec<_>>();
        let in_scope = self.required_root.map_or_else(
            || {
                segments
                    .first()
                    .is_some_and(|segment| matches!(*segment, "crate" | "self" | "super"))
            },
            |root| segments.first() == Some(&root),
        );
        if in_scope {
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
                required_root: None,
                violations: BTreeSet::new(),
            };
            visitor.visit_file(&syntax);
            (!visitor.violations.is_empty()).then_some((path, visitor.violations))
        })
        .collect()
}

#[test]
fn tui_views_are_pure_projections_without_filesystem_or_process_io() {
    let root = workspace_root().join("crates/tui/src/presentation/views");
    let forbidden = ["fs", "process"].into_iter().collect::<BTreeSet<_>>();
    let mut sources = Vec::new();
    rust_sources(&root, &mut sources);
    let violations = sources
        .into_iter()
        .filter_map(|path| {
            let source = fs::read_to_string(&path).expect("TUI view source is readable");
            let syntax: File = syn::parse_file(&source).expect("TUI view source parses");
            let mut visitor = DependencyVisitor {
                forbidden: &forbidden,
                required_root: Some("std"),
                violations: BTreeSet::new(),
            };
            visitor.visit_file(&syntax);
            (!visitor.violations.is_empty()).then_some((path, visitor.violations))
        })
        .collect::<BTreeMap<_, _>>();

    assert!(
        violations.is_empty(),
        "TUI views must request IO through an injected port:\n{violations:#?}"
    );
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
    violations.extend(layer_violations(
        &root.join("crates/core/src/usecase"),
        &["infrastructure"],
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

fn calls_heavy_e2e_lock(statement: &Stmt) -> bool {
    let Stmt::Local(local) = statement else {
        return false;
    };
    // A wildcard (`let _ = ...`) drops the guard at this statement and does not
    // serialize the test body. Require a real binding so the RAII lock remains
    // alive until the function scope ends.
    if !matches!(local.pat, Pat::Ident(_)) {
        return false;
    }
    let Some(initializer) = &local.init else {
        return false;
    };
    let Expr::Call(call) = initializer.expr.as_ref() else {
        return false;
    };
    let Expr::Path(path) = call.func.as_ref() else {
        return false;
    };
    path.path
        .segments
        .last()
        .is_some_and(|segment| segment.ident == "heavy_e2e_lock")
}

#[test]
fn heavy_e2e_guard_must_be_retained_by_a_local_binding() {
    let retained: Stmt = syn::parse_str("let _guard = daemon_fixture::heavy_e2e_lock();")
        .expect("retained guard statement parses");
    let discarded: Stmt = syn::parse_str("let _ = daemon_fixture::heavy_e2e_lock();")
        .expect("discarded guard statement parses");

    assert!(calls_heavy_e2e_lock(&retained));
    assert!(!calls_heavy_e2e_lock(&discarded));
}

#[test]
fn shipping_cli_integration_tests_enter_the_shared_e2e_lane_first() {
    let source = fs::read_to_string(workspace_root().join("tests/cli_tui.rs"))
        .expect("shipping CLI integration source is readable");
    let syntax: File = syn::parse_file(&source).expect("shipping CLI integration source parses");
    let unlocked = syntax
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Fn(function)
                if function
                    .attrs
                    .iter()
                    .any(|attribute| attribute.path().is_ident("test")) =>
            {
                Some(function)
            }
            _ => None,
        })
        .filter(|function| {
            !function
                .block
                .stmts
                .iter()
                .find(|statement| !matches!(statement, Stmt::Item(_)))
                .is_some_and(calls_heavy_e2e_lock)
        })
        .map(|function| function.sig.ident.to_string())
        .collect::<Vec<_>>();

    assert!(
        unlocked.is_empty(),
        "every tests/cli_tui.rs test must acquire heavy_e2e_lock first: {unlocked:?}"
    );
}

#[test]
fn tui_application_runtime_ports_are_not_declared_by_presentation() {
    let root = workspace_root();
    let source = fs::read_to_string(root.join("crates/tui/src/presentation/mod.rs"))
        .expect("TUI presentation source is readable");
    let syntax: File = syn::parse_file(&source).expect("TUI presentation source parses");
    let declared = syntax
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Trait(item) => Some(item.ident.to_string()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let application_ports = [
        "AgentCommandPort",
        "AgentCommandPortFactory",
        "DecisionCommandPort",
        "DesktopNotificationPort",
        "EnvironmentStorePort",
        "ExternalTerminalPort",
        "GardenInventoryPort",
        "MetricsPort",
        "MetricsPortFactory",
        "PaneLaunchCommandPort",
        "RestoreConnectionPort",
        "SessionBranchCatalogPort",
        "SessionCatalogPort",
        "SessionCommandPort",
        "SessionCommandPortFactory",
        "SessionRefreshPort",
        "SessionWorktreeScanPort",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<BTreeSet<_>>();

    assert!(
        declared.is_disjoint(&application_ports),
        "application runtime ports belong in tui/usecase, not presentation: {:?}",
        declared
            .intersection(&application_ports)
            .collect::<Vec<_>>()
    );
    assert!(
        source.contains("struct WorkspaceIoRuntime"),
        "the presentation loop must name its transport-only coordinator explicitly"
    );
    assert!(
        !source.contains("WorkspaceUi"),
        "the retired dual-state WorkspaceUi name must not return"
    );
}

#[test]
fn tui_presentation_discovers_session_catalogs_through_an_application_port() {
    let root = workspace_root();
    let presentation = fs::read_to_string(root.join("crates/tui/src/presentation/mod.rs"))
        .expect("TUI presentation source is readable");
    let ports =
        fs::read_to_string(root.join("crates/tui/src/usecase/application/runtime_ports.rs"))
            .expect("TUI runtime ports are readable");

    assert!(ports.contains("trait SessionCatalogPort"));
    assert!(ports.contains("trait SessionBranchCatalogPort"));
    assert!(ports.contains("fn branch_worker(&self) -> Box<dyn SessionBranchCatalogPort>"));
    assert!(presentation.contains("session_catalogs.roles("));
    assert!(presentation.contains("session_catalogs.branches("));
    assert!(presentation.contains("session_catalogs.branch_worker()"));
    assert!(!presentation.contains("Arc::clone(&session_catalogs)"));
    for forbidden in [
        "infrastructure::role_catalog",
        "infrastructure::git::confined_git_command",
    ] {
        assert!(
            !presentation.contains(forbidden),
            "session catalog IO belongs to the binary composition adapter: {forbidden}"
        );
    }
}

#[test]
fn tui_presentation_keeps_tests_and_observation_policy_out_of_its_composition_module() {
    let root = workspace_root();
    let composition = fs::read_to_string(root.join("crates/tui/src/presentation/mod.rs"))
        .expect("TUI presentation source is readable");
    let banner = fs::read_to_string(root.join("crates/tui/src/presentation/banner.rs"))
        .expect("TUI banner presentation is readable");
    let startup = fs::read_to_string(root.join("crates/tui/src/presentation/startup.rs"))
        .expect("TUI startup presentation is readable");
    let tests = fs::read_to_string(root.join("crates/tui/src/presentation/tests.rs"))
        .expect("TUI presentation tests are readable");
    let observation =
        fs::read_to_string(root.join("crates/tui/src/usecase/application/observation_lane.rs"))
            .expect("TUI observation policy is readable");

    for module in ["mod banner;", "mod startup;", "mod tests;"] {
        assert!(composition.contains(module));
    }
    assert!(!composition.contains("mod tests {"));
    assert!(!composition.contains("pub struct BannerScreenRunner"));
    assert!(!composition.contains("pub struct StartupSplash"));
    assert!(banner.contains("pub struct BannerScreenRunner"));
    assert!(startup.contains("pub struct StartupSplash"));
    assert!(tests.contains("#![coverage(off)]"));
    assert!(observation.contains("struct ObservationLane"));
    assert!(!composition.contains("struct GardenObservation {"));
    assert!(!composition.contains("struct WorkRunObservation {"));
    assert!(
        composition.lines().count() <= 10_000,
        "TUI presentation composition grew beyond its reviewable boundary"
    );
}

#[test]
fn tui_controller_keeps_its_bounded_contexts_and_tests_out_of_the_home_reducer() {
    let root = workspace_root();
    let controller =
        fs::read_to_string(root.join("crates/tui/src/usecase/application/controller.rs"))
            .expect("TUI Home controller is readable");
    let entry =
        fs::read_to_string(root.join("crates/tui/src/usecase/application/controller/entry.rs"))
            .expect("TUI entry controller is readable");
    let new = fs::read_to_string(root.join("crates/tui/src/usecase/application/controller/new.rs"))
        .expect("TUI new-workspace controller is readable");
    let tests =
        fs::read_to_string(root.join("crates/tui/src/usecase/application/controller/tests.rs"))
            .expect("TUI controller tests are readable");
    let pull_requests = fs::read_to_string(
        root.join("crates/tui/src/usecase/application/controller/pull_requests.rs"),
    )
    .expect("TUI pull request controller is readable");

    for module in [
        "mod entry;",
        "mod new;",
        "mod preview;",
        "mod pull_requests;",
        "mod tests;",
    ] {
        assert!(controller.contains(module));
    }
    assert!(!controller.contains("mod tests {"));
    assert!(!controller.contains("pub struct EntryState"));
    assert!(!controller.contains("pub struct NewState"));
    assert!(!controller.contains("pub struct PrOverlay"));
    assert!(!controller.contains("pub struct PreviewOverlay"));
    assert!(pull_requests.contains("pub struct PrOverlay"));
    assert!(entry.contains("pub fn update_entry("));
    assert!(new.contains("pub fn update_new("));
    assert!(tests.contains("#![coverage(off)]"));
    assert!(
        controller.lines().count() <= 6_500,
        "TUI Home controller grew beyond its reviewable boundary"
    );
}

#[test]
fn clipboard_platform_variants_are_compiled_only_for_their_targets_or_tests() {
    let root = workspace_root();
    let source = fs::read_to_string(root.join("src/runtime/clipboard.rs"))
        .expect("clipboard adapter is readable");

    assert!(!source.contains("allow(dead_code)"));
    assert!(source.contains("#[cfg(any(test, target_os = \"macos\"))]"));
    assert!(source.contains("#[cfg(any(test, target_os = \"windows\"))]"));
    assert!(
        source.contains(
            "#[cfg(any(test, not(any(target_os = \"macos\", target_os = \"windows\"))))]"
        )
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

#[test]
fn daemon_request_dispatch_stays_out_of_the_socket_and_lifecycle_composition_module() {
    let root = workspace_root();
    let composition = fs::read_to_string(root.join("src/runtime/daemon.rs"))
        .expect("daemon composition source is readable");
    let dispatch = fs::read_to_string(root.join("src/runtime/daemon/dispatch.rs"))
        .expect("daemon dispatch source is readable");

    assert!(composition.contains("mod dispatch;"));
    assert!(
        !composition.contains("use dispatch::*;"),
        "daemon dispatch must expose an explicit composition surface"
    );
    assert!(composition.contains("fn start_supervisor_recovery("));
    assert!(!dispatch.contains("fn start_supervisor_recovery("));
    assert!(!dispatch.contains("usagi-supervisor-recovery"));
    for symbol in [
        "fn dispatch_agent(",
        "fn dispatch_session(",
        "fn dispatch_supervisor_tool(",
        "fn dispatch_user_decision(",
        "fn dispatch_metrics(",
    ] {
        assert!(
            !composition.contains(symbol),
            "{symbol} must stay out of socket and process lifecycle composition"
        );
        assert!(
            dispatch.contains(symbol),
            "{symbol} must remain owned by the request dispatch adapter"
        );
    }
    assert!(
        !dispatch
            .lines()
            .take(10)
            .any(|line| line.trim_start().starts_with("#![coverage(off)]")),
        "moving dispatch must not exclude the module from coverage"
    );
    assert!(
        dispatch.lines().count() <= 6_000,
        "daemon request dispatch grew beyond its reviewable boundary"
    );
}

#[test]
fn daemon_agent_provisioning_stays_in_its_product_boundary() {
    let root = workspace_root();
    let composition = fs::read_to_string(root.join("src/runtime/daemon.rs"))
        .expect("daemon composition source is readable");
    let provisioning = fs::read_to_string(root.join("src/runtime/daemon/agent_provisioning.rs"))
        .expect("agent provisioning source is readable");
    let agy = fs::read_to_string(root.join("src/runtime/daemon/agent_provisioning/agy.rs"))
        .expect("Antigravity provisioning source is readable");
    let secure_path = fs::read_to_string(root.join("src/runtime/daemon/secure_path.rs"))
        .expect("daemon secure-path source is readable");

    assert!(composition.contains("mod agent_provisioning;"));
    for symbol in [
        "struct RootCodexProvisioner",
        "struct RootClaudeProvisioner",
        "fn claude_sandbox_launcher(",
        "fn working_directories(",
        "fn effective_role_instruction(",
        "fn repair_agent_codex_arg0_permissions(",
    ] {
        assert!(
            !composition.contains(symbol),
            "{symbol} must stay out of the daemon socket/lifecycle composition"
        );
        assert!(
            provisioning.contains(symbol),
            "{symbol} must remain owned by Agent provisioning"
        );
    }
    assert!(!composition.contains("use agent_provisioning::*;"));
    assert!(!provisioning.contains("use super::*;"));
    assert!(!composition.contains("struct RootAgyProvisioner"));
    assert!(agy.contains("struct RootAgyProvisioner"));
    assert!(!agy.contains("use super::*;"));
    assert!(!provisioning.contains("fn validate_owned_directory("));
    assert!(secure_path.contains("fn validate_owned_directory("));
    assert!(
        provisioning.lines().count() <= 1_400,
        "agent provisioning grew beyond its reviewable product boundary"
    );
    assert!(
        agy.lines().count() <= 300,
        "Antigravity provisioning grew beyond its reviewable product boundary"
    );
}
