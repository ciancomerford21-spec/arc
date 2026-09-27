//! AI provider abstraction: local llama.cpp, OpenAI-compatible, Anthropic.
//!
//! All providers are async (non-blocking reqwest) so they can be called from
//! inside the daemon's tokio runtime. They support native tool calling: the
//! caller passes [`ToolDef`]s, the model answers with text and/or
//! [`ToolCall`]s, and tool results go back as [`AiRole::Tool`] messages.
//!
//! Request building and response parsing are pure functions so they are
//! tested without a network.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use std::time::Duration;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AiRole {
    System,
    User,
    Assistant,
    /// Result of a tool call; `tool_call_id` links it to the request.
    Tool,
}

/// A message in an AI conversation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AiMessage {
    pub role: AiRole,
    pub content: String,
    /// Tool calls requested by the assistant in this message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// For `Tool` messages: the call this is the result of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl AiMessage {
    pub fn system(s: impl Into<String>) -> Self {
        Self { role: AiRole::System, content: s.into(), tool_calls: vec![], tool_call_id: None }
    }
    pub fn user(s: impl Into<String>) -> Self {
        Self { role: AiRole::User, content: s.into(), tool_calls: vec![], tool_call_id: None }
    }
    pub fn assistant(s: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self { role: AiRole::Assistant, content: s.into(), tool_calls, tool_call_id: None }
    }
    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self { role: AiRole::Tool, content: content.into(), tool_calls: vec![], tool_call_id: Some(call_id.into()) }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Always a JSON object (string-encoded arguments are decoded).
    pub args: Json,
}

/// A tool offered to the model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON schema of the arguments object.
    pub parameters: Json,
}

/// Result from an AI completion.
#[derive(Debug, Clone, PartialEq)]
pub struct AiResult {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Clone, Copy)]
pub struct Sampling {
    pub temperature: f32,
    pub max_tokens: u32,
}

impl Default for Sampling {
    fn default() -> Self {
        Self { temperature: 0.2, max_tokens: 400 }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AiError {
    #[error("network error: {0}")]
    Network(String),
    #[error("provider error: {0}")]
    Provider(String),
    #[error("timeout")]
    Timeout,
    #[error("no provider configured")]
    Unconfigured,
}

impl From<reqwest::Error> for AiError {
    fn from(e: reqwest::Error) -> Self {
        if e.is_timeout() { AiError::Timeout } else { AiError::Network(e.to_string()) }
    }
}

/// Abstraction over AI providers.
#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    async fn complete(&self, messages: &[AiMessage], tools: &[ToolDef]) -> Result<AiResult, AiError>;
}

fn http_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(5))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

async fn post_json(req: reqwest::RequestBuilder, body: &Json) -> Result<Json, AiError> {
    let resp = req.json(body).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        let text: String = text.chars().take(500).collect();
        return Err(AiError::Provider(format!("status {status}: {text}")));
    }
    Ok(resp.json().await?)
}

// ---------------------------------------------------------------------------
// OpenAI-compatible (also used for the local llama.cpp server)
// ---------------------------------------------------------------------------

pub struct OpenAiCompat {
    name: String,
    base_url: String,
    model: String,
    api_key: Option<String>,
    sampling: Sampling,
    client: reqwest::Client,
}

impl OpenAiCompat {
    pub fn new(
        name: &str,
        base_url: String,
        model: String,
        api_key: Option<String>,
        sampling: Sampling,
        timeout: Duration,
    ) -> Self {
        Self { name: name.into(), base_url, model, api_key, sampling, client: http_client(timeout) }
    }
}

