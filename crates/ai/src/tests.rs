use std::cell::RefCell;

use serde_json::{Value, json};

use crate::tasks::{self, Cell, Workspace};
use crate::wire::{self, Stop};
use crate::*;

/// One recorded request: URL, headers and body.
type Seen = (String, Vec<(String, String)>, Value);

/// Answers with queued JSON and records every request.
#[derive(Default)]
struct Fake {
    answers: RefCell<Vec<Result<Value, AiError>>>,
    seen: RefCell<Vec<Seen>>,
}

impl Fake {
    fn with(answers: Vec<Value>) -> Self {
        Self { answers: RefCell::new(answers.into_iter().rev().map(Ok).collect()), ..Self::default() }
    }
    fn body(&self, i: usize) -> Value {
        self.seen.borrow()[i].2.clone()
    }
}

impl Transport for Fake {
    fn post(&self, url: &str, headers: &[(String, String)], body: &Value) -> Result<Value, AiError> {
        self.seen.borrow_mut().push((url.to_string(), headers.to_vec(), body.clone()));
        self.answers.borrow_mut().pop().unwrap_or_else(|| Err(AiError::Network("no more answers".into())))
    }
}

fn openai_text(text: &str) -> Value {
    json!({ "choices": [{ "message": { "role": "assistant", "content": text }, "finish_reason": "stop" }], "usage": { "prompt_tokens": 10, "completion_tokens": 5 } })
}

fn anthropic_text(text: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "stop_reason": "end_turn", "usage": { "input_tokens": 7, "output_tokens": 3 } })
}

fn pages() -> Vec<(usize, String)> {
    vec![(1, "Receita total: 1.234,56".into()), (2, "Despesa: 100,00".into())]
}

#[test]
fn providers_parse_by_id_and_common_name() {
    for p in Provider::ALL {
        assert_eq!(Provider::parse(p.id()), Some(p));
        assert!(!p.default_model().is_empty() && p.base_url().starts_with("https://"));
    }
    assert_eq!(Provider::parse(" Claude "), Some(Provider::Anthropic));
    assert_eq!(Provider::parse("ChatGPT"), Some(Provider::OpenAi));
    assert_eq!(Provider::parse("xai"), Some(Provider::Grok));
    assert_eq!(Provider::parse("google"), Some(Provider::Gemini));
    assert_eq!(Provider::parse("other"), None);
    assert_eq!(Provider::Anthropic.format(), Format::Anthropic);
    for p in [Provider::OpenAi, Provider::DeepSeek, Provider::Grok, Provider::Gemini] {
        assert_eq!(p.format(), Format::OpenAiChat);
    }
}

#[test]
fn settings_that_cannot_work_or_would_leak_the_key_are_refused() {
    assert!(Config::new(Provider::OpenAi, "sk-abc").validate().is_ok());
    assert!(Config::new(Provider::OpenAi, "  ").validate().is_err());
    assert!(Config::new(Provider::OpenAi, "sk-a\r\nX-Evil: 1").validate().is_err(), "header injection");
    assert!(Config::new(Provider::OpenAi, "sk-ção").validate().is_err());
    let mut c = Config::new(Provider::Gemini, "k");
    c.base_url = "http://example.com/v1".into();
    assert!(c.validate().is_err(), "plain HTTP off this machine");
    c.base_url = "https://".into();
    assert!(c.validate().is_err());
    for local in ["http://localhost:11434/v1", "http://127.0.0.1", "http://[::1]:8080/v1"] {
        c.base_url = local.into();
        assert!(c.validate().is_ok(), "{local}");
    }
    c.base_url = "http://localhost.evil.com/v1".into();
    assert!(c.validate().is_err());
    c.base_url = "https://gateway.example/v1".into();
    c.model = String::new();
    assert!(c.validate().is_err());
    let shown = format!("{:?}", Config::new(Provider::Anthropic, "sk-ant-secret"));
    assert!(!shown.contains("secret") && shown.contains("redacted"), "{shown}");
}

