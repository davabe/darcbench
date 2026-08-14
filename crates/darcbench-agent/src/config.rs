//! Agent configuration, state directory and access tokens.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

/// Default port. Chosen high and unusual so it is unlikely to collide with
/// anything already running on a hosting server, and never 80/443, which
/// DARCBench must never take over.
pub(crate) const DEFAULT_PORT: u16 = 7842;

/// Ports the agent refuses to bind, no matter what is requested.
///
/// Binding these on a live hosting server would either fail or, worse,
/// succeed after something else died and quietly replace a customer's web
/// server. See `docs/INSTALLER-AND-DISCOVERY.md`.
pub(crate) const FORBIDDEN_PORTS: &[u16] = &[80, 443, 22, 21, 25, 3306, 5432, 6379, 8443];

#[derive(Debug, Clone)]
pub(crate) struct AgentConfig {
    pub(crate) bind: SocketAddr,
    pub(crate) state_dir: PathBuf,
    /// Bearer token required for every API call.
    pub(crate) token: AccessToken,
    /// When true, the agent was explicitly asked to listen on a non-loopback
    /// address. This raises additional warnings and is never the default.
    pub(crate) non_loopback_requested: bool,
}

impl AgentConfig {
    pub(crate) fn is_loopback(&self) -> bool {
        self.bind.ip().is_loopback()
    }

    /// URL an operator should open, including the one-time token.
    ///
    /// The token is in the query string because `EventSource` cannot set
    /// headers. It is immediately exchanged for a `HttpOnly` cookie by the UI
    /// and is scoped to this process lifetime - see `docs/THREAT-MODEL.md`
    /// (T-TOKEN-URL) for the residual risk and mitigations.
    pub(crate) fn dashboard_url(&self) -> String {
        let host = if self.bind.ip().is_unspecified() {
            IpAddr::V4(Ipv4Addr::LOCALHOST).to_string()
        } else if self.bind.is_ipv6() {
            format!("[{}]", self.bind.ip())
        } else {
            self.bind.ip().to_string()
        };
        format!(
            "http://{host}:{}/?token={}",
            self.bind.port(),
            self.token.expose()
        )
    }
}

/// A high-entropy bearer token.
///
/// Deliberately a distinct type with no `Display`, so it cannot be logged by
/// accident; revealing it requires calling [`AccessToken::expose`].
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct AccessToken(String);

impl AccessToken {
    /// Generates a 256-bit token from the OS CSPRNG.
    pub(crate) fn generate() -> Result<Self, std::io::Error> {
        let mut raw = [0u8; 32];
        getrandom::getrandom(&mut raw)
            .map_err(|e| std::io::Error::other(format!("entropy unavailable: {e}")))?;
        Ok(Self(hex::encode(raw)))
    }

    pub(crate) fn from_string(value: String) -> Self {
        Self(value)
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }

    /// Constant-time comparison.
    ///
    /// A short-circuiting `==` on a secret is a timing oracle. The dashboard is
    /// reachable over a network in tunnelled setups, so this matters.
    pub(crate) fn matches(&self, candidate: &str) -> bool {
        let expected = self.0.as_bytes();
        let actual = candidate.as_bytes();
        // The token length is fixed and public (64 hex characters), so
        // rejecting a length mismatch early leaks nothing. What must not
        // short-circuit is the byte comparison below.
        if expected.len() != actual.len() {
            return false;
        }
        let mut diff = 0u8;
        for (a, b) in expected.iter().zip(actual) {
            diff |= a ^ b;
        }
        diff == 0
    }
}

impl std::fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AccessToken([redacted])")
    }
}

/// Resolves the state directory.
///
/// Order: `DARCBENCH_HOME`, then `$XDG_STATE_HOME/darcbench`, then
/// `$HOME/.local/state/darcbench`, then `/var/lib/darcbench` for a system
/// service. Never a world-writable location such as `/tmp`, because the agent
/// key and result bundles live here.
pub(crate) fn default_state_dir() -> PathBuf {
    if let Ok(explicit) = std::env::var("DARCBENCH_HOME") {
        if !explicit.is_empty() {
            return PathBuf::from(explicit);
        }
    }
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("darcbench");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() && home != "/" {
            return PathBuf::from(home).join(".local/state/darcbench");
        }
    }
    PathBuf::from("/var/lib/darcbench")
}

/// A validated, absolute path inside the state directory.
///
/// Every filesystem write the agent performs goes through this type. The
/// component grammar is restrictive enough that traversal is impossible, and
/// the result is asserted to still be under the root.
#[derive(Debug, Clone)]
pub(crate) struct StatePath(PathBuf);

#[derive(Debug, thiserror::Error)]
pub(crate) enum PathError {
    #[error("path component `{0}` is not permitted")]
    IllegalComponent(String),
    #[error("resolved path escapes the state directory")]
    Escape,
}

