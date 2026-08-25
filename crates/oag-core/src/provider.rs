//! Upstream providers.

use crate::credential::CredentialKind;
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

    /// Every variant, in declaration order.
    ///
    /// Hand-written because the language offers no way to enumerate an enum,
    /// and a `strum`-style derive is a dependency for one list. What keeps it
    /// from rotting is [`Provider::support`] directly below: a new variant
    /// stops that match compiling, and the arm you add to fix it is a few lines
    /// from this array.
    pub const ALL: &'static [Self] = &[
        Self::Anthropic,
        Self::OpenAI,
        Self::Gemini,
        Self::Kimi,
        Self::DeepSeek,
        Self::Zhipu,
        Self::XAI,
        Self::Bedrock,
        Self::Vertex,
    ];

    /// What an operator can actually do with this provider.
    ///
    /// A total match rather than a table in a doc or a map with a default,
    /// because both of those answer for a provider nobody wrote an entry for —
    /// and the answer they invent is "supported". Adding a variant breaks this
    /// function, which is the point.
    ///
    /// Everything here is a property of the *build*, not of the deployment: it
    /// says what is possible, and the admin API pairs it with what an operator
    /// has actually configured.
    // Long because it is nine rows of data, not nine branches of logic.
    // Splitting it into per-provider helpers would buy a shorter function and
    // lose the one property worth having: every provider's answer visible in
    // one place, in one match the compiler will not let go stale.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub const fn support(self) -> ProviderSupport {
        match self {
            Self::Anthropic => ProviderSupport {
                provider: self,
                display_name: "Anthropic",
                aliases: &[],
                credential_kinds: &[CredentialKind::ApiKey],
                // Not a gap. Anthropic's terms forbid a third party holding a
                // Claude.ai session at all, so there is deliberately no
                // importer to write — see `docs/compliance.md`.
                subscription: SubscriptionSupport::NotOffered {
                    why: NoSubscription::ProhibitedByTerms {
                        quote: "developers may not collect, store, or intermediate Claude.ai \
                                credentials or session tokens",
                        source: "https://code.claude.com/docs/en/legal-and-compliance",
                    },
                },
                note: Some(
                    "Console API keys, pooled for the org: the carve-out those same terms \
                     grant, and the reason api_key is the default kind.",
                ),
            },
            Self::OpenAI => ProviderSupport {
                provider: self,
                display_name: "OpenAI",
                aliases: &[],
                credential_kinds: &[CredentialKind::ApiKey, CredentialKind::OAuth],
                // Two adapters share this provider: an API key speaks Chat
                // Completions, a Codex seat speaks the Responses surface of the
                // ChatGPT backend. The gateway picks between them per account,
                // which is why one provider carries two credential kinds.
                subscription: SubscriptionSupport::Served {
                    import: "oag admin account add --from codex",
                },
                // Served, but not by importing alone: the backend checks the
                // request's `instructions` against what its own client sends,
                // and OAG compiles none in. A seat imported without
                // `gateway.codex.instructions` set reaches the backend and is
                // refused, which looks like a broken credential rather than
                // missing configuration.
                note: Some(
                    "A Codex seat also needs `gateway.codex.instructions` (or `instructions_path`, \
                     e.g. deploy/codex-instructions.txt); without it the backend rejects the request.",
                ),
            },
            Self::Gemini => ProviderSupport {
                provider: self,
                display_name: "Google Gemini",
                aliases: &[],
                credential_kinds: &[CredentialKind::ApiKey],
                subscription: SubscriptionSupport::NotOffered {
                    why: NoSubscription::NoImporter,
                },
                note: None,
            },
            Self::Kimi => ProviderSupport {
                provider: self,
                display_name: "Moonshot Kimi",
                aliases: &["moonshot"],
                credential_kinds: &[CredentialKind::ApiKey],
                subscription: SubscriptionSupport::NotOffered {
                    why: NoSubscription::NoImporter,
                },
                note: None,
            },
            Self::DeepSeek => ProviderSupport {
                provider: self,
                display_name: "DeepSeek",
                aliases: &[],
                credential_kinds: &[CredentialKind::ApiKey],
                subscription: SubscriptionSupport::NotOffered {
                    why: NoSubscription::NoImporter,
                },
                note: None,
            },
            Self::Zhipu => ProviderSupport {
                provider: self,
                display_name: "Zhipu GLM",
                aliases: &["glm"],
                credential_kinds: &[CredentialKind::ApiKey],
                subscription: SubscriptionSupport::NotOffered {
                    why: NoSubscription::NoImporter,
                },
                note: None,
            },
            Self::XAI => ProviderSupport {
                provider: self,
                display_name: "xAI",
                aliases: &["grok"],
                credential_kinds: &[CredentialKind::ApiKey, CredentialKind::OAuth],
                subscription: SubscriptionSupport::Served {
                    import: "oag admin account add --from grok",
                },
                note: Some(
                    "A seat binds to one principal unless --shared is passed: it is sanctioned \
                     for the holder's own use.",
                ),
            },
            Self::Bedrock => ProviderSupport {
                provider: self,
                display_name: "AWS Bedrock",
                aliases: &[],
                credential_kinds: &[CredentialKind::Bedrock],
                subscription: SubscriptionSupport::NotOffered {
                    why: NoSubscription::NoImporter,
                },
                note: Some(
                    "SigV4-signed, billed through your AWS agreement. The credential is stored \
                     packed as access_key:secret[:session_token], so it needs no shape of its own.",
                ),
            },
            Self::Vertex => ProviderSupport {
                provider: self,
                display_name: "Google Vertex AI",
                aliases: &[],
                credential_kinds: &[CredentialKind::Vertex],
                subscription: SubscriptionSupport::NotOffered {
                    why: NoSubscription::NoImporter,
                },
                note: Some("GCP service-account credentials, billed through your GCP agreement."),
            },
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

/// What one provider supports, as [`Provider::support`] reports it.
///
/// `Serialize` but not `Deserialize`: every field is a `&'static str` compiled
/// into the binary, and nothing reads a matrix back in. It is produced from the
/// enum or it does not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct ProviderSupport {
    pub provider: Provider,
    /// The vendor's own spelling, for a human reading a page. `as_str` is the
    /// spelling the config, the CLI, and the `account.provider` column use.
    pub display_name: &'static str,
    /// Other spellings [`FromStr`] accepts, canonical excluded. Someone who
    /// knows the product as "grok" should not have to guess that we filed it
    /// under "xai".
    pub aliases: &'static [&'static str],
    /// Which credential kinds this provider can be registered with. Not every
    /// provider takes an API key: the two cloud ones take their cloud's
    /// credential and nothing else.
    pub credential_kinds: &'static [CredentialKind],
    pub subscription: SubscriptionSupport,
    /// One line where the row would otherwise mislead. `None` is the common
    /// case — a note on every row is a note nobody reads.
    pub note: Option<&'static str>,
}

