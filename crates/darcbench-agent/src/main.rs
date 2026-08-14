//! The DARCBench agent binary.
//!
//! See `docs/ARCHITECTURE.md` for how the pieces fit together and
//! `docs/THREAT-MODEL.md` for why the server is shaped the way it is.

mod cli;
mod config;
mod external;
mod follow;
mod index;
mod preflight;
mod proxy;
mod runner;
mod server;
mod tui;
mod ui;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use darcbench_inventory::{Inventory, RedactionPolicy};
use darcbench_protocol::{Profile, RunState, ENDURANCE_MAX_MINUTES, ENDURANCE_MIN_MINUTES};
use darcbench_report::{validate_bundle, AgentKey};

use cli::{Cli, Command, ProxyCommand, Style};
use config::{AccessToken, AgentConfig};
use runner::{RunManager, AGENT_VERSION};

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(&cli);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("could not start the async runtime")?;

    let exit = runtime.block_on(dispatch(cli));
    match exit {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("darcbench: {error:#}");
            std::process::exit(1);
        }
    }
}

fn init_logging(cli: &Cli) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_new(&cli.log).unwrap_or_else(|_| EnvFilter::new("warn"));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        // Logs go to stderr so `--json` output on stdout stays a clean document.
        .with_writer(std::io::stderr);
    if cli.json {
        builder.json().init();
    } else {
        builder.with_ansi(!cli.no_color).init();
    }
}

async fn dispatch(mut cli: Cli) -> Result<i32> {
    let style = Style::new(
        !cli.no_color && !cli.json && std::io::IsTerminal::is_terminal(&std::io::stdout()),
    );
    let state_dir = cli.home.clone().unwrap_or_else(config::default_state_dir);
    let command = cli.command.take();

    match command {
        None => {
            // Bare `darcbench` is the friendly path: inspect, explain, and
            // point at the next command. It never starts a benchmark on its
            // own, because a bare invocation must be safe on a production box.
            print_welcome(&style, &state_dir);
            Ok(0)
        }
        Some(Command::Doctor) => doctor(&cli, &style, &state_dir),
        Some(Command::Inspect { include_sensitive }) => inspect(&cli, include_sensitive),
        Some(Command::Serve { port, bind, token }) => {
            serve(&cli, &style, state_dir, port, &bind, token).await
        }
        Some(Command::Run {
            profile,
            modules,
            force,
            duration_minutes,
            output,
        }) => {
            run(
                &cli,
                &style,
                state_dir,
                &profile,
                modules,
                force,
                duration_minutes,
                output,
            )
            .await
        }
        Some(Command::Proxy(action)) => match action {
            ProxyCommand::Preview {
                server,
                port,
                location,
            } => proxy::preview(&cli, &style, &server, port, &location),
            ProxyCommand::Apply {
                server,
                port,
                location,
                confirm,
            } => proxy::apply(&cli, &style, &state_dir, &server, port, &location, confirm),
            ProxyCommand::Rollback { confirm, force } => {
                proxy::rollback(&cli, &style, &state_dir, confirm, force)
            }
            ProxyCommand::Verify { server } => proxy::verify(&cli, &style, &server),
            ProxyCommand::Status => proxy::status(&cli, &style, &state_dir),
        },
        Some(Command::WebTarget { bind, tls, minutes }) => {
            external::web_target(&cli, &style, &state_dir, &bind, tls, minutes)
        }
        Some(Command::WebDrive {
            ticket,
            rate,
            seconds,
            connections,
        }) => external::web_drive(&cli, &style, &ticket, rate, seconds, connections),
        Some(Command::Status { limit }) => status(&cli, &style, &state_dir, limit),
        Some(Command::Compare {
            baseline,
            candidate,
        }) => compare(&cli, &style, &state_dir, &baseline, &candidate),
        Some(Command::Prune {
            older_than_days,
            keep_last,
            confirm,
        }) => prune(
            &cli,
            &style,
            &state_dir,
            crate::index::RetentionPolicy {
                older_than_days,
                keep_last,
            },
            confirm,
        ),
        Some(Command::Report { run_id, html }) => report(&cli, &state_dir, run_id, html),
        Some(Command::Verify { path }) => verify(&cli, &style, &path),
        Some(Command::Uninstall { confirm }) => uninstall(&cli, &style, &state_dir, confirm),
    }
}

/// The wordmark, split so the three parts can be coloured separately.
///
/// Identical treatment to the live dashboard's header (`crate::tui`): cyan
/// `DARC`, violet separator, bright `BENCH`. The product should not change
/// appearance depending on which of its own surfaces you are looking at.
fn brand(style: &Style) -> String {
    format!(
        "{}{}{}",
        style.cyan(&style.bold("DARC")),
        style.magenta("//"),
        style.bold("BENCH")
    )
}

/// A section heading. Dim and upper-case rather than a rule of dashes, which
/// costs a whole line to say less.
fn heading(style: &Style, text: &str) -> String {
    style.dim(&style.bold(text))
}

/// Terminal width, when there is a terminal to ask.
///
/// `None` when output is redirected, and callers must then leave text
/// unwrapped. Hard-wrapping into a pipe would put newlines in the middle of
/// sentences that somebody is about to `grep`, which is worse than a long line.
fn terminal_width(style: &Style) -> Option<usize> {
    if !style.is_enabled() {
        return None;
    }
    ratatui::crossterm::terminal::size()
        .ok()
        .map(|(width, _)| usize::from(width))
}

