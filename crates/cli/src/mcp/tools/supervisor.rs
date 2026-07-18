//! Run-level supervisor MCP vocabulary.
//!
//! These declarations deliberately live beside (rather than inside) the
//! session tools: a supervisor run is a daemon-owned aggregate and does not
//! replace a session lifecycle or a one-worker dispatch operation.

use crate::mcp::tool::Tool;

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
        r#"{"type":"object","properties":{"root_task":{"type":"string"},"initial_task_dag":{"type":"array","items":{"type":"object","properties":{"task_id":{"type":"string"},"dependencies":{"type":"array","items":{"type":"string"}},"instruction":{"type":"string"},"required_artifact_contract":{"type":"string"}},"required":["task_id","instruction"],"additionalProperties":false}},"policy_selector":{"type":"string"},"idempotency_key":{"type":"string"}},"required":["root_task","idempotency_key"],"additionalProperties":false}"#
    }
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
