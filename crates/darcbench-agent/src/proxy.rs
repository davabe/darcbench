//! Optional reverse-proxy integration: generate, preview, validate, back up,
//! roll back.
//!
//! # What this is for
//!
//! The dashboard binds loopback and prints an SSH port-forward hint, which is
//! correct and is what most operators should use. Some cannot: a machine
//! reached through a bastion, a team that shares a browser session, an
//! operator who wants `https://host/darcbench/` behind the certificate their
//! web server already has. This generates the server configuration for that.
//!
//! # This is the most dangerous thing DARCBench does
//!
//! `docs/THREAT-MODEL.md` T-CONFIG is blunt: *"Corrupting a Plesk vhost or
//! restarting nginx on a live box is worse than any benchmark result is
//! worth."* Every other part of this program reads the operator's
//! configuration; this part writes to it, on a machine that is probably
//! serving customers right now.
//!
//! Five rules follow, and they are enforced here rather than documented and
//! hoped for.
//!
//! **1. Never write a byte into a path the web server reads.** DARCBench
//! writes exactly one file, whose name and directory are compile-time
//! constants, and it goes somewhere the server does *not* auto-include -
//! `/etc/nginx/darcbench-location.conf`, not `/etc/nginx/conf.d/`. The file is
//! inert. Nothing about the running server changes when it appears, when it is
//! edited, or when it is deleted.
//!
//! The single line that makes it live is an `include` the *operator* adds
//! inside the `server` block they choose, and this program neither writes that
//! line nor knows which file it went into. That is the one change to a live
//! configuration, it is one line, they make it, and they can see it.
//!
//! This was not the first design. The first version wrote into
//! `/etc/nginx/conf.d/`, which is included at nginx's `http` level - so a bare
//! `location` directive there is a syntax error, and the very first run
//! against a real nginx produced `"location" directive is not allowed here`.
//! The safety machinery did its job and removed the file, but the lesson is
//! the better rule above: a program that never stages anything live cannot
//! stage anything broken.
//!
//! **2. A file already at our path is moved aside, never overwritten.** And if
//! the backup itself exists, the whole operation is refused rather than
//! destroying somebody's only copy.
//!
//! **3. Never reload.** The reload is the moment a change becomes an outage,
//! and the operator knows whether now is a good moment to bounce their web
//! server while this program does not. [`apply`] prints the reload command; it
//! does not run it.
//!
//! **4. Validate with the server's own validator, or say plainly that it was
//! not validated.** A config DARCBench believes is fine is worth nothing;
//! `nginx -t` is the only opinion that matters. That means executing a binary
//! the operator installed, which is
//! [T-EXEC](../../../docs/THREAT-MODEL.md), so it goes through the same
//! hardened layer the runtime modules use: a compile-time path allow-list, an
//! ownership check on the binary *and every ancestor directory*, fixed
//! arguments, a cleared environment and a hard timeout.
//!
//! The snippet is checked in isolation, by wrapping it in a minimal complete
//! configuration in a temporary directory. That proves the syntax without
//! reading, and without needing, anything the operator wrote. [`verify`] runs
//! the live validator afterwards, once their `include` line is in place, which
//! is the check that covers the whole result.
//!
//! Because the file is inert, "could not be validated" is no longer a reason
//! to delete it - an unvalidated file nothing reads is harmless. That
//! simplification falls straight out of rule 1.
//!
//! **5. Refuse on panel-managed hosts.** Plesk, cPanel, DirectAdmin and their
//! kin *generate* the web server's configuration and rewrite it on their own
//! schedule. A snippet the operator includes by hand is at best ignored after
//! the next regeneration and at worst breaks it, and the failure surfaces
//! hours later as vhosts that stopped working. There is no `--force` for this.
//! Those panels have their own reverse-proxy feature, and it is the right
//! tool.
//!
//! # What the generated configuration contains, and why
//!
//! Two directives in it are not obvious and an operator writing this by hand
//! would very likely omit both:
//!
//! * `proxy_buffering off` (nginx) / `ProxyPass ... flushpackets=on` (Apache).
//!   The dashboard's live progress is a Server-Sent Events stream. A buffering
//!   proxy holds it until the buffer fills, so the run appears frozen and then
//!   jumps - which reads as the agent being broken.
//! * `X-Forwarded-Proto`. The session cookie is marked `Secure` when, and only
//!   when, the browser reached the agent over TLS, and the agent always speaks
//!   plain HTTP so it cannot tell on its own. Without this header the cookie
//!   is not `Secure` on an HTTPS site; with it wrongly set to `https` on a
//!   plain-HTTP site the browser discards the cookie and the event stream has
//!   no way to authenticate. It is emitted from `$scheme` rather than
//!   hardcoded, so it is right in both cases.

use std::io::Write as _;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use darcbench_inventory::Inventory;
use darcbench_modules::runtime_exec;
use serde::{Deserialize, Serialize};

use crate::cli::{Cli, Style};
use crate::config::StatePath;

/// The receipt, under the state directory, that makes rollback possible.
const RECEIPT_FILE: &str = "proxy.json";

/// Suffix for a file we found at our own path and moved aside.
const BACKUP_SUFFIX: &str = ".darcbench-backup";

/// How long a config validator may run before it is killed.
///
/// `nginx -t` on a large configuration reads every included file and resolves
/// every `server_name`; ten seconds is generous for that and short enough that
/// a validator waiting on something that will never arrive does not hold the
/// operator's terminal.
const VALIDATE_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Which server, and where its files live
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Server {
    Nginx,
    Apache,
}

impl std::str::FromStr for Server {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "nginx" => Ok(Self::Nginx),
            "apache" | "apache2" | "httpd" => Ok(Self::Apache),
            other => Err(format!(
                "`{other}` is not a supported reverse proxy. DARCBench generates configuration \
                 for `nginx` and `apache` only - not because others are worse, but because a \
                 generator for a server nobody tested is a config file with an authoritative \
                 tone and no evidence behind it."
            )),
        }
    }
}

