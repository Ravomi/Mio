//! Bidirectional JSONL RPC protocol (`--mode rpc`) and Multi-Provider Catalog (`--list-models`).
//!
//! Aligns Mio's headless integration surface with `pi-coding-agent` RPC mode (`prompt`, `steer`,
//! `follow_up`, `abort`, `compact`, `get_state`, `set_model`, `new_session`, `fork_session`).

use crate::{Provider, ThinkingLevel};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};

/// Hard limit for one JSONL input line in `--mode rpc`.
pub const MAX_RPC_LINE_BYTES: usize = 1024 * 1024;

/// Highest event envelope schema version this server can emit.
///
/// A client selects a version through the `initialize` command; a client that
/// never sends `initialize` keeps the legacy envelope (no correlation fields),
/// but every emitted document still carries `schemaVersion`.
pub const RPC_SCHEMA_VERSION: u64 = 1;

/// Envelope versions the server supports, in ascending order.
pub const RPC_ENVELOPE_VERSIONS: [u64; 1] = [1];

/// Discover the current RPC contract without starting a model request.
///
/// This versions the discovery document, not every legacy event payload.
/// Missing discovery on an older server means capabilities are unknown.
/// `schemaVersion` / `envelopeVersions` are additive; `eventEnvelope` stays
/// `legacy` until a client negotiates a newer envelope with `initialize`.
pub fn protocol_info() -> Value {
    json!({
        "name": "mio-jsonl-rpc",
        "discoveryVersion": 1,
        "eventEnvelope": "legacy",
        "schemaVersion": RPC_SCHEMA_VERSION,
        "envelopeVersions": RPC_ENVELOPE_VERSIONS,
        "capabilities": {
            "steering": true,
            "followUp": true,
            "asyncApproval": true,
            "dedupKey": true,
            "recoveryView": true,
            "pinnedContext": true,
            "contextCompactionAudit": true,
            "eventReplay": false,
            "automaticSideEffectReplay": false,
            "toolLifecycle": true,
            "imageInput": true,
            "imageSteering": false,
            "reasoningEvents": true,
            "initialize": true,
        },
        "correlation": {
            "sessionId": true,
            "runId": true,
            "turnId": true,
            "toolCallId": true,
            "subagentId": true,
            "parentRunId": true,
            "note": "Envelope v1 fills these fields from real run observations: sessionId from the host's persisted session, runId/turnId from the run's own UUID and engine turn, toolCallId from the model's own tool-call id (never the approval callId), and subagentId/parentRunId from the delegated child's stamped identity. A field the engine cannot truthfully supply stays null.",
        },
        "limits": {
            "inputLineBytes": MAX_RPC_LINE_BYTES,
            "messageBytes": 65536,
            "steeringQueue": 64,
            "followUpQueue": 64,
            "eventQueue": 256,
            "eventBytes": 1048576,
            "eventQueueBytes": 8388608,
        },
    })
}

/// Failure while reading a bounded input line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineReadError {
    /// I/O failure; the stream state is unknown, so reading must stop.
    Io(String),
    /// The line exceeded the limit; the rest of the line was already consumed.
    TooLong { max_bytes: usize },
}

impl std::fmt::Display for LineReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "read failed: {error}"),
            Self::TooLong { max_bytes } => write!(f, "line exceeds {max_bytes} bytes"),
        }
    }
}

/// Read one newline-terminated line without allowing unbounded buffering.
///
/// Returns `Ok(None)` on EOF, `Ok(Some(line))` for a line (trailing newline
/// stripped), and [`LineReadError::TooLong`] when the line would exceed
/// `max_bytes`. On overflow the rest of the line is consumed before returning so
/// the caller can keep reading framed input.
pub(crate) async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> Result<Option<String>, LineReadError> {
    read_line_with_state(reader, &mut LineBuffer::default(), max_bytes).await
}

#[derive(Default)]
struct LineBuffer {
    bytes: Vec<u8>,
    overflow: bool,
}

