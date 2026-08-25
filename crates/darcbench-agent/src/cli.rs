//! Command-line interface.
//!
//! Every command supports `--json` for machine consumption and `--no-color`
//! for logs and CI. Human output is decorated; JSON output is never decorated,
//! never interleaved with progress text, and is a single well-formed document
//! on stdout so `darcbench run --json | jq` works.

use clap::{Parser, Subcommand};

use crate::config::DEFAULT_PORT;

#[derive(Debug, Parser)]
#[command(
    name = "darcbench",
    version,
    about = "DARC//BENCH - Deployment, Application, Runtime & Compute benchmark suite",
    long_about = "DARCBench measures real server and web-application performance.\n\
                  Tombatossals Softworks LLC - https://darcbench.com"
)]
pub(crate) struct Cli {
    /// Emit machine-readable JSON instead of formatted text.
    #[arg(long, global = true)]
    pub(crate) json: bool,

    /// Disable ANSI colour. Also honoured via the NO_COLOR environment variable.
    #[arg(long, global = true, env = "NO_COLOR")]
    pub(crate) no_color: bool,

    /// Never prompt; fail instead of asking. Implied by --json.
    #[arg(long, global = true)]
    pub(crate) non_interactive: bool,

    /// Do not draw the live dashboard during a run; print plain progress lines
    /// instead. The dashboard is already off when output is redirected, under
    /// --json, and under --no-color; this is for a terminal that could show it
    /// but should not spend anything on it.
    #[arg(long, global = true, env = "DARCBENCH_NO_TUI")]
    pub(crate) no_tui: bool,

    /// Override the state directory (agent key, run artifacts).
    #[arg(long, global = true, env = "DARCBENCH_HOME")]
    pub(crate) home: Option<std::path::PathBuf>,

    /// Log verbosity: error, warn, info, debug, trace.
    #[arg(long, global = true, env = "DARCBENCH_LOG", default_value = "warn")]
    pub(crate) log: String,

    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

/// The reverse-proxy actions, in the order an operator should use them.
#[derive(Debug, Subcommand)]
pub(crate) enum ProxyCommand {
    /// Print the configuration that would be written. Writes nothing.
    ///
    /// Always run this first. It is the whole file, so you can read it, and it
    /// is the same bytes `apply` writes.
    Preview {
        /// nginx or apache.
        #[arg(long, default_value = "nginx")]
        server: String,

        /// Port the agent is listening on.
        #[arg(long, default_value_t = DEFAULT_PORT)]
        port: u16,

        /// URL prefix the dashboard will answer on.
        #[arg(long, default_value = "/darcbench/")]
        location: String,
    },

    /// Write the configuration and have the web server validate it.
    ///
    /// Stops there. It does not reload your server, because a written file
    /// changes nothing until the server re-reads it and that moment is yours
    /// to choose. If the server's own validator rejects the configuration, the
    /// file is removed and anything it displaced is put back before the error
    /// is printed.
    Apply {
        #[arg(long, default_value = "nginx")]
        server: String,

        #[arg(long, default_value_t = DEFAULT_PORT)]
        port: u16,

        #[arg(long, default_value = "/darcbench/")]
        location: String,

        /// Required. Without it this reports what it would do and exits.
        #[arg(long)]
        confirm: bool,
    },

    /// Remove the configuration DARCBench wrote, and restore what it displaced.
    ///
    /// Removes only what the receipt in the state directory records creating.
    /// A rollback that guessed would eventually delete a file somebody else
    /// put there.
    Rollback {
        /// Required. Without it this reports what it would remove and exits.
        #[arg(long)]
        confirm: bool,

        /// Remove the file even though something still includes it. Your
        /// server will fail to start at the next reload until you remove that
        /// line too.
        #[arg(long)]
        force: bool,
    },

