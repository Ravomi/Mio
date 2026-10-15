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
    pub configured: bool,
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
        models: &[("deepseek-chat", false), ("deepseek-reasoner", true)],
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
        for &(model, supports_thinking) in preset.models {
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
        "custom" => {
            let base = explicit_base.unwrap_or_else(|| "https://api.openai.com/v1".into());
            let key = read_first_env(&["MIO_API_KEY", "OPENAI_API_KEY"]);
            Ok((Provider::Custom, base, key))
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
                    "CLI provider must be custom, deepseek, antigravity, openai, anthropic, google, openrouter, groq, xai, mistral, or ollama (got '{provider_name}')"
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
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RpcCommand {
    Prompt {
        #[serde(default)]
        id: Option<String>,
        message: String,
    },
    Steer {
        #[serde(default)]
        id: Option<String>,
        message: String,
    },
    FollowUp {
        #[serde(default)]
        id: Option<String>,
        message: String,
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
                message: "fix bug".into()
            }
        );

        let steer: RpcCommand =
            serde_json::from_str(r#"{"type":"steer","message":"use BTreeMap instead"}"#).unwrap();
        assert_eq!(
            steer,
            RpcCommand::Steer {
                id: None,
                message: "use BTreeMap instead".into()
            }
        );
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