// The state belongs to the input, not to this Future. `select!` can drop a
// next_line call at any await without discarding a partially consumed command.
async fn read_line_with_state<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    state: &mut LineBuffer,
    max_bytes: usize,
) -> Result<Option<String>, LineReadError> {
    loop {
        let available = reader
            .fill_buf()
            .await
            .map_err(|e| LineReadError::Io(e.to_string()))?;
        if available.is_empty() {
            break;
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |position| position + 1);
        if !state.overflow {
            if state.bytes.len().saturating_add(consumed) > max_bytes {
                state.overflow = true;
                state.bytes.clear();
            } else {
                state.bytes.extend_from_slice(&available[..consumed]);
            }
        }
        reader.consume(consumed);
        if newline.is_some() {
            break;
        }
    }
    if std::mem::take(&mut state.overflow) {
        state.bytes.clear();
        return Err(LineReadError::TooLong { max_bytes });
    }
    let mut bytes = std::mem::take(&mut state.bytes);
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| LineReadError::Io("input frame is not valid UTF-8".into()))
}

/// Bounded JSONL reader over process stdin for `--mode rpc`.
///
/// Input lines larger than [`MAX_RPC_LINE_BYTES`] are rejected explicitly and
/// drained, so a hostile or buggy client cannot force unbounded allocation.
pub struct RpcInput {
    inner: BufReader<tokio::io::Stdin>,
    partial: LineBuffer,
}

#[cfg(test)]
mod framed_input_tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn cancellation_keeps_partial_commands_and_overflow_state() {
        let (mut writer, reader) = tokio::io::duplex(128);
        let mut reader = BufReader::new(reader);
        let mut state = LineBuffer::default();
        writer.write_all(b"{\"type\":").await.unwrap();
        let mut pending = Box::pin(read_line_with_state(&mut reader, &mut state, 32));
        assert!(futures_util::poll!(&mut pending).is_pending());
        drop(pending);
        assert_eq!(state.bytes, b"{\"type\":");
        writer.write_all(b"\"abort\"}\n").await.unwrap();
        assert_eq!(
            read_line_with_state(&mut reader, &mut state, 32)
                .await
                .unwrap(),
            Some("{\"type\":\"abort\"}".into())
        );

        writer.write_all(b"123456789").await.unwrap();
        let mut pending = Box::pin(read_line_with_state(&mut reader, &mut state, 8));
        assert!(futures_util::poll!(&mut pending).is_pending());
        drop(pending);
        assert!(state.overflow);
        writer.write_all(b"ignored\nnext\n").await.unwrap();
        assert_eq!(
            read_line_with_state(&mut reader, &mut state, 8).await,
            Err(LineReadError::TooLong { max_bytes: 8 })
        );
        assert_eq!(
            read_line_with_state(&mut reader, &mut state, 8)
                .await
                .unwrap(),
            Some("next".into())
        );
    }

    #[tokio::test]
    async fn invalid_utf8_is_rejected_without_lossy_command_rewriting() {
        let mut reader = BufReader::new(&b"bad\xff\n"[..]);
        assert!(matches!(
            read_bounded_line(&mut reader, 32).await,
            Err(LineReadError::Io(_))
        ));
    }
}

impl Default for RpcInput {
    fn default() -> Self {
        Self::new()
    }
}

impl RpcInput {
    pub fn new() -> Self {
        Self {
            inner: BufReader::new(tokio::io::stdin()),
            partial: LineBuffer::default(),
        }
    }

    pub async fn next_line(&mut self) -> Result<Option<String>, LineReadError> {
        read_line_with_state(&mut self.inner, &mut self.partial, MAX_RPC_LINE_BYTES).await
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCatalogEntry {
    pub provider: String,
    pub model: String,
    pub base_url: String,
    pub env_var: String,
    pub supports_thinking: bool,
    /// Legacy credential/local-preset hint; never a reachability/auth verdict.
    pub configured: bool,
    /// Credential presence only; local unauthenticated presets use None.
    pub credential_present: Option<bool>,
    /// Live states are unknown until explicitly probed.
    pub reachable: Option<bool>,
    pub authenticated: Option<bool>,
    pub protocol_supported: Option<bool>,
    #[serde(default)]
    pub capability_source: String,
    #[serde(default)]
    pub capability_snapshot_sha256: String,
}

struct ProviderPreset {
    name: &'static str,
    base_url: &'static str,
    env_vars: &'static [&'static str],
    models: &'static [(&'static str, bool)],
}