pub fn openai_body(model: &str, messages: &[AiMessage], tools: &[ToolDef], s: Sampling) -> Json {
    let msgs: Vec<Json> = messages
        .iter()
        .map(|m| match m.role {
            AiRole::System => json!({"role": "system", "content": m.content}),
            AiRole::User => json!({"role": "user", "content": m.content}),
            AiRole::Tool => json!({
                "role": "tool",
                "tool_call_id": m.tool_call_id.clone().unwrap_or_default(),
                "content": m.content,
            }),
            AiRole::Assistant => {
                let mut v = json!({"role": "assistant", "content": m.content});
                if !m.tool_calls.is_empty() {
                    v["tool_calls"] = Json::Array(
                        m.tool_calls
                            .iter()
                            .map(|c| {
                                json!({
                                    "id": c.id,
                                    "type": "function",
                                    "function": {"name": c.name, "arguments": c.args.to_string()},
                                })
                            })
                            .collect(),
                    );
                }
                v
            }
        })
        .collect();
    let mut body = json!({
        "model": model,
        "messages": msgs,
        "temperature": s.temperature,
        "max_tokens": s.max_tokens,
    });
    if !tools.is_empty() {
        body["tools"] = Json::Array(
            tools
                .iter()
                .map(|t| {
                    json!({"type": "function", "function": {
                        "name": t.name, "description": t.description, "parameters": t.parameters,
                    }})
                })
                .collect(),
        );
    }
    body
}

/// Tool-call arguments arrive as a JSON-encoded string (OpenAI) or an
/// object (some compatible servers). Normalise to an object.
fn decode_args(v: &Json) -> Json {
    match v {
        Json::String(s) if s.trim().is_empty() => json!({}),
        Json::String(s) => serde_json::from_str(s).unwrap_or_else(|_| json!({})),
        Json::Object(_) => v.clone(),
        _ => json!({}),
    }
}

pub fn parse_openai(parsed: &Json) -> Result<AiResult, AiError> {
    let msg = parsed["choices"]
        .get(0)
        .map(|c| &c["message"])
        .ok_or_else(|| AiError::Provider("response has no choices".into()))?;
    let content = msg["content"].as_str().unwrap_or("").to_string();
    let tool_calls = msg["tool_calls"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .enumerate()
                .map(|(i, tc)| ToolCall {
                    id: tc["id"].as_str().map(str::to_string).unwrap_or_else(|| format!("call_{i}")),
                    name: tc["function"]["name"].as_str().unwrap_or("").to_string(),
                    args: decode_args(&tc["function"]["arguments"]),
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(AiResult { content, tool_calls })
}

#[async_trait]
impl Provider for OpenAiCompat {
    fn name(&self) -> &str {
        &self.name
    }

    async fn complete(&self, messages: &[AiMessage], tools: &[ToolDef]) -> Result<AiResult, AiError> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let mut req = self.client.post(url);
        if let Some(key) = self.api_key.as_deref().filter(|k| !k.is_empty()) {
            req = req.bearer_auth(key);
        }
        let body = openai_body(&self.model, messages, tools, self.sampling);
        parse_openai(&post_json(req, &body).await?)
    }
}

// ---------------------------------------------------------------------------
// Anthropic Messages API
// ---------------------------------------------------------------------------

pub struct AnthropicProvider {
    base_url: String,
    model: String,
    api_key: Option<String>,
    sampling: Sampling,
    client: reqwest::Client,
}

impl AnthropicProvider {
    pub fn new(base_url: String, model: String, api_key: Option<String>, sampling: Sampling, timeout: Duration) -> Self {
        Self { base_url, model, api_key, sampling, client: http_client(timeout) }
    }
}

pub fn anthropic_body(model: &str, messages: &[AiMessage], tools: &[ToolDef], s: Sampling) -> Json {
    let mut system: Vec<String> = vec![];
    let mut out: Vec<Json> = vec![];
    // Anthropic requires tool results as user-role blocks; consecutive
    // results are merged into one user message.
    let push = |role: &str, block: Json, out: &mut Vec<Json>| {
        if let Some(last) = out.last_mut() {
            if last["role"] == role {
                last["content"].as_array_mut().unwrap().push(block);
                return;
            }
        }
        out.push(json!({"role": role, "content": [block]}));
    };
    for m in messages {
        match m.role {
            AiRole::System => system.push(m.content.clone()),
            AiRole::User => push("user", json!({"type": "text", "text": m.content}), &mut out),
            AiRole::Tool => push(
                "user",
                json!({
                    "type": "tool_result",
                    "tool_use_id": m.tool_call_id.clone().unwrap_or_default(),
                    "content": m.content,
                }),
                &mut out,
            ),
            AiRole::Assistant => {
                if !m.content.is_empty() {
                    push("assistant", json!({"type": "text", "text": m.content}), &mut out);
                }
                for c in &m.tool_calls {
                    push(
                        "assistant",
                        json!({"type": "tool_use", "id": c.id, "name": c.name, "input": c.args}),
                        &mut out,
                    );
                }
            }
        }
    }
    let mut body = json!({
        "model": model,
        "max_tokens": s.max_tokens,
        "temperature": s.temperature,
        "messages": out,
    });
    if !system.is_empty() {
        body["system"] = json!(system.join("\n\n"));
    }
    if !tools.is_empty() {
        body["tools"] = Json::Array(
            tools
                .iter()
                .map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.parameters}))
                .collect(),
        );
    }
    body
}

