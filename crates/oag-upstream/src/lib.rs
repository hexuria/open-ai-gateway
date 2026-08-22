#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! Talking to providers.

pub mod adapter;
pub mod anthropic;
pub mod openai;
pub mod transport;

pub use adapter::{ProviderAdapter, UpstreamRequest};
pub use anthropic::AnthropicAdapter;
pub use openai::OpenAICompatAdapter;
pub use transport::{HttpTransport, Transport, TransportKey, TransportPool};
