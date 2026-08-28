//! `wordpress.site` - what a WordPress page costs on this machine.
//!
//! # What it measures
//!
//! | Metric | Unit | What it predicts |
//! |---|---|---|
//! | `origin.cold` | ms | The first request after a deploy or a restart: nothing warm anywhere |
//! | `origin.warm` | ms | Steady-state page render - what almost every visitor actually gets |
//! | `database.archive` | ms | A category archive: many posts, many terms, many queries in one render |
//! | `admin.dashboard` | ms | Authenticated `wp-admin` - the page the site's owner waits on all day |
//! | `origin.capacity` | req/s | How many renders the machine sustains - the half a latency figure cannot answer |
//!
//! # This is the module the product exists for
//!
//! `docs/MARKET-RESEARCH.md` names WordPress hosting as the segment DARCBench
//! is aimed at, and every other module here measures a *component* of that:
//! `cpu.mixed` measures the silicon, `php.runtime` measures the interpreter,
//! `database.oltp` measures a database, `web.static` measures serving bytes.
//! This one measures the thing an operator is actually buying, which is none of
//! those and all of them at once.
//!
//! That is also why it is the only module that runs somebody else's
//! application. Everywhere else the rule is that DARCBench supplies the
//! workload, because a run against the operator's software measures their
//! configuration. Here the whole question is *"how will WordPress run on this
//! machine"*, and there is no way to answer it with a proxy for WordPress.
//!
//! # Cache disclosure, which is the point rather than a footnote
//!
//! `docs/BENCHMARK-METHODOLOGY.md` is blunt about it: *"WordPress performance
//! without a cache disclosure is meaningless."* A number from a site with a
//! page cache in front and a number from one without differ by two orders of
//! magnitude, and both are honest until somebody puts them in the same table.
//!
//! So this module installs **no page cache and no object cache**, and says so
//! in the bundle rather than leaving it to be inferred. Every figure here is
//! what WordPress costs when it actually renders. The `cold` and `warm` pair is
//! not a cached-versus-uncached comparison and is deliberately not named as
//! one: what differs between them is PHP's opcode cache and the database's
//! buffer pool, not a page cache, and `origin.warm` is a page WordPress built
//! from scratch every single time.
//!
//! Adding a page cache would need a plugin, which would need a download from
//! wordpress.org at run time - an unpinned dependency this codebase refuses
//! everywhere else, and one whose version would silently change the number.
//!
//! # The stack is two containers, which is new here
//!
//! Every module before this one measured a single container.
//! [`crate::container`] grew a private per-run network for this, and the
//! reasoning for that - and for why the network is not `--internal` - is
//! recorded on `Runtime::create_network`.
//!
//! WordPress is prevented from reaching the internet by `WP_HTTP_BLOCK_EXTERNAL`
//! instead, which is its own switch for refusing every outbound request it
//! would otherwise make. Without it a core or plugin update check against
//! api.wordpress.org can land inside a measured request.
//!
//! # Where the content comes from
//!
//! [`crate::wordpress_fixture`], which is checksum-pinned, and it is inserted
//! by WordPress's own `wp_insert_post` through `wp eval-file` - see
//! `Fixture::to_php_import` for why that rather than `wp import`, and for the
//! escaping.
//!
//! **A WordPress that returned a setup screen for every request would produce
//! fast, meaningless numbers**, which Phase 4's exit criterion names in as many
//! words. So the install is verified before anything is timed, and verified by
//! evidence rather than by an exit code: the homepage has to be a page, of
//! plausible size, containing a title the fixture generated.
//!
//! # What is not measured
//!
//! **A second PHP version, a second web server, and a second database.** This
//! stack is Apache and PHP 8.3 against MariaDB, pinned. Sweeping those would
//! multiply the runtime by the number of combinations to answer a question an
//! operator can answer by choosing their host's options - and a comparison
//! across two of them is not one these numbers support, which is why every one
//! is in `comparability`.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use darcbench_protocol::metrics::{Direction, Metric, MetricSample, Warning, WarningCode};
use darcbench_protocol::stats::{outlier_indices, summarize, Summary};
use darcbench_protocol::ModuleId;

use crate::container::{ContainerError, Ephemeral, Image, Launch, Runtime, Sandbox};
use crate::module::{
    BenchmarkModule, ModuleError, ModuleManifest, ModuleOutput, ModuleParams, ModuleReporter,
    SafetyClass,
};
use crate::wordpress_fixture::{Fixture, FixtureSize, FIXTURE_VERSION};

/// Workload-definition version. Major bump = results are not comparable.
pub const VERSION: &str = "1.0.0";

/// The module's identifier, validated against the [`ModuleId`] grammar by a
/// unit test in this file.
pub const MODULE_ID: &str = "wordpress.site";

const DB_IMAGE_KEY: &str = "mariadb";
const WP_IMAGE_KEY: &str = "wordpress";
const CLI_IMAGE_KEY: &str = "wordpress-cli";

/// Database and administrator credentials the stack is created with.
///
/// Fixed rather than generated, for the reason `database.oltp` gives: the
/// containers are on loopback, they live for one module, and a generated
/// credential would protect nothing while making a failed run impossible to
/// reproduce by hand.
const DB_NAME: &str = "wordpress";
const DB_USER: &str = "wordpress";
const DB_PASSWORD: &str = "darcbench";
const DB_ROOT_PASSWORD: &str = "darcbench";
const ADMIN_USER: &str = "darcbench";
const ADMIN_PASSWORD: &str = "darcbench";

/// The corpus size this module measures against.
///
/// `Standard` - three hundred posts and a few hundred comments - rather than
/// `Small`. A thirty-post site fits entirely in any buffer pool and any
/// opcode cache, so it would measure a machine's cache hierarchy under a
/// workload nobody runs. It is fixed rather than chosen, because a corpus an
/// operator could select would make two runs incomparable while looking like
/// the same benchmark.
const CORPUS: FixtureSize = FixtureSize::Standard;

/// Timed requests per steady-state metric.
///
/// Nine, and the median is taken. A WordPress render is tens of milliseconds
/// on a good machine and hundreds on a bad one, which is the same order as a
/// garbage collection or a scheduler slice - so one sample is noise, and the
/// mean would follow whichever request collided with something.
const REQUESTS: usize = 9;

/// Untimed requests before each steady-state metric.
///
/// The opcode cache, the autoloaded options and the database's buffer pool all
/// warm on the first hit of a given path. `origin.cold` is where that cost is
/// measured deliberately; everywhere else it would be contamination.
///
/// Six rather than three, because three was not enough: with three, the first
/// steady-state metric measured after the cold start was still climbing out of
/// it. See the warm-up pass in `measure` for what that looked like.
const WARMUPS: usize = 6;

