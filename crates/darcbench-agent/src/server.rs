//! The local dashboard server.
//!
//! # Security posture
//!
//! * Binds `127.0.0.1` unless explicitly told otherwise.
//! * Every endpoint except `/healthz` and `/api/v1/meta` requires the access
//!   token. There is no unauthenticated dashboard, on any interface.
//! * **Mutating endpoints require the `Authorization` header specifically**,
//!   never the cookie alone. A browser attaches cookies to cross-site requests
//!   but cannot attach a custom header without a successful CORS preflight,
//!   which is refused. That is the CSRF defence.
//! * `EventSource` cannot send headers, so the SSE stream accepts the cookie.
//!   Streaming is read-only, so cookie-only auth there is not a CSRF risk.
//! * No CORS headers are ever emitted, so no other origin can read a response.
//! * The API surface is closed: profiles and module ids are validated against
//!   an allow-list. Nothing accepts a path, a command or a URL.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use darcbench_inventory::{Inventory, RedactionPolicy};
use darcbench_protocol::events::Envelope;
use darcbench_protocol::{ModuleId, Profile, RunId, ENDURANCE_MAX_MINUTES, ENDURANCE_MIN_MINUTES};
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};

use crate::config::AccessToken;
use crate::runner::{RunError, RunManager, AGENT_VERSION};
use crate::ui;

/// Name of the session cookie the UI exchanges its bootstrap token for.
const SESSION_COOKIE: &str = "darcbench_session";

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) manager: Arc<RunManager>,
    pub(crate) token: AccessToken,
    pub(crate) loopback_only: bool,
}

pub(crate) fn router(state: AppState) -> Router {
    Router::new()
        // --- unauthenticated -------------------------------------------
        .route("/healthz", get(healthz))
        .route("/api/v1/meta", get(meta))
        // --- authenticated ----------------------------------------------
        .route("/api/v1/session", post(create_session))
        .route("/api/v1/inventory", get(inventory))
        .route("/api/v1/profiles", get(profiles))
        .route("/api/v1/modules", get(modules))
        .route("/api/v1/runs", get(list_runs).post(create_run))
        .route("/api/v1/runs/{run_id}", get(get_run))
        .route("/api/v1/runs/{run_id}/cancel", post(cancel_run))
        .route("/api/v1/runs/{run_id}/events", get(stream_events))
        .route("/api/v1/runs/{run_id}/bundle", get(get_bundle))
        .route("/api/v1/runs/{run_id}/report", get(get_report))
        .route(
            "/api/v1/runs/{baseline}/compare/{candidate}",
            get(compare_runs),
        )
        // --- static UI -----------------------------------------------------
        .fallback(get(serve_ui))
        .layer(axum::middleware::from_fn(security_headers))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

/// What a request proved about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Auth {
    /// Presented the token in the `Authorization` header. Permitted to mutate.
    Header,
    /// Presented the token in the session cookie or query string. Read-only.
    Ambient,
}

fn authenticate(
    headers: &HeaderMap,
    query_token: Option<&str>,
    token: &AccessToken,
) -> Option<Auth> {
    if let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(bearer) = value.strip_prefix("Bearer ") {
            if token.matches(bearer.trim()) {
                return Some(Auth::Header);
            }
        }
    }
    if let Some(cookie) = headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) {
        for part in cookie.split(';') {
            if let Some((name, value)) = part.trim().split_once('=') {
                if name == SESSION_COOKIE && token.matches(value) {
                    return Some(Auth::Ambient);
                }
            }
        }
    }
    if let Some(candidate) = query_token {
        if token.matches(candidate) {
            return Some(Auth::Ambient);
        }
    }
    None
}

fn require(
    headers: &HeaderMap,
    query: &TokenQuery,
    state: &AppState,
    mutating: bool,
) -> Result<(), ApiError> {
    match authenticate(headers, query.token.as_deref(), &state.token) {
        Some(Auth::Header) => Ok(()),
        Some(Auth::Ambient) if !mutating => Ok(()),
        Some(Auth::Ambient) => Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "csrf_protection",
            "Mutating requests must present the token in the Authorization header, not a cookie \
             or query string.",
        )),
        None => Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "A valid DARCBench access token is required.",
        )),
    }
}

