//! Network interfaces and link characteristics.
//!
//! MAC addresses and IP addresses are identifying, so they are either not
//! collected at all or wrapped in [`Sensitive`]. A negotiated link speed is
//! not identifying and is essential context for any network score - a 10 Gbit/s
//! result on a 1 Gbit/s port is a measurement error, not a fast server.

use serde::{Deserialize, Serialize};

use crate::{read_file, read_parse, Gap, Sensitive};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkInfo {
    pub interfaces: Vec<Interface>,
    /// Name of the interface carrying the default route, when determinable.
    pub primary_interface: Option<String>,
    pub ipv6_enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interface {
    pub name: String,
    /// Negotiated link speed in Mbit/s. `None` for virtual interfaces and for
    /// most virtio NICs, which do not report one.
    pub speed_mbps: Option<u32>,
    pub mtu: Option<u32>,
    /// `up`, `down`, `unknown`.
    pub operstate: String,
    /// True for loopback, bridge, veth, docker and other virtual devices.
    pub virtual_device: bool,
    /// Present only so a local operator can identify the NIC; redacted by
    /// default because it is a stable hardware identifier.
    pub mac: Sensitive<String>,
}

impl NetworkInfo {
    pub fn collect(gaps: &mut Vec<Gap>) -> Self {
        let Ok(entries) = std::fs::read_dir("/sys/class/net") else {
            gaps.push(Gap {
                field: "network.interfaces".into(),
                reason: "/sys/class/net unreadable".into(),
            });
            return Self {
                interfaces: Vec::new(),
                primary_interface: None,
                ipv6_enabled: false,
            };
        };

        let mut interfaces: Vec<Interface> = entries
            .filter_map(Result::ok)
            .map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                let base = format!("/sys/class/net/{name}");
                Interface {
                    // Reading `speed` on a down interface can log a kernel
                    // warning, so it is only read when the link is up.
                    speed_mbps: read_file(&format!("{base}/operstate"))
                        .filter(|s| s.trim() == "up")
                        .and_then(|_| read_parse::<i64>(&format!("{base}/speed")))
                        .filter(|v| *v > 0)
                        .map(|v| v as u32),
                    mtu: read_parse(&format!("{base}/mtu")),
                    operstate: read_file(&format!("{base}/operstate"))
                        .map(|s| s.trim().to_string())
                        .unwrap_or_else(|| "unknown".into()),
                    virtual_device: is_virtual(&name, &base),
                    mac: Sensitive::new(
                        read_file(&format!("{base}/address"))
                            .map(|s| s.trim().to_string())
                            .unwrap_or_default(),
                    ),
                    name,
                }
            })
            .collect();
        interfaces.sort_by(|a, b| a.name.cmp(&b.name));

        Self {
            primary_interface: default_route_interface(),
            ipv6_enabled: std::path::Path::new("/proc/net/if_inet6").exists(),
            interfaces,
        }
    }
}

fn is_virtual(name: &str, base: &str) -> bool {
    if name == "lo" {
        return true;
    }
    for prefix in [
        "veth", "docker", "br-", "virbr", "tun", "tap", "wg", "cni", "flannel", "cali",
    ] {
        if name.starts_with(prefix) {
            return true;
        }
    }
    // A physical NIC has a `device` symlink into the PCI/USB tree.
    !std::path::Path::new(&format!("{base}/device")).exists()
}

/// Finds the interface with the default route by parsing `/proc/net/route`.
///
/// A destination of `00000000` with the `RTF_UP|RTF_GATEWAY` flags is the
/// default route. Parsed directly rather than by running `ip route`.
fn default_route_interface() -> Option<String> {
    let raw = read_file("/proc/net/route")?;
    raw.lines()
        .skip(1)
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            let iface = f.next()?;
            let destination = f.next()?;
            (destination == "00000000").then(|| iface.to_string())
        })
        .next()
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;

    #[test]
    fn network_collects_interfaces() {
        let mut gaps = Vec::new();
        let n = NetworkInfo::collect(&mut gaps);
        assert!(
            n.interfaces.iter().any(|i| i.name == "lo"),
            "loopback should always exist"
        );
    }

    #[test]
    fn mac_addresses_are_redacted_by_default() {
        let mut gaps = Vec::new();
        let n = NetworkInfo::collect(&mut gaps);
        let json = serde_json::to_value(&n).expect("ser");
        for iface in json["interfaces"].as_array().expect("array") {
            assert_eq!(iface["mac"], crate::redact::REDACTED);
        }
    }

    #[test]
    fn virtual_device_classification() {
        assert!(is_virtual("lo", "/nonexistent"));
        assert!(is_virtual("docker0", "/nonexistent"));
        assert!(is_virtual("veth1a2b3c", "/nonexistent"));
        // No `device` symlink at a bogus path, so this is treated as virtual -
        // the conservative direction, since claiming a link speed for a device
        // we cannot confirm is physical would be worse.
        assert!(is_virtual("eth0", "/nonexistent"));
    }
}
