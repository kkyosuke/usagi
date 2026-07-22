//! MCP tool の共通インターフェース。

use std::fmt;

use serde_json::Value;
use usagi_core::usecase::client::{DispatchToolAction, SessionAction, SupervisorToolAction};

/// Descriptor-owned execution destination. A route cannot be advertised without
/// being attached to the same descriptor as its metadata and policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolRoute {
    Store,
    Session(SessionAction),
    AgentInventory,
    AgentResume,
    Dispatch(DispatchToolAction),
    Supervisor(SupervisorToolAction),
    Unavailable(&'static str),
}

/// Caller provenance required by a tool route.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallerPolicy {
    Public,
    SessionCredential,
    AgentCredential,
    DaemonProvenance,
}

/// The single source of truth consumed by both `tools/list` and `tools/call`.
pub struct ToolDescriptor {
    tool: Box<dyn Tool>,
    route: ToolRoute,
    caller_policy: CallerPolicy,
    advertised: bool,
}

impl ToolDescriptor {
    #[must_use]
    pub fn new(tool: Box<dyn Tool>, route: ToolRoute, caller_policy: CallerPolicy) -> Self {
        Self {
            tool,
            route,
            caller_policy,
            advertised: true,
        }
    }

    #[cfg(test)]
    #[must_use]
    pub fn fixture(
        tool: Box<dyn Tool>,
        route: ToolRoute,
        caller_policy: CallerPolicy,
        advertised: bool,
    ) -> Self {
        Self {
            tool,
            route,
            caller_policy,
            advertised,
        }
    }

    #[must_use]
    pub fn name(&self) -> &'static str {
        self.tool.name()
    }

    #[must_use]
    pub fn description(&self) -> &'static str {
        self.tool.description()
    }

    #[must_use]
    pub fn input_schema(&self) -> &'static str {
        self.tool.input_schema()
    }

    #[must_use]
    pub const fn route(&self) -> ToolRoute {
        self.route
    }

    #[must_use]
    pub const fn caller_policy(&self) -> CallerPolicy {
        self.caller_policy
    }

    #[must_use]
    pub const fn is_advertised(&self) -> bool {
        self.advertised
    }

    /// Executes the descriptor's store adapter.
    ///
    /// # Errors
    ///
    /// Returns the adapter's validation, execution, or capability error.
    pub fn call_store(&self, arguments: &Value) -> Result<String, ToolError> {
        self.tool.call(&arguments.to_string())
    }

    /// Validates runtime arguments with the exact schema advertised for this call.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::InvalidParams`] when arguments do not match the schema.
    pub fn validate(&self, arguments: &Value, schema: &Value) -> Result<(), ToolError> {
        validate_schema(arguments, schema, "$").map_err(ToolError::InvalidParams)
    }
}

fn validate_schema(value: &Value, schema: &Value, path: &str) -> Result<(), String> {
    if let Some(options) = schema.get("oneOf").and_then(Value::as_array) {
        let matches = options
            .iter()
            .filter(|candidate| validate_schema(value, candidate, path).is_ok())
            .count();
        return (matches == 1)
            .then_some(())
            .ok_or_else(|| format!("{path} must match exactly one schema"));
    }
    if let Some(expected) = schema.get("const")
        && value != expected
    {
        return Err(format!("{path} must equal {expected}"));
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array)
        && !values.contains(value)
    {
        return Err(format!("{path} is not an allowed value"));
    }
    if let Some(types) = schema.get("type") {
        let matches_type = |kind: &str| match kind {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
            "number" => value.is_number(),
            "boolean" => value.is_boolean(),
            "null" => value.is_null(),
            _ => false,
        };
        let valid = types.as_str().is_some_and(matches_type)
            || types
                .as_array()
                .is_some_and(|kinds| kinds.iter().filter_map(Value::as_str).any(matches_type));
        if !valid {
            return Err(format!("{path} has the wrong type"));
        }
    }
    if let Some(minimum) = schema.get("minimum").and_then(Value::as_f64)
        && value.as_f64().is_some_and(|number| number < minimum)
    {
        return Err(format!("{path} must be at least {minimum}"));
    }
    if let Some(maximum) = schema.get("maximum").and_then(Value::as_f64)
        && value.as_f64().is_some_and(|number| number > maximum)
    {
        return Err(format!("{path} must be at most {maximum}"));
    }
    if let Some(object) = value.as_object() {
        let properties = schema.get("properties").and_then(Value::as_object);
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for key in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(key) {
                    return Err(format!("{path}.{key} is required"));
                }
            }
        }
        if schema.get("additionalProperties") == Some(&Value::Bool(false))
            && object
                .keys()
                .any(|key| properties.is_none_or(|known| !known.contains_key(key)))
        {
            return Err(format!("{path} contains an unknown property"));
        }
        if let Some(properties) = properties {
            for (key, child_schema) in properties {
                if let Some(child) = object.get(key) {
                    validate_schema(child, child_schema, &format!("{path}.{key}"))?;
                }
            }
        }
    }
    if let (Some(array), Some(items)) = (value.as_array(), schema.get("items")) {
        for (index, child) in array.iter().enumerate() {
            validate_schema(child, items, &format!("{path}[{index}]"))?;
        }
    }
    Ok(())
}