/// How long the whole stack gets to become a working site.
///
/// Generous: it covers MariaDB initialising a database, WordPress copying
/// itself into a volume, and the fixture being inserted post by post through
/// WordPress's API. On the machine this was written on that is about a minute;
/// on a small VPS with a slow disk it is several.
const SETUP_TIMEOUT: Duration = Duration::from_secs(900);

/// How long one HTTP request may take before it is abandoned.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// How long fetching the three images may take.
const PULL_TIMEOUT: Duration = Duration::from_secs(1800);

/// Workers for the capacity phase, clamped.
///
/// One per logical CPU. Fewer would leave the machine idle and measure the
/// generator; many more would measure Apache's process pool refusing
/// connections, which is a configuration rather than a capability.
const CAPACITY_WORKERS: (usize, usize) = (2, 32);

/// How long the capacity phase saturates the machine.
///
/// Ten seconds. A WordPress render is tens of milliseconds, so this is
/// hundreds of requests - enough that the rate is a rate rather than a sample,
/// and short enough that the suite's heaviest module does not also become its
/// longest.
const CAPACITY_SECONDS: u64 = 10;

/// Smallest response that could be a rendered WordPress page.
///
/// WordPress's own setup screen and its error pages are a few kilobytes; a
/// rendered page with a theme, a menu and post content is tens. This is the
/// floor under "this is a page rather than a message about why there is not
/// one", and it is checked before anything is recorded.
const PLAUSIBLE_PAGE_BYTES: usize = 20 * 1024;

// ---------------------------------------------------------------------------
// A very small HTTP client
// ---------------------------------------------------------------------------

/// What one request returned.
#[derive(Clone, Debug)]
pub(crate) struct Response {
    pub(crate) status: u16,
    pub(crate) body_bytes: usize,
    pub(crate) elapsed_ms: f64,
    /// Cookies the response set, as `name=value`, in the order they arrived.
    pub(crate) cookies: Vec<String>,
}

/// Reads the status line and the `Set-Cookie` headers out of a raw response.
///
/// Written against the format rather than with a regular expression, for the
/// same reason every other parser here is: a pattern that silently matched
/// nothing would produce a confident zero, and a status of zero would be
/// refused by the caller rather than published - which is only true because
/// the caller checks.
pub(crate) fn parse_response(raw: &[u8]) -> Option<(u16, usize, Vec<String>)> {
    // The body may be anything at all, so only the head is treated as text.
    // No blank line: the response was truncated, which is not a fast page.
    let at = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let body_len = raw.len().saturating_sub(at + 4);
    let head = String::from_utf8_lossy(&raw[..at]);

    // The version is whatever the server chose to answer with, which is not
    // always the one that was asked for: Apache answers an HTTP/1.0 request
    // with an HTTP/1.1 status line.
    let status_line = head.lines().next()?;
    let status: u16 = status_line
        .strip_prefix("HTTP/1.1 ")
        .or_else(|| status_line.strip_prefix("HTTP/1.0 "))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;

    let cookies = head
        .lines()
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("set-cookie")
                // Only the `name=value` pair; the attributes after the first
                // `;` are the browser's business and this is not one.
                .then(|| {
                    value
                        .trim()
                        .split(';')
                        .next()
                        .unwrap_or_default()
                        .to_string()
                })
        })
        .filter(|pair| pair.contains('='))
        .collect();

    Some((status, body_len, cookies))
}