/// Word-wraps `text` to `width` columns, hanging-indented by `indent`.
///
/// Preflight findings are written as prose and several run past two hundred
/// characters - the storage-wear disclosure is a small paragraph. Printed flat
/// they wrap at the terminal edge back to column zero, so the continuation sits
/// under the severity badge and the message stops looking like one field.
///
/// Words longer than the available width are emitted on their own line rather
/// than split: the long tokens here are paths and identifiers, and a path
/// broken across a line boundary cannot be copied.
fn wrap_indented(text: &str, width: usize, indent: usize) -> Vec<String> {
    let available = width.saturating_sub(indent).max(20);
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
        } else if current.chars().count() + 1 + word.chars().count() <= available {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current));
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn print_welcome(style: &Style, state_dir: &std::path::Path) {
    println!();
    println!("  {}", brand(style));
    println!(
        "  {}",
        style.dim("Deployment · Application · Runtime · Compute")
    );
    println!();

    // One label width for every key/value line on the screen, so the values
    // form a single column the eye can run down.
    const LABEL: usize = 16;
    for (label, value) in [
        ("agent", AGENT_VERSION.to_string()),
        ("state", state_dir.display().to_string()),
        (
            "scoring model",
            darcbench_scoring::SCORING_MODEL_VERSION.to_string(),
        ),
    ] {
        println!("  {}{value}", cli::pad(&style.dim(label), label, LABEL));
    }
    println!(
        "  {}{}",
        cli::pad(&style.dim(""), "", LABEL),
        style.yellow("uncalibrated - development output")
    );
    println!();

    println!("  {}", heading(style, "COMMANDS"));
    // The command column is padded to a fixed width so the descriptions line
    // up; `darcbench run --profile quick` is the longest and sets it.
    const COMMAND: usize = 32;
    for (command, description) in [
        ("darcbench doctor", "check this machine is ready"),
        ("darcbench inspect", "print the system inventory"),
        ("darcbench serve", "open the browser dashboard"),
        (
            "darcbench run --profile quick",
            "benchmark from the terminal",
        ),
    ] {
        println!(
            "    {}{}",
            cli::pad(&style.cyan(command), command, COMMAND),
            style.dim(description)
        );
    }
    println!();
    println!(
        "  {}",
        style.dim("A bare `darcbench` never starts a benchmark. Pick a command above.")
    );
    println!();
}

/// Bytes in the largest unit that keeps the number readable.
///
/// An operator deciding whether to let a benchmark write to their production
/// disk should not have to count digits in `2147483648`.
fn human_bytes(bytes: u64) -> String {
    const UNITS: [(&str, u64); 3] = [("GiB", 1 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)];
    for (label, scale) in UNITS {
        if bytes >= scale {
            return format!("{:.1} {label}", bytes as f64 / scale as f64);
        }
    }
    format!("{bytes} bytes")
}