    /// Ask the web server whether its live configuration is valid.
    ///
    /// Run this after adding the `include` line. `apply` can only prove the
    /// snippet parses on its own; this proves it parses where you put it.
    Verify {
        #[arg(long, default_value = "nginx")]
        server: String,
    },

    /// Report whether a DARCBench proxy configuration is installed.
    Status,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Check that this machine can run DARCBench, and report what it found.
    Doctor,

    /// Print the system inventory without running anything.
    Inspect {
        /// Include identifying fields (hostname, MAC addresses). Local only.
        #[arg(long)]
        include_sensitive: bool,
    },

    /// Serve the local dashboard and wait for a browser to drive it.
    Serve {
        /// Port to bind. Never 80, 443 or another well-known service port.
        #[arg(long, default_value_t = DEFAULT_PORT)]
        port: u16,

        /// Address to bind. Defaults to loopback; anything else is opt-in and
        /// warned about loudly.
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,

        /// Reuse a fixed access token instead of generating one per start.
        /// Intended for supervised services; a generated token is safer.
        #[arg(long, env = "DARCBENCH_TOKEN")]
        token: Option<String>,
    },

    /// Run a benchmark to completion on the command line.
    Run {
        /// quick, standard, deep, endurance, read-only, web.
        #[arg(long, default_value = "quick")]
        profile: String,

        /// Explicit module list. Forces the run to be non-standard.
        #[arg(long, value_delimiter = ',')]
        modules: Option<Vec<String>>,

        /// Proceed despite non-blocking preflight warnings.
        #[arg(long)]
        force: bool,

        /// Minutes an endurance run keeps cycling. Forces the run to be
        /// non-standard: a shorter run has had less time to decline.
        #[arg(long)]
        duration_minutes: Option<u32>,

        /// Write the result bundle here in addition to the state directory.
        #[arg(long)]
        output: Option<std::path::PathBuf>,
    },

    /// Host a benchmark origin for a load generator on another machine, and
    /// wait for its report.
    ///
    /// This is the machine being measured. It starts an HTTP origin on the
    /// interface you name, prints one ticket, and waits. Nothing is served to
    /// anyone who does not present the ticket's token, so the ticket is a
    /// secret: carry it to the generator machine over a channel you trust.
    ///
    /// It exists because a load generator sharing a machine with the server it
    /// is loading competes with it for CPU, which puts a floor under every
    /// latency figure `web.static` reports. See docs/ROADMAP.md, Phase 3.
    WebTarget {
        /// Address to listen on. Must be one of this host's own interfaces,
        /// and never a wildcard: 0.0.0.0 would expose the origin on networks
        /// you were not thinking about.
        #[arg(long)]
        bind: String,

        /// Terminate TLS, with a certificate generated for this session and
        /// carried in the ticket. Recommended on any network you do not own.
        #[arg(long)]
        tls: bool,

        /// Minutes to wait for the generator before giving up.
        #[arg(long, default_value_t = 10)]
        minutes: u64,
    },

    /// Generate load against a DARCBench target on another machine.
    ///
    /// This is the generator. It refuses to send load anywhere that did not
    /// give it a ticket, which is what keeps it from being a tool for loading
    /// somebody else's server - see T-AMPLIFY in docs/THREAT-MODEL.md. There
    /// is no way to give it a bare URL, and there will not be one.
    WebDrive {
        /// The ticket printed by `darcbench web-target` on the other machine.
        #[arg(long, env = "DARCBENCH_TICKET")]
        ticket: String,

        /// Requests per second to offer. The target may clamp it, and every
        /// clamp is reported rather than applied silently.
        #[arg(long, default_value_t = 5000.0)]
        rate: f64,

        /// Seconds to hold each shape.
        #[arg(long, default_value_t = 10)]
        seconds: u64,

        /// Concurrent connections.
        #[arg(long, default_value_t = 32)]
        connections: u32,
    },