#[test]
fn anthropic_requests_use_the_messages_api_with_fallbacks() {
    let cfg = Config::new(Provider::Anthropic, "sk-ant-x");
    assert_eq!(wire::endpoint(&cfg), "https://api.anthropic.com/v1/messages");
    let h = wire::headers(&cfg);
    assert!(h.contains(&("x-api-key".into(), "sk-ant-x".into())));
    assert!(h.contains(&("anthropic-version".into(), "2023-06-01".into())));
    assert!(h.iter().any(|(k, v)| k == "anthropic-beta" && v.starts_with("server-side-fallback")));
    let tool = ToolSpec {
        name: "page_rotate".into(),
        description: "Rotate".into(),
        schema: json!({ "type": "object", "properties": { "doc": { "type": "integer" } } }),
    };
    let b = wire::body(&cfg, "sys", &[wire::user_message(Format::Anthropic, "hi")], &[tool]);
    assert_eq!(b["model"], "claude-opus-5-5");
    assert_eq!(b["system"], "sys");
    assert_eq!(b["fallbacks"], "default");
    assert_eq!(b["tools"][0]["input_schema"]["properties"]["doc"]["type"], "integer");
    assert_eq!(b["tools"][0]["cache_control"]["type"], "ephemeral", "the tool list is cached");
    assert!(b.get("thinking").is_none(), "Opus 5.5 thinks adaptively by default");
    // Another model or a gateway gets no fallback beta.
    let mut other = cfg.clone();
    other.base_url = "https://gateway.example/v1".into();
    assert!(wire::body(&other, "s", &[], &[]).get("fallbacks").is_none());
    assert!(!wire::headers(&other).iter().any(|(k, _)| k == "anthropic-beta"));
}

#[test]
fn openai_compatible_requests_share_one_format() {
    for (p, url) in [
        (Provider::OpenAi, "https://api.openai.com/v1/chat/completions"),
        (Provider::DeepSeek, "https://api.deepseek.com/v1/chat/completions"),
        (Provider::Grok, "https://api.x.ai/v1/chat/completions"),
        (Provider::Gemini, "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions"),
    ] {
        let cfg = Config::new(p, "key");
        assert_eq!(wire::endpoint(&cfg), url);
        assert!(wire::headers(&cfg).contains(&("authorization".into(), "Bearer key".into())));
        let b = wire::body(&cfg, "sys", &[wire::user_message(Format::OpenAiChat, "hi")], &[]);
        assert_eq!(b["messages"][0], json!({ "role": "system", "content": "sys" }));
        assert_eq!(b["messages"][1], json!({ "role": "user", "content": "hi" }));
        assert!(b.get("tools").is_none());
    }
}

#[test]
fn schemas_are_reduced_to_what_every_endpoint_accepts() {
    let s = json!({
        "type": "object", "additionalProperties": false, "$schema": "x",
        "properties": {
            "doc": { "type": "integer", "minimum": 1, "description": "id" },
            "pages": { "type": "array", "items": { "type": "integer", "minimum": 1 }, "minItems": 1 },
            "mode": { "type": ["string", "null"], "enum": ["a", "b"] },
            "any": { "oneOf": [{ "type": "string" }, { "type": "number" }] },
            "opts": { "type": "object", "properties": {} }
        },
        "required": ["doc", "missing"]
    });
    let p = wire::portable_schema(&s);
    assert!(p.get("additionalProperties").is_none() && p.get("$schema").is_none());
    assert_eq!(p["required"], json!(["doc"]));
    assert_eq!(p["properties"]["doc"], json!({ "type": "integer", "minimum": 1, "description": "id" }));
    assert_eq!(p["properties"]["pages"]["items"]["type"], "integer");
    assert!(p["properties"]["pages"].get("minItems").is_none());
    assert_eq!(p["properties"]["mode"]["type"], "string");
    assert_eq!(p["properties"]["any"]["type"], "string");
    assert!(p["properties"]["opts"].get("properties").is_none(), "Gemini refuses empty properties");
    // Deeply nested or odd schemas don't recurse without bound or panic.
    let mut deep = json!({ "type": "string" });
    for _ in 0..200 {
        deep = json!({ "type": "object", "properties": { "x": deep } });
    }
    let _ = wire::portable_schema(&deep);
    assert_eq!(wire::portable_schema(&json!(5)), json!({ "type": "string" }));
}

