//! Stateless `OpenAI` Responses server compaction. This module returns an immutable
//! candidate, not a session commit. The host owns coverage, branch identity, retirement,
//! fallback policy, and durable acceptance. Cancellation is terminal, never a decline.
//!
//! Register support explicitly for an exact resolved route. Neither the Responses API
//! name nor reasoning/vision flags establish support. No catalog or global provider
//! state is changed. HTTP cancellation drops local work; it cannot promise a refund.

mod native_input;
mod protocol;
pub use native_input::{responses_input_json, NativeResponsesInput};
#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::utils_inner::http::{send, RequestOptions};
use crate::utils_inner::stream_failure::ProviderError;
use protocol::{parse_response, validate_input, ItemWindow};

const PROTOCOL: &str = "openai.responses.compact";
const VERSION: u32 = 1;

/// Exact caller-resolved route. `endpoint` is the full `/responses/compact` URL.
/// An explicit custom registration may use a gateway or local HTTP endpoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionRoute {
    pub provider: String,
    pub endpoint: String,
    pub model: String,
}

/// Resource bounds, independent of the caller's model-token budget.
#[derive(Clone, Copy, Debug)]
pub struct CompactionLimits {
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
    pub max_items: usize,
    pub timeout: std::time::Duration,
}

impl Default for CompactionLimits {
    fn default() -> Self {
        Self {
            max_request_bytes: 8 * 1024 * 1024,
            max_response_bytes: 8 * 1024 * 1024,
            max_items: 10_000,
            timeout: std::time::Duration::from_secs(60),
        }
    }
}

/// A stateless Responses wire window, not normalized assistant content. Native
/// items stay bound to the route supplied at preparation or archive recovery.
#[derive(Clone, Debug)]
pub struct ResponsesWindow {
    route: CompactionRoute,
    input_json: String,
    kind: ItemWindow,
}

impl ResponsesWindow {
    /// Prepare native Responses input, or portable messages already converted to
    /// Responses input by the caller. Pass an array, not a conversation/response ID.
    /// The caller must not relabel another route's native items as portable input.
    ///
    /// # Errors
    /// Rejects malformed JSON, empty/non-object items, invalid compaction items,
    /// and configured size/item limits.
    pub fn from_stateless_input(
        route: &CompactionRoute,
        input_json: &str,
        limits: CompactionLimits,
    ) -> Result<Self, CompactionError> {
        validate_input(input_json, limits, ItemWindow::Input)?;
        Ok(Self {
            route: route.clone(),
            input_json: input_json.to_owned(),
            kind: ItemWindow::Input,
        })
    }

    /// Append fresh Responses input without reinterpreting archived native items.
    /// The original array's item bytes and order remain unchanged. The returned
    /// window retains native provenance for the next compaction or inference call.
    ///
    /// # Errors
    /// Rejects invalid fresh input, combined byte/item limits, and unusable items.
    pub fn append_input(
        &self,
        continuation_json: &str,
        limits: CompactionLimits,
    ) -> Result<Self, CompactionError> {
        validate_input(continuation_json, limits, ItemWindow::Input)?;
        let combined_length = self
            .input_json
            .len()
            .checked_add(continuation_json.len())
            .ok_or(CompactionError::LimitExceeded)?;
        if combined_length > limits.max_request_bytes {
            return Err(CompactionError::LimitExceeded);
        }
        // Both operands are validated array JSON. Keep the original raw prefix,
        // including whitespace and numeric/string spellings; splice only delimiters.
        let close = self
            .input_json
            .rfind(']')
            .ok_or(CompactionError::InvalidData("invalid native array"))?;
        let open_tail = continuation_json
            .find('[')
            .ok_or(CompactionError::InvalidData("invalid continuation array"))?;
        let close_tail = continuation_json
            .rfind(']')
            .ok_or(CompactionError::InvalidData("invalid continuation array"))?;
        let mut input_json = String::with_capacity(combined_length);
        input_json.push_str(&self.input_json[..close]);
        input_json.push(',');
        input_json.push_str(&continuation_json[open_tail + 1..close_tail]);
        input_json.push_str(&self.input_json[close..]);
        validate_input(&input_json, limits, self.kind)?;
        Ok(Self {
            route: self.route.clone(),
            input_json,
            kind: self.kind,
        })
    }

