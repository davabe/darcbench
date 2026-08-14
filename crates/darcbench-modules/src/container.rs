//! The container-based module isolation tier.
//!
//! Phase 4 measures databases and CMS stacks, and its absolute requirement is
//! that DARCBench **never touches a production database**. The roadmap's exit
//! criterion is that every database module *creates and destroys its own
//! instance*, and this is how: a container the agent starts, loads, and
//! removes, with nothing of the host inside it.
//!
//! # Why this is not "just shell out to docker"
//!
//! Running a container means executing a binary the operator installed and
//! handing it an argument vector. Both halves are dangerous and are handled
//! separately.
//!
//! **The binary** goes through [`crate::runtime_exec`], the layer
//! `docs/adr/0013-executing-a-discovered-runtime.md` built for the runtime
//! modules: a compile-time path allow-list, an ownership check on the binary
//! *and every ancestor directory*, a cleared environment and a hard timeout.
//! The agent frequently runs as root on shared hosts, so executing "whatever
//! `docker` resolves to on `PATH`" hands root to whoever last wrote to a
//! directory on it - and a container runtime is a particularly bad thing to
//! hand over, since anything that can run containers can mount the host root.
//!
//! **The argument vector** is built by [`run_args`], which is a pure function
//! over a fixed set of constants and a run id. That makes the dangerous
//! properties testable without a daemon, and they are tested:
//!
//! | Property | Why | Test |
//! |---|---|---|
//! | No host path is ever passed | A `-v /:/host` is a root shell | `no_host_path_can_reach_the_argument_vector` |
//! | Ports publish to `127.0.0.1` only | Otherwise a benchmark database is on the network | `a_published_port_is_never_reachable_off_this_machine` |
//! | Images are pinned by digest | A tag is mutable; T-SUPPLY | `every_allowed_image_is_pinned_by_digest` |
//! | Removal is filtered by our own label | So DARCBench cannot delete the operator's containers | `only_containers_this_agent_labelled_can_be_removed` |
//! | Data lives on tmpfs | Nothing survives the run, and no host disk is written | `the_data_directory_is_a_tmpfs_that_cannot_outlive_the_run` |
//!
//! # The image allow-list, and why it was empty for two commits
//!
//! [`ALLOWED_IMAGES`] is the same shape as `network_endpoints`' host table: a
//! compile-time list, each entry justified, with no way to add one at runtime.
//! Every entry is pinned to a digest resolved against a real registry. The
//! table carried its two entries unpinned for as long as this project had no
//! host with a container daemon, because inventing a `sha256:` string would
//! have been writing a security control that cannot work.
//!
//! `Image` cannot be constructed outside this module, and
//! [`Image::from_allow_list`] is the only way to obtain one - so a module that
//! wants MariaDB must add a digest-pinned entry here, and a test refuses the
//! table if any entry names a tag.
//!
//! # What is not solved: an agent that dies mid-run
//!
//! The release profile sets `panic = "abort"`, so destructors do not run on a
//! panic, and nothing runs on `SIGKILL`. A container started by a run that
//! died keeps running.
//!
//! `--rm` would not help, which is half of why it is not used: it reaps when
//! the *container* exits, not when the agent does. So the mitigation is
//! reaping rather than prevention -
//! [`reap`] removes containers carrying this agent's label, and it is
//! deliberately label-scoped rather than name-scoped so that no amount of
//! coincidence in an operator's naming can put one of their containers in
//! range. A module calls it before it starts, so the cost of a previous crash
//! is bounded to "until the next run" rather than "forever".
//!
//! This is stated rather than hidden because it is the one guarantee this
//! module cannot make.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::runtime_exec::{self, ExecError, Interpreter};

/// Container runtimes this agent will execute, in preference order.
///
/// Docker first because it is what is installed on the hosts this product
/// targets. Rootless paths under a user's home are deliberately absent for the
/// same reason `node.runtime` omits nvm: a runtime binary a non-root user can
/// rewrite is a runtime binary that fails the safe-path check anyway, and
/// listing it would produce a confusing rejection instead of an honest
/// "not found".
const RUNTIMES: &[&str] = &[
    "/usr/bin/docker",
    "/usr/local/bin/docker",
    "/usr/bin/podman",
    "/usr/local/bin/podman",
    "/usr/bin/nerdctl",
];

/// Label every container this agent creates carries.
///
/// The whole of [`reap`]'s safety. A name prefix could collide with something
/// an operator chose; a label this agent sets could not have been set by
/// anything but this agent.
const OWNER_LABEL: &str = "com.getdarc.darcbench.owned=1";

/// The filter form of [`OWNER_LABEL`], for `ps` and `rm`.
const OWNER_FILTER: &str = "label=com.getdarc.darcbench.owned=1";

/// Tmpfs size for the service's data directory.
///
/// Sized for the largest dataset any module here builds: `database.oltp` at
/// pgbench scale 10 occupies about 324 MiB after its indexes, and grows to
/// roughly 360 MiB once a write phase has cycled some WAL.
const TMPFS_SIZE_BYTES: u64 = 512 * 1024 * 1024;

/// What the service itself may use, on top of its data.
///
/// PostgreSQL's default `shared_buffers` is 128 MiB, and eight `pgbench`
/// backends and their client add most of the rest. Measured peak for the
/// heaviest phase this suite runs is 507 MiB of the total below.
const SERVICE_MEMORY_BYTES: u64 = 512 * 1024 * 1024;

/// Memory ceiling for a sandboxed service: its data plus its working set.
///
/// **These are one budget, not two, and that is the whole reason this is
/// computed rather than written.** A tmpfs lives in the page cache and its
/// pages are charged to the cgroup that faults them in, so every byte of the
/// data directory is a byte of the memory limit. The two constants above were
/// once independent and both `512m`, which read as "half a gig of disk and
/// half a gig of RAM" and meant "half a gig, and the database may have
/// whatever the dataset does not want".
///
/// It did not want much. `database.oltp` built a 324 MiB dataset and was
/// OOM-killed part-way through its first write phase — `server process was
/// terminated by signal 9`, then automatic recovery, then every later phase
/// refused. What the module published from that was two throughput figures,
/// one of them from a phase that had died half-way through, and silence about
/// the other four metrics. A benchmark that reports a number for a database
/// that was being killed while it was measured is worse than one that reports
/// nothing.
///
/// The ceiling still exists and still matters — a runaway container must not
/// take the host down, because a benchmark that causes an outage is a failure
/// however accurate its numbers are. It is now set to what the workload was
/// measured to need rather than to a round number that sounded safe.
fn memory_limit() -> String {
    format!("{}b", TMPFS_SIZE_BYTES + SERVICE_MEMORY_BYTES)
}

/// The same figure, for a module declaring its footprint to preflight.
///
/// A container's memory is memory on the host, so a module that provisions one
/// must say so before the operator agrees to the run.
pub const fn sandbox_memory_budget_bytes() -> u64 {
    TMPFS_SIZE_BYTES + SERVICE_MEMORY_BYTES
}

/// How long the runtime gets to answer a control command.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a container gets to start serving before it is given up on.
const READY_TIMEOUT: Duration = Duration::from_secs(90);

/// How long the published port gets to accept a connection, per attempt.
///
/// Only a reachability check, not a readiness signal — see
/// [`Sandbox::wait_ready`]. Short because a loopback connect either succeeds
/// at once or is refused at once, and a longer value would only slow down the
/// loop that surrounds it.
const READY_POLL: Duration = Duration::from_millis(200);

/// Gap between passes of the readiness loop.
///
/// Every check in that loop is now a round trip to the daemon, so the cadence
/// is the loop's cost. One second bounds the wasted wait on a container that
/// dies at startup to about a second, which is the point, without making the
/// waiting itself into load on the machine under measurement.
const LIVENESS_POLL: Duration = Duration::from_secs(1);

/// How long a readiness probe gets before it is treated as "not ready".
///
/// Short, and shorter than [`CONTROL_TIMEOUT`] on purpose. A probe is asked
/// again a second later, so a slow one costs nothing but a wait — where a
/// probe that hung for the full control timeout would stall the readiness loop
/// it is supposed to drive.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// How much of a failed container's log may reach an error message.
///
/// A bundle is an artifact somebody reads and stores. A service that failed by
/// repeating one line does not get to put all of it in there.
const LOG_EXCERPT_BYTES: usize = 1024;

// ---------------------------------------------------------------------------
// Images
// ---------------------------------------------------------------------------

