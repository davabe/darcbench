//! `deployment.container` - what a deploy costs on this machine.
//!
//! # What it measures
//!
//! | Metric | Unit | What it predicts |
//! |---|---|---|
//! | `build.uncached` | s | A CI job with a cold cache: every layer rebuilt |
//! | `build.cached` | s | The same build with a warm layer cache - the common case |
//! | `cache.speedup` | × | What the layer cache is worth here, as a ratio |
//! | `image.save` | MiB/s | Writing an image out: `docker save`, a registry push's local half |
//! | `image.load` | MiB/s | Reading one back in: extraction, decompression, layer commit |
//!
//! # The base image is `scratch`, and that is what makes this module work
//!
//! Every other Phase 4 module is blocked on a digest that has to be resolved
//! against a real registry. This one is not, because it needs no base image at
//! all: the generated Dockerfile starts `FROM scratch` and copies files this
//! module wrote. Nothing is pulled, nothing is fetched, and the build runs with
//! `--network none` so it *cannot* be.
//!
//! That is not a workaround, it is the correct measurement. A build that
//! started `FROM node:22` would spend most of its time pulling and then
//! extracting somebody else's layers, so the number would be a network
//! measurement with a build attached - and `network.transfer` already measures
//! the network, directly, under a bounded transfer ceiling. What an operator
//! wants from *this* module is the machine's own contribution: reading a build
//! context, writing layers, committing them to the storage driver, and reading
//! them back.
//!
//! # Startup and health, and why they use a different image
//!
//! `startup.cold` and `health.to_serving` are the other half of the
//! deliverable, and they were declared absent for two commits because they need
//! an image with something *runnable* in it - which `scratch` is not.
//!
//! They now run BusyBox: 877 KB, one static multi-call binary, no init system
//! and nothing to warm up. That is not a compromise on the paragraph above, it
//! is the same argument applied to a different question. The build must not
//! start `FROM` a real base image because that would measure a registry. The
//! startup measurement must start from *something*, and the right something is
//! whatever contributes least of its own - so what is left is the machine
//! creating namespaces, mounting an overlay and execing a process.
//!
//! The build stays `FROM scratch` regardless. A base image now being available
//! is not a reason to put one in the build.
//!
//! # Where this writes, which is the one place a module writes to the host
//!
//! A build goes into the container runtime's storage, on a host filesystem the
//! operator chose. This module cannot put that on a tmpfs the way the database
//! modules put their data directories - the storage driver is configured
//! daemon-wide and is not this program's to change, and changing it would be
//! T-CONFIG.
//!
//! So it is bounded and disclosed instead: the generated context is a few
//! megabytes, `max_bytes_written` says so, the image carries this agent's
//! label, and every image it creates is removed on the way out **and** by
//! [`Runtime::reap_images`] at the start of the next run if this one is killed.

use std::path::Path;
use std::time::{Duration, Instant};

use darcbench_protocol::metrics::{Direction, Metric, MetricSample, Warning, WarningCode};
use darcbench_protocol::stats::{outlier_indices, summarize, Summary};
use darcbench_protocol::ModuleId;

use crate::container::{ContainerError, Image, Runtime, Sandbox};
use crate::module::{
    BenchmarkModule, ModuleError, ModuleManifest, ModuleOutput, ModuleParams, ModuleReporter,
    SafetyClass,
};
use crate::workloads::{SplitMix64, CORPUS_SEED};

/// Workload-definition version. Major bump = results are not comparable.
pub const VERSION: &str = "1.0.0";

/// The module's identifier, validated against the [`ModuleId`] grammar by a
/// unit test in this file.
pub const MODULE_ID: &str = "deployment.container";

/// Seed for the generated build context.
///
/// Derived from the shared [`CORPUS_SEED`] and salted, so this corpus is not
/// byte-identical to any other DARCBench corpus - two that shared a prefix
/// would compress differently in a way nobody would think to look for.
const CONTEXT_SEED: u64 = CORPUS_SEED ^ 0x0D0C_CE12;

/// Files in the generated build context.
///
/// Enough that layer creation and context transfer are real work rather than
/// rounding error, and spread over several `COPY` directives so the cached
/// build has more than one layer to hit.
const FILES_PER_LAYER: usize = 64;

/// How many `COPY` layers the Dockerfile has.
///
/// Six. A single-layer image would make the cached build a one-line cache hit
/// and tell an operator nothing about a real Dockerfile, which is a stack of
/// them. Six is enough that the cached path exercises the cache lookup
/// repeatedly and few enough that the uncached build stays inside the time
/// budget on a slow machine.
const LAYERS: usize = 6;

/// Bytes per generated file.
///
/// 48 KiB across 64 files per layer is about 3 MiB a layer and 18 MiB of
/// context. Big enough that the storage driver does real work committing it,
/// small enough that this is not a disk benchmark - `storage.mixed` is where
/// the disk is measured, and measuring it again here would put the same device
/// in two categories.
const FILE_BYTES: usize = 48 * 1024;

/// How long a build may take before it is killed.
const BUILD_TIMEOUT: Duration = Duration::from_secs(600);

/// How long an image save or load may take.
const IMAGE_TIMEOUT: Duration = Duration::from_secs(300);