    /// Bind this complete native window to the real Responses inference transport.
    /// Support is explicitly registered on the adapter, never inferred from the API
    /// name or reasoning/vision flags. Native inference requires explicit credentials,
    /// empty `Context.messages`, and the same provider, endpoint and model. Tools are
    /// still converted normally; `Context.system_prompt` becomes `instructions`.
    ///
    /// # Errors
    /// Rejects cancellation, unsupported capability and route/model mismatch.
    pub fn inference_input(
        &self,
        adapter: &ResponsesCompactor,
        cancel: &CancellationToken,
    ) -> Result<NativeResponsesInput, CompactionError> {
        adapter.check_support(cancel)?;
        if self.route != adapter.route {
            return Err(CompactionError::RouteMismatch);
        }
        let input = NativeResponsesInput {
            route: self.route.clone(),
            input_json: self.input_json.clone(),
        };
        if cancel.is_cancelled() {
            return Err(CompactionError::Cancelled);
        }
        Ok(input)
    }

    /// Exact ordered native input JSON. Insert this array directly into the next
    /// stateless Responses request; do not render it as an assistant message.
    #[must_use]
    pub fn input_json(&self) -> &str {
        &self.input_json
    }
}

/// Caller-attested upper bound for the *whole* compact request context, including
/// instructions. Compaction is not a rescue path for an already oversized input.
#[derive(Clone, Copy, Debug)]
pub struct InputBudget {
    pub input_tokens_upper_bound: u64,
    pub context_window_tokens: u64,
}

/// Only documented standalone compact parameters are exposed. Auth is supplied
/// per call, never discovered from home or the environment, and never archived.
pub struct CompactOptions<'a> {
    pub bearer_token: &'a str,
    pub instructions: Option<&'a str>,
    pub budget: InputBudget,
}

/// Request diagnostics for host policy. A 400/404 is not a universal unsupported
/// signal. Only explicit capability errors have that classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpFailureKind {
    Authentication,
    Permission,
    Quota,
    RateLimit,
    Transient,
    MalformedRequest,
    Route,
    Capability,
    Other,
}

