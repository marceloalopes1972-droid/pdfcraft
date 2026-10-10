//! One model, its settings and a transport: single answers and the tool-use loop.

use serde_json::Value;

use crate::wire::{self, Reply, Stop, ToolCall, ToolSpec};
use crate::{AiError, Config, Transport};

/// Tokens used, summed over every request (what the provider bills).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub requests: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// One tool call made during [`Assistant::run_tools`], for the user's log.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub tool: String,
    pub input: Value,
    pub ok: bool,
    /// The result or error, shortened.
    pub result: String,
}

/// The end of a tool-use run.
#[derive(Clone, Debug, PartialEq)]
pub struct Outcome {
    /// The model's closing message.
    pub text: String,
    pub steps: Vec<Step>,
    /// False when the run stopped at the step limit before the model finished.
    pub finished: bool,
}

/// The longest tool result passed back to the model, in characters.
const MAX_TOOL_RESULT: usize = 30_000;

/// A configured model.
pub struct Assistant<'t> {
    cfg: Config,
    transport: &'t dyn Transport,
    usage: std::cell::Cell<Usage>,
    cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

impl<'t> Assistant<'t> {
    pub fn new(cfg: Config, transport: &'t dyn Transport) -> Result<Self, AiError> {
        cfg.validate()?;
        Ok(Self { cfg, transport, usage: std::cell::Cell::new(Usage::default()), cancel: None })
    }

    /// Stop before the next request once `flag` is set (the request under way finishes).
    pub fn with_cancel(mut self, flag: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.cancel = Some(flag);
        self
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Tokens used so far.
    pub fn usage(&self) -> Usage {
        self.usage.get()
    }

    /// One request with the conversation so far.
    pub fn chat(&self, system: &str, messages: &[Value], tools: &[ToolSpec]) -> Result<Reply, AiError> {
        if self.cancel.as_ref().is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed)) {
            return Err(AiError::Cancelled);
        }
        let body = wire::body(&self.cfg, system, messages, tools);
        let answer = self.transport.post(&wire::endpoint(&self.cfg), &wire::headers(&self.cfg), &body)?;
        let reply = wire::parse(self.cfg.provider.format(), &answer)?;
        let mut u = self.usage.get();
        u.requests = u.requests.saturating_add(1);
        u.input_tokens = u.input_tokens.saturating_add(reply.input_tokens);
        u.output_tokens = u.output_tokens.saturating_add(reply.output_tokens);
        self.usage.set(u);
        Ok(reply)
    }

    /// A single answer to `user`. A refusal, or an answer cut off at the output limit, is an
    /// error rather than a partial answer.
    pub fn answer(&self, system: &str, user: &str) -> Result<String, AiError> {
        self.converse(system, &mut vec![wire::user_message(self.cfg.provider.format(), user)])
    }

    /// Continue `messages` (appending the reply) and return its text.
    pub fn converse(&self, system: &str, messages: &mut Vec<Value>) -> Result<String, AiError> {
        let reply = self.chat(system, messages, &[])?;
        messages.push(reply.message.clone());
        match reply.stop {
            Stop::Refusal(why) => Err(AiError::Refused(why)),
            Stop::MaxTokens => Err(AiError::Response(format!(
                "the answer was cut off at {} tokens; ask for less (fewer pages or a shorter result)",
                self.cfg.max_output_tokens
            ))),
            _ if reply.text.trim().is_empty() => Err(AiError::Response("the model returned an empty answer".into())),
            _ => Ok(reply.text),
        }
    }

    /// Append a user turn to `messages`.
    pub fn push_user(&self, messages: &mut Vec<Value>, text: &str) {
        messages.push(wire::user_message(self.cfg.provider.format(), text));
    }

    /// Let the model work with `tools` until it answers without calling one, or `max_steps`
    /// tool calls have run. `run` executes one call (`Ok` result text or `Err` message, both
    /// passed back to the model); `on_step` sees each call as it finishes.
    pub fn run_tools(
        &self,
        system: &str,
        user: &str,
        tools: &[ToolSpec],
        max_steps: usize,
        run: &mut dyn FnMut(&ToolCall) -> Result<String, String>,
        on_step: &mut dyn FnMut(&Step),
    ) -> Result<Outcome, AiError> {
        let format = self.cfg.provider.format();
        let mut messages = vec![wire::user_message(format, user)];
        let mut steps: Vec<Step> = Vec::new();
        loop {
            let reply = self.chat(system, &messages, tools)?;
            messages.push(reply.message.clone());
            match &reply.stop {
                Stop::Refusal(why) => return Err(AiError::Refused(why.clone())),
                Stop::ToolUse if !reply.calls.is_empty() => {}
                Stop::MaxTokens if reply.calls.is_empty() => {
                    return Err(AiError::Response(format!("the answer was cut off at {} tokens", self.cfg.max_output_tokens)));
                }
                _ if reply.calls.is_empty() => return Ok(Outcome { text: reply.text, steps, finished: true }),
                _ => {}
            }
            if steps.len() >= max_steps {
                return Ok(Outcome { text: reply.text, steps, finished: false });
            }
            let mut results = Vec::new();
            for call in &reply.calls {
                let known = tools.iter().any(|t| t.name == call.name);
                let outcome = match (&call.invalid, known) {
                    (Some(e), _) => Err(e.clone()),
                    (None, false) => Err(format!("unknown tool {:?}; use one of the tools provided", call.name)),
                    // Past the limit, remaining calls in this turn are answered without running.
                    (None, true) if steps.len() >= max_steps => Err("step limit reached; finish with a summary of what was done".into()),
                    (None, true) => run(call),
                };
                let (ok, text) = match outcome {
                    Ok(t) => (true, crate::clip(&t, MAX_TOOL_RESULT)),
                    Err(e) => (false, crate::clip(&e, MAX_TOOL_RESULT)),
                };
                let step = Step { tool: call.name.clone(), input: call.input.clone(), ok, result: crate::clip(&text, 300) };
                on_step(&step);
                steps.push(step);
                results.push((call.id.clone(), text, !ok));
            }
            messages.extend(wire::tool_results(format, &results));
        }
    }
}