fn doctor(cli: &Cli, style: &Style, state_dir: &std::path::Path) -> Result<i32> {
    let inventory = Inventory::collect();
    let registry = darcbench_modules::Registry::builtin();
    let modules = registry.modules_for_profile(Profile::Quick);
    let params = darcbench_modules::ModuleParams::for_profile(Profile::Quick);

    let result = preflight::run(&preflight::PreflightInput {
        inventory: &inventory,
        registry: &registry,
        modules: &modules,
        profile: Profile::Quick,
        params: &params,
        state_dir,
        force: false,
        cycle_target: Profile::Quick
            .cycle_target_minutes()
            .map(|m| std::time::Duration::from_secs(u64::from(m) * 60)),
    });

    if cli.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "agent_version": AGENT_VERSION,
                "state_dir": state_dir,
                "ui_bundled": ui::has_bundled_ui(),
                "scoring_model": darcbench_scoring::SCORING_MODEL_VERSION,
                "scoring_calibrated": false,
                "preflight": result,
                "gaps": inventory.gaps,
            }))?
        );
        return Ok(if result.passed { 0 } else { 2 });
    }

    // One label width across all three sections, so every value on the screen
    // lands in the same column regardless of which group it belongs to.
    const LABEL: usize = 17;
    let row = |label: &str, value: &str| {
        println!("  {}{value}", cli::pad(&style.dim(label), label, LABEL));
    };

    println!();
    println!("  {}  {}", brand(style), style.dim("doctor"));
    println!();

    println!("  {}", heading(style, "AGENT"));
    row("version", AGENT_VERSION);
    row("state directory", &state_dir.display().to_string());
    row(
        "bundled web UI",
        if ui::has_bundled_ui() {
            "yes"
        } else {
            "no (built-in console will be served)"
        },
    );
    row(
        "scoring model",
        &format!(
            "{} {}",
            darcbench_scoring::SCORING_MODEL_VERSION,
            style.yellow("uncalibrated")
        ),
    );
    println!();

    println!("  {}", heading(style, "MACHINE"));
    row("scope", &format!("{:?}", inventory.platform.scope));
    row("cpu", inventory.cpu.model.as_deref().unwrap_or("unknown"));
    row("logical cpus", &inventory.cpu.logical_cpus.to_string());
    row(
        "memory",
        &format!(
            "{:.1} GiB",
            inventory.memory.total_bytes as f64 / 1024.0_f64.powi(3)
        ),
    );
    println!();

    println!(
        "  {}  {}",
        heading(style, "A QUICK RUN WOULD COST"),
        style.dim("(this is an estimate, nothing has run)")
    );
    // Risk is the field an operator decides on, so it carries the only colour
    // in this group and is graded rather than merely printed.
    let risk = format!("{:?}", result.risk);
    let risk_painted = match result.risk {
        darcbench_protocol::events::RiskClass::Safe => style.green(&risk),
        darcbench_protocol::events::RiskClass::ModerateLoad => style.cyan(&risk),
        darcbench_protocol::events::RiskClass::HeavyLoad => style.yellow(&risk),
        _ => style.red(&risk),
    };
    row("risk class", &risk_painted);
    row("time", &format!("~{} s", result.estimated_duration_s));
    // Space and wear are different costs and both belong on this screen: the
    // first is what has to be free, the second is flash endurance spent
    // permanently. `docs/BENCHMARK-METHODOLOGY.md` requires both before a
    // storage run.
    row("disk space", &human_bytes(result.estimated_bytes_written));
    if result.estimated_write_volume_bytes > 0 {
        row(
            "disk writes",
            &format!(
                "{} in total {}",
                human_bytes(result.estimated_write_volume_bytes),
                style.dim("(flash endurance)")
            ),
        );
    }

    if !result.findings.is_empty() {
        println!();
        println!("  {}", heading(style, "FINDINGS"));
        for finding in &result.findings {
            let (label, colour): (&str, fn(&Style, &str) -> String) = match finding.severity {
                darcbench_protocol::events::Severity::Error => ("ERROR", |s, t| s.red(t)),
                darcbench_protocol::events::Severity::Warning => ("WARN", |s, t| s.yellow(t)),
                darcbench_protocol::events::Severity::Info => ("INFO", |s, t| s.dim(t)),
            };
            // A blocking finding is the reason the whole command exits 2, so it
            // says so on its own line rather than leaving the reader to infer
            // it from the verdict at the bottom.
            let blocking = if finding.blocking {
                format!(" {}", style.red("[blocking]"))
            } else {
                String::new()
            };
            // 4 leading spaces + the two padded columns + their separators.
            const MESSAGE_COLUMN: usize = 4 + 5 + 1 + 22 + 1;
            let body = match terminal_width(style) {
                Some(width) => wrap_indented(&finding.message, width, MESSAGE_COLUMN),
                None => vec![finding.message.clone()],
            };
            let mut lines = body.iter();
            let first = lines.next().map(String::as_str).unwrap_or_default();
            println!(
                "    {} {} {first}{blocking}",
                cli::pad(&colour(style, label), label, 5),
                cli::pad(&style.dim(&finding.check), &finding.check, 22),
            );
            for line in lines {
                println!("{}{line}", " ".repeat(MESSAGE_COLUMN));
            }
        }
    }

    if !inventory.gaps.is_empty() {
        println!();
        println!("  {}", heading(style, "NOT EXPOSED BY THIS PLATFORM"));
        for gap in &inventory.gaps {
            println!(
                "    {} {}",
                cli::pad(&style.dim(&gap.field), &gap.field, 22),
                style.dim(&gap.reason)
            );
        }
    }

    println!();
    if result.passed {
        println!(
            "  {} ready to benchmark",
            style.green(&style.invert(" PASS "))
        );
        println!();
        Ok(0)
    } else {
        println!(
            "  {} preflight would refuse to start; see the findings above",
            style.red(&style.invert(" BLOCKED "))
        );
        println!();
        Ok(2)
    }
}

fn inspect(cli: &Cli, include_sensitive: bool) -> Result<i32> {
    let inventory = Inventory::collect();
    let policy = if include_sensitive {
        RedactionPolicy::Reveal
    } else {
        RedactionPolicy::Redact
    };
    let value =
        darcbench_inventory::redact::with_policy(policy, || serde_json::to_value(&inventory))?;
    let payload = serde_json::json!({
        "inventory": value,
        "redacted": !include_sensitive,
        "performance_digest": inventory.performance_digest(),
    });
    if cli.json {
        println!("{}", serde_json::to_string(&payload)?);
    } else {
        println!("{}", serde_json::to_string_pretty(&payload)?);
    }
    Ok(0)
}