const PROVIDER_PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        name: "antigravity",
        base_url: "https://cloudcode-pa.googleapis.com",
        env_vars: &["MIO_CONFIG_DIR"],
        models: &[
            ("claude-sonnet-4-5", true),
            ("claude-opus-4-6-thinking", true),
            ("gemini-2.5-pro", true),
            ("gemini-2.5-flash", true),
        ],
    },
    ProviderPreset {
        name: "openai",
        base_url: "https://api.openai.com/v1",
        env_vars: &["OPENAI_API_KEY", "MIO_API_KEY"],
        models: &[
            ("gpt-4.1", false),
            ("gpt-4o", false),
            ("o3", true),
            ("o4-mini", true),
            ("codex-mini-latest", true),
        ],
    },
    ProviderPreset {
        name: "anthropic",
        // Native Messages root; `AnthropicConfig` normalizes a pasted `/v1`.
        base_url: "https://api.anthropic.com",
        env_vars: &["ANTHROPIC_API_KEY", "MIO_API_KEY"],
        models: &[
            ("claude-sonnet-4-5", true),
            ("claude-opus-4-6", true),
            ("claude-haiku-4-5", true),
        ],
    },
    ProviderPreset {
        name: "google",
        // Native GenerateContent root, not the OpenAI-compatible `/v1beta/openai`.
        base_url: "https://generativelanguage.googleapis.com",
        env_vars: &["GEMINI_API_KEY", "GOOGLE_API_KEY", "MIO_API_KEY"],
        models: &[("gemini-2.5-pro", true), ("gemini-2.5-flash", true)],
    },
    ProviderPreset {
        name: "deepseek",
        base_url: "https://api.deepseek.com/v1",
        env_vars: &["DEEPSEEK_API_KEY", "MIO_API_KEY"],
        models: &[
            ("deepseek-chat", false),
            ("deepseek-reasoner", true),
            ("deepseek-flash", true),
            ("deepseek-v4-pro", true),
        ],
    },
    ProviderPreset {
        name: "openrouter",
        base_url: "https://openrouter.ai/api/v1",
        env_vars: &["OPENROUTER_API_KEY", "MIO_API_KEY"],
        models: &[
            ("anthropic/claude-sonnet-4.5", true),
            ("openai/gpt-4.1", false),
            ("google/gemini-2.5-pro", true),
        ],
    },
    ProviderPreset {
        name: "groq",
        base_url: "https://api.groq.com/openai/v1",
        env_vars: &["GROQ_API_KEY", "MIO_API_KEY"],
        models: &[("llama-3.3-70b-versatile", false), ("qwen-qwq-32b", true)],
    },
    ProviderPreset {
        name: "xai",
        base_url: "https://api.x.ai/v1",
        env_vars: &["XAI_API_KEY", "MIO_API_KEY"],
        models: &[("grok-3", false), ("grok-3-mini", true)],
    },
    ProviderPreset {
        name: "mistral",
        base_url: "https://api.mistral.ai/v1",
        env_vars: &["MISTRAL_API_KEY", "MIO_API_KEY"],
        models: &[("codestral-latest", false), ("mistral-large-latest", false)],
    },
    ProviderPreset {
        name: "ollama",
        // Native daemon root, not the OpenAI-compatible `/v1`.
        base_url: "http://127.0.0.1:11434",
        env_vars: &["OLLAMA_API_KEY"],
        models: &[("qwen2.5-coder:32b", false), ("deepseek-r1:32b", true)],
    },
];

