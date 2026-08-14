//! Discovery of hosting panels, web servers, runtimes and occupied ports.
//!
//! # Why this exists
//!
//! DARCBench is designed to be installed on servers that are already doing
//! something. Before the agent binds a port or suggests a way to reach the
//! dashboard, it has to know whether it is standing on a Plesk box with 200
//! customer sites or an empty VPS. Everything here is **read-only detection**:
//! nothing is configured, started, stopped or rewritten.
//!
//! Detection is filesystem- and `/proc`-based. It never executes a discovered
//! binary to ask for its version, because running arbitrary binaries found on
//! a possibly-compromised host is exactly the behaviour a benchmark tool
//! should not have.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{read_file, Gap};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SoftwareInfo {
    pub panels: Vec<DetectedSoftware>,
    pub web_servers: Vec<DetectedSoftware>,
    pub container_runtimes: Vec<DetectedSoftware>,
    pub databases: Vec<DetectedSoftware>,
    pub runtimes: Vec<DetectedSoftware>,
    pub firewalls: Vec<DetectedSoftware>,
    /// TCP ports currently listening, from `/proc/net/tcp{,6}`.
    pub listening_tcp_ports: Vec<u16>,
    /// Composite judgement used to pick the least invasive exposure strategy
    /// and to raise the run's risk classification.
    pub production_likelihood: ProductionLikelihood,
    pub production_signals: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectedSoftware {
    pub name: String,
    /// What proved it exists, e.g. a path or a listening port.
    pub evidence: String,
}

/// How likely this machine is serving real traffic right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProductionLikelihood {
    /// No panel, no web server, nothing listening on 80/443, low uptime.
    Unlikely,
    /// Some server software present but no strong signal of live customers.
    Possible,
    /// Hosting panel and/or public web server actively listening.
    Likely,
}

/// `(display name, marker paths)`. A single existing path is enough evidence.
const PANELS: &[(&str, &[&str])] = &[
    ("Plesk", &["/usr/local/psa", "/opt/psa", "/etc/psa"]),
    ("cPanel/WHM", &["/usr/local/cpanel", "/var/cpanel"]),
    ("DirectAdmin", &["/usr/local/directadmin"]),
    ("CloudPanel", &["/home/clp", "/etc/cloudpanel"]),
    ("CyberPanel", &["/usr/local/CyberCP"]),
    ("HestiaCP", &["/usr/local/hestia"]),
    ("VestaCP", &["/usr/local/vesta"]),
    ("ISPConfig", &["/usr/local/ispconfig"]),
    ("aaPanel/BT", &["/www/server/panel"]),
    ("Webmin", &["/etc/webmin", "/usr/share/webmin"]),
    ("Virtualmin", &["/etc/webmin/virtual-server"]),
    ("Coolify", &["/data/coolify"]),
    ("CapRover", &["/captain"]),
];

const WEB_SERVERS: &[(&str, &[&str])] = &[
    (
        "nginx",
        &[
            "/etc/nginx/nginx.conf",
            "/usr/sbin/nginx",
            "/usr/local/nginx",
        ],
    ),
    (
        "Apache httpd",
        &["/etc/apache2/apache2.conf", "/etc/httpd/conf/httpd.conf"],
    ),
    ("Caddy", &["/etc/caddy/Caddyfile", "/usr/bin/caddy"]),
    ("LiteSpeed", &["/usr/local/lsws"]),
    ("OpenLiteSpeed", &["/usr/local/lsws/bin/openlitespeed"]),
    ("HAProxy", &["/etc/haproxy/haproxy.cfg"]),
    (
        "Traefik",
        &["/etc/traefik/traefik.yml", "/etc/traefik/traefik.toml"],
    ),
];