#[test]
fn anthropic_answers_are_read_with_tool_calls_and_refusals() {
    let v = json!({
        "content": [
            { "type": "thinking", "thinking": "", "signature": "sig" },
            { "type": "text", "text": "Rotating." },
            { "type": "tool_use", "id": "toolu_1", "name": "page_rotate", "input": { "doc": 1, "degrees": 90 } }
        ],
        "stop_reason": "tool_use",
        "usage": { "input_tokens": 100, "cache_read_input_tokens": 20, "output_tokens": 30 }
    });
    let r = wire::parse(Format::Anthropic, &v).unwrap();
    assert_eq!(r.text, "Rotating.");
    assert_eq!(r.stop, Stop::ToolUse);
    assert_eq!(r.calls[0].name, "page_rotate");
    assert_eq!(r.calls[0].input["degrees"], 90);
    assert_eq!((r.input_tokens, r.output_tokens), (120, 30));
    assert_eq!(r.message["content"][0]["signature"], "sig", "thinking blocks are replayed unchanged");
    let refused =
        json!({ "content": [], "stop_reason": "refusal", "stop_details": { "type": "refusal", "category": "cyber", "explanation": "not allowed" } });
    assert_eq!(wire::parse(Format::Anthropic, &refused).unwrap().stop, Stop::Refusal("not allowed".into()));
    let err = json!({ "type": "error", "error": { "type": "invalid_request_error", "message": "bad model" } });
    assert_eq!(wire::parse(Format::Anthropic, &err), Err(AiError::Response("bad model".into())));
    assert!(wire::parse(Format::Anthropic, &json!({ "nothing": 1 })).is_err());
}

#[test]
fn openai_answers_are_read_and_bad_arguments_are_flagged() {
    let v = json!({ "choices": [{ "message": { "role": "assistant", "content": null, "tool_calls": [
        { "id": "call_a", "type": "function", "function": { "name": "text_extract", "arguments": "{\"doc\":1}" }, "extra_content": { "google": { "thought_signature": "t" } } },
        { "type": "function", "function": { "name": "page_rotate", "arguments": "{oops" } }
    ] }, "finish_reason": "tool_calls" }] });
    let r = wire::parse(Format::OpenAiChat, &v).unwrap();
    assert_eq!(r.stop, Stop::ToolUse);
    assert_eq!(r.calls[0].input, json!({ "doc": 1 }));
    assert_eq!(r.calls[1].id, "call_2", "a missing id is filled in");
    assert!(r.calls[1].invalid.is_some());
    assert_eq!(r.message["tool_calls"][0]["extra_content"]["google"]["thought_signature"], "t");
    assert_eq!(r.message["tool_calls"][1]["id"], "call_2");
    let filtered = json!({ "choices": [{ "message": { "content": "" }, "finish_reason": "content_filter" }] });
    assert!(matches!(wire::parse(Format::OpenAiChat, &filtered).unwrap().stop, Stop::Refusal(_)));
    let cut = json!({ "choices": [{ "message": { "content": "part" }, "finish_reason": "length" }] });
    assert_eq!(wire::parse(Format::OpenAiChat, &cut).unwrap().stop, Stop::MaxTokens);
    let parts = json!({ "choices": [{ "message": { "content": [{ "type": "text", "text": "a" }, { "type": "text", "text": "b" }] } }] });
    assert_eq!(wire::parse(Format::OpenAiChat, &parts).unwrap().text, "ab");
    assert!(wire::parse(Format::OpenAiChat, &json!({ "choices": [] })).is_err());
    assert_eq!(wire::error_message(&json!([{ "error": { "code": 400, "message": "API key not valid" } }])), Some("API key not valid".into()));
}