/// One way a distribution lays out a web server's configuration.
///
/// Every path here is a compile-time constant. Nothing an operator types
/// reaches a filesystem path in this module - the flags they do have choose a
/// port and a URL prefix, both of which land in the *contents* of a file whose
/// location was decided when this program was compiled.
struct Layout {
    server: Server,
    /// Where the snippet goes. Deliberately **not** an auto-included
    /// directory: the server's configuration root, where nothing scans.
    file: &'static str,
    /// A file that must exist for this layout to be the right one.
    marker: &'static str,
    /// The directive the operator adds inside their own server block.
    include_directive: &'static str,
    /// Where that directive belongs, in the operator's words.
    include_context: &'static str,
}

const LAYOUTS: &[Layout] = &[
    Layout {
        server: Server::Nginx,
        file: "/etc/nginx/darcbench-location.conf",
        marker: "/etc/nginx/nginx.conf",
        include_directive: "include /etc/nginx/darcbench-location.conf;",
        include_context: "inside the `server { ... }` block that already serves the hostname you \
                          want the dashboard on - usually in /etc/nginx/sites-enabled/ or \
                          /etc/nginx/conf.d/",
    },
    // Debian and Ubuntu.
    Layout {
        server: Server::Apache,
        file: "/etc/apache2/darcbench-location.conf",
        marker: "/etc/apache2/apache2.conf",
        include_directive: "Include /etc/apache2/darcbench-location.conf",
        include_context: "inside the <VirtualHost> that already serves the hostname you want the \
                          dashboard on - usually in /etc/apache2/sites-enabled/",
    },
    // RHEL, Fedora, Alma, Rocky.
    Layout {
        server: Server::Apache,
        file: "/etc/httpd/darcbench-location.conf",
        marker: "/etc/httpd/conf/httpd.conf",
        include_directive: "Include /etc/httpd/darcbench-location.conf",
        include_context: "inside the <VirtualHost> that already serves the hostname you want the \
                          dashboard on - usually in /etc/httpd/conf.d/",
    },
];

impl Layout {
    fn target(&self) -> PathBuf {
        PathBuf::from(self.file)
    }

    /// The directory the snippet's own file lives in, which must already exist.
    fn config_root(&self) -> &Path {
        Path::new(self.file)
            .parent()
            .unwrap_or_else(|| Path::new("/etc"))
    }

    fn reload_command(&self) -> &'static str {
        match self.server {
            Server::Nginx => "systemctl reload nginx   (or: nginx -s reload)",
            Server::Apache => "systemctl reload apache2   (or: apachectl graceful)",
        }
    }
}

/// Binaries allowed to validate a configuration, most specific first.
///
/// An allow-list for the same reason the runtime modules have one: this
/// program runs as root on shared hosts, and "whatever `nginx` resolves to on
/// `PATH`" is a decision made by whoever last wrote to a directory on it.
const NGINX_VALIDATORS: &[&str] = &[
    "/usr/sbin/nginx",
    "/usr/local/sbin/nginx",
    "/usr/bin/nginx",
    "/opt/nginx/sbin/nginx",
];

const APACHE_VALIDATORS: &[&str] = &[
    "/usr/sbin/apachectl",
    "/usr/sbin/apache2ctl",
    "/usr/local/sbin/apachectl",
    "/usr/sbin/httpd",
    "/usr/bin/apachectl",
];

// ---------------------------------------------------------------------------
// The receipt
// ---------------------------------------------------------------------------

/// What `apply` did, so `rollback` can undo exactly that and nothing else.
///
/// Written before the reload command is printed and removed by a successful
/// rollback. A rollback that guessed - "delete anything called darcbench.conf"
/// - would eventually delete a file somebody else put there.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Receipt {
    server: String,
    /// The file DARCBench created.
    file: PathBuf,
    /// A file that was already at `file` and was moved aside.
    backup: Option<PathBuf>,
    port: u16,
    location: String,
    written_at: String,
}

fn receipt_path(state_dir: &Path) -> Result<PathBuf> {
    Ok(StatePath::join(state_dir, &[RECEIPT_FILE])
        .map_err(|error| anyhow::anyhow!("{error}"))?
        .as_path()
        .to_path_buf())
}

// ---------------------------------------------------------------------------
// Input validation
// ---------------------------------------------------------------------------

/// Checks the URL prefix the operator asked for.
///
/// This is the one piece of caller input that reaches the generated file, and
/// the check is not cosmetic. An nginx `location` is terminated by `}`, so a
/// prefix containing one would close the block and everything after it would
/// be top-level directives of the operator's choosing - in a file this program
/// writes as root and the web server executes. The same is true of Apache and
/// `<`. So the grammar is an allow-list of characters that cannot terminate
/// anything, not a blocklist of the ones noticed so far.
fn validate_location(location: &str) -> Result<String> {
    anyhow::ensure!(
        location.starts_with('/'),
        "the URL prefix must start with `/`"
    );
    anyhow::ensure!(
        location.len() <= 128,
        "the URL prefix must be at most 128 characters"
    );
    anyhow::ensure!(
        location
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/-_.~".contains(&byte)),
        "the URL prefix may contain only letters, digits and `/ - _ . ~`. Anything else could \
         close the configuration block it is written into and turn a prefix into a directive."
    );
    anyhow::ensure!(
        !location.contains(".."),
        "the URL prefix may not contain `..`"
    );
    // A prefix without a trailing slash makes `/darcbenchfoo` match too, which
    // is a surprise nobody wants on a shared hostname.
    Ok(if location.ends_with('/') {
        location.to_string()
    } else {
        format!("{location}/")
    })
}

// ---------------------------------------------------------------------------
// Generation
// ---------------------------------------------------------------------------

