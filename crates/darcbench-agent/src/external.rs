//! The two sides of an external load-generation session, on the command line.
//!
//! The machinery is [`darcbench_modules::external_session`]; the reasoning is
//! in [`darcbench_protocol::external`] and `docs/ROADMAP.md`. What lives here
//! is the operator's experience of it, which has one job beyond wiring:
//!
//! **Make the ticket look like the secret it is.** It carries a 256-bit token,
//! and anything holding it can be served by the target for the length of the
//! session. It is printed once, it is never logged, and the text around it
//! says so - because an operator who pastes it into a ticketing system has
//! given away the session, and nothing downstream can detect that they did.

use std::io::Write;
use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Context, Result};
use darcbench_modules::external_session::{
    Driver, SessionError, TargetSession, Ticket, DRIVE_SHAPES,
};
use darcbench_protocol::external::{LoadRequest, ShapeReport};

use crate::cli::{Cli, Style};
use crate::config::StatePath;

/// Object sizes an external session serves. The same three `web.static` uses,
/// so the two are talking about the same workload.
const OBJECT_SIZES: &[usize] = &[1024, 64 * 1024, 1024 * 1024];

/// How often the target checks for a report while waiting.
///
/// A quarter second. It bounds how long after expiry the wait returns and
/// nothing else; a report that arrives is picked up on the next tick, and the
/// tick costs a mutex acquisition against a session that lasts minutes.
const POLL: Duration = Duration::from_millis(250);

/// Hosts the origin and waits.
pub(crate) fn web_target(
    cli: &Cli,
    style: &Style,
    state_dir: &std::path::Path,
    bind: &str,
    tls: bool,
    minutes: u64,
) -> Result<i32> {
    let ip: IpAddr = bind
        .parse()
        .with_context(|| format!("`{bind}` is not an IP address"))?;

    let ttl = Duration::from_secs(minutes.saturating_mul(60));
    let session = TargetSession::start(ip, OBJECT_SIZES.to_vec(), tls, ttl).map_err(refusal)?;
    let ticket = session
        .ticket()
        .encode()
        .map_err(|error| anyhow::anyhow!("{error}"))?;

    if cli.json {
        // One document on stdout, as everywhere else under --json. The ticket
        // is in it, which is why this is the mode a script uses and the mode
        // whose output must not be teed into a log.
        println!(
            "{}",
            serde_json::json!({
                "kind": "web-target",
                "origin": session.origin_address().to_string(),
                "control": session.control_address().to_string(),
                "tls": tls,
                "expires_in_seconds": ttl.as_secs(),
                "ticket": ticket,
            })
        );
    } else {
        println!("{}", style.bold("DARCBench external load target"));
        println!();
        println!("  Origin    {}", session.origin_address());
        println!("  Control   {}", session.control_address());
        println!("  Transport {}", if tls { "TLS" } else { "plaintext" });
        println!("  Expires   in {minutes} minute(s)");
        println!();
        println!("{}", style.bold("Run this on the generator machine:"));
        println!();
        println!("  darcbench web-drive --ticket {ticket}");
        println!();
        println!(
            "{}",
            style.yellow(
                "The ticket above is a secret. It contains the token that lets a machine be \
                 served by this one, and anything holding it can use this session. Carry it over \
                 a channel you trust and do not paste it anywhere it will be stored."
            )
        );
        if !tls {
            println!(
                "{}",
                style.yellow(
                    "This session is plaintext, so the token travels in clear on both channels. \
                     Use --tls on any network you do not own."
                )
            );
        }
        println!();
        println!("Waiting for the generator. Ctrl-C to stop.");
    }
    // Flushed explicitly: the wait below can hold this thread for minutes, and
    // a ticket sitting in a pipe's buffer while the operator stares at nothing
    // is a feature that appears to have hung.
    let _ = std::io::stdout().flush();

    let result = match session.wait(POLL) {
        Ok(result) => result,
        Err(SessionError::Expired) => {
            eprintln!(
                "{}",
                style.red(&format!(
                    "No report arrived within {minutes} minute(s). The origin has been shut down."
                ))
            );
            return Ok(2);
        }
        Err(SessionError::Rejected(rejection)) => {
            // Not a crash and not a benchmark result. The generator sent
            // something; this machine checked it against what it actually did
            // and will not record it.
            eprintln!("{}", style.red(&format!("Report rejected: {rejection}")));
            return Ok(2);
        }
        Err(error) => return Err(anyhow::anyhow!("{error}")),
    };

    let path =
        StatePath::join(state_dir, &["external"]).map_err(|error| anyhow::anyhow!("{error}"))?;
    std::fs::create_dir_all(path.as_path())
        .with_context(|| format!("could not create {}", path.as_path().display()))?;
    let file = StatePath::join(
        state_dir,
        &["external", &format!("{}.json", result.report.session_id)],
    )
    .map_err(|error| anyhow::anyhow!("{error}"))?;
    let document = serde_json::json!({
        "session_id": result.report.session_id,
        "served": result.served,
        "refused": result.refused,
        "report": result.report,
    });
    std::fs::write(file.as_path(), serde_json::to_vec_pretty(&document)?)
        .with_context(|| format!("could not write {}", file.as_path().display()))?;

    if cli.json {
        println!("{document}");
    } else {
        println!();
        println!("{}", style.green("Report accepted and reconciled."));
        println!(
            "  {} requests answered by this machine, {} claimed by the generator.",
            result.served,
            result.report.claimed_requests()
        );
        if result.refused > 0 {
            // Never fatal, never hidden. It means the port was found by
            // something that was not the generator.
            println!(
                "{}",
                style.yellow(&format!(
                    "  {} request(s) reached the origin without the session token. They were \
                     refused and are not part of the measurement, but something other than your \
                     generator found this port.",
                    result.refused
                ))
            );
        }
        for shape in &result.report.shapes {
            print_shape(style, shape);
        }
        for clamp in &result.report.clamps {
            println!("  {}", style.dim(&format!("clamped: {clamp}")));
        }
        println!();
        println!("  Written to {}", file.as_path().display());
    }
    Ok(0)
}

