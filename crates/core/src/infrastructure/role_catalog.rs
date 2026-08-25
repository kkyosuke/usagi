//! Versioned global/workspace role-catalog reader.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::persistence::json_file::write_text_atomic;

use crate::domain::role::{
    DelegationPolicy, EffectiveRoleCatalog, MAX_ROLE_INSTRUCTIONS_BYTES, RoleDefaults,
    RoleDefinition, RoleId, RoleScope,
};
use crate::domain::settings::TeamTemplate;
use crate::infrastructure::store::settings::WorkspaceSettingsStore;
use crate::infrastructure::store::workspace::Storage;

const CATALOG_VERSION: u16 = 1;
const MAX_CATALOG_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RoleCatalogFile {
    version: u16,
    #[serde(default)]
    defaults: RoleDefaults,
    #[serde(default)]
    roles: BTreeMap<RoleId, RoleDefinition>,
}

#[derive(Debug)]
pub enum RoleCatalogError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    TooLarge(PathBuf),
    Malformed {
        path: PathBuf,
        source: toml::de::Error,
    },
    UnsupportedVersion {
        path: PathBuf,
        version: u16,
    },
    EmptyScopes {
        path: PathBuf,
        role: RoleId,
    },
    InvalidInstructions {
        path: PathBuf,
        role: RoleId,
    },
    InvalidDefault {
        scope: RoleScope,
        role: RoleId,
    },
}

impl fmt::Display for RoleCatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(formatter, "could not read {}: {source}", path.display())
            }
            Self::TooLarge(path) => {
                write!(formatter, "role catalog {} exceeds 1 MiB", path.display())
            }
            Self::Malformed { path, source } => write!(
                formatter,
                "invalid role catalog {}: {source}",
                path.display()
            ),
            Self::UnsupportedVersion { path, version } => write!(
                formatter,
                "unsupported role catalog version {version} in {}",
                path.display()
            ),
            Self::EmptyScopes { path, role } => write!(
                formatter,
                "role \"{role}\" has no scopes in {}",
                path.display()
            ),
            Self::InvalidInstructions { path, role } => write!(
                formatter,
                "role \"{role}\" instructions are invalid in {}",
                path.display()
            ),
            Self::InvalidDefault { scope, role } => write!(
                formatter,
                "default role \"{role}\" is not valid for {scope:?} scope"
            ),
        }
    }
}

impl std::error::Error for RoleCatalogError {}

/// Reads, validates, and deterministically merges the global and workspace catalogs.
///
/// # Errors
///
/// Returns [`RoleCatalogError`] when either catalog cannot be read, exceeds
/// the size limit, has an unsupported or malformed schema, or fails semantic
/// role/default validation.
pub fn load_effective(
    data_home: &Path,
    workspace_root: &Path,
) -> Result<EffectiveRoleCatalog, RoleCatalogError> {
    let global_path = data_home.join("roles.toml");
    let workspace_path = workspace_root.join(".usagi").join("roles.toml");
    let global = read_optional(&global_path)?;
    let workspace = read_optional(&workspace_path)?;
    let template = load_team_template(data_home, workspace_root);
    let mut effective = builtin_catalog(template);
    effective.configured =
        template != TeamTemplate::None || global.is_some() || workspace.is_some();
    if let Some(global) = global {
        if global.defaults.root.is_some() {
            effective.defaults.root = global.defaults.root;
        }
        if global.defaults.session.is_some() {
            effective.defaults.session = global.defaults.session;
        }
        effective.roles.extend(global.roles);
    }
    if let Some(workspace) = workspace {
        if workspace.defaults.root.is_some() {
            effective.defaults.root = workspace.defaults.root;
        }
        if workspace.defaults.session.is_some() {
            effective.defaults.session = workspace.defaults.session;
        }
        for (id, role) in workspace.roles {
            effective.roles.insert(id, role);
        }
    }
    validate_default(&effective, RoleScope::Root)?;
    validate_default(&effective, RoleScope::Session)?;
    Ok(effective)
}