impl ProviderSupport {
    /// The wire format this provider speaks. Read through, rather than stored,
    /// so the matrix cannot disagree with the thing that actually routes.
    #[must_use]
    pub const fn dialect(&self) -> Dialect {
        self.provider.native_dialect()
    }

    /// Whether a plain provider API key can be registered here.
    #[must_use]
    pub fn api_key(&self) -> bool {
        self.accepts(CredentialKind::ApiKey)
    }

    #[must_use]
    pub fn accepts(&self, kind: CredentialKind) -> bool {
        self.credential_kinds.contains(&kind)
    }
}

/// Whether a provider's *subscription* — as opposed to its metered API — can be
/// used through this gateway.
///
/// Three states rather than a bool, because two of them are "no" for reasons an
/// operator plans differently around. A seat that imports but serves nothing is
/// not the same as a seat this gateway refuses to hold at all, and collapsing
/// them into `false` loses exactly the part someone was asking about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
#[non_exhaustive]
pub enum SubscriptionSupport {
    /// The importer ships and requests route through the seat.
    Served {
        /// The command that imports it.
        import: &'static str,
    },
    /// The credential imports, seals, and refreshes — and nothing serves
    /// inference on it. Reported rather than hidden: an operator who imports a
    /// seat and sees no traffic move deserves to know why before they debug it.
    CredentialImportOnly {
        import: &'static str,
        /// What is missing before the seat can answer a request.
        gap: &'static str,
    },
    /// No subscription path here.
    NotOffered {
        /// "Nobody wrote one" and "the provider forbids it" are different
        /// answers to "can this be added?", so the reason is typed.
        why: NoSubscription,
    },
}

/// Why a provider has no subscription path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "why", rename_all = "snake_case")]
#[non_exhaustive]
pub enum NoSubscription {
    /// The provider's terms prohibit a third party intermediating subscription
    /// credentials. A decision, not a gap — see `docs/compliance.md`.
    ProhibitedByTerms {
        /// The provider's own words, so an operator can check the refusal
        /// rather than take our summary of it.
        quote: &'static str,
        /// Where those words are published.
        source: &'static str,
    },
    /// No importer exists. Nothing forbids one; nobody has needed it.
    NoImporter,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_provider_in_all_describes_itself() {
        for &p in Provider::ALL {
            let s = p.support();
            assert_eq!(s.provider, p, "{p} carries another provider's descriptor");
            assert!(!s.display_name.is_empty(), "{p} has no display name");
            assert!(
                !s.credential_kinds.is_empty(),
                "{p} accepts no credential kind, so it cannot be registered at all"
            );
        }
    }