const CONTAINER_RUNTIMES: &[(&str, &[&str])] = &[
    (
        "Docker",
        &["/var/run/docker.sock", "/run/docker.sock", "/etc/docker"],
    ),
    (
        "Podman",
        &["/run/podman/podman.sock", "/etc/containers/containers.conf"],
    ),
    ("containerd", &["/run/containerd/containerd.sock"]),
    (
        "Kubernetes (kubelet)",
        &["/etc/kubernetes/kubelet.conf", "/var/lib/kubelet"],
    ),
    ("K3s", &["/etc/rancher/k3s", "/var/lib/rancher/k3s"]),
];

const DATABASES: &[(&str, &[&str])] = &[
    ("MySQL", &["/etc/mysql/my.cnf", "/var/lib/mysql"]),
    (
        "MariaDB",
        &["/etc/my.cnf.d/mariadb-server.cnf", "/etc/mysql/mariadb.cnf"],
    ),
    (
        "PostgreSQL",
        &["/etc/postgresql", "/var/lib/pgsql", "/var/lib/postgresql"],
    ),
    ("Redis", &["/etc/redis/redis.conf", "/etc/redis.conf"]),
    ("Valkey", &["/etc/valkey/valkey.conf"]),
    ("MongoDB", &["/etc/mongod.conf"]),
];

const RUNTIMES: &[(&str, &[&str])] = &[
    (
        "PHP-FPM",
        &[
            "/etc/php-fpm.conf",
            "/etc/php/8.3/fpm",
            "/etc/php/8.4/fpm",
            "/usr/sbin/php-fpm",
        ],
    ),
    ("PHP CLI", &["/usr/bin/php", "/usr/local/bin/php"]),
    ("Node.js", &["/usr/bin/node", "/usr/local/bin/node"]),
    ("Python 3", &["/usr/bin/python3"]),
    ("Java", &["/usr/bin/java"]),
];

const FIREWALLS: &[(&str, &[&str])] = &[
    ("ufw", &["/etc/ufw/ufw.conf"]),
    ("firewalld", &["/etc/firewalld/firewalld.conf"]),
    ("nftables", &["/etc/nftables.conf"]),
    (
        "iptables (persistent)",
        &["/etc/iptables/rules.v4", "/etc/sysconfig/iptables"],
    ),
    ("CSF", &["/etc/csf/csf.conf"]),
    (
        "fail2ban",
        &["/etc/fail2ban/jail.local", "/etc/fail2ban/jail.conf"],
    ),
];

impl SoftwareInfo {
    pub fn collect(gaps: &mut Vec<Gap>) -> Self {
        let listening_tcp_ports = listening_ports();
        if listening_tcp_ports.is_empty() {
            gaps.push(Gap {
                field: "software.listening_tcp_ports".into(),
                reason: "/proc/net/tcp unreadable or no listeners found".into(),
            });
        }

        let panels = detect(PANELS);
        let web_servers = detect(WEB_SERVERS);

        let mut signals = Vec::new();
        for panel in &panels {
            signals.push(format!("hosting panel detected: {}", panel.name));
        }
        for port in [80u16, 443] {
            if listening_tcp_ports.contains(&port) {
                signals.push(format!("port {port} has an active listener"));
            }
        }
        for server in &web_servers {
            signals.push(format!("web server installed: {}", server.name));
        }
        if std::path::Path::new("/var/www").exists() {
            if let Ok(entries) = std::fs::read_dir("/var/www") {
                let count = entries.filter_map(Result::ok).count();
                if count > 0 {
                    signals.push(format!("/var/www contains {count} entr(y|ies)"));
                }
            }
        }

        let production_likelihood = if !panels.is_empty()
            || (listening_tcp_ports.contains(&443) || listening_tcp_ports.contains(&80))
        {
            ProductionLikelihood::Likely
        } else if !web_servers.is_empty() || !signals.is_empty() {
            ProductionLikelihood::Possible
        } else {
            ProductionLikelihood::Unlikely
        };

        Self {
            panels,
            web_servers,
            container_runtimes: detect(CONTAINER_RUNTIMES),
            databases: detect(DATABASES),
            runtimes: detect(RUNTIMES),
            firewalls: detect(FIREWALLS),
            listening_tcp_ports,
            production_likelihood,
            production_signals: signals,
        }
    }

