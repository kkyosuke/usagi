//! MCP wire adapter for the local independent-opinion usecase.

use std::path::Path;

use crate::infrastructure::ollama::OllamaClient;
use crate::usecase::local_opinion::{
    LocalOpinionError, LocalOpinionPort, LocalOpinionRequest, ask,
};
use serde::Deserialize;

use crate::mcp::tool::{Tool, ToolError};

#[derive(Deserialize)]
struct OpinionArgs {
    model: String,
    prompt: String,
}

/// `ollama_opinion` tool 一覧。
#[must_use]
pub fn tools() -> Vec<Box<dyn Tool>> {
    vec![Box::new(OllamaOpinion)]
}

/// localhost の Ollama model に独立した third opinion を求める。
pub struct OllamaOpinion;

impl OllamaOpinion {
    fn call_with_port(params: &str, port: &dyn LocalOpinionPort) -> Result<String, ToolError> {
        let args: OpinionArgs = serde_json::from_str(params)
            .map_err(|error| ToolError::InvalidParams(error.to_string()))?;
        let opinion = ask(
            port,
            &LocalOpinionRequest {
                model: &args.model,
                prompt: &args.prompt,
            },
        )
        .map_err(|error| match error {
            LocalOpinionError::Invalid(message) => ToolError::InvalidParams(message),
            LocalOpinionError::Provider(message) => ToolError::Execution(message),
        })?;
        serde_json::to_string_pretty(&serde_json::json!({
            "model": opinion.model,
            "opinion": opinion.content,
        }))
        .map_err(|error| ToolError::Execution(error.to_string()))
    }
}

impl Tool for OllamaOpinion {
    fn name(&self) -> &'static str {
        "ollama_opinion"
    }

    fn description(&self) -> &'static str {
        "Ollama で実行中の local LLM に独立した third opinion を質問するときに使う。model と prompt 必須。127.0.0.1:11434 だけに接続し、Ollama が未導入・未起動ならエラーを返す。"
    }

    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"model":{"type":"string","minLength":1,"maxLength":128,"x-maxUtf8Bytes":128},"prompt":{"type":"string","minLength":1,"maxLength":32768,"x-maxUtf8Bytes":32768}},"required":["model","prompt"],"additionalProperties":false}"#
    }

    fn call(&self, params: &str, _store_root: &Path) -> Result<String, ToolError> {
        Self::call_with_port(params, &OllamaClient)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usecase::local_opinion::{LocalOpinion, MAX_MODEL_BYTES, MAX_PROMPT_BYTES};

    struct Fake(Result<LocalOpinion, &'static str>);

    impl LocalOpinionPort for Fake {
        fn ask(&self, _model: &str, _prompt: &str) -> Result<LocalOpinion, String> {
            self.0.as_ref().map_or_else(
                |error| Err((*error).into()),
                |opinion| {
                    Ok(LocalOpinion {
                        model: opinion.model.clone(),
                        content: opinion.content.clone(),
                    })
                },
            )
        }
    }

    #[test]
    fn maps_success_and_both_error_classes_to_the_mcp_wire() {
        let result = OllamaOpinion::call_with_port(
            r#"{"model":"gemma3","prompt":"review"}"#,
            &Fake(Ok(LocalOpinion {
                model: "gemma3".into(),
                content: "check rollback".into(),
            })),
        )
        .unwrap();
        let result: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(result["model"], "gemma3");
        assert_eq!(result["opinion"], "check rollback");

        assert!(matches!(
            OllamaOpinion::call_with_port(r#"{"model":"","prompt":"p"}"#, &Fake(Err("unused"))),
            Err(ToolError::InvalidParams(_))
        ));
        assert!(matches!(
            OllamaOpinion::call_with_port(r#"{"model":"m","prompt":"p"}"#, &Fake(Err("offline"))),
            Err(ToolError::Execution(message)) if message == "offline"
        ));
    }

    #[test]
    fn tool_metadata_and_malformed_json_are_stable() {
        assert_eq!(tools().len(), 1);
        assert_eq!(OllamaOpinion.name(), "ollama_opinion");
        assert!(OllamaOpinion.description().contains("127.0.0.1"));
        let schema: serde_json::Value = serde_json::from_str(OllamaOpinion.input_schema()).unwrap();
        assert_eq!(schema["properties"]["model"]["maxLength"], MAX_MODEL_BYTES);
        assert_eq!(
            schema["properties"]["prompt"]["maxLength"],
            MAX_PROMPT_BYTES
        );
        assert!(matches!(
            OllamaOpinion.call("{", Path::new(".")),
            Err(ToolError::InvalidParams(_))
        ));
    }
}