    #[test]
    fn all_is_the_enum_in_declaration_order_without_repeats() {
        // Ord is derived, so declaration order is sort order. This catches a
        // provider listed twice or inserted in the wrong place; the compiler
        // catches one left out of `support`.
        let mut sorted = Provider::ALL.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.as_slice(), Provider::ALL);
    }

    #[test]
    fn every_canonical_name_and_alias_parses_back_to_its_provider() {
        // The matrix is what an operator types from. An alias listed here that
        // FromStr does not accept is a documented lie.
        for &p in Provider::ALL {
            let s = p.support();
            assert_eq!(p.as_str().parse::<Provider>().ok(), Some(p));
            for alias in s.aliases {
                assert_eq!(alias.parse::<Provider>().ok(), Some(p), "alias {alias}");
            }
            assert!(
                !s.aliases.contains(&p.as_str()),
                "{p} lists its canonical name as an alias"
            );
        }
    }

    #[test]
    fn a_subscription_path_and_an_oauth_credential_agree() {
        // Two fields that can contradict each other: a provider offering a seat
        // must accept the kind a seat is stored as, and one accepting `oauth`
        // must say what happens when you import one.
        for &p in Provider::ALL {
            let s = p.support();
            let oauth = s.accepts(CredentialKind::OAuth);
            let offered = !matches!(s.subscription, SubscriptionSupport::NotOffered { .. });
            assert_eq!(oauth, offered, "{p} disagrees with itself about seats");
        }
    }

    #[test]
    fn the_anthropic_refusal_cites_the_terms_it_rests_on() {
        // The one row where "no" is a policy decision. If this ever becomes a
        // bare NoImporter, the refusal has lost the reason that justifies it.
        let SubscriptionSupport::NotOffered {
            why: NoSubscription::ProhibitedByTerms { quote, source },
        } = Provider::Anthropic.support().subscription
        else {
            panic!("Anthropic's subscription refusal must carry the terms behind it");
        };
        assert!(quote.contains("intermediate Claude.ai"));
        assert!(source.starts_with("https://"));
    }

    #[test]
    fn a_served_subscription_that_needs_configuring_says_so() {
        // Both subscriptions serve traffic, but only one of them works on the
        // strength of the import alone. The Codex backend validates the
        // request's `instructions` and OAG compiles none in, so a seat imported
        // without that configured is refused at the upstream — which reads as a
        // dead credential unless the matrix names the missing setting. A Grok
        // seat has no such second step and needs no note.
        for provider in [Provider::OpenAI, Provider::XAI] {
            assert!(
                matches!(
                    provider.support().subscription,
                    SubscriptionSupport::Served { .. }
                ),
                "{provider} serves subscription traffic in this build"
            );
        }
        assert!(
            Provider::OpenAI
                .support()
                .note
                .is_some_and(|n| n.contains("instructions")),
            "a Codex seat is only servable once instructions are configured, \
             and the matrix is where an operator finds that out"
        );
    }

    #[test]
    fn the_wire_shape_carries_the_discriminator_the_dashboard_switches_on() {
        // The page renders three states differently and has no other way to
        // tell them apart. Renaming a variant without renaming its arm there
        // silently collapses the matrix to "unknown", which no compiler catches.
        let served = serde_json::to_value(Provider::XAI.support().subscription)
            .expect("a matrix row serialises");
        assert_eq!(served["state"], "served");
        assert!(
            served["import"]
                .as_str()
                .is_some_and(|s| s.contains("--from grok"))
        );

        // Built here rather than read off a provider: no provider is in this
        // state today, and the dashboard still has an arm for it. A variant the
        // matrix can produce but nothing currently uses is exactly the one that
        // rots unnoticed until some future build puts a provider back into it.
        let partial = serde_json::to_value(SubscriptionSupport::CredentialImportOnly {
            import: "oag admin add-account --from-example",
            gap: "nothing serves it",
        })
        .expect("a matrix row serialises");
        assert_eq!(partial["state"], "credential_import_only");

        let refused = serde_json::to_value(Provider::Anthropic.support().subscription)
            .expect("a matrix row serialises");
        assert_eq!(refused["state"], "not_offered");
        assert_eq!(refused["why"]["why"], "prohibited_by_terms");

        let absent = serde_json::to_value(Provider::Gemini.support().subscription)
            .expect("a matrix row serialises");
        assert_eq!(absent["why"]["why"], "no_importer");
    }

    #[test]
    fn the_dialect_is_read_from_the_router_not_restated() {
        for &p in Provider::ALL {
            assert_eq!(p.support().dialect(), p.native_dialect());
        }
    }
}
