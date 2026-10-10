//! Sending a request: HTTPS on native builds, anything else (a fake in tests) through the trait.

use serde_json::Value;

use crate::AiError;

/// Posts a JSON body and returns the JSON answer. An error status becomes
/// [`AiError::Http`] with the provider's message.
pub trait Transport {
    fn post(&self, url: &str, headers: &[(String, String)], body: &Value) -> Result<Value, AiError>;
}

#[cfg(all(feature = "http", not(target_arch = "wasm32")))]
mod http {
    use std::time::Duration;

    use serde_json::Value;

    use super::Transport;
    use crate::AiError;

    /// The largest answer read (a long answer is a few hundred kilobytes).
    const MAX_ANSWER: u64 = 32 << 20;
    /// Statuses worth retrying: rate limits, overload and gateway errors.
    const RETRY: &[u16] = &[408, 429, 500, 502, 503, 504, 529];

    /// HTTPS over rustls with the operating system's trusted roots.
    pub struct HttpTransport {
        agent: ureq::Agent,
        attempts: u32,
    }

    impl HttpTransport {
        /// `timeout` bounds one request; long answers from large documents can take minutes.
        pub fn new(timeout: Duration) -> Result<Self, AiError> {
            let found = rustls_native_certs::load_native_certs();
            let certs: Vec<ureq::tls::Certificate<'static>> =
                found.certs.iter().map(|c| ureq::tls::Certificate::from_der(c.as_ref()).to_owned()).collect();
            if certs.is_empty() {
                return Err(AiError::Network("no trusted certificates found on this system".into()));
            }
            let agent = ureq::Agent::config_builder()
                .timeout_global(Some(timeout))
                .http_status_as_error(false)
                .tls_config(ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::new_with_certs(&certs)).build())
                .user_agent(concat!("PdfCraft/", env!("CARGO_PKG_VERSION")))
                .build()
                .new_agent();
            Ok(Self { agent, attempts: 3 })
        }
    }

    impl Transport for HttpTransport {
        fn post(&self, url: &str, headers: &[(String, String)], body: &Value) -> Result<Value, AiError> {
            let payload = serde_json::to_vec(body).map_err(|e| AiError::Response(e.to_string()))?;
            let mut last = AiError::Network("no attempt was made".into());
            for attempt in 0..self.attempts {
                let mut req = self.agent.post(url);
                for (k, v) in headers {
                    req = req.header(k.as_str(), v.as_str());
                }
                let mut response = match req.send(&payload[..]) {
                    Ok(r) => r,
                    // Never echo the request: the error text holds no headers, so no key.
                    Err(e) => {
                        last = AiError::Network(e.to_string());
                        if attempt + 1 < self.attempts {
                            std::thread::sleep(Duration::from_secs(2u64 << attempt));
                        }
                        continue;
                    }
                };
                let status = response.status().as_u16();
                let wait =
                    response.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok()).map(|s| s.min(60));
                let text = response
                    .body_mut()
                    .with_config()
                    .limit(MAX_ANSWER)
                    .read_to_string()
                    .map_err(|e| AiError::Network(format!("reading the answer: {e}")))?;
                let json: Option<Value> = serde_json::from_str(&text).ok();
                if (200..300).contains(&status) {
                    return json.ok_or_else(|| AiError::Response(format!("not JSON: {}", crate::clip(&text, 200))));
                }
                let message = json.as_ref().and_then(crate::wire::error_message).unwrap_or_else(|| crate::clip(text.trim(), 300));
                last = AiError::Http { status, message };
                if !RETRY.contains(&status) || attempt + 1 == self.attempts {
                    break;
                }
                std::thread::sleep(Duration::from_secs(wait.unwrap_or(2u64 << attempt)));
            }
            Err(last)
        }
    }
}

#[cfg(all(feature = "http", not(target_arch = "wasm32")))]
pub use http::HttpTransport;