/// How long fetching the base image may take before it is given up on.
///
/// Untimed and before any clock starts. BusyBox is under a megabyte, so this is
/// generous by two orders - but the run that exposed the need for it happened
/// to be on a fast link, and a bound sized to that would be a bound sized to
/// luck.
const PULL_TIMEOUT: Duration = Duration::from_secs(600);

/// The allow-list key for the base image the startup phases run.
const BASE_IMAGE_KEY: &str = "busybox";

/// Repetitions of the startup measurements.
///
/// **The builds are measured once and these are not, and the difference is the
/// point.** A build takes seconds, so one observation is dominated by the work
/// and a second uncached one would cost another minute for very little. A
/// container start takes a few hundred milliseconds, which is the same order as
/// whatever else the daemon happened to be doing at that instant - so a single
/// sample is mostly noise, and these are the only metrics in this module that
/// come with a distribution and a coefficient of variation rather than a bare
/// number.
///
/// Seven and five rather than one number for both: a start is cheaper than a
/// start-and-serve, so it can afford more.
const STARTUP_REPS: usize = 7;
const HEALTH_REPS: usize = 5;

/// How long one ephemeral container gets before it is killed.
const EPHEMERAL_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a container gets to start serving before the sample is discarded.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(30);

/// Gap between HTTP attempts while waiting for the server to answer.
///
/// Two milliseconds, which is two orders finer than the tier's own readiness
/// cadence and affordable for exactly the reason that one is not: this is a
/// loopback connect that fails immediately, not a round trip to a daemon. It
/// bounds the quantisation of a ~300 ms measurement to under one percent.
const HEALTH_POLL: Duration = Duration::from_millis(2);

/// The command the health phase runs in the base image.
///
/// `httpd -f` stays in the foreground, which is what makes the container's
/// lifetime the server's lifetime. `-h /srv` points it at the tmpfs the tier
/// mounts, which is empty - see [`crate::container::ALLOWED_IMAGES`] for why an
/// empty document root is the right one.
const HEALTH_COMMAND: &[&str] = &["httpd", "-f", "-p", "8080", "-h", "/srv"];

// ---------------------------------------------------------------------------
// The build context
// ---------------------------------------------------------------------------

/// Writes a deterministic build context into `dir`.
///
/// Deterministic for the same reason the WordPress fixture is: two machines are
/// only comparable if they built the same thing, and a context generated from
/// the clock or from `/dev/urandom` would make every run's build a different
/// amount of work. The bytes are pseudo-random rather than a repeated pattern
/// so that a storage driver with compression cannot turn 18 MiB into nothing
/// and flatter the machine - the same argument `web_origin` makes about its
/// object bodies.
///
/// Returns the total bytes written, which is what the save and load rates are
/// computed against.
pub(crate) fn write_context(dir: &Path) -> Result<u64, std::io::Error> {
    std::fs::create_dir_all(dir)?;

    let mut rng = SplitMix64::new(CONTEXT_SEED);
    let mut total: u64 = 0;

    for layer in 0..LAYERS {
        let layer_dir = dir.join(format!("layer{layer}"));
        std::fs::create_dir_all(&layer_dir)?;
        for index in 0..FILES_PER_LAYER {
            let mut body = vec![0u8; FILE_BYTES];
            for chunk in body.chunks_mut(8) {
                let word = rng.next_u64().to_le_bytes();
                chunk.copy_from_slice(&word[..chunk.len()]);
            }
            std::fs::write(layer_dir.join(format!("asset{index}.bin")), &body)?;
            total += body.len() as u64;
        }
    }

    let dockerfile = dockerfile();
    std::fs::write(dir.join("Dockerfile"), dockerfile.as_bytes())?;
    total += dockerfile.len() as u64;
    Ok(total)
}

/// The generated Dockerfile.
///
/// `FROM scratch` on purpose - see the module documentation. There is no `RUN`
/// anywhere in it, which is not an omission: a `RUN` needs a shell, `scratch`
/// has none, and a Dockerfile that needed one would need a base image and would
/// put this module back under the same block as the rest of Phase 4.
fn dockerfile() -> String {
    let mut out = String::from(
        "# Generated by DARCBench. Deterministic build context; not a real application.\n\
         #\n\
         # FROM scratch, so nothing is pulled and the measurement is this machine's\n\
         # own contribution: reading a context, writing layers, committing them.\n\
         # A build starting FROM a real base image would mostly measure a registry.\n\
         FROM scratch\n",
    );
    for layer in 0..LAYERS {
        out.push_str(&format!("COPY layer{layer}/ /app/layer{layer}/\n"));
    }
    out
}

// ---------------------------------------------------------------------------
// The module
// ---------------------------------------------------------------------------

pub struct DeploymentContainer {
    manifest: ModuleManifest,
}

impl Default for DeploymentContainer {
    fn default() -> Self {
        Self::new()
    }
}

