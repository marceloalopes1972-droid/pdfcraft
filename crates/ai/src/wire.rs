//! The two wire formats: building requests and reading answers.
//!
//! The conversation is kept in the provider's own message format, so what the model sent comes
//! back to it unchanged on the next turn (Anthropic's thinking blocks and Gemini's thought
//! signatures must be returned as they were).

use serde_json::{Map, Value, json};

use crate::{AiError, Config, Format};

/// A tool the model may call.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema of the arguments (an object schema).
    pub schema: Value,
}

/// One tool call the model asked for.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
    /// Set when the arguments weren't valid JSON; the call is answered with this error.
    pub invalid: Option<String>,
}

/// Why the model stopped.
#[derive(Clone, Debug, PartialEq)]
pub enum Stop {
    /// It finished its answer.
    Done,
    /// It wants tool results.
    ToolUse,
    /// The answer reached the output limit.
    MaxTokens,
    /// It declined (with the provider's explanation).
    Refusal(String),
    /// Anything else, as the provider named it.
    Other(String),
}

/// One answer.
#[derive(Clone, Debug, PartialEq)]
pub struct Reply {
    pub text: String,
    pub calls: Vec<ToolCall>,
    pub stop: Stop,
    /// The assistant message to append to the conversation, in the provider's format.
    pub message: Value,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Anthropic's beta for server-side refusal fallbacks, and the models that take it.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const FALLBACK_MODELS: &[&str] = &["claude-fable-5-1", "claude-opus-5-5", "claude-opus-5", "claude-sonnet-5-5"];

fn uses_fallback(cfg: &Config) -> bool {
    cfg.provider.format() == Format::Anthropic
        && FALLBACK_MODELS.contains(&cfg.model.as_str())
        && cfg.base_url.trim_end_matches('/') == cfg.provider.base_url()
}

/// The URL requests go to.
pub fn endpoint(cfg: &Config) -> String {
    let base = cfg.base_url.trim().trim_end_matches('/');
    match cfg.provider.format() {
        Format::Anthropic => format!("{base}/messages"),
        Format::OpenAiChat => format!("{base}/chat/completions"),
    }
}

/// The request headers (the key included; never log them).
pub fn headers(cfg: &Config) -> Vec<(String, String)> {
    let mut h = vec![("content-type".to_string(), "application/json".to_string())];
    match cfg.provider.format() {
        Format::Anthropic => {
            h.push(("x-api-key".into(), cfg.api_key.clone()));
            h.push(("anthropic-version".into(), "2023-06-01".into()));
            if uses_fallback(cfg) {
                h.push(("anthropic-beta".into(), FALLBACK_BETA.into()));
            }
        }
        Format::OpenAiChat => h.push(("authorization".into(), format!("Bearer {}", cfg.api_key))),
    }
    h
}

/// A user message with `text`.
pub fn user_message(format: Format, text: &str) -> Value {
    match format {
        Format::Anthropic => json!({ "role": "user", "content": [{ "type": "text", "text": text }] }),
        Format::OpenAiChat => json!({ "role": "user", "content": text }),
    }
}

/// The messages answering tool calls: `(call id, result text, is an error)`.
pub fn tool_results(format: Format, results: &[(String, String, bool)]) -> Vec<Value> {
    match format {
        Format::Anthropic => {
            let blocks: Vec<Value> =
                results.iter().map(|(id, text, err)| json!({ "type": "tool_result", "tool_use_id": id, "content": text, "is_error": err })).collect();
            vec![json!({ "role": "user", "content": blocks })]
        }
        Format::OpenAiChat => results
            .iter()
            .map(|(id, text, err)| json!({ "role": "tool", "tool_call_id": id, "content": if *err { format!("Error: {text}") } else { text.clone() } }))
            .collect(),
    }
}

/// Keep the parts of a JSON Schema every OpenAI-compatible endpoint accepts (Gemini's is the
/// strictest): `type` (a single type), `properties`, `items`, `required`, `description`, `enum`,
/// `minimum`, `maximum`. Arguments are still checked against the full schema when the tool runs.
pub fn portable_schema(schema: &Value) -> Value {
    fn walk(v: &Value, depth: usize) -> Value {
        let Some(o) = v.as_object() else { return json!({ "type": "string" }) };
        if depth > 16 {
            return json!({ "type": "object" });
        }
        let mut out = Map::new();
        let ty = match o.get("type") {
            Some(Value::String(t)) => Some(t.clone()),
            Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).find(|t| *t != "null").map(str::to_string),
            _ => None,
        };
        let ty = ty.unwrap_or_else(|| if o.contains_key("properties") { "object".into() } else { "string".into() });
        out.insert("type".into(), Value::String(ty.clone()));
        for key in ["description", "enum", "minimum", "maximum"] {
            if let Some(x) = o.get(key) {
                out.insert(key.into(), x.clone());
            }
        }
        if ty == "object"
            && let Some(props) = o.get("properties").and_then(Value::as_object).filter(|p| !p.is_empty())
        {
            out.insert("properties".into(), Value::Object(props.iter().map(|(k, p)| (k.clone(), walk(p, depth + 1))).collect()));
            if let Some(req) = o.get("required").and_then(Value::as_array) {
                let req: Vec<Value> = req.iter().filter(|r| r.as_str().is_some_and(|r| props.contains_key(r))).cloned().collect();
                if !req.is_empty() {
                    out.insert("required".into(), Value::Array(req));
                }
            }
        }
        if ty == "array" {
            out.insert("items".into(), o.get("items").map_or_else(|| json!({ "type": "string" }), |i| walk(i, depth + 1)));
        }
        Value::Object(out)
    }
    walk(schema, 0)
}

