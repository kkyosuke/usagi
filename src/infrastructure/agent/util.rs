//! Shared helpers for the agent adapters.
//!
//! Several adapters render their launch command into a `sh -c` line and locate a
//! worktree's prior session by comparing directory paths. The shell-quoting and
//! path-comparison idioms are identical across them, so they live here once
//! rather than being copied per adapter.

use crate::domain::agent::AgentWiring;
use std::path::Path;

/// Wrap `text` as a single shell argument in single quotes, safe to drop into a
/// `sh -c` command line. A single quote cannot appear inside a single-quoted
/// string, so each one is rendered as `'\''` (close the quote, an escaped quote,
/// reopen) — the standard POSIX idiom. Everything else (newlines, `$`, spaces,
/// the `[`, `]`, `"` of a TOML value …) is literal inside single quotes, so the
/// agent receives the argument verbatim.
pub(super) fn shell_single_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// Whether two paths name the same directory, comparing canonicalized forms (so a
/// symlinked or `/tmp` ⇄ `/private/tmp` difference still matches) and falling back
/// to a plain comparison when a path cannot be canonicalized (e.g. the recorded
/// directory no longer exists).
pub(super) fn same_dir(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(x), Ok(y)) if x == y
        )
}

/// Write or merge usagi's MCP server configuration into the JSON file at `path`.
pub(super) fn update_mcp_config(path: &Path, wiring: &AgentWiring) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return Err(format!("failed to create directories for MCP config: {e}"));
        }
    }

    let mut config: serde_json::Value = if path.exists() {
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(e) => return Err(format!("failed to read MCP config: {e}")),
        };
        serde_json::from_str(&contents).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    if !config.is_object() {
        config = serde_json::json!({});
    }

    let mcp_servers = config
        .as_object_mut()
        .unwrap()
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));

    if !mcp_servers.is_object() {
        *mcp_servers = serde_json::json!({});
    }

    let servers_obj = mcp_servers
        .as_object_mut()
        .expect("mcpServers is forced to an object above");
    servers_obj.insert(
        "usagi".to_string(),
        serde_json::json!({
            "command": wiring.usagi_bin,
            "args": ["mcp"]
        }),
    );

    if let Some(ref model) = wiring.local_llm_model {
        servers_obj.insert(
            "usagi-llm".to_string(),
            serde_json::json!({
                "command": wiring.usagi_bin,
                "args": ["llm-mcp", "--model", model]
            }),
        );
    } else {
        servers_obj.remove("usagi-llm");
    }

    let serialized =
        serde_json::to_string_pretty(&config).expect("serializing a serde_json::Value cannot fail");
    if let Err(e) = std::fs::write(path, serialized) {
        return Err(format!("failed to write MCP config: {e}"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_single_quote_wraps_and_escapes() {
        assert_eq!(shell_single_quote("plain"), "'plain'");
        // An embedded single quote closes, escapes, and reopens the quoting.
        assert_eq!(shell_single_quote("a'b"), r"'a'\''b'");
        // Shell metacharacters stay literal inside single quotes.
        assert_eq!(shell_single_quote("$x `y` \"z\""), "'$x `y` \"z\"'");
    }

    #[test]
    fn same_dir_compares_raw_then_canonical() {
        // Identical paths match outright (the raw short-circuit).
        assert!(same_dir(Path::new("/a/b"), Path::new("/a/b")));

        let dir = tempfile::tempdir().unwrap();
        let real = dir.path();
        // Raw-different but canonically-equal paths match via canonicalization. A
        // `sub/..` round-trip stays distinct as a `Path` (unlike a trailing `.`,
        // which `Path` normalizes away) yet canonicalizes back to `real`.
        std::fs::create_dir_all(real.join("sub")).unwrap();
        let round_trip = real.join("sub").join("..");
        assert_ne!(real, round_trip.as_path());
        assert!(same_dir(real, &round_trip));

        // Two distinct real directories canonicalize to different paths → no match
        // (both canonicalize, the guard is evaluated and fails).
        let other = tempfile::tempdir().unwrap();
        assert!(!same_dir(real, other.path()));

        // A path that cannot be canonicalized (does not exist) and is raw-different
        // also does not match.
        assert!(!same_dir(real, Path::new("/nonexistent/xyz")));
    }

    #[test]
    fn test_update_mcp_config_creates_new_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config_path = temp_dir.path().join("config.json");
        let wiring = AgentWiring {
            usagi_bin: "/bin/usagi".to_string(),
            local_llm_model: None,
            model: None,
        };

        update_mcp_config(&config_path, &wiring).unwrap();

        assert!(config_path.exists());
        let contents = std::fs::read_to_string(&config_path).unwrap();
        let val: serde_json::Value = serde_json::from_str(&contents).unwrap();

        assert_eq!(val["mcpServers"]["usagi"]["command"], "/bin/usagi");
        assert_eq!(
            val["mcpServers"]["usagi"]["args"],
            serde_json::json!(["mcp"])
        );
        assert!(val["mcpServers"]["usagi-llm"].is_null());
    }

    #[test]
    fn test_update_mcp_config_merges_existing_and_includes_llm() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config_path = temp_dir.path().join("subdir").join("config.json");

        // Write existing file with other mcpServers
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        let existing = serde_json::json!({
            "otherField": "value",
            "mcpServers": {
                "existing-server": {
                    "command": "node",
                    "args": ["server.js"]
                },
                "usagi-llm": {
                    "command": "old-bin",
                    "args": ["llm-mcp"]
                }
            }
        });
        std::fs::write(&config_path, serde_json::to_string(&existing).unwrap()).unwrap();

        let wiring = AgentWiring {
            usagi_bin: "/bin/usagi".to_string(),
            local_llm_model: Some("qwen2.5-coder".to_string()),
            model: None,
        };

        update_mcp_config(&config_path, &wiring).unwrap();

        let contents = std::fs::read_to_string(&config_path).unwrap();
        let val: serde_json::Value = serde_json::from_str(&contents).unwrap();

        // Check merged fields
        assert_eq!(val["otherField"], "value");
        assert_eq!(val["mcpServers"]["existing-server"]["command"], "node");
        assert_eq!(val["mcpServers"]["usagi"]["command"], "/bin/usagi");
        assert_eq!(val["mcpServers"]["usagi-llm"]["command"], "/bin/usagi");
        assert_eq!(
            val["mcpServers"]["usagi-llm"]["args"],
            serde_json::json!(["llm-mcp", "--model", "qwen2.5-coder"])
        );
    }

    #[test]
    fn test_update_mcp_config_handles_corrupt_file_and_removes_llm() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config_path = temp_dir.path().join("config.json");

        // Write invalid json
        std::fs::write(&config_path, "invalid-json").unwrap();

        let wiring = AgentWiring {
            usagi_bin: "/bin/usagi".to_string(),
            local_llm_model: None,
            model: None,
        };

        update_mcp_config(&config_path, &wiring).unwrap();

        let contents = std::fs::read_to_string(&config_path).unwrap();
        let val: serde_json::Value = serde_json::from_str(&contents).unwrap();

        assert_eq!(val["mcpServers"]["usagi"]["command"], "/bin/usagi");
        assert!(val["mcpServers"]["usagi-llm"].is_null());
    }

    #[test]
    fn test_update_mcp_config_replaces_non_object_roots() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config_path = temp_dir.path().join("config.json");
        std::fs::write(&config_path, "[]").unwrap();
        let wiring = AgentWiring {
            usagi_bin: "/bin/usagi".to_string(),
            local_llm_model: None,
            model: None,
        };

        update_mcp_config(&config_path, &wiring).unwrap();

        let val: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(val["mcpServers"]["usagi"]["command"], "/bin/usagi");
    }

    #[test]
    fn test_update_mcp_config_replaces_non_object_mcp_servers() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config_path = temp_dir.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::json!({"mcpServers": "not an object"}).to_string(),
        )
        .unwrap();
        let wiring = AgentWiring {
            usagi_bin: "/bin/usagi".to_string(),
            local_llm_model: None,
            model: None,
        };

        update_mcp_config(&config_path, &wiring).unwrap();

        let val: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert!(val["mcpServers"].is_object());
        assert_eq!(
            val["mcpServers"]["usagi"]["args"],
            serde_json::json!(["mcp"])
        );
    }

    #[test]
    fn test_update_mcp_config_reports_parent_creation_errors() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_parent = temp_dir.path().join("not-a-dir");
        std::fs::write(&file_parent, "x").unwrap();
        let config_path = file_parent.join("config.json");
        let wiring = AgentWiring {
            usagi_bin: "/bin/usagi".to_string(),
            local_llm_model: None,
            model: None,
        };

        let err = update_mcp_config(&config_path, &wiring).unwrap_err();
        assert!(err.contains("failed to create directories"), "{err}");
    }

    #[test]
    fn test_update_mcp_config_reports_read_errors() {
        let temp_dir = tempfile::tempdir().unwrap();
        let wiring = AgentWiring {
            usagi_bin: "/bin/usagi".to_string(),
            local_llm_model: None,
            model: None,
        };

        let err = update_mcp_config(temp_dir.path(), &wiring).unwrap_err();
        assert!(err.contains("failed to read MCP config"), "{err}");
    }

    #[test]
    fn test_update_mcp_config_reports_write_errors_without_a_parent() {
        let wiring = AgentWiring {
            usagi_bin: "/bin/usagi".to_string(),
            local_llm_model: None,
            model: None,
        };

        let err = update_mcp_config(Path::new(""), &wiring).unwrap_err();
        assert!(err.contains("failed to write MCP config"), "{err}");
    }
}