/// List available models across built-in provider presets, optionally filtered by substring search.
pub fn list_catalog_models(search: Option<&str>) -> Vec<ModelCatalogEntry> {
    let query = search.map(|s| s.trim().to_ascii_lowercase());
    let has_antigravity = crate::profile::config_directory()
        .ok()
        .and_then(|dir| crate::profile::summary(&dir).ok().flatten())
        .is_some();

    use sha2::{Digest, Sha256};
    let snapshot = format!(
        "{:x}",
        Sha256::digest(mio_core::catalog::Catalog::builtin_source().as_bytes())
    );
    let mut out = Vec::new();
    for preset in PROVIDER_PRESETS {
        let configured = if preset.name == "antigravity" {
            has_antigravity
        } else if preset.name == "ollama" {
            true
        } else {
            preset
                .env_vars
                .iter()
                .any(|k| std::env::var(k).is_ok_and(|v| !v.trim().is_empty()))
        };
        let primary_env = preset.env_vars.first().copied().unwrap_or("MIO_API_KEY");
        for &(model, adapter_thinking) in preset.models {
            let vendor = if preset.name == "google" {
                "gcp.gemini"
            } else {
                preset.name
            };
            let spec = mio_core::providers::registry::ProviderId::catalog(vendor)
                .and_then(|provider| mio_core::catalog::Catalog::builtin().get(provider, model));
            let supports_thinking =
                spec.map_or(adapter_thinking, |model| model.reasoning.supported);
            let capability_source = if spec.is_some() {
                "mio-core/catalog/models.json"
            } else {
                "provider-preset (not live verified)"
            };
            if let Some(ref q) = query
                && !q.is_empty()
                && !preset.name.to_ascii_lowercase().contains(q)
                && !model.to_ascii_lowercase().contains(q)
            {
                continue;
            }
            out.push(ModelCatalogEntry {
                provider: preset.name.to_owned(),
                model: model.to_owned(),
                base_url: preset.base_url.to_owned(),
                env_var: primary_env.to_owned(),
                supports_thinking,
                configured,
                credential_present: if preset.name == "ollama" {
                    None
                } else {
                    Some(configured)
                },
                reachable: None,
                authenticated: None,
                protocol_supported: None,
                capability_source: capability_source.into(),
                capability_snapshot_sha256: snapshot.clone(),
            });
        }
    }
    out
}

/// Resolve provider enum, default base URL, and credential from environment variables for a named provider.
pub fn resolve_provider_endpoint(
    provider_name: &str,
    explicit_base: Option<String>,
) -> Result<(Provider, String, String), String> {
    let lower = provider_name.trim().to_ascii_lowercase();
    let normalized = match lower.as_str() {
        "gemini" => "google",
        other => other,
    };

    match normalized {
        "antigravity" => {
            if explicit_base.is_some() {
                return Err(
                    "Antigravity uses its account endpoint; remove --base-url/MIO_BASE_URL".into(),
                );
            }
            Ok((Provider::Antigravity, String::new(), String::new()))
        }
        "deepseek" => {
            let base = explicit_base.unwrap_or_else(|| "https://api.deepseek.com/v1".into());
            let key = read_first_env(&["DEEPSEEK_API_KEY", "MIO_API_KEY"]);
            Ok((Provider::Deepseek, base, key))
        }
        "custom" | "custom-responses" => {
            let base = explicit_base.unwrap_or_else(|| "https://api.openai.com/v1".into());
            let provider = if normalized == "custom-responses" {
                Provider::CustomResponses
            } else {
                Provider::Custom
            };
            let key = if provider == Provider::CustomResponses {
                read_first_env(&["SUB2API_API_KEY", "MIO_API_KEY", "OPENAI_API_KEY"])
            } else {
                read_first_env(&["MIO_API_KEY", "OPENAI_API_KEY"])
            };
            Ok((provider, base, key))
        }
        other => {
            if let Some(preset) = PROVIDER_PRESETS.iter().find(|p| p.name == other) {
                let base = explicit_base.unwrap_or_else(|| preset.base_url.to_owned());
                let mut vars = preset.env_vars.to_vec();
                vars.push("MIO_API_KEY");
                let key = read_first_env(&vars);
                // Anthropic, Google and Ollama presets use their native
                // protocols. The OpenAI-compatible vendors stay on the
                // `custom` Chat Completions route.
                let provider = match other {
                    "anthropic" => Provider::Anthropic,
                    "google" => Provider::Google,
                    "ollama" => Provider::Ollama,
                    _ => Provider::Custom,
                };
                Ok((provider, base, key))
            } else {
                Err(format!(
                    "CLI provider must be custom, custom-responses, deepseek, antigravity, openai, anthropic, google, openrouter, groq, xai, mistral, or ollama (got '{provider_name}')"
                ))
            }
        }
    }
}