/// Rejects schema vocabulary that the runtime validator does not implement.
///
/// # Errors
///
/// Returns a path-qualified message for unsupported or malformed schema vocabulary.
pub fn validate_schema_definition(schema: &Value) -> Result<(), String> {
    fn visit(schema: &Value, path: &str) -> Result<(), String> {
        let object = schema
            .as_object()
            .ok_or_else(|| format!("{path} schema must be an object"))?;
        for keyword in object.keys() {
            if !matches!(
                keyword.as_str(),
                "type"
                    | "properties"
                    | "required"
                    | "additionalProperties"
                    | "enum"
                    | "oneOf"
                    | "const"
                    | "items"
                    | "minimum"
                    | "maximum"
                    | "default"
                    | "deprecated"
            ) {
                return Err(format!("{path} uses unsupported keyword {keyword}"));
            }
        }
        let supported_type = |kind: &str| {
            matches!(
                kind,
                "object" | "array" | "string" | "integer" | "number" | "boolean" | "null"
            )
        };
        if object.get("type").is_some_and(|value| {
            !value.as_str().is_some_and(supported_type)
                && value.as_array().is_none_or(|types| {
                    types.is_empty()
                        || types
                            .iter()
                            .any(|kind| !kind.as_str().is_some_and(supported_type))
                })
        }) || object
            .get("properties")
            .is_some_and(|value| !value.is_object())
            || object.get("required").is_some_and(|value| {
                value
                    .as_array()
                    .is_none_or(|items| items.iter().any(|item| !item.is_string()))
            })
            || object
                .get("additionalProperties")
                .is_some_and(|value| !value.is_boolean())
            || object.get("enum").is_some_and(|value| !value.is_array())
            || object
                .get("oneOf")
                .is_some_and(|value| value.as_array().is_none_or(Vec::is_empty))
            || object.get("items").is_some_and(|value| !value.is_object())
            || object
                .get("minimum")
                .is_some_and(|value| !value.is_number())
            || object
                .get("maximum")
                .is_some_and(|value| !value.is_number())
            || object
                .get("deprecated")
                .is_some_and(|value| !value.is_boolean())
        {
            return Err(format!("{path} has an invalid keyword value"));
        }
        if let Some(properties) = object.get("properties").and_then(Value::as_object) {
            for (name, child) in properties {
                visit(child, &format!("{path}.properties.{name}"))?;
            }
        }
        if let Some(items) = object.get("items") {
            visit(items, &format!("{path}.items"))?;
        }
        if let Some(options) = object.get("oneOf").and_then(Value::as_array) {
            for (index, child) in options.iter().enumerate() {
                visit(child, &format!("{path}.oneOf[{index}]"))?;
            }
        }
        Ok(())
    }
    visit(schema, "$schema")
}

/// MCP tool の実行インターフェース。
///
/// 各 tool は wire 上の名前・説明・入力スキーマ（`tools/list` に載る IF）と、呼び出し方
/// （`call`）を知る。dispatch は「型ごとに分岐する巨大な match」ではなく、名前で
/// レジストリを引いて `call` を呼ぶ一様な経路になる。
///
/// ロジックは usagi-core の usecase（issue / memory）と daemon への IPC（session）へ
/// 委譲する方針で、CLI のコマンドハンドラ（`crate::cli::commands`）と同じ core usecase を
/// 呼ぶ兄弟である。現状は **tool 面の枠だけ**で、`call` は既定実装（未実装を返すスタブ）の
/// ままにし、中身を実装する tool だけがこれをオーバーライドする。
pub trait Tool {
    /// wire 上の tool 名（例: `"issue_create"`）。
    fn name(&self) -> &'static str;

    /// tool の説明（`tools/list` に載る）。
    fn description(&self) -> &'static str;

    /// 入力パラメータの JSON Schema（`tools/list` に載る）。
    fn input_schema(&self) -> &'static str;

    /// tool を実行する。`params` は JSON-RPC の引数（JSON 文字列）、結果も JSON 文字列。
    ///
    /// 既定は未実装スタブ。中身（core usecase 呼び出し・daemon IPC・整形）を実装する
    /// tool はこのメソッドをオーバーライドする。
    ///
    /// # Errors
    ///
    /// 実行に失敗した場合や未実装の場合、`ToolError` を返す。
    fn call(&self, _params: &str) -> Result<String, ToolError> {
        Err(ToolError::Unimplemented(self.name()))
    }
}