impl DeploymentContainer {
    pub fn new() -> Self {
        // Justified `expect`: `MODULE_ID` is a compile-time constant whose
        // conformance to the `ModuleId` grammar is asserted by a unit test in
        // this file, so this cannot fail in a built binary.
        #[allow(clippy::expect_used)]
        let id = ModuleId::new(MODULE_ID).expect("MODULE_ID is a valid module id");
        let context_bytes = (LAYERS * FILES_PER_LAYER * FILE_BYTES) as u64;
        Self {
            manifest: ModuleManifest {
                id,
                version: VERSION.into(),
                title: "Container deployment".into(),
                purpose: "Measure what a deploy costs on this machine: building an image with a \
                          cold and a warm layer cache, writing it out and reading it back, and \
                          starting a container until it answers."
                    .into(),
                safety_class: SafetyClass::ProvisionsServices,
                dependencies: vec![
                    "A container runtime (Docker or Podman) whose daemon is reachable".into(),
                ],
                // The context, the archive, and the image in the daemon's
                // storage - roughly three copies of the context. Declared
                // generously, because the disk-space guard refusing a run is a
                // better outcome than a build filling somebody's root volume.
                max_bytes_written: context_bytes * 4,
                // The build pulls nothing - it is `FROM scratch` and runs with
                // `--network none`, so it *cannot*. What does cross the network
                // is the BusyBox base image the startup phases run, once, on a
                // machine that does not already have it. Under a megabyte, and
                // declared rather than left to happen inside a measurement.
                max_network_bytes: 877_398,
                cleanup: "The build context and archive are removed from the scratch directory, \
                          and every image this module creates is removed by tag on the way out. \
                          Images a killed run leaves behind carry this agent's label and are \
                          removed at the start of the next run."
                    .into(),
                validation: vec![
                    "A build that did not succeed produces no metric rather than a duration; a \
                     failed build finishes quickly and would read as a fast machine."
                        .into(),
                    "The cached build must not be slower than the uncached one. If it is, the \
                     cache did not hit and the pair describes two cold builds."
                        .into(),
                ],
                limitations: vec![
                    "Startup and health are measured against BusyBox rather than an application \
                     image, so they describe what the runtime costs and not what any particular \
                     application costs to boot. A real service adds its own start-up on top, and \
                     that part is a property of the service."
                        .into(),
                    "health.to_serving includes the runtime's port publication, which a container \
                     started without a published port does not pay. It is timed from the moment \
                     the container is asked for, because that is when an operator starts waiting."
                        .into(),
                    "The build is FROM scratch and runs with --network none, so no registry or \
                     package mirror is involved. That is deliberate: a build starting FROM a real \
                     base image would mostly measure a registry, which network.transfer measures \
                     directly and under a bounded ceiling."
                        .into(),
                    "This is the one module that writes to a host filesystem it cannot put on a \
                     tmpfs. The storage driver is configured daemon-wide and is not this \
                     program's to change - changing it would be T-CONFIG."
                        .into(),
                    "The layer cache is warmed by this module's own first build, so the cached \
                     figure is a best case. A CI cache that has to be restored from elsewhere \
                     pays a transfer this does not measure."
                        .into(),
                ],
                comparability: vec![
                    "runtime".into(),
                    "runtime_version".into(),
                    "storage_driver".into(),
                    "context_bytes".into(),
                    "layers".into(),
                ],
                stability_cv_bound: 0.20,
            },
        }
    }
}

impl BenchmarkModule for DeploymentContainer {
    fn manifest(&self) -> &ModuleManifest {
        &self.manifest
    }

    /// Every phase here runs in the daemon or in a container it started, and
    /// none of it is this process's child.
    ///
    /// More completely true of this module than of the database ones. There,
    /// at least the module waits while a container works; here the *build* is
    /// `dockerd` and `buildkitd` doing minutes of compression and layer
    /// commits while this process holds a pipe. The runtime load ceiling would
    /// see all of it as a competing tenant.
    ///
    /// It did not fire during the run that added these phases, and that is not
    /// evidence of anything: the guard needs twenty consecutive seconds over
    /// its threshold, and the phases here are shorter than that on a fast
    /// machine. On a slow one, where the build takes minutes, it would - which
    /// is precisely the machine whose result would then be wrongly degraded.
    fn workload_runs_outside_this_process(&self) -> bool {
        true
    }

    /// Deliberately pessimistic, and by a wide margin.
    ///
    /// Measured on the first host to run this at all: nine seconds, of which
    /// two were the uncached build. Two hundred is more than twenty times that
    /// and is still the right number to publish, because what this feeds is
    /// preflight - the screen where an operator decides whether to let a
    /// benchmark loose on a server that is doing something else. An estimate
    /// that is too low there is a broken promise; one that is too high is a
    /// pleasant surprise.
    ///
    /// And the spread across machines is genuinely enormous. Every figure in
    /// this module is a *daemon* measurement as much as a machine one: the same
    /// build is seconds on an NVMe host with overlay2 and minutes on a small
    /// VPS with a spinning disk and a storage driver that copies whole files on
    /// write. Twelve container starts and an 18 MiB build have a long tail that
    /// a number fitted to a fast machine would hide.
    ///
    /// It replaces a bare `180` with no justification at all, which by this
    /// codebase's own rule was a constant somebody would change casually.
    fn estimated_duration_s(&self, _params: &ModuleParams) -> u64 {
        200
    }