/// An image this agent is permitted to run.
///
/// The fields are private and there is no public constructor, so an `Image`
/// can only come from [`ALLOWED_IMAGES`]. That is the point: a caller cannot
/// assemble one from a string, so there is no path from configuration, an HTTP
/// request or a command line to "run this image".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Image {
    /// Short name a module asks for, e.g. `mariadb`.
    key: &'static str,
    /// What will actually be run, if anything.
    pin: Pin,
    /// Port inside the container the service listens on.
    service_port: u16,
    /// Directory inside the container that must be writable, mounted tmpfs.
    data_dir: &'static str,
    /// The uid and gid of the image's own service account.
    ///
    /// The container is started *as* this account rather than as root, so the
    /// entrypoint never holds the privileges it would otherwise drop. See
    /// [`run_args`] for why that is the fix rather than granting capabilities
    /// back.
    ///
    /// It is a property of the pinned digest, not of the image name — which is
    /// the second reason the digest is pinned. `every_allowed_image_runs_as_a_
    /// service_account` is what stops a future entry defaulting to root.
    run_as: (u32, u32),
    /// Roughly what fetching this image costs in bytes.
    ///
    /// The uncompressed size the runtime reports, which over-states the wire
    /// transfer because layers are pulled compressed. Over-stating is the right
    /// direction: this feeds `max_network_bytes`, which is a bound an operator
    /// agrees to at preflight, and a bound that turns out to be generous is a
    /// better failure than one that turns out to be a lie.
    download_bytes: u64,
    /// A command run *inside* the container that exits zero once the service
    /// is actually serving.
    ///
    /// See [`Sandbox::wait_ready`] for why a connect from the host is not one.
    /// A fixed argv of constants, like everything else this module runs.
    ready_probe: &'static [&'static str],
    /// Why this image is on the list. Read by a human reviewing the table.
    justification: &'static str,
}

/// Whether an allow-list entry can actually be run.
///
/// A digest has to be resolved against a real registry. On a machine with no
/// container daemon and no registry access - a sandbox, an air-gapped build
/// host, this one - there is no honest way to produce one, and writing a
/// plausible-looking `sha256:` string would be shipping a security control
/// that cannot work.
///
/// So the table carries entries that are declared but not yet pinned, and the
/// type makes them unlaunchable rather than trusting a future reader to
/// notice. A module that asks for a [`Pin::Pending`] image gets
/// [`ContainerError::ImageNotPinned`] and reports itself as not measured -
/// which is the same outcome as a missing container runtime, and it is the
/// correct one: an unpinned image is an unknown image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pin {
    /// A digest resolved against a registry. `repo@sha256:...`.
    Pinned(&'static str),
    /// Declared, not yet pinned. Carries the tag the digest must be resolved
    /// *from*, so whoever pins it does not have to guess which version this
    /// module was written against.
    Pending {
        /// e.g. `postgres:17.2-bookworm`.
        resolve_from: &'static str,
        /// Why it is not pinned yet, in a sentence an operator can read.
        blocked_by: &'static str,
    },
}

/// Every image DARCBench may run.
///
/// Two entries, both resolved against Docker Hub on 2026-08-14 on the first
/// host this project had with a container daemon. Until then the table carried
/// them as [`Pin::Pending`], because a `sha256:` string that was not resolved
/// against a registry is a security control that cannot work.
///
/// The rules for adding one, enforced by the tests below:
///
/// * **Pinned by digest.** `mariadb@sha256:...`, never `mariadb:11.4`. A tag
///   is a mutable pointer, so a tag-pinned benchmark measures whatever the
///   publisher pushed last (T-SUPPLY), and two runs a month apart are not
///   comparable even though nothing in DARCBench changed.
/// * **Official or first-party only**, and the justification says which.
/// * **One entry per service**, not per version. A second version of the same
///   service is a comparability problem the scoring layer has to know about,
///   not a table row.
pub const ALLOWED_IMAGES: &[Image] = &[
    Image {
        key: "postgres",
        // `postgres:17-bookworm` as Docker Hub served it on 2026-08-14. The date
        // matters as much as the digest: a digest is a fact about a moment, and a
        // reader six months out needs to know which moment in order to judge
        // whether a newer one would change a measurement.
        pin: Pin::Pinned(
            "postgres@sha256:84560e3b9c6874893fc4e2854f5dc3e7c1a37bc9d1dfd7a8c641310ae22ba5ad",
        ),
        // 5432 is PostgreSQL's port inside the container. Nothing is published to
        // it except loopback on this host - see `run_args`.
        service_port: 5432,
        // The official image's `PGDATA`. Mounted tmpfs, so the database is created
        // fresh for every run and nothing it writes reaches a host disk.
        data_dir: "/var/lib/postgresql/data",
        // `postgres:postgres` in the official image, both 999.
        run_as: (999, 999),
        // `-h 127.0.0.1` rather than the default socket, and it is the whole
        // point: `initdb` starts a temporary server during startup that listens
        // on the unix socket *only*, precisely so that nothing connects to a
        // database that is still being built. A probe over the socket would
        // report that one as ready.
        ready_probe: &["pg_isready", "-q", "-h", "127.0.0.1"],
        // 156 MB. By far the largest thing this program will ever fetch, which
        // is exactly why it is declared rather than left to the runtime.
        download_bytes: 156_190_575,
        justification:
            "The official PostgreSQL image, published by the PostgreSQL Docker Community. \
                    Chosen over a distribution package because the point of the container tier is \
                    that every machine measures the same server, and a distro package is whatever \
                    that distro shipped. It also carries `pgbench`, which is what makes an \
                    open-model OLTP measurement possible without this workspace growing a \
                    database driver.",
    },
    Image {
        key: "valkey",
        // `valkey/valkey:8-alpine` as Docker Hub served it on 2026-08-14.
        pin: Pin::Pinned(
            "valkey/valkey@sha256:a038175878d66b9d274fbf8be73c0305e93798b83917647f167e18cef3c71eec",
        ),
        service_port: 6379,
        // Valkey's working directory in the official image. Mounted tmpfs, so an
        // RDB snapshot goes to RAM and nothing reaches a host disk.
        data_dir: "/data",
        // `valkey:valkey` in the official image. The gid is 1000, not 999 —
        // they differ, which is exactly why this is a pair per entry rather
        // than one number reused for both.
        run_as: (999, 1000),
        // Exits 1 and says so on a refused connection, which is what makes it
        // usable as a probe rather than merely as a command that runs.
        ready_probe: &["redis-cli", "-h", "127.0.0.1", "ping"],
        download_bytes: 17_456_505,
        justification: "The official Valkey image, published by the Valkey project. Valkey rather \
                    than Redis because it is the fork the major distributions and cloud providers \
                    moved to after the 2024 licence change, and because its image is \
                    BSD-licensed throughout. It carries `redis-benchmark` and `redis-cli`, which \
                    is what makes a cache measurement possible without this workspace growing a \
                    RESP client.",
    },
    Image {
        key: "busybox",
        // `busybox:stable-musl` as Docker Hub served it on 2026-08-14.
        pin: Pin::Pinned(
            "busybox@sha256:3c6ae8008e2c2eedd141725c30b20d9c36b026eb796688f88205845ef17aa213",
        ),
        // Above 1024, because the container is not root and cannot bind below it.
        service_port: 8080,
        // Empty, and that is the point: `httpd` serving an empty tmpfs answers
        // every request with a 404, which is a response from a running server
        // and is all the health measurement needs. Putting a file in there
        // would take a bind mount, which this tier does not have and will not
        // get.
        data_dir: "/srv",
        // `nobody`, which exists in the image. Nothing here needs an identity.
        run_as: (65534, 65534),
        // An in-container TCP connect, which is a true readiness signal where
        // the same connect from the host is not - inside the container there is
        // no userland proxy to answer it. `wget` would be the obvious probe and
        // is wrong: it exits non-zero on the 404 that proves the server is up.
        ready_probe: &["nc", "-z", "127.0.0.1", "8080"],
        download_bytes: 877_398,
        justification:
            "The official BusyBox image, and the only entry here that is not a service under \
             measurement. `deployment.container` uses it to measure what starting a container \
             costs on this machine, which needs an image with something runnable in it and wants \
             that something to be as close to nothing as possible: 877 KB, one static multi-call \
             binary, no init system, no package manager and no runtime to warm up. A heavier base \
             would fold somebody else's start-up into a number that is supposed to describe the \
             machine. It carries `httpd`, which is what makes the health half measurable without \
             this workspace shipping a server into a container, and `nc`, which is what makes it \
             probeable.",
    },
];

