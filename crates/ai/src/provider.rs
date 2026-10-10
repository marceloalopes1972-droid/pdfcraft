//! The five providers, their endpoints and defaults, and the user's settings.

use crate::AiError;

/// An AI provider the user has an API key for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Provider {
    #[default]
    Anthropic,
    OpenAi,
    DeepSeek,
    Grok,
    Gemini,
}

/// The request format a provider speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// Anthropic's Messages API (`/v1/messages`).
    Anthropic,
    /// OpenAI's chat completions (`/chat/completions`), which DeepSeek, xAI and Google's
    /// OpenAI-compatible endpoint also accept.
    OpenAiChat,
}

impl Provider {
    pub const ALL: [Provider; 5] = [Provider::Anthropic, Provider::OpenAi, Provider::DeepSeek, Provider::Grok, Provider::Gemini];

    /// The id used in settings and on the command line.
    pub fn id(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::DeepSeek => "deepseek",
            Self::Grok => "grok",
            Self::Gemini => "gemini",
        }
    }

    /// The name shown to the user.
    pub fn name(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic (Claude)",
            Self::OpenAi => "OpenAI (ChatGPT)",
            Self::DeepSeek => "DeepSeek",
            Self::Grok => "xAI (Grok)",
            Self::Gemini => "Google (Gemini)",
        }
    }

    /// A provider by id or common name, ignoring case (`claude`, `chatgpt`, `xai`, `google`).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "anthropic" | "claude" => Some(Self::Anthropic),
            "openai" | "chatgpt" | "gpt" => Some(Self::OpenAi),
            "deepseek" => Some(Self::DeepSeek),
            "grok" | "xai" | "x.ai" => Some(Self::Grok),
            "gemini" | "google" => Some(Self::Gemini),
            _ => None,
        }
    }

    pub fn format(self) -> Format {
        match self {
            Self::Anthropic => Format::Anthropic,
            _ => Format::OpenAiChat,
        }
    }

    /// The API base URL.
    pub fn base_url(self) -> &'static str {
        match self {
            Self::Anthropic => "https://api.anthropic.com/v1",
            Self::OpenAi => "https://api.openai.com/v1",
            Self::DeepSeek => "https://api.deepseek.com/v1",
            Self::Grok => "https://api.x.ai/v1",
            Self::Gemini => "https://generativelanguage.googleapis.com/v1beta/openai",
        }
    }

    /// The model used unless the user names another. Providers rename and retire models, so
    /// the user can always override this.
    pub fn default_model(self) -> &'static str {
        match self {
            Self::Anthropic => "claude-opus-5-5",
            Self::OpenAi => "gpt-5",
            Self::DeepSeek => "deepseek-chat",
            Self::Grok => "grok-4",
            Self::Gemini => "gemini-2.5-flash",
        }
    }

    /// The environment variable the provider's own tools read the key from.
    pub fn key_env(self) -> &'static str {
        match self {
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::OpenAi => "OPENAI_API_KEY",
            Self::DeepSeek => "DEEPSEEK_API_KEY",
            Self::Grok => "XAI_API_KEY",
            Self::Gemini => "GEMINI_API_KEY",
        }
    }

    /// Roughly how many characters of document text the default model reads at once (about
    /// three characters per token, leaving room for the answer).
    pub fn max_document_chars(self) -> usize {
        match self {
            Self::Anthropic | Self::Gemini => 2_000_000,
            Self::OpenAi => 800_000,
            Self::Grok => 600_000,
            Self::DeepSeek => 250_000,
        }
    }
}

/// The settings for one provider: which, its key, the model and the endpoint.
#[derive(Clone, PartialEq)]
pub struct Config {
    pub provider: Provider,
    pub api_key: String,
    pub model: String,
    /// The API base URL; the provider's own by default. Another HTTPS endpoint speaking the same
    /// format (a proxy, a gateway) or a local server on this machine (`http://localhost:…`)
    /// may be given instead.
    pub base_url: String,
    /// The longest answer, in tokens.
    pub max_output_tokens: u32,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("provider", &self.provider)
            .field("api_key", &"<redacted>")
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .field("max_output_tokens", &self.max_output_tokens)
            .finish()
    }
}

impl Config {
    /// The provider's defaults with `api_key`.
    pub fn new(provider: Provider, api_key: impl Into<String>) -> Self {
        Self {
            provider,
            api_key: api_key.into().trim().to_string(),
            model: provider.default_model().into(),
            base_url: provider.base_url().into(),
            max_output_tokens: 16_000,
        }
    }

    /// Refuse settings that can't work or would leak the key: an empty key, a key with spaces
    /// or control characters (it goes into an HTTP header), an empty model, and an endpoint
    /// that isn't HTTPS (plain HTTP only to this machine).
    pub fn validate(&self) -> Result<(), AiError> {
        if self.api_key.is_empty() {
            return Err(AiError::Config(format!("no API key for {} (set {} or give a key file)", self.provider.name(), self.provider.key_env())));
        }
        if self.api_key.len() > 4096 || self.api_key.chars().any(|c| c.is_whitespace() || c.is_control() || !c.is_ascii()) {
            return Err(AiError::Config("the API key has characters an API key can't have (spaces, line breaks or accents); copy it again".into()));
        }
        if self.model.trim().is_empty() || self.model.len() > 200 || self.model.chars().any(|c| c.is_control()) {
            return Err(AiError::Config("the model name is empty or invalid".into()));
        }
        if !(1..=1_000_000).contains(&self.max_output_tokens) {
            return Err(AiError::Config("the answer length must be between 1 and 1,000,000 tokens".into()));
        }
        let url = self.base_url.trim();
        let local = ["http://localhost", "http://127.0.0.1", "http://[::1]"]
            .iter()
            .any(|p| url.strip_prefix(p).is_some_and(|rest| rest.is_empty() || rest.starts_with(':') || rest.starts_with('/')));
        let https = url.strip_prefix("https://").is_some_and(|rest| !rest.is_empty() && !rest.starts_with('/'));
        if !(https || local) || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(AiError::Config(format!("the endpoint {url:?} must start with https:// (or http://localhost for a model on this computer)")));
        }
        Ok(())
    }
}