    fn run(
        &self,
        params: &ModuleParams,
        _reporter: &dyn ModuleReporter,
    ) -> Result<ModuleOutput, ModuleError> {
        let scratch = params.scratch_dir.as_ref().ok_or_else(|| {
            ModuleError::Precondition(
                "this module needs a scratch directory to generate its build context in, and was \
                 given none."
                    .into(),
            )
        })?;

        let runtime = Runtime::discover().map_err(not_measured)?;
        // Images first, then containers: an image cannot be removed while a
        // container made from it exists, so reaping in the other order would
        // leave every image from a killed run behind.
        let reaped_images = runtime.reap_images().map_err(not_measured)?;
        let reaped = runtime.reap().map_err(not_measured)?;

        // A fixed name directly inside the scratch directory the agent gave
        // us. `ModuleParams::scratch_dir` is already validated by the agent's
        // `StatePath`, which is the only type permitted to compose a path from
        // components, and nothing caller-supplied is joined onto it here.
        let context = scratch.join("deployment-context");
        let archive = scratch.join("deployment-image.tar");
        let tag = format!("darcbench-deploy:{}", unique_suffix());

        let outcome = self.measure(&runtime, &context, &archive, &tag, reaped_images + reaped);

        // On every path, including the error ones. A build left in the
        // daemon's storage is disk the operator did not agree to lose.
        runtime.remove_image(&tag);
        let _ = std::fs::remove_dir_all(&context);
        let _ = std::fs::remove_file(&archive);
        outcome
    }
}

impl DeploymentContainer {
    fn measure(
        &self,
        runtime: &Runtime,
        context: &Path,
        archive: &Path,
        tag: &str,
        reaped: usize,
    ) -> Result<ModuleOutput, ModuleError> {
        let context_bytes = write_context(context).map_err(|error| {
            ModuleError::Precondition(format!(
                "could not generate the build context in {}: {error}",
                context.display()
            ))
        })?;

        let mut metrics = Vec::new();
        let mut warnings = Vec::new();

        // Cold. `--no-cache` rather than a fresh context, because a fresh
        // context would also change what is being built and the two builds
        // would not be the same build.
        let uncached = timed(|| runtime.build(context, tag, true, BUILD_TIMEOUT));
        let cached = timed(|| runtime.build(context, tag, false, BUILD_TIMEOUT));

        match (&uncached, &cached) {
            (Some(cold), Some(warm)) => {
                push_seconds(&mut metrics, "build.uncached", "Build, cold cache", *cold);
                push_seconds(&mut metrics, "build.cached", "Build, warm cache", *warm);

                if warm > cold {
                    // Not a slow machine: a cache that did not hit. The pair
                    // then describes two cold builds and the ratio below would
                    // be a number about nothing.
                    warnings.push(Warning {
                        code: WarningCode::ValidationFailed,
                        message: format!(
                            "the cached build took {warm:.2}s against {cold:.2}s uncached, so the \
                             layer cache did not hit. The two figures describe two cold builds \
                             and the speedup ratio is withheld."
                        ),
                        metric_key: Some("cache.speedup".into()),
                    });
                } else if *warm > 0.0 {
                    let speedup = cold / warm;
                    metrics.push(Metric {
                        key: "cache.speedup".into(),
                        label: "Layer cache speedup".into(),
                        unit: "x".into(),
                        value: speedup,
                        direction: Direction::HigherIsBetter,
                        summary: single(speedup),
                        samples: Vec::new(),
                        outliers: Vec::new(),
                        measures_dispersion: false,
                        tail_quantile: false,
                    });
                }
            }
            _ => warnings.push(Warning {
                code: WarningCode::ValidationFailed,
                message: "a build did not succeed, so no duration is reported for it. A failed \
                          build finishes quickly and would read as a fast machine."
                    .into(),
                metric_key: Some("build.uncached".into()),
            }),
        }

        // Save and load, as rates rather than durations, because the useful
        // comparison across machines is throughput and the archive is the same
        // size on all of them.
        if uncached.is_some() {
            if let Some(seconds) = timed(|| runtime.save_image(tag, archive, IMAGE_TIMEOUT)) {
                let bytes = std::fs::metadata(archive).map(|m| m.len()).unwrap_or(0);
                push_rate(
                    &mut metrics,
                    "image.save",
                    "Image write-out",
                    bytes,
                    seconds,
                );

                // The image has to be gone before it is loaded, or `load`
                // recognises every layer and measures nothing.
                runtime.remove_image(tag);
                if let Some(seconds) = timed(|| runtime.load_image(archive, IMAGE_TIMEOUT)) {
                    push_rate(
                        &mut metrics,
                        "image.load",
                        "Image extraction",
                        bytes,
                        seconds,
                    );
                }
            }
        }

        // --- startup and health -----------------------------------------------
        //
        // The other half of the deliverable, and for two commits it was
        // declared absent because it needs an image with something runnable in
        // it. It now has one.
        self.measure_startup(runtime, &mut metrics, &mut warnings);

        // --- variance -----------------------------------------------------------
        //
        // The manifest declares a stability bound and until this commit nothing
        // in this module checked it. That was harmless while it was unfalsifiable:
        // every metric here was a single observation with `cv: None`, so there was
        // no coefficient of variation to be above anything. The two metrics above
        // are the first with a distribution behind them, which turns a dormant
        // promise into a live one - and an unkept promise in a manifest is worse
        // than an absent one, because the manifest is what a reader trusts.
        //
        // Swept over the metric list rather than checked where the metrics are
        // built, which is the shape `network.transfer` arrived at the hard way:
        // checking inside one construction path made the promise true for the
        // metrics on that path and quietly false for the rest.
        for metric in &metrics {
            let Some(cv) = metric.summary.cv else {
                continue;
            };
            if cv > self.manifest.stability_cv_bound {
                warnings.push(Warning {
                    code: WarningCode::HighVariance,
                    message: format!(
                        "`{}` varied by {:.0}% across repetitions (bound {:.0}%). Container start \
                         time is a daemon measurement as much as a machine one, so this usually \
                         means the runtime was doing something else - an image being pulled, \
                         another container starting - rather than that the hardware is \
                         inconsistent.",
                        metric.key,
                        cv * 100.0,
                        self.manifest.stability_cv_bound * 100.0
                    ),
                    metric_key: Some(metric.key.clone()),
                });
            }
        }

        let (runtime_version, storage_driver) = runtime.identity();
        let mut context_map = serde_json::Map::new();
        for (key, value) in [
            ("runtime".to_string(), runtime.name()),
            ("runtime_version".to_string(), runtime_version),
            // Declared in `comparability` since this module was written and
            // recorded nowhere until now. It is the fact that decides whether
            // two of these results describe comparable machines.
            ("storage_driver".to_string(), storage_driver),
            ("context_bytes".to_string(), context_bytes.to_string()),
            ("layers".to_string(), LAYERS.to_string()),
            ("files_per_layer".to_string(), FILES_PER_LAYER.to_string()),
            (
                "base_image".to_string(),
                "scratch - nothing is pulled, and the build runs with --network none so it \
                 cannot be. A build FROM a real base image would mostly measure a registry."
                    .to_string(),
            ),
            (
                "startup_base_image".to_string(),
                "busybox, pinned by digest. Chosen for contributing as little of its own as a \
                 running container can: 877 KB and one static binary, so what is measured is the \
                 runtime creating namespaces, mounting an overlay and execing a process. The \
                 build itself still starts FROM scratch."
                    .to_string(),
            ),
            (
                "health_signal".to_string(),
                "an HTTP status line from busybox httpd on the published loopback port. A TCP \
                 connect is not used and would not work: the runtime publishes a port with a \
                 userland proxy that accepts as soon as the container exists, whatever the \
                 server inside is doing. The response is a 404 - the document root is an empty \
                 tmpfs, because filling it would need a host path inside a container - and a 404 \
                 from a running server is a response."
                    .to_string(),
            ),
            (
                "layer_cache_origin".to_string(),
                "warmed by this module's own first build, so the cached figure is a best case. A \
                 CI cache restored from elsewhere pays a transfer this does not measure."
                    .to_string(),
            ),
        ] {
            context_map.insert(key, serde_json::Value::String(value));
        }
        if reaped > 0 {
            context_map.insert(
                "artifacts_reaped_from_earlier_runs".into(),
                serde_json::Value::from(reaped as u64),
            );
        }

        Ok(ModuleOutput {
            metrics,
            warnings,
            context: context_map,
        })
    }
}