impl Image {
    /// The only way to get an [`Image`].
    pub fn from_allow_list(key: &str) -> Option<&'static Image> {
        ALLOWED_IMAGES.iter().find(|image| image.key == key)
    }

    pub fn key(&self) -> &'static str {
        self.key
    }

    /// The reference to run, or the reason there is not one.
    pub fn reference(&self) -> Result<&'static str, ContainerError> {
        match self.pin {
            Pin::Pinned(reference) => Ok(reference),
            Pin::Pending {
                resolve_from,
                blocked_by,
            } => Err(ContainerError::ImageNotPinned {
                key: self.key,
                resolve_from,
                blocked_by,
            }),
        }
    }

    pub fn justification(&self) -> &'static str {
        self.justification
    }

    pub fn is_pinned(&self) -> bool {
        matches!(self.pin, Pin::Pinned(_))
    }

    /// What a module must declare as `max_network_bytes` if it runs this image.
    pub fn download_bytes(&self) -> u64 {
        self.download_bytes
    }
}

// ---------------------------------------------------------------------------
// The runtime
// ---------------------------------------------------------------------------

/// A container runtime that passed the safe-path check.
#[derive(Clone, Debug)]
pub struct Runtime {
    exec: Interpreter,
}

/// Why a container could not be used.
///
/// Every variant is a reason to report a module as *not measured*, never a
/// reason to fall back to something on the host. A `database.oltp` that
/// quietly measured the operator's production MySQL because no container
/// runtime was available would be the single worst thing this program could
/// do, so there is no fallback path in this type for one to travel along.
#[derive(Debug, thiserror::Error)]
pub enum ContainerError {
    #[error(
        "no container runtime was found among {candidates}. Database modules create and destroy \
         their own instance and will not use one already on this machine, so they are reported \
         as not measured rather than pointed at your server.{rejections}"
    )]
    NoRuntime {
        candidates: String,
        rejections: String,
    },
    #[error(
        "{runtime} is installed but its daemon did not answer: {detail}. The module is reported \
         as not measured; nothing on this host was used instead."
    )]
    RuntimeUnavailable { runtime: String, detail: String },
    #[error("`{key}` is not on the image allow-list, so it will not be run")]
    ImageNotAllowed { key: String },
    #[error(
        "the `{key}` image is declared but not pinned to a digest, so it will not be run and the \
         module is reported as not measured. Pin it by resolving `{resolve_from}` against a \
         registry and replacing `Pin::Pending` with `Pin::Pinned`. It is unpinned because: \
         {blocked_by}."
    )]
    ImageNotPinned {
        key: &'static str,
        resolve_from: &'static str,
        blocked_by: &'static str,
    },
    #[error("{runtime} failed to start the container: {detail}")]
    Start { runtime: String, detail: String },
    #[error("the container started but published no loopback port for {port}/tcp")]
    NoPort { port: u16 },
    #[error(
        "the container was still not serving after {}s, so nothing was measured. It said: {log}",
        .waited.as_secs()
    )]
    NotReady { waited: Duration, log: String },
    #[error(
        "the container started and then exited with status {status} after {}s, so nothing was \
         measured. It said: {log}",
        .waited.as_secs()
    )]
    Exited {
        status: String,
        waited: Duration,
        log: String,
    },
    #[error("could not run {runtime}: {source}")]
    Exec {
        runtime: String,
        #[source]
        source: ExecError,
    },
}

impl Runtime {
    /// Finds a runtime and confirms its daemon answers.
    ///
    /// Two steps, and they are separate because they fail for different
    /// reasons an operator acts on differently: "you have no container
    /// runtime" is a thing to install, and "you have Docker but its daemon is
    /// not running" is a thing to start. Collapsing them into "containers
    /// unavailable" would waste the one piece of information the operator
    /// needs.
    pub fn discover() -> Result<Self, ContainerError> {
        let (found, rejections) = runtime_exec::discover(RUNTIMES);
        let Some(exec) = found else {
            let mut detail = String::new();
            for rejection in &rejections {
                detail.push_str(&format!(
                    "\n  {} was refused: {}",
                    rejection.path.display(),
                    rejection.reason
                ));
            }
            return Err(ContainerError::NoRuntime {
                candidates: RUNTIMES.join(", "),
                rejections: detail,
            });
        };

        let runtime = Self { exec };
        // `info` rather than `version`: the client answers `version` from its
        // own binary whether or not a daemon exists, so a machine with the CLI
        // and no daemon would pass. This host is exactly that machine, which
        // is how the distinction got noticed.
        let output = runtime.control(&["info", "--format", "{{.ServerVersion}}"])?;
        if !output.succeeded() || output.stdout.trim().is_empty() {
            return Err(ContainerError::RuntimeUnavailable {
                runtime: runtime.name(),
                detail: first_line(&format!("{}{}", output.stderr, output.stdout)),
            });
        }
        Ok(runtime)
    }

    pub fn name(&self) -> String {
        self.exec.path.display().to_string()
    }

    fn control(&self, args: &[&str]) -> Result<runtime_exec::Output, ContainerError> {
        self.control_with(args, CONTROL_TIMEOUT)
    }

    /// A control command with a caller-chosen deadline.
    ///
    /// A build takes minutes where a `ps` takes milliseconds, and one timeout
    /// for both would either kill builds or let a wedged `ps` hold the run.
    fn control_with(
        &self,
        args: &[&str],
        timeout: Duration,
    ) -> Result<runtime_exec::Output, ContainerError> {
        runtime_exec::run(&self.exec, args, timeout).map_err(|source| ContainerError::Exec {
            runtime: self.name(),
            source,
        })
    }

    /// Builds an image from a context directory this agent created.
    ///
    /// **The only place a host path enters an argument vector in this module,
    /// and the exception is unavoidable**: a build context *is* a directory,
    /// and there is no way to hand a daemon one without naming it. The
    /// property `run_args` holds - that nothing from the host is ever visible
    /// inside a container - is untouched by this, because a build context is
    /// copied into the build rather than mounted into a running container, and
    /// because the directory named here is one this agent made under its own
    /// scratch path microseconds earlier.
    ///
    /// `context` is therefore never caller-supplied and never can be: the only
    /// caller is `deployment.container`, which passes a directory it created
    /// itself. It is checked to be absolute rather than trusted to be, because
    /// a relative path would resolve against the daemon's working directory
    /// rather than this process's and could land anywhere.
    ///
    /// The image carries this agent's label, so [`Self::reap_images`] can find
    /// it after a crash without ever matching one of the operator's.
    pub fn build(
        &self,
        context: &Path,
        tag: &str,
        no_cache: bool,
        timeout: Duration,
    ) -> Result<runtime_exec::Output, ContainerError> {
        if !context.is_absolute() {
            return Err(ContainerError::Start {
                runtime: self.name(),
                detail: format!(
                    "build context {} is not an absolute path",
                    context.display()
                ),
            });
        }
        let context = context.display().to_string();
        let mut args: Vec<&str> = vec![
            "build",
            "--label",
            OWNER_LABEL,
            // No network during the build. A `RUN apt-get` in a generated
            // Dockerfile would turn a deployment measurement into a
            // measurement of a package mirror, and this module generates its
            // own Dockerfile so nothing needs one.
            "--network",
            "none",
            "-t",
            tag,
        ];
        if no_cache {
            args.push("--no-cache");
        }
        args.push(&context);
        self.control_with(&args, timeout)
    }

    /// Writes an image to a tar archive at a path this agent created.
    pub fn save_image(
        &self,
        tag: &str,
        archive: &Path,
        timeout: Duration,
    ) -> Result<runtime_exec::Output, ContainerError> {
        let archive = archive.display().to_string();
        self.control_with(&["save", "-o", &archive, tag], timeout)
    }

    /// Reads an image back from a tar archive.
    pub fn load_image(
        &self,
        archive: &Path,
        timeout: Duration,
    ) -> Result<runtime_exec::Output, ContainerError> {
        let archive = archive.display().to_string();
        self.control_with(&["load", "-i", &archive], timeout)
    }

    /// Removes an image by tag, best effort.
    pub fn remove_image(&self, tag: &str) {
        let _ = self.control(&["rmi", "-f", tag]);
    }

    /// Removes every image this agent labelled.
    ///
    /// The image half of [`Self::reap`], and label-scoped for the same reason:
    /// a build that was killed leaves an image behind, and an operator's own
    /// images must never be in range of a cleanup this program performs.
    pub fn reap_images(&self) -> Result<usize, ContainerError> {
        let listed = self.control(&["images", "-q", "--filter", OWNER_FILTER])?;
        let ids: Vec<&str> = listed.stdout.split_whitespace().collect();
        if ids.is_empty() {
            return Ok(0);
        }
        let mut args = vec!["rmi", "-f"];
        args.extend_from_slice(&ids);
        self.control(&args)?;
        Ok(ids.len())
    }

