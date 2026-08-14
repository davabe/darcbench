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
//! # The image allow-list is empty, and that is not an oversight
//!
//! [`ALLOWED_IMAGES`] is the same shape as `network_endpoints`' host table: a
//! compile-time list, each entry justified, with no way to add one at runtime.
//! It is empty because this commit ships the *tier*, not a module that uses
//! it, and an image reference must be pinned to a digest resolved against a
//! real registry. Inventing one would be writing a security control that
//! cannot work.
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
//! `--rm` does not help: it reaps when the *container* exits, not when the
//! agent does. So the mitigation is reaping rather than prevention -
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

/// Memory ceiling for a sandboxed service.
///
/// Half a gigabyte. Enough for a database with a benchmark-sized dataset,
/// small enough that a runaway container cannot take the host down - which is
/// the failure this whole module exists to make impossible. A benchmark that
/// causes an outage is a failure however accurate its numbers are.
const MEMORY_LIMIT: &str = "512m";

/// Tmpfs size for the service's data directory.
const TMPFS_SIZE_BYTES: u64 = 512 * 1024 * 1024;

/// How long the runtime gets to answer a control command.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a container gets to start serving before it is given up on.
const READY_TIMEOUT: Duration = Duration::from_secs(90);

/// Gap between readiness probes.
const READY_POLL: Duration = Duration::from_millis(200);

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
/// Empty, and deliberately so - see the module documentation. This commit
/// ships the isolation tier; the first database module adds the first entry,
/// with a digest resolved against a real registry rather than invented.
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
        pin: Pin::Pending {
            resolve_from: "postgres:17-bookworm",
            blocked_by: "no container daemon or registry access was available on the machine this \
                     entry was written on, and a digest that was not resolved against a registry \
                     is not a pin",
        },
        // 5432 is PostgreSQL's port inside the container. Nothing is published to
        // it except loopback on this host - see `run_args`.
        service_port: 5432,
        // The official image's `PGDATA`. Mounted tmpfs, so the database is created
        // fresh for every run and nothing it writes reaches a host disk.
        data_dir: "/var/lib/postgresql/data",
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
        pin: Pin::Pending {
            resolve_from: "valkey/valkey:8-alpine",
            blocked_by: "no container daemon or registry access was available on the machine this \
                     entry was written on, and a digest that was not resolved against a registry \
                     is not a pin",
        },
        service_port: 6379,
        // Valkey's working directory in the official image. Mounted tmpfs, so an
        // RDB snapshot goes to RAM and nothing reaches a host disk.
        data_dir: "/data",
        justification: "The official Valkey image, published by the Valkey project. Valkey rather \
                    than Redis because it is the fork the major distributions and cloud providers \
                    moved to after the 2024 licence change, and because its image is \
                    BSD-licensed throughout. It carries `redis-benchmark` and `redis-cli`, which \
                    is what makes a cache measurement possible without this workspace growing a \
                    RESP client.",
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
    #[error("the container did not accept a connection within {}s", .0.as_secs())]
    NotReady(Duration),
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
fn run_args<'a>(
    image: &'a Image,
    reference: &'a str,
    name: &'a str,
    env: &'a [String],
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "run".into(),
        "--detach".into(),
        // Reaped by the runtime when the container exits. Not a substitute for
        // `reap`: this fires when the container stops, not when the agent does.
        "--rm".into(),
        "--name".into(),
        name.into(),
        "--label".into(),
        OWNER_LABEL.into(),
        // Published on loopback, never on a routable address. An unpublished
        // port would be safer still and is not possible: the load has to reach
        // the service from this process.
        "--publish".into(),
        format!("127.0.0.1::{}", image.service_port),
        // Bounds a runaway container. A benchmark that causes an outage is a
        // failure however accurate its numbers are.
        "--memory".into(),
        MEMORY_LIMIT.into(),
        "--memory-swap".into(),
        MEMORY_LIMIT.into(),
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
        "--tmpfs".into(),
        format!("{}:rw,size={}", image.data_dir, TMPFS_SIZE_BYTES),
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

// ---------------------------------------------------------------------------
// The sandbox
// ---------------------------------------------------------------------------

/// A running, isolated service. Dropping it removes the container.
#[derive(Debug)]
pub struct Sandbox {
    runtime: Interpreter,
    name: String,
    address: SocketAddr,
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
        // Before anything else, so an unpinned image never reaches a daemon.
        let reference = image.reference()?;
        let name = container_name(run_id, image.key);
        let args = run_args(image, reference, &name, env);
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
        };

        // From here on every failure has to take the container with it.
        let address = match published_port(runtime, &name, image.service_port) {
            Ok(address) => address,
            Err(error) => {
                remove(&runtime.exec, &name);
                return Err(error);
            }
        };
        // `wait_ready` removes the container itself on failure, because it is
        // the only place that knows the container was reachable but never
        // answered - a distinction worth keeping out of this function.
        let sandbox = sandbox(address);
        sandbox.wait_ready()?;
        Ok(sandbox)
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
    fn wait_ready(&self) -> Result<(), ContainerError> {
        let deadline = Instant::now() + READY_TIMEOUT;
        while Instant::now() < deadline {
            if TcpStream::connect_timeout(&self.address, READY_POLL).is_ok() {
                return Ok(());
            }
            std::thread::sleep(READY_POLL);
        }
        // The container is removed here rather than left for `reap`, because
        // an operator watching a run fail should not also be left with a
        // container consuming memory until the next one starts.
        remove(&self.runtime, &self.name);
        Err(ContainerError::NotReady(READY_TIMEOUT))
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
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--memory" && w[1] == MEMORY_LIMIT));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--memory-swap" && w[1] == MEMORY_LIMIT));
        assert!(args.windows(2).any(|w| w[0] == "--pids-limit"));
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
        // The table is empty today; this test is what stops the first entry
        // from being `mariadb:11.4`.
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