/// Runs a control command and returns how long it took, or `None` if it failed.
///
/// `None` rather than the elapsed time, because a build that failed finished
/// quickly and reporting its duration would read as a fast machine - the same
/// trap `is_credible` catches in the database modules.
impl DeploymentContainer {
    /// What starting a container costs on this machine, and what it costs
    /// before it answers.
    ///
    /// Two questions rather than one, because a deploy pays them separately.
    /// `startup.cold` is the runtime's own price - namespaces, cgroups, an
    /// overlay mount, an exec - measured with the least possible application
    /// in the way: a container whose whole job is to run `true` and exit.
    /// `health.to_serving` is what an operator actually waits for, from asking
    /// for a container to getting an answer out of it, and it includes port
    /// publication and the server's own start.
    ///
    /// Neither is a measurement of BusyBox. That is why BusyBox is the image:
    /// 877 KB and one static binary, so the application's contribution is as
    /// close to zero as a running container can get and what remains is the
    /// machine.
    fn measure_startup(
        &self,
        runtime: &Runtime,
        metrics: &mut Vec<Metric>,
        warnings: &mut Vec<Warning>,
    ) {
        let Some(image) = Image::from_allow_list(BASE_IMAGE_KEY) else {
            warnings.push(not_measured_warning(
                "the base image is not on the container allow-list, so startup and health are \
                 not measured",
            ));
            return;
        };
        if let Err(error) = image.reference() {
            warnings.push(not_measured_warning(&format!(
                "startup and health are not measured: {error}"
            )));
            return;
        }

        // Before the first repetition, not during it. `docker run` on an absent
        // image pulls it, which put a download inside one sample of seven and
        // took the coefficient of variation to 147% - see
        // `Runtime::ensure_image_present`.
        match runtime.ensure_image_present(image, PULL_TIMEOUT) {
            Ok(_) => {}
            Err(error) => {
                warnings.push(not_measured_warning(&format!(
                    "the base image could not be fetched, so startup and health are not \
                     measured: {error}"
                )));
                return;
            }
        }

        // `true` rather than `sh -c true`: a shell would put its own start-up
        // inside a number that is supposed to be the container's.
        let mut starts = Vec::with_capacity(STARTUP_REPS);
        for _ in 0..STARTUP_REPS {
            // A container that failed to start also finishes quickly, so a
            // failure contributes no sample rather than a fast one. Both the
            // `Err` and the `Ok(None)` cases collapse to that, which is what
            // makes this an `if let` over the one case that produces a number.
            if let Ok(Some(elapsed)) =
                runtime.run_ephemeral(image, &unique_suffix(), &["true"], EPHEMERAL_TIMEOUT)
            {
                starts.push(elapsed.as_secs_f64() * 1000.0);
            }
        }
        push_distribution(
            metrics,
            warnings,
            "startup.cold",
            "Container start to exit",
            "ms",
            starts,
            STARTUP_REPS,
        );

        let mut serves = Vec::with_capacity(HEALTH_REPS);
        for _ in 0..HEALTH_REPS {
            if let Some(ms) = time_to_serving(runtime, image) {
                serves.push(ms);
            }
        }
        push_distribution(
            metrics,
            warnings,
            "health.to_serving",
            "Container start to first response",
            "ms",
            serves,
            HEALTH_REPS,
        );
    }
}