fn generate(layout: &Layout, port: u16, location: &str) -> String {
    let version = crate::runner::AGENT_VERSION;
    match layout.server {
        Server::Nginx => format!(
            "# Generated by DARCBench {version}. Safe to delete: removing this file and\n\
             # reloading nginx returns the server to exactly its previous behaviour.\n\
             #\n\
             # Written by `darcbench proxy apply`; undo with `darcbench proxy rollback`.\n\
             #\n\
             # This file is inert on its own. It does nothing until an `include` for it\n\
             # appears inside a server block. DARCBench did not add that line and cannot\n\
             # remove it: editing your configuration is yours to do and to undo.\n\
             \n\
             location {location} {{\n\
             \x20   proxy_pass http://127.0.0.1:{port}{location};\n\
             \x20   proxy_http_version 1.1;\n\
             \n\
             \x20   proxy_set_header Host              $host;\n\
             \x20   proxy_set_header X-Real-IP         $remote_addr;\n\
             \x20   proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;\n\
             \x20   # The agent always speaks plain HTTP, so it cannot tell whether the\n\
             \x20   # browser reached it over TLS - and it marks the session cookie `Secure`\n\
             \x20   # only when it did. From $scheme, so it is correct on both.\n\
             \x20   proxy_set_header X-Forwarded-Proto $scheme;\n\
             \n\
             \x20   # The dashboard's live progress is a Server-Sent Events stream. A\n\
             \x20   # buffering proxy holds it until the buffer fills, so a running\n\
             \x20   # benchmark appears frozen and then jumps.\n\
             \x20   proxy_buffering off;\n\
             \x20   proxy_cache off;\n\
             \x20   proxy_read_timeout 3600s;\n\
             \x20   chunked_transfer_encoding off;\n\
             }}\n"
        ),
        Server::Apache => format!(
            "# Generated by DARCBench {version}. Safe to delete: removing this file and\n\
             # reloading Apache returns the server to exactly its previous behaviour.\n\
             #\n\
             # Written by `darcbench proxy apply`; undo with `darcbench proxy rollback`.\n\
             #\n\
             # This file is inert on its own. It does nothing until an `Include` for it\n\
             # appears inside a <VirtualHost>. DARCBench did not add that line and cannot\n\
             # remove it: editing your configuration is yours to do and to undo.\n\
             #\n\
             # Needs mod_proxy and mod_proxy_http:\n\
             #   a2enmod proxy proxy_http     (Debian/Ubuntu)\n\
             \n\
             <Location {location}>\n\
             \x20   ProxyPass        http://127.0.0.1:{port}{location} flushpackets=on timeout=3600\n\
             \x20   ProxyPassReverse http://127.0.0.1:{port}{location}\n\
             \n\
             \x20   # See the nginx equivalent for why this header is derived rather\n\
             \x20   # than hardcoded: the agent speaks plain HTTP and marks its session\n\
             \x20   # cookie `Secure` only when the browser used TLS.\n\
             \x20   RequestHeader set X-Forwarded-Proto \"https\" env=HTTPS\n\
             \x20   SetEnvIf X-Forwarded-Proto \"^$\" no_proto\n\
             </Location>\n"
        ),
    }
}

// ---------------------------------------------------------------------------
// Preconditions
// ---------------------------------------------------------------------------

/// Everything that must be true before a byte is written.
fn preflight(server: Server) -> Result<&'static Layout> {
    // Panels first. On a Plesk or cPanel box nothing below matters, because
    // the answer is no whatever the rest of the machine looks like.
    let inventory = Inventory::collect();
    if let Some(panel) = inventory.software.panels.first() {
        anyhow::bail!(
            "{} is installed on this machine ({}), and it generates the web server's \
             configuration itself. A file dropped beside that is at best ignored and at worst \
             breaks the next regeneration - hours later, as vhosts that stopped working. There \
             is no --force for this: use {}'s own reverse-proxy feature, which is the right tool \
             and knows how to survive a regeneration.",
            panel.name,
            panel.evidence,
            panel.name
        );
    }

    let layout = LAYOUTS
        .iter()
        .find(|layout| layout.server == server && Path::new(layout.marker).exists())
        .ok_or_else(|| {
            let looked_for: Vec<&str> = LAYOUTS
                .iter()
                .filter(|layout| layout.server == server)
                .map(|layout| layout.marker)
                .collect();
            anyhow::anyhow!(
                "no {server:?} installation found. Looked for {}. DARCBench will not create a \
                 configuration directory that does not exist: on a machine without that server, \
                 a file there is a file nothing reads and nobody expects.",
                looked_for.join(", ")
            )
        })?;

    anyhow::ensure!(
        layout.config_root().is_dir(),
        "{} exists but {} is not a directory.",
        layout.marker,
        layout.config_root().display()
    );
    Ok(layout)
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Prints what would be written, and writes nothing.
pub(crate) fn preview(
    cli: &Cli,
    style: &Style,
    server: &str,
    port: u16,
    location: &str,
) -> Result<i32> {
    let server: Server = server.parse().map_err(|e: String| anyhow::anyhow!(e))?;
    let location = validate_location(location)?;
    let layout = preflight(server)?;
    let body = generate(layout, port, &location);

    if cli.json {
        println!(
            "{}",
            serde_json::json!({
                "kind": "proxy-preview",
                "server": format!("{server:?}").to_lowercase(),
                "file": layout.target(),
                "include_directive": layout.include_directive,
                "reload_command": layout.reload_command(),
                "configuration": body,
            })
        );
    } else {
        println!(
            "{}",
            style.bold(&format!("Would write {}", layout.target().display()))
        );
        println!();
        println!("{body}");
        println!("{}", style.bold("Then add one line, yourself:"));
        println!("    {}", layout.include_directive);
        println!("  {}", style.dim(layout.include_context));
        println!();
        println!(
            "{}",
            style.dim("Nothing has been written. `darcbench proxy apply --confirm` writes it.")
        );
    }
    Ok(0)
}