pub fn parse_anthropic(parsed: &Json) -> Result<AiResult, AiError> {
    let blocks = parsed["content"]
        .as_array()
        .ok_or_else(|| AiError::Provider("response has no content".into()))?;
    let content = blocks
        .iter()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("");
    let tool_calls = blocks
        .iter()
        .filter(|b| b["type"] == "tool_use")
        .map(|b| ToolCall {
            id: b["id"].as_str().unwrap_or("").to_string(),
            name: b["name"].as_str().unwrap_or("").to_string(),
            args: decode_args(&b["input"]),
        })
        .collect();
    Ok(AiResult { content, tool_calls })
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    async fn complete(&self, messages: &[AiMessage], tools: &[ToolDef]) -> Result<AiResult, AiError> {
        let key = self.api_key.as_deref().filter(|k| !k.is_empty()).ok_or(AiError::Unconfigured)?;
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let req = self.client.post(url).header("x-api-key", key).header("anthropic-version", "2023-06-01");
        let body = anthropic_body(&self.model, messages, tools, self.sampling);
        parse_anthropic(&post_json(req, &body).await?)
    }
}

// ---------------------------------------------------------------------------
// Provider set (primary + optional fallback)
// ---------------------------------------------------------------------------

pub struct ProviderSet {
    primary: Box<dyn Provider>,
    fallback: Option<Box<dyn Provider>>,
    /// Optional model used only to word the final spoken reply, once the
    /// tool-calling model has finished acting. Lets a cheap local model do
    /// system work while a stronger model does the talking.
    phrasing: Option<Box<dyn Provider>>,
}

impl ProviderSet {
    pub fn new(primary: Box<dyn Provider>, fallback: Option<Box<dyn Provider>>) -> Self {
        Self { primary, fallback, phrasing: None }
    }

    /// Attach a separate model for final replies.
    pub fn with_phrasing(mut self, phrasing: Box<dyn Provider>) -> Self {
        self.phrasing = Some(phrasing);
        self
    }

    pub fn primary_name(&self) -> &str {
        self.primary.name()
    }

    /// Name of the phrasing model, if one is configured.
    pub fn phrasing_name(&self) -> Option<&str> {
        self.phrasing.as_ref().map(|p| p.name())
    }

    /// Word a final reply with the phrasing model, falling back to the
    /// tool-calling model's own answer if that fails.
    ///
    /// `result` is what the acting model produced; it is passed through
    /// unchanged when there is nothing to improve, so a failure here never
    /// costs the user their answer.
    pub async fn phrase(&self, messages: &[AiMessage], result: &str) -> String {
        let Some(phraser) = &self.phrasing else {
            return result.to_string();
        };
        // Nothing to reword: an empty or very short answer is usually an
        // error path ("Cancelled.") that should be spoken verbatim.
        if result.trim().is_empty() || result.chars().count() < 12 {
            return result.to_string();
        }
        let mut msgs = messages.to_vec();
        msgs.push(AiMessage::assistant(result.to_string(), vec![]));
        msgs.push(AiMessage::user(
            "[system] That reply was produced by an assistant acting on a Linux desktop, and it will be \
             SPOKEN ALOUD. Rewrite it as natural spoken English in one or two short sentences: no markdown, \
             no lists, no tool names, no filler like 'Certainly' or 'It looks like'. Keep every fact and \
             any question exactly as it is. Reply with the rewritten text only.",
        ));
        match phraser.complete(&msgs, &[]).await {
            Ok(r) if !r.content.trim().is_empty() => r.content.trim().to_string(),
            Ok(_) | Err(_) => {
                tracing::warn!(provider = %phraser.name(), "phrasing model failed; using the raw reply");
                result.to_string()
            }
        }
    }