fn load_team_template(data_home: &Path, workspace_root: &Path) -> TeamTemplate {
    let Ok(global) = Storage::new(data_home).load_settings() else {
        return TeamTemplate::None;
    };
    let Ok(local) = WorkspaceSettingsStore::new(workspace_root).load() else {
        return TeamTemplate::None;
    };
    global.with_local(&local).team_template
}

/// Construct one trusted built-in team catalog.
#[must_use]
pub fn builtin_catalog(template: TeamTemplate) -> EffectiveRoleCatalog {
    match template {
        TeamTemplate::None => EffectiveRoleCatalog::default(),
        TeamTemplate::Hierarchical => hierarchical_catalog(),
        TeamTemplate::Flat => flat_catalog(),
        TeamTemplate::Pipeline => pipeline_catalog(),
    }
}

fn hierarchical_catalog() -> EffectiveRoleCatalog {
    catalog(
        defaults("director", "manager"),
        [
            built_in_role(
                "director",
                "全体方針と結果統合",
                RoleScope::Root,
                "要求を分解し、小さいタスクはWorkerへ、大きいタスクはManagerへ委譲して結果を統合する。",
                &["manager", "worker"],
                2,
            ),
            built_in_role(
                "manager",
                "タスクの分解と統合",
                RoleScope::Session,
                "担当範囲をWorkerへ委譲し、各結果を検証して直近のcallerへ報告する。",
                &["worker"],
                2,
            ),
            built_in_role(
                "worker",
                "実行と検証",
                RoleScope::Session,
                "依頼された作業を実行し、結果と検証内容をcallerへ報告する。",
                &[],
                2,
            ),
        ],
    )
}

fn flat_catalog() -> EffectiveRoleCatalog {
    catalog(
        defaults("director", "worker"),
        [
            built_in_role(
                "director",
                "全体調整と結果統合",
                RoleScope::Root,
                "独立した作業をWorkerへ直接委譲し、結果を統合する。",
                &["worker"],
                1,
            ),
            built_in_role(
                "worker",
                "実行と検証",
                RoleScope::Session,
                "依頼された作業を自律的に実行し、結果と検証内容をDirectorへ報告する。",
                &[],
                1,
            ),
        ],
    )
}

fn pipeline_catalog() -> EffectiveRoleCatalog {
    catalog(
        defaults("director", "planner"),
        [
            built_in_role(
                "director",
                "パイプライン全体の統合",
                RoleScope::Root,
                "要求をPlannerへ渡し、完了した工程の結果を統合する。",
                &["planner"],
                3,
            ),
            built_in_role(
                "planner",
                "計画と受入条件の定義",
                RoleScope::Session,
                "要求を実行可能な計画と受入条件にしてImplementerへ委譲し、工程結果を確認してcallerへ報告する。",
                &["implementer"],
                3,
            ),
            built_in_role(
                "implementer",
                "計画の実装",
                RoleScope::Session,
                "計画を実装して成果物と受入条件をTesterへ委譲し、検証結果を反映してcallerへ報告する。",
                &["tester"],
                3,
            ),
            built_in_role(
                "tester",
                "受入条件の検証",
                RoleScope::Session,
                "成果物を受入条件に照らして検証し、結果をcallerへ報告する。",
                &[],
                3,
            ),
        ],
    )
}

fn catalog<const N: usize>(
    defaults: RoleDefaults,
    roles: [(RoleId, RoleDefinition); N],
) -> EffectiveRoleCatalog {
    EffectiveRoleCatalog {
        configured: true,
        defaults,
        roles: BTreeMap::from(roles),
    }
}

fn built_in_role(
    id: &str,
    summary: &str,
    scope: RoleScope,
    instructions: &str,
    children: &[&str],
    max_depth: usize,
) -> (RoleId, RoleDefinition) {
    (
        role_id(id),
        RoleDefinition {
            summary: summary.to_owned(),
            scopes: BTreeSet::from([scope]),
            instructions: instructions.to_owned(),
            delegation: Some(DelegationPolicy {
                enabled: !children.is_empty(),
                child_roles: children.iter().map(|child| role_id(child)).collect(),
                max_depth,
                max_concurrency: 4,
            }),
        },
    )
}