/// Whether the browser reached us over TLS, as reported by a terminating proxy.
///
/// Only ever used to decide whether to mark our own cookie `Secure`, never as an
/// authorisation input - so a client that lies about it gains nothing. The
/// header may carry a comma-separated chain; the first entry is the client-facing
/// hop.
fn request_is_over_tls(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(',').next().unwrap_or("").trim())
        .is_some_and(|proto| proto.eq_ignore_ascii_case("https"))
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct TokenQuery {
    pub(crate) token: Option<String>,
    /// SSE replay position, mirroring the `Last-Event-ID` header.
    pub(crate) last_event_id: Option<u64>,
}

// ---------------------------------------------------------------------------
// Error model
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub(crate) struct ApiError {
    #[serde(skip)]
    status: StatusCode,
    /// Stable machine-readable code. Clients branch on this, never on `message`.
    pub(crate) code: String,
    pub(crate) message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) detail: Option<String>,
}

impl ApiError {
    fn new(status: StatusCode, code: &str, message: &str) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
            detail: None,
        }
    }

    fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self)).into_response()
    }
}

impl From<RunError> for ApiError {
    fn from(error: RunError) -> Self {
        match error {
            RunError::AlreadyRunning(id) => ApiError::new(
                StatusCode::CONFLICT,
                "run_in_progress",
                "Another benchmark run is already in progress. Two concurrent runs would \
                 measure each other.",
            )
            .with_detail(id.to_string()),
            RunError::UnknownModules(ref list) => ApiError::new(
                StatusCode::BAD_REQUEST,
                "unknown_module",
                "One or more requested modules are not in this agent's allow-list.",
            )
            .with_detail(list.clone()),
            RunError::NoModules(profile) => ApiError::new(
                StatusCode::BAD_REQUEST,
                "profile_unavailable",
                "This profile has no implemented modules in this build.",
            )
            .with_detail(profile.to_string()),
            other => ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "The agent could not complete the request.",
            )
            .with_detail(other.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn healthz() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok", "agent_version": AGENT_VERSION }))
}

/// Unauthenticated capability advertisement.
///
/// Deliberately contains nothing about the machine: an unauthenticated caller
/// learns that a DARCBench agent is here and which protocol it speaks, and
/// nothing else.
async fn meta(State(state): State<AppState>) -> impl IntoResponse {
    Json(serde_json::json!({
        "product": "DARCBench",
        "agent_version": AGENT_VERSION,
        "protocol": darcbench_protocol::PROTOCOL_VERSION,
        "bundle_schema": darcbench_protocol::BUNDLE_SCHEMA_VERSION,
        "scoring_model": state.manager.model().version,
        "scoring_calibrated": state.manager.model().reference.calibrated,
        "authentication_required": true,
        "loopback_only": state.loopback_only,
    }))
}

/// Exchanges a bootstrap token for a session cookie.
///
/// The UI calls this once on load with `?token=` from the URL, then strips the
/// token from the address bar. That keeps the secret out of subsequent
/// `Referer` headers, browser history entries and copy-pasted links.
async fn create_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<TokenQuery>,
) -> Result<Response, ApiError> {
    // Read-only auth is sufficient here: this endpoint grants nothing the
    // caller did not already prove it holds.
    require(&headers, &query, &state, false)?;

    let mut cookie = format!(
        "{SESSION_COOKIE}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age=86400",
        state.token.expose()
    );
    // `Secure` must reflect how the *browser* reached us, not how the agent
    // bound its socket. The agent always speaks plain HTTP; TLS is terminated
    // by a tunnel or a reverse proxy, which announces it with
    // `X-Forwarded-Proto`. Setting `Secure` on a plain-HTTP connection makes the
    // browser silently discard the cookie, and the SSE stream - which can only
    // authenticate by cookie - then fails with 401. Getting this backwards
    // breaks the dashboard for every non-loopback HTTP deployment.
    if request_is_over_tls(&headers) {
        cookie.push_str("; Secure");
    }

    let mut response = Json(serde_json::json!({ "ok": true })).into_response();
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    Ok(response)
}

#[derive(Debug, Deserialize)]
pub(crate) struct InventoryQuery {
    #[serde(flatten)]
    pub(crate) auth: TokenQuery,
    /// Opt-in to unredacted output. Only honoured on a loopback bind.
    #[serde(default)]
    pub(crate) include_sensitive: bool,
}

async fn inventory(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<InventoryQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require(&headers, &query.auth, &state, false)?;

    let inventory = Inventory::collect();
    // Revealing identifying fields is only ever possible for a local operator
    // on a loopback bind. Over a tunnel the answer is always redacted.
    let policy = if query.include_sensitive && state.loopback_only {
        RedactionPolicy::Reveal
    } else {
        RedactionPolicy::Redact
    };
    let value =
        darcbench_inventory::redact::with_policy(policy, || serde_json::to_value(&inventory))
            .map_err(|e| {
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "inventory encoding failed",
                )
                .with_detail(e.to_string())
            })?;