async fn serve(
    cli: &Cli,
    style: &Style,
    state_dir: std::path::PathBuf,
    port: u16,
    bind: &str,
    token: Option<String>,
) -> Result<i32> {
    let port = config::validate_port(port).map_err(|e| anyhow::anyhow!(e))?;
    let ip: IpAddr = bind
        .parse()
        .with_context(|| format!("`{bind}` is not an IP address"))?;
    let address = SocketAddr::new(ip, port);

    // Refuse to bind a port something else is already using, rather than
    // racing it. On a hosting server the incumbent is somebody's website.
    let inventory = Inventory::collect();
    if inventory.software.port_is_occupied(port) {
        anyhow::bail!(
            "port {port} already has a listener. DARCBench never displaces an existing \
             service; choose another port with --port."
        );
    }

    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("could not create {}", state_dir.display()))?;
    let key = Arc::new(
        AgentKey::load_or_create(&state_dir.join("agent.key"))
            .context("could not load or create the agent signing key")?,
    );

    let access_token = match token {
        Some(value) if value.len() >= 32 => AccessToken::from_string(value),
        Some(_) => anyhow::bail!("--token must be at least 32 characters"),
        None => AccessToken::generate().context("could not generate an access token")?,
    };

    let config = AgentConfig {
        bind: address,
        state_dir: state_dir.clone(),
        token: access_token.clone(),
        non_loopback_requested: !ip.is_loopback(),
    };
    tracing::info!(
        state_dir = %config.state_dir.display(),
        non_loopback = config.non_loopback_requested,
        "agent configured"
    );

    let manager = Arc::new(RunManager::new(state_dir, key));
    // Once, at startup, before anything can ask for a run list. This is what
    // lets a fresh `serve` show the runs a previous process executed, and what
    // drops rows for directories an operator has deleted by hand.
    manager.reconcile_index();
    let app = server::router(server::AppState {
        manager,
        token: access_token,
        loopback_only: config.is_loopback(),
    });

    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("could not bind {address}"))?;

    if cli.json {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "listening": address.to_string(),
                "url": config.dashboard_url(),
                "loopback_only": config.is_loopback(),
                "ui_bundled": ui::has_bundled_ui(),
            }))?
        );
    } else {
        println!("{}", style.bold("DARC//BENCH agent"));
        println!();
        println!("  Open: {}", style.cyan(&config.dashboard_url()));
        println!();
        if config.is_loopback() {
            println!(
                "  {}",
                style.dim("Bound to loopback. From another machine, forward the port:")
            );
            println!(
                "  {}",
                style.dim(&format!(
                    "    ssh -N -L {port}:127.0.0.1:{port} user@this-server"
                ))
            );
        } else {
            println!(
                "  {} bound to {ip}, which is reachable beyond this machine.",
                style.yellow("WARNING:")
            );
            println!(
                "  {}",
                style.yellow(
                    "  The token is the only thing protecting it and it travels in clear over \
                     plain HTTP. Put it behind TLS, or prefer an SSH tunnel to loopback."
                )
            );
        }
        println!();
        println!(
            "  {}",
            style.dim("Ctrl-C to stop. No benchmark starts until you ask for one.")
        );
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("the dashboard server stopped unexpectedly")?;

    if !cli.json {
        println!(
            "\n{}",
            style.dim("Agent stopped. Temporary state cleaned up.")
        );
    }
    Ok(0)
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

#[allow(clippy::too_many_arguments)]
async fn run(
    cli: &Cli,
    style: &Style,
    state_dir: std::path::PathBuf,
    profile: &str,
    modules: Option<Vec<String>>,
    force: bool,
    duration_minutes: Option<u32>,
    output: Option<std::path::PathBuf>,
) -> Result<i32> {
    let profile: Profile = profile.parse().map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut module_ids = match modules {
        Some(names) => {
            let mut parsed = Vec::with_capacity(names.len());
            for name in names {
                parsed.push(
                    name.parse::<darcbench_protocol::ModuleId>()
                        .map_err(|e| anyhow::anyhow!("{e}"))?,
                );
            }
            Some(parsed)
        }
        None => None,
    };

    let duration = match duration_minutes {
        Some(minutes) => {
            // Only a profile that cycles by nature has a duration to override.
            // Otherwise this flag is a general-purpose "hold this machine at
            // full load for N hours" switch attached to any workload, which is
            // what bounding the number was supposed to prevent.
            anyhow::ensure!(
                profile.cycle_target_minutes().is_some(),
                "`{profile}` runs its module set once, so --duration-minutes has no meaning for \
                 it. Today `endurance` is the only cycling profile."
            );
            anyhow::ensure!(
                (ENDURANCE_MIN_MINUTES..=ENDURANCE_MAX_MINUTES).contains(&minutes),
                "--duration-minutes must be between {ENDURANCE_MIN_MINUTES} and \
                 {ENDURANCE_MAX_MINUTES}"
            );
            // The module set still comes from the requested profile, so a
            // shorter endurance run is a shorter *endurance* run rather than
            // the custom module set.
            if module_ids.is_none() {
                module_ids =
                    Some(darcbench_modules::Registry::builtin().modules_for_profile(profile));
            }
            Some(std::time::Duration::from_secs(u64::from(minutes) * 60))
        }
        None => None,
    };

    // Either override makes the result incomparable, for the same reason: a
    // hand-picked module set did not run the profile's workload, and a shorter
    // endurance run was not given the profile's time to decline.
    let profile = if module_ids.is_some() {
        Profile::Custom
    } else {
        profile
    };

    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("could not create {}", state_dir.display()))?;
    let key = Arc::new(AgentKey::load_or_create(&state_dir.join("agent.key"))?);
    let manager = Arc::new(RunManager::new(state_dir.clone(), key));
    manager.reconcile_index();

    let dashboard = tui::should_render(style.is_enabled(), cli.no_tui);

    // No banner is printed here. Each of the three paths below already
    // introduces the run: the dashboard draws its own header, `follow::plain`
    // prints one from `run.created` - where it can also report the module count
    // and agent version, which are not known until the run exists - and `--json`
    // must emit nothing but the bundle. Printing one here as well put the
    // banner on screen twice for every plain-output run.
    //
    // `start` rather than `run_to_completion`: the run has to be observable
    // while it happens. `run_to_completion` is start-then-await with nothing in
    // between, which is exactly the silence this replaces.
    let handle = manager
        .start(profile, module_ids, force, duration)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    if dashboard {
        tui::drive(handle.clone()).await?;
    } else if !cli.json {
        // Not a terminal, or decoration is off: a CI log still deserves to know
        // which module is running rather than watching a blank pane for an
        // hour.
        follow::plain(handle.clone(), style).await;
    }

    // Both observers above return on the terminal *event*, which the run emits
    // before it finishes finalising - the bundle is signed and written after
    // it, and `RunState` reaches `Completed` after that. Reading `state()` or
    // `bundle()` straight away is therefore a race, and it is one that loses
    // silently in the worst possible way: `handle.state()` reads `Finalizing`,
    // which is not `Completed`, so a perfectly good run exits 1 and a caller
    // that checks the exit code sees a failure. It was observed printing
    // `state Finalizing` before this wait was added.
    //
    // Waiting on the state is what `run_to_completion` always did; the
    // observers replaced how the run is watched, not when it is finished.
    handle.wait_for_terminal().await;

    let bundle = handle
        .bundle()
        .ok_or_else(|| anyhow::anyhow!("the run finished without producing a bundle"))?;

    if let Some(path) = output {
        std::fs::write(&path, serde_json::to_vec_pretty(&bundle)?)
            .with_context(|| format!("could not write {}", path.display()))?;
    }

    if cli.json {
        println!("{}", serde_json::to_string(&bundle)?);
    } else {
        println!();
        println!("  run          {}", handle.id);
        println!("  state        {:?}", handle.state());
        println!("  verdict      {:?}", bundle.verdict.state);
        for (key, value) in &bundle.scores.facets {
            println!("  {key:<12} {value:.0}");
        }
        for category in &bundle.scores.categories {
            println!("  {:<12} {:.0}", category.key.key(), category.score);
        }
        println!(
            "  {:<12} {}",
            "total",
            bundle
                .scores
                .total
                .map(|v| format!("{v:.0}"))
                .unwrap_or_else(|| "n/a".into())
        );
        println!();
        println!(
            "  {}",
            style.dim(&format!(
                "Artifacts: {}/runs/{}/",
                state_dir.display(),
                handle.id
            ))
        );
    }

    Ok(match handle.state() {
        RunState::Completed => 0,
        RunState::Cancelled => 130,
        _ => 1,
    })
}

