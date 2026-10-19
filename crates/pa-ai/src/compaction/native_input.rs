//! Route-bound handoff from validated compaction windows to real inference.

use crate::providers::openai_responses_shared::{
    convert_responses_messages, ConvertResponsesMessagesOptions, ResponsesMessageScope,
    OPENAI_TOOL_CALL_PROVIDERS,
};
use crate::types::{Context, Message, Model, StreamOptions};
use crate::utils_inner::stream_failure::ProviderError;

use super::CompactionRoute;

/// Complete authoritative Responses input, with explicit compaction capability
/// provenance. Construct it with [`super::ResponsesWindow::inference_input`].
/// Credentials and transport options are supplied separately for every invocation.
/// Fresh continuation belongs in the window, not `Context.messages`.
#[derive(Clone)]
pub struct NativeResponsesInput {
    pub(super) route: CompactionRoute,
    pub(super) input_json: String,
}

impl std::fmt::Debug for NativeResponsesInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeResponsesInput")
            .field("route", &self.route)
            .field("input_bytes", &self.input_json.len())
            .finish()
    }
}

impl NativeResponsesInput {
    pub(crate) fn input_json(&self) -> &str {
        &self.input_json
    }

    pub(crate) fn validate(
        &self,
        model: &Model,
        context: &Context,
        options: &StreamOptions,
    ) -> Result<(), ProviderError> {
        if model.api != "openai-responses" {
            return Err(ProviderError::Message(
                "Native Responses input is not supported by this transport".into(),
            ));
        }
        let endpoint = format!("{}/responses/compact", model.base_url.trim_end_matches('/'));
        if self.route.provider != model.provider
            || self.route.model != model.id
            || self.route.endpoint != endpoint
        {
            return Err(ProviderError::Message(
                "Native Responses input provider, endpoint or model does not match".into(),
            ));
        }
        if !context.messages.is_empty() {
            return Err(ProviderError::Message(
                "Append fresh items to native Responses input; Context.messages must be empty"
                    .into(),
            ));
        }
        if options.api_key.as_deref().is_none_or(str::is_empty) {
            return Err(ProviderError::Message(
                "Native Responses input requires explicit per-invocation credentials".into(),
            ));
        }
        Ok(())
    }
}

/// Convert only fresh portable conversation items with the real Responses mapping.
/// System instructions are deliberately excluded; pass the current instructions
/// separately to compaction and inference. Never pass a retained native prefix
/// through this helper. Empty messages produce `[]`; do not append an empty array.
///
/// # Errors
/// Rejects other transports and reports JSON serialization failures.
pub fn responses_input_json(model: &Model, messages: &[Message]) -> Result<String, ProviderError> {
    if model.api != "openai-responses" {
        return Err(ProviderError::Message(
            "Portable Responses input requires the openai-responses transport".into(),
        ));
    }
    let context = Context {
        messages: messages.to_vec(),
        ..Context::default()
    };
    let items = convert_responses_messages(
        model,
        &context,
        &OPENAI_TOOL_CALL_PROVIDERS,
        ConvertResponsesMessagesOptions {
            scope: ResponsesMessageScope::FreshWindow,
            ..Default::default()
        },
    );
    serde_json::to_string(&items).map_err(|error| {
        ProviderError::Message(format!("Invalid portable Responses input: {error}"))
    })
}