    /// Makes sure the image is on this machine, fetching it if it is not.
    ///
    /// Returns whether it had to fetch, so the module can disclose that it did.
    ///
    /// # Why this is a step rather than something the runtime does for you
    ///
    /// It is, in fact, something the runtime does for you: `docker run` on an
    /// absent image pulls it first. That is precisely the problem, and it went
    /// unnoticed until this host was made to run a module without the image
    /// already local.
    ///
    /// Two things were wrong with letting it happen implicitly.
    ///
    /// **The transfer was undeclared.** All three container modules said
    /// `max_network_bytes: 0`, and `database.oltp`'s comment even stated the
    /// assumption - "the image is pulled by the container runtime before the
    /// run" - with nothing making it true. On a machine that has never run
    /// DARCBench, that module fetches 156 MB while preflight tells the operator
    /// the run uses no network at all. On a metered VPS that is somebody's
    /// money, and it is the sort of promise this program does not get to break.
    ///
    /// **And the pull landed inside a measurement.** With the base image
    /// removed, `deployment.container`'s startup figure came back with a 147%
    /// coefficient of variation: six repetitions of a container start and one
    /// of a container start plus a download. The variance sweep caught it,
    /// which is the sweep working - but a metric that needs a warning to be
    /// interpretable is one measured wrong.
    ///
    /// So the fetch is explicit, before the clock starts, and reported.
    pub fn ensure_image_present(
        &self,
        image: &'static Image,
        timeout: Duration,
    ) -> Result<bool, ContainerError> {
        let reference = image.reference()?;

        // `inspect` rather than `pull` with a hope that it is a no-op: a pull
        // of a present image still contacts the registry to check, which is a
        // network round trip on a run that may have been promised none.
        let present = self
            .control(&["image", "inspect", "--format", "{{.Id}}", reference])
            .map(|output| output.succeeded())
            .unwrap_or(false);
        if present {
            return Ok(false);
        }

        let pulled = self.control_with(&["pull", "--quiet", reference], timeout)?;
        if !pulled.succeeded() {
            return Err(ContainerError::Start {
                runtime: self.name(),
                detail: format!(
                    "{} could not be fetched: {}",
                    image.key,
                    first_line(&format!("{}{}", pulled.stderr, pulled.stdout))
                ),
            });
        }
        Ok(true)
    }

    /// Runs one command in a throwaway container and returns how long the
    /// whole round trip took.
    ///
    /// Create, start, exec, exit, remove — measured in the foreground, so the
    /// figure is a wall clock rather than the resolution of a poll. See
    /// [`ephemeral_run_args`] for the argument vector and why it is a second
    /// one.
    ///
    /// `None` if the command did not succeed. A container that failed to start
    /// also "finishes quickly", and timing that would report the fastest
    /// container start on record.
    pub fn run_ephemeral(
        &self,
        image: &'static Image,
        run_id: &str,
        argv: &[&str],
        timeout: Duration,
    ) -> Result<Option<Duration>, ContainerError> {
        let reference = image.reference()?;
        let name = container_name(run_id, image.key);
        let args = ephemeral_run_args(image, reference, &name, argv);
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();

        let started = Instant::now();
        let output = self.control_with(&borrowed, timeout)?;
        let elapsed = started.elapsed();

        if !output.succeeded() {
            // `--rm` should have removed it; this covers the case where the
            // failure was early enough that it did not.
            remove(&self.exec, &name);
            return Ok(None);
        }
        Ok(Some(elapsed))
    }

    /// Removes every container this agent labelled, from this run or any
    /// earlier one.
    ///
    /// Called before a module starts, because the release profile aborts on
    /// panic and nothing runs on `SIGKILL` - so a run that died leaves its
    /// container behind and only the next run can clear it.
    ///
    /// Label-scoped, never name-scoped. A name prefix could collide with
    /// something an operator chose; a label this agent sets could not have
    /// been set by anything else. Returns how many were removed, so a module
    /// can disclose that it cleaned up after a previous failure rather than
    /// doing it silently.
    pub fn reap(&self) -> Result<usize, ContainerError> {
        let listed = self.control(&["ps", "-aq", "--filter", OWNER_FILTER])?;
        let ids: Vec<&str> = listed.stdout.split_whitespace().collect();
        if ids.is_empty() {
            return Ok(0);
        }
        let mut args = vec!["rm", "-f"];
        args.extend_from_slice(&ids);
        self.control(&args)?;
        Ok(ids.len())
    }
}

// ---------------------------------------------------------------------------
// The argument vector
// ---------------------------------------------------------------------------

/// Builds the `run` arguments for a sandboxed service.
///
/// A pure function, separated from everything that touches a daemon, because
/// this vector *is* the isolation and it has to be provable on a machine with
/// no container runtime at all. Every dangerous thing a container can do is
/// something that would have to appear here, so a test that reads the whole
/// vector is a test of the whole boundary.
///
/// `env` is a slice of `KEY=VALUE` pairs the image needs to start, supplied by
/// the module. They reach the container's environment and nothing else - they
/// cannot become flags, because they follow `--env` one value at a time rather
/// than being spliced into a string.
///
/// # Why there is no root inside, and no `--rm`
///
/// Both of these were changed by the first run this module ever made against a
/// real daemon, and neither could have been found without one.
///
/// **The container runs as the image's own service account.** With
/// `--cap-drop ALL` and nothing else, the official PostgreSQL entrypoint dies
/// on its second line: it starts as root, `chown`s `PGDATA`, and `gosu`s down
/// to `postgres`. Dropping `CAP_CHOWN` stops the `chown` and `set -e` does the
/// rest — `chown: changing ownership of '/var/lib/postgresql/data': Operation
/// not permitted`.
///
/// The documented fix, and the one the image's own README gives, is to add
/// `CHOWN`, `DAC_OVERRIDE`, `FOWNER`, `SETUID` and `SETGID` back. That is the
/// wrong trade here. Those five are close to the whole interesting surface of
/// a container escape, and they would be granted for no purpose except letting
/// a process hold privileges long enough to drop them. Starting as `--user
/// 999:999` skips the entire dance: the entrypoint sees it is not root, does
/// not try to `chown`, and the privileges are never held rather than held and
/// surrendered. `no-new-privileges` then makes the state permanent.
///
/// The cost is that the tmpfs has to arrive already owned — Docker's default
/// is root-owned and `1777`, and PostgreSQL refuses a `PGDATA` that is group
/// or world accessible — so the mount carries `mode`, `uid` and `gid`. `0700`
/// rather than `0750`: the service account is the only account that has any
/// business in there.
///
/// **`--rm` is gone**, and it was actively harmful. A container that dies
/// during startup is the case that most needs explaining, and `--rm` deletes
/// it — along with its logs — before anything can read them. That is not
/// hypothetical: the failure above was invisible on the first attempt for
/// exactly this reason, and appeared the moment the flag came off.
/// [`Sandbox::wait_ready`] now watches for the exit and puts the log in the
/// error. Removal is [`Drop`]'s job and [`Runtime::reap`]'s, which is where it
/// always really was — `--rm` never covered the case those two exist for.
fn run_args<'a>(
    image: &'a Image,
    reference: &'a str,
    name: &'a str,
    env: &'a [String],
) -> Vec<String> {
    let (uid, gid) = image.run_as;
    let mut args: Vec<String> = vec![
        "run".into(),
        "--detach".into(),
        "--name".into(),
        name.into(),
        "--label".into(),
        OWNER_LABEL.into(),
        // The image's own service account, so nothing inside is ever root.
        "--user".into(),
        format!("{uid}:{gid}"),
        // Published on loopback, never on a routable address. An unpublished
        // port would be safer still and is not possible: the load has to reach
        // the service from this process.
        "--publish".into(),
        format!("127.0.0.1::{}", image.service_port),
        // Bounds a runaway container. A benchmark that causes an outage is a
        // failure however accurate its numbers are. It covers the tmpfs below
        // as well as the service - see `memory_limit`, which is why it is one
        // number computed from both rather than two chosen separately.
        "--memory".into(),
        memory_limit(),
        "--memory-swap".into(),
        memory_limit(),
        "--pids-limit".into(),
        "512".into(),
        // No new privileges, and every capability dropped that the service
        // does not need. A database does not need to load kernel modules.
        "--security-opt".into(),
        "no-new-privileges".into(),
        "--cap-drop".into(),
        "ALL".into(),
        // The data directory is a tmpfs, so the service has somewhere to write
        // and nothing it writes touches a host filesystem or outlives the
        // container. This is also what makes every run start from an identical
        // empty database rather than whatever the last run left.
        //
        // Owned by the service account and `0700`, because the container is
        // not root and so cannot fix the ownership itself - and because the
        // default, root-owned and `1777`, is one PostgreSQL refuses outright.
        "--tmpfs".into(),
        format!(
            "{}:rw,size={},mode=0700,uid={uid},gid={gid}",
            image.data_dir, TMPFS_SIZE_BYTES
        ),
    ];
    for pair in env {
        args.push("--env".into());
        args.push(pair.clone());
    }
    // The image reference is last. It came from `Image::reference`, which
    // only yields a `&'static str` for a digest-pinned entry - so an unpinned
    // image cannot reach this line at all. Nothing a caller typed can appear
    // here either.
    args.push(reference.into());
    args
}