#[test]
fn tool_results_follow_each_format() {
    let r = vec![("id1".to_string(), "ok".to_string(), false), ("id2".to_string(), "boom".to_string(), true)];
    let a = wire::tool_results(Format::Anthropic, &r);
    assert_eq!(a.len(), 1, "all results in one user message");
    assert_eq!(a[0]["content"][1], json!({ "type": "tool_result", "tool_use_id": "id2", "content": "boom", "is_error": true }));
    let o = wire::tool_results(Format::OpenAiChat, &r);
    assert_eq!(o.len(), 2);
    assert_eq!(o[1], json!({ "role": "tool", "tool_call_id": "id2", "content": "Error: boom" }));
}

#[test]
fn a_single_answer_counts_tokens_and_refuses_partial_answers() {
    let fake = Fake::with(vec![anthropic_text("Resumo."), json!({ "content": [{ "type": "text", "text": "cut" }], "stop_reason": "max_tokens" })]);
    let a = Assistant::new(Config::new(Provider::Anthropic, "k"), &fake).unwrap();
    assert_eq!(a.answer("s", "u").unwrap(), "Resumo.");
    assert_eq!(a.usage(), Usage { requests: 1, input_tokens: 7, output_tokens: 3 });
    assert!(matches!(a.answer("s", "u"), Err(AiError::Response(_))));
    assert!(Assistant::new(Config::new(Provider::Anthropic, ""), &fake).is_err());
}

#[test]
fn the_tool_loop_runs_calls_and_returns_results_to_the_model() {
    let call = |id: &str, name: &str, args: &str| json!({ "choices": [{ "message": { "content": "", "tool_calls": [{ "id": id, "type": "function", "function": { "name": name, "arguments": args } }] }, "finish_reason": "tool_calls" }] });
    let fake = Fake::with(vec![call("c1", "page_rotate", "{\"doc\":1,\"degrees\":90}"), call("c2", "rm_rf", "{}"), openai_text("Rotated page 1.")]);
    let a = Assistant::new(Config::new(Provider::DeepSeek, "k"), &fake).unwrap();
    let tools = vec![ToolSpec { name: "page_rotate".into(), description: "Rotate pages".into(), schema: json!({ "type": "object" }) }];
    let mut ran = Vec::new();
    let mut logged = 0;
    let out = a
        .run_tools(
            "sys",
            "rotate page 1",
            &tools,
            10,
            &mut |c| {
                ran.push(c.input.clone());
                Ok("{\"pages\":3}".into())
            },
            &mut |_| logged += 1,
        )
        .unwrap();
    assert!(out.finished);
    assert_eq!(out.text, "Rotated page 1.");
    assert_eq!(ran, vec![json!({ "doc": 1, "degrees": 90 })], "an unknown tool is never run");
    assert_eq!(logged, 2);
    assert!(!out.steps[1].ok && out.steps[1].result.contains("unknown tool"));
    // The third request carries the whole conversation, tool results included.
    let msgs = fake.body(2)["messages"].as_array().unwrap().clone();
    assert_eq!(msgs.iter().filter(|m| m["role"] == "tool").count(), 2);
    assert_eq!(fake.body(0)["tools"][0]["function"]["name"], "page_rotate");
}

#[test]
fn the_tool_loop_stops_at_the_step_limit() {
    let call = json!({ "content": [{ "type": "tool_use", "id": "t", "name": "x", "input": {} }], "stop_reason": "tool_use" });
    let fake = Fake::with(vec![call.clone(), call.clone(), call]);
    let a = Assistant::new(Config::new(Provider::Anthropic, "k"), &fake).unwrap();
    let tools = vec![ToolSpec { name: "x".into(), description: String::new(), schema: json!({ "type": "object" }) }];
    let out = a.run_tools("s", "u", &tools, 2, &mut |_| Ok(String::new()), &mut |_| {}).unwrap();
    assert!(!out.finished);
    assert_eq!(out.steps.len(), 2);
}