    Ok(Json(serde_json::json!({
        "inventory": value,
        "redacted": policy == RedactionPolicy::Redact,
        "performance_digest": inventory.performance_digest(),
    })))
}

async fn profiles(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<TokenQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require(&headers, &query, &state, false)?;
    let registry = state.manager.registry();
    let entries: Vec<serde_json::Value> = [
        Profile::Quick,
        Profile::Standard,
        Profile::Deep,
        Profile::Endurance,
        Profile::ReadOnly,
        Profile::WebOnly,
    ]
    .into_iter()
    .map(|profile| {
        let modules = registry.modules_for_profile(profile);
        let (min, max) = profile.nominal_duration_minutes();
        serde_json::json!({
            "key": profile.as_str(),
            "standard": profile.is_standard(),
            "nominal_minutes": [min, max],
            "modules": modules.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "available": !modules.is_empty(),
        })
    })
    .collect();
    Ok(Json(serde_json::json!({ "profiles": entries })))
}

async fn modules(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<TokenQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require(&headers, &query, &state, false)?;
    Ok(Json(
        serde_json::json!({ "modules": state.manager.registry().manifests() }),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreateRunRequest {
    #[serde(default)]
    pub(crate) profile: Option<String>,
    /// Explicit module selection. Any value here forces the run to be `Custom`.
    #[serde(default)]
    pub(crate) modules: Option<Vec<String>>,
    /// Proceed despite non-blocking preflight warnings. Never clears a blocker.
    #[serde(default)]
    pub(crate) force: bool,
    /// How long a cycling profile keeps repeating its module set.
    ///
    /// Only `endurance` cycles, and any value here forces the run to `Custom`
    /// for the same reason an explicit module list does: an endurance run of a
    /// different length measures a different amount of decline, so it is not
    /// comparable with one that ran the standard hour.
    #[serde(default)]
    pub(crate) duration_minutes: Option<u32>,
}

async fn create_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<TokenQuery>,
    Json(request): Json<CreateRunRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    require(&headers, &query, &state, true)?;

    let profile = match request.profile.as_deref() {
        Some(raw) => raw.parse::<Profile>().map_err(|e| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "unknown_profile",
                "Unknown profile.",
            )
            .with_detail(e.to_string())
        })?,
        None => Profile::Quick,
    };

    let modules = match request.modules {
        Some(raw) => {
            let mut parsed = Vec::with_capacity(raw.len());
            for name in raw {
                parsed.push(name.parse::<ModuleId>().map_err(|e| {
                    ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "invalid_module_id",
                        "A module id was not well formed.",
                    )
                    .with_detail(e.to_string())
                })?);
            }
            Some(parsed)
        }
        None => None,
    };
    let duration = match request.duration_minutes {
        Some(minutes) => {
            // Only a profile that cycles by nature may have its duration set.
            //
            // Without this the field is a general-purpose "run this workload for
            // N hours" switch: `{"profile":"quick","duration_minutes":1440}`
            // would resolve the quick module set, turn the run `Custom`, and
            // then cycle it - `storage.mixed` fixture and all - for a day. The
            // range check above bounds the number but not what it is applied
            // to, and the whole point of bounding it was that this tool must
            // not be able to hold somebody's server at full load.
            if profile.cycle_target_minutes().is_none() {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "duration_not_supported",
                    "Only a cycling profile has a duration to override.",
                )
                .with_detail(format!(
                    "`{profile}` runs its module set once, so `duration_minutes` has no meaning \
                     for it. Today `endurance` is the only cycling profile."
                )));
            }
            if !(ENDURANCE_MIN_MINUTES..=ENDURANCE_MAX_MINUTES).contains(&minutes) {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "invalid_duration",
                    "The requested duration is outside the permitted range.",
                )
                .with_detail(format!(
                    "duration_minutes must be between {ENDURANCE_MIN_MINUTES} and \
                     {ENDURANCE_MAX_MINUTES}; below the floor there are too few cycles for a \
                     decline to mean anything, and the ceiling exists so a mistyped value cannot \
                     hold a machine at full load for a week."
                )));
            }
            Some(std::time::Duration::from_secs(u64::from(minutes) * 60))
        }
        None => None,
    };

    // A hand-picked module list is never a standard run, whatever profile was
    // named - letting one claim `standard` is the easiest way to game a
    // benchmark suite. A non-standard duration is the same problem in a
    // different dimension: two endurance runs of different lengths have been
    // given different amounts of time to decline, so neither ranks against the
    // other.
    let modules = if duration.is_some() && modules.is_none() {
        // The module set still comes from the profile the caller asked for.
        // Resolving it here rather than letting `Custom` pick means asking for a
        // shorter endurance run gets a shorter *endurance* run, not the custom
        // module set.
        Some(state.manager.registry().modules_for_profile(profile))
    } else {
        modules
    };
    let profile = if modules.is_some() {
        Profile::Custom
    } else {
        profile
    };

    let handle = state
        .manager
        .start(profile, modules, request.force, duration)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "run_id": handle.id,
            "profile": handle.profile,
            "modules": handle.modules,
            "events_url": format!("/api/v1/runs/{}/events", handle.id),
        })),
    ))
}