/// Opens the run index and brings it into agreement with the bundles on disk.
///
/// Every read-only command does this rather than trusting whatever the index
/// happened to contain: an operator may have run a benchmark from a different
/// process, deleted a directory by hand, or be using a machine where the index
/// was never written. Reconciling first costs one directory listing and makes
/// the answer describe the disk rather than the database.
fn open_reconciled_index(
    style: &Style,
    state_dir: &std::path::Path,
) -> (crate::index::RunIndex, bool) {
    let index = match crate::index::RunIndex::open(state_dir.join(crate::index::INDEX_FILE)) {
        Ok(index) => index,
        Err(error) => {
            // Printed, not swallowed. `FutureSchema`'s whole message is written
            // for an operator to read and act on, and a silent fallback to an
            // in-memory index means they never learn the on-disk one is
            // stranded and no longer being maintained.
            eprintln!("{}", style.yellow(&error.to_string()));
            crate::index::RunIndex::in_memory()
                .unwrap_or_else(|_| crate::index::RunIndex::unavailable())
        }
    };
    let mut reconciled = true;
    match index.reconcile(&state_dir.join("runs")) {
        Ok(outcome) => {
            if !outcome.unreadable.is_empty() {
                // Named because a command that acts on the index - `prune`
                // above all - is acting on a set that does not include these,
                // and "removed 99 runs" reads as complete when 400 were
                // invisible to the policy.
                eprintln!(
                    "{}",
                    style.yellow(&format!(
                        "{} run(s) on disk could not be read by this build and are not in the                          index: {}",
                        outcome.unreadable.len(),
                        outcome.unreadable.join(", ")
                    ))
                );
            }
        }
        Err(error) => {
            reconciled = false;
            eprintln!(
                "{}",
                style.yellow(&format!(
                    "the run index could not be brought up to date ({error}); what follows                      describes the index as it stands, which may be behind the bundles on disk"
                ))
            );
        }
    }
    (index, reconciled)
}

fn status(cli: &Cli, style: &Style, state_dir: &std::path::Path, limit: usize) -> Result<i32> {
    // Read from the index rather than by parsing every bundle on disk. The scan
    // it replaces opened a complete inventory, every metric and every
    // per-repetition sample to read four fields, once per run.
    let (index, _) = open_reconciled_index(style, state_dir);
    let runs = index.list(limit).unwrap_or_default();

    if cli.json {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "runs": runs.iter().map(|run| serde_json::json!({
                    "run_id": run.run_id,
                    "profile": run.profile,
                    "finished_at": run.finished_at,
                    "duration_ms": run.duration_ms,
                    "total_score": run.total_score,
                    "total_is_standard": run.total_is_standard,
                    "result_state": run.result_state,
                    "scoring_model": run.scoring_model,
                    "environment_digest": run.environment_digest,
                    "bundle_digest": run.bundle_digest,
                    "modules": run.modules,
                })).collect::<Vec<_>>()
            }))?
        );
        return Ok(0);
    }

    if runs.is_empty() {
        println!(
            "{}",
            style.dim("No completed runs in this state directory.")
        );
        return Ok(0);
    }
    // The bar is scaled against the best total on screen, not against an
    // absolute ceiling. Scores are uncalibrated development output with no
    // meaningful maximum, so a bar drawn against a made-up 1000 would imply a
    // scale the model does not have. Relative to the listed runs it says only
    // what it can: how these compare with each other.
    let best = runs
        .iter()
        .filter_map(|run| run.total_score)
        .filter(|score| score.is_finite() && *score > 0.0)
        .fold(0.0f64, f64::max);

    println!(
        "{}",
        style.dim(&format!(
            "{:<38} {:<26} {:>8}  {:<12} {}",
            "RUN", "FINISHED", "TOTAL", "STATE", ""
        ))
    );
    for run in &runs {
        let state = format!("{:?}", run.result_state);
        let coloured_state = match run.result_state {
            darcbench_protocol::ResultState::Invalid => style.red(&state),
            darcbench_protocol::ResultState::Local
            | darcbench_protocol::ResultState::SelfReported => style.dim(&state),
            _ => style.green(&state),
        };
        let padded_state = cli::pad(&coloured_state, &state, 12);

        let bar = match run.total_score {
            Some(score) if best > 0.0 && score.is_finite() && score > 0.0 => {
                let filled = ((score / best) * 16.0).round().clamp(0.0, 16.0) as usize;
                style.cyan(&"\u{2588}".repeat(filled))
            }
            _ => String::new(),
        };

        println!(
            "{:<38} {:<26} {:>8}  {padded_state} {bar}",
            run.run_id,
            run.finished_at.to_rfc3339(),
            run.total_score
                .map(|v| format!("{v:.0}"))
                .unwrap_or_else(|| "-".into()),
        );
    }
    Ok(0)
}

