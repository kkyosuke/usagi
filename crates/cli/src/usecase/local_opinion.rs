//! Independent local-LLM opinion usecase and its outbound port for the CLI face.

use std::fmt;

/// Maximum UTF-8 bytes accepted for an Ollama model name.
pub const MAX_MODEL_BYTES: usize = 128;
/// Maximum UTF-8 bytes accepted for one opinion prompt.
pub const MAX_PROMPT_BYTES: usize = 32 * 1024;

/// Request supplied by an inbound surface.
pub struct LocalOpinionRequest<'a> {
    pub model: &'a str,
    pub prompt: &'a str,
}

/// Independent opinion returned by the selected local model.
#[derive(Debug, Eq, PartialEq)]
pub struct LocalOpinion {
    pub model: String,
    pub content: String,
}

/// Outbound boundary implemented by a local model provider.
pub trait LocalOpinionPort {
    /// Ask one already validated model/prompt pair.
    ///
    /// # Errors
    ///
    /// Returns a safe diagnostic when the provider is unavailable or malformed.
    fn ask(&self, model: &str, prompt: &str) -> Result<LocalOpinion, String>;
}

/// Failure classification retained by the MCP adapter.
#[derive(Debug, Eq, PartialEq)]
pub enum LocalOpinionError {
    Invalid(String),
    Provider(String),
}

impl fmt::Display for LocalOpinionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) | Self::Provider(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for LocalOpinionError {}

/// Validate a request and ask the injected local provider.
///
/// # Errors
///
/// Returns [`LocalOpinionError::Invalid`] for the public input contract and
/// [`LocalOpinionError::Provider`] for provider connectivity or response failures.
pub fn ask(
    port: &dyn LocalOpinionPort,
    request: &LocalOpinionRequest<'_>,
) -> Result<LocalOpinion, LocalOpinionError> {
    validate(request.model, request.prompt)?;
    port.ask(request.model, request.prompt)
        .map_err(LocalOpinionError::Provider)
}

fn validate(model: &str, prompt: &str) -> Result<(), LocalOpinionError> {
    if model.is_empty() || model.len() > MAX_MODEL_BYTES {
        return Err(LocalOpinionError::Invalid(
            "model must contain 1..=128 UTF-8 bytes".into(),
        ));
    }
    if prompt.is_empty() || prompt.len() > MAX_PROMPT_BYTES {
        return Err(LocalOpinionError::Invalid(
            "prompt must contain 1..=32768 UTF-8 bytes".into(),
        ));
    }
    if model.chars().any(char::is_control) {
        return Err(LocalOpinionError::Invalid(
            "model must not contain control characters".into(),
        ));
    }
    let normalized = model.to_ascii_lowercase();
    if normalized.ends_with(":cloud") || normalized.ends_with("-cloud") {
        return Err(LocalOpinionError::Invalid(
            "cloud-backed Ollama models are not allowed by this local opinion tool".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn validates_then_returns_the_provider_opinion() {
        let expected = LocalOpinion {
            model: "gemma3".into(),
            content: "check rollback".into(),
        };
        let actual = ask(
            &Fake(Ok(expected)),
            &LocalOpinionRequest {
                model: "gemma3",
                prompt: "review",
            },
        )
        .unwrap();
        assert_eq!(actual.model, "gemma3");
        assert_eq!(actual.content, "check rollback");
    }

    #[test]
    fn rejects_invalid_or_cloud_inputs_before_the_provider() {
        let long_model = "m".repeat(MAX_MODEL_BYTES + 1);
        let long_prompt = "p".repeat(MAX_PROMPT_BYTES + 1);
        for (model, prompt) in [
            ("", "p"),
            (&long_model, "p"),
            ("m", ""),
            ("m", &long_prompt),
            ("bad\nname", "p"),
            ("glm:cloud", "p"),
            ("gpt-oss-cloud", "p"),
        ] {
            assert!(matches!(
                ask(
                    &Fake(Err("must not run")),
                    &LocalOpinionRequest { model, prompt }
                ),
                Err(LocalOpinionError::Invalid(_))
            ));
        }
    }

    #[test]
    fn preserves_provider_failures_and_error_traits() {
        let error = ask(
            &Fake(Err("offline")),
            &LocalOpinionRequest {
                model: "gemma3",
                prompt: "review",
            },
        )
        .unwrap_err();
        assert_eq!(error, LocalOpinionError::Provider("offline".into()));
        assert_eq!(error.to_string(), "offline");
        let as_error: &dyn std::error::Error = &error;
        assert_eq!(as_error.to_string(), "offline");
    }
}