/// Writes the file, validates the server's whole configuration, and stops.
pub(crate) fn apply(
    cli: &Cli,
    style: &Style,
    state_dir: &Path,
    server: &str,
    port: u16,
    location: &str,
    confirm: bool,
) -> Result<i32> {
    let server: Server = server.parse().map_err(|e: String| anyhow::anyhow!(e))?;
    let location = validate_location(location)?;
    let layout = preflight(server)?;

    if !confirm {
        eprintln!(
            "{}",
            style.yellow(
                "This writes a file into your web server's configuration directory. Re-run with \
                 --confirm once you have read `darcbench proxy preview`."
            )
        );
        return Ok(2);
    }

    let receipt_file = receipt_path(state_dir)?;
    if receipt_file.exists() {
        eprintln!(
            "{}",
            style.red(&format!(
                "DARCBench has already written a proxy configuration; its receipt is at {}. Roll \
                 that back before writing another, so there is never more than one file this \
                 program is responsible for.",
                receipt_file.display()
            ))
        );
        return Ok(2);
    }

    let target = layout.target();
    let backup = if target.exists() {
        // Something is already at our path. Moved aside rather than
        // overwritten - and if the backup itself exists we stop, because
        // overwriting a backup destroys the only copy of whatever the last
        // attempt displaced.
        let backup = PathBuf::from(format!("{}{BACKUP_SUFFIX}", target.display()));
        anyhow::ensure!(
            !backup.exists(),
            "{} already exists. Move it somewhere safe first: overwriting it would destroy the \
             only copy of whatever was displaced last time.",
            backup.display()
        );
        std::fs::rename(&target, &backup)
            .with_context(|| format!("could not move {} aside", target.display()))?;
        Some(backup)
    } else {
        None
    };

    let body = generate(layout, port, &location);
    if let Err(error) = write_config(&target, &body) {
        restore(&target, backup.as_deref());
        return Err(error);
    }

    let receipt = Receipt {
        server: format!("{server:?}").to_lowercase(),
        file: target.clone(),
        backup: backup.clone(),
        port,
        location: location.clone(),
        written_at: chrono::Utc::now().to_rfc3339(),
    };
    // Undone on failure, not propagated. A full or read-only state directory
    // would otherwise leave the generated file installed, the operator's file
    // moved aside, and no receipt - so `rollback` has nothing to act on and
    // the next `apply` is refused because both the target and its backup
    // exist. The operator is left reconstructing by hand an operation this
    // command exists to make reversible.
    if let Err(error) = write_receipt(&receipt_file, &receipt) {
        undo_files(&target, backup.as_deref());
        return Err(error.context(
            "the configuration was written and then removed, because the receipt that makes \
             `darcbench proxy rollback` possible could not be saved. A change this program \
             cannot record is a change it cannot undo, and it will not leave one behind",
        ));
    }

    // Checked in isolation, by wrapping the snippet in a minimal complete
    // configuration in a temporary directory. That proves the syntax without
    // reading, and without needing, anything the operator wrote.
    let checked = validate_snippet(layout, &target);
    if let Validation::Failed { validator, output } = &checked {
        // The file is inert, so leaving it would harm nothing - but a broken
        // snippet the operator is about to `include` is a trap, and removing
        // it is what keeps `apply` from ever handing them one.
        undo_files(&target, backup.as_deref());
        let _ = std::fs::remove_file(&receipt_file);
        eprintln!(
            "{}",
            style.red(&format!(
                "{validator} rejected the generated configuration, so it has been removed and \
                 anything it displaced has been put back. Nothing on this machine was ever going \
                 to read it - it is removed so you are not handed a broken snippet to \
                 include.\n\n{output}"
            ))
        );
        return Ok(2);
    }

    report_applied(cli, style, layout, &receipt, &checked);
    Ok(0)
}

/// Runs the server's own validator against the live configuration.
///
/// The check that covers the whole result, for after the operator has added
/// their `include` line. [`apply`] can only prove the snippet parses on its
/// own; this proves it parses where they put it.
pub(crate) fn verify(cli: &Cli, style: &Style, server: &str) -> Result<i32> {
    let server: Server = server.parse().map_err(|e: String| anyhow::anyhow!(e))?;
    let layout = preflight(server)?;
    let checked = validate(layout, &[], None);

    let (ok, detail) = match &checked {
        Validation::Passed { validator, output } => (true, format!("{validator}: {output}")),
        Validation::Failed { validator, output } => (false, format!("{validator}: {output}")),
        Validation::Unavailable { reason } => (false, reason.clone()),
    };

    if cli.json {
        println!(
            "{}",
            serde_json::json!({ "kind": "proxy-verify", "valid": ok, "detail": detail })
        );
    } else if ok {
        println!("{}", style.green("The live configuration is valid."));
        println!("  {detail}");
        println!();
        println!("  Reload to make it take effect:");
        println!("    {}", layout.reload_command());
    } else {
        eprintln!("{}", style.red("The live configuration is not valid."));
        eprintln!("  {detail}");
        eprintln!();
        eprintln!(
            "{}",
            style.dim(
                "DARCBench has not reloaded anything, so your server is still running its last \
                 good configuration. `darcbench proxy rollback --confirm` removes the snippet; \
                 the `include` line is yours to remove."
            )
        );
    }
    Ok(if ok { 0 } else { 2 })
}