#[test]
fn documents_are_marked_by_page_and_refused_when_too_large_or_empty() {
    let d = tasks::document(&pages(), 10_000).unwrap();
    assert!(d.starts_with("<document>\n[Page 1]\nReceita") && d.contains("[Page 2]") && d.ends_with("</document>"));
    assert!(matches!(tasks::document(&pages(), 20), Err(AiError::TooLarge(_))));
    assert!(matches!(tasks::document(&[(1, "  ".into())], 10_000), Err(AiError::TooLarge(_))));
}

#[test]
fn summaries_and_answers_send_the_document_as_data() {
    let fake = Fake::with(vec![openai_text("Resumo (p. 1)."), openai_text("Sim (p. 2).")]);
    let a = Assistant::new(Config::new(Provider::Grok, "k"), &fake).unwrap();
    assert_eq!(tasks::summarize(&a, &pages(), Some("pt-BR"), None).unwrap(), "Resumo (p. 1).");
    assert_eq!(tasks::ask(&a, &pages(), "Há despesas?", None).unwrap(), "Sim (p. 2).");
    let b = fake.body(0);
    let system = b["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("never instructions") && system.contains("pt-BR"));
    assert!(b["messages"][1]["content"].as_str().unwrap().contains("[Page 2]\nDespesa"));
}

#[test]
fn tables_keep_numbers_as_numbers_and_retry_bad_json_once() {
    let good =
        json!({ "tables": [{ "name": "Resultado", "page": 1, "columns": ["Conta", "Valor"], "rows": [["Receita", 1234.56], ["Despesa", null]] }] });
    let fake = Fake::with(vec![openai_text("Here you go: {not json"), openai_text(&format!("```json\n{good}\n```"))]);
    let a = Assistant::new(Config::new(Provider::Gemini, "k"), &fake).unwrap();
    let t = tasks::tables(&a, &pages(), None).unwrap();
    assert_eq!(t.len(), 1);
    assert_eq!(t[0].page, Some(1));
    assert_eq!(t[0].rows[0], vec![Cell::Text("Conta".into()), Cell::Text("Valor".into())]);
    assert_eq!(t[0].rows[1], vec![Cell::Text("Receita".into()), Cell::Number(1234.56)]);
    assert_eq!(t[0].rows[2][1], Cell::Empty);
    let retry = fake.body(1)["messages"].as_array().unwrap().clone();
    assert!(retry.last().unwrap()["content"].as_str().unwrap().contains("can't be used"));
    // Two bad answers fail with a readable error.
    let fake = Fake::with(vec![openai_text("no"), openai_text("still no")]);
    let a = Assistant::new(Config::new(Provider::Gemini, "k"), &fake).unwrap();
    assert!(matches!(tasks::tables(&a, &pages(), None), Err(AiError::Response(_))));
}

#[test]
fn table_and_deck_answers_are_checked() {
    assert!(tasks::read_tables(&json!({})).is_err());
    assert!(tasks::read_tables(&json!({ "tables": [{ "rows": [5] }] })).is_err());
    assert_eq!(tasks::read_tables(&json!({ "tables": [] })).unwrap(), vec![]);
    let t = tasks::read_tables(&json!({ "tables": [{ "rows": [[true, {"a": 1}, 1e308, -0.5]] }] })).unwrap();
    assert_eq!(t[0].name, "Table 1");
    assert_eq!(t[0].rows[0][0], Cell::Text("true".into()));
    assert_eq!(t[0].rows[0][3], Cell::Number(-0.5));
    assert!(tasks::read_deck(&json!({ "slides": [] })).is_err());
    let d = tasks::read_deck(&json!({ "title": "T", "slides": [{ "title": "A\u{0}", "bullets": ["x", "", 3] }] })).unwrap();
    assert_eq!(d.slides[0].title, "A");
    assert_eq!(d.slides[0].bullets, vec!["x".to_string()]);
    assert_eq!(tasks::json_object("prefix {\"a\": 1} suffix").unwrap(), json!({ "a": 1 }));
    assert!(tasks::json_object("[1, 2]").is_err());
    assert!(tasks::json_object("} {").is_err());
}

#[test]
fn slides_are_outlined_from_the_document() {
    let deck = json!({ "title": "Relatório", "subtitle": "2026", "slides": [{ "title": "Receitas", "bullets": ["R$ 1.234,56 (p. 1)"] }] });
    let fake = Fake::with(vec![anthropic_text(&deck.to_string())]);
    let a = Assistant::new(Config::new(Provider::Anthropic, "k"), &fake).unwrap();
    let d = tasks::slides(&a, &pages(), Some(5), None, Some("diretoria")).unwrap();
    assert_eq!(d.title, "Relatório");
    assert_eq!(d.slides.len(), 1);
    let ask = fake.body(0)["messages"][0]["content"][0]["text"].as_str().unwrap().to_string();
    assert!(ask.contains("diretoria"));
    assert!(fake.body(0)["system"].as_str().unwrap().contains("5 content slides"));
}

struct Doc {
    calls: Vec<String>,
}

impl Workspace for Doc {
    fn tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec { name: "page_delete".into(), description: "Delete pages".into(), schema: json!({ "type": "object" }) }]
    }
    fn call(&mut self, call: &ToolCall) -> Result<String, String> {
        self.calls.push(call.name.clone());
        Err("page 9 doesn't exist".into())
    }
}

