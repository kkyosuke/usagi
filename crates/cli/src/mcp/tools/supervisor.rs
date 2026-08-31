//! Run-level supervisor MCP vocabulary.
//!
//! These declarations deliberately live beside (rather than inside) the
//! session tools: a supervisor run is a daemon-owned aggregate and does not
//! replace a session lifecycle or a one-worker dispatch operation.

use crate::mcp::tool::Tool;
use std::sync::OnceLock;
use usagi_core::domain::supervisor::{
    MAX_ARTIFACT_CONTRACT_BYTES, MAX_INITIAL_TASKS, MAX_SUPERVISOR_KEY_BYTES,
    MAX_SUPERVISOR_TEXT_BYTES, MAX_TASK_DEPENDENCIES, MAX_TASK_ID_BYTES,
};

#[must_use]
pub fn tools() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(SupervisorStart),
        Box::new(SupervisorGet),
        Box::new(SupervisorList),
        Box::new(SupervisorCancel),
        Box::new(SupervisorResolveEscalation),
        Box::new(SupervisorEvents),
    ]
}

pub struct SupervisorStart;
impl Tool for SupervisorStart {
    fn name(&self) -> &'static str {
        "supervisor_start"
    }
    fn description(&self) -> &'static str {
        "daemon 所有の supervisor run を開始する"
    }
    fn input_schema(&self) -> &'static str {
        static SCHEMA: OnceLock<String> = OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "root_task": bounded_string(MAX_SUPERVISOR_TEXT_BYTES),
                    "initial_task_dag": {
                        "type": "array",
                        "maxItems": MAX_INITIAL_TASKS,
                        "items": {
                            "type": "object",
                            "properties": {
                                "task_id": bounded_string(MAX_TASK_ID_BYTES),
                                "parent_task_id": bounded_string(MAX_TASK_ID_BYTES),
                                "dependencies": {
                                    "type": "array",
                                    "maxItems": MAX_TASK_DEPENDENCIES,
                                    "items": bounded_string(MAX_TASK_ID_BYTES),
                                },
                                "instruction": bounded_string(MAX_SUPERVISOR_TEXT_BYTES),
                                "required_artifact_contract": bounded_string(
                                    MAX_ARTIFACT_CONTRACT_BYTES,
                                ),
                            },
                            "required": ["task_id", "instruction"],
                            "additionalProperties": false,
                        },
                    },
                    "policy_selector": bounded_string(MAX_SUPERVISOR_KEY_BYTES),
                    "idempotency_key": bounded_string(MAX_SUPERVISOR_KEY_BYTES),
                },
                "required": ["root_task", "idempotency_key"],
                "additionalProperties": false,
            })
            .to_string()
        })
    }
}

fn bounded_string(maximum: usize) -> serde_json::Value {
    serde_json::json!({
        "type": "string",
        "minLength": 1,
        "maxLength": maximum,
        "x-maxUtf8Bytes": maximum,
    })
}
pub struct SupervisorGet;
impl Tool for SupervisorGet {
    fn name(&self) -> &'static str {
        "supervisor_get"
    }
    fn description(&self) -> &'static str {
        "supervisor run の安全な状態と相関を返す"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"supervisor_run_id":{"type":"string"},"event_cursor":{"type":"integer","minimum":0}},"required":["supervisor_run_id"],"additionalProperties":false}"#
    }
}
pub struct SupervisorList;
impl Tool for SupervisorList {
    fn name(&self) -> &'static str {
        "supervisor_list"
    }
    fn description(&self) -> &'static str {
        "supervisor run のページ済み要約を返す"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"state":{"type":"string","enum":["planning","running","waiting_for_decision","verifying","succeeded","failed","cancelled","escalated"]},"caller":{"type":"string"},"session":{"type":"string"},"cursor":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":100}},"additionalProperties":false}"#
    }
}
pub struct SupervisorCancel;
impl Tool for SupervisorCancel {
    fn name(&self) -> &'static str {
        "supervisor_cancel"
    }
    fn description(&self) -> &'static str {
        "権限と fence を検証して supervisor run を cancel する"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"supervisor_run_id":{"type":"string"},"reason":{"type":"string"}},"required":["supervisor_run_id","reason"],"additionalProperties":false}"#
    }
}
pub struct SupervisorResolveEscalation;
impl Tool for SupervisorResolveEscalation {
    fn name(&self) -> &'static str {
        "supervisor_resolve_escalation"
    }
    fn description(&self) -> &'static str {
        "authorized supervisor controller だけが escalation を解決する"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"supervisor_run_id":{"type":"string"},"escalation_id":{"type":"string"},"decision":{"type":"string","enum":["resume","cancel","fail"]}},"required":["supervisor_run_id","escalation_id","decision"],"additionalProperties":false}"#
    }
}
pub struct SupervisorEvents;
impl Tool for SupervisorEvents {
    fn name(&self) -> &'static str {
        "supervisor_events"
    }
    fn description(&self) -> &'static str {
        "supervisor run の順序付き durable event 要約を返す"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"supervisor_run_id":{"type":"string"},"after_sequence":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":100}},"required":["supervisor_run_id"],"additionalProperties":false}"#
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_schema_is_derived_from_the_domain_resource_policy() {
        let schema: serde_json::Value =
            serde_json::from_str(SupervisorStart.input_schema()).unwrap();
        let properties = &schema["properties"];
        assert_eq!(
            properties["root_task"]["x-maxUtf8Bytes"],
            MAX_SUPERVISOR_TEXT_BYTES
        );
        assert_eq!(
            properties["initial_task_dag"]["maxItems"],
            MAX_INITIAL_TASKS
        );
        let task = &properties["initial_task_dag"]["items"]["properties"];
        assert_eq!(task["task_id"]["maxLength"], MAX_TASK_ID_BYTES);
        assert_eq!(task["dependencies"]["maxItems"], MAX_TASK_DEPENDENCIES);
        assert_eq!(
            task["required_artifact_contract"]["maxLength"],
            MAX_ARTIFACT_CONTRACT_BYTES
        );
        assert_eq!(
            properties["idempotency_key"]["maxLength"],
            MAX_SUPERVISOR_KEY_BYTES
        );
    }
}
