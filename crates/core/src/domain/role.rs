//! Session-role vocabulary and deterministic effective-catalog model.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

/// Maximum UTF-8 byte length of one role instruction.
pub const MAX_ROLE_INSTRUCTIONS_BYTES: usize = 16 * 1024;

/// Stable, human-readable role identity.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct RoleId(String);

impl fmt::Debug for RoleId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("RoleId").field(&self.0).finish()
    }
}

impl fmt::Display for RoleId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for RoleId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

impl RoleId {
    /// Parses lowercase ASCII kebab-case in the 1..=64 byte range.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidRoleId`] when the value is empty, too long, or is not
    /// canonical lowercase ASCII kebab-case.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidRoleId> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            && !value.starts_with('-')
            && !value.ends_with('-')
            && !value.contains("--");
        valid.then_some(Self(value)).ok_or(InvalidRoleId)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidRoleId;

impl fmt::Display for InvalidRoleId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("role id must be lowercase ASCII kebab-case and 1-64 bytes")
    }
}

impl std::error::Error for InvalidRoleId {}

/// Launch scope in which a role definition may be selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleScope {
    Root,
    Session,
}

/// One complete role definition. Workspace definitions replace global ones as
/// a whole; individual fields are never inherited.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleDefinition {
    pub summary: String,
    pub scopes: BTreeSet<RoleScope>,
    pub instructions: String,
    /// Optional daemon-enforced delegation authority. Absence preserves the
    /// version-1 catalog's legacy behavior; once present every limit is
    /// enforced before a session or worker is created.
    #[serde(default)]
    pub delegation: Option<DelegationPolicy>,
}

/// Authority a role may exercise over child sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationPolicy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub child_roles: BTreeSet<RoleId>,
    #[serde(default = "default_max_delegation_depth")]
    pub max_depth: usize,
    #[serde(default = "default_max_delegation_concurrency")]
    pub max_concurrency: usize,
}

const fn default_max_delegation_depth() -> usize {
    8
}

const fn default_max_delegation_concurrency() -> usize {
    4
}

/// Optional defaults supplied by one catalog layer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleDefaults {
    #[serde(default)]
    pub root: Option<RoleId>,
    #[serde(default)]
    pub session: Option<RoleId>,
}

/// Fully merged role policy. `configured == false` preserves the legacy
/// generic prompt behavior when neither catalog file exists.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EffectiveRoleCatalog {
    pub configured: bool,
    pub defaults: RoleDefaults,
    pub roles: BTreeMap<RoleId, RoleDefinition>,
}

impl EffectiveRoleCatalog {
    #[must_use]
    pub fn default_for(&self, scope: RoleScope) -> Option<&RoleId> {
        match scope {
            RoleScope::Root => self.defaults.root.as_ref(),
            RoleScope::Session => self.defaults.session.as_ref(),
        }
    }

    /// Resolves an explicit selector or effective default and validates scope.
    ///
    /// # Errors
    ///
    /// Returns [`RoleResolutionError`] when the selected role is absent from
    /// the effective catalog or does not allow the requested scope.
    pub fn resolve(
        &self,
        requested: Option<&RoleId>,
        scope: RoleScope,
    ) -> Result<Option<RoleId>, RoleResolutionError> {
        let selected = requested.or_else(|| self.default_for(scope));
        let Some(selected) = selected else {
            return Ok(None);
        };
        let definition = self
            .roles
            .get(selected)
            .ok_or_else(|| RoleResolutionError::Unknown(selected.clone()))?;
        if !definition.scopes.contains(&scope) {
            return Err(RoleResolutionError::ScopeMismatch {
                role: selected.clone(),
                scope,
            });
        }
        Ok(Some(selected.clone()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoleResolutionError {
    Unknown(RoleId),
    ScopeMismatch { role: RoleId, scope: RoleScope },
}

impl fmt::Display for RoleResolutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(role) => write!(formatter, "unknown role \"{role}\""),
            Self::ScopeMismatch { role, scope } => {
                write!(
                    formatter,
                    "role \"{role}\" is not valid for {scope:?} scope"
                )
            }
        }
    }
}

impl std::error::Error for RoleResolutionError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_id_accepts_only_canonical_kebab_case() {
        for value in ["coder", "review-2", "a"] {
            assert_eq!(RoleId::new(value).unwrap().as_str(), value);
        }
        for value in ["", "Coder", "review_2", "-coder", "coder-", "co--der"] {
            assert!(RoleId::new(value).is_err(), "{value}");
        }
        assert!(RoleId::new("a".repeat(65)).is_err());
        assert!(serde_json::from_str::<RoleId>("\"Bad\"").is_err());
        assert_eq!(
            serde_json::to_string(&RoleId::new("coder").unwrap()).unwrap(),
            "\"coder\""
        );
        let coder = RoleId::new("coder").unwrap();
        assert_eq!(coder.to_string(), "coder");
        assert_eq!(format!("{coder:?}"), "RoleId(\"coder\")");
        assert_eq!(
            InvalidRoleId.to_string(),
            "role id must be lowercase ASCII kebab-case and 1-64 bytes"
        );
        let scope: RoleScope = serde_json::from_str("\"root\"").unwrap();
        assert_eq!(scope, RoleScope::Root);
    }