#[test]
fn commands_run_through_the_workspace_and_errors_reach_the_model() {
    let fake = Fake::with(vec![
        json!({ "content": [{ "type": "tool_use", "id": "t1", "name": "page_delete", "input": { "doc": 1, "pages": [9] } }], "stop_reason": "tool_use" }),
        anthropic_text("Page 9 doesn't exist; nothing was deleted."),
    ]);
    let a = Assistant::new(Config::new(Provider::Anthropic, "k"), &fake).unwrap();
    let mut ws = Doc { calls: vec![] };
    let out = tasks::command(&a, &mut ws, 1, 3, "apague a página 9", 20, &mut |_| {}).unwrap();
    assert_eq!(ws.calls, vec!["page_delete"]);
    assert!(out.finished && out.text.contains("nothing was deleted"));
    let second = fake.body(1);
    let result = &second["messages"][2]["content"][0];
    assert_eq!(result["is_error"], true);
    assert!(fake.body(0)["system"].as_str().unwrap().contains("document id 1 (3 pages)"));
}

#[test]
fn clipping_respects_character_boundaries() {
    assert_eq!(clip("ação", 10), "ação");
    let c = clip("ççççç", 2);
    assert!(c.starts_with("çç…") && c.contains("3 more"));
}

#[test]
fn page_lists_are_checked() {
    assert_eq!(tasks::page_list("", 3).unwrap(), vec![1, 2, 3]);
    assert_eq!(tasks::page_list("3,1-2,2", 5).unwrap(), vec![3, 1, 2]);
    for bad in ["0", "4-2", "6", "a", ",", "1-18446744073709551615", "2-"] {
        assert!(tasks::page_list(bad, 5).is_err(), "{bad}");
    }
}

#[test]
fn translation_needs_a_language_and_keeps_page_markers() {
    let fake = Fake::with(vec![openai_text("[Page 1]\nTotal revenue: 1,234.56")]);
    let a = Assistant::new(Config::new(Provider::OpenAi, "k"), &fake).unwrap();
    assert!(matches!(tasks::translate(&a, &pages(), " "), Err(AiError::Config(_))));
    assert!(tasks::translate(&a, &pages(), "English").unwrap().starts_with("[Page 1]"));
    assert!(fake.body(0)["messages"][0]["content"].as_str().unwrap().contains("into English"));
}

#[test]
fn a_cancelled_run_sends_nothing_more() {
    let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let fake = Fake::with(vec![openai_text("x")]);
    let a = Assistant::new(Config::new(Provider::OpenAi, "k"), &fake).unwrap().with_cancel(flag);
    assert_eq!(a.answer("s", "u"), Err(AiError::Cancelled));
    assert!(fake.seen.borrow().is_empty());
}