/// Builds the `run` arguments for a one-shot foreground command.
///
/// The second argument vector in this module, and it exists because
/// `deployment.container` asks a question the first one cannot answer: **what
/// does starting a container cost here?** [`run_args`] always detaches,
/// publishes a port and mounts a tmpfs, because it is building a service to
/// measure. Timing that would measure the service. What is wanted instead is
/// the smallest possible round trip — create, start, exec something trivial,
/// exit, remove — timed in the foreground so the number is a wall clock and not
/// the resolution of a polling loop.
///
/// It is a separate function rather than a flag on the first because the two
/// vectors have to be *read* separately. This vector is the isolation for every
/// container that goes through it, so the same tests apply to the whole of it:
/// no host path, no added capability, nothing privileged, nothing published.
///
/// **Nothing is published and nothing is mounted**, which makes this strictly
/// more contained than [`run_args`] rather than a relaxation of it. `--rm` *is*
/// used here, and for once it is right: the container is expected to exit, its
/// exit is the measurement, and there is no startup log to lose because the
/// command is chosen to do nothing at all.
///
/// `argv` is a fixed vector of constants the module owns. Nothing a caller
/// typed reaches it, for the same reason nothing does in [`Sandbox::exec`].
fn ephemeral_run_args<'a>(
    image: &'a Image,
    reference: &'a str,
    name: &'a str,
    argv: &'a [&'a str],
) -> Vec<String> {
    let (uid, gid) = image.run_as;
    let mut args: Vec<String> = vec![
        "run".into(),
        // Foreground: the command's own wall time is what is being measured,
        // and detaching would replace it with how fast this process can poll.
        "--rm".into(),
        "--name".into(),
        name.into(),
        "--label".into(),
        OWNER_LABEL.into(),
        "--user".into(),
        format!("{uid}:{gid}"),
        "--network".into(),
        // Not merely unpublished: no network namespace with an interface at
        // all. Nothing here has anything to talk to.
        "none".into(),
        "--memory".into(),
        memory_limit(),
        "--memory-swap".into(),
        memory_limit(),
        "--pids-limit".into(),
        "512".into(),
        "--security-opt".into(),
        "no-new-privileges".into(),
        "--cap-drop".into(),
        "ALL".into(),
        reference.into(),
    ];
    for argument in argv {
        args.push((*argument).into());
    }
    args
}

// ---------------------------------------------------------------------------
// The sandbox
// ---------------------------------------------------------------------------

/// A running, isolated service. Dropping it removes the container.
#[derive(Debug)]
pub struct Sandbox {
    runtime: Interpreter,
    name: String,
    address: SocketAddr,
    ready_probe: &'static [&'static str],
}

impl Sandbox {
    /// Starts `image`, waits for it to accept a connection, and returns it.
    ///
    /// On any failure after the container starts, the container is removed
    /// before the error is returned. A half-started sandbox left running would
    /// be exactly the leak `reap` exists to clean up, arriving through the
    /// error path of the function that was supposed to prevent it.
    pub fn launch(
        runtime: &Runtime,
        image: &'static Image,
        run_id: &str,
        env: &[String],
    ) -> Result<Self, ContainerError> {
        let sandbox = Self::launch_without_waiting(runtime, image, run_id, env, &[])?;
        // `wait_ready` removes the container itself on failure, because it is
        // the only place that knows the container was reachable but never
        // answered - a distinction worth keeping out of this function.
        sandbox.wait_ready()?;
        Ok(sandbox)
    }