/// `darcbench compare <baseline> <candidate>`.
fn compare(
    cli: &Cli,
    style: &Style,
    state_dir: &std::path::Path,
    baseline: &str,
    candidate: &str,
) -> Result<i32> {
    // Validated before they reach the index, so a malformed argument is a clear
    // error rather than an empty result that looks like "no such run".
    for id in [baseline, candidate] {
        if id.parse::<darcbench_protocol::RunId>().is_err() {
            eprintln!("{}", style.red(&format!("`{id}` is not a run id")));
            return Ok(2);
        }
    }

    let (index, _) = open_reconciled_index(style, state_dir);
    let Some(comparison) = index.compare(baseline, candidate)? else {
        eprintln!(
            "{}",
            style.red("one or both runs are not in this state directory")
        );
        return Ok(2);
    };

    if cli.json {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "baseline": comparison.baseline.run_id,
                "candidate": comparison.candidate.run_id,
                "comparable": comparison.comparable,
                "incomparable_reasons": comparison.incomparable_reasons,
                "total": {
                    "baseline": comparison.baseline.total_score,
                    "candidate": comparison.candidate.total_score,
                },
                "metrics": comparison.metrics.iter().map(|d| serde_json::json!({
                    "module": d.module,
                    "metric_key": d.metric_key,
                    "unit": d.unit,
                    "baseline": d.baseline,
                    "candidate": d.candidate,
                    "ratio": d.ratio,
                })).collect::<Vec<_>>(),
                "unmatched": comparison.unmatched,
            }))?
        );
        return Ok(0);
    }

    println!();
    println!("  {}  {}", brand(style), style.dim("compare"));
    println!();
    println!(
        "    {} {}",
        style.dim("baseline "),
        comparison.baseline.run_id
    );
    println!(
        "    {} {}",
        style.dim("candidate"),
        comparison.candidate.run_id
    );
    println!();

    // Comparability first, and as a badge, because it governs whether anything
    // below may be quoted. Two runs from different machines still produce a
    // full table of tidy percentages, and the only thing standing between that
    // table and a false claim is this line being read.
    if comparison.comparable {
        println!(
            "  {} {}",
            style.green(&style.invert(" COMPARABLE ")),
            style.dim("same machine, profile, scoring model and agent build")
        );
    } else {
        println!(
            "  {} {}",
            style.yellow(&style.invert(" NOT COMPARABLE ")),
            style.dim("the differences below are not attributable to one cause")
        );
        for reason in &comparison.incomparable_reasons {
            println!("    {} {reason}", style.yellow("·"));
        }
    }

    // The totals, which the JSON output has always carried and this one did
    // not. It is the first number anyone wants from a comparison.
    let total = |score: Option<f64>| {
        score
            .map(|value| format!("{value:.0}"))
            .unwrap_or_else(|| "n/a".into())
    };
    println!();
    println!(
        "  {} {} {} {}",
        style.dim("total"),
        total(comparison.baseline.total_score),
        style.dim("->"),
        style.bold(&total(comparison.candidate.total_score))
    );

    println!();
    // Column widths are measured from the rows about to be printed, not fixed.
    // A `{:<34}` name column does not truncate - it just stops padding - so a
    // metric whose key runs past the width pushed its own value columns right
    // and the table stopped being a table. `memory.bandwidth/latency_random.
    // single` is 38 characters and did exactly that.
    let rows: Vec<(String, String, String)> = comparison
        .metrics
        .iter()
        .map(|delta| {
            (
                format!("{}/{}", delta.module, delta.metric_key),
                format!("{:.3}", delta.baseline),
                format!("{:.3}", delta.candidate),
            )
        })
        .collect();
    let width = |header: &str, pick: fn(&(String, String, String)) -> &String| {
        rows.iter()
            .map(|row| pick(row).chars().count())
            .chain(std::iter::once(header.chars().count()))
            .max()
            .unwrap_or(0)
    };
    let name_width = width("METRIC", |row| &row.0);
    let base_width = width("BASELINE", |row| &row.1);
    let cand_width = width("CANDIDATE", |row| &row.2);

    println!(
        "  {}",
        style.dim(&format!(
            "{:<name_width$} {:>base_width$} {:>cand_width$} {:>10}  UNIT",
            "METRIC", "BASELINE", "CANDIDATE", "CHANGE"
        ))
    );

    for (delta, (name, baseline, candidate)) in comparison.metrics.iter().zip(&rows) {
        // Rendered as a percentage change rather than a bare ratio because
        // "+12%" is read correctly by everyone and "1.12" is not.
        //
        // `ratio` is direction-adjusted upstream - above 1.0 always means
        // better, including for a lower-is-better metric like latency - so
        // green for positive is correct here and would not be if this did its
        // own arithmetic on the raw values. See `index::MetricDelta::ratio`.
        let change = (delta.ratio - 1.0) * 100.0;
        let text = format!("{change:+.1}%");
        // Sign only. This says which way the number moved, not that the move
        // is larger than the run-to-run noise: a comparison of two runs has no
        // confidence interval to make that claim from, and colouring one in
        // would be inventing a significance test.
        let painted = if change > 0.05 {
            style.green(&text)
        } else if change < -0.05 {
            style.red(&text)
        } else {
            style.dim(&text)
        };

        // Right-aligned by hand: the painted string carries escape bytes that
        // `{:>10}` would count as visible width.
        let indent = " ".repeat(10usize.saturating_sub(text.chars().count()));
        println!(
            "  {name:<name_width$} {baseline:>base_width$} {candidate:>cand_width$} \
             {indent}{painted}  {}",
            style.dim(&delta.unit),
        );
    }

    if !comparison.unmatched.is_empty() {
        println!();
        println!("  {}", heading(style, "NOT COMPARED"));
        for entry in &comparison.unmatched {
            println!("    {} {}", style.dim("·"), style.dim(entry));
        }
    }
    println!();
    Ok(0)
}