    /// Generate, preview, install or roll back a reverse-proxy configuration
    /// for the dashboard.
    ///
    /// This is the only part of DARCBench that writes to your web server's
    /// configuration, and it is deliberately narrow: it adds one file whose
    /// name and directory are fixed at compile time, it never edits a file you
    /// wrote, and it never reloads your server. Removing the file it adds
    /// returns the server to exactly its previous behaviour.
    ///
    /// Refused outright on Plesk, cPanel and other panels that generate the
    /// web server's configuration themselves. Use the panel's own
    /// reverse-proxy feature there.
    #[command(subcommand)]
    Proxy(ProxyCommand),

    /// Show the most recent runs.
    Status {
        /// How many runs to list.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },

    /// Compare two runs metric by metric.
    ///
    /// Ratios are direction-adjusted, so above 1.0 always means the second run
    /// did better - whether the metric counts throughput or latency.
    Compare {
        /// The run to measure against.
        baseline: String,
        /// The run being judged.
        candidate: String,
    },

    /// Apply a retention policy to the stored runs.
    ///
    /// Deleting a benchmark result is not undoable and the bundle is the only
    /// copy, so this reports what it would remove unless `--confirm` is given,
    /// and it never removes an `Invalid` run: the reason a run failed is
    /// evidence.
    Prune {
        /// Remove runs that finished more than this many days ago.
        #[arg(long)]
        older_than_days: Option<u32>,

        /// Keep only this many runs, newest first.
        #[arg(long)]
        keep_last: Option<usize>,

        /// Actually delete. Without this, the command only reports.
        #[arg(long)]
        confirm: bool,
    },

    /// Print a stored result bundle or its HTML report.
    Report {
        /// Run id. Defaults to the most recent completed run.
        run_id: Option<String>,

        /// Emit the HTML report instead of the JSON bundle.
        #[arg(long)]
        html: bool,
    },

    /// Verify a stored bundle's signature and revalidate it.
    Verify {
        /// Path to a bundle.json.
        path: std::path::PathBuf,
    },

    /// Describe what an uninstall would remove, and optionally do it.
    Uninstall {
        /// Actually delete. Without this, the command only reports.
        #[arg(long)]
        confirm: bool,
    },
}

/// Minimal ANSI helper. Colour is opt-out, and never emitted when stdout is
/// not a terminal or when `--json` is in effect.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Style {
    enabled: bool,
}

impl Style {
    pub(crate) fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    /// Whether output is being decorated at all.
    ///
    /// This is the single signal for "stdout is an interactive terminal that
    /// wants escape sequences", so the live dashboard keys off the same fact
    /// as colour rather than re-deriving it and drifting from it.
    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn dim(&self, text: &str) -> String {
        self.wrap("\x1b[2m", text)
    }
    pub(crate) fn bold(&self, text: &str) -> String {
        self.wrap("\x1b[1m", text)
    }
    pub(crate) fn cyan(&self, text: &str) -> String {
        self.wrap("\x1b[36m", text)
    }
    pub(crate) fn green(&self, text: &str) -> String {
        self.wrap("\x1b[32m", text)
    }
    pub(crate) fn yellow(&self, text: &str) -> String {
        self.wrap("\x1b[33m", text)
    }
    pub(crate) fn red(&self, text: &str) -> String {
        self.wrap("\x1b[31m", text)
    }
    pub(crate) fn magenta(&self, text: &str) -> String {
        self.wrap("\x1b[35m", text)
    }

    /// Reverse video, for a badge that has to be found at a glance.
    ///
    /// Used only for a run's final verdict. It is deliberately rare: a screen
    /// where several things are shouting has nothing that stands out.
    pub(crate) fn invert(&self, text: &str) -> String {
        self.wrap("\x1b[7m", text)
    }