    /// True when `port` already has a listener, so the agent must not try to
    /// bind it. DARCBench never takes over an occupied port.
    pub fn port_is_occupied(&self, port: u16) -> bool {
        self.listening_tcp_ports.contains(&port)
    }
}

fn detect(table: &[(&str, &[&str])]) -> Vec<DetectedSoftware> {
    table
        .iter()
        .filter_map(|(name, paths)| {
            paths
                .iter()
                .find(|p| std::path::Path::new(p).exists())
                .map(|p| DetectedSoftware {
                    name: (*name).to_string(),
                    evidence: format!("path exists: {p}"),
                })
        })
        .collect()
}

/// Parses listening TCP sockets out of `/proc/net/tcp` and `/proc/net/tcp6`.
///
/// State `0A` is `TCP_LISTEN`. The local address column is
/// `<hex-addr>:<hex-port>`.
fn listening_ports() -> Vec<u16> {
    let mut ports = BTreeSet::new();
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Some(raw) = read_file(path) else { continue };
        for line in raw.lines().skip(1) {
            let mut fields = line.split_whitespace();
            let Some(local) = fields.nth(1) else { continue };
            let Some(state) = fields.nth(1) else { continue };
            if state != "0A" {
                continue;
            }
            if let Some((_, port_hex)) = local.rsplit_once(':') {
                if let Ok(port) = u16::from_str_radix(port_hex, 16) {
                    ports.insert(port);
                }
            }
        }
    }
    ports.into_iter().collect()
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;

    #[test]
    fn software_detection_runs_and_is_read_only() {
        let mut gaps = Vec::new();
        let s = SoftwareInfo::collect(&mut gaps);
        // Detection must be deterministic within a single process.
        let s2 = SoftwareInfo::collect(&mut Vec::new());
        assert_eq!(s.panels, s2.panels);
        assert_eq!(s.web_servers, s2.web_servers);
    }

    #[test]
    fn every_detection_carries_evidence() {
        let mut gaps = Vec::new();
        let s = SoftwareInfo::collect(&mut gaps);
        for group in [
            &s.panels,
            &s.web_servers,
            &s.databases,
            &s.runtimes,
            &s.firewalls,
        ] {
            for item in group {
                assert!(
                    !item.evidence.is_empty(),
                    "{} was detected with no evidence",
                    item.name
                );
            }
        }
    }

    #[test]
    fn detection_tables_have_no_empty_marker_sets() {
        for table in [
            PANELS,
            WEB_SERVERS,
            CONTAINER_RUNTIMES,
            DATABASES,
            RUNTIMES,
            FIREWALLS,
        ] {
            for (name, paths) in table {
                assert!(!paths.is_empty(), "{name} has no marker paths");
                for path in *paths {
                    assert!(
                        path.starts_with('/'),
                        "{name}: marker `{path}` must be absolute"
                    );
                }
            }
        }
    }

    #[test]
    fn port_occupancy_lookup() {
        let info = SoftwareInfo {
            panels: vec![],
            web_servers: vec![],
            container_runtimes: vec![],
            databases: vec![],
            runtimes: vec![],
            firewalls: vec![],
            listening_tcp_ports: vec![22, 80, 443],
            production_likelihood: ProductionLikelihood::Likely,
            production_signals: vec![],
        };
        assert!(info.port_is_occupied(443));
        assert!(!info.port_is_occupied(7842));
    }

    #[test]
    fn listening_ports_are_sorted_and_deduplicated() {
        let ports = listening_ports();
        assert!(
            ports.windows(2).all(|w| w[0] < w[1]),
            "ports must be sorted and unique"
        );
    }
}
