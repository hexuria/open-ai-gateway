//! Typed identifiers.
//!
//! These are newtypes rather than bare `Uuid` because the compiler catching a
//! swapped `AccountId`/`RouteId` argument is free, and the alternative is
//! finding it in production when the wrong credential gets picked.

use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

macro_rules! typed_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            /// A fresh time-ordered identifier.
            ///
            /// v7 so rows cluster by creation time on the primary key, which is
            /// what the usage ledger's range scans want.
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            #[must_use]
            pub const fn from_uuid(id: Uuid) -> Self {
                Self(id)
            }

            #[must_use]
            pub const fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl From<Uuid> for $name {
            fn from(id: Uuid) -> Self {
                Self(id)
            }
        }
    };
}

typed_id!(AccountId, "Identifies one upstream credential.");
typed_id!(RouteId, "Identifies a tier ladder plus its entitlements and budget.");
typed_id!(ApiKeyId, "Identifies an inbound client credential.");
typed_id!(PrincipalId, "Identifies an organisation member.");

/// Identifies one inbound request for its whole lifetime.
///
/// Carried into the usage ledger as the idempotency key: a retried metering
/// write for the same request must not bill twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RequestId(pub Uuid);

impl RequestId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    #[must_use]
    pub const fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl Default for RequestId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
