# Installer and discovery

The governing assumption: **the machine is already doing something.** The likely
host is a production web server with customer sites on it. Everything here
follows from refusing to damage that.

## Discovery — read-only, always

Before anything is bound or written, the agent inspects the host. Detection is
filesystem existence checks and `/proc` reads only. **A discovered binary is
never executed to ask for its version** — running arbitrary binaries found on a
possibly-compromised host is not behaviour a benchmark tool should have.

**Detected:** Plesk, cPanel/WHM, DirectAdmin, CloudPanel, CyberPanel, HestiaCP,
VestaCP, ISPConfig, aaPanel, Webmin, Virtualmin, Coolify, CapRover · nginx,
Apache, Caddy, LiteSpeed, OpenLiteSpeed, HAProxy, Traefik · Docker, Podman,
containerd, Kubernetes, K3s · MySQL, MariaDB, PostgreSQL, Redis, Valkey,
MongoDB · PHP-FPM, PHP CLI, Node.js, Python, Java · ufw, firewalld, nftables,
iptables, CSF, fail2ban · listening TCP ports from `/proc/net/tcp{,6}` ·
SELinux/AppArmor · virtualization and container scope with evidence · cgroup CPU
and memory limits · storage stack (RAID, LVM, ZFS, virtio, network-attached).

Every detection carries its evidence (`path exists: /usr/local/psa`), so a claim
about the machine can be checked rather than believed.

**Cloud platform is inferred from DMI strings only.** DARCBench never queries a
metadata endpoint — those responses carry credentials, and an SSRF-shaped read
is not something a benchmark should perform. DMI serial numbers and UUIDs are
never collected at all.

## Production likelihood

| Level | Signals |
|---|---|
| `Likely` | A hosting panel is installed, or port 80/443 has a listener |
| `Possible` | A web server is installed, or `/var/www` is non-empty |
| `Unlikely` | None of the above |

Combined with the highest module safety class, this produces the `RiskClass`
shown before every run. Anything above observational on a `Likely` machine is
`ProductionRisk` and will not start unattended.

## Exposure hierarchy

Preference order. The installer picks the **least invasive option that works**
and explains what it chose.

1. **Loopback service (default).** `127.0.0.1:7842`, token-authenticated.
   Nothing on the network can reach it.
2. **SSH tunnel.** The recommended remote path. The agent prints the exact
   command: `ssh -N -L 7842:127.0.0.1:7842 user@server`. No firewall change, no
   new exposure, authentication already solved by SSH.
3. **Temporary public listener.** Opt-in, explicit, high-entropy token, loud
   warning that the token travels in clear over plain HTTP. Never the default.
4. **Existing reverse proxy integration.** ⏳ Phase 3. Must generate the config,
   **preview it**, validate it with the server's own syntax checker, back up the
   original, and offer a tested rollback — before activation.
5. **DARCBench-controlled subdomain / tunnel provider.** ⏳ Later, and only as an
   explicit user choice.

**Absolute rules, enforced in code:**

- The dashboard is **never** exposed without authentication, on any interface.
- Ports 80, 443, 22, 21, 25, 3306, 5432, 6379 and 8443 are hard-blocked
  regardless of flags.
- Privileged ports (<1024) are refused.
- A port that already has a listener is refused, not raced.
- No existing web server is ever restarted.
- No existing virtual host is ever altered "for convenience".
- Docker-published ports must not bypass host firewall rules — the agent binds
  the host directly rather than publishing through a container.

## Ports

Default **7842**: high, unusual, unlikely to collide with anything a hosting
server runs. `--port` accepts anything not in the forbidden list and above 1023.

## Privileges

The agent **does not need root** for the modules in this build, and `doctor`
says so when it detects uid 0. Phase 2+ modules that need elevated capabilities
will use a small privileged helper with the unprivileged web/API process
separated from it — never a single root process serving HTTP.

## State directory

Resolution order: `$DARCBENCH_HOME` → `$XDG_STATE_HOME/darcbench` →
`$HOME/.local/state/darcbench` → `/var/lib/darcbench`.

Never `/tmp`: the agent's signing key and result bundles live here, and a
world-writable location is not acceptable for either.

Every write goes through `StatePath::join`, which rejects `.`, `..`, path
separators and NUL, and asserts the result stays under the root.

## Installation

**Today (from source):**
```bash
git clone https://github.com/davabe/darcbench && cd darcbench
(cd apps/web && pnpm install && pnpm build)   # optional
cargo build --release
```

**Phase 8 (packages):** Debian and RPM packages, a container image, a standalone
binary archive, and an install script.

The install script will **not** be a bare `curl … | sh`. It will print what it is
about to do, and the documentation will lead with the verifiable alternative:

```bash
curl -fsSLO https://get.darcbench.io/darcbench-linux-amd64.tar.gz
curl -fsSLO https://get.darcbench.io/darcbench-linux-amd64.tar.gz.sig
cosign verify-blob --signature darcbench-linux-amd64.tar.gz.sig ...
tar xzf darcbench-linux-amd64.tar.gz && ./darcbench doctor
```

Checksums and signatures published for every artifact.

## Uninstall

```bash
darcbench uninstall            # reports what it would remove
darcbench uninstall --confirm  # removes it
```

Uninstall is trivial precisely because the agent only ever creates files inside
its state directory. There is no configuration to undo, because none was ever
made.
