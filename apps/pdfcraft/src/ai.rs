//! The AI assistant's HTTPS transport: built on first use (it reads the system's trusted
//! certificates), so starting PdfCraft never touches the network or the key store.

use std::sync::OnceLock;
use std::time::Duration;

use pdfcraft_ai::{AiError, HttpTransport, Transport};
use serde_json::Value;

#[derive(Default)]
pub struct LazyHttps(OnceLock<Result<HttpTransport, AiError>>);

impl Transport for LazyHttps {
    fn post(&self, url: &str, headers: &[(String, String)], body: &Value) -> Result<Value, AiError> {
        // Long answers about large documents can take minutes.
        match self.0.get_or_init(|| HttpTransport::new(Duration::from_secs(600))) {
            Ok(t) => t.post(url, headers, body),
            Err(e) => Err(e.clone()),
        }
    }
}
