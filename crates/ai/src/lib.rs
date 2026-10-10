//! pdfcraft-ai — opt-in AI providers (L4). See the README.
//!
//! The user brings their own API key for one of five providers ([`Provider`]): Anthropic
//! (Claude), OpenAI, DeepSeek, xAI (Grok) or Google (Gemini). Anthropic speaks its Messages
//! API; the other four share the OpenAI-compatible chat-completions format, so there are two
//! wire formats ([`wire`]) rather than five. [`Assistant`] sends requests through a
//! [`Transport`] (HTTPS on native builds, a fake in tests) and runs the tool-use loop; [`tasks`]
//! holds the document tasks: summarize, answer questions with page citations, carry out an
//! instruction with the automation tools, extract tables and outline a presentation.
//!
//! Nothing here runs unless the user asks for it, and nothing is sent anywhere but the
//! provider's endpoint they chose. The API key is never logged or shown (`Config`'s `Debug`
//! redacts it).

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod agent;
mod provider;
pub mod tasks;
mod transport;
pub mod wire;

pub use agent::{Assistant, Outcome, Step, Usage};
pub use provider::{Config, Format, Provider};
#[cfg(all(feature = "http", not(target_arch = "wasm32")))]
pub use transport::HttpTransport;
pub use transport::Transport;
pub use wire::{Reply, Stop, ToolCall, ToolSpec};

/// What went wrong talking to a provider or reading its answer.
#[derive(Debug, Clone, PartialEq)]
pub enum AiError {
    /// The settings can't be used (no key, a malformed key, an endpoint that isn't HTTPS).
    Config(String),
    /// The provider answered with an error status.
    Http { status: u16, message: String },
    /// The provider couldn't be reached.
    Network(String),
    /// The answer couldn't be read, or was cut off.
    Response(String),
    /// The model declined the request.
    Refused(String),
    /// The document is larger than the model can read at once.
    TooLarge(String),
    /// The user stopped the run.
    Cancelled,
}

impl std::fmt::Display for AiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(m) => write!(f, "AI settings: {m}"),
            Self::Http { status, message } => {
                let hint = match status {
                    401 | 403 => " (check the API key and that it belongs to this provider)",
                    402 => " (the provider account has no credit)",
                    404 => " (check the model name)",
                    429 => " (rate limit or quota reached; try again later)",
                    _ => "",
                };
                write!(f, "the AI provider answered {status}: {message}{hint}")
            }
            Self::Network(m) => write!(f, "couldn't reach the AI provider: {m}"),
            Self::Response(m) => write!(f, "unreadable answer from the AI provider: {m}"),
            Self::Refused(m) => write!(f, "the model declined the request: {m}"),
            Self::TooLarge(m) => f.write_str(m),
            Self::Cancelled => f.write_str("stopped"),
        }
    }
}

impl std::error::Error for AiError {}

/// The first `max` characters of `s` (at a character boundary), with a note when cut.
pub(crate) fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        None => s.to_string(),
        Some((at, _)) => format!("{}… [{} more characters left out]", s.get(..at).unwrap_or(""), s.chars().count().saturating_sub(max)),
    }
}

#[cfg(test)]
mod tests;