    #[test]
    fn effective_catalog_resolves_defaults_and_scope_without_fallback() {
        let coder = RoleId::new("coder").unwrap();
        let reviewer = RoleId::new("reviewer").unwrap();
        let mut catalog = EffectiveRoleCatalog {
            configured: true,
            defaults: RoleDefaults {
                root: None,
                session: Some(coder.clone()),
            },
            roles: BTreeMap::new(),
        };
        catalog.roles.insert(
            coder.clone(),
            RoleDefinition {
                summary: "code".into(),
                scopes: BTreeSet::from([RoleScope::Session]),
                instructions: "implement".into(),
                delegation: None,
            },
        );
        assert_eq!(
            catalog.resolve(None, RoleScope::Session).unwrap(),
            Some(coder)
        );
        assert!(matches!(catalog.resolve(None, RoleScope::Root), Ok(None)));
        assert!(matches!(
            catalog.resolve(Some(&reviewer), RoleScope::Session),
            Err(RoleResolutionError::Unknown(_))
        ));
        assert_eq!(
            catalog
                .resolve(Some(&reviewer), RoleScope::Session)
                .unwrap_err()
                .to_string(),
            "unknown role \"reviewer\""
        );
        let coder = RoleId::new("coder").unwrap();
        assert!(matches!(
            catalog.resolve(Some(&coder), RoleScope::Root),
            Err(RoleResolutionError::ScopeMismatch { .. })
        ));
        assert!(
            catalog
                .resolve(Some(&coder), RoleScope::Root)
                .unwrap_err()
                .to_string()
                .contains("Root scope")
        );
    }

    #[test]
    fn delegation_policy_is_explicit_and_bounded() {
        let definition: RoleDefinition = toml::from_str(
            r#"
summary = "Manage"
scopes = ["session"]
instructions = "delegate"
[delegation]
enabled = true
child_roles = ["executor"]
max_depth = 3
max_concurrency = 2
"#,
        )
        .unwrap();
        let policy = definition.delegation.unwrap();
        assert!(policy.enabled);
        assert!(
            policy
                .child_roles
                .contains(&RoleId::new("executor").unwrap())
        );
        assert_eq!(policy.max_depth, 3);
        assert_eq!(policy.max_concurrency, 2);

        let legacy: RoleDefinition = toml::from_str(
            "summary = \"Work\"\nscopes = [\"session\"]\ninstructions = \"execute\"\n",
        )
        .unwrap();
        assert!(legacy.delegation.is_none());
    }

    #[test]
    fn delegation_policy_uses_safe_defaults_when_limits_are_omitted() {
        let definition: RoleDefinition = toml::from_str(
            r#"
summary = "Manage"
scopes = ["session"]
instructions = "delegate"
[delegation]
enabled = true
"#,
        )
        .unwrap();

        let policy = definition.delegation.unwrap();
        assert_eq!(policy.max_depth, 8);
        assert_eq!(policy.max_concurrency, 4);
    }
}