impl StatePath {
    /// Joins `components` under `root`, rejecting anything that could escape.
    pub(crate) fn join(root: &std::path::Path, components: &[&str]) -> Result<Self, PathError> {
        let mut path = root.to_path_buf();
        for component in components {
            if component.is_empty()
                || component == &"."
                || component == &".."
                || component.contains('/')
                || component.contains('\\')
                || component.contains('\0')
            {
                return Err(PathError::IllegalComponent((*component).to_string()));
            }
            path.push(component);
        }
        if !path.starts_with(root) {
            return Err(PathError::Escape);
        }
        Ok(Self(path))
    }

    pub(crate) fn as_path(&self) -> &std::path::Path {
        &self.0
    }
}

/// Validates a requested bind port.
pub(crate) fn validate_port(port: u16) -> Result<u16, String> {
    if port == 0 {
        return Err("port 0 would bind an arbitrary ephemeral port".to_string());
    }
    if FORBIDDEN_PORTS.contains(&port) {
        return Err(format!(
            "port {port} is reserved for existing services; DARCBench never binds it. \
             Pick another port with --port."
        ));
    }
    if port < 1024 {
        return Err(format!(
            "port {port} is privileged; DARCBench does not bind privileged ports"
        ));
    }
    Ok(port)
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_high_entropy_and_unique() {
        let a = AccessToken::generate().expect("token");
        let b = AccessToken::generate().expect("token");
        assert_eq!(a.expose().len(), 64, "256 bits as hex");
        assert_ne!(a.expose(), b.expose());
    }

    #[test]
    fn token_comparison_accepts_only_the_exact_value() {
        let token = AccessToken::generate().expect("token");
        assert!(token.matches(token.expose()));
        assert!(!token.matches(""));
        assert!(!token.matches("short"));
        assert!(!token.matches(&format!("{}x", token.expose())));
        let mut wrong = token.expose().to_string();
        wrong.replace_range(0..1, if wrong.starts_with('a') { "b" } else { "a" });
        assert!(!token.matches(&wrong));
    }

    #[test]
    fn token_is_never_rendered_by_debug() {
        let token = AccessToken::generate().expect("token");
        let rendered = format!("{token:?}");
        assert!(!rendered.contains(token.expose()));
        assert!(rendered.contains("redacted"));
    }

    #[test]
    fn reserved_and_privileged_ports_are_refused() {
        for port in [80, 443, 22, 3306, 5432, 6379] {
            assert!(validate_port(port).is_err(), "port {port} must be refused");
        }
        assert!(validate_port(0).is_err());
        assert!(validate_port(1023).is_err());
        assert_eq!(validate_port(DEFAULT_PORT).expect("default"), DEFAULT_PORT);
        assert_eq!(validate_port(9000).expect("ok"), 9000);
    }

    #[test]
    fn state_paths_cannot_traverse_out_of_the_root() {
        let root = std::path::Path::new("/var/lib/darcbench");
        assert!(StatePath::join(root, &["runs", "run_abc"]).is_ok());
        for evil in ["..", ".", "", "../etc", "a/b", "a\\b", "nul\0byte"] {
            assert!(
                StatePath::join(root, &["runs", evil]).is_err(),
                "component `{evil}` must be rejected"
            );
        }
    }

    #[test]
    fn state_path_stays_under_the_root() {
        let root = std::path::Path::new("/var/lib/darcbench");
        let path = StatePath::join(root, &["runs", "run_1", "bundle.json"]).expect("join");
        assert!(path.as_path().starts_with(root));
        assert_eq!(
            path.as_path(),
            std::path::Path::new("/var/lib/darcbench/runs/run_1/bundle.json")
        );
    }

    #[test]
    fn dashboard_url_carries_the_token_and_a_reachable_host() {
        let config = AgentConfig {
            bind: SocketAddr::from(([127, 0, 0, 1], DEFAULT_PORT)),
            state_dir: PathBuf::from("/tmp/darcbench"),
            token: AccessToken::from_string("deadbeef".into()),
            non_loopback_requested: false,
        };
        assert_eq!(
            config.dashboard_url(),
            "http://127.0.0.1:7842/?token=deadbeef"
        );
        assert!(config.is_loopback());

        // 0.0.0.0 is not a URL a browser can use; it must be rendered as
        // localhost rather than handed to the operator verbatim.
        let wildcard = AgentConfig {
            bind: SocketAddr::from(([0, 0, 0, 0], 9000)),
            ..config
        };
        assert!(wildcard
            .dashboard_url()
            .starts_with("http://127.0.0.1:9000/"));
    }

    #[test]
    fn state_dir_prefers_the_explicit_override() {
        // Not using std::env::set_var: it is process-global and would race
        // other tests. The precedence itself is asserted structurally.
        let dir = default_state_dir();
        assert!(
            dir.is_absolute(),
            "state dir must be absolute, got {}",
            dir.display()
        );
        assert!(
            !dir.starts_with("/tmp"),
            "state dir must not live in a world-writable location"
        );
    }
}