fn read_first_env(keys: &[&str]) -> String {
    for &k in keys {
        if let Ok(val) = std::env::var(k)
            && !val.trim().is_empty()
        {
            return val;
        }
    }
    String::new()
}

/// Incoming JSONL command in `--mode rpc`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RpcImage {
    pub mime_type: String,
    pub data_base64: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl RpcImage {
    pub fn to_image(&self) -> Result<mio_core::message::Image, String> {
        use mio_core::message::{DocumentSourceKind, Image, ImageDetail, ImageMediaType};
        let media_type =
            match self.mime_type.as_str() {
                "image/png" => ImageMediaType::PNG,
                "image/jpeg" => ImageMediaType::JPEG,
                "image/gif" => ImageMediaType::GIF,
                "image/webp" => ImageMediaType::WEBP,
                _ => return Err(
                    "Image input MIME type must be image/png, image/jpeg, image/gif or image/webp"
                        .into(),
                ),
            };
        let detail = match self.detail.as_deref() {
            None => None,
            Some("auto") => Some(ImageDetail::Auto),
            Some("low") => Some(ImageDetail::Low),
            Some("high") => Some(ImageDetail::High),
            Some(_) => return Err("Image detail must be auto, low or high".into()),
        };
        Ok(Image {
            data: DocumentSourceKind::Base64(self.data_base64.clone()),
            media_type: Some(media_type),
            detail,
            native: None,
        })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RpcCommand {
    Prompt {
        #[serde(default)]
        id: Option<String>,
        message: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<RpcImage>,
        /// 显式任务 key：相同 key（活动/排队/终态）不重复执行，返回已有任务标识。
        #[serde(default)]
        dedup_key: Option<String>,
    },
    Steer {
        #[serde(default)]
        id: Option<String>,
        message: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<RpcImage>,
    },
    FollowUp {
        #[serde(default)]
        id: Option<String>,
        message: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<RpcImage>,
        /// 显式任务 key，语义与 [`RpcCommand::Prompt`] 相同。
        #[serde(default)]
        dedup_key: Option<String>,
    },
    Abort {
        #[serde(default)]
        id: Option<String>,
    },
    Compact {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        instructions: Option<String>,
    },
    GetState {
        #[serde(default)]
        id: Option<String>,
    },
    /// Atomically replace the host-owned pinned registry while idle.
    SetPinnedContext {
        #[serde(default)]
        id: Option<String>,
        context: crate::context_memory::PinnedContext,
    },
    SetModel {
        #[serde(default)]
        id: Option<String>,
        model: String,
        #[serde(default)]
        thinking: Option<ThinkingLevel>,
    },
    NewSession {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        name: Option<String>,
    },
    ForkSession {
        #[serde(default)]
        id: Option<String>,
    },
    SetSessionName {
        #[serde(default)]
        id: Option<String>,
        name: String,
    },
    ExportHtml {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        output_path: Option<String>,
    },
    ApproveResponse {
        request_id: String,
        approved: bool,
    },
    /// One-shot capability negotiation.
    ///
    /// `schema_versions` lists the envelope versions the client accepts.
    /// `protocol_versions` is accepted as an alias. The server selects the
    /// highest common version, or rejects the negotiation when the sets are
    /// disjoint. A rejected negotiation never silently falls back to a newer
    /// envelope and blocks new task starts until a supported version is offered.
    Initialize {
        #[serde(default)]
        id: Option<String>,
        #[serde(default, alias = "protocol_versions")]
        schema_versions: Vec<u64>,
        #[serde(default)]
        capabilities: Vec<String>,
    },
}

pub fn rpc_response(command: &str, id: Option<&str>, success: bool, data: Value) -> Value {
    let mut obj = json!({
        "type": "response",
        "command": command,
        "success": success,
        "data": data,
    });
    if let Some(req_id) = id {
        obj["id"] = json!(req_id);
    }
    obj
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_rpc_commands() {
        let cmd: RpcCommand =
            serde_json::from_str(r#"{"type":"prompt","id":"req-1","message":"fix bug"}"#).unwrap();
        assert_eq!(
            cmd,
            RpcCommand::Prompt {
                id: Some("req-1".into()),
                message: "fix bug".into(),
                images: vec![],
                dedup_key: None,
            }
        );

        let keyed: RpcCommand = serde_json::from_str(
            r#"{"type":"prompt","id":"req-2","message":"fix bug","dedup_key":"job-1"}"#,
        )
        .unwrap();
        assert_eq!(
            keyed,
            RpcCommand::Prompt {
                id: Some("req-2".into()),
                message: "fix bug".into(),
                images: vec![],
                dedup_key: Some("job-1".into()),
            }
        );
        let keyed_follow_up: RpcCommand =
            serde_json::from_str(r#"{"type":"follow_up","message":"next","dedup_key":"job-2"}"#)
                .unwrap();
        assert_eq!(
            keyed_follow_up,
            RpcCommand::FollowUp {
                id: None,
                message: "next".into(),
                images: vec![],
                dedup_key: Some("job-2".into()),
            }
        );

        let steer: RpcCommand =
            serde_json::from_str(r#"{"type":"steer","message":"use BTreeMap instead"}"#).unwrap();
        assert_eq!(
            steer,
            RpcCommand::Steer {
                id: None,
                message: "use BTreeMap instead".into(),
                images: vec![],
            }
        );

        let initialize: RpcCommand = serde_json::from_str(
            r#"{"type":"initialize","id":"i1","schema_versions":[1],"capabilities":["steering"]}"#,
        )
        .unwrap();
        assert_eq!(
            initialize,
            RpcCommand::Initialize {
                id: Some("i1".into()),
                schema_versions: vec![1],
                capabilities: vec!["steering".into()],
            }
        );
        // `protocol_versions` is an accepted alias for the same field.
        let aliased: RpcCommand =
            serde_json::from_str(r#"{"type":"initialize","protocol_versions":[1,2]}"#).unwrap();
        assert_eq!(
            aliased,
            RpcCommand::Initialize {
                id: None,
                schema_versions: vec![1, 2],
                capabilities: Vec::new(),
            }
        );
    }

    #[test]
    fn discovery_advertises_schema_version_and_negotiation() {
        let info = protocol_info();
        assert_eq!(info["discoveryVersion"], 1);
        assert_eq!(info["eventEnvelope"], "legacy");
        assert_eq!(info["schemaVersion"], RPC_SCHEMA_VERSION);
        assert_eq!(info["envelopeVersions"], json!(RPC_ENVELOPE_VERSIONS));
        assert_eq!(info["capabilities"]["initialize"], true);
        assert_eq!(info["correlation"]["sessionId"], true);
        assert_eq!(info["correlation"]["runId"], true);
        assert_eq!(info["correlation"]["turnId"], true);
        assert_eq!(info["correlation"]["toolCallId"], true);
        assert_eq!(info["correlation"]["subagentId"], true);
        assert_eq!(info["correlation"]["parentRunId"], true);
    }

    #[test]
    fn test_catalog_models_and_provider_resolution() {
        let all = list_catalog_models(None);
        assert!(all.len() >= 10);
        let claude = list_catalog_models(Some("claude"));
        assert!(!claude.is_empty());
        assert!(claude.iter().all(|m| m.model.contains("claude")));

        let (prov, base, _) = resolve_provider_endpoint("openrouter", None).unwrap();
        assert_eq!(prov, Provider::Custom);
        assert_eq!(base, "https://openrouter.ai/api/v1");
    }
}
