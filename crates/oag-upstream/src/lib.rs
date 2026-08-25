#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! Talking to providers.

pub mod adapter;
pub mod anthropic;
pub mod bedrock;
pub mod eventstream;
pub mod gemini;
pub mod openai;
pub mod openai_oauth;
pub mod sigv4;
pub mod transport;
pub mod usage;
pub mod xai_oauth;

pub use adapter::{Framing, ProviderAdapter, UpstreamRequest};
pub use anthropic::AnthropicAdapter;
pub use bedrock::BedrockAdapter;
pub use gemini::GeminiAdapter;
pub use openai::OpenAICompatAdapter;
pub use transport::{HttpTransport, Transport, TransportKey, TransportPool};