    /// Starts `image` and returns as soon as its port is published, without
    /// waiting for the service inside to be serving.
    ///
    /// For a caller that wants to *measure* how long becoming ready takes.
    /// [`Self::wait_ready`] polls on a one-second cadence, which is the right
    /// cost for a guard and useless as a stopwatch for an event that takes a
    /// few hundred milliseconds — so `deployment.container` times its own
    /// tight loop instead of being handed a number rounded to the second.
    ///
    /// The container is still a [`Sandbox`], so [`Drop`] still removes it and
    /// nothing leaks if the caller's loop gives up. What the caller loses is
    /// the guarantee that anything inside is answering, which is precisely
    /// what it has undertaken to find out.
    ///
    /// `command` overrides the image's own, for an image whose default does
    /// not serve. Fixed constants from the module, never anything a caller
    /// typed — the same rule as everywhere else here.
    pub fn launch_without_waiting(
        runtime: &Runtime,
        image: &'static Image,
        run_id: &str,
        env: &[String],
        command: &[&str],
    ) -> Result<Self, ContainerError> {
        // Before anything else, so an unpinned image never reaches a daemon.
        let reference = image.reference()?;
        let name = container_name(run_id, image.key);
        let mut args = run_args(image, reference, &name, env);
        // After the reference, which `run_args` puts last: everything past it
        // is the container's command rather than a flag to the runtime, which
        // is what makes appending here safe.
        args.extend(command.iter().map(|part| (*part).to_string()));
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();

        let started = runtime.control(&borrowed)?;
        if !started.succeeded() {
            return Err(ContainerError::Start {
                runtime: runtime.name(),
                detail: first_line(&format!("{}{}", started.stderr, started.stdout)),
            });
        }

        let sandbox = |address| Self {
            runtime: runtime.exec.clone(),
            name: name.clone(),
            address,
            ready_probe: image.ready_probe,
        };

        // From here on every failure has to take the container with it.
        let address = match published_port(runtime, &name, image.service_port) {
            Ok(address) => address,
            Err(error) => {
                remove(&runtime.exec, &name);
                return Err(error);
            }
        };
        Ok(sandbox(address))
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Runs a command *inside* the container and returns what it printed.
    ///
    /// This is how `database.oltp` reaches `pgbench`: the tool ships in the
    /// image, so the measurement is taken by a client the image's publisher
    /// built, against a server in the same container. No database driver has
    /// to exist in this workspace, and nothing on the host is involved.
    ///
    /// `argv` is a fixed vector a module builds from constants and numbers.
    /// It is passed through element by element - never joined into a string
    /// and never handed to a shell - so a value containing a space or a
    /// semicolon is one argument and stays one argument.
    pub fn exec(
        &self,
        argv: &[&str],
        timeout: Duration,
    ) -> Result<runtime_exec::Output, ContainerError> {
        let mut args: Vec<&str> = vec!["exec", &self.name];
        args.extend_from_slice(argv);
        runtime_exec::run(&self.runtime, &args, timeout).map_err(|source| ContainerError::Exec {
            runtime: self.runtime.path.display().to_string(),
            source,
        })
    }

    /// Polls until the service accepts a TCP connection, or gives up.
    ///
    /// A TCP connect rather than a protocol handshake. It is what every
    /// service has in common, and the alternative - speaking MySQL to find out
    /// whether MySQL is up - would put a protocol implementation in the
    /// isolation tier for each service it isolates. A module that needs a
    /// stronger readiness signal than "the port answers" is better placed to
    /// send it than this is.
    /// Polls until the service is serving, the container dies, or the deadline
    /// passes.
    ///
    /// # Why the readiness signal comes from inside the container
    ///
    /// This used to be a TCP connect from the host to the published port, on
    /// the reasoning that it is the one signal every service has in common and
    /// that speaking a protocol to find out whether that protocol is up would
    /// put a client for each service into the isolation tier.
    ///
    /// The reasoning was sound and the check was worthless, which the first
    /// real launch showed within a second. Docker publishes a port by putting
    /// a userland proxy on it, and that proxy starts listening when the
    /// *container* starts — not when the service inside it does. So the connect
    /// succeeded immediately, every time, whatever the service was doing.
    /// Measured on this host: at 0.5 s the host port accepted connections and
    /// `pg_isready` still said no. `database.oltp` then failed in 0.8 s with
    /// `connection refused` from `pgbench`, having been handed a sandbox
    /// declared ready.
    ///
    /// `database.cache` passed the same broken check, which is the part worth
    /// keeping in mind: Valkey starts in about a tenth of a second, so it won
    /// the race every time. A green result from an unsound check is not
    /// evidence, and the two modules differed only in how fast their service
    /// happened to start.
    ///
    /// So the probe runs inside the container, and each image brings its own —
    /// `pg_isready`, `redis-cli ping` — which is not a client in this tier but
    /// a command in an image the tier already trusts enough to run. The host
    /// connect stays as a first gate, because it does prove one thing the
    /// in-container probe cannot: that the port was published where this
    /// process can reach it, which is where the load will come from.
    ///
    /// # And why liveness is checked too
    ///
    /// A container that fails during startup never becomes ready, so a loop
    /// that only polls readiness waits the full ninety seconds and reports a
    /// timeout — which describes what the loop did, not what went wrong. The
    /// container had already exited in under a second and said why on the way
    /// out. So the run now fails in about the time the container took to fail,
    /// carrying the container's own account of it.
    ///
    /// Both of these are daemon round trips, so they run on the coarser
    /// [`LIVENESS_POLL`] cadence rather than [`READY_POLL`]'s: polling a
    /// daemon five times a second is itself load on a machine whose spare
    /// capacity is the thing about to be measured.
    fn wait_ready(&self) -> Result<(), ContainerError> {
        let started = Instant::now();
        let deadline = started + READY_TIMEOUT;
        let mut port_reachable = false;
        while Instant::now() < deadline {
            // Cheap, and a precondition rather than a readiness signal: if the
            // publish did not land, no amount of the service being up helps.
            if !port_reachable {
                port_reachable = TcpStream::connect_timeout(&self.address, READY_POLL).is_ok();
            }
            if port_reachable && self.probe_says_ready() {
                return Ok(());
            }
            if let Some(status) = self.exited_status() {
                let log = self.last_log_lines();
                remove(&self.runtime, &self.name);
                return Err(ContainerError::Exited {
                    status,
                    waited: started.elapsed(),
                    log,
                });
            }
            std::thread::sleep(LIVENESS_POLL);
        }
        // The container is removed here rather than left for `reap`, because
        // an operator watching a run fail should not also be left with a
        // container consuming memory until the next one starts.
        let log = self.last_log_lines();
        remove(&self.runtime, &self.name);
        Err(ContainerError::NotReady {
            waited: READY_TIMEOUT,
            log,
        })
    }

    /// Whether the image's own readiness command says the service is serving.
    ///
    /// A probe that could not be run is "not ready", never "ready": the whole
    /// value of this check is that it fails closed, and a daemon hiccup read
    /// as a green light would hand a module an empty database.
    fn probe_says_ready(&self) -> bool {
        self.exec(self.ready_probe, PROBE_TIMEOUT)
            .map(|output| output.succeeded())
            .unwrap_or(false)
    }

    /// `Some(status)` if the container is no longer running.
    ///
    /// `None` covers both "running" and "the runtime would not say", and they
    /// are deliberately the same answer: an inspect that failed is not
    /// evidence the container died, and treating it as such would turn a
    /// hiccup in the daemon into a failed benchmark.
    fn exited_status(&self) -> Option<String> {
        let output = runtime_exec::run(
            &self.runtime,
            &[
                "inspect",
                "--format",
                "{{.State.Status}} {{.State.ExitCode}}",
                &self.name,
            ],
            CONTROL_TIMEOUT,
        )
        .ok()?;
        let reported = output.stdout.trim();
        let (state, code) = reported.split_once(' ')?;
        match state {
            "running" | "created" | "restarting" => None,
            _ => Some(format!("{state} ({code})")),
        }
    }

    /// The tail of the container's own log, for an error message.
    ///
    /// Bounded, and both bounds matter. This text goes into a benchmark
    /// bundle: a service that failed by printing a megabyte of the same line
    /// would otherwise put a megabyte of it in the artifact, and a database's
    /// startup log is one of the places a connection string can appear.
    fn last_log_lines(&self) -> String {
        let Ok(output) = runtime_exec::run(
            &self.runtime,
            &["logs", "--tail", "12", &self.name],
            CONTROL_TIMEOUT,
        ) else {
            return "the runtime would not return the container's log".into();
        };
        // Startup failures reach stderr far more often than stdout, and the
        // interesting line is the last one either way.
        let mut text: String =
            format!("{}\n{}", output.stdout.trim_end(), output.stderr.trim_end())
                .lines()
                .filter(|line| !line.trim().is_empty())
                .collect::<Vec<_>>()
                .join(" | ");
        if text.len() > LOG_EXCERPT_BYTES {
            let cut = text
                .char_indices()
                .map(|(at, _)| at)
                .take_while(|at| *at <= LOG_EXCERPT_BYTES)
                .last()
                .unwrap_or(0);
            text.truncate(cut);
            text.push_str(" [...]");
        }
        if text.is_empty() {
            return "the container printed nothing before it exited".into();
        }
        text
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        remove(&self.runtime, &self.name);
    }
}

/// Removes one container, best effort.
///
/// Best effort because this runs on `Drop`, and a `Drop` that can fail is a
/// `Drop` whose failure nobody handles. What makes the best effort acceptable
/// is [`Runtime::reap`]: anything missed here is labelled, and the next run
/// clears it.
fn remove(runtime: &Interpreter, name: &str) {
    let _ = runtime_exec::run(runtime, &["rm", "-f", name], CONTROL_TIMEOUT);
}

/// The container's name: this agent, this run, this image.
///
/// The run id is already `run_` plus 32 lowercase hex characters, so it cannot
/// contain anything a shell or a flag parser would notice - but the name is
/// filtered anyway rather than trusted, because "it cannot contain that" is a
/// property of a type two crates away and this is the string that becomes an
/// argument.
fn container_name(run_id: &str, key: &str) -> String {
    let safe: String = run_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .take(48)
        .collect();
    format!("darcbench-{safe}-{key}")
}

/// Asks the runtime which loopback port the service was published on.
fn published_port(
    runtime: &Runtime,
    name: &str,
    service_port: u16,
) -> Result<SocketAddr, ContainerError> {
    let spec = format!("{service_port}/tcp");
    let output = runtime.control(&["port", name, &spec])?;
    // `docker port` prints `127.0.0.1:49154`, one line per binding. Only a
    // loopback binding is accepted: if a runtime ever published elsewhere
    // despite the `--publish` above, connecting to it anyway would make the
    // one guarantee this module offers silently false.
    output
        .stdout
        .lines()
        .filter_map(|line| line.trim().rsplit_once(':'))
        .filter_map(|(host, port)| {
            let host = host.trim_start_matches('[').trim_end_matches(']');
            let ip: IpAddr = host.parse().ok()?;
            let port: u16 = port.trim().parse().ok()?;
            ip.is_loopback().then_some(SocketAddr::new(ip, port))
        })
        .next()
        .ok_or(ContainerError::NoPort { port: service_port })
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no output")
        .to_string()
}

/// The loopback address a sandbox is reachable on, for a module's own use.
pub fn loopback(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Stands in for a real allow-list entry, so the argument vector can be
    /// tested on a machine with no container runtime. It is `pub(self)` and
    /// lives in the test module, so it cannot reach [`Image::from_allow_list`]
    /// and cannot be launched by anything.
    const TEST_REFERENCE: &str =
        "example@sha256:0000000000000000000000000000000000000000000000000000000000000000";

    const TEST_IMAGE: Image = Image {
        key: "testsvc",
        pin: Pin::Pinned(TEST_REFERENCE),
        service_port: 3306,
        data_dir: "/var/lib/testsvc",
        run_as: (999, 998),
        ready_probe: &["testsvc-isready"],
        download_bytes: 1_000_000,
        justification: "test fixture; never launched",
    };

    fn args() -> Vec<String> {
        run_args(
            &TEST_IMAGE,
            TEST_REFERENCE,
            "darcbench-run_abc-testsvc",
            &["PASSWORD=x".to_string()],
        )
    }

    #[test]
    fn no_host_path_can_reach_the_argument_vector() {
        // The failure this prevents is total: `-v /:/host` on a container
        // running as root is a root shell on the machine being benchmarked.
        // Asserted over the whole vector rather than over the code that builds
        // it, so a mount added anywhere still fails this.
        let args = args();
        for forbidden in ["-v", "--volume", "--mount", "--privileged", "--device"] {
            assert!(
                !args.iter().any(|arg| arg == forbidden),
                "`{forbidden}` reached the argument vector: {args:?}"
            );
        }
        // And nothing that looks like a bind mount, whatever flag carried it.
        assert!(
            !args
                .iter()
                .any(|arg| arg.contains(":/") && arg.contains('/') && arg.starts_with('/')),
            "an argument looks like a host bind mount: {args:?}"
        );
    }

    #[test]
    fn a_published_port_is_never_reachable_off_this_machine() {
        // A benchmark database reachable from the network is a benchmark
        // database somebody else can reach.
        let args = args();
        let publish = args
            .iter()
            .position(|arg| arg == "--publish")
            .expect("nothing was published");
        let spec = &args[publish + 1];
        assert!(spec.starts_with("127.0.0.1:"), "published as {spec}");
        assert!(!spec.starts_with("0.0.0.0"), "published as {spec}");
    }

    #[test]
    fn the_data_directory_is_a_tmpfs_that_cannot_outlive_the_run() {
        // Two properties in one: no host disk is written, and every run starts
        // from an identical empty database rather than whatever the last one
        // left behind.
        let args = args();
        let tmpfs = args
            .iter()
            .position(|arg| arg == "--tmpfs")
            .expect("no tmpfs");
        assert!(args[tmpfs + 1].starts_with("/var/lib/testsvc:rw,size="));
    }

    #[test]
    fn nothing_inside_the_container_is_ever_root() {
        // The alternative fix for the startup failure this replaced was
        // `--cap-add CHOWN,DAC_OVERRIDE,FOWNER,SETUID,SETGID`, which is what
        // the image's own documentation suggests. This test is the reason that
        // cannot quietly come back: it fails on a `--user` that is missing, on
        // one that is `0`, and on any capability being added at all.
        let args = args();
        let user = args
            .iter()
            .position(|arg| arg == "--user")
            .expect("the container was not given a user, so it runs as root");
        let spec = &args[user + 1];
        assert_eq!(spec, "999:998", "ran as {spec}");
        let (uid, gid) = spec.split_once(':').expect("not a uid:gid pair");
        assert_ne!(uid, "0", "the container runs as root");
        assert_ne!(gid, "0", "the container runs as the root group");
        assert!(
            !args.iter().any(|arg| arg == "--cap-add"),
            "a capability was added back: {args:?}"
        );
    }

    #[test]
    fn the_tmpfs_arrives_owned_by_the_service_account_and_closed_to_everyone_else() {
        // A container that is not root cannot fix the ownership of its own
        // data directory, so the mount has to arrive correct. Docker's default
        // is root-owned and 1777, which PostgreSQL refuses outright and which
        // would be worth refusing anyway.
        let args = args();
        let tmpfs = args
            .iter()
            .position(|arg| arg == "--tmpfs")
            .expect("no tmpfs");
        let spec = &args[tmpfs + 1];
        assert!(spec.contains("uid=999"), "{spec}");
        assert!(spec.contains("gid=998"), "{spec}");
        assert!(spec.contains("mode=0700"), "{spec}");
    }

    #[test]
    fn a_container_that_dies_at_startup_is_not_deleted_before_it_can_be_read() {
        // `--rm` deletes a container the instant it exits, taking its log with
        // it - and a container that exits during startup is precisely the one
        // whose log is the whole diagnosis. The first real launch this module
        // ever made reported nothing at all for this reason.
        //
        // Removal is `Drop`'s job and `reap`'s. Neither is `--rm`, which never
        // covered the case they exist for: an agent killed while a container
        // is still running.
        let args = args();
        assert!(
            !args.iter().any(|arg| arg == "--rm"),
            "--rm is back, and a failed container's log will be deleted before it is read"
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--label" && w[1] == OWNER_LABEL),
            "without the label, nothing can reap what --rm no longer removes"
        );
    }

    #[test]
    fn every_allowed_image_brings_a_readiness_probe_of_its_own() {
        // The check this replaced was a TCP connect from the host, which
        // Docker's userland proxy answers as soon as the container exists and
        // regardless of what the service inside is doing. It passed instantly
        // and always, and it handed `database.oltp` a PostgreSQL that was
        // still running `initdb`.
        //
        // A probe is therefore per-image and mandatory. An entry added without
        // one fails here rather than silently inheriting the old behaviour,
        // which is the failure mode that made the original worth fixing.
        for image in ALLOWED_IMAGES {
            assert!(
                !image.ready_probe.is_empty(),
                "{} has no readiness probe, so it would be declared ready as soon as its \
                 container existed",
                image.key
            );
            for argument in image.ready_probe {
                assert!(
                    !argument.is_empty(),
                    "{}: an empty argument in the probe",
                    image.key
                );
            }
        }
    }

    #[test]
    fn every_allowed_image_runs_as_a_service_account() {
        // Applied to the real table rather than the fixture, because the way
        // this goes wrong is a new entry added without the field being thought
        // about.
        for image in ALLOWED_IMAGES {
            let (uid, gid) = image.run_as;
            assert_ne!(uid, 0, "{} would run as root", image.key);
            assert_ne!(gid, 0, "{} would run as the root group", image.key);
        }
    }

    #[test]
    fn the_ephemeral_vector_is_at_least_as_contained_as_the_service_one() {
        // A second argument vector is a second chance to get the isolation
        // wrong, so it gets the same treatment as the first: the whole vector
        // is read, not the code that builds it.
        //
        // Asserted as "at least as contained", not "the same": this one is
        // strictly tighter. Nothing is published, nothing is mounted and the
        // network namespace has no interface at all, because a container whose
        // whole job is to exit has nothing to talk to and nowhere to write.
        let args = ephemeral_run_args(&TEST_IMAGE, TEST_REFERENCE, "n", &["true"]);

        for forbidden in [
            "-v",
            "--volume",
            "--mount",
            "--privileged",
            "--device",
            "--cap-add",
            "--publish",
            "-p",
            "--tmpfs",
        ] {
            assert!(
                !args.iter().any(|arg| arg == forbidden),
                "`{forbidden}` reached the ephemeral vector: {args:?}"
            );
        }
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--cap-drop" && w[1] == "ALL"));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--security-opt" && w[1] == "no-new-privileges"));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--network" && w[1] == "none"));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--user" && w[1] == "999:998"));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--label" && w[1] == OWNER_LABEL));
        // `--rm` is right here and wrong in `run_args`, which is worth an
        // assertion rather than a comment: this container is *expected* to
        // exit, its exit is the measurement, and there is no startup log to
        // lose because the command does nothing.
        assert!(args.iter().any(|arg| arg == "--rm"));
        assert!(!args.iter().any(|arg| arg == "--detach"));
    }

    #[test]
    fn the_ephemeral_command_cannot_be_read_as_a_flag() {
        // Everything after the image reference is the container's command
        // rather than an argument to the runtime, so the reference has to come
        // before all of it. If a command element could land ahead of the
        // reference it would be parsed by the runtime instead of passed to the
        // container - which is the whole of the difference between running
        // `true` and running `--privileged`.
        let hostile = ["--privileged", "-v", "/:/host"];
        let args = ephemeral_run_args(&TEST_IMAGE, TEST_REFERENCE, "n", &hostile);
        let reference_at = args
            .iter()
            .position(|arg| arg == TEST_REFERENCE)
            .expect("the image reference is missing");
        assert_eq!(
            &args[reference_at + 1..],
            &hostile,
            "the command must be exactly the tail after the reference"
        );
        for argument in &hostile {
            assert!(
                args[..reference_at].iter().all(|arg| arg != argument),
                "`{argument}` appeared before the image reference: {args:?}"
            );
        }
    }

    #[test]
    fn the_container_drops_every_capability_and_gains_no_privileges() {
        let args = args();
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--cap-drop" && w[1] == "ALL"));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--security-opt" && w[1] == "no-new-privileges"));
    }

    #[test]
    fn a_runaway_container_cannot_take_the_host_down() {
        let args = args();
        let limit = memory_limit();
        assert!(args.windows(2).any(|w| w[0] == "--memory" && w[1] == limit));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--memory-swap" && w[1] == limit));
        assert!(args.windows(2).any(|w| w[0] == "--pids-limit"));
    }

    #[test]
    fn the_memory_limit_covers_the_data_directory_it_has_to_hold() {
        // A tmpfs is page cache, and its pages are charged to the cgroup that
        // faults them in. So a limit equal to the tmpfs size leaves the service
        // nothing: `database.oltp` built a 324 MiB dataset under a 512 MiB
        // limit with a 512 MiB tmpfs and was OOM-killed mid-phase.
        //
        // The point of this test is not the arithmetic, which is trivial. It
        // is that the two constants are one budget, and that someone raising
        // the tmpfs to hold a bigger dataset has to raise the ceiling with it.
        assert!(
            sandbox_memory_budget_bytes() > TMPFS_SIZE_BYTES,
            "the memory ceiling does not exceed the tmpfs it must contain, so a full data \
             directory leaves the service no memory at all"
        );
        assert_eq!(
            sandbox_memory_budget_bytes(),
            TMPFS_SIZE_BYTES + SERVICE_MEMORY_BYTES
        );
        // And the flag says bytes explicitly, so no unit suffix can be misread.
        assert!(memory_limit().ends_with('b'), "{}", memory_limit());
    }

    #[test]
    fn environment_values_cannot_become_flags() {
        // Each pair follows its own `--env`, so a value containing a space or
        // a leading dash is one argument and stays one argument.
        let hostile = vec![
            "PASSWORD=--privileged".to_string(),
            "X=a b --volume /:/host".to_string(),
        ];
        let args = run_args(&TEST_IMAGE, TEST_REFERENCE, "n", &hostile);
        for pair in &hostile {
            let at = args.iter().position(|arg| arg == pair).unwrap();
            assert_eq!(args[at - 1], "--env", "{pair} was not preceded by --env");
        }
        assert!(!args.iter().any(|arg| arg == "--privileged"));
    }

    #[test]
    fn the_image_reference_is_the_last_argument_and_comes_from_the_allow_list() {
        let args = args();
        assert_eq!(args.last().unwrap(), TEST_REFERENCE);
    }

    #[test]
    fn an_unpinned_image_cannot_reach_a_daemon() {
        // The whole of the pending-entry design. `reference()` is the only
        // source of the string that becomes the last argument, and it refuses
        // for a `Pending` entry - so there is no path from an unpinned image
        // to a running container, and the refusal carries the tag whoever
        // pins it should resolve.
        const UNPINNED: Image = Image {
            key: "notyet",
            pin: Pin::Pending {
                resolve_from: "postgres:17-bookworm",
                blocked_by: "no registry access on the machine this was written on",
            },
            service_port: 5432,
            data_dir: "/var/lib/x",
            run_as: (999, 999),
            ready_probe: &["pg_isready"],
            download_bytes: 1_000_000,
            justification: "a fixture long enough to satisfy the justification check above",
        };
        assert!(!UNPINNED.is_pinned());
        let error = UNPINNED.reference().unwrap_err();
        assert!(matches!(error, ContainerError::ImageNotPinned { .. }));
        let message = error.to_string();
        assert!(message.contains("not measured"), "{message}");
        assert!(message.contains("postgres:17-bookworm"), "{message}");
        assert!(message.contains("no registry access"), "{message}");
    }

    #[test]
    fn a_pending_entry_names_a_tag_to_resolve_and_a_reason() {
        // A pending entry with no tag leaves whoever pins it guessing which
        // version the module was written against, and one with no reason is a
        // `TODO` with better formatting.
        for image in ALLOWED_IMAGES {
            if let Pin::Pending {
                resolve_from,
                blocked_by,
            } = image.pin
            {
                assert!(
                    resolve_from.contains(':'),
                    "{}: `{resolve_from}` is not a tagged reference",
                    image.key
                );
                assert!(
                    blocked_by.len() > 20,
                    "{}: the reason is too short to be one",
                    image.key
                );
            }
        }
    }

    #[test]
    fn every_allowed_image_is_pinned_by_digest() {
        // A tag is a mutable pointer. A tag-pinned benchmark measures whatever
        // the publisher pushed last, so two runs a month apart are not
        // comparable even though nothing in DARCBench changed - and a
        // compromised tag is a compromised benchmark (T-SUPPLY).
        //
        // This is what stopped the first entry from being `postgres:17`, and
        // it is what will stop the next one.
        for image in ALLOWED_IMAGES {
            if let Pin::Pinned(reference) = image.pin {
                assert!(
                    reference.contains("@sha256:"),
                    "{} is not pinned by digest: {reference}",
                    image.key
                );
                // No tag alongside the digest either: `repo:tag@sha256:...`
                // parses, and reads as though the tag meant something.
                assert!(
                    !reference
                        .split('@')
                        .next()
                        .unwrap_or_default()
                        .contains(':'),
                    "{} carries a tag as well as a digest: {reference}",
                    image.key
                );
            }
            assert!(
                image.justification.len() > 40,
                "{} has no real justification",
                image.key
            );
            assert!(image.data_dir.starts_with('/'), "{}", image.key);
        }
    }

    #[test]
    fn an_image_cannot_be_named_into_existence() {
        // There is no public constructor, so the only way to a launchable
        // image is the allow-list. A caller cannot assemble one from a string,
        // which is what keeps configuration, an HTTP request or a command line
        // from reaching "run this image".
        assert!(Image::from_allow_list("mariadb").is_none());
        assert!(Image::from_allow_list("postgres").is_some());
        assert!(Image::from_allow_list("../../etc").is_none());
        assert!(Image::from_allow_list("").is_none());
    }

    #[test]
    fn only_containers_this_agent_labelled_can_be_removed() {
        // `reap` removes whatever the filter matches, so the filter is the
        // safety property. A name prefix could collide with something an
        // operator chose; a label this agent sets could not have been set by
        // anything else.
        assert!(OWNER_FILTER.starts_with("label="));
        assert!(OWNER_FILTER.contains("com.getdarc.darcbench.owned"));
        assert_eq!(OWNER_FILTER, format!("label={OWNER_LABEL}"));
    }

    #[test]
    fn a_container_name_cannot_carry_anything_a_flag_parser_would_notice() {
        let name = container_name("run_../../etc/passwd; rm -rf /", "svc");
        assert_eq!(name, "darcbench-run_etcpasswdrm-rf-svc");
        assert!(!name.contains('/'));
        assert!(!name.contains(' '));
        assert!(name.starts_with("darcbench-"));

        // And a run id long enough to be an attack on the name itself.
        let long = container_name(&"a".repeat(500), "svc");
        assert!(long.len() < 80, "{}", long.len());
    }

    #[test]
    fn a_published_port_that_is_not_loopback_is_refused() {
        // Belt and braces against the `--publish` above: if a runtime ever
        // published elsewhere, connecting to it anyway would make the one
        // guarantee this module offers silently false. Exercised through the
        // parser rather than a daemon.
        let parse = |text: &str| -> Option<SocketAddr> {
            text.lines()
                .filter_map(|line| line.trim().rsplit_once(':'))
                .filter_map(|(host, port)| {
                    let host = host.trim_start_matches('[').trim_end_matches(']');
                    let ip: IpAddr = host.parse().ok()?;
                    let port: u16 = port.trim().parse().ok()?;
                    ip.is_loopback().then_some(SocketAddr::new(ip, port))
                })
                .next()
        };
        assert_eq!(
            parse("127.0.0.1:49154").map(|a| a.port()),
            Some(49154),
            "a loopback binding must be accepted"
        );
        assert_eq!(parse("0.0.0.0:49154"), None);
        assert_eq!(parse("10.0.0.5:49154"), None);
        assert_eq!(parse("[::]:49154"), None);
        assert_eq!(parse("[::1]:49154").map(|a| a.port()), Some(49154));
    }

    #[test]
    fn discovery_and_daemon_reachability_are_separate_failures() {
        // On this host `/usr/bin/docker` exists and is root-owned, and no
        // daemon answers it - which is how the distinction got noticed. The
        // test asserts on whichever case the machine running it presents,
        // because both are real and neither is a defect.
        match Runtime::discover() {
            Ok(runtime) => {
                assert!(runtime.name().starts_with('/'));
            }
            Err(ContainerError::NoRuntime { candidates, .. }) => {
                assert!(candidates.contains("docker"));
            }
            Err(ContainerError::RuntimeUnavailable { runtime, detail }) => {
                // The useful case: "you have Docker but its daemon is not
                // running" is a thing to start, and collapsing it into
                // "containers unavailable" would throw that away.
                assert!(runtime.contains("docker") || runtime.contains("podman"));
                assert!(!detail.is_empty(), "the reason must reach the operator");
            }
            Err(other) => panic!("unexpected discovery failure: {other}"),
        }
    }

    #[test]
    fn no_failure_to_get_a_container_offers_a_host_fallback() {
        // The one thing this module must never do. Asserted on the text an
        // operator reads, because that is where a fallback would first be
        // offered.
        let error = ContainerError::NoRuntime {
            candidates: RUNTIMES.join(", "),
            rejections: String::new(),
        };
        let message = error.to_string();
        assert!(message.contains("not measured"));
        assert!(
            message.contains("will not use one already on this machine"),
            "{message}"
        );
    }
}