/// Drives a target named by a ticket.
pub(crate) fn web_drive(
    cli: &Cli,
    style: &Style,
    ticket: &str,
    rate: f64,
    seconds: u64,
    connections: u32,
) -> Result<i32> {
    let ticket = Ticket::decode(ticket).map_err(|error| {
        anyhow::anyhow!(
            "{error}. A ticket comes from `darcbench web-target` on the machine you want to \
             measure; there is no way to give this command a plain address, and there will not be."
        )
    })?;

    let wanted = LoadRequest {
        rate_per_s: rate,
        duration_ms: seconds.saturating_mul(1000),
        workers: connections,
        // Every shape is measured back to back inside one session, so the
        // session's lifetime is a budget they share. Declaring the count is
        // what lets `accept` divide it: without this, each shape was clamped
        // against the *whole* remaining session, and four thirty-second shapes
        // inside a one-minute ticket ran the target out of time around the
        // third - losing the report of the two that had already succeeded.
        phases: DRIVE_SHAPES.len() as u32,
    };
    let driver = match Driver::open(ticket, wanted) {
        Ok(driver) => driver,
        Err(SessionError::Refused(refusal)) => {
            eprintln!(
                "{}",
                style.red(&format!("Refusing to generate load: {refusal}"))
            );
            return Ok(2);
        }
        Err(error) => return Err(anyhow::anyhow!("{error}")),
    };

    let granted = driver.accepted().granted;
    if !cli.json {
        println!("{}", style.bold("DARCBench external load generator"));
        println!(
            "  {:.0} req/s, {} ms per shape, {} connections",
            granted.rate_per_s, granted.duration_ms, granted.workers
        );
        for clamp in &driver.accepted().clamps {
            println!("  {}", style.yellow(&format!("clamped: {clamp}")));
        }
        println!();
    }

    let mut shapes: Vec<ShapeReport> = Vec::with_capacity(DRIVE_SHAPES.len());
    for shape in DRIVE_SHAPES {
        match driver.measure(*shape) {
            Ok(report) => {
                if !cli.json {
                    print_shape(style, &report);
                    let _ = std::io::stdout().flush();
                }
                shapes.push(report);
            }
            // A shape the target does not serve is not a reason to throw away
            // the shapes it does. It is reported and the run continues, which
            // is also what makes a target offering a subset usable at all.
            Err(error) => eprintln!(
                "{}",
                style.yellow(&format!("skipped {}: {error}", shape.key))
            ),
        }
    }

    if shapes.is_empty() {
        eprintln!(
            "{}",
            style.red("No shape could be measured, so there is nothing to report.")
        );
        return Ok(2);
    }

    driver
        .submit(shapes.clone())
        .map_err(|error| anyhow::anyhow!("{error}"))?;

    if cli.json {
        println!(
            "{}",
            serde_json::json!({ "kind": "web-drive", "shapes": shapes })
        );
    } else {
        println!();
        println!(
            "{}",
            style.green("Report submitted. The target reconciles it against what it served.")
        );
    }
    Ok(0)
}

fn print_shape(style: &Style, shape: &ShapeReport) {
    println!(
        "  {:<28} {:>10.0} req/s   p50 {:>7.2} ms   p99 {:>7.2} ms",
        shape.shape, shape.achieved_rate_per_s, shape.response_ms.p50_ms, shape.response_ms.p99_ms
    );
    if let Some(saturation) = &shape.saturation {
        println!("    {}", style.yellow(saturation));
    }
    if shape.requests_failed > 0 {
        println!(
            "    {}",
            style.yellow(&format!(
                "{} request(s) failed: {}",
                shape.requests_failed,
                shape.error_examples.join("; ")
            ))
        );
    }
}

/// Turns a session start-up failure into an error whose text says what to do.
///
/// The two refusals an operator actually hits are a wildcard bind and a
/// session length outside the bounds, and both are decisions rather than
/// faults - so they get an explanation rather than a stack of `Caused by`.
fn refusal(error: SessionError) -> anyhow::Error {
    anyhow::anyhow!("{error}")
}