/// tool の dispatch・実行のエラー。
#[derive(Debug, PartialEq, Eq)]
pub enum ToolError {
    /// 指定された名前の tool が存在しない。
    UnknownTool(String),
    /// tool の枠だけがあり、中身が未実装。
    Unimplemented(&'static str),
    /// tool の JSON 引数が wire 契約に合わない。
    InvalidParams(String),
    /// tool の usecase または永続化処理が失敗した。
    Execution(String),
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ToolError::UnknownTool(name) => write!(f, "unknown tool `{name}`"),
            ToolError::Unimplemented(name) => write!(f, "tool `{name}` is not yet implemented"),
            ToolError::InvalidParams(message) | ToolError::Execution(message) => {
                f.write_str(message)
            }
        }
    }
}

impl std::error::Error for ToolError {}

#[cfg(test)]
mod tests {
    use super::{ToolError, validate_schema, validate_schema_definition};
    use serde_json::json;

    #[test]
    fn display_covers_both_variants() {
        assert_eq!(
            ToolError::UnknownTool("nope".into()).to_string(),
            "unknown tool `nope`"
        );
        assert_eq!(
            ToolError::Unimplemented("issue_create").to_string(),
            "tool `issue_create` is not yet implemented"
        );
        assert_eq!(
            ToolError::InvalidParams("bad arguments".into()).to_string(),
            "bad arguments"
        );
        assert_eq!(
            ToolError::Execution("store failed".into()).to_string(),
            "store failed"
        );
    }

    #[test]
    fn derives_and_error_trait() {
        let err = ToolError::Unimplemented("a");
        assert_eq!(err, ToolError::Unimplemented("a"));
        assert!(format!("{err:?}").contains("Unimplemented"));
        let as_error: &dyn std::error::Error = &err;
        assert!(as_error.to_string().contains("not yet implemented"));
    }

    #[test]
    fn runtime_schema_validator_covers_constraints_and_nested_values() {
        for (value, schema) in [
            (json!("x"), json!({"oneOf":[{"type":"integer"}]})),
            (json!("x"), json!({"const":"y"})),
            (json!("x"), json!({"enum":["y"]})),
            (json!(null), json!({"type":"unsupported"})),
            (json!(-1), json!({"type":"integer","minimum":0})),
            (json!(2), json!({"type":"integer","maximum":1})),
            (
                json!({"extra":true}),
                json!({"type":"object","additionalProperties":false}),
            ),
            (
                json!({"child":1}),
                json!({"type":"object","properties":{"child":{"type":"string"}}}),
            ),
            (
                json!([false]),
                json!({"type":"array","items":{"type":"string"}}),
            ),
        ] {
            assert!(validate_schema(&value, &schema, "$").is_err());
        }
        assert!(validate_schema(&json!(0), &json!({"type":["integer","null"]}), "$").is_ok());
        for (value, kind) in [
            (json!(1.5), "number"),
            (json!(true), "boolean"),
            (json!(null), "null"),
        ] {
            assert!(validate_schema(&value, &json!({"type":kind}), "$").is_ok());
        }
        assert!(validate_schema(&json!({}), &json!({"type":"object"}), "$").is_ok());
        assert!(
            validate_schema(
                &json!({}),
                &json!({"type":"object","properties":{"optional":{"type":"string"}}}),
                "$"
            )
            .is_ok()
        );
        assert!(
            validate_schema(
                &json!(["ok"]),
                &json!({"type":"array","items":{"type":"string"}}),
                "$"
            )
            .is_ok()
        );
    }

    #[test]
    fn schema_definition_rejects_every_malformed_keyword_shape() {
        let malformed = [
            json!([]),
            json!({"type":[]}),
            json!({"type":["unknown"]}),
            json!({"properties":[]}),
            json!({"required":[1]}),
            json!({"additionalProperties":{}}),
            json!({"enum":{}}),
            json!({"oneOf":[]}),
            json!({"items":[]}),
            json!({"items":{"pattern":"x"}}),
            json!({"oneOf":[{"pattern":"x"}]}),
            json!({"minimum":"zero"}),
            json!({"maximum":"one"}),
            json!({"deprecated":"yes"}),
        ];
        for schema in malformed {
            assert!(validate_schema_definition(&schema).is_err());
        }
        for schema in [
            json!({"type":"array","items":{"type":"string"}}),
            json!({"oneOf":[{"type":"string"}]}),
        ] {
            assert!(validate_schema_definition(&schema).is_ok());
        }
    }
}