/// Sends one HTTP request and times it.
///
/// HTTP/1.0 with `Connection: close`, which makes the server close the socket
/// when the body ends and removes any need to understand `Content-Length` or
/// chunked encoding here. The cost is a connection per request, which is the
/// honest shape anyway: this measures page *render* time, and a browser
/// arriving at a WordPress site pays a connection too.
fn request(
    address: SocketAddr,
    method: &str,
    path: &str,
    cookies: &[String],
    body: Option<&str>,
) -> Option<Response> {
    // **The port belongs in the `Host` header.** WordPress stores its own URL
    // - including the published port, because that is what it was installed
    // with - and canonical-redirects anything that arrives claiming a
    // different one. A `Host: 127.0.0.1` with the port left off got
    // `301 Moved Permanently` for every request, which is a redirect and
    // therefore the fastest page on the site.
    let mut head = format!(
        "{method} {path} HTTP/1.0\r\nHost: {address}\r\nConnection: close\r\n\
         User-Agent: darcbench\r\n"
    );
    if !cookies.is_empty() {
        head.push_str(&format!("Cookie: {}\r\n", cookies.join("; ")));
    }
    if let Some(body) = body {
        head.push_str("Content-Type: application/x-www-form-urlencoded\r\n");
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");

    let started = Instant::now();
    let mut stream = TcpStream::connect_timeout(&address, REQUEST_TIMEOUT).ok()?;
    stream.set_read_timeout(Some(REQUEST_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(REQUEST_TIMEOUT)).ok()?;
    stream.write_all(head.as_bytes()).ok()?;
    if let Some(body) = body {
        stream.write_all(body.as_bytes()).ok()?;
    }
    stream.flush().ok()?;

    // Read to the end, because the whole body is what the server had to
    // produce. Stopping at the headers would time how fast PHP can start
    // sending rather than how long the page took to build.
    let mut raw = Vec::with_capacity(128 * 1024);
    stream.read_to_end(&mut raw).ok()?;
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;

    let (status, body_bytes, cookies) = parse_response(&raw)?;
    Some(Response {
        status,
        body_bytes,
        elapsed_ms,
        cookies,
    })
}

/// Fetches a page and returns its body as text, for verification.
fn fetch_text(address: SocketAddr, path: &str, cookies: &[String]) -> Option<String> {
    let mut head = format!(
        "GET {path} HTTP/1.0\r\nHost: {address}\r\nConnection: close\r\nUser-Agent: darcbench\r\n"
    );
    if !cookies.is_empty() {
        head.push_str(&format!("Cookie: {}\r\n", cookies.join("; ")));
    }
    head.push_str("\r\n");
    let mut stream = TcpStream::connect_timeout(&address, REQUEST_TIMEOUT).ok()?;
    stream.set_read_timeout(Some(REQUEST_TIMEOUT)).ok()?;
    stream.write_all(head.as_bytes()).ok()?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).ok()?;
    Some(String::from_utf8_lossy(&raw).into_owned())
}

/// The site, as something the shared load generator can drive.
///
/// One connection per request rather than one held per worker, which
/// `LoadTarget` would allow. That is the honest shape here: this measures how
/// many *page renders* the machine sustains, and a WordPress render is three
/// orders of magnitude more expensive than the connection in front of it. A
/// pooled connection would shave a rounding error off a number dominated by
/// PHP and MariaDB.
struct SiteUnderLoad {
    address: SocketAddr,
}

impl crate::loadgen::LoadTarget for SiteUnderLoad {
    fn request(&self, _worker: usize) -> Result<u64, String> {
        // A failure has to be an error rather than a fast success, for the same
        // reason the latency phases refuse anything that is not a 200 of
        // plausible size: an error page and a redirect are the two fastest
        // things a WordPress can return, and counting them would report a
        // machine as fastest at the moment it broke.
        match request(self.address, "GET", "/", &[], None) {
            Some(response)
                if response.status == 200 && response.body_bytes >= PLAUSIBLE_PAGE_BYTES =>
            {
                Ok(response.body_bytes as u64)
            }
            Some(response) => Err(format!(
                "HTTP {} of {} bytes",
                response.status, response.body_bytes
            )),
            None => Err("no response".to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// The module
// ---------------------------------------------------------------------------

pub struct WordpressSite {
    manifest: ModuleManifest,
}

impl Default for WordpressSite {
    fn default() -> Self {
        Self::new()
    }
}

impl WordpressSite {
    pub fn new() -> Self {
        // Justified `expect`: `MODULE_ID` is a compile-time constant whose
        // conformance to the `ModuleId` grammar is asserted by a unit test in
        // this file, so this cannot fail in a built binary.
        #[allow(clippy::expect_used)]
        let id = ModuleId::new(MODULE_ID).expect("MODULE_ID is a valid module id");
        Self {
            manifest: ModuleManifest {
                id,
                version: VERSION.into(),
                title: "WordPress site".into(),
                purpose: "Measure what a WordPress page costs on this machine - cold, warm, an \
                          archive and the admin dashboard - against a WordPress and MariaDB this \
                          agent creates in containers and destroys when the module ends."
                    .into(),
                safety_class: SafetyClass::ProvisionsServices,
                dependencies: vec![
                    "A container runtime (Docker or Podman) whose daemon is reachable".into(),
                    "2 GiB of free memory: two sandboxed services, each with the tier's ceiling"
                        .into(),
                    "About 350 MB in the container runtime's storage for the WordPress volume"
                        .into(),
                ],
                // WordPress is the one entry in the image allow-list that is
                // not on a tmpfs: WP-CLI has to see the same files Apache is
                // serving, and a tmpfs is visible to exactly one container. So
                // this writes to the daemon's storage, like
                // `deployment.container` and for a different reason.
                max_bytes_written: 350 * 1024 * 1024,
                // Three images, once, on a machine that does not already have
                // them. Nothing crosses the network during the measurement:
                // the two containers share a private network and WordPress
                // itself is refused outbound HTTP by WP_HTTP_BLOCK_EXTERNAL.
                max_network_bytes: 266_322_653 + 106_220_996 + 69_069_790,
                cleanup: "Both containers, the WordPress volume and the run's private network are \
                          removed when the module ends, including on failure. Anything a crashed \
                          run leaves behind carries this agent's label and is removed at the \
                          start of the next run - containers first, then the network they were \
                          attached to."
                    .into(),
                validation: vec![
                    "The install is verified before anything is timed, by evidence rather than by \
                     an exit code: the homepage must be a page of plausible size containing a \
                     title the fixture generated. A WordPress serving a setup screen would \
                     produce fast, meaningless numbers."
                        .into(),
                    "The import must insert exactly the number of posts and comments the fixture \
                     contains, and report the fixture's own checksum back. A partial import is a \
                     different site."
                        .into(),
                    "The admin measurement must be authenticated. An unauthenticated wp-admin \
                     request is a redirect with an empty body, which would be the fastest page on \
                     the site and a measurement of nothing."
                        .into(),
                    "A metric needs a majority of its requests to have succeeded, and every \
                     response must be a 200 of plausible size."
                        .into(),
                ],
                limitations: vec![
                    "NO page cache and NO object cache are installed, and every figure here is \
                     what WordPress costs when it actually renders. docs/BENCHMARK-METHODOLOGY.md \
                     requires that disclosure because a cached and an uncached number differ by \
                     two orders of magnitude and both are honest until somebody puts them in one \
                     table."
                        .into(),
                    "`origin.cold` and `origin.warm` are NOT a cached-versus-uncached pair. What \
                     differs between them is PHP's opcode cache and the database's buffer pool; \
                     both are pages WordPress built from scratch."
                        .into(),
                    "One stack only: Apache with PHP 8.3 against MariaDB, all pinned by digest. A \
                     comparison across a different PHP version, web server or database is not one \
                     these numbers support, which is why each is declared in comparability."
                        .into(),
                    "The theme is whichever one the pinned WordPress ships as its default. A \
                     theme is a large part of what a render costs, so this measures WordPress's \
                     own theme rather than the operator's - the same choice web.static makes \
                     about serving its own origin rather than the operator's nginx."
                        .into(),
                    "The measuring process shares the machine with the stack it measures, so \
                     every figure is a floor. It is a much smaller share than in the load-driven \
                     modules - this issues one request at a time and spends the wait blocked - \
                     but it is not zero."
                        .into(),
                    "WordPress is served over plaintext HTTP on loopback. TLS termination is a \
                     real cost of running a site and is measured by web.static, which does it \
                     without a CMS in the way."
                        .into(),
                    "origin.capacity is closed-loop and deliberately so: workers looping flat out \
                     ask the machine for everything it will give, which is the capacity question, \
                     and the phase produces no latency distribution for coordinated omission to \
                     distort. It runs last, after every latency metric, so its saturation cannot \
                     reach them."
                        .into(),
                    "origin.capacity is a floor twice over. The generator shares the machine with \
                     the stack it drives, so on a small host it takes a share of the same cores \
                     Apache needs - which understates a machine with threads to spare by less \
                     than it understates one without."
                        .into(),
                ],
                comparability: vec![
                    "wordpress_image".into(),
                    "wordpress_version".into(),
                    "php_version".into(),
                    "mariadb_image".into(),
                    "mariadb_version".into(),
                    "fixture_version".into(),
                    "fixture_checksum".into(),
                    "object_cache".into(),
                    "page_cache".into(),
                    "opcache".into(),
                    "theme".into(),
                ],
                stability_cv_bound: 0.2,
            },
        }
    }
}

impl BenchmarkModule for WordpressSite {
    fn manifest(&self) -> &ModuleManifest {
        &self.manifest
    }

    /// Every request, both containers and the fixture insert happen where this
    /// process's CPU accounting cannot see them.
    fn workload_runs_outside_this_process(&self) -> bool {
        true
    }

    /// Two sandboxes, so twice the tier's ceiling.
    fn estimated_peak_memory_bytes(&self, _params: &ModuleParams) -> u64 {
        crate::container::sandbox_memory_budget_bytes() * 2
    }

    /// Dominated by setup rather than by measurement.
    ///
    /// The four metrics are about forty requests between them, which is
    /// seconds. What takes the time is standing the stack up: MariaDB
    /// initialising, WordPress unpacking itself into a volume, and three
    /// hundred posts going in one at a time through WordPress's API. Six
    /// minutes is comfortable on a fast machine and not obviously generous on
    /// a small VPS with a slow disk, which is the direction preflight should
    /// err in.
    fn estimated_duration_s(&self, _params: &ModuleParams) -> u64 {
        360
    }

    fn run(
        &self,
        _params: &ModuleParams,
        reporter: &dyn ModuleReporter,
    ) -> Result<ModuleOutput, ModuleError> {
        let images = StackImages::resolve()?;
        let runtime = Runtime::discover().map_err(not_measured)?;

        // Containers first, then the network they were attached to: a network
        // with a container on it refuses to be removed.
        let reaped = runtime.reap().map_err(not_measured)?;
        let _ = runtime.reap_networks();

        let fetched = images.ensure_present(&runtime)?;

        let run_id = unique_suffix();
        let network = runtime.create_network(&run_id).map_err(not_measured)?;
        let outcome = self.with_stack(
            &runtime, &images, &run_id, &network, reaped, fetched, reporter,
        );
        // After the sandboxes inside `with_stack` have dropped, which is what
        // makes the network removable at all.
        runtime.remove_network(&network);
        outcome
    }
}

/// The three allow-list entries this module runs, resolved together.
///
/// Together rather than one at a time, because a stack missing one of them is
/// not a partial measurement - it is no measurement, and an operator should
/// hear that before a container starts rather than after two of them have.
struct StackImages {
    database: &'static Image,
    web: &'static Image,
    cli: &'static Image,
}

impl StackImages {
    fn resolve() -> Result<Self, ModuleError> {
        let get = |key: &str| -> Result<&'static Image, ModuleError> {
            let image = Image::from_allow_list(key).ok_or_else(|| {
                ModuleError::Precondition(format!(
                    "`{key}` is not on the container image allow-list, so this module has \
                     nothing it is permitted to run."
                ))
            })?;
            image.reference().map_err(not_measured)?;
            Ok(image)
        };
        Ok(Self {
            database: get(DB_IMAGE_KEY)?,
            web: get(WP_IMAGE_KEY)?,
            cli: get(CLI_IMAGE_KEY)?,
        })
    }

    /// Fetches whichever are missing, before any clock starts.
    fn ensure_present(&self, runtime: &Runtime) -> Result<bool, ModuleError> {
        let mut fetched = false;
        for image in [self.database, self.web, self.cli] {
            fetched |= runtime
                .ensure_image_present(image, PULL_TIMEOUT)
                .map_err(not_measured)?;
        }
        Ok(fetched)
    }
}

impl WordpressSite {
    /// Everything that happens while both containers exist.
    ///
    /// Split out so their lifetimes are one scope: whatever happens in here,
    /// both sandboxes drop on the way out and `Drop` removes them.
    #[allow(clippy::too_many_arguments)]
    fn with_stack(
        &self,
        runtime: &Runtime,
        images: &StackImages,
        run_id: &str,
        network: &str,
        reaped: usize,
        fetched: bool,
        reporter: &dyn ModuleReporter,
    ) -> Result<ModuleOutput, ModuleError> {
        let db_env = vec![
            format!("MARIADB_ROOT_PASSWORD={DB_ROOT_PASSWORD}"),
            format!("MARIADB_DATABASE={DB_NAME}"),
            format!("MARIADB_USER={DB_USER}"),
            format!("MARIADB_PASSWORD={DB_PASSWORD}"),
        ];
        let _database = Sandbox::launch_with(
            runtime,
            images.database,
            run_id,
            &Launch {
                env: &db_env,
                network: Some(network),
                ..Launch::default()
            },
        )
        .map_err(not_measured)?;

        // The web container addresses the database by the network alias the
        // tier gives it, which is the image key.
        let wp_env = vec![
            format!("WORDPRESS_DB_HOST={}", images.database.key()),
            format!("WORDPRESS_DB_USER={DB_USER}"),
            format!("WORDPRESS_DB_PASSWORD={DB_PASSWORD}"),
            format!("WORDPRESS_DB_NAME={DB_NAME}"),
            // WordPress's own refusal to make outbound HTTP requests. Without
            // it a core or plugin update check can land inside a measured
            // request. See `Runtime::create_network` for why this rather than
            // an internal network.
            "WORDPRESS_CONFIG_EXTRA=define('WP_HTTP_BLOCK_EXTERNAL', true);".to_string(),
        ];
        // Waited on, unlike the first version of this: WP-CLI arriving while
        // the image's entrypoint is still writing `wp-config.php` fails with
        // `Strange wp-config.php file`, which is a true statement about a file
        // that was about to be finished.
        let web = Sandbox::launch_with(
            runtime,
            images.web,
            run_id,
            &Launch {
                env: &wp_env,
                network: Some(network),
                ..Launch::default()
            },
        )
        .map_err(not_measured)?;

        let address = web.address();
        let base = format!("http://127.0.0.1:{}", address.port());

        self.install(runtime, images, run_id, network, &wp_env, &base)?;
        let imported = self.import(runtime, images, run_id, network, &wp_env)?;

        // The cold request has to be the *first* request, so it is taken here
        // rather than after a verification pass that would have warmed
        // everything it was measuring. Its body is what the verification then
        // reads: one request, both jobs, and no way for the two to disagree
        // about which page was looked at.
        let cold = self.cold_request(address, &imported)?;

        self.measure(
            runtime, images, run_id, network, &wp_env, &web, cold, imported, reaped, fetched,
            reporter,
        )
    }

    /// The first request the site ever serves, timed, and checked.
    fn cold_request(&self, address: SocketAddr, imported: &Imported) -> Result<f64, ModuleError> {
        let started = Instant::now();
        let body = fetch_text(address, "/", &[]).ok_or_else(|| {
            ModuleError::Precondition(
                "the site did not answer a request for its homepage, so nothing is measured."
                    .into(),
            )
        })?;
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        self.verify(&body, imported)?;
        Ok(elapsed_ms)
    }

    /// Runs `wp core install`, then confirms WordPress agrees that it worked.
    fn install(
        &self,
        runtime: &Runtime,
        images: &StackImages,
        run_id: &str,
        network: &str,
        env: &[String],
        base: &str,
    ) -> Result<(), ModuleError> {
        let options = Ephemeral {
            env,
            network: Some(network),
            volumes_from: Some(&container_name(run_id, images.web.key())),
            stdin: None,
        };
        // `wp` explicitly rather than relying on the image's entrypoint to add
        // it. It does not always: the same invocation without it came back
        // `exec: core: not found`.
        let installed = runtime
            .run_ephemeral_with(
                images.cli,
                &format!("{run_id}-install"),
                &[
                    "wp",
                    "core",
                    "install",
                    &format!("--url={base}"),
                    "--title=darcbench",
                    &format!("--admin_user={ADMIN_USER}"),
                    &format!("--admin_password={ADMIN_PASSWORD}"),
                    "--admin_email=darcbench@example.invalid",
                    "--skip-email",
                ],
                &options,
                SETUP_TIMEOUT,
            )
            .map_err(not_measured)?;
        if !installed.succeeded() {
            return Err(ModuleError::Precondition(format!(
                "WordPress could not be installed, so there is nothing to measure: {}",
                first_line(&format!("{}{}", installed.stderr, installed.stdout))
            )));
        }
        Ok(())
    }

    /// Inserts the fixture and returns what WordPress says it inserted.
    fn import(
        &self,
        runtime: &Runtime,
        images: &StackImages,
        run_id: &str,
        network: &str,
        env: &[String],
    ) -> Result<Imported, ModuleError> {
        let fixture = Fixture::generate(CORPUS);
        let script = fixture.to_php_import();

        // `-` is WP-CLI's "read the script from standard input", which is what
        // lets a 1.6 MB fixture reach the container without a host path
        // appearing in an argument vector.
        let output = runtime
            .run_ephemeral_with(
                images.cli,
                &format!("{run_id}-import"),
                &["wp", "eval-file", "-"],
                &Ephemeral {
                    env,
                    network: Some(network),
                    volumes_from: Some(&container_name(run_id, images.web.key())),
                    stdin: Some(script.as_bytes()),
                },
                SETUP_TIMEOUT,
            )
            .map_err(not_measured)?;

        let reported =
            parse_import(&format!("{}{}", output.stdout, output.stderr)).ok_or_else(|| {
                ModuleError::Precondition(format!(
                    "the fixture was not imported, so the site is not the one this module \
                     measures: {}",
                    first_line(&format!("{}{}", output.stderr, output.stdout))
                ))
            })?;

        let expected_posts = fixture.posts.len();
        let expected_comments: usize = fixture.posts.iter().map(|p| p.comments.len()).sum();
        if reported.posts != expected_posts
            || reported.comments != expected_comments
            || reported.checksum != fixture.checksum()
        {
            // A partial import is a different site, and its numbers would be a
            // measurement of that site rather than of this machine.
            return Err(ModuleError::Precondition(format!(
                "the import inserted {} post(s) and {} comment(s) against an expected {} and {}, \
                 or reported checksum {} against {}. The site is not the fixture, so nothing is \
                 measured.",
                reported.posts,
                reported.comments,
                expected_posts,
                expected_comments,
                reported.checksum,
                fixture.checksum()
            )));
        }
        Ok(reported)
    }

    /// Confirms the site is a site, by looking at one.
    ///
    /// Phase 4's exit criterion in as many words: *verify the installation
    /// before recording anything*. An exit code from `wp core install` is not
    /// that - it says the command believed it succeeded - so this asks the
    /// server the same question a visitor would.
    fn verify(&self, body: &str, imported: &Imported) -> Result<(), ModuleError> {
        if !body.starts_with("HTTP/1.1 200") && !body.starts_with("HTTP/1.0 200") {
            return Err(ModuleError::Precondition(format!(
                "the homepage answered `{}` rather than 200. A site that redirects to its own \
                 setup screen renders fast and measures nothing.",
                first_line(body)
            )));
        }
        if body.len() < PLAUSIBLE_PAGE_BYTES {
            return Err(ModuleError::Precondition(format!(
                "the homepage was {} bytes, below the {PLAUSIBLE_PAGE_BYTES} a rendered page \
                 could plausibly be. An error page is small and quick.",
                body.len()
            )));
        }
        // The strongest of the three: the content this module inserted has to
        // be on the page it is about to time. A themed WordPress with an empty
        // database passes the two checks above.
        let fixture = Fixture::generate(CORPUS);
        let title = fixture
            .posts
            .iter()
            .find(|post| post.kind == "post")
            .map(|post| post.title.clone())
            .unwrap_or_default();
        if !title.is_empty() && !body.contains(title.as_str()) {
            return Err(ModuleError::Precondition(format!(
                "the homepage did not contain `{title}`, which the import said it had inserted \
                 ({} posts, checksum {}). The site is serving something other than the fixture.",
                imported.posts, imported.checksum
            )));
        }
        Ok(())
    }
}

impl WordpressSite {
    /// The four timed dimensions, and everything the bundle has to disclose.
    #[allow(clippy::too_many_arguments)]
    fn measure(
        &self,
        runtime: &Runtime,
        images: &StackImages,
        run_id: &str,
        network: &str,
        env: &[String],
        web: &Sandbox,
        cold_ms: f64,
        imported: Imported,
        reaped: usize,
        fetched: bool,
        reporter: &dyn ModuleReporter,
    ) -> Result<ModuleOutput, ModuleError> {
        let address = web.address();
        let mut metrics = Vec::new();
        let mut warnings = Vec::new();

        // One observation by definition: a site is only cold once, and running
        // it again would need the whole stack rebuilt. Published with no
        // distribution rather than with a fabricated one.
        metrics.push(Metric {
            key: "origin.cold".into(),
            label: "Homepage, cold".into(),
            unit: "ms".into(),
            value: cold_ms,
            direction: Direction::LowerIsBetter,
            summary: single(cold_ms),
            samples: Vec::new(),
            outliers: Vec::new(),
            measures_dispersion: false,
            tail_quantile: false,
        });

        if reporter.is_cancelled() {
            return Err(ModuleError::Cancelled);
        }

        // The archive path is query-string rather than pretty, because the
        // permalink structure a fresh WordPress installs with is the plain one
        // and rewriting it would measure a different set of queries than the
        // site actually ships with.
        let category = Fixture::generate(CORPUS)
            .categories
            .first()
            .map(|term| term.slug.clone())
            .unwrap_or_default();
        let archive = format!("/?category_name={category}");

        // **One warm-up pass over every path before any of them is timed.**
        //
        // Not an optimisation - a correction. The first version warmed each
        // path immediately before timing it, in order, so `origin.warm` was
        // measured while the stack was still climbing out of the cold start
        // that had just been measured deliberately. It came back at 94 ms with
        // a 64% coefficient of variation while the *archive* - a strictly
        // heavier page, timed a few seconds later - came back at 43 ms with
        // 5.5%. A warm page slower and six times noisier than a heavy one is
        // not a finding about the machine; it is a finding about the order.
        //
        // Warming everything first costs a few more requests and removes the
        // ordering bias entirely, which also means the three steady-state
        // metrics can be compared with each other rather than only across
        // machines.
        for path in ["/", archive.as_str()] {
            for _ in 0..WARMUPS {
                let _ = request(address, "GET", path, &[], None);
            }
        }

        for (key, label, path) in [
            ("origin.warm", "Homepage, warm", "/"),
            ("database.archive", "Category archive", archive.as_str()),
        ] {
            if reporter.is_cancelled() {
                return Err(ModuleError::Cancelled);
            }
            let samples = self.time_path(address, path, &[]);
            push_distribution(&mut metrics, &mut warnings, key, label, samples);
        }

        // Admin last, because it is the only phase that needs a session and the
        // only one whose failure is a failure of this module rather than of the
        // machine.
        if reporter.is_cancelled() {
            return Err(ModuleError::Cancelled);
        }
        match self.log_in(address) {
            Some(session) => {
                let samples = self.time_path(address, "/wp-admin/", &session);
                push_distribution(
                    &mut metrics,
                    &mut warnings,
                    "admin.dashboard",
                    "Admin dashboard",
                    samples,
                );
            }
            None => warnings.push(Warning {
                code: WarningCode::ValidationFailed,
                message: "could not sign in to wp-admin, so the admin dashboard is not measured. \
                          It is withheld rather than measured signed-out, because an \
                          unauthenticated wp-admin request is a redirect with an empty body - the \
                          fastest response the site can produce and a measurement of nothing."
                    .into(),
                metric_key: Some("admin.dashboard".into()),
            }),
        }

        // --- capacity ---------------------------------------------------------
        //
        // **Last, and that placement is the measurement's own precondition.**
        // This phase drives the machine to saturation; anything timed after it
        // would be timed against a machine still working through the queue it
        // left. The three latency metrics above are all taken first, on a
        // quiet stack, which is what makes them latency figures rather than
        // latency-under-some-load figures nobody declared.
        //
        // Closed-loop on purpose, and it is not a contradiction of everything
        // this codebase says about coordinated omission: workers looping flat
        // out ask the machine for everything it will give and report what it
        // gave. That is exactly the capacity question, and the phase produces
        // no latency distribution for the omission to distort. See
        // `loadgen::measure_capacity`.
        if reporter.is_cancelled() {
            return Err(ModuleError::Cancelled);
        }
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(CAPACITY_WORKERS.0)
            .clamp(CAPACITY_WORKERS.0, CAPACITY_WORKERS.1);
        let capacity = crate::loadgen::measure_capacity(
            &SiteUnderLoad { address },
            workers,
            Duration::from_secs(CAPACITY_SECONDS),
        );
        if capacity > 0.0 {
            metrics.push(Metric {
                key: "origin.capacity".into(),
                label: "Homepage renders sustained".into(),
                unit: "req/s".into(),
                value: capacity,
                direction: Direction::HigherIsBetter,
                summary: single(capacity),
                samples: Vec::new(),
                outliers: Vec::new(),
                measures_dispersion: false,
                tail_quantile: false,
            });
        } else {
            warnings.push(Warning {
                code: WarningCode::ValidationFailed,
                message: "the capacity phase served no successful request, so no rate is \
                          reported. A rate of zero would read as a machine that cannot serve \
                          WordPress at all, which is a claim this phase is not entitled to make \
                          on its own."
                    .into(),
                metric_key: Some("origin.capacity".into()),
            });
        }

        // The variance sweep, over the metric list rather than inside one
        // construction path - the shape `network.transfer` arrived at after
        // making the same promise true for some of its metrics and false for
        // the rest.
        for metric in &metrics {
            let Some(cv) = metric.summary.cv else {
                continue;
            };
            if metric.summary.is_unstable(self.manifest.stability_cv_bound) {
                warnings.push(Warning {
                    code: WarningCode::HighVariance,
                    message: format!(
                        "`{}` varied by {:.0}% across requests (bound {:.0}%). A WordPress render \
                         touches PHP, the database and the filesystem in one request, so this is \
                         usually the machine being shared rather than any one of the three being \
                         inconsistent.",
                        metric.key,
                        cv * 100.0,
                        self.manifest.stability_cv_bound * 100.0
                    ),
                    metric_key: Some(metric.key.clone()),
                });
            }
        }

        let context = self.disclosure(
            runtime, images, run_id, network, env, web, imported, reaped, fetched, workers,
        );
        Ok(ModuleOutput {
            metrics,
            warnings,
            context,
        })
    }

    /// Warms a path, then times it [`REQUESTS`] times.
    ///
    /// A response that is not a 200, or that is too small to be a page,
    /// contributes no sample. Both are the same failure wearing different
    /// clothes: an error page and a redirect are the two fastest things a
    /// WordPress can return, so admitting either would reward the site for
    /// breaking.
    fn time_path(&self, address: SocketAddr, path: &str, cookies: &[String]) -> Vec<f64> {
        // Warms this path again even though the caller warmed every path
        // already. The second pass is cheap and covers the case the first
        // cannot: a path that needs a session, which does not exist until
        // after the global pass has run.
        for _ in 0..WARMUPS {
            let _ = request(address, "GET", path, cookies, None);
        }
        (0..REQUESTS)
            .filter_map(|_| {
                let response = request(address, "GET", path, cookies, None)?;
                (response.status == 200 && response.body_bytes >= PLAUSIBLE_PAGE_BYTES)
                    .then_some(response.elapsed_ms)
            })
            .collect()
    }

    /// Signs in and returns the session cookies, or `None`.
    ///
    /// The same exchange a browser makes: a form POST to `wp-login.php`
    /// carrying the test cookie WordPress insists on, and the `Set-Cookie`
    /// headers it answers with.
    ///
    /// `None` rather than an error, because a failure here costs one metric
    /// and not the run - and because the caller says so in a warning instead
    /// of measuring a signed-out redirect.
    fn log_in(&self, address: SocketAddr) -> Option<Vec<String>> {
        // WordPress refuses a login unless the client demonstrates it keeps
        // cookies, by sending back one the login form set. Sending it up front
        // is what a browser does on its second attempt.
        let test_cookie = "wordpress_test_cookie=WP%20Cookie%20check".to_string();
        let body = format!(
            "log={ADMIN_USER}&pwd={ADMIN_PASSWORD}&wp-submit=Log+In&testcookie=1&redirect_to=%2Fwp-admin%2F"
        );
        let response = request(
            address,
            "POST",
            "/wp-login.php",
            std::slice::from_ref(&test_cookie),
            Some(&body),
        )?;

        // A successful login is a redirect that sets cookies. A *failed* one
        // is a 200 with the form again - which is why the status is checked
        // rather than only the cookies.
        if response.status != 302 {
            return None;
        }
        let session: Vec<String> = response
            .cookies
            .into_iter()
            .filter(|cookie| cookie.starts_with("wordpress_") || cookie.starts_with("wp-settings"))
            // A logged-out response also sets `wordpress_test_cookie`, and a
            // session made only of that is not a session.
            .filter(|cookie| !cookie.starts_with("wordpress_test_cookie"))
            .collect();
        if session.is_empty() {
            return None;
        }

        // Proven rather than assumed: wp-admin must answer 200 with a page,
        // where a signed-out request gets a 302 and no body.
        let probe = request(address, "GET", "/wp-admin/", &session, None)?;
        (probe.status == 200 && probe.body_bytes >= PLAUSIBLE_PAGE_BYTES).then_some(session)
    }

    /// Everything the bundle must record about what was measured.
    #[allow(clippy::too_many_arguments)]
    fn disclosure(
        &self,
        runtime: &Runtime,
        images: &StackImages,
        run_id: &str,
        network: &str,
        env: &[String],
        web: &Sandbox,
        imported: Imported,
        reaped: usize,
        fetched: bool,
        workers: usize,
    ) -> serde_json::Map<String, serde_json::Value> {
        // **Asked of the container that served the requests, not of WP-CLI.**
        // The first version of this asked WP-CLI for both, and both answers
        // were wrong in a way that mattered: WP-CLI is a *different image*, so
        // `php_version` came back 8.3.33 where Apache was running 8.3.31, and
        // `opcache` came back `disabled` because `opcache.enable_cli` is 0
        // while `opcache.enable` - the one that governs every measured request
        // - is 1.
        //
        // Both are comparability keys. A comparison layer refusing or allowing
        // a comparison on a false fact is worse than one with no fact at all.
        let ask_web = |argv: &[&str]| -> String {
            web.exec(argv, SETUP_TIMEOUT)
                .ok()
                .filter(|output| output.succeeded())
                .map(|output| first_line(&output.stdout))
                .unwrap_or_else(|| "unknown".to_string())
        };

        let ask = |argv: &[&str]| -> String {
            runtime
                .run_ephemeral_with(
                    images.cli,
                    &format!("{run_id}-ask"),
                    argv,
                    &Ephemeral {
                        env,
                        network: Some(network),
                        volumes_from: Some(&container_name(run_id, images.web.key())),
                        stdin: None,
                    },
                    SETUP_TIMEOUT,
                )
                .ok()
                .filter(|output| output.succeeded())
                .map(|output| first_line(&output.stdout))
                .unwrap_or_else(|| "unknown".to_string())
        };

        let mut context = serde_json::Map::new();
        for (key, value) in [
            (
                "wordpress_image",
                images.web.reference().unwrap_or("unknown").to_string(),
            ),
            (
                "mariadb_image",
                images.database.reference().unwrap_or("unknown").to_string(),
            ),
            ("wordpress_version", ask(&["wp", "core", "version"])),
            ("php_version", ask_web(&["php", "-r", "echo PHP_VERSION;"])),
            (
                "mariadb_version",
                ask(&[
                    "wp",
                    "db",
                    "query",
                    "SELECT VERSION();",
                    "--skip-column-names",
                ]),
            ),
            (
                "theme",
                ask(&["wp", "theme", "list", "--status=active", "--field=name"]),
            ),
            (
                "opcache",
                // The directive rather than `opcache_get_status()`. The status
                // call reports the SAPI it is asked in, and asking from a
                // command line says nothing about Apache - `opcache.enable_cli`
                // is 0 in this image while `opcache.enable` is 1.
                match ask_web(&["php", "-r", "echo ini_get('opcache.enable') ?: '0';"]).as_str() {
                    "1" | "On" | "on" => "enabled for the web SAPI (opcache.enable=1), which is \
                                          the one that served every measured request"
                        .to_string(),
                    other => format!("opcache.enable={other} for the web SAPI"),
                },
            ),
            ("fixture_version", FIXTURE_VERSION.to_string()),
            ("fixture_checksum", imported.checksum.clone()),
            ("fixture_size", CORPUS.as_str().to_string()),
            (
                "object_cache",
                "none. No persistent object cache is installed, so every request rebuilds what a \
                 Redis or Memcached drop-in would have kept. docs/BENCHMARK-METHODOLOGY.md \
                 requires this disclosure: a cached and an uncached WordPress differ by two \
                 orders of magnitude."
                    .to_string(),
            ),
            (
                "page_cache",
                "none. Every measured request was rendered by PHP. Installing one would need a \
                 plugin downloaded from wordpress.org at run time, which is an unpinned \
                 dependency this project refuses everywhere else."
                    .to_string(),
            ),
            (
                "cold_versus_warm",
                "origin.cold is the first request the site ever served and origin.warm is its \
                 steady state. The difference is PHP's opcode cache and the database's buffer \
                 pool warming - NOT a page cache. Both are pages WordPress built from scratch."
                    .to_string(),
            ),
            (
                "outbound_http",
                "blocked. WP_HTTP_BLOCK_EXTERNAL is defined, so WordPress makes no request to \
                 api.wordpress.org or anywhere else during a measurement."
                    .to_string(),
            ),
            (
                "capacity_workers",
                format!(
                    "{workers} closed-loop workers for {CAPACITY_SECONDS}s, run last so the \
                     saturation cannot reach the latency metrics above it"
                ),
            ),
            (
                "client_shares_the_machine",
                "yes - the measuring process runs beside the stack it measures, so every figure \
                 is a floor. A much smaller share than in the load-driven modules: this issues \
                 one request at a time and spends the wait blocked."
                    .to_string(),
            ),
            (
                "wordpress_files",
                "a container volume in the daemon's storage, on a host filesystem. Unlike every \
                 other sandbox here this is not a tmpfs, because WP-CLI has to see the same files \
                 Apache is serving and a tmpfs is visible to one container."
                    .to_string(),
            ),
        ] {
            context.insert(key.to_string(), serde_json::Value::String(value));
        }
        context.insert(
            "fixture_posts_imported".to_string(),
            serde_json::Value::from(imported.posts as u64),
        );
        context.insert(
            "fixture_comments_imported".to_string(),
            serde_json::Value::from(imported.comments as u64),
        );
        context.insert(
            "images_fetched_during_this_run".to_string(),
            serde_json::Value::Bool(fetched),
        );
        if reaped > 0 {
            context.insert(
                "containers_reaped_from_earlier_runs".to_string(),
                serde_json::Value::from(reaped as u64),
            );
        }
        context
    }
}

/// Publishes a metric from repeated requests, or says why it did not.
fn push_distribution(
    metrics: &mut Vec<Metric>,
    warnings: &mut Vec<Warning>,
    key: &str,
    label: &str,
    samples: Vec<f64>,
) {
    let needed = REQUESTS.div_ceil(2);
    let Some(summary) = summarize(&samples).filter(|_| samples.len() >= needed) else {
        warnings.push(Warning {
            code: WarningCode::ValidationFailed,
            message: format!(
                "`{key}` got {} usable response(s) from {REQUESTS} requests, which is not enough \
                 to report. A response that was not a 200 of plausible size contributes nothing: \
                 an error page and a redirect are the two fastest things a WordPress can return, \
                 so admitting either would reward the site for breaking.",
                samples.len()
            ),
            metric_key: Some(key.to_string()),
        });
        return;
    };
    metrics.push(Metric {
        key: key.into(),
        label: label.into(),
        unit: "ms".into(),
        // The median rather than the mean, because a request that collided
        // with something else on the machine is a long tail and the mean
        // follows it.
        value: summary.median,
        direction: Direction::LowerIsBetter,
        outliers: outlier_indices(&samples, 3.5),
        summary,
        samples: samples
            .iter()
            .enumerate()
            .map(|(rep, value)| MetricSample {
                rep: rep as u32,
                value: *value,
                duration_ms: *value,
                warmup: false,
            })
            .collect(),
        measures_dispersion: false,
        tail_quantile: false,
    });
}

/// A [`Summary`] for a figure with exactly one observation.
///
/// `cv` is `None` rather than zero: zero claims the measurement was perfectly
/// stable, `None` says it was measured once. A site is only cold once.
fn single(value: f64) -> Summary {
    Summary {
        n: 1,
        min: value,
        max: value,
        mean: value,
        median: value,
        stddev: 0.0,
        cv: None,
        // A single observation has no interval, and inventing one would be a
        // claim about a distribution nobody sampled.
        ci95: None,
    }
}

/// What the import script reported back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Imported {
    pub(crate) posts: usize,
    pub(crate) comments: usize,
    pub(crate) checksum: String,
}

/// Reads the fixed last line the import script prints.
///
/// A fixed shape rather than prose precisely so this can be parsed: the module
/// has to compare what WordPress says it inserted against what the fixture
/// contains, and "it seemed to work" is not a comparison.
pub(crate) fn parse_import(output: &str) -> Option<Imported> {
    let line = output
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with("DARCBENCH_IMPORT"))?;
    let mut posts = None;
    let mut comments = None;
    let mut checksum = None;
    for field in line.split_whitespace() {
        if let Some(value) = field.strip_prefix("posts=") {
            posts = value.parse().ok();
        } else if let Some(value) = field.strip_prefix("comments=") {
            comments = value.parse().ok();
        } else if let Some(value) = field.strip_prefix("checksum=") {
            checksum = Some(value.to_string());
        }
    }
    Some(Imported {
        posts: posts?,
        comments: comments?,
        checksum: checksum?,
    })
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no output")
        .chars()
        .take(300)
        .collect()
}

fn not_measured(error: ContainerError) -> ModuleError {
    ModuleError::Precondition(error.to_string())
}

/// The container name the tier will have given a launched image.
///
/// Duplicated from the tier deliberately rather than exposed from it: the
/// only reason this module needs it is `--volumes-from`, which names a
/// container, and widening the tier's surface for one caller would make the
/// name part of its contract.
fn container_name(run_id: &str, key: &str) -> String {
    let safe: String = run_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .take(48)
        .collect();
    format!("darcbench-{safe}-{key}")
}

/// A per-run suffix, unique within this process.
fn unique_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;

    #[test]
    fn the_module_id_is_valid() {
        assert!(ModuleId::new(MODULE_ID).is_ok());
    }

    #[test]
    fn the_manifest_discloses_that_there_is_no_cache() {
        // The methodology's own words: "WordPress performance without a cache
        // disclosure is meaningless." This is the one manifest claim that is
        // load-bearing for whether the numbers may be quoted at all.
        let manifest = WordpressSite::new().manifest;
        assert!(
            manifest
                .limitations
                .iter()
                .any(|note| note.contains("NO page cache and NO object cache")),
            "the absence of a cache must be declared, not inferred"
        );
        for key in ["object_cache", "page_cache", "opcache"] {
            assert!(
                manifest.comparability.contains(&key.to_string()),
                "{key} must be a comparability key"
            );
        }
        // And the cold/warm pair must say what it is not, because the obvious
        // reading of those two names is the one this module does not measure.
        assert!(manifest
            .limitations
            .iter()
            .any(|note| note.contains("NOT a cached-versus-uncached pair")));
    }

    #[test]
    fn the_manifest_admits_what_it_costs_and_what_it_runs() {
        let manifest = WordpressSite::new().manifest;
        assert_eq!(manifest.safety_class, SafetyClass::ProvisionsServices);
        // Three images have to reach the machine, and the WordPress volume is
        // on a host filesystem. Both were zero in an earlier generation of
        // these modules and both were wrong.
        assert!(manifest.max_network_bytes > 400_000_000);
        assert!(manifest.max_bytes_written > 0);
        assert!(manifest
            .validation
            .iter()
            .any(|note| note.contains("verified before anything is timed")));
    }

    #[test]
    fn a_partial_import_is_read_as_one() {
        // The script prints a fixed shape so this can be compared rather than
        // trusted. Every field has to arrive or the whole line is refused: a
        // parse that found two of three and defaulted the last would silently
        // accept an import of the wrong corpus.
        let good = parse_import("noise\nDARCBENCH_IMPORT posts=312 comments=772 checksum=abc\n")
            .expect("parses");
        assert_eq!(good.posts, 312);
        assert_eq!(good.comments, 772);
        assert_eq!(good.checksum, "abc");

        assert!(parse_import("DARCBENCH_IMPORT posts=1 comments=2\n").is_none());
        assert!(parse_import("DARCBENCH_IMPORT comments=2 checksum=a\n").is_none());
        assert!(parse_import("").is_none());
        assert!(parse_import("Error: could not connect\n").is_none());
    }

    #[test]
    fn a_redirect_is_never_read_as_a_fast_page() {
        // The failure this prevents is the most attractive one in the module:
        // an unauthenticated wp-admin request is a 302 with no body, which is
        // the fastest response the site can produce and a measurement of
        // nothing at all.
        let (status, body, _) =
            parse_response(b"HTTP/1.1 302 Found\r\nLocation: /wp-login.php\r\n\r\n")
                .expect("parses");
        assert_eq!(status, 302);
        assert_eq!(body, 0);

        let (status, body, cookies) = parse_response(
            b"HTTP/1.1 200 OK\r\nSet-Cookie: wordpress_logged_in_x=y; path=/; HttpOnly\r\n\
              Set-Cookie: wp-settings=1; path=/\r\n\r\n<html>hello</html>",
        )
        .expect("parses");
        assert_eq!(status, 200);
        // `<html>hello</html>`, which is the whole point of counting from the
        // blank line rather than trusting a `Content-Length` the server may
        // not have sent.
        assert_eq!(body, 18);
        // Only the name=value pair; the attributes are a browser's business.
        assert_eq!(cookies, vec!["wordpress_logged_in_x=y", "wp-settings=1"]);
    }

    #[test]
    fn a_truncated_response_is_not_a_response() {
        // No blank line means the head never ended, so there is no status to
        // read and no body length to believe. Returning `None` is what keeps a
        // connection that died mid-header out of the distribution.
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nContent-Type: text/html").is_none());
        assert!(parse_response(b"").is_none());
    }
}