/// Removes what `apply` created and restores what it displaced.
pub(crate) fn rollback(
    cli: &Cli,
    style: &Style,
    state_dir: &Path,
    confirm: bool,
    force: bool,
) -> Result<i32> {
    let receipt_file = receipt_path(state_dir)?;
    let Ok(raw) = std::fs::read(&receipt_file) else {
        eprintln!(
            "{}",
            style.yellow(&format!(
                "No proxy configuration to roll back: there is no receipt at {}. DARCBench \
                 removes only files it recorded creating, because a rollback that guessed would \
                 eventually delete something somebody else put there.",
                receipt_file.display()
            ))
        );
        return Ok(2);
    };
    let receipt: Receipt = serde_json::from_slice(&raw)
        .with_context(|| format!("{} is unreadable", receipt_file.display()))?;

    if !confirm {
        println!("Would remove {}", receipt.file.display());
        if let Some(backup) = &receipt.backup {
            println!(
                "Would restore {} to {}",
                backup.display(),
                receipt.file.display()
            );
        }
        println!();
        println!(
            "{}",
            style.dim("Nothing has been removed. Re-run with --confirm.")
        );
        return Ok(2);
    }

    // Removing the snippet while something still includes it guarantees an
    // outage at the next reload - and that reload may be for an unrelated
    // reason, days later, by somebody who has never heard of DARCBench. So
    // the config tree is searched for references first, and a live one stops
    // this rather than producing a warning nobody reads.
    if !force {
        if let Some(reference) = find_reference(&receipt.file) {
            eprintln!(
                "{}",
                style.red(&format!(
                    "{reference} still includes {}. Removing the file now would leave your \
                     server unable to start at the next reload - which might be days from now, \
                     for an unrelated reason, done by somebody who has never heard of \
                     DARCBench.\n\n\
                     Remove that line first, then run this again. `--force` skips this check if \
                     you know what you are doing.",
                    receipt.file.display()
                ))
            );
            return Ok(2);
        }
    }

    undo_files(&receipt.file, receipt.backup.as_deref());
    std::fs::remove_file(&receipt_file)
        .with_context(|| format!("could not remove {}", receipt_file.display()))?;

    let reload = LAYOUTS
        .iter()
        .find(|layout| layout.target() == receipt.file)
        .map(|layout| layout.reload_command())
        .unwrap_or("reload your web server");

    if cli.json {
        println!(
            "{}",
            serde_json::json!({ "kind": "proxy-rollback", "removed": receipt.file })
        );
    } else {
        println!("{}", style.green("Rolled back."));
        println!("  Removed {}", receipt.file.display());
        if receipt.backup.is_some() {
            println!("  Restored the file that was there before.");
        }
        println!();
        println!("  Reload when convenient:");
        println!("    {reload}");
    }
    Ok(0)
}

/// Reports whether a DARCBench proxy configuration is installed.
pub(crate) fn status(cli: &Cli, style: &Style, state_dir: &Path) -> Result<i32> {
    let receipt_file = receipt_path(state_dir)?;
    let receipt: Option<Receipt> = std::fs::read(&receipt_file)
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok());

    match receipt {
        None => {
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({ "kind": "proxy-status", "installed": false })
                );
            } else {
                println!("No DARCBench proxy configuration is installed.");
            }
            Ok(0)
        }
        Some(receipt) => {
            // The file is checked rather than assumed. An operator who deleted
            // it by hand should be told the receipt is stale, not told the
            // configuration is present.
            let present = receipt.file.exists();
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "kind": "proxy-status",
                        "installed": true,
                        "file_present": present,
                        "receipt": receipt,
                    })
                );
            } else {
                println!("{}", style.bold("DARCBench proxy configuration"));
                println!("  Server    {}", receipt.server);
                println!("  File      {}", receipt.file.display());
                println!("  Prefix    {}", receipt.location);
                println!("  Agent     127.0.0.1:{}", receipt.port);
                println!("  Written   {}", receipt.written_at);
                if !present {
                    println!(
                        "{}",
                        style.yellow(
                            "  The file is gone. Something removed it outside DARCBench; \
                             `proxy rollback --confirm` will clear the receipt."
                        )
                    );
                }
            }
            Ok(if present { 0 } else { 2 })
        }
    }
}

// ---------------------------------------------------------------------------
// Filesystem and validation helpers
// ---------------------------------------------------------------------------

/// Writes the configuration at 0644, world-readable and root-only-writable.
///
/// Readable because the web server may run as an unprivileged user and has to
/// read it; writable by nobody else for the reason every other file this
/// program creates is - a config file a local user can edit is a config file a
/// local user can turn into a proxy to anywhere.
fn write_config(target: &Path, body: &str) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(target)
        .with_context(|| format!("could not create {}", target.display()))?;
    file.write_all(body.as_bytes())
        .and_then(|()| file.sync_all())
        .with_context(|| format!("could not write {}", target.display()))?;
    Ok(())
}

/// Puts a displaced file back, best effort.
fn restore(target: &Path, backup: Option<&Path>) {
    if let Some(backup) = backup {
        let _ = std::fs::rename(backup, target);
    }
}

/// The whole undo: remove what we made, put back what we moved.
///
fn undo_files(target: &Path, backup: Option<&Path>) {
    let _ = std::fs::remove_file(target);
    restore(target, backup);
}

/// Finds a configuration file that still names `snippet`, if one does.
///
/// Read-only, and bounded three ways: depth, file count and file size. This
/// walks a directory tree on somebody's production server, so a symlink loop
/// or a stray multi-gigabyte file in `/etc/nginx` must be a bounded
/// disappointment rather than a hang. Symlinks are not followed for the same
/// reason.
///
/// A substring match, not a parser. It over-reports - a commented-out include
/// counts - and that is the right direction: the cost of a false positive is
/// an operator being told to check a line, and the cost of a false negative is
/// their web server failing to start.
fn find_reference(snippet: &Path) -> Option<String> {
    const MAX_DEPTH: usize = 6;
    const MAX_FILES: usize = 4096;
    const MAX_BYTES: u64 = 4 << 20;

    let root = snippet.parent()?;
    let needle = snippet.to_string_lossy().to_string();
    let mut queue = vec![(root.to_path_buf(), 0usize)];
    let mut seen = 0usize;

    while let Some((dir, depth)) = queue.pop() {
        if depth > MAX_DEPTH || seen >= MAX_FILES {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if seen >= MAX_FILES {
                break;
            }
            let path = entry.path();
            // `symlink_metadata`, so a link is inspected rather than followed.
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                queue.push((path, depth + 1));
                continue;
            }
            if !meta.is_file() || meta.len() > MAX_BYTES || path == snippet {
                continue;
            }
            seen += 1;
            let Ok(body) = std::fs::read_to_string(&path) else {
                continue;
            };
            for (index, line) in body.lines().enumerate() {
                if line.contains(&needle) {
                    return Some(format!("{}:{}", path.display(), index + 1));
                }
            }
        }
    }
    None
}