async fn list_runs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<TokenQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require(&headers, &query, &state, false)?;
    // Live runs first, then the index. The two answer different questions and
    // both belong in one list: the manager knows about the run happening right
    // now, which has no bundle yet and therefore no index row, and the index
    // knows about every run this state directory has ever completed - including
    // those executed by a previous process, which the manager has never heard
    // of. Listing only the manager was the old behaviour, and it meant a fresh
    // `serve` reported zero runs next to five hundred bundles on disk.
    let live = state.manager.list();
    let known: std::collections::HashSet<String> =
        live.iter().map(|run| run.run_id.to_string()).collect();
    let mut runs: Vec<serde_json::Value> = live
        .iter()
        .map(|run| serde_json::to_value(run).unwrap_or(serde_json::Value::Null))
        .collect();
    for indexed in state
        .manager
        .index()
        .list(RUN_LIST_LIMIT)
        .unwrap_or_default()
    {
        if known.contains(&indexed.run_id) {
            continue;
        }
        // `state` comes from the row, never a hardcoded "completed". A run the
        // watchdog stopped or an operator cancelled still writes a bundle, and
        // reporting it as completed would erase the one distinction
        // `stopped_because` exists to preserve - a run that ended early and
        // cannot say why is indistinguishable from one that finished.
        runs.push(serde_json::json!({
            "run_id": indexed.run_id,
            "profile": indexed.profile,
            "state": indexed.run_state.to_lowercase(),
            "created_at": indexed.finished_at,
            "finished_at": indexed.finished_at,
            "modules": indexed.modules,
            "progress": 1.0,
            "total_score": indexed.total_score,
            "result_state": indexed.result_state,
            "stopped_because": indexed.stopped_because,
        }));
    }
    Ok(Json(serde_json::json!({ "runs": runs })))
}

/// How many historical runs the list endpoint returns.
///
/// Bounded because this is rendered in a browser on the machine under test: an
/// agent that has run nightly for a year would otherwise serialise thousands of
/// rows into a page nobody scrolls. Pagination belongs with the fleet views in
/// Phase 7.
const RUN_LIST_LIMIT: usize = 200;

