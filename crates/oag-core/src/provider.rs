//! Upstream providers.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// An upstream inference provider.
///
/// Deliberately a closed enum rather than a string: adding a provider means
/// writing an adapter, and the compiler should make you notice every match arm
/// that needs a new case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Provider {
    Anthropic,
    OpenAI,
    Gemini,
    Kimi,
    DeepSeek,
    Zhipu,
    XAI,
    Bedrock,
    Vertex,
}

impl Provider {
    /// The wire dialect this provider speaks natively.
    ///
    /// Most "OpenAI-compatible" Chinese providers really are — they are served
    /// by the same translation path rather than each getting an adapter.
    #[must_use]
    pub const fn native_dialect(self) -> Dialect {
        match self {
            Self::Anthropic | Self::Bedrock => Dialect::AnthropicMessages,
            // Chat Completions, not Responses. OpenAI serves both, but the
            // adapter registered for it speaks Chat Completions — and if this
            // said otherwise, an OpenAI client hitting an OpenAI upstream would
            // never take the passthrough path and would round-trip every frame
            // through the canonical form for no reason. Declaring Responses
            // here is a promise only a Responses codec can keep.
            Self::OpenAI | Self::Kimi | Self::DeepSeek | Self::Zhipu | Self::XAI => {
                Dialect::OpenAIChatCompletions
            }
            Self::Gemini | Self::Vertex => Dialect::GeminiGenerateContent,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAI => "openai",
            Self::Gemini => "gemini",
            Self::Kimi => "kimi",
            Self::DeepSeek => "deepseek",
            Self::Zhipu => "zhipu",
            Self::XAI => "xai",
            Self::Bedrock => "bedrock",
            Self::Vertex => "vertex",
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Provider {
    type Err = crate::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "anthropic" => Ok(Self::Anthropic),
            "openai" => Ok(Self::OpenAI),
            "gemini" => Ok(Self::Gemini),
            "kimi" | "moonshot" => Ok(Self::Kimi),
            "deepseek" => Ok(Self::DeepSeek),
            "zhipu" | "glm" => Ok(Self::Zhipu),
            "xai" | "grok" => Ok(Self::XAI),
            "bedrock" => Ok(Self::Bedrock),
            "vertex" => Ok(Self::Vertex),
            other => Err(crate::Error::UnknownProvider(other.to_owned())),
        }
    }
}

/// A wire format. Both a client can speak one and an upstream can expect one;
/// translating between them is `oag-proto`'s entire job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Dialect {
    /// `POST /v1/messages` — Anthropic's native format, and our canonical hub.
    AnthropicMessages,
    /// `POST /v1/chat/completions` — the lingua franca of everything else.
    OpenAIChatCompletions,
    /// `POST /v1/responses` — `OpenAI`'s newer stateful surface.
    OpenAIResponses,
    /// `POST /v1beta/models/{model}:generateContent`
    GeminiGenerateContent,
}

impl Dialect {
    /// How to name this dialect in a message a client will read.
    ///
    /// The `Debug` spelling leaks our enum variants at people debugging their
    /// own request bodies; these are the names the vendors' own documentation
    /// uses, so a caller can go and look the field up.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AnthropicMessages => "Anthropic Messages",
            Self::OpenAIChatCompletions => "OpenAI Chat Completions",
            Self::OpenAIResponses => "OpenAI Responses",
            Self::GeminiGenerateContent => "Gemini generateContent",
        }
    }
}

impl fmt::Display for Dialect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
