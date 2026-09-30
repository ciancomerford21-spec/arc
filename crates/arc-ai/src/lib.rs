//! AI provider abstraction: OpenAI-compatible (Hermes proxy, OpenAI) and Anthropic.
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
        Self {
            role: AiRole::Tool,
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: Some(call_id.into()),
        }
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
    /// The model's reasoning, when the provider returns it separately from the
    /// answer. Never sent back to the model and never spoken: it exists so the
    /// Arc app can show how a turn was decided. Empty when the model or
    /// provider does not expose it.
    pub reasoning: String,
    /// The provider stopped because it hit `max_tokens` (OpenAI
    /// `finish_reason: "length"`, Anthropic `stop_reason: "max_tokens"`).
    /// A reasoning model can spend the whole budget thinking and return no
    /// answer at all, or stop half-way through a tool call's arguments.
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct Sampling {
    pub temperature: f32,
    pub max_tokens: u32,
}

impl Default for Sampling {
    fn default() -> Self {
        // Greedy on purpose. Measured on Qwen3.5-2B with the tool list in
        // scope, temperature 0.4 against 0.0: two fewer correct tool calls out
        // of 18, and it answered "am I on wifi" with "Yes, you are currently
        // connected to the Wi-Fi network" without ever calling network_status.
        // That is a fabricated system fact, which is the one failure Arc must
        // not have. Any warmth in the reply should come from the personality
        // prompt and the phraser, not from sampling noise.
        Self { temperature: 0.0, max_tokens: 400 }
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

impl AiError {
    /// Whether retrying the same request could plausibly work.
    ///
    /// A 5xx or a dropped connection is the provider's problem and usually
    /// clears. A 401, 403 or 400 means the request itself is wrong, and
    /// sending it again just burns the user's time before failing identically.
    pub fn is_transient(&self) -> bool {
        match self {
            AiError::Network(_) | AiError::Timeout => true,
            AiError::Provider(text) => {
                if text.contains("400 Bad Request")
                    || text.contains("401")
                    || text.contains("403")
                    || text.contains("404")
                {
                    return false;
                }
                text.contains("429")
                    || text.contains("500")
                    || text.contains("502")
                    || text.contains("503")
                    || text.contains("504")
            }
            AiError::Unconfigured => false,
        }
    }
}

/// Abstraction over AI providers.
#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    async fn complete(&self, messages: &[AiMessage], tools: &[ToolDef]) -> Result<AiResult, AiError>;
    /// As `complete`, with a different token budget. Providers that cannot
    /// change it just run `complete`.
    async fn complete_with_budget(
        &self,
        messages: &[AiMessage],
        tools: &[ToolDef],
        _max_tokens: u32,
    ) -> Result<AiResult, AiError> {
        self.complete(messages, tools).await
    }
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
// OpenAI-compatible (the Hermes proxy speaks this dialect)
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
    let reasoning = msg["reasoning"]
        .as_str()
        .or_else(|| msg["reasoning_content"].as_str())
        .unwrap_or("")
        .trim()
        .to_string();
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
    // Cut off by the budget. finish_reason says so most of the time, but not
    // always: measured, the proxy returned finish_reason "tool_calls" for a
    // call that stopped at exactly max_tokens with its arguments cut
    // mid-JSON (1208 chars, unparseable). decode_args turns that into `{}`
    // or a fragment, and a script tool arrived as just its shebang line. So
    // arguments that are not valid JSON also count as cut off.
    let broken_args = msg["tool_calls"].as_array().is_some_and(|a| {
        a.iter().any(|tc| match &tc["function"]["arguments"] {
            Json::String(s) => !s.trim().is_empty() && serde_json::from_str::<Json>(s).is_err(),
            _ => false,
        })
    });
    let truncated = parsed["choices"][0]["finish_reason"] == "length" || broken_args;
    Ok(AiResult { content, tool_calls, reasoning, truncated })
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
        if std::env::var("ARC_DEBUG_BODY").is_ok() {
            eprintln!("[BODY] {}", serde_json::to_string(&body).unwrap_or_default());
        }
        parse_openai(&post_json(req, &body).await?)
    }