    fn wrap(&self, code: &str, text: &str) -> String {
        if self.enabled {
            format!("{code}{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }
}

/// Pads an already-decorated string to `width` display columns.
///
/// `plain` is the same text without escape sequences, and it is what the width
/// is measured against. This exists because `format!("{:<12}", decorated)`
/// counts the escape bytes as characters: a red twelve-character cell carries
/// nine invisible bytes, `format!` believes it is already over width, adds no
/// padding at all, and every column to its right drifts left by a different
/// amount on every row. The columns only stay aligned if the padding is
/// computed from the text a person can actually see.
///
/// Character count rather than byte length, so a `·` or a box-drawing glyph is
/// one column and not two or three.
pub(crate) fn pad(decorated: &str, plain: &str, width: usize) -> String {
    let visible = plain.chars().count();
    format!("{decorated}{}", " ".repeat(width.saturating_sub(visible)))
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn default_invocation_has_no_subcommand() {
        let cli = Cli::parse_from(["darcbench"]);
        assert!(cli.command.is_none());
        assert!(!cli.json);
    }

    #[test]
    fn serve_defaults_to_loopback_and_the_default_port() {
        let cli = Cli::parse_from(["darcbench", "serve"]);
        match cli.command {
            Some(Command::Serve { port, bind, token }) => {
                assert_eq!(port, DEFAULT_PORT);
                assert_eq!(bind, "127.0.0.1");
                assert!(token.is_none());
            }
            other => panic!("expected serve, got {other:?}"),
        }
    }

    #[test]
    fn run_accepts_a_comma_separated_module_list() {
        let cli = Cli::parse_from([
            "darcbench",
            "run",
            "--modules",
            "cpu.mixed,memory.bandwidth",
        ]);
        match cli.command {
            Some(Command::Run {
                modules, profile, ..
            }) => {
                assert_eq!(
                    modules,
                    Some(vec![
                        "cpu.mixed".to_string(),
                        "memory.bandwidth".to_string()
                    ])
                );
                assert_eq!(profile, "quick");
            }
            other => panic!("expected run, got {other:?}"),
        }
    }

    #[test]
    fn global_flags_work_after_the_subcommand() {
        let cli = Cli::parse_from(["darcbench", "run", "--json", "--no-color"]);
        assert!(cli.json);
        assert!(cli.no_color);
    }

    #[test]
    fn uninstall_requires_explicit_confirmation() {
        let cli = Cli::parse_from(["darcbench", "uninstall"]);
        match cli.command {
            Some(Command::Uninstall { confirm }) => assert!(!confirm),
            other => panic!("expected uninstall, got {other:?}"),
        }
    }

    /// The bug this guards: `format!("{:<12}", coloured)` counts escape bytes
    /// as visible characters, so a coloured cell gets no padding at all and
    /// every column to its right drifts.
    #[test]
    fn padding_is_measured_on_the_visible_text_not_the_escape_bytes() {
        let style = Style::new(true);
        let coloured = style.red("Invalid");
        assert!(
            coloured.len() > "Invalid".len(),
            "precondition: escapes exist"
        );

        let padded = pad(&coloured, "Invalid", 12);
        assert!(padded.starts_with(&coloured));
        assert_eq!(
            padded.len() - coloured.len(),
            5,
            "12 columns minus 7 visible characters is 5 spaces"
        );
    }

    #[test]
    fn padding_never_truncates_text_wider_than_its_column() {
        let padded = pad("SelfReported", "SelfReported", 4);
        assert_eq!(padded, "SelfReported", "a column may be overrun, never cut");
    }

    /// Multi-byte characters are one column each, not one per byte.
    #[test]
    fn padding_counts_characters_rather_than_bytes() {
        let padded = pad("·····", "·····", 7);
        assert_eq!(padded.chars().count(), 7);
    }

    #[test]
    fn style_is_a_no_op_when_disabled() {
        let plain = Style::new(false);
        assert_eq!(plain.cyan("hello"), "hello");
        assert_eq!(plain.bold("x"), "x");

        let coloured = Style::new(true);
        assert!(coloured.cyan("hello").contains("\x1b[36m"));
        assert!(coloured.cyan("hello").ends_with("\x1b[0m"));
    }
}