fn write_receipt(path: &Path, receipt: &Receipt) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(receipt)?)
        .with_context(|| format!("could not write {}", path.display()))
}

enum Validation {
    Passed {
        validator: String,
        output: String,
    },
    Failed {
        validator: String,
        output: String,
    },
    /// No validator this program is willing to execute.
    Unavailable {
        reason: String,
    },
}

/// Asks the server whether a configuration is valid.
///
/// Through [`runtime_exec`], which is the layer ADR-0013 built for the runtime
/// modules: a compile-time path allow-list, an ownership check on the binary
/// and every ancestor directory, fixed arguments, a cleared environment and a
/// hard timeout. The reasoning transfers exactly - this program often runs as
/// root on a shared host, and executing "whatever `nginx` resolves to" hands
/// root to whoever last wrote to a directory on `PATH`.
///
/// `extra` and `prefix` are how the isolated check points the validator at a
/// generated wrapper instead of the live configuration. They are built here,
/// never by a caller outside this module, so the argument vector stays fixed
/// in the sense T-EXEC requires: it is never assembled from anything a user
/// typed.
fn validate(layout: &Layout, extra: &[&str], _prefix: Option<&Path>) -> Validation {
    let (candidates, base): (&[&str], &[&str]) = match layout.server {
        Server::Nginx => (NGINX_VALIDATORS, &["-t"]),
        Server::Apache => (APACHE_VALIDATORS, &["-t"]),
    };

    let (found, rejections) = runtime_exec::discover(candidates);
    let Some(validator) = found else {
        let mut reason = format!(
            "No config validator was found among {}.",
            candidates.join(", ")
        );
        for rejection in &rejections {
            // A rejected binary is a much more useful thing to report than an
            // absent one: it means the operator has the server installed at a
            // path this program refuses to execute, and they can act on that.
            reason.push_str(&format!(
                "\n  {} was refused: {}",
                rejection.path.display(),
                rejection.reason
            ));
        }
        return Validation::Unavailable { reason };
    };

    let mut args: Vec<&str> = base.to_vec();
    args.extend_from_slice(extra);

    let name = validator.path.display().to_string();
    match runtime_exec::run(&validator, &args, VALIDATE_TIMEOUT) {
        Err(error) => Validation::Unavailable {
            reason: format!("{name} could not be run: {error}"),
        },
        Ok(output) => {
            // Both streams. `nginx -t` writes its verdict to stderr on success
            // as well as failure, and an operator reading a rejection wants
            // the line number it names.
            let combined = format!("{}{}", output.stdout, output.stderr)
                .trim()
                .to_string();
            if output.succeeded() {
                Validation::Passed {
                    validator: name,
                    output: combined,
                }
            } else {
                Validation::Failed {
                    validator: name,
                    output: combined,
                }
            }
        }
    }
}

/// Checks the snippet on its own, without reading the operator's config.
///
/// Wraps it in a minimal complete configuration in a fresh temporary
/// directory and validates that. What it proves is exactly what it can prove:
/// the snippet parses and its directives are legal in a server block. It does
/// not prove the result is valid where the operator puts it - that is
/// [`verify`], and it needs their `include` line to exist first.
///
/// nginx only. Apache's `-f` needs a `ServerRoot`, a module set and load paths
/// that vary per distribution, and a wrapper assembled from guesses would fail
/// for reasons that have nothing to do with the snippet - reporting a
/// perfectly good file as broken. Reported as unvalidated instead, which is
/// true and is safe because the file is inert.
fn validate_snippet(layout: &Layout, snippet: &Path) -> Validation {
    if layout.server != Server::Nginx {
        return Validation::Unavailable {
            reason: "Apache cannot be asked to check a fragment on its own without guessing at \
                     this distribution's module paths, and a wrapper built from guesses would \
                     report a good file as broken."
                .to_string(),
        };
    }

    let Ok(scratch) = scratch_dir() else {
        return Validation::Unavailable {
            reason: "could not create a temporary directory for the isolated check".to_string(),
        };
    };
    let wrapper = scratch.join("wrapper.conf");
    // `worker_connections` must exceed the number of listening sockets or
    // nginx refuses the config for a reason that has nothing to do with the
    // snippet. A port in the ephemeral range, and nothing ever listens on it:
    // `-t` parses and exits without binding.
    let body = format!(
        "events {{ worker_connections 64; }}\n         http {{\n         \x20   server {{\n         \x20       listen 127.0.0.1:59999;\n         \x20       include {};\n         \x20   }}\n         }}\n",
        snippet.display()
    );
    if std::fs::write(&wrapper, body).is_err() {
        let _ = std::fs::remove_dir_all(&scratch);
        return Validation::Unavailable {
            reason: "could not write the wrapper for the isolated check".to_string(),
        };
    }

    let wrapper_arg = wrapper.display().to_string();
    let prefix_arg = scratch.display().to_string();
    let error_log = scratch.join("error.log").display().to_string();
    let outcome = validate(
        layout,
        &["-c", &wrapper_arg, "-p", &prefix_arg, "-e", &error_log],
        Some(&scratch),
    );
    let _ = std::fs::remove_dir_all(&scratch);

    // The wrapper's own path appears in every message nginx emits about it,
    // and a temporary directory in an error an operator has to act on is
    // noise that points at a file which no longer exists.
    match outcome {
        Validation::Failed { validator, output } => Validation::Failed {
            validator,
            output: output.replace(&wrapper_arg, "<isolated check>"),
        },
        Validation::Passed { validator, output } => Validation::Passed {
            validator,
            output: output.replace(&wrapper_arg, "<isolated check>"),
        },
        other => other,
    }
}