/// `GET /api/v1/runs/{baseline}/compare/{candidate}`.
///
/// Answers from the index, so it costs two indexed lookups rather than parsing
/// two complete bundles.
async fn compare_runs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((baseline, candidate)): Path<(String, String)>,
    Query(query): Query<TokenQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require(&headers, &query, &state, false)?;
    let baseline = parse_run_id(&baseline)?;
    let candidate = parse_run_id(&candidate)?;

    let comparison = state
        .manager
        .index()
        .compare(baseline.as_str(), candidate.as_str())
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "The run index could not answer this comparison.",
            )
            .with_detail(error.to_string())
        })?;
    let Some(comparison) = comparison else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "unknown_run",
            "One or both runs are not in this agent's state directory.",
        ));
    };

    Ok(Json(serde_json::json!({
        "baseline": {
            "run_id": comparison.baseline.run_id,
            "profile": comparison.baseline.profile,
            "finished_at": comparison.baseline.finished_at,
            "total_score": comparison.baseline.total_score,
        },
        "candidate": {
            "run_id": comparison.candidate.run_id,
            "profile": comparison.candidate.profile,
            "finished_at": comparison.candidate.finished_at,
            "total_score": comparison.candidate.total_score,
        },
        "comparable": comparison.comparable,
        "incomparable_reasons": comparison.incomparable_reasons,
        "metrics": comparison.metrics.iter().map(|delta| serde_json::json!({
            "module": delta.module,
            "metric_key": delta.metric_key,
            "unit": delta.unit,
            "baseline": delta.baseline,
            "candidate": delta.candidate,
            "ratio": delta.ratio,
        })).collect::<Vec<_>>(),
        "unmatched": comparison.unmatched,
    })))
}

fn parse_run_id(raw: &str) -> Result<RunId, ApiError> {
    raw.parse::<RunId>().map_err(|e| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_run_id",
            "Malformed run id.",
        )
        .with_detail(e.to_string())
    })
}

async fn get_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(run_id): Path<String>,
    Query(query): Query<TokenQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require(&headers, &query, &state, false)?;
    let id = parse_run_id(&run_id)?;
    if let Some(handle) = state.manager.get(&id) {
        return Ok(Json(serde_json::json!({
            "summary": handle.summary(),
            "last_seq": handle.last_seq(),
        })));
    }
    // Not a run of this process. The index knows every run this state directory
    // has completed, so answer from the row in the same shape `list_runs` uses
    // - otherwise the list shows a run whose detail view 404s.
    //
    // `last_seq` is null rather than counted: for a finished run it exists only
    // to resume an SSE stream, the clients derive their position from the events
    // themselves, and reading the whole log to produce a number nobody consumes
    // is not worth a file read per request.
    let indexed = state
        .manager
        .index()
        .get(id.as_str())
        .ok()
        .flatten()
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "unknown_run", "No such run."))?;
    Ok(Json(serde_json::json!({
        "summary": {
            "run_id": indexed.run_id,
            "profile": indexed.profile,
            "state": indexed.run_state.to_lowercase(),
            "created_at": indexed.finished_at,
            "finished_at": indexed.finished_at,
            "modules": indexed.modules,
            "progress": 1.0,
            "total_score": indexed.total_score,
            "result_state": indexed.result_state,
            "stopped_because": indexed.stopped_because,
        },
        "last_seq": serde_json::Value::Null,
    })))
}

async fn cancel_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(run_id): Path<String>,
    Query(query): Query<TokenQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require(&headers, &query, &state, true)?;
    let id = parse_run_id(&run_id)?;
    let handle = state
        .manager
        .get(&id)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "unknown_run", "No such run."))?;
    handle.cancel();
    Ok(Json(
        serde_json::json!({ "run_id": handle.id, "cancelling": true }),
    ))
}

async fn get_bundle(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(run_id): Path<String>,
    Query(query): Query<TokenQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require(&headers, &query, &state, false)?;
    let id = parse_run_id(&run_id)?;
    let bundle = match state.manager.get(&id) {
        Some(handle) => handle.bundle().ok_or_else(|| {
            ApiError::new(
                StatusCode::CONFLICT,
                "run_incomplete",
                "The run has not finished, so there is no result bundle yet.",
            )
        })?,
        // A run from an earlier process. Its bundle is on disk; see
        // `RunManager::stored_bundle`.
        None => state
            .manager
            .stored_bundle(&id)
            .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "unknown_run", "No such run."))?,
    };
    serde_json::to_value(&bundle).map(Json).map_err(|e| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "bundle encoding failed",
        )
        .with_detail(e.to_string())
    })
}

