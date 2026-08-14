//! Serving the embedded dashboard.
//!
//! Assets are compiled in by `build.rs`. When `apps/web/dist` was not built,
//! [`EMBEDDED_UI`] is empty and the agent serves [`FALLBACK`] instead: a small
//! self-contained console that is enough to start a run, watch it live and read
//! the result. That means a Rust-only checkout, a minimal container image and a
//! distribution package all still produce a working agent.

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

include!(concat!(env!("OUT_DIR"), "/embedded_ui.rs"));

/// True when the full web UI was compiled into this binary.
pub(crate) fn has_bundled_ui() -> bool {
    !EMBEDDED_UI.is_empty()
}

/// Resolves a request path to an embedded asset, or to the SPA entry point.
pub(crate) fn serve(path: &str) -> Response {
    let normalised = normalise(path);

    // The fallback console's script is served as its own file so the agent's
    // CSP can stay at `script-src 'self'` with no inline-script exemption.
    if normalised == FALLBACK_SCRIPT_ROUTE {
        return (
            [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
            FALLBACK_SCRIPT,
        )
            .into_response();
    }

    if !has_bundled_ui() {
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            FALLBACK,
        )
            .into_response();
    }

    if let Some((_, mime, body)) = EMBEDDED_UI
        .iter()
        .find(|(route, _, _)| *route == normalised)
    {
        return ([(header::CONTENT_TYPE, *mime)], *body).into_response();
    }

    // Single-page app: unknown non-asset paths fall through to index.html so
    // client-side routes survive a reload.
    if let Some((_, mime, body)) = EMBEDDED_UI
        .iter()
        .find(|(route, _, _)| *route == "/index.html")
    {
        return ([(header::CONTENT_TYPE, *mime)], *body).into_response();
    }

    (StatusCode::NOT_FOUND, "not found").into_response()
}

/// Maps a request path to an embedded route key.
///
/// Traversal is structurally impossible here: the result is only ever compared
/// against a fixed table of compile-time strings, never joined onto a
/// filesystem path. The normalisation rules exist so `/` resolves to
/// `/index.html`, and as defence in depth for the day someone refactors this
/// into real file serving.
///
/// Any segment containing `..` is **dropped, not resolved**. Resolving would
/// mean implementing path arithmetic correctly, and the only reason to do that
/// is to support requests no browser actually sends (browsers normalise before
/// transmitting). Dropping cannot escape anything, including percent-encoded
/// forms such as `..%2f..%2f` that a decoder further down the stack might
/// later turn back into separators.
fn normalise(path: &str) -> String {
    let path = path.split(['?', '#']).next().unwrap_or("/");
    if path == "/" || path.is_empty() {
        return "/index.html".to_string();
    }
    let mut out = String::with_capacity(path.len() + 1);
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment.contains("..") {
            continue;
        }
        out.push('/');
        out.push_str(segment);
    }
    if out.is_empty() {
        "/index.html".to_string()
    } else {
        out
    }
}

/// The built-in console, used when the React UI was not built.
///
/// Kept deliberately small. It is a functional fallback, not a second
/// implementation of the dashboard: start a run, watch the live event stream,
/// cancel, and read the final scores.
const FALLBACK: &str = include_str!("../ui/fallback.html");

/// Route the fallback console loads its script from.
const FALLBACK_SCRIPT_ROUTE: &str = "/__darcbench/fallback.js";
const FALLBACK_SCRIPT: &str = include_str!("../ui/fallback.js");

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;

    #[test]
    fn root_resolves_to_the_spa_entry_point() {
        assert_eq!(normalise("/"), "/index.html");
        assert_eq!(normalise(""), "/index.html");
    }

    #[test]
    fn traversal_segments_are_dropped_not_resolved() {
        assert_eq!(normalise("/../../etc/passwd"), "/etc/passwd");
        assert_eq!(normalise("/assets/../../../secret"), "/assets/secret");
        assert_eq!(normalise("/a/./b"), "/a/b");
        assert_eq!(normalise("/////"), "/index.html");
        // Percent-encoded traversal is dropped too, so a decoder added later
        // in the stack cannot resurrect a separator.
        assert_eq!(normalise("/..%2f..%2fetc/passwd"), "/passwd");
    }

    #[test]
    fn query_and_fragment_are_ignored() {
        assert_eq!(normalise("/assets/app.js?v=2"), "/assets/app.js");
        assert_eq!(normalise("/#/runs"), "/index.html");
    }

    #[test]
    fn normalised_paths_are_only_ever_table_lookups() {
        // The output is compared against compile-time constants, so even a
        // pathological input cannot reach the filesystem. Assert the shape
        // anyway, so a future refactor to real file serving starts safe.
        for evil in [
            "/..%2f..%2fetc/passwd",
            "/\0",
            "/a/../../../../root/.ssh/id_rsa",
        ] {
            let out = normalise(evil);
            assert!(out.starts_with('/'));
            assert!(!out.contains(".."));
        }
    }

    #[test]
    fn fallback_is_served_when_no_ui_is_bundled() {
        if has_bundled_ui() {
            return;
        }
        let response = serve("/");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn fallback_console_is_self_contained() {
        assert!(FALLBACK.contains("DARC//BENCH"));
        for forbidden in ["http://", "https://", "//cdn"] {
            assert!(
                !FALLBACK.contains(forbidden),
                "the fallback console must not reference `{forbidden}`"
            );
            assert!(
                !FALLBACK_SCRIPT.contains(forbidden),
                "the fallback script must not reference `{forbidden}`"
            );
        }
    }

    /// The agent sends `script-src 'self'`, which blocks inline scripts. If the
    /// fallback ever grew one, the console would silently stop working in the
    /// browser while every Rust test still passed.
    #[test]
    fn fallback_has_no_inline_script_that_csp_would_block() {
        let inline_open = FALLBACK
            .match_indices("<script")
            .filter(|(index, _)| !FALLBACK[*index..].starts_with("<script src="));
        assert_eq!(
            inline_open.count(),
            0,
            "the fallback console must load its script from {FALLBACK_SCRIPT_ROUTE}, not inline"
        );
        assert!(FALLBACK.contains(FALLBACK_SCRIPT_ROUTE));
    }

    #[test]
    fn fallback_script_is_served_at_its_own_route() {
        let response = serve(FALLBACK_SCRIPT_ROUTE);
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(
            content_type.starts_with("text/javascript"),
            "got `{content_type}`"
        );
    }
}