fn role_id(value: &str) -> RoleId {
    RoleId::new(value).expect("built-in role IDs are valid")
}

fn defaults(root: &str, session: &str) -> RoleDefaults {
    RoleDefaults {
        root: Some(role_id(root)),
        session: Some(role_id(session)),
    }
}

fn read_optional(path: &Path) -> Result<Option<RoleCatalogFile>, RoleCatalogError> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(RoleCatalogError::Io {
                path: path.to_owned(),
                source,
            });
        }
    };
    if metadata.len() > MAX_CATALOG_BYTES {
        return Err(RoleCatalogError::TooLarge(path.to_owned()));
    }
    let source = fs::read_to_string(path).map_err(|source| RoleCatalogError::Io {
        path: path.to_owned(),
        source,
    })?;
    parse_source(path, &source).map(Some)
}

fn parse_source(path: &Path, source: &str) -> Result<RoleCatalogFile, RoleCatalogError> {
    let catalog: RoleCatalogFile =
        toml::from_str(source).map_err(|source| RoleCatalogError::Malformed {
            path: path.to_owned(),
            source,
        })?;
    if catalog.version != CATALOG_VERSION {
        return Err(RoleCatalogError::UnsupportedVersion {
            path: path.to_owned(),
            version: catalog.version,
        });
    }
    for (role, definition) in &catalog.roles {
        if definition.scopes.is_empty() {
            return Err(RoleCatalogError::EmptyScopes {
                path: path.to_owned(),
                role: role.clone(),
            });
        }
        if definition.instructions.len() > MAX_ROLE_INSTRUCTIONS_BYTES
            || definition.instructions.contains('\0')
        {
            return Err(RoleCatalogError::InvalidInstructions {
                path: path.to_owned(),
                role: role.clone(),
            });
        }
    }
    Ok(catalog)
}

