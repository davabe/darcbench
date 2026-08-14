//! Privacy: identifying values are typed, not merely conventionally handled.
//!
//! Wrapping a value in [`Sensitive`] makes leaking it a deliberate act. The
//! default `Serialize` implementation emits a redaction marker, so the failure
//! mode of forgetting to think about privacy is *over*-redaction, not a leaked
//! hostname on a public report page.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Marker emitted in place of a redacted value.
pub const REDACTED: &str = "[redacted]";

/// How much identifying detail a given serialisation is allowed to contain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum RedactionPolicy {
    /// Public reports, leaderboards, uploaded bundles by default.
    #[default]
    Redact,
    /// Local-only output the operator explicitly asked for (`--include-sensitive`).
    Reveal,
}

thread_local! {
    static POLICY: std::cell::Cell<RedactionPolicy> =
        const { std::cell::Cell::new(RedactionPolicy::Redact) };
}

/// Runs `f` with the given redaction policy in effect for this thread.
///
/// Scoped rather than global: a process can serve a local operator view and a
/// public share page concurrently without one contaminating the other.
pub fn with_policy<T>(policy: RedactionPolicy, f: impl FnOnce() -> T) -> T {
    let previous = POLICY.with(|p| p.replace(policy));
    let out = f();
    POLICY.with(|p| p.set(previous));
    out
}

pub fn current_policy() -> RedactionPolicy {
    POLICY.with(|p| p.get())
}

/// A value that identifies a machine, an account or a person.
///
/// Examples: hostname, DMI serial numbers, MAC addresses, public IP addresses,
/// cloud instance and account ids.
#[derive(Clone, PartialEq, Eq)]
pub struct Sensitive<T>(T);

impl<T> Sensitive<T> {
    pub fn new(value: T) -> Self {
        Self(value)
    }

    /// Deliberate, greppable unwrapping. Every call site is a place where a
    /// privacy decision is being made.
    pub fn expose(&self) -> &T {
        &self.0
    }

    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T: fmt::Debug> fmt::Debug for Sensitive<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match current_policy() {
            RedactionPolicy::Reveal => write!(f, "Sensitive({:?})", self.0),
            RedactionPolicy::Redact => f.write_str("Sensitive([redacted])"),
        }
    }
}

impl<T: Serialize> Serialize for Sensitive<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match current_policy() {
            RedactionPolicy::Reveal => self.0.serialize(serializer),
            RedactionPolicy::Redact => serializer.serialize_str(REDACTED),
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Sensitive<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        T::deserialize(deserializer).map(Sensitive)
    }
}

impl<T> From<T> for Sensitive<T> {
    fn from(value: T) -> Self {
        Self(value)
    }
}

/// Collapses an IPv4/IPv6 address to its network prefix.
///
/// Used where geography or provider routing matters but the exact host does
/// not: `203.0.113.42` becomes `203.0.113.0/24`.
pub fn coarsen_ip(addr: &str) -> String {
    if let Some((v6_head, _)) = addr.split_once("::") {
        let groups: Vec<&str> = v6_head.split(':').take(2).collect();
        return format!("{}::/32", groups.join(":"));
    }
    let octets: Vec<&str> = addr.split('.').collect();
    if octets.len() == 4 {
        return format!("{}.{}.{}.0/24", octets[0], octets[1], octets[2]);
    }
    let groups: Vec<&str> = addr.split(':').take(2).collect();
    if groups.len() == 2 {
        return format!("{}::/32", groups.join(":"));
    }
    REDACTED.to_string()
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;

    #[derive(Serialize)]
    struct Host {
        name: Sensitive<String>,
        cores: u32,
    }

    #[test]
    fn redaction_is_the_default() {
        let host = Host {
            name: Sensitive::new("web01.prod.example".into()),
            cores: 8,
        };
        let json = serde_json::to_value(&host).expect("ser");
        assert_eq!(json["name"], REDACTED);
        assert_eq!(json["cores"], 8);
    }

    #[test]
    fn reveal_is_opt_in_and_scoped() {
        let host = Host {
            name: Sensitive::new("web01.prod.example".into()),
            cores: 8,
        };
        let revealed = with_policy(RedactionPolicy::Reveal, || {
            serde_json::to_value(&host).expect("ser")
        });
        assert_eq!(revealed["name"], "web01.prod.example");

        // The policy must not leak past the scope.
        let after = serde_json::to_value(&host).expect("ser");
        assert_eq!(after["name"], REDACTED);
    }

    #[test]
    fn debug_output_is_also_redacted() {
        let s = Sensitive::new("00:1a:2b:3c:4d:5e".to_string());
        assert!(!format!("{s:?}").contains("1a:2b"));
    }

    #[test]
    fn nested_policy_restores_the_outer_value() {
        with_policy(RedactionPolicy::Reveal, || {
            with_policy(RedactionPolicy::Redact, || {
                assert_eq!(current_policy(), RedactionPolicy::Redact);
            });
            assert_eq!(current_policy(), RedactionPolicy::Reveal);
        });
        assert_eq!(current_policy(), RedactionPolicy::Redact);
    }

    #[test]
    fn ip_coarsening() {
        assert_eq!(coarsen_ip("203.0.113.42"), "203.0.113.0/24");
        assert_eq!(coarsen_ip("2001:db8::1"), "2001:db8::/32");
        assert_eq!(coarsen_ip("not-an-ip"), REDACTED);
    }
}
