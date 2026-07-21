//! MCP tool アダプタの置き場。tool は系統ごとにファイルを分け（`issue` / `memory` /
//! `session`）、各 tool が 1 struct として `Tool` を実装する。registry は metadata、schema
//! validator、execution route、caller policy を 1 つの `ToolDescriptor` に束ねる。
//!
//! 各アダプタは presentation に徹する — store 系は usagi-core の usecase を直接呼び、
//! session 系は usagi-core の IPC クライアント経由で daemon に委譲し、結果を JSON に
//! 整形する（独自のビジネスロジックは持たない）。CLI のコマンドハンドラ
//! （`crate::cli::commands`）は同じ core usecase を呼ぶ兄弟である。

pub mod issue;
pub mod memory;
pub mod session;
pub mod supervisor;

use std::collections::HashSet;
use std::fmt;

use usagi_core::usecase::client::{DispatchToolAction, SessionAction, SupervisorToolAction};

use super::tool::{CallerPolicy, Tool, ToolDescriptor, ToolRoute, validate_schema_definition};

/// 公開する全 MCP tool のレジストリ（issue / memory / session を連結）。
///
/// # Panics
///
/// Panics before the MCP serve loop starts when descriptor validation fails.
#[must_use]
pub fn registry() -> Vec<ToolDescriptor> {
    let mut tools = issue::tools();
    tools.extend(memory::tools());
    tools.extend(session::tools());
    tools.extend(supervisor::tools());
    let descriptors = tools.into_iter().map(descriptor).collect::<Vec<_>>();
    validate_registry(&descriptors).expect("invalid MCP tool descriptor registry");
    descriptors
}

fn descriptor(tool: Box<dyn Tool>) -> ToolDescriptor {
    use CallerPolicy::{AgentCredential, DaemonProvenance, Public, SessionCredential};
    use DispatchToolAction as Dispatch;
    use SessionAction as Session;
    use SupervisorToolAction as Supervisor;
    use ToolRoute::{
        Dispatch as DispatchRoute, Session as SessionRoute, Store, Supervisor as SupervisorRoute,
    };

    let (route, policy) = match tool.name() {
        name if name.starts_with("issue_") || name.starts_with("memory_") => (Store, Public),
        "session_create" => (SessionRoute(Session::Create), Public),
        "session_list" => (SessionRoute(Session::List), Public),
        "session_status" => (SessionRoute(Session::Status), Public),
        "session_complete" => (SessionRoute(Session::Complete), SessionCredential),
        "session_pr" => (SessionRoute(Session::Pr), Public),
        "session_remove" => (SessionRoute(Session::Remove), Public),
        "session_resume" => (SessionRoute(Session::ResumeAgent), Public),
        "session_recover_legacy" => (SessionRoute(Session::RecoverLegacy), Public),
        "session_prompt" => (SessionRoute(Session::Prompt), Public),
        "session_note_get" => (SessionRoute(Session::NoteGet), SessionCredential),
        "session_note_update" => (SessionRoute(Session::NoteUpdate), SessionCredential),
        "session_todo_list" => (SessionRoute(Session::TodoList), SessionCredential),
        "session_todo_add" => (SessionRoute(Session::TodoAdd), SessionCredential),
        "session_todo_update" => (SessionRoute(Session::TodoUpdate), SessionCredential),
        "session_todo_remove" => (SessionRoute(Session::TodoRemove), SessionCredential),
        "session_decision_list" => (SessionRoute(Session::DecisionList), SessionCredential),
        "session_decision_log" => (SessionRoute(Session::DecisionLog), SessionCredential),
        "session_delegate_issue" => (SessionRoute(Session::DelegateIssue), Public),
        "session_delegate_brief" => (SessionRoute(Session::DelegateBrief), SessionCredential),
        "session_dispatch" => (DispatchRoute(Dispatch::Dispatch), AgentCredential),
        "session_get" => (DispatchRoute(Dispatch::SessionGet), AgentCredential),
        "agent_list" => (DispatchRoute(Dispatch::AgentList), AgentCredential),
        "agent_get" => (DispatchRoute(Dispatch::AgentGet), AgentCredential),
        "agent_complete" => (DispatchRoute(Dispatch::AgentComplete), AgentCredential),
        "agent_fail" => (DispatchRoute(Dispatch::AgentFail), AgentCredential),
        "agent_inbox" => (DispatchRoute(Dispatch::AgentInbox), AgentCredential),
        "user_decision_request" => (
            DispatchRoute(Dispatch::UserDecisionRequest),
            AgentCredential,
        ),
        "user_decision_get" => (DispatchRoute(Dispatch::UserDecisionGet), AgentCredential),
        "user_decision_list" => (DispatchRoute(Dispatch::UserDecisionList), AgentCredential),
        "user_decision_resolve" => (
            DispatchRoute(Dispatch::UserDecisionResolve),
            AgentCredential,
        ),
        "user_decision_cancel" => (DispatchRoute(Dispatch::UserDecisionCancel), AgentCredential),
        "user_decision_expire" => (DispatchRoute(Dispatch::UserDecisionExpire), AgentCredential),
        "supervisor_start" => (SupervisorRoute(Supervisor::Start), DaemonProvenance),
        "supervisor_get" => (SupervisorRoute(Supervisor::Get), DaemonProvenance),
        "supervisor_list" => (SupervisorRoute(Supervisor::List), DaemonProvenance),
        "supervisor_cancel" => (SupervisorRoute(Supervisor::Cancel), DaemonProvenance),
        "supervisor_resolve_escalation" => (
            SupervisorRoute(Supervisor::ResolveEscalation),
            DaemonProvenance,
        ),
        "supervisor_events" => (SupervisorRoute(Supervisor::Events), DaemonProvenance),
        name => panic!("tool {name} has no descriptor route"),
    };
    ToolDescriptor::new(tool, route, policy)
}