/// Read one catalog layer as exact editable TOML. A missing file starts with a
/// valid versioned document; existing comments, ordering, and whitespace are
/// returned unchanged.
///
/// # Errors
///
/// Returns a catalog IO, size, schema, or semantic validation error.
pub fn read_source(path: &Path) -> Result<String, RoleCatalogError> {
    match fs::read_to_string(path) {
        Ok(source) => {
            if source.len() as u64 > MAX_CATALOG_BYTES {
                return Err(RoleCatalogError::TooLarge(path.to_owned()));
            }
            parse_source(path, &source)?;
            Ok(source)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok("version = 1\n".to_owned())
        }
        Err(source) => Err(RoleCatalogError::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

/// Validate and atomically replace one catalog layer without reserializing it.
/// This preserves every comment and formatting choice in the supplied document.
///
/// # Errors
///
/// Returns a validation or atomic persistence error without replacing the target.
pub fn write_source(path: &Path, source: &str) -> Result<(), RoleCatalogError> {
    if source.len() as u64 > MAX_CATALOG_BYTES {
        return Err(RoleCatalogError::TooLarge(path.to_owned()));
    }
    parse_source(path, source)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| RoleCatalogError::Io {
        path: parent.to_owned(),
        source,
    })?;
    write_text_atomic(path, source).map_err(|error| RoleCatalogError::Io {
        path: path.to_owned(),
        source: std::io::Error::other(error.to_string()),
    })
}

/// Which versioned catalog layer the editor owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogLayer {
    Global,
    Workspace,
}

fn layer_path(data_home: &Path, workspace_root: &Path, layer: CatalogLayer) -> PathBuf {
    match layer {
        CatalogLayer::Global => data_home.join("roles.toml"),
        CatalogLayer::Workspace => workspace_root.join(".usagi").join("roles.toml"),
    }
}

/// Read one editable layer verbatim.
///
/// # Errors
///
/// Returns an IO or catalog validation error for the selected layer.
pub fn read_layer_source(
    data_home: &Path,
    workspace_root: &Path,
    layer: CatalogLayer,
) -> Result<String, RoleCatalogError> {
    read_source(&layer_path(data_home, workspace_root, layer))
}

/// Validate the replacement in the effective two-layer context, then atomically
/// write only the selected layer. No serialization occurs, so unrelated TOML is
/// lossless.
///
/// # Errors
///
/// Returns an effective-catalog validation or atomic persistence error.
pub fn write_layer_source(
    data_home: &Path,
    workspace_root: &Path,
    layer: CatalogLayer,
    source: &str,
) -> Result<(), RoleCatalogError> {
    let target = layer_path(data_home, workspace_root, layer);
    let replacement = parse_source(&target, source)?;
    let global_path = layer_path(data_home, workspace_root, CatalogLayer::Global);
    let workspace_path = layer_path(data_home, workspace_root, CatalogLayer::Workspace);
    let global = if layer == CatalogLayer::Global {
        Some(replacement.clone())
    } else {
        read_optional(&global_path)?
    };
    let workspace = if layer == CatalogLayer::Workspace {
        Some(replacement)
    } else {
        read_optional(&workspace_path)?
    };
    let mut effective = builtin_catalog(load_team_template(data_home, workspace_root));
    effective.configured = true;
    if let Some(global) = global {
        if global.defaults.root.is_some() {
            effective.defaults.root = global.defaults.root;
        }
        if global.defaults.session.is_some() {
            effective.defaults.session = global.defaults.session;
        }
        effective.roles.extend(global.roles);
    }
    if let Some(workspace) = workspace {
        if workspace.defaults.root.is_some() {
            effective.defaults.root = workspace.defaults.root;
        }
        if workspace.defaults.session.is_some() {
            effective.defaults.session = workspace.defaults.session;
        }
        effective.roles.extend(workspace.roles);
    }
    validate_default(&effective, RoleScope::Root)?;
    validate_default(&effective, RoleScope::Session)?;
    write_source(&target, source)
}

fn validate_default(
    catalog: &EffectiveRoleCatalog,
    scope: RoleScope,
) -> Result<(), RoleCatalogError> {
    let Some(role) = catalog.default_for(scope) else {
        return Ok(());
    };
    if catalog
        .roles
        .get(role)
        .is_some_and(|definition| definition.scopes.contains(&scope))
    {
        Ok(())
    } else {
        Err(RoleCatalogError::InvalidDefault {
            scope,
            role: role.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    #[test]
    fn layer_editor_preserves_source_and_rejects_invalid_replacements_atomically() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        let source = "# keep me\nversion = 1\n\n[roles.coder]\nsummary = \"Code\"\nscopes = [\"session\"]\ninstructions = \"ship\"\n";
        write_layer_source(&home, &workspace, CatalogLayer::Workspace, source).unwrap();
        assert_eq!(
            read_layer_source(&home, &workspace, CatalogLayer::Workspace).unwrap(),
            source
        );

        let error =
            write_layer_source(&home, &workspace, CatalogLayer::Workspace, "version = 99\n")
                .unwrap_err();
        assert!(matches!(error, RoleCatalogError::UnsupportedVersion { .. }));
        assert_eq!(
            fs::read_to_string(workspace.join(".usagi/roles.toml")).unwrap(),
            source
        );
    }

    #[test]
    fn workspace_editor_validates_defaults_against_the_effective_global_layer() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        write(
            &home.join("roles.toml"),
            "version = 1\n[roles.coder]\nsummary = \"Code\"\nscopes = [\"session\"]\ninstructions = \"ship\"\n",
        );
        write_layer_source(
            &home,
            &workspace,
            CatalogLayer::Workspace,
            "version = 1\n[defaults]\nsession = \"coder\"\n",
        )
        .unwrap();
        assert_eq!(
            load_effective(&home, &workspace)
                .unwrap()
                .defaults
                .session
                .unwrap()
                .as_str(),
            "coder"
        );
    }

    #[test]
    fn source_editor_reports_missing_large_and_io_boundaries() {
        let root = tempdir().unwrap();
        let missing = root.path().join("missing/roles.toml");
        assert_eq!(read_source(&missing).unwrap(), "version = 1\n");

        let large = root.path().join("large.toml");
        let file = fs::File::create(&large).unwrap();
        file.set_len(MAX_CATALOG_BYTES + 1).unwrap();
        assert!(matches!(
            read_source(&large),
            Err(RoleCatalogError::TooLarge(_))
        ));
        assert!(matches!(
            write_source(
                &large,
                &"x".repeat(usize::try_from(MAX_CATALOG_BYTES).unwrap() + 1),
            ),
            Err(RoleCatalogError::TooLarge(_))
        ));

        let directory = root.path().join("directory");
        fs::create_dir(&directory).unwrap();
        assert!(matches!(
            read_source(&directory),
            Err(RoleCatalogError::Io { .. })
        ));

        let blocked_parent = root.path().join("blocked");
        fs::write(&blocked_parent, "file").unwrap();
        assert!(matches!(
            write_source(&blocked_parent.join("roles.toml"), "version = 1\n"),
            Err(RoleCatalogError::Io { .. })
        ));

        let directory_target = root.path().join("target");
        fs::create_dir(&directory_target).unwrap();
        assert!(matches!(
            write_source(&directory_target, "version = 1\n"),
            Err(RoleCatalogError::Io { .. })
        ));

        assert!(matches!(
            write_source(Path::new(""), "version = 1\n"),
            Err(RoleCatalogError::Io { .. })
        ));
    }

    #[test]
    fn global_editor_validates_with_workspace_overrides_and_preserves_them() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        write(
            &workspace.join(".usagi/roles.toml"),
            "version = 1\n[defaults]\nroot = \"lead\"\n[roles.lead]\nsummary = \"Lead\"\nscopes = [\"root\"]\ninstructions = \"lead\"\n[roles.review]\nsummary = \"Review\"\nscopes = [\"session\"]\ninstructions = \"review\"\n",
        );
        write_layer_source(
            &home,
            &workspace,
            CatalogLayer::Global,
            "version = 1\n[defaults]\nroot = \"director\"\nsession = \"coder\"\n[roles.director]\nsummary = \"Director\"\nscopes = [\"root\"]\ninstructions = \"direct\"\n[roles.coder]\nsummary = \"Code\"\nscopes = [\"session\"]\ninstructions = \"code\"\n",
        )
        .unwrap();

        let catalog = load_effective(&home, &workspace).unwrap();
        assert_eq!(catalog.defaults.root.unwrap().as_str(), "lead");
        assert_eq!(catalog.defaults.session.unwrap().as_str(), "coder");
        assert!(catalog.roles.contains_key(&RoleId::new("review").unwrap()));
    }

    #[test]
    fn global_editor_accepts_a_missing_workspace_layer() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        let source = "version = 1\n[roles.coder]\nsummary = \"Code\"\nscopes = [\"session\"]\ninstructions = \"code\"\n";

        write_layer_source(&home, &workspace, CatalogLayer::Global, source).unwrap();

        assert_eq!(
            read_layer_source(&home, &workspace, CatalogLayer::Global).unwrap(),
            source
        );
    }

    #[test]
    fn missing_files_preserve_legacy_mode() {
        let root = tempdir().unwrap();
        let catalog =
            load_effective(&root.path().join("home"), &root.path().join("workspace")).unwrap();
        assert!(!catalog.configured);
        assert!(catalog.roles.is_empty());
    }

    #[test]
    fn built_in_templates_define_distinct_defaults_and_enforced_routes() {
        let none = builtin_catalog(TeamTemplate::None);
        assert!(none.roles.is_empty());

        let hierarchical = builtin_catalog(TeamTemplate::Hierarchical);
        assert_eq!(hierarchical.defaults.session.unwrap().as_str(), "manager");
        assert_eq!(hierarchical.roles.len(), 3);
        let director = &hierarchical.roles[&role_id("director")];
        assert_eq!(
            director
                .delegation
                .as_ref()
                .unwrap()
                .child_roles
                .iter()
                .map(RoleId::as_str)
                .collect::<Vec<_>>(),
            ["manager", "worker"]
        );

        let flat = builtin_catalog(TeamTemplate::Flat);
        assert_eq!(flat.defaults.session.unwrap().as_str(), "worker");
        assert_eq!(flat.roles.len(), 2);
        assert_eq!(
            flat.roles[&role_id("director")]
                .delegation
                .as_ref()
                .unwrap()
                .max_depth,
            1
        );

        let pipeline = builtin_catalog(TeamTemplate::Pipeline);
        assert_eq!(pipeline.defaults.session.unwrap().as_str(), "planner");
        assert_eq!(pipeline.roles.len(), 4);
        assert!(
            pipeline.roles[&role_id("implementer")]
                .delegation
                .as_ref()
                .unwrap()
                .child_roles
                .contains(&role_id("tester"))
        );
        assert!(
            !pipeline.roles[&role_id("tester")]
                .delegation
                .as_ref()
                .unwrap()
                .enabled
        );
    }

    #[test]
    fn config_selects_a_template_and_workspace_settings_override_global() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        Storage::new(&home)
            .save_settings(&crate::domain::settings::Settings {
                team_template: TeamTemplate::Hierarchical,
                ..crate::domain::settings::Settings::default()
            })
            .unwrap();
        write(
            &home.join("roles.toml"),
            "version = 1\n[roles.auditor]\nsummary = \"Custom audit\"\nscopes = [\"session\"]\ninstructions = \"audit\"\n",
        );

        let hierarchical = load_effective(&home, &workspace).unwrap();
        assert!(hierarchical.configured);
        assert_eq!(hierarchical.defaults.session.unwrap().as_str(), "manager");
        assert!(hierarchical.roles.contains_key(&role_id("manager")));
        assert!(hierarchical.roles.contains_key(&role_id("auditor")));

        WorkspaceSettingsStore::new(&workspace)
            .save(&crate::domain::settings::LocalSettings {
                team_template: Some(TeamTemplate::Flat),
                ..crate::domain::settings::LocalSettings::default()
            })
            .unwrap();
        write(
            &workspace.join(".usagi/roles.toml"),
            "version = 1\n[roles.worker]\nsummary = \"Custom worker\"\nscopes = [\"session\"]\ninstructions = \"custom\"\n",
        );
        let flat = load_effective(&home, &workspace).unwrap();
        assert_eq!(flat.defaults.session.unwrap().as_str(), "worker");
        assert_eq!(flat.roles[&role_id("worker")].summary, "Custom worker");
        assert!(!flat.roles.contains_key(&role_id("manager")));
    }

    #[test]
    fn damaged_settings_fail_closed_without_granting_template_roles() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join("settings.json"), "{ broken").unwrap();
        assert!(load_effective(&home, &workspace).unwrap().roles.is_empty());

        Storage::new(&home)
            .save_settings(&crate::domain::settings::Settings {
                team_template: TeamTemplate::Pipeline,
                ..crate::domain::settings::Settings::default()
            })
            .unwrap();
        let local = WorkspaceSettingsStore::new(&workspace);
        fs::create_dir_all(local.path().parent().unwrap()).unwrap();
        fs::write(local.path(), "{ broken").unwrap();
        let catalog = load_effective(&home, &workspace).unwrap();
        assert!(!catalog.configured);
        assert!(catalog.roles.is_empty());
    }

    #[test]
    fn workspace_replaces_whole_definition_and_only_present_defaults() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        write(
            &home.join("roles.toml"),
            r#"version = 1
[defaults]
root = "director"
session = "coder"
[roles.director]
summary = "global director"
scopes = ["root"]
instructions = "direct"
[roles.coder]
summary = "global coder"
scopes = ["session"]
instructions = "code"
"#,
        );
        write(
            &workspace.join(".usagi/roles.toml"),
            r#"version = 1
[defaults]
root = "workspace-director"
session = "reviewer"
[roles.workspace-director]
summary = "workspace director"
scopes = ["root"]
instructions = "direct workspace"
[roles.coder]
summary = "workspace coder"
scopes = ["session"]
instructions = "workspace code"
[roles.reviewer]
summary = "review"
scopes = ["session"]
instructions = "review"
"#,
        );
        let catalog = load_effective(&home, &workspace).unwrap();
        assert_eq!(
            catalog.defaults.root.unwrap().as_str(),
            "workspace-director"
        );
        assert_eq!(catalog.defaults.session.unwrap().as_str(), "reviewer");
        assert_eq!(
            catalog.roles[&RoleId::new("coder").unwrap()].summary,
            "workspace coder"
        );
    }

    #[test]
    fn malformed_future_and_invalid_policy_fail_closed() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        write(&home.join("roles.toml"), "version = 2\n");
        assert!(matches!(
            load_effective(&home, root.path()),
            Err(RoleCatalogError::UnsupportedVersion { .. })
        ));
        write(&home.join("roles.toml"), "version = [\n");
        assert!(matches!(
            load_effective(&home, root.path()),
            Err(RoleCatalogError::Malformed { .. })
        ));
        write(
            &home.join("roles.toml"),
            "version = 1\n[defaults]\nsession = \"missing\"\n",
        );
        assert!(matches!(
            load_effective(&home, root.path()),
            Err(RoleCatalogError::InvalidDefault { .. })
        ));
        write(
            &home.join("roles.toml"),
            "version = 1\n[roles.coder]\nsummary = \"x\"\nscopes = []\ninstructions = \"x\"\n",
        );
        assert!(matches!(
            load_effective(&home, root.path()),
            Err(RoleCatalogError::EmptyScopes { .. })
        ));
        let nul = "version = 1\n[roles.coder]\nsummary = \"x\"\nscopes = [\"session\"]\ninstructions = \"x\\u0000y\"\n";
        write(&home.join("roles.toml"), nul);
        assert!(matches!(
            load_effective(&home, root.path()),
            Err(RoleCatalogError::InvalidInstructions { .. })
        ));

        let oversized = format!(
            "version = 1\n[roles.coder]\nsummary = \"x\"\nscopes = [\"session\"]\ninstructions = \"{}\"\n",
            "x".repeat(MAX_ROLE_INSTRUCTIONS_BYTES + 1)
        );
        write(&home.join("roles.toml"), &oversized);
        assert!(matches!(
            load_effective(&home, root.path()),
            Err(RoleCatalogError::InvalidInstructions { .. })
        ));

        write(
            &home.join("roles.toml"),
            "version = 1\n[roles.coder]\nsummary = \"x\"\nscopes = [\"session\"]\ninstructions = \"x\"\nextra = true\n",
        );
        assert!(matches!(
            load_effective(&home, root.path()),
            Err(RoleCatalogError::Malformed { .. })
        ));
    }

    #[test]
    fn catalog_size_and_io_failures_are_distinct() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let large = fs::File::create(home.join("roles.toml")).unwrap();
        large.set_len(MAX_CATALOG_BYTES + 1).unwrap();
        assert!(matches!(
            load_effective(&home, root.path()),
            Err(RoleCatalogError::TooLarge(_))
        ));

        fs::remove_file(home.join("roles.toml")).unwrap();
        fs::create_dir(home.join("roles.toml")).unwrap();
        assert!(matches!(
            load_effective(&home, root.path()),
            Err(RoleCatalogError::Io { .. })
        ));

        let blocked_home = root.path().join("not-a-directory");
        fs::write(&blocked_home, "file").unwrap();
        assert!(matches!(
            load_effective(&blocked_home, root.path()),
            Err(RoleCatalogError::Io { .. })
        ));
    }

    #[test]
    fn catalog_errors_have_safe_diagnostic_categories() {
        let path = PathBuf::from("roles.toml");
        let role = RoleId::new("coder").unwrap();
        let malformed = toml::from_str::<RoleCatalogFile>("version = [").unwrap_err();
        let errors = [
            RoleCatalogError::Io {
                path: path.clone(),
                source: std::io::Error::other("denied"),
            },
            RoleCatalogError::TooLarge(path.clone()),
            RoleCatalogError::Malformed {
                path: path.clone(),
                source: malformed,
            },
            RoleCatalogError::UnsupportedVersion {
                path: path.clone(),
                version: 2,
            },
            RoleCatalogError::EmptyScopes {
                path: path.clone(),
                role: role.clone(),
            },
            RoleCatalogError::InvalidInstructions {
                path,
                role: role.clone(),
            },
            RoleCatalogError::InvalidDefault {
                scope: RoleScope::Session,
                role,
            },
        ];
        for error in errors {
            assert!(!error.to_string().is_empty());
        }
    }
}