    pub async fn complete(&self, messages: &[AiMessage], tools: &[ToolDef]) -> Result<AiResult, AiError> {
        match self.primary.complete(messages, tools).await {
            Ok(r) => Ok(r),
            Err(e) => match &self.fallback {
                Some(fb) => {
                    tracing::warn!(provider = %self.primary.name(), error = %e, "primary provider failed; using fallback");
                    fb.complete(messages, tools).await
                }
                None => Err(e),
            },
        }
    }
}

/// API key from the environment variable named in the config, falling back
/// to a `NAME=value` line in ~/.config/arc/secrets.env.
fn key_from_env(var: &str) -> Option<String> {
    if var.is_empty() {
        return None;
    }
    if let Some(k) = std::env::var(var).ok().filter(|k| !k.is_empty()) {
        return Some(k);
    }
    let text = std::fs::read_to_string(arc_config::paths::config_dir().join("secrets.env")).ok()?;
    key_from_secrets(&text, var)
}

fn key_from_secrets(text: &str, var: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let line = line.trim();
        let line = line.strip_prefix("export ").unwrap_or(line);
        let (name, value) = line.split_once('=')?;
        if name.trim() != var {
            return None;
        }
        let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
        (!value.is_empty()).then(|| value.to_string())
    })
}

/// Build a ProviderSet from arc-config's `[ai]` section.
pub fn from_config(config: &arc_config::Ai) -> Result<ProviderSet, AiError> {
    use arc_config::ProviderKind;
    let sampling = Sampling { temperature: config.temperature, max_tokens: config.max_tokens };
    let timeout = Duration::from_secs(config.timeout_s.max(1));
    let make = |kind: ProviderKind| -> Result<Box<dyn Provider>, AiError> {
        Ok(match kind {
            ProviderKind::Local => Box::new(OpenAiCompat::new(
                "local",
                config.local.endpoint.clone(),
                config.local.model.clone(),
                key_from_env(&config.local.api_key_env),
                sampling,
                timeout,
            )),
            ProviderKind::Openai => Box::new(OpenAiCompat::new(
                "openai",
                config.openai.base_url.clone(),
                config.openai.model.clone(),
                key_from_env(&config.openai.api_key_env),
                sampling,
                timeout,
            )),
            ProviderKind::Anthropic => Box::new(AnthropicProvider::new(
                config.anthropic.base_url.clone(),
                config.anthropic.model.clone(),
                key_from_env(&config.anthropic.api_key_env),
                sampling,
                timeout,
            )),
            ProviderKind::None => return Err(AiError::Unconfigured),
        })
    };
    let primary = make(config.provider)?;
    let fallback = match config.fallback {
        ProviderKind::None => None,
        k if k == config.provider => None,
        k => Some(make(k)?),
    };
    // The phrasing model may double as the primary (e.g. local does the tools
    // and the same strong cloud model does the talking), so reuse it rather
    // than opening a second connection.
    let phrasing = if !config.phrasing_enabled || config.phrasing == ProviderKind::None {
        None
    } else if config.phrasing == config.provider {
        tracing::info!("phrasing model is the primary; reusing the primary connection");
        None
    } else {
        match make(config.phrasing) {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::warn!(error = %e, "phrasing model unavailable; replies will not be reworded");
                None
            }
        }
    };
    Ok(ProviderSet { primary, fallback, phrasing })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> ToolDef {
        ToolDef {
            name: "audio_volume_set".into(),
            description: "Set volume".into(),
            parameters: json!({"type": "object", "properties": {"level": {"type": "integer"}}}),
        }
    }

    fn convo() -> Vec<AiMessage> {
        let call = ToolCall { id: "c1".into(), name: "audio_volume_set".into(), args: json!({"level": 30}) };
        vec![
            AiMessage::system("be brief"),
            AiMessage::user("turn it down"),
            AiMessage::assistant("", vec![call]),
            AiMessage::tool_result("c1", r#"{"percent":30}"#),
        ]
    }

    #[test]
    fn message_roundtrip() {
        let m = AiMessage::user("hello");
        let d: AiMessage = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(d, m);
    }

    #[test]
    fn openai_body_encodes_tools_and_results() {
        let b = openai_body("m", &convo(), &[tool()], Sampling::default());
        assert_eq!(b["tools"][0]["function"]["name"], "audio_volume_set");
        assert_eq!(b["messages"][2]["tool_calls"][0]["function"]["arguments"], r#"{"level":30}"#);
        assert_eq!(b["messages"][3]["role"], "tool");
        assert_eq!(b["messages"][3]["tool_call_id"], "c1");
    }

    #[test]
    fn openai_parse_decodes_string_arguments() {
        let r = parse_openai(&json!({"choices": [{"message": {"content": null, "tool_calls": [
            {"id": "x", "type": "function", "function": {"name": "lock", "arguments": "{\"a\":1}"}}
        ]}}]}))
        .unwrap();
        assert_eq!(r.content, "");
        assert_eq!(r.tool_calls[0].name, "lock");
        assert_eq!(r.tool_calls[0].args, json!({"a": 1}));
    }

    #[test]
    fn openai_parse_rejects_empty() {
        assert!(parse_openai(&json!({"choices": []})).is_err());
    }

    #[test]
    fn anthropic_body_structure() {
        let b = anthropic_body("m", &convo(), &[tool()], Sampling::default());
        assert_eq!(b["system"], "be brief");
        assert_eq!(b["tools"][0]["input_schema"]["type"], "object");
        let msgs = b["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[2]["content"][0]["tool_use_id"], "c1");
    }

    #[test]
    fn anthropic_parse_text_and_tools() {
        let r = parse_anthropic(&json!({"content": [
            {"type": "text", "text": "Sure."},
            {"type": "tool_use", "id": "t1", "name": "lock", "input": {}}
        ]}))
        .unwrap();
        assert_eq!(r.content, "Sure.");
        assert_eq!(r.tool_calls[0].id, "t1");
    }

    #[test]
    fn provider_kind_none_is_unconfigured() {
        let cfg = arc_config::Ai { provider: arc_config::ProviderKind::None, ..Default::default() };
        assert!(matches!(from_config(&cfg), Err(AiError::Unconfigured)));
    }

    #[test]
    fn default_config_builds_local_provider() {
        let set = from_config(&arc_config::Ai::default()).unwrap();
        assert_eq!(set.primary_name(), "local");
    }

    /// Regression test for the old blocking client, which panicked when
    /// called inside a tokio runtime. Talks to a one-shot mock server.
    #[tokio::test]
    async fn async_client_works_inside_runtime() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16384];
            let _ = s.read(&mut buf).await;
            let body = r#"{"choices":[{"message":{"content":"hi there"}}]}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            s.write_all(resp.as_bytes()).await.unwrap();
        });
        let p = OpenAiCompat::new(
            "local",
            format!("http://{addr}/v1"),
            "m".into(),
            None,
            Sampling::default(),
            Duration::from_secs(5),
        );
        let r = p.complete(&[AiMessage::user("hi")], &[]).await.unwrap();
        assert_eq!(r.content, "hi there");
    }

    #[test]
    fn secrets_file_lines_are_parsed() {
        let text = "# comment\nOTHER=x\nexport GROQ_API_KEY=\"abc123\"\nEMPTY=\n";
        assert_eq!(key_from_secrets(text, "GROQ_API_KEY").as_deref(), Some("abc123"));
        assert_eq!(key_from_secrets(text, "OTHER").as_deref(), Some("x"));
        assert_eq!(key_from_secrets(text, "EMPTY"), None);
        assert_eq!(key_from_secrets(text, "MISSING"), None);
    }

    #[tokio::test]
    async fn fallback_is_used_when_primary_fails() {
        struct Fail;
        struct Ok_;
        #[async_trait]
        impl Provider for Fail {
            fn name(&self) -> &str {
                "fail"
            }
            async fn complete(&self, _: &[AiMessage], _: &[ToolDef]) -> Result<AiResult, AiError> {
                Err(AiError::Timeout)
            }
        }
        #[async_trait]
        impl Provider for Ok_ {
            fn name(&self) -> &str {
                "ok"
            }
            async fn complete(&self, _: &[AiMessage], _: &[ToolDef]) -> Result<AiResult, AiError> {
                Ok(AiResult { content: "fb".into(), tool_calls: vec![] })
            }
        }
        let set = ProviderSet::new(Box::new(Fail), Some(Box::new(Ok_)));
        assert_eq!(set.complete(&[], &[]).await.unwrap().content, "fb");
    }
}