/// A fresh directory nobody else can write to, for the isolated check.
fn scratch_dir() -> Result<PathBuf> {
    let mut raw = [0u8; 8];
    getrandom::getrandom(&mut raw).map_err(|error| anyhow::anyhow!("{error}"))?;
    let dir = std::env::temp_dir().join(format!("darcbench-proxy-check-{}", hex::encode(raw)));
    std::fs::DirBuilder::new()
        .recursive(false)
        .mode(0o700)
        .create(&dir)?;
    Ok(dir)
}

fn report_applied(
    cli: &Cli,
    style: &Style,
    layout: &Layout,
    receipt: &Receipt,
    checked: &Validation,
) {
    let (validated, detail) = match checked {
        Validation::Passed { validator, output } => (true, format!("{validator}: {output}")),
        Validation::Failed { validator, output } => (false, format!("{validator}: {output}")),
        Validation::Unavailable { reason } => (false, reason.clone()),
    };

    if cli.json {
        println!(
            "{}",
            serde_json::json!({
                "kind": "proxy-apply",
                "snippet_validated": validated,
                "validation_detail": detail,
                "include_directive": layout.include_directive,
                "reload_command": layout.reload_command(),
                "receipt": receipt,
            })
        );
        return;
    }
    println!("{}", style.green("Written."));
    println!("  File       {}", receipt.file.display());
    if let Some(backup) = &receipt.backup {
        println!("  Displaced  {} (kept)", backup.display());
    }
    if validated {
        println!("  Checked    {detail}");
    } else {
        // Said plainly rather than implied. The file is inert, so an
        // unvalidated one is harmless - but an operator who thinks it was
        // checked and finds out at reload time has been misled.
        println!(
            "{}",
            style.yellow(&format!(
                "  NOT checked: {detail}\n  The file is inert, so nothing is at risk - but run \
                 `darcbench proxy verify` after you add the include line."
            ))
        );
    }
    println!();
    println!(
        "{}",
        style.bold("Nothing is live yet. This file is inert until you include it.")
    );
    println!("  Add this one line, {}:", layout.include_context);
    println!("    {}", layout.include_directive);
    println!();
    println!("  Then check and reload:");
    println!("    darcbench proxy verify");
    println!("    {}", layout.reload_command());
    println!();
    println!("  Undo at any time with:  darcbench proxy rollback --confirm");
    println!();
    println!(
        "{}",
        style.yellow(&format!(
            "The dashboard will then be reachable at {} on whichever hostname that server block \
             answers. Its token is the only thing protecting it, and the token will appear in \
             your web server's access log the first time a browser follows the bootstrap URL. \
             Put the prefix behind whatever authentication your other admin paths use.",
            receipt.location
        ))
    );
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_url_prefix_that_could_close_the_configuration_block_is_refused() {
        // The one piece of caller input that reaches the generated file. An
        // nginx `location` is terminated by `}`, so a prefix containing one
        // would close the block and everything after it would be top-level
        // directives - in a file this program writes as root and the web
        // server executes.
        for hostile in [
            "/a} location / { proxy_pass http://evil;",
            "/a\n}\n",
            "/a\"; }",
            "/a<Location />",
            "/a$b",
            "/a b",
            "/a\0b",
            "/a'",
            "/a\\",
        ] {
            assert!(
                validate_location(hostile).is_err(),
                "accepted {hostile:?}, which can escape the block it is written into"
            );
        }
    }

    #[test]
    fn a_prefix_that_is_not_a_prefix_is_refused() {
        assert!(validate_location("darcbench").is_err());
        assert!(validate_location("../etc").is_err());
        assert!(validate_location("/a/../../b").is_err());
        assert!(validate_location(&format!("/{}", "a".repeat(200))).is_err());
    }

    #[test]
    fn a_prefix_without_a_trailing_slash_gets_one() {
        // Otherwise `/darcbench` matches `/darcbenchfoo` too, which is a
        // surprise nobody wants on a shared hostname.
        assert_eq!(validate_location("/darcbench").unwrap(), "/darcbench/");
        assert_eq!(validate_location("/darcbench/").unwrap(), "/darcbench/");
        assert_eq!(validate_location("/").unwrap(), "/");
    }

    #[test]
    fn the_generated_nginx_config_carries_the_two_directives_nobody_remembers() {
        let layout = LAYOUTS.iter().find(|l| l.server == Server::Nginx).unwrap();
        let body = generate(layout, 7842, "/darcbench/");
        // Without this the live event stream is buffered and a running
        // benchmark appears frozen.
        assert!(body.contains("proxy_buffering off"), "{body}");
        // Without this the session cookie is not `Secure` on an HTTPS site.
        assert!(
            body.contains("proxy_set_header X-Forwarded-Proto $scheme"),
            "{body}"
        );
        // From $scheme, never hardcoded: hardcoding `https` on a plain-HTTP
        // site makes the browser discard the cookie and the event stream then
        // has no way to authenticate.
        assert!(!body.contains("X-Forwarded-Proto https"), "{body}");
        assert!(body.contains("proxy_pass http://127.0.0.1:7842/darcbench/"));
    }

    #[test]
    fn the_generated_config_points_only_at_loopback() {
        // The agent binds loopback and this file is the bridge to it. A
        // generated config that could name another host would be a
        // general-purpose proxy generator, which is not what this is.
        for layout in LAYOUTS {
            let body = generate(layout, 7842, "/darcbench/");
            assert!(body.contains("http://127.0.0.1:7842"), "{}", layout.marker);
        }
    }

    #[test]
    fn the_generated_config_says_how_to_undo_it() {
        // An operator who finds this file in six months, with no memory of
        // DARCBench, must be able to tell from the file itself that deleting
        // it is safe.
        for layout in LAYOUTS {
            let body = generate(layout, 7842, "/darcbench/");
            assert!(body.contains("Safe to delete"), "{}", layout.marker);
            assert!(body.contains("proxy rollback"), "{}", layout.marker);
            assert!(body.contains("inert"), "{}", layout.marker);
        }
    }

    #[test]
    fn every_layout_writes_a_compile_time_path_outside_any_include_directory() {
        // The rule this file rests on: DARCBench never puts a byte into a path
        // the web server reads. The first version wrote into
        // `/etc/nginx/conf.d/`, which nginx includes at the `http` level, so
        // the very first run against a real nginx produced `"location"
        // directive is not allowed here`.
        for layout in LAYOUTS {
            let target = layout.target();
            assert!(target.is_absolute(), "{target:?}");
            assert!(target.starts_with("/etc/"), "{target:?}");
            assert_eq!(target.file_name().unwrap(), "darcbench-location.conf");
            for scanned in ["conf.d", "conf-available", "conf-enabled", "sites-enabled"] {
                assert!(
                    !target.to_string_lossy().contains(scanned),
                    "{target:?} is inside {scanned}, which the server scans"
                );
            }
            // And the operator's one line must point at exactly that file.
            assert!(
                layout.include_directive.contains(layout.file),
                "{}: `{}` does not name {}",
                layout.marker,
                layout.include_directive,
                layout.file
            );
        }
    }

    #[test]
    fn an_unknown_server_is_refused_rather_than_guessed_at() {
        assert!("caddy".parse::<Server>().is_err());
        assert!("lighttpd".parse::<Server>().is_err());
        assert_eq!("NGINX".parse::<Server>().unwrap(), Server::Nginx);
        assert_eq!("httpd".parse::<Server>().unwrap(), Server::Apache);
    }

    #[test]
    fn undo_removes_what_was_made_and_restores_what_was_moved() {
        let dir = tempdir();
        let target = dir.join("darcbench-location.conf");
        let backup = dir.join("darcbench-location.conf.darcbench-backup");

        std::fs::write(&backup, "the operator's file").unwrap();
        std::fs::write(&target, "ours").unwrap();

        undo_files(&target, Some(&backup));

        assert!(!backup.exists(), "the backup was not moved back");
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "the operator's file"
        );
    }

    #[test]
    fn undo_with_no_backup_leaves_nothing_behind() {
        let dir = tempdir();
        let target = dir.join("darcbench-location.conf");
        std::fs::write(&target, "ours").unwrap();
        undo_files(&target, None);
        assert!(!target.exists());
    }

    #[test]
    fn a_config_is_never_written_over_an_existing_file() {
        // `create_new`, so a file that appeared between the rename and the
        // write is not silently replaced.
        let dir = tempdir();
        let target = dir.join("darcbench.conf");
        std::fs::write(&target, "someone else's").unwrap();
        assert!(write_config(&target, "ours").is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "someone else's");
    }

    #[test]
    fn a_written_config_is_not_writable_by_anyone_but_root() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempdir();
        let target = dir.join("darcbench.conf");
        write_config(&target, "body").unwrap();
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o022, 0, "mode {mode:o} is writable by others");
        // Readable, because the web server may run as an unprivileged user.
        assert_ne!(mode & 0o044, 0, "mode {mode:o} is not readable");
    }

    #[test]
    fn a_rollback_that_would_break_the_server_finds_the_line_that_would_break_it() {
        // Removing the snippet while something still includes it guarantees an
        // outage at the next reload - which may be days later, for an
        // unrelated reason, done by somebody who has never heard of DARCBench.
        let dir = tempdir();
        let snippet = dir.join("darcbench-location.conf");
        std::fs::write(&snippet, "location / {}").unwrap();
        assert_eq!(find_reference(&snippet), None);

        let sites = dir.join("sites-enabled");
        std::fs::create_dir_all(&sites).unwrap();
        let vhost = sites.join("default");
        std::fs::write(
            &vhost,
            format!(
                "server {{\n  server_name _;\n  include {};\n}}\n",
                snippet.display()
            ),
        )
        .unwrap();

        let found = find_reference(&snippet).expect("the include was not found");
        assert!(found.starts_with(&vhost.display().to_string()), "{found}");
        assert!(found.ends_with(":3"), "{found}");
    }

    #[test]
    fn the_reference_scan_does_not_count_the_snippet_itself() {
        // The file names its own path in its header comment. Matching that
        // would make every rollback refuse itself.
        let dir = tempdir();
        let snippet = dir.join("darcbench-location.conf");
        std::fs::write(&snippet, format!("# {}\n", snippet.display())).unwrap();
        assert_eq!(find_reference(&snippet), None);
    }

    #[test]
    fn a_receipt_round_trips() {
        // Rollback removes only what the receipt records, so the receipt has
        // to survive being written and read back exactly.
        let dir = tempdir();
        let path = dir.join("proxy.json");
        let receipt = Receipt {
            server: "nginx".to_string(),
            file: PathBuf::from("/etc/nginx/darcbench-location.conf"),
            backup: Some(PathBuf::from("/etc/nginx/darcbench-location.conf.bak")),
            port: 7842,
            location: "/darcbench/".to_string(),
            written_at: "2026-01-01T00:00:00Z".to_string(),
        };
        write_receipt(&path, &receipt).unwrap();
        let back: Receipt = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(back.file, receipt.file);
        assert_eq!(back.backup, receipt.backup);
    }

    fn tempdir() -> PathBuf {
        let mut raw = [0u8; 8];
        getrandom::getrandom(&mut raw).unwrap();
        let dir = std::env::temp_dir().join(format!("darcbench-proxy-{}", hex::encode(raw)));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
