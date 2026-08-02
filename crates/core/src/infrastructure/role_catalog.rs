//! Versioned global/workspace role-catalog reader.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::domain::role::{
    EffectiveRoleCatalog, MAX_ROLE_INSTRUCTIONS_BYTES, RoleDefaults, RoleDefinition, RoleId,
    RoleScope,
};

const CATALOG_VERSION: u16 = 1;
const MAX_CATALOG_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Deserialize)]
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
    let configured = global.is_some() || workspace.is_some();
    let mut effective = EffectiveRoleCatalog {
        configured,
        ..EffectiveRoleCatalog::default()
    };
    if let Some(global) = global {
        effective.defaults = global.defaults;
        effective.roles = global.roles;
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
    let catalog: RoleCatalogFile =
        toml::from_str(&source).map_err(|source| RoleCatalogError::Malformed {
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
    Ok(Some(catalog))
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
    fn missing_files_preserve_legacy_mode() {
        let root = tempdir().unwrap();
        let catalog =
            load_effective(&root.path().join("home"), &root.path().join("workspace")).unwrap();
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