async fn get_report(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(run_id): Path<String>,
    Query(query): Query<TokenQuery>,
) -> Result<Response, ApiError> {
    require(&headers, &query, &state, false)?;
    let id = parse_run_id(&run_id)?;
    let bundle = match state.manager.get(&id) {
        Some(handle) => handle.bundle().ok_or_else(|| {
            ApiError::new(
                StatusCode::CONFLICT,
                "run_incomplete",
                "The run has not finished.",
            )
        })?,
        None => state
            .manager
            .stored_bundle(&id)
            .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "unknown_run", "No such run."))?,
    };
    Ok((
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        darcbench_report::html::render(&bundle),
    )
        .into_response())
}

/// Boxed so the live and the replayed-from-disk paths can share one return
/// type. Both are `Stream`s of the same items; only their origin differs.
type EventStream =
    std::pin::Pin<Box<dyn Stream<Item = Result<SseEvent, std::convert::Infallible>> + Send>>;

/// One protocol envelope as one SSE frame, `seq` carried as the event id so a
/// reconnecting client can resume with `Last-Event-ID`.
fn sse_from(envelope: &Envelope) -> SseEvent {
    let payload = serde_json::to_string(envelope).unwrap_or_else(|_| "{}".to_string());
    SseEvent::default()
        .id(envelope.seq.to_string())
        .event(envelope.kind())
        .data(payload)
}

/// Server-Sent Events stream for a run.
///
/// Replay: a reconnecting client sends `Last-Event-ID` (or `?last_event_id=`)
/// and receives every event after that sequence number, then joins the live
/// stream. If the requested position has fallen out of the replay buffer the
/// client is told to refetch rather than handed an undetectable gap.
async fn stream_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(run_id): Path<String>,
    Query(query): Query<TokenQuery>,
) -> Result<Response, ApiError> {
    require(&headers, &query, &state, false)?;
    let id = parse_run_id(&run_id)?;

    let last_seen = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .or(query.last_event_id);

    // A run this process did not start has no live stream to join, because the
    // process that ran it has exited. Its recorded log is the whole answer, so
    // replay it and close - rather than 404 for a run the list just showed.
    let Some(handle) = state.manager.get(&id) else {
        let replay = state.manager.stored_events(&id).ok_or_else(|| {
            if state.manager.stored_bundle(&id).is_some() {
                // The result survived and its log did not. Say which, rather
                // than reporting the run as absent when it plainly is not.
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "event_log_unreadable",
                    "The run exists and its bundle is readable, but `events.ndjson` is missing \
                     or contains a record that could not be decoded.",
                )
            } else {
                ApiError::new(StatusCode::NOT_FOUND, "unknown_run", "No such run.")
            }
        })?;
        let stream = tokio_stream::iter(
            replay
                .into_iter()
                .filter(move |envelope| last_seen.is_none_or(|seen| envelope.seq > seen))
                .map(|envelope| Ok(sse_from(&envelope))),
        );
        // No keep-alive: this stream ends on its own once the log is drained.
        return Ok(Sse::new(Box::pin(stream) as EventStream).into_response());
    };

    // Subscribe *before* snapshotting the backlog. In the other order, an event
    // emitted between the snapshot and the subscription is in neither - and on a
    // reconnect near the end of a run that lost event is typically
    // `report.generated` or `run.completed`, so the dashboard would sit at
    // "running" forever. Overlap is harmless: duplicates are dropped by sequence
    // number below, and the stream is idempotent per `seq`.
    let live = handle.subscribe();
    let backlog = handle.events_since(last_seen).ok_or_else(|| {
        ApiError::new(
            StatusCode::GONE,
            "replay_unavailable",
            "Events from that position are no longer buffered. Refetch the run instead of \
             resuming, so you do not silently miss events.",
        )
    })?;
    let highest_replayed = backlog.last().map(|e| e.seq);

    let backlog_stream = tokio_stream::iter(backlog.into_iter().map(Ok::<_, ()>));
    let live_stream = BroadcastStream::new(live).filter_map(move |item| match item {
        Ok(envelope) => match highest_replayed {
            Some(seq) if envelope.seq <= seq => None,
            _ => Some(Ok(envelope)),
        },
        // A lagged receiver means the client is too slow. Ending the stream is
        // correct: it forces a reconnect with `Last-Event-ID`, which is the
        // path that can actually recover the missed events.
        Err(_) => None,
    });

    let stream = backlog_stream
        .chain(live_stream)
        .map(|item: Result<_, ()>| match item {
            Ok(envelope) => Ok(sse_from(&envelope)),
            Err(()) => Ok(SseEvent::default().event("stream.error").data("{}")),
        });

    Ok(Sse::new(Box::pin(stream) as EventStream)
        .keep_alive(
            KeepAlive::new()
                .interval(std::time::Duration::from_secs(15))
                .text("keepalive"),
        )
        .into_response())
}

