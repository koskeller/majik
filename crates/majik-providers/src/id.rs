//! Newtypes for the string ids that cross the provider boundary.
//!
//! Every one of them is a persistence key, written into `library.db`, into a stored request, or
//! into `config.json`, so the raw strings are fixed and a rename is a schema change.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Provider identifier. The raw strings are persistence keys; do not change them.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderId(pub String);

impl ProviderId {
    pub const FAL: &'static str = "fal.ai";
    pub const OPEN_ROUTER: &'static str = "OpenRouter";
    pub const REPLICATE: &'static str = "Replicate";
    pub const MOCK: &'static str = "Mock";

    pub fn fal() -> Self {
        Self(Self::FAL.into())
    }
    pub fn open_router() -> Self {
        Self(Self::OPEN_ROUTER.into())
    }
    pub fn replicate() -> Self {
        Self(Self::REPLICATE.into())
    }
    pub fn mock() -> Self {
        Self(Self::MOCK.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ProviderId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}