/// The request body.
pub fn body(cfg: &Config, system: &str, messages: &[Value], tools: &[ToolSpec]) -> Value {
    match cfg.provider.format() {
        Format::Anthropic => {
            let mut b = json!({ "model": cfg.model, "max_tokens": cfg.max_output_tokens, "system": system, "messages": messages });
            if !tools.is_empty() {
                let mut list: Vec<Value> =
                    tools.iter().map(|t| json!({ "name": t.name, "description": t.description, "input_schema": t.schema })).collect();
                // The tool list is the same on every turn of a run: cache it.
                if let Some(last) = list.last_mut() {
                    last["cache_control"] = json!({ "type": "ephemeral" });
                }
                b["tools"] = Value::Array(list);
            }
            if uses_fallback(cfg) {
                b["fallbacks"] = json!("default");
            }
            b
        }
        Format::OpenAiChat => {
            let mut all = vec![json!({ "role": "system", "content": system })];
            all.extend(messages.iter().cloned());
            let mut b = json!({ "model": cfg.model, "messages": all });
            if !tools.is_empty() {
                b["tools"] = tools
                    .iter()
                    .map(|t| json!({ "type": "function", "function": { "name": t.name, "description": t.description, "parameters": portable_schema(&t.schema) } }))
                    .collect();
            }
            b
        }
    }
}

/// The error message in an error answer (`{"error": {"message": …}}`, Google's list form, or a
/// plain string), shortened.
pub fn error_message(v: &Value) -> Option<String> {
    let e = v.get("error").or_else(|| v.as_array().and_then(|a| a.first()).and_then(|x| x.get("error")))?;
    let m = e.get("message").and_then(Value::as_str).or_else(|| e.as_str())?;
    Some(crate::clip(m, 500))
}