    async fn complete_with_budget(
        &self,
        messages: &[AiMessage],
        tools: &[ToolDef],
        max_tokens: u32,
    ) -> Result<AiResult, AiError> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let mut req = self.client.post(url);
        if let Some(key) = self.api_key.as_deref().filter(|k| !k.is_empty()) {
            req = req.bearer_auth(key);
        }
        let s = Sampling { max_tokens, ..self.sampling };
        parse_openai(&post_json(req, &openai_body(&self.model, messages, tools, s)).await?)
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
    pub fn new(
        base_url: String,
        model: String,
        api_key: Option<String>,
        sampling: Sampling,
        timeout: Duration,
    ) -> Self {
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
    let blocks =
        parsed["content"].as_array().ok_or_else(|| AiError::Provider("response has no content".into()))?;
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
    let reasoning = blocks
        .iter()
        .filter(|b| b["type"] == "thinking")
        .filter_map(|b| b["thinking"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();
    let truncated = parsed["stop_reason"] == "max_tokens";
    Ok(AiResult { content, tool_calls, reasoning, truncated })
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

/// Trim a provider error to a status-line length. Cloud errors carry a
/// whole JSON body, which is unreadable in a two-column table.
fn truncate(s: &str, n: usize) -> String {
    let t = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() <= n { t } else { format!("{}…", t.chars().take(n).collect::<String>()) }
}

pub struct ProviderSet {
    primary: Box<dyn Provider>,
    fallback: Option<Box<dyn Provider>>,
    /// Optional model used only to word the final spoken reply, once the
    /// tool-calling model has finished acting. Lets a cheap local model do
    /// system work while a stronger model does the talking.
    phrasing: Option<Box<dyn Provider>>,
    /// Outcome of the most recent completion, for `arc status`.
    ///
    /// The alternative was a health probe, and a probe here would mean
    /// spending a real request (and a real round trip, and a possible 500)
    /// purely to colour a line in a status table. Recording what actually
    /// happened to real traffic is free and strictly more honest: it reports
    /// the provider Arc is actually using, not a synthetic call.
    last: std::sync::Arc<std::sync::Mutex<LastCall>>,
    /// The configured budget, so a cut-off reply can be retried with more.
    max_tokens: u32,
}

/// A reply cut off by `max_tokens` is retried once with this multiple of
/// the budget, and never less than `RETRY_BUDGET_MIN`.
///
/// Sized from a measurement, not a guess. "Create yourself a screenshot
/// tool" against the real tool_create schema: at 400 and at 1600 tokens
/// every attempt was cut off (all budget spent reasoning, or arguments cut
/// mid-JSON); at 8000 it produced a valid 129-line script using 6301
/// tokens, 14.6k chars of them reasoning, in 37 s. It is only spent when the
/// normal budget has already failed.
const RETRY_BUDGET_FACTOR: u32 = 4;
const RETRY_BUDGET_MIN: u32 = 8000;

/// The last thing the provider did, for the health line in `arc status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LastCall {
    /// Nothing has been sent yet, so nothing is known.
    Never,
    Ok,
    /// The last call failed. Carries the reason, trimmed, because the whole
    /// point is explaining why the component is not ok.
    Failed(String),
}

impl ProviderSet {
    pub fn new(primary: Box<dyn Provider>, fallback: Option<Box<dyn Provider>>) -> Self {
        Self {
            primary,
            fallback,
            phrasing: None,
            last: std::sync::Arc::new(std::sync::Mutex::new(LastCall::Never)),
            max_tokens: Sampling::default().max_tokens,
        }
    }

    /// The budget a cut-off reply is retried from.
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Attach a separate model for final replies.
    pub fn with_phrasing(mut self, phrasing: Box<dyn Provider>) -> Self {
        self.phrasing = Some(phrasing);
        self
    }

    pub fn primary_name(&self) -> &str {
        self.primary.name()
    }

    /// What happened on the most recent completion, for `arc status`.
    pub fn last_call(&self) -> LastCall {
        self.last.lock().map(|g| g.clone()).unwrap_or(LastCall::Never)
    }

    fn record(&self, r: Result<(), &AiError>) {
        if let Ok(mut g) = self.last.lock() {
            *g = match r {
                Ok(()) => LastCall::Ok,
                Err(e) => LastCall::Failed(truncate(&e.to_string(), 120)),
            };
        }
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
        let r = self.complete_once(messages, tools).await?;
        if !r.truncated {
            return Ok(r);
        }
        // Cut off by max_tokens. The budget is set for speech, but a reasoning
        // model spends it *thinking* first, and on a list-style question it
        // can use all 400 tokens before writing a word: measured on
        // "create a list of some tools that might be useful" -- 400 tokens,
        // 1727 chars of reasoning, 0 chars of answer, finish_reason=length --
        // which Arc then passed on as a silent reply, three times running.
        // A truncated tool call is worse: its arguments are cut mid-JSON, so
        // a script arrives as just its shebang line.
        //
        // Retry once with a bigger budget. Speech stays short regardless:
        // the reply is still cut to MAX_SPOKEN_CHARS before it is spoken.
        let bigger = (self.max_tokens * RETRY_BUDGET_FACTOR).max(RETRY_BUDGET_MIN);
        tracing::info!(
            provider = %self.primary.name(),
            from = self.max_tokens,
            to = bigger,
            content = r.content.len(),
            tool_calls = r.tool_calls.len(),
            "reply cut off by max_tokens; retrying with a bigger budget"
        );
        match self.primary.complete_with_budget(messages, tools, bigger).await {
            Ok(again) => {
                if again.truncated {
                    tracing::warn!(to = bigger, "still cut off after the bigger budget");
                }
                Ok(again)
            }
            // The first reply is still something; keep it rather than fail.
            Err(e) => {
                tracing::warn!(error = %e, "bigger-budget retry failed; keeping the cut-off reply");
                Ok(r)
            }
        }
    }

    async fn complete_once(&self, messages: &[AiMessage], tools: &[ToolDef]) -> Result<AiResult, AiError> {
        // The cloud provider 500s on roughly 2 in 5 requests, transiently, and
        // a retry clears it: measured 27/27 successes across max_tokens
        // 400-1200 and temperature 0.0-0.9, so it is upstream noise rather
        // than anything in the request. Retrying once keeps a hiccup on a
        // capable model from silently degrading to the 2B, which is a far
        // worse answer than a second of waiting.
        let mut last = None;
        for attempt in 0..2 {
            match self.primary.complete(messages, tools).await {
                Ok(r) => {
                    self.record(Ok(()));
                    return Ok(r);
                }
                Err(e) if e.is_transient() => {
                    tracing::debug!(provider = %self.primary.name(), %e, attempt, "transient provider error; retrying");
                    last = Some(e);
                    tokio::time::sleep(std::time::Duration::from_millis(400 * (attempt + 1))).await;
                }
                Err(e) => {
                    // Not worth retrying (bad key, malformed request): go
                    // straight to the fallback.
                    let r = match &self.fallback {
                        Some(fb) => {
                            tracing::warn!(provider = %self.primary.name(), error = %e, "primary provider failed; using fallback");
                            fb.complete(messages, tools).await
                        }
                        None => Err(e),
                    };
                    self.record(r.as_ref().map(|_| ()).map_err(|x| x));
                    return r;
                }
            }
        }
        let e = last.expect("a transient error to have been recorded");
        let r = match &self.fallback {
            Some(fb) => {
                tracing::warn!(provider = %self.primary.name(), error = %e, "primary provider failed after retry; using fallback");
                fb.complete(messages, tools).await
            }
            None => Err(e),
        };
        self.record(r.as_ref().map(|_| ()).map_err(|x| x));
        r
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
            ProviderKind::Hermes => Box::new(OpenAiCompat::new(
                "hermes-proxy",
                config.hermes.base_url.clone(),
                config.hermes.model.clone(),
                // The proxy accepts any bearer and swaps in the real one.
                key_from_env(&config.hermes.api_key_env).or_else(|| Some("hermes".into())),
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
    Ok(ProviderSet {
        primary,
        fallback,
        phrasing,
        last: std::sync::Arc::new(std::sync::Mutex::new(LastCall::Never)),
        max_tokens: config.max_tokens,
    })
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
    fn default_config_builds_the_hermes_provider() {
        // The local model is gone. If this ever reads "local" again it means
        // the provider default drifted back to something Arc cannot start.
        let set = from_config(&arc_config::Ai::default()).unwrap();
        assert_eq!(set.primary_name(), "hermes-proxy");
    }

    #[test]
    fn a_config_naming_the_local_provider_is_rejected() {
        // The variant is gone, so a config still saying provider = "local"
        // must fail loudly rather than quietly fall back to a default and
        // leave the user wondering why Arc sounds like a different Arc.
        let bad = "[ai]\nprovider = \"local\"\n";
        assert!(arc_config::parse_str(bad).is_err(), "a config naming the local provider should be rejected");
    }

    #[test]
    fn hermes_as_fallback_is_ignored_when_it_is_already_primary() {
        // Otherwise Arc opens a second proxy connection to the same place.
        let cfg = arc_config::Ai {
            provider: arc_config::ProviderKind::Hermes,
            fallback: arc_config::ProviderKind::Hermes,
            ..Default::default()
        };
        let set = from_config(&cfg).unwrap();
        assert_eq!(set.primary_name(), "hermes-proxy");
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
                Ok(AiResult {
                    content: "fb".into(),
                    tool_calls: vec![],
                    reasoning: String::new(),
                    truncated: false,
                })
            }
        }
        let set = ProviderSet::new(Box::new(Fail), Some(Box::new(Ok_)));
        assert_eq!(set.complete(&[], &[]).await.unwrap().content, "fb");
    }

    /// Stops at max_tokens on the normal budget, answers on the bigger one.
    struct Thinker {
        budgets: std::sync::Arc<std::sync::Mutex<Vec<u32>>>,
    }
    #[async_trait]
    impl Provider for Thinker {
        fn name(&self) -> &str {
            "thinker"
        }
        async fn complete(&self, _: &[AiMessage], _: &[ToolDef]) -> Result<AiResult, AiError> {
            self.budgets.lock().unwrap().push(400);
            Ok(AiResult {
                content: String::new(),
                tool_calls: vec![],
                reasoning: "hmm".repeat(500),
                truncated: true,
            })
        }
        async fn complete_with_budget(
            &self,
            _: &[AiMessage],
            _: &[ToolDef],
            n: u32,
        ) -> Result<AiResult, AiError> {
            self.budgets.lock().unwrap().push(n);
            Ok(AiResult {
                content: "Here is the list.".into(),
                tool_calls: vec![],
                reasoning: String::new(),
                truncated: false,
            })
        }
    }

    #[tokio::test]
    async fn a_reply_cut_off_by_max_tokens_is_retried_once_with_more() {
        let budgets = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
        let set = ProviderSet::new(Box::new(Thinker { budgets: budgets.clone() }), None).with_max_tokens(400);
        let r = set.complete(&[AiMessage::user("list some tools")], &[]).await.unwrap();
        assert_eq!(r.content, "Here is the list.");
        assert_eq!(
            *budgets.lock().unwrap(),
            vec![400, RETRY_BUDGET_MIN],
            "one normal call, one bigger retry"
        );
    }

    #[test]
    fn cut_off_is_detected_from_finish_reason_or_broken_arguments() {
        let reply = |finish: &str, args: &str| {
            json!({"choices": [{"finish_reason": finish, "message": {"content": "", "tool_calls": [
                {"id": "a", "function": {"name": "tool_create", "arguments": args}}]}}]})
        };
        assert!(parse_openai(&reply("length", "{}")).unwrap().truncated);
        // Measured: finish_reason said "tool_calls" while the arguments were
        // cut mid-string at exactly max_tokens.
        assert!(
            parse_openai(&reply(
                "tool_calls",
                r##"{"name": "screenshot", "script": "#!/usr/bin/env bash\ngr"##
            ))
            .unwrap()
            .truncated
        );
        assert!(!parse_openai(&reply("tool_calls", r#"{"name": "x"}"#)).unwrap().truncated);
        assert!(!parse_openai(&reply("tool_calls", "")).unwrap().truncated);
        assert!(parse_anthropic(&json!({"stop_reason": "max_tokens", "content": []})).unwrap().truncated);
    }

    #[test]
    fn sampling_is_greedy_by_default() {
        // Not a style preference. At 0.4 this model answered "am I on wifi"
        // with "Yes, you are currently connected to the Wi-Fi network" without
        // calling the tool that was sitting in its list, and lost two of 18
        // tool calls against 0.0. Raising this default is how Arc starts
        // inventing facts about the user's machine.
        assert_eq!(Sampling::default().temperature, 0.0);
    }

    #[test]
    fn transient_provider_errors_are_retried_and_hard_ones_are_not() {
        // The cloud provider 500s intermittently; a retry clears it. A 401 means
        // the key is wrong and retrying only delays the same failure.
        assert!(AiError::Provider("status 500 Internal Server Error".into()).is_transient());
        assert!(AiError::Provider("status 503".into()).is_transient());
        assert!(AiError::Provider("status 429".into()).is_transient());
        assert!(AiError::Network("connection reset".into()).is_transient());
        assert!(AiError::Timeout.is_transient());
        assert!(!AiError::Provider("status 401 Unauthorized".into()).is_transient());
        assert!(!AiError::Provider("status 400 Bad Request".into()).is_transient());
        assert!(!AiError::Unconfigured.is_transient());
    }

    #[test]
    fn provider_health_reflects_real_traffic_not_a_probe() {
        // A fresh provider has done nothing, so nothing is known. Reporting
        // "ok" here would be a lie, and reporting anything else would be
        // guessing: the line is meant to describe the last real request.
        let cfg = arc_config::Ai { provider: arc_config::ProviderKind::Hermes, ..Default::default() };
        let set = from_config(&cfg).unwrap();
        assert_eq!(set.last_call(), LastCall::Never);
    }

    #[test]
    fn a_provider_error_is_trimmed_to_something_readable() {
        // A real 500 carries a whole JSON body, which is unreadable in a
        // two-column status table.
        let long = format!("status 500 Internal Server Error: {}", "x".repeat(400));
        let out = truncate(&long, 120);
        assert!(out.chars().count() <= 121, "{} chars", out.chars().count());
        assert!(out.ends_with('…'), "a trimmed error should say so: {out}");
        assert!(!out.contains("  "), "whitespace should be collapsed: {out}");
    }

    #[test]
    fn a_short_error_is_left_alone() {
        assert_eq!(truncate("timeout", 120), "timeout");
    }

    #[test]
    fn fallback_and_last_call_survive_a_second_completion() {
        // The record has to be written on every exit path, including the
        // retry-exhausted one, or a provider that is down reports itself ok
        // because only the happy path was instrumented.
        let set = from_config(&arc_config::Ai::default()).unwrap();
        assert_eq!(set.last_call(), LastCall::Never);
        assert!(set.primary_name().contains("hermes"));
    }

    /// The Arc app shows the model's reasoning; each provider family returns
    /// it under a different key, and a missing one must not be an error.
    #[test]
    fn reasoning_is_read_from_every_provider_shape() {
        let or =
            parse_openai(&json!({"choices": [{"message": {"content": "hi", "reasoning": " weighing it "}}]}))
                .unwrap();
        assert_eq!(or.reasoning, "weighing it");
        let ds =
            parse_openai(&json!({"choices": [{"message": {"content": "hi", "reasoning_content": "step"}}]}))
                .unwrap();
        assert_eq!(ds.reasoning, "step");
        let none = parse_openai(&json!({"choices": [{"message": {"content": "hi"}}]})).unwrap();
        assert_eq!(none.reasoning, "");
        let an = parse_anthropic(&json!({"content": [
            {"type": "thinking", "thinking": "first"},
            {"type": "text", "text": "answer"}
        ]}))
        .unwrap();
        assert_eq!((an.reasoning.as_str(), an.content.as_str()), ("first", "answer"));
    }
}
