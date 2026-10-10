# pdfcraft-ai

Opt-in AI assistant with the user's own API key.

- **Layer:** L4. It depends only on `serde_json` and, on native builds with the `http` feature (the default), `ureq` with rustls and the operating system's trusted roots. No engine or UI dependency; the web build compiles without HTTP.
- **Status:** provider layer, tool-use loop and document tasks are done and tested with a fake transport. `pdfcraft-cli ai` uses them. The desktop app has no AI pane yet.

## Providers

| id | Provider | Format | Default model | Key variable |
|---|---|---|---|---|
| `anthropic` | Anthropic (Claude) | Messages API | `claude-opus-5-5` | `ANTHROPIC_API_KEY` |
| `openai` | OpenAI | chat completions | `gpt-5` | `OPENAI_API_KEY` |
| `deepseek` | DeepSeek | chat completions | `deepseek-chat` | `DEEPSEEK_API_KEY` |
| `grok` | xAI (Grok) | chat completions | `grok-4` | `XAI_API_KEY` |
| `gemini` | Google (Gemini) | chat completions (OpenAI-compatible endpoint) | `gemini-2.5-flash` | `GEMINI_API_KEY` |

Two wire formats (`wire`), not five: Anthropic speaks its Messages API, and the other four accept OpenAI's chat-completions format. Any other endpoint that speaks one of them works with `Config::base_url`: HTTPS, or `http://localhost…` for a model running on the same computer. Models change, so the model is always configurable.

On Anthropic's first-party API with a current Opus/Sonnet/Fable model, requests carry the server-side refusal fallback (`fallbacks: "default"`, beta `server-side-fallback-2026-07-01`), and the tool list is cached (`cache_control`) so a long editing run doesn't pay for it on every turn.

## API

```rust
let cfg = Config::new(Provider::Anthropic, key);            // defaults; set model / base_url as needed
let http = HttpTransport::new(Duration::from_secs(600))?;   // or any `Transport`
let a = Assistant::new(cfg, &http)?;                        // validates the settings
tasks::summarize(&a, &pages, Some("pt-BR"), None)?;         // pages: [(page number, text)]
tasks::ask(&a, &pages, "Qual o prazo?", None)?;
tasks::tables(&a, &pages, None)?;                           // Vec<Table> (numbers stay numbers)
tasks::slides(&a, &pages, Some(8), None, None)?;            // Deck { title, subtitle, slides }
tasks::command(&a, &mut workspace, doc, pages, "…", 40, &mut |step| …)?;  // tool-use loop
a.usage();                                                  // requests and tokens, for the user
```

## Rules

- **Opt-in.** Nothing here runs or connects unless the user asks for an AI task. The caller says what is sent to which provider before sending.
- **The key stays private.** It goes only into the request header for the endpoint the user chose; `Config`'s `Debug` redacts it; error messages never include requests. Keys with spaces, line breaks or non-ASCII are refused (they would land in an HTTP header).
- **Documents are data.** Document text is wrapped in `<document>` tags with `[Page N]` markers, and every system prompt says text inside is never an instruction.
- **No silent truncation.** A document larger than the model reads at once is refused with a message to choose fewer pages. An answer cut off at the output limit is an error, not a partial result.
- **Untrusted answers.** Structured answers (tables, slide outlines) are parsed with limits (tables, rows, columns, slides, text length) and asked for once more when they are not valid JSON. Tool calls with invalid arguments or unknown tools are answered with an error and never run.