#[derive(Debug, Eq, PartialEq)]
pub struct RegistryError(String);

impl fmt::Display for RegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Rejects descriptor states that could advertise an ambiguous or non-executable tool.
///
/// # Errors
///
/// Returns a registry error for duplicate or unavailable routes, caller-policy drift,
/// or schema vocabulary the runtime validator cannot enforce.
pub fn validate_registry(descriptors: &[ToolDescriptor]) -> Result<(), RegistryError> {
    let mut names = HashSet::new();
    let mut routes = HashSet::new();
    for descriptor in descriptors {
        if !descriptor.is_advertised() {
            return Err(RegistryError(format!(
                "route for {} is not advertised",
                descriptor.name()
            )));
        }
        if !names.insert(descriptor.name()) {
            return Err(RegistryError(format!(
                "duplicate tool name: {}",
                descriptor.name()
            )));
        }
        if let ToolRoute::Unavailable(reason) = descriptor.route() {
            return Err(RegistryError(format!(
                "advertised tool {} is unavailable: {reason}",
                descriptor.name()
            )));
        }
        if !matches!(
            (descriptor.route(), descriptor.caller_policy()),
            (
                ToolRoute::Store | ToolRoute::Session(_),
                CallerPolicy::Public
            ) | (ToolRoute::Session(_), CallerPolicy::SessionCredential)
                | (ToolRoute::Dispatch(_), CallerPolicy::AgentCredential)
                | (ToolRoute::Supervisor(_), CallerPolicy::DaemonProvenance)
        ) {
            return Err(RegistryError(format!(
                "route and caller policy mismatch for {}",
                descriptor.name()
            )));
        }
        let route = match descriptor.route() {
            ToolRoute::Store => format!("store:{}", descriptor.name()),
            route => format!("{route:?}"),
        };
        if !routes.insert(route.clone()) {
            return Err(RegistryError(format!(
                "duplicate executable route: {route}"
            )));
        }
        let schema: serde_json::Value =
            serde_json::from_str(descriptor.input_schema()).map_err(|error| {
                RegistryError(format!("invalid schema for {}: {error}", descriptor.name()))
            })?;
        validate_schema_definition(&schema).map_err(|error| {
            RegistryError(format!("invalid schema for {}: {error}", descriptor.name()))
        })?;
        if schema.get("type") != Some(&serde_json::Value::String("object".into()))
            || schema.get("properties").is_none()
        {
            return Err(RegistryError(format!(
                "schema for {} must describe an object with properties",
                descriptor.name()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{descriptor, registry, validate_registry};
    use crate::mcp::tool::{CallerPolicy, Tool, ToolDescriptor, ToolError, ToolRoute};
    use usagi_core::usecase::client::SessionAction;

    struct FixtureTool(&'static str);
    impl Tool for FixtureTool {
        fn name(&self) -> &'static str {
            self.0
        }
        fn description(&self) -> &'static str {
            "fixture"
        }
        fn input_schema(&self) -> &'static str {
            r#"{"type":"object","properties":{},"additionalProperties":false}"#
        }
    }

    struct UnsupportedSchema;
    impl Tool for UnsupportedSchema {
        fn name(&self) -> &'static str {
            "unsupported_schema"
        }
        fn description(&self) -> &'static str {
            "fixture"
        }
        fn input_schema(&self) -> &'static str {
            r#"{"type":"object","properties":{"name":{"type":"string","pattern":"x"}}}"#
        }
    }

    struct SchemaFixture(&'static str, &'static str);
    impl Tool for SchemaFixture {
        fn name(&self) -> &'static str {
            self.0
        }
        fn description(&self) -> &'static str {
            "fixture"
        }
        fn input_schema(&self) -> &'static str {
            self.1
        }
    }

    fn valid_value(schema: &serde_json::Value) -> serde_json::Value {
        if let Some(value) = schema.get("const") {
            return value.clone();
        }
        if let Some(value) = schema
            .get("enum")
            .and_then(serde_json::Value::as_array)
            .and_then(|values| values.first())
        {
            return value.clone();
        }
        if let Some(schema) = schema
            .get("oneOf")
            .and_then(serde_json::Value::as_array)
            .and_then(|schemas| schemas.first())
        {
            return valid_value(schema);
        }
        match schema.get("type").and_then(serde_json::Value::as_str) {
            Some("object") => {
                let mut value = serde_json::Map::new();
                let properties = schema["properties"].as_object().unwrap();
                for name in schema
                    .get("required")
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(serde_json::Value::as_str)
                {
                    value.insert(name.to_owned(), valid_value(&properties[name]));
                }
                serde_json::Value::Object(value)
            }
            Some("array") => serde_json::json!([]),
            Some("string") => serde_json::json!("value"),
            Some("integer") => schema
                .get("minimum")
                .cloned()
                .unwrap_or_else(|| serde_json::json!(0)),
            Some("number") => serde_json::json!(0),
            Some("boolean") => serde_json::json!(false),
            _ => serde_json::Value::Null,
        }
    }

    /// 全 tool の IF メタデータが健全である（名前一意・説明非空・スキーマが JSON object・
    /// session / supervisor の既定 `call` は未実装）。store tool は durable effect を持つため、
    /// call の被覆は専用テストに委ねる。
    #[test]
    fn every_tool_has_valid_metadata() {
        let reg = registry();
        assert_eq!(reg.len(), 48); // issue 6 + memory 4 + session 32 + supervisor 6

        let mut seen = std::collections::HashSet::new();
        for tool in &reg {
            let name = tool.name();
            assert!(seen.insert(name));
            assert!(!tool.description().is_empty());

            let schema: serde_json::Value = serde_json::from_str(tool.input_schema()).unwrap();
            assert_eq!(schema["type"], "object");
            assert!(schema.get("properties").is_some());

            if !name.starts_with("issue_") && !name.starts_with("memory_") {
                assert!(
                    matches!(tool.call_store(&serde_json::json!({})), Err(ToolError::Unimplemented(n)) if n == name)
                );
            }
        }
    }

    /// 系統ごとの tool 数を固定する（IF の増減に気づけるように）。
    #[test]
    fn each_category_contributes_its_tools() {
        assert_eq!(super::issue::tools().len(), 6);
        assert_eq!(super::memory::tools().len(), 4);
        assert_eq!(super::session::tools().len(), 32);
        assert_eq!(super::supervisor::tools().len(), 6);
    }

    #[test]
    fn every_advertised_tool_has_one_route_schema_validator_and_policy() {
        let registry = registry();
        assert_eq!(registry.len(), 48);
        validate_registry(&registry).unwrap();
        for descriptor in &registry {
            let schema: serde_json::Value =
                serde_json::from_str(descriptor.input_schema()).unwrap();
            let valid = valid_value(&schema);
            descriptor.validate(&valid, &schema).unwrap();
            assert!(
                descriptor
                    .validate(&serde_json::json!([]), &schema)
                    .is_err()
            );
            assert!(matches!(
                (descriptor.route(), descriptor.caller_policy()),
                (
                    ToolRoute::Store | ToolRoute::Session(_),
                    CallerPolicy::Public
                ) | (ToolRoute::Session(_), CallerPolicy::SessionCredential)
                    | (ToolRoute::Dispatch(_), CallerPolicy::AgentCredential)
                    | (ToolRoute::Supervisor(_), CallerPolicy::DaemonProvenance)
            ));
        }
        assert_eq!(valid_value(&serde_json::json!({"type":"number"})), 0);
        assert_eq!(valid_value(&serde_json::json!({"type":"boolean"})), false);
        assert_eq!(valid_value(&serde_json::json!({})), serde_json::Value::Null);
    }

    #[test]
    fn registry_rejects_duplicate_name_duplicate_route_and_unadvertised_route() {
        assert_eq!(FixtureTool("description").description(), "fixture");
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                descriptor(Box::new(FixtureTool("unknown")));
            }))
            .is_err()
        );
        let duplicate_name = [
            ToolDescriptor::new(
                Box::new(FixtureTool("same")),
                ToolRoute::Store,
                CallerPolicy::Public,
            ),
            ToolDescriptor::new(
                Box::new(FixtureTool("same")),
                ToolRoute::Session(SessionAction::List),
                CallerPolicy::Public,
            ),
        ];
        assert!(
            validate_registry(&duplicate_name)
                .unwrap_err()
                .to_string()
                .contains("duplicate tool name")
        );

        let duplicate_route = [
            ToolDescriptor::new(
                Box::new(FixtureTool("one")),
                ToolRoute::Session(SessionAction::List),
                CallerPolicy::Public,
            ),
            ToolDescriptor::new(
                Box::new(FixtureTool("two")),
                ToolRoute::Session(SessionAction::List),
                CallerPolicy::Public,
            ),
        ];
        assert!(
            validate_registry(&duplicate_route)
                .unwrap_err()
                .to_string()
                .contains("duplicate executable route")
        );

        let unadvertised = [ToolDescriptor::fixture(
            Box::new(FixtureTool("hidden")),
            ToolRoute::Store,
            CallerPolicy::Public,
            false,
        )];
        assert!(
            validate_registry(&unadvertised)
                .unwrap_err()
                .to_string()
                .contains("not advertised")
        );

        let unavailable = [ToolDescriptor::new(
            Box::new(FixtureTool("stub")),
            ToolRoute::Unavailable("capability disabled"),
            CallerPolicy::Public,
        )];
        assert!(
            validate_registry(&unavailable)
                .unwrap_err()
                .to_string()
                .contains("unavailable")
        );
    }

    #[test]
    fn registry_rejects_schema_and_policy_drift() {
        assert_eq!(UnsupportedSchema.description(), "fixture");
        assert_eq!(SchemaFixture("fixture", "{}").description(), "fixture");
        let unsupported_schema = [ToolDescriptor::new(
            Box::new(UnsupportedSchema),
            ToolRoute::Store,
            CallerPolicy::Public,
        )];
        assert!(
            validate_registry(&unsupported_schema)
                .unwrap_err()
                .to_string()
                .contains("unsupported keyword")
        );

        let mismatch = [ToolDescriptor::new(
            Box::new(FixtureTool("mismatch")),
            ToolRoute::Store,
            CallerPolicy::AgentCredential,
        )];
        assert!(
            validate_registry(&mismatch)
                .unwrap_err()
                .to_string()
                .contains("policy mismatch")
        );

        for fixture in [
            SchemaFixture("invalid_json", "{"),
            SchemaFixture("not_object", r#"{"type":"array","properties":{}}"#),
            SchemaFixture("no_properties", r#"{"type":"object"}"#),
        ] {
            assert!(
                validate_registry(&[ToolDescriptor::new(
                    Box::new(fixture),
                    ToolRoute::Store,
                    CallerPolicy::Public,
                )])
                .is_err()
            );
        }

        assert!(matches!(
            super::memory::MemoryGet.call("{}"),
            Err(ToolError::InvalidParams(_))
        ));
    }
}