/// `darcbench prune`.
fn prune(
    cli: &Cli,
    style: &Style,
    state_dir: &std::path::Path,
    policy: crate::index::RetentionPolicy,
    confirm: bool,
) -> Result<i32> {
    if policy.is_empty() {
        eprintln!(
            "{}",
            style.red(
                "no retention policy given. Pass --older-than-days and/or --keep-last; a prune \
                 with no policy deletes nothing rather than everything."
            )
        );
        return Ok(2);
    }
    if policy.selects_everything() {
        eprintln!(
            "{}",
            style.red(
                "a policy of 0 selects every run. If that is really the intent, remove the runs \
                 directory deliberately; it is not something a mistyped flag should do."
            )
        );
        return Ok(2);
    }

    let (index, reconciled) = open_reconciled_index(style, state_dir);
    if confirm && !reconciled {
        // Refused rather than approximated. A `--keep-last` applied to a stale
        // index deletes by position in a list that does not describe the disk.
        eprintln!(
            "{}",
            style.red(
                "refusing to delete: the index could not be brought up to date, so the policy \
                 would be applied to a list that may not describe what is on disk. Re-run once \
                 the agent holding the index has finished."
            )
        );
        return Ok(2);
    }
    let runs_dir = state_dir.join("runs");
    let outcome = index.prune(&runs_dir, policy, !confirm)?;

    if cli.json {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "applied": confirm,
                "removed": outcome.removed,
                "retained_as_evidence": outcome.retained_as_evidence,
                "failed": outcome.failed.iter().map(|(run_id, error)| serde_json::json!({
                    "run_id": run_id, "error": error,
                })).collect::<Vec<_>>(),
                "bytes_freed": outcome.bytes_freed,
            }))?
        );
        return Ok(0);
    }

    let verb = if confirm { "Removed" } else { "Would remove" };
    println!(
        "{} {} run(s), freeing {:.1} MiB.",
        verb,
        outcome.removed.len(),
        outcome.bytes_freed as f64 / (1024.0 * 1024.0),
    );
    for run_id in &outcome.removed {
        println!("  {run_id}");
    }
    if !outcome.retained_as_evidence.is_empty() {
        println!();
        println!(
            "{}",
            style.dim(
                "Kept despite the policy, because an invalid result is evidence of why a run \
                 failed:"
            )
        );
        for run_id in &outcome.retained_as_evidence {
            println!("  {run_id}");
        }
    }
    if !outcome.failed.is_empty() {
        println!();
        eprintln!(
            "{}",
            style.yellow(&format!(
                "{} run(s) selected by the policy could not be removed:",
                outcome.failed.len()
            ))
        );
        for (run_id, error) in &outcome.failed {
            eprintln!("  {run_id}: {error}");
        }
    }
    if !confirm && !outcome.removed.is_empty() {
        println!();
        println!("{}", style.dim("Re-run with --confirm to apply."));
    }
    // Non-zero when the policy did not fully apply: a script that prunes on a
    // schedule has to be able to tell.
    Ok(if outcome.failed.is_empty() { 0 } else { 1 })
}

fn report(
    cli: &Cli,
    state_dir: &std::path::Path,
    run_id: Option<String>,
    html: bool,
) -> Result<i32> {
    let bundle = load_bundle(state_dir, run_id)?;
    if html {
        println!("{}", darcbench_report::html::render(&bundle));
    } else if cli.json {
        println!("{}", serde_json::to_string(&bundle)?);
    } else {
        println!("{}", serde_json::to_string_pretty(&bundle)?);
    }
    Ok(0)
}