async fn serve_ui(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    // The shell itself is not secret; the API behind it is. Serving it
    // unauthenticated lets the page load and then exchange its bootstrap
    // token, which is the flow the UI is built around.
    let _ = (&state, &headers);
    ui::serve(uri.path())
}

/// Response headers applied to everything the agent serves.
async fn security_headers(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    // A strict CSP: no external anything, no inline event handlers. The UI is
    // fully self-hosted, so nothing legitimate needs a wider policy. `connect-src
    // 'self'` also blocks a compromised script from exfiltrating results.
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
             img-src 'self' data:; font-src 'self'; connect-src 'self'; \
             frame-ancestors 'none'; base-uri 'none'; form-action 'none'; object-src 'none'",
        ),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, no-cache, must-revalidate"),
    );
    if let Ok(name) = header::HeaderName::from_bytes(b"permissions-policy") {
        headers.insert(
            name,
            HeaderValue::from_static(
                "camera=(), microphone=(), geolocation=(), interest-cohort=()",
            ),
        );
    }
    response
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;

    fn token() -> AccessToken {
        AccessToken::from_string("a".repeat(64))
    }

    fn headers_with(name: header::HeaderName, value: &str) -> HeaderMap {
        let mut map = HeaderMap::new();
        if let Ok(v) = HeaderValue::from_str(value) {
            map.insert(name, v);
        }
        map
    }

    #[test]
    fn bearer_header_grants_mutating_access() {
        let token = token();
        let headers = headers_with(header::AUTHORIZATION, &format!("Bearer {}", token.expose()));
        assert_eq!(authenticate(&headers, None, &token), Some(Auth::Header));
    }

    #[test]
    fn cookie_grants_only_read_access() {
        let token = token();
        let headers = headers_with(
            header::COOKIE,
            &format!("{SESSION_COOKIE}={}", token.expose()),
        );
        assert_eq!(authenticate(&headers, None, &token), Some(Auth::Ambient));
    }

    #[test]
    fn query_token_grants_only_read_access() {
        let token = token();
        assert_eq!(
            authenticate(&HeaderMap::new(), Some(token.expose()), &token),
            Some(Auth::Ambient)
        );
    }

    #[test]
    fn a_wrong_token_is_rejected_everywhere() {
        let token = token();
        let wrong = "b".repeat(64);
        assert_eq!(
            authenticate(
                &headers_with(header::AUTHORIZATION, &format!("Bearer {wrong}")),
                None,
                &token
            ),
            None
        );
        assert_eq!(
            authenticate(
                &headers_with(header::COOKIE, &format!("{SESSION_COOKIE}={wrong}")),
                None,
                &token
            ),
            None
        );
        assert_eq!(authenticate(&HeaderMap::new(), Some(&wrong), &token), None);
        assert_eq!(authenticate(&HeaderMap::new(), None, &token), None);
    }

    #[test]
    fn malformed_authorization_schemes_are_rejected() {
        let token = token();
        for value in [
            token.expose().to_string(),
            format!("Basic {}", token.expose()),
            format!("bearer {}", token.expose()),
            "Bearer".to_string(),
        ] {
            assert_eq!(
                authenticate(&headers_with(header::AUTHORIZATION, &value), None, &token),
                None,
                "`{value}` must not authenticate"
            );
        }
    }

    #[test]
    fn cookies_are_parsed_out_of_a_multi_value_header() {
        let token = token();
        let headers = headers_with(
            header::COOKIE,
            &format!("other=1; {SESSION_COOKIE}={}; third=3", token.expose()),
        );
        assert_eq!(authenticate(&headers, None, &token), Some(Auth::Ambient));
    }

    /// The CSRF property, asserted directly: a browser can be made to send a
    /// cookie cross-site, but not a custom header. So cookie-authenticated
    /// mutation must be refused.
    #[test]
    fn cookie_auth_cannot_start_or_cancel_a_run() {
        let state = AppState {
            manager: Arc::new(RunManager::new(
                std::env::temp_dir().join("darcbench-authtest"),
                Arc::new(darcbench_report::AgentKey::generate().expect("keygen")),
            )),
            token: token(),
            loopback_only: true,
        };
        let cookie_headers = headers_with(
            header::COOKIE,
            &format!("{SESSION_COOKIE}={}", state.token.expose()),
        );
        let query = TokenQuery::default();

        require(&cookie_headers, &query, &state, false).expect("reads are allowed");
        let error = require(&cookie_headers, &query, &state, true)
            .expect_err("mutation via cookie must be refused");
        assert_eq!(error.status, StatusCode::FORBIDDEN);
        assert_eq!(error.code, "csrf_protection");
    }

    #[test]
    fn query_token_cannot_start_a_run_either() {
        let state = AppState {
            manager: Arc::new(RunManager::new(
                std::env::temp_dir().join("darcbench-authtest2"),
                Arc::new(darcbench_report::AgentKey::generate().expect("keygen")),
            )),
            token: token(),
            loopback_only: true,
        };
        let query = TokenQuery {
            token: Some(state.token.expose().to_string()),
            last_event_id: None,
        };
        assert!(require(&HeaderMap::new(), &query, &state, true).is_err());
        assert!(require(&HeaderMap::new(), &query, &state, false).is_ok());
    }

    #[test]
    fn unauthenticated_requests_get_401_not_403() {
        let state = AppState {
            manager: Arc::new(RunManager::new(
                std::env::temp_dir().join("darcbench-authtest3"),
                Arc::new(darcbench_report::AgentKey::generate().expect("keygen")),
            )),
            token: token(),
            loopback_only: true,
        };
        let error = require(&HeaderMap::new(), &TokenQuery::default(), &state, false)
            .expect_err("must reject");
        assert_eq!(error.status, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn run_ids_from_the_path_are_validated_before_use() {
        assert!(parse_run_id("run_0123456789abcdef0123456789abcdef").is_ok());
        for evil in ["../../etc/passwd", "run_../..", "", "run_short"] {
            assert!(parse_run_id(evil).is_err(), "`{evil}` must be rejected");
        }
    }

    /// Regression: the cookie was marked `Secure` whenever the agent bound a
    /// non-loopback address, but the agent serves plain HTTP - so the browser
    /// discarded the cookie and the SSE stream, which can only authenticate by
    /// cookie, failed with 401 on every `--bind 0.0.0.0` deployment.
    #[test]
    fn secure_flag_follows_the_browser_connection_not_the_bind_address() {
        assert!(
            !request_is_over_tls(&HeaderMap::new()),
            "plain HTTP must not get Secure"
        );
        assert!(!request_is_over_tls(&headers_with(
            header::HeaderName::from_static("x-forwarded-proto"),
            "http"
        )));
        assert!(request_is_over_tls(&headers_with(
            header::HeaderName::from_static("x-forwarded-proto"),
            "https"
        )));
        // Casing and proxy chains are both real in the wild.
        assert!(request_is_over_tls(&headers_with(
            header::HeaderName::from_static("x-forwarded-proto"),
            "HTTPS"
        )));
        assert!(request_is_over_tls(&headers_with(
            header::HeaderName::from_static("x-forwarded-proto"),
            "https, http"
        )));
        assert!(!request_is_over_tls(&headers_with(
            header::HeaderName::from_static("x-forwarded-proto"),
            "http, https"
        )));
    }

    #[test]
    fn run_error_maps_to_a_stable_api_code() {
        let id = RunId::try_new().expect("id");
        let error: ApiError = RunError::AlreadyRunning(id).into();
        assert_eq!(error.status, StatusCode::CONFLICT);
        assert_eq!(error.code, "run_in_progress");

        let error: ApiError = RunError::NoModules(Profile::WebOnly).into();
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert_eq!(error.code, "profile_unavailable");
    }
}