#[derive(Debug, thiserror::Error)]
pub enum CompactionError {
    #[error("server compaction is not explicitly supported for this route")]
    Unsupported,
    #[error("native compaction route or model does not match")]
    RouteMismatch,
    #[error("invalid compaction route configuration")]
    InvalidRoute,
    #[error("compaction input does not fit the caller's model context budget")]
    InputTooLarge,
    #[error("compaction resource limit exceeded")]
    LimitExceeded,
    #[error("invalid compaction data: {0}")]
    InvalidData(&'static str),
    #[error("server compaction cancelled; remote billing may already have occurred")]
    Cancelled,
    #[error("server compaction timed out")]
    Timeout,
    #[error("server compaction HTTP {status}: {kind:?}")]
    Http {
        status: u16,
        kind: HttpFailureKind,
        code: Option<String>,
        request_id: Option<String>,
        headers: std::collections::HashMap<String, String>,
    },
    #[error("server compaction transport failed: {0}")]
    Transport(ProviderError),
}

impl From<ProviderError> for CompactionError {
    fn from(error: ProviderError) -> Self {
        if error == ProviderError::Aborted {
            Self::Cancelled
        } else {
            Self::Transport(error)
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Archive {
    version: u32,
    protocol: String,
    route: CompactionRoute,
    // Raw envelope preserves all ordered output items, unknown fields, JSON
    // number spellings and escapes. Reconstructing it from Value would lose bytes.
    response_json: String,
    usage: CompactionUsage,
}

/// Provider-reported request accounting, NOT the size of the next context window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cached_input_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
}

/// Validated immutable proposal candidate. No session state is published here.
/// Archive recovery revalidates the envelope and exact route before native replay.
pub struct CompactionCandidate {
    archive: Archive,
    output_json: String,
    usage_json: String,
}

impl CompactionCandidate {
    #[must_use]
    pub fn usage(&self) -> CompactionUsage {
        self.archive.usage
    }

    #[must_use]
    pub fn raw_usage_json(&self) -> &str {
        &self.usage_json
    }

    /// Persist the versioned provider/protocol/endpoint/model-bound archive.
    ///
    /// # Errors
    /// Returns an error if serialization fails.
    pub fn serialize(&self) -> Result<String, CompactionError> {
        serde_json::to_string(&self.archive)
            .map_err(|_| CompactionError::InvalidData("archive serialization failed"))
    }

    /// Recover exact native replay on a compatible, explicitly supported adapter.
    ///
    /// # Errors
    /// Rejects unsupported/mismatched routes, cancelled work, invalid archives,
    /// protocol versions, envelopes, and configured resource limits.
    pub fn restore(
        serialized: &str,
        adapter: &ResponsesCompactor,
        cancel: &CancellationToken,
    ) -> Result<Self, CompactionError> {
        adapter.check_support(cancel)?;
        // JSON-string escaping can expand a raw response by up to six times.
        if serialized.len()
            > adapter
                .limits
                .max_response_bytes
                .saturating_mul(6)
                .saturating_add(16_384)
        {
            return Err(CompactionError::LimitExceeded);
        }
        let archive: Archive = serde_json::from_str(serialized)
            .map_err(|_| CompactionError::InvalidData("invalid archive JSON"))?;
        if archive.version != VERSION || archive.protocol != PROTOCOL {
            return Err(CompactionError::InvalidData(
                "unsupported archive protocol/version",
            ));
        }
        if archive.route != adapter.route {
            return Err(CompactionError::RouteMismatch);
        }
        let parsed = parse_response(&archive.response_json, &adapter.route, adapter.limits)?;
        if parsed.usage != archive.usage {
            return Err(CompactionError::InvalidData(
                "archive usage does not match response",
            ));
        }
        if cancel.is_cancelled() {
            return Err(CompactionError::Cancelled);
        }
        Ok(Self {
            archive,
            output_json: parsed.output_json,
            usage_json: parsed.usage_json,
        })
    }

    /// Validate route and cancellation again at the host's acceptance point. The
    /// host must also check its branch and plugin generation before durable commit.
    ///
    /// # Errors
    /// Rejects cancellation, unsupported capability, and route/model mismatch.
    pub fn replay(
        &self,
        adapter: &ResponsesCompactor,
        cancel: &CancellationToken,
    ) -> Result<ResponsesWindow, CompactionError> {
        adapter.check_support(cancel)?;
        if self.archive.route != adapter.route {
            return Err(CompactionError::RouteMismatch);
        }
        let window = ResponsesWindow {
            route: self.archive.route.clone(),
            input_json: self.output_json.clone(),
            kind: ItemWindow::Compacted,
        };
        if cancel.is_cancelled() {
            return Err(CompactionError::Cancelled);
        }
        Ok(window)
    }
}

/// Standalone transport adapter. `supported_route` is an explicit capability
/// registration, not inferred from catalog flags. `None` declines before network.
/// A custom endpoint is possible only through this explicit configuration.
pub struct ResponsesCompactor {
    route: CompactionRoute,
    supported_route: Option<CompactionRoute>,
    limits: CompactionLimits,
}

impl ResponsesCompactor {
    /// # Errors
    /// Rejects malformed routes (including credentials, query/fragment, and paths
    /// other than `/responses/compact`) and zero resource bounds.
    pub fn new(
        route: CompactionRoute,
        supported_route: Option<CompactionRoute>,
        limits: CompactionLimits,
    ) -> Result<Self, CompactionError> {
        let url = url::Url::parse(&route.endpoint).map_err(|_| CompactionError::InvalidRoute)?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.path().ends_with("/responses/compact")
            || route.provider.is_empty()
            || route.model.is_empty()
        {
            return Err(CompactionError::InvalidRoute);
        }
        if limits.max_request_bytes == 0
            || limits.max_response_bytes == 0
            || limits.max_items == 0
            || limits.timeout.is_zero()
        {
            return Err(CompactionError::LimitExceeded);
        }
        Ok(Self {
            route,
            supported_route,
            limits,
        })
    }

    fn check_support(&self, cancel: &CancellationToken) -> Result<(), CompactionError> {
        if cancel.is_cancelled() {
            return Err(CompactionError::Cancelled);
        }
        if self.supported_route.as_ref() != Some(&self.route) {
            return Err(CompactionError::Unsupported);
        }
        Ok(())
    }

    /// Compact one immutable stateless window. No retries or fallback occur here.
    /// Dropping this future drops its request and cannot publish a late candidate.
    ///
    /// # Errors
    /// Distinguishes decline, route/budget/format errors, cancellation, timeout,
    /// HTTP diagnostic categories, and transport failures.
    #[tracing::instrument(skip_all)]
    pub async fn compact(
        &self,
        input: &ResponsesWindow,
        options: CompactOptions<'_>,
        cancel: &CancellationToken,
    ) -> Result<CompactionCandidate, CompactionError> {
        self.check_support(cancel)?;
        if input.route != self.route {
            return Err(CompactionError::RouteMismatch);
        }
        if options.budget.input_tokens_upper_bound == 0
            || options.budget.context_window_tokens == 0
            || options.budget.input_tokens_upper_bound > options.budget.context_window_tokens
        {
            return Err(CompactionError::InputTooLarge);
        }
        validate_input(&input.input_json, self.limits, input.kind)?;
        // Embed the raw array; round-tripping through Value corrupts native replay.
        let model =
            serde_json::to_string(&self.route.model).map_err(|_| CompactionError::InvalidRoute)?;
        let mut body = format!("{{\"model\":{model},\"input\":{}", input.input_json);
        if let Some(instructions) = options.instructions {
            let instructions = serde_json::to_string(instructions)
                .map_err(|_| CompactionError::InvalidData("invalid instructions"))?;
            body.push_str(",\"instructions\":");
            body.push_str(&instructions);
        }
        body.push('}');
        if body.len() > self.limits.max_request_bytes {
            return Err(CompactionError::LimitExceeded);
        }
        let work = async {
            let mut response = send(RequestOptions {
                headers: vec![(
                    "Authorization".to_owned(),
                    format!("Bearer {}", options.bearer_token),
                )],
                body: Some(body),
                signal: Some(cancel.clone()),
                ..RequestOptions::new(reqwest::Method::POST, self.route.endpoint.clone())
            })
            .await?;
            let mut bytes = Vec::new();
            while let Some(chunk) = response.next_bytes().await? {
                if chunk.len() > self.limits.max_response_bytes.saturating_sub(bytes.len()) {
                    return Err(CompactionError::LimitExceeded);
                }
                bytes.extend_from_slice(&chunk);
            }
            if cancel.is_cancelled() {
                return Err(CompactionError::Cancelled);
            }
            if response.status != 200 {
                return Err(protocol::http_error(
                    response.status,
                    &bytes,
                    &response.headers,
                ));
            }
            let response_json = String::from_utf8(bytes)
                .map_err(|_| CompactionError::InvalidData("response is not UTF-8"))?;
            let parsed = parse_response(&response_json, &self.route, self.limits)?;
            if cancel.is_cancelled() {
                return Err(CompactionError::Cancelled);
            }
            Ok(CompactionCandidate {
                archive: Archive {
                    version: VERSION,
                    protocol: PROTOCOL.to_owned(),
                    route: self.route.clone(),
                    response_json,
                    usage: parsed.usage,
                },
                output_json: parsed.output_json,
                usage_json: parsed.usage_json,
            })
        };
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(CompactionError::Cancelled),
            result = tokio::time::timeout(self.limits.timeout, work) => {
                result.unwrap_or(Err(CompactionError::Timeout))
            }
        }
    }
}
