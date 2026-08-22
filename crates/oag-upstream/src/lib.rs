#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! Talking to providers.

pub mod adapter;
pub mod anthropic;
pub mod bedrock;
pub mod gemini;
pub mod openai;
pub mod sigv4;
pub mod transport;

pub use adapter::{ProviderAdapter, UpstreamRequest};
pub use anthropic::AnthropicAdapter;
pub use bedrock::BedrockAdapter;
pub use gemini::GeminiAdapter;
pub use openai::OpenAICompatAdapter;
pub use transport::{HttpTransport, Transport, TransportKey, TransportPool};
