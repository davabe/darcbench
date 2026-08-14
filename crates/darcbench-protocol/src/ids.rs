//! Opaque identifiers.
//!
//! Identifiers are deliberately *not* derived from any host property. A run id
//! must not leak hostnames, MAC addresses or instance ids - see
//! `docs/PRIVACY.md`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::ProtocolError;

/// A run identifier: `run_` followed by 32 lowercase hex characters (128 bits
/// of CSPRNG output).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RunId(String);

impl RunId {
    const PREFIX: &'static str = "run_";

    /// Generates a new random run id from the operating system CSPRNG.
    ///
    /// # Panics
    /// Never panics: if the OS entropy source is unavailable the process has no
    /// safe way to continue producing verifiable results, so the error is
    /// surfaced by [`RunId::try_new`] instead.
    pub fn try_new() -> Result<Self, ProtocolError> {
        let mut raw = [0u8; 16];
        getrandom::getrandom(&mut raw)
            .map_err(|e| ProtocolError::InvalidId(format!("entropy unavailable: {e}")))?;
        Ok(Self(format!("{}{}", Self::PREFIX, hex::encode(raw))))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for RunId {
    type Err = ProtocolError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let Some(body) = s.strip_prefix(Self::PREFIX) else {
            return Err(ProtocolError::InvalidId(format!(
                "missing `{}` prefix",
                Self::PREFIX
            )));
        };
        if body.len() != 32
            || !body
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(ProtocolError::InvalidId(
                "run id body must be 32 lowercase hex characters".to_string(),
            ));
        }
        Ok(Self(s.to_string()))
    }
}

impl TryFrom<String> for RunId {
    type Error = ProtocolError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<RunId> for String {
    fn from(value: RunId) -> Self {
        value.0
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A benchmark module identifier such as `cpu.mixed`.
///
/// Grammar: `segment ( "." segment )*` where `segment` is
/// `[a-z][a-z0-9_]{0,31}`. The restricted grammar means a module id can be used
/// directly as a filesystem path component without traversal risk.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ModuleId(String);

impl ModuleId {
    pub fn new(s: &str) -> Result<Self, ProtocolError> {
        s.parse()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ModuleId {
    type Err = ProtocolError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() || s.len() > 96 {
            return Err(ProtocolError::InvalidId(
                "module id length out of range".into(),
            ));
        }
        for segment in s.split('.') {
            let mut chars = segment.chars();
            match chars.next() {
                Some(c) if c.is_ascii_lowercase() => {}
                _ => {
                    return Err(ProtocolError::InvalidId(format!(
                        "segment `{segment}` must start with [a-z]"
                    )))
                }
            }
            if segment.len() > 32 {
                return Err(ProtocolError::InvalidId(format!(
                    "segment `{segment}` too long"
                )));
            }
            if !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
                return Err(ProtocolError::InvalidId(format!(
                    "segment `{segment}` must match [a-z][a-z0-9_]*"
                )));
            }
        }
        Ok(Self(s.to_string()))
    }
}

impl TryFrom<String> for ModuleId {
    type Error = ProtocolError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<ModuleId> for String {
    fn from(value: ModuleId) -> Self {
        value.0
    }
}

impl fmt::Display for ModuleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A module identity pinned to a specific version. Results are only comparable
/// between identical [`ModuleRef`]s - see `docs/BENCHMARK-METHODOLOGY.md`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModuleRef {
    pub id: ModuleId,
    /// Semantic version of the *workload definition*, not of the agent.
    pub version: String,
}

impl fmt::Display for ModuleRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.id, self.version)
    }
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;

    #[test]
    fn run_id_roundtrips() {
        let id = RunId::try_new().expect("entropy");
        let parsed: RunId = id.as_str().parse().expect("parse");
        assert_eq!(id, parsed);
        assert_eq!(id.as_str().len(), 4 + 32);
    }

    #[test]
    fn run_id_rejects_malformed() {
        assert!("run_".parse::<RunId>().is_err());
        assert!("nope_00000000000000000000000000000000"
            .parse::<RunId>()
            .is_err());
        assert!("run_ABCDEF00000000000000000000000000"
            .parse::<RunId>()
            .is_err());
        // Path traversal attempt must not parse.
        assert!("run_../../etc/passwd".parse::<RunId>().is_err());
    }

    #[test]
    fn module_id_grammar() {
        assert!(ModuleId::new("cpu.mixed").is_ok());
        assert!(ModuleId::new("wordpress.origin_cached").is_ok());
        assert!(ModuleId::new("Cpu.Mixed").is_err());
        assert!(ModuleId::new("../etc").is_err());
        assert!(ModuleId::new("cpu..mixed").is_err());
        assert!(ModuleId::new("9cpu").is_err());
    }
}