fn tokens(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// Read an answer.
pub fn parse(format: Format, v: &Value) -> Result<Reply, AiError> {
    if let Some(m) = error_message(v) {
        return Err(AiError::Response(m));
    }
    match format {
        Format::Anthropic => {
            let content = v.get("content").and_then(Value::as_array).ok_or_else(|| AiError::Response("no content in the answer".into()))?;
            let mut text = String::new();
            let mut calls = Vec::new();
            for block in content {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => text.push_str(block.get("text").and_then(Value::as_str).unwrap_or("")),
                    Some("tool_use") => calls.push(ToolCall {
                        id: block.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
                        name: block.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                        input: block.get("input").cloned().unwrap_or_else(|| json!({})),
                        invalid: None,
                    }),
                    _ => {}
                }
            }
            let stop = match v.get("stop_reason").and_then(Value::as_str) {
                Some("end_turn" | "stop_sequence") | None => Stop::Done,
                Some("tool_use") => Stop::ToolUse,
                Some("max_tokens") => Stop::MaxTokens,
                Some("refusal") => {
                    let why = v
                        .pointer("/stop_details/explanation")
                        .and_then(Value::as_str)
                        .or_else(|| v.pointer("/stop_details/category").and_then(Value::as_str));
                    Stop::Refusal(why.unwrap_or("no reason given").to_string())
                }
                Some(other) => Stop::Other(other.to_string()),
            };
            let stop = if stop == Stop::Done && !calls.is_empty() { Stop::ToolUse } else { stop };
            let usage = v.get("usage").cloned().unwrap_or(Value::Null);
            Ok(Reply {
                text,
                calls,
                stop,
                message: json!({ "role": "assistant", "content": content }),
                input_tokens: tokens(&usage, "input_tokens")
                    + tokens(&usage, "cache_read_input_tokens")
                    + tokens(&usage, "cache_creation_input_tokens"),
                output_tokens: tokens(&usage, "output_tokens"),
            })
        }
        Format::OpenAiChat => {
            let choice = v
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|c| c.first())
                .ok_or_else(|| AiError::Response("no choices in the answer".into()))?;
            let msg = choice.get("message").ok_or_else(|| AiError::Response("no message in the answer".into()))?;
            let text = match msg.get("content") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(parts)) => parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join(""),
                _ => String::new(),
            };
            let raw_calls: Vec<Value> = msg.get("tool_calls").and_then(Value::as_array).cloned().unwrap_or_default();
            let mut calls = Vec::new();
            let mut replay = Vec::new();
            for (i, c) in raw_calls.iter().enumerate() {
                let id = c.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()).map_or_else(|| format!("call_{}", i + 1), str::to_string);
                let name = c.pointer("/function/name").and_then(Value::as_str).unwrap_or("").to_string();
                let args = c.pointer("/function/arguments");
                let (input, invalid) = match args {
                    Some(Value::String(s)) if s.trim().is_empty() => (json!({}), None),
                    Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
                        Ok(v) => (v, None),
                        Err(e) => (json!({}), Some(format!("the arguments were not valid JSON ({e})"))),
                    },
                    Some(v @ Value::Object(_)) => (v.clone(), None),
                    _ => (json!({}), None),
                };
                // Returned as received (Gemini's thought signatures ride along), with the id we use.
                let mut r = c.clone();
                r["id"] = Value::String(id.clone());
                replay.push(r);
                calls.push(ToolCall { id, name, input, invalid });
            }
            let stop = match choice.get("finish_reason").and_then(Value::as_str) {
                _ if !calls.is_empty() => Stop::ToolUse,
                Some("stop") | None => Stop::Done,
                Some("tool_calls" | "function_call") => Stop::ToolUse,
                Some("length") => Stop::MaxTokens,
                Some("content_filter") => Stop::Refusal("the provider's content filter stopped the answer".into()),
                Some(other) => Stop::Other(other.to_string()),
            };
            let mut message = json!({ "role": "assistant", "content": text });
            if !replay.is_empty() {
                message["tool_calls"] = Value::Array(replay);
            }
            let usage = v.get("usage").cloned().unwrap_or(Value::Null);
            Ok(Reply {
                text,
                calls,
                stop,
                message,
                input_tokens: tokens(&usage, "prompt_tokens"),
                output_tokens: tokens(&usage, "completion_tokens"),
            })
        }
    }
}