/// Starts a server in a container and returns milliseconds until it answers.
///
/// The clock starts before the container is asked for, because that is the
/// question: an operator waiting on a deploy is waiting from the moment they
/// asked. It therefore includes the runtime's own port lookup, which is a
/// daemon round trip the container is booting *during* rather than after - the
/// two overlap, so this is not a sum of two costs with one that does not
/// belong.
///
/// `None` if the server never answered inside the deadline. A container that
/// never served has no serving time, and the alternative - recording the
/// timeout - would put the deadline into the distribution as though it were a
/// measurement.
fn time_to_serving(runtime: &Runtime, image: &'static Image) -> Option<f64> {
    let started = Instant::now();
    let sandbox = Sandbox::launch_without_waiting(
        runtime,
        image,
        &unique_suffix(),
        &crate::container::Launch {
            command: HEALTH_COMMAND,
            ..Default::default()
        },
    )
    .ok()?;
    let deadline = started + HEALTH_TIMEOUT;
    while Instant::now() < deadline {
        if http_answered(sandbox.address()) {
            return Some(started.elapsed().as_secs_f64() * 1000.0);
        }
        std::thread::sleep(HEALTH_POLL);
    }
    // `sandbox` drops here either way, and `Drop` removes the container.
    None
}

/// Whether an HTTP server at `address` produced a response.
///
/// **A connect is not enough and that is the whole reason this speaks HTTP.**
/// Docker publishes a port with a userland proxy that accepts as soon as the
/// container exists, so a successful connect says nothing about the server -
/// the same trap that made the isolation tier's readiness check worthless
/// until a real daemon exposed it. A status line can only come from something
/// that parsed a request.
///
/// Any status is an answer, including the 404 this will actually get: the
/// document root is an empty tmpfs, because filling it would need a host path
/// inside a container and this tier does not have one. What is being timed is
/// when the server started answering, not what it said.
fn http_answered(address: std::net::SocketAddr) -> bool {
    use std::io::{Read, Write};

    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&address, HTTP_ATTEMPT_TIMEOUT)
    else {
        return false;
    };
    if stream.set_read_timeout(Some(HTTP_ATTEMPT_TIMEOUT)).is_err()
        || stream.write_all(b"GET / HTTP/1.0\r\n\r\n").is_err()
    {
        return false;
    }
    // Enough for a status line and no more. Nothing here needs the body, and a
    // read that drained one would be timing the transfer as well as the wait.
    let mut head = [0_u8; 16];
    match stream.read(&mut head) {
        Ok(read) => head[..read].starts_with(b"HTTP/"),
        Err(_) => false,
    }
}

/// How long one HTTP attempt may block.
///
/// Generous next to [`HEALTH_POLL`] because it is a ceiling rather than a
/// cadence: it only matters when the proxy accepts and then holds the
/// connection open without a server behind it, which is exactly the state
/// being waited out.
const HTTP_ATTEMPT_TIMEOUT: Duration = Duration::from_millis(250);

/// Publishes a metric from repeated samples, or says why it did not.
///
/// The only metrics in this module with a real distribution behind them. The
/// threshold for publishing is a majority of the attempted repetitions:
/// below that the machine is failing to start containers, which is a finding
/// about the machine rather than a slow one to be averaged in.
fn push_distribution(
    metrics: &mut Vec<Metric>,
    warnings: &mut Vec<Warning>,
    key: &str,
    label: &str,
    unit: &str,
    samples: Vec<f64>,
    attempted: usize,
) {
    let needed = attempted.div_ceil(2);
    let Some(summary) = summarize(&samples).filter(|_| samples.len() >= needed) else {
        warnings.push(Warning {
            code: WarningCode::ValidationFailed,
            message: format!(
                "`{key}` produced {} usable sample(s) from {attempted} attempts, which is not \
                 enough to report. A container that failed to start also finished quickly, so an \
                 average over the ones that worked would describe a machine that was not having \
                 trouble.",
                samples.len()
            ),
            metric_key: Some(key.to_string()),
        });
        return;
    };
    metrics.push(Metric {
        key: key.into(),
        label: label.into(),
        unit: unit.into(),
        // The median, not the mean: a start that collided with something else
        // on the daemon is a long tail, and the mean follows it.
        value: summary.median,
        direction: Direction::LowerIsBetter,
        outliers: outlier_indices(&samples, 3.5),
        summary,
        // The per-repetition samples are published, not just the summary. For
        // these two metrics the spread *is* part of the finding: a machine
        // whose container starts take 200 ms and occasionally 900 ms is a
        // different machine from one that always takes 350 ms, and a median
        // alone cannot tell them apart.
        samples: samples
            .iter()
            .enumerate()
            .map(|(rep, value)| MetricSample {
                rep: rep as u32,
                value: *value,
                // The value *is* the duration here: both metrics are wall
                // times in milliseconds, so a separate figure would be the
                // same number claiming to be a different one.
                duration_ms: *value,
                warmup: false,
            })
            .collect(),
        measures_dispersion: false,
        tail_quantile: false,
    });
}