fn load_bundle(
    state_dir: &std::path::Path,
    run_id: Option<String>,
) -> Result<darcbench_report::Bundle> {
    let runs_dir = state_dir.join("runs");
    let path = match run_id {
        Some(raw) => {
            // Parse before touching the filesystem: this is the only place a
            // caller-supplied string reaches a path join.
            let id: darcbench_protocol::RunId = raw.parse().map_err(|e| anyhow::anyhow!("{e}"))?;
            config::StatePath::join(&runs_dir, &[id.as_str(), "bundle.json"])
                .map_err(|e| anyhow::anyhow!("{e}"))?
                .as_path()
                .to_path_buf()
        }
        None => {
            let mut candidates: Vec<(std::path::PathBuf, std::time::SystemTime)> = Vec::new();
            if let Ok(dir) = std::fs::read_dir(&runs_dir) {
                for entry in dir.filter_map(Result::ok) {
                    let candidate = entry.path().join("bundle.json");
                    if let Ok(meta) = std::fs::metadata(&candidate) {
                        if let Ok(modified) = meta.modified() {
                            candidates.push((candidate, modified));
                        }
                    }
                }
            }
            candidates.sort_by_key(|(_, at)| *at);
            candidates
                .pop()
                .map(|(path, _)| path)
                .ok_or_else(|| anyhow::anyhow!("no runs found in {}", runs_dir.display()))?
        }
    };

    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("could not read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("{} is not a valid bundle", path.display()))
}

fn verify(cli: &Cli, style: &Style, path: &std::path::Path) -> Result<i32> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("could not read {}", path.display()))?;
    let bundle: darcbench_report::Bundle = serde_json::from_str(&raw)
        .with_context(|| format!("{} is not a valid bundle", path.display()))?;

    let signature_ok = bundle.verify_signature().is_ok();
    // `server_side = true` runs the strict path: signature required and every
    // score recomputed from the raw metrics.
    let outcome = validate_bundle(&bundle, true);

    if cli.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "path": path,
                "signature_valid": signature_ok,
                "scores_recomputed_match": outcome.recomputation_matched,
                "score_mismatch_field": outcome.mismatch_field,
                "verdict": outcome.verdict,
                "digest": bundle.digest().ok(),
            }))?
        );
    } else {
        println!("{}", style.bold(&format!("Verifying {}", path.display())));
        println!(
            "  signature        {}",
            if signature_ok {
                style.green("valid")
            } else {
                style.red("INVALID")
            }
        );
        println!(
            "  score recompute  {}",
            match (outcome.recomputation_matched, &outcome.mismatch_field) {
                (Some(true), _) => style.green("matches raw metrics"),
                (Some(false), Some(field)) => style.red(&format!(
                    "MISMATCH in `{field}` - scores do not follow from the measurements"
                )),
                (Some(false), None) => {
                    style.red("MISMATCH - scores do not follow from the measurements")
                }
                (None, _) =>
                    style.red("IMPOSSIBLE - this build cannot recompute that scoring model"),
            }
        );
        println!("  verdict          {:?}", outcome.verdict.state);
        for reason in &outcome.verdict.reasons {
            println!("    - {reason:?}");
        }
    }

    Ok(
        if signature_ok && outcome.verdict.state != darcbench_protocol::ResultState::Invalid {
            0
        } else {
            3
        },
    )
}

fn uninstall(cli: &Cli, style: &Style, state_dir: &std::path::Path, confirm: bool) -> Result<i32> {
    let targets = [state_dir.to_path_buf()];
    if cli.json {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "would_remove": targets,
                "removed": confirm,
                "note": "DARCBench only ever creates files under its state directory. It never \
                         modifies web server, panel or firewall configuration, so there is \
                         nothing else to undo.",
            }))?
        );
    } else {
        println!("{}", style.bold("DARCBench uninstall"));
        println!();
        println!("  DARCBench writes only inside its state directory and never modifies");
        println!("  web server, hosting panel or firewall configuration, so uninstalling");
        println!("  is just removing this directory:");
        println!();
        println!("    {}", state_dir.display());
        println!();
        if !confirm {
            println!(
                "  {}",
                style.yellow("Nothing removed. Re-run with --confirm to delete.")
            );
            return Ok(0);
        }
    }

    if confirm && state_dir.exists() {
        std::fs::remove_dir_all(state_dir)
            .with_context(|| format!("could not remove {}", state_dir.display()))?;
        if !cli.json {
            println!("  {}", style.green("Removed."));
        }
    }
    Ok(0)
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;

    #[test]
    fn wrapping_keeps_every_line_inside_the_column() {
        let text = "This run will write about 5 GiB in total. On flash storage that is \
                    endurance spent permanently, and on a consumer SSD a run like this is a \
                    measurable fraction of a day's normal writes.";
        let indent = 33;
        let lines = wrap_indented(text, 100, indent);
        assert!(lines.len() > 1, "a paragraph must actually wrap");
        for line in &lines {
            assert!(
                line.chars().count() + indent <= 100,
                "line overruns the terminal: {line:?}"
            );
        }
    }

    #[test]
    fn wrapping_loses_no_words() {
        let text = "alpha beta gamma delta epsilon zeta eta theta";
        let joined = wrap_indented(text, 40, 20).join(" ");
        assert_eq!(joined, text);
    }

    /// A path or identifier wider than the column is emitted whole. Splitting
    /// it would produce something that cannot be copied out of the terminal.
    #[test]
    fn an_overlong_word_is_not_broken() {
        let path = "/very/long/path/that/exceeds/the/available/width/by/a/lot";
        let lines = wrap_indented(&format!("see {path} now"), 40, 20);
        assert!(
            lines.iter().any(|line| line.contains(path)),
            "the path must survive intact: {lines:?}"
        );
    }

    #[test]
    fn wrapping_an_empty_message_produces_nothing_to_print() {
        assert!(wrap_indented("", 80, 10).is_empty());
    }

    /// A narrow terminal must still make progress rather than emit one word
    /// per line forever or divide by zero on the available width.
    #[test]
    fn wrapping_survives_an_indent_wider_than_the_terminal() {
        let lines = wrap_indented("alpha beta gamma", 20, 40);
        assert!(!lines.is_empty());
        assert_eq!(lines.join(" "), "alpha beta gamma");
    }
}