fn not_measured_warning(message: &str) -> Warning {
    Warning {
        code: WarningCode::ValidationFailed,
        message: message.to_string(),
        metric_key: Some("startup.cold".into()),
    }
}

fn timed<F>(operation: F) -> Option<f64>
where
    F: FnOnce() -> Result<crate::runtime_exec::Output, ContainerError>,
{
    let started = Instant::now();
    match operation() {
        Ok(output) if output.succeeded() => Some(started.elapsed().as_secs_f64()),
        _ => None,
    }
}

fn push_seconds(metrics: &mut Vec<Metric>, key: &str, label: &str, seconds: f64) {
    metrics.push(Metric {
        key: key.into(),
        label: label.into(),
        unit: "s".into(),
        value: seconds,
        direction: Direction::LowerIsBetter,
        summary: single(seconds),
        samples: Vec::new(),
        outliers: Vec::new(),
        measures_dispersion: false,
        tail_quantile: false,
    });
}

fn push_rate(metrics: &mut Vec<Metric>, key: &str, label: &str, bytes: u64, seconds: f64) {
    if bytes == 0 || seconds <= 0.0 {
        // An archive of no bytes, or an operation that took no measurable
        // time, is not an infinitely fast machine.
        return;
    }
    let mib_per_second = (bytes as f64 / (1024.0 * 1024.0)) / seconds;
    metrics.push(Metric {
        key: key.into(),
        label: label.into(),
        unit: "MiB/s".into(),
        value: mib_per_second,
        direction: Direction::HigherIsBetter,
        summary: single(mib_per_second),
        samples: Vec::new(),
        outliers: Vec::new(),
        measures_dispersion: false,
        tail_quantile: false,
    });
}

/// A [`Summary`] for a figure with exactly one observation.
///
/// `cv` is `None` rather than zero: zero claims the measurement was perfectly
/// stable, `None` says it was measured once.
fn single(value: f64) -> Summary {
    Summary {
        n: 1,
        min: value,
        max: value,
        mean: value,
        median: value,
        stddev: 0.0,
        cv: None,
        ci95: None,
    }
}

fn not_measured(error: ContainerError) -> ModuleError {
    ModuleError::Precondition(error.to_string())
}

/// A short unique token for an image tag. See `database_oltp`.
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
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("darcbench-deploy-test-{}", unique_suffix()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_module_id_is_valid() {
        assert!(ModuleId::new(MODULE_ID).is_ok());
    }

    #[test]
    fn the_dockerfile_pulls_nothing_and_runs_nothing() {
        // The property that makes this module deliverable while the rest of
        // Phase 4 waits on a registry - and the property that makes the number
        // correct, because a build FROM a real base image would mostly measure
        // a registry.
        let dockerfile = dockerfile();
        // Directives, not substrings. The comment block explains *why* the
        // build starts FROM scratch rather than FROM a real base image, and a
        // substring count reads that prose as three FROM lines - which is how
        // a test ends up asserting something about English.
        let directives: Vec<&str> = dockerfile
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect();

        let from: Vec<&&str> = directives
            .iter()
            .filter(|line| line.starts_with("FROM "))
            .collect();
        assert_eq!(from.len(), 1, "{directives:?}");
        assert_eq!(*from[0], "FROM scratch");

        // No RUN: `scratch` has no shell, and a Dockerfile that needed one
        // would need a base image and would put this module back under the
        // same block as the rest of Phase 4. No ADD either, because ADD can
        // fetch a URL and this build runs with --network none.
        for forbidden in ["RUN ", "ADD ", "FROM scratch AS"] {
            assert!(
                !directives.iter().any(|line| line.starts_with(forbidden)),
                "`{forbidden}` is a directive here: {directives:?}"
            );
        }
        assert_eq!(
            directives
                .iter()
                .filter(|line| line.starts_with("COPY "))
                .count(),
            LAYERS
        );
        // Every directive is one of the two this module emits.
        for line in &directives {
            assert!(
                line.starts_with("FROM ") || line.starts_with("COPY "),
                "unexpected directive: {line}"
            );
        }
    }

    #[test]
    fn the_build_context_is_identical_on_every_machine() {
        // Two machines are only comparable if they built the same thing. A
        // context generated from the clock or from /dev/urandom would make
        // every run's build a different amount of work.
        let first = tempdir();
        let second = tempdir();
        let bytes_a = write_context(&first).unwrap();
        let bytes_b = write_context(&second).unwrap();
        assert_eq!(bytes_a, bytes_b);

        for layer in 0..LAYERS {
            for index in [0usize, FILES_PER_LAYER - 1] {
                let name = format!("layer{layer}/asset{index}.bin");
                assert_eq!(
                    std::fs::read(first.join(&name)).unwrap(),
                    std::fs::read(second.join(&name)).unwrap(),
                    "{name} differs between two generations"
                );
            }
        }
        assert_eq!(
            std::fs::read(first.join("Dockerfile")).unwrap(),
            std::fs::read(second.join("Dockerfile")).unwrap()
        );

        let _ = std::fs::remove_dir_all(&first);
        let _ = std::fs::remove_dir_all(&second);
    }

    #[test]
    fn the_context_is_not_trivially_compressible() {
        // A storage driver or an archive format with compression could turn a
        // repeated pattern into nothing and flatter the machine - the same
        // argument web_origin makes about its object bodies. Checked by
        // counting distinct bytes, which a run of zeroes would fail and
        // pseudo-random data passes.
        let dir = tempdir();
        write_context(&dir).unwrap();
        let body = std::fs::read(dir.join("layer0/asset0.bin")).unwrap();
        assert_eq!(body.len(), FILE_BYTES);
        let mut seen = [false; 256];
        for byte in &body {
            seen[*byte as usize] = true;
        }
        let distinct = seen.iter().filter(|s| **s).count();
        assert!(distinct > 200, "only {distinct} distinct byte values");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_context_is_big_enough_to_measure_and_small_enough_not_to_be_a_disk_benchmark() {
        let total = (LAYERS * FILES_PER_LAYER * FILE_BYTES) as u64;
        assert!(total > 8 * 1024 * 1024, "{total} bytes is rounding error");
        // storage.mixed is where the disk is measured, and measuring it again
        // here would put the same device in two categories.
        assert!(
            total < 64 * 1024 * 1024,
            "{total} bytes is a disk benchmark"
        );
    }

    #[test]
    fn a_failed_operation_produces_no_duration() {
        // A build that failed finished quickly, and reporting its duration
        // would read as a fast machine.
        let failed = timed(|| {
            Err(ContainerError::Start {
                runtime: "/usr/bin/docker".into(),
                detail: "no".into(),
            })
        });
        assert_eq!(failed, None);

        let nonzero_exit = timed(|| {
            Ok(crate::runtime_exec::Output {
                stdout: String::new(),
                stderr: "error".into(),
                status: Some(1),
                elapsed: Duration::from_millis(5),
            })
        });
        assert_eq!(nonzero_exit, None, "a non-zero exit is not a measurement");
    }

    #[test]
    fn a_rate_is_not_reported_for_no_bytes_or_no_time() {
        // Either would be an infinitely fast machine.
        let mut metrics = Vec::new();
        push_rate(&mut metrics, "image.save", "x", 0, 1.0);
        push_rate(&mut metrics, "image.save", "x", 1024, 0.0);
        push_rate(&mut metrics, "image.save", "x", 1024, -1.0);
        assert!(metrics.is_empty());

        push_rate(&mut metrics, "image.save", "x", 1024 * 1024, 1.0);
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].value, 1.0);
    }

    #[test]
    fn the_manifest_declares_the_two_things_this_module_does_not_do() {
        // Startup and health are in the deliverable and are not measured. A
        // reader must find that stated rather than infer it from an absence.
        let manifest = DeploymentContainer::new().manifest().clone();
        // Startup and health were declared absent for two commits and are now
        // delivered. What has to stay declared is the narrower thing that is
        // still true: they measure the runtime, not an application.
        assert!(manifest
            .limitations
            .iter()
            .any(|note| note.contains("rather than an application image")));
        // And that the cached figure is a best case.
        assert!(manifest
            .limitations
            .iter()
            .any(|note| note.contains("best case")));
    }

    #[test]
    fn the_manifest_admits_it_writes_to_a_host_filesystem() {
        // Every other Phase 4 module puts its data on a tmpfs. This one cannot
        // - the storage driver is daemon-wide and changing it would be
        // T-CONFIG - so the bound is declared instead.
        let manifest = DeploymentContainer::new().manifest().clone();
        assert!(manifest.max_bytes_written > 0);
        // Not zero. The build genuinely pulls nothing, but the startup phases
        // need a base image on the machine, and a manifest that promises no
        // network while the runtime fetches one is a broken promise.
        assert!(manifest.max_network_bytes > 0);
        assert!(manifest
            .limitations
            .iter()
            .any(|note| note.contains("host filesystem")));
        assert!(manifest.cleanup.contains("label"));
    }

    #[test]
    fn a_cached_build_slower_than_a_cold_one_is_a_cache_miss_not_a_slow_machine() {
        // Asserted on the manifest's validation rules, because the branch
        // itself needs a daemon. The rule has to exist for the ratio to mean
        // anything: two cold builds produce a speedup near 1.0 that would read
        // as "this machine's cache is worthless".
        let manifest = DeploymentContainer::new().manifest().clone();
        assert!(manifest
            .validation
            .iter()
            .any(|rule| rule.contains("must not be slower")));
    }
}
