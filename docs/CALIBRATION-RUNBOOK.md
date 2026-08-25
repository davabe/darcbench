# Calibration runbook

The operational half of [SCORING-SYSTEM.md §3.1](SCORING-SYSTEM.md), which
defines *what* calibration is. This is *how to actually do it*: what to rent,
what to install, what to type, and what to send back.

Calibration is the single thing gating `dbs/1.0.0`, and through it every public
score and the leaderboard in [PUBLIC-RESULTS.md](PUBLIC-RESULTS.md). It cannot
be done by writing more code.

---

## 1. What to provision

**Three physically distinct hosts, from at least two vendors.** Two vendors is
not bureaucracy: one vendor's BIOS defaults, firmware or cooling design is a
property of that vendor, and a reference built on it would encode that vendor's
quirks as "normal".

Target specification — DARC-REF-1, chosen because it is close to the median
dedicated server people actually buy, so a score of 1000 means "as fast as a
normal good web server" rather than an arbitrary index:

| Component | Specification |
|---|---|
| CPU | 8 physical cores / 16 threads, x86-64-v3, ~3.8 GHz sustained all-core |
| Memory | 64 GB DDR5-4800 ECC, dual channel |
| Storage | 2 × NVMe PCIe 4.0 datacenter SSD, software RAID 1, ext4, `relatime` |
| Network | 1 Gbit/s symmetric, unmetered |
| OS | Debian 12, kernel 6.1 LTS, `performance` governor, default mitigations |

Concretely: **Hetzner AX52** (Ryzen 7 7700) is the class this was specified
from. A second vendor with a comparable Ryzen 7 7700 / EPYC 4344P box —
OVH, Netcup, Servers.com, or a local provider — satisfies "at least two".

**Do not use VPS instances.** The whole point is a machine whose CPU nobody else
is sharing. A noisy neighbour on the reference machine would be baked into every
score DARCBench ever produces.

### Cost sanity check

Three AX52-class boxes for one month is roughly the cheapest part of this
project. The 30 runs below take on the order of **20–30 machine-hours per host**
including the mandated idle gaps, so a month is generous and leaves room for
the re-runs step 5 will demand.

---

## 2. Prepare each host

```bash
# 1. Confirm the machine is what you ordered.
lscpu | grep -E 'Model name|^CPU\(s\)|Thread|MHz'
free -g
lsblk -d -o NAME,MODEL,SIZE,ROTA
uname -r; cat /etc/debian_version

# 2. performance governor on every core. This is mandatory: a machine that
#    clocks down between runs measures its cooling, not its silicon.
sudo apt-get install -y linux-cpupower
sudo cpupower frequency-set -g performance
cpupower frequency-info | grep -i 'governor'

# 3. Record what the run cannot see for itself. Send this file with the bundles.
{
  echo "== host =="; hostnamectl
  echo "== cpu ==";  lscpu
  echo "== microcode =="; grep -m1 microcode /proc/cpuinfo
  echo "== firmware =="; sudo dmidecode -t bios 2>/dev/null | head -20
  echo "== memory =="; sudo dmidecode -t memory 2>/dev/null | grep -E 'Size|Speed|Type:|Rank'
  echo "== storage =="; lsblk -o NAME,MODEL,SIZE,ROTA,FSTYPE,MOUNTPOINT
  echo "== raid =="; cat /proc/mdstat 2>/dev/null
  echo "== mount =="; findmnt -no SOURCE,FSTYPE,OPTIONS /
  echo "== governor =="; cpupower frequency-info 2>/dev/null | head -20
} > host-facts.txt

# 4. Quiet the machine. Anything running during a run is measured as competition,
#    and the load ceiling will degrade or stop the run - correctly.
sudo systemctl stop unattended-upgrades apt-daily.timer apt-daily-upgrade.timer
sudo systemctl disable --now man-db.timer 2>/dev/null || true
```

`deep` needs a container runtime for the database and WordPress modules. Without
it those modules report themselves *not measured*, which is honest but leaves
four categories unanchored — so install it:

```bash
sudo apt-get install -y docker.io
sudo systemctl enable --now docker
sudo docker info >/dev/null && echo "daemon reachable"
```

---

## 3. Get the binary onto the host

**Use the static musl build.** Not a convenience: a binary built against glibc
2.39 will not start on Debian 12, whose glibc is 2.36, and a benchmark that has
to be compiled per host is a benchmark whose compiler version is part of the
measurement.

```bash
scp darcbench root@<host>:/usr/local/bin/darcbench
ssh root@<host> 'chmod +x /usr/local/bin/darcbench && darcbench --version'
```

Then, on the host, confirm the binary is the one you think it is and that it
agrees with the machine:

```bash
sha256sum /usr/local/bin/darcbench     # must match the hash shipped with it
darcbench doctor                        # must end: PASS ready to benchmark
```

`doctor` is not ceremony. It reports the estimated disk writes, the risk class
and anything it found wrong with the host, and a `PASS` here is the precondition
for a run whose numbers are worth keeping.

---

## 4. The runs

**Ten `deep` runs per host, with at least 30 minutes idle between them.**

The gap is the part people skip and must not. A machine that has just run a
30-minute deep profile is hot, its page cache is full and its SSD is still
flushing. Starting the next run immediately measures the tail of the previous
one.

```bash
export DARCBENCH_HOME=/var/lib/darcbench
mkdir -p "$DARCBENCH_HOME"

for i in $(seq 1 10); do
  echo "=== run $i/10 on $(hostname) at $(date -Is) ==="
  darcbench run --profile deep --json > "run-$i.json" 2> "run-$i.err" || echo "RUN $i FAILED"
  [ "$i" -lt 10 ] && sleep 1800
done
```

Run it under `tmux` or `screen`; the whole sequence is many hours and an SSH
drop mid-run wastes one.

**Do not pass `--force`.** `--force` proceeds past non-blocking preflight
warnings, and during calibration a warning is information you need rather than
an obstacle. If a run needs `--force` to start, find out why first.

**Do not pass `--modules`.** An explicit module list makes the run `Custom`,
and `Custom` is never comparable — the calibration would be unusable.

---

## 5. What to collect and send

Per host, one archive:

```bash
tar czf calib-$(hostname)-$(date +%Y%m%d).tar.gz \
    host-facts.txt \
    run-*.err \
    "$DARCBENCH_HOME"/runs/*/bundle.json
```

That is everything. **The bundle is the deliverable** — it is signed and
self-contained, carrying every raw metric, every repetition, the full machine
inventory and the verdict. Roughly 100–300 KB per `deep` run, so thirty of them
compress to a few megabytes.

Not needed: `events.ndjson` (the live stream, useful for debugging a run and
nothing else), `report.html` (regenerable from the bundle), or shell access.

### Before you send, check three things

```bash
# Every bundle verifies. A bundle that does not is evidence of a problem on the
# host, not something to quietly drop.
for b in "$DARCBENCH_HOME"/runs/*/bundle.json; do darcbench verify "$b" || echo "BAD: $b"; done

# Every run is `standard`-eligible, not Custom or Partial.
grep -ho '"state":"[a-z]*"' run-*.json | sort | uniq -c

# The build target is the same on all three hosts.
grep -ho '"build_target":"[^"]*"' run-*.json | sort -u
```

That last check matters more than it looks. `build_target` is a comparability
key, and a musl binary on two hosts and a source-built gnu binary on the third
would produce a reference blended from two different libc implementations —
musl and glibc differ in allocator and `memcpy`, which is exactly what
`cpu.mixed` and `memory.bandwidth` measure.

---

## 6. What happens with them

1. Verify every signature and recompute every score from the raw metrics.
2. Per host, take the median of each metric across its ten runs.
3. Across hosts, take the median of those per-host medians.
4. **Reject any metric whose across-host CV exceeds 5%** and investigate before
   proceeding. This is the step that catches a bad host, a thermally limited
   chassis, or a metric that is not stable enough to anchor anything.
5. Write the results into `reference::provisional_reference`, set
   `calibrated: true`, bump to `dbs/1.0.0`.
6. **Publish the raw calibration bundles alongside the release.** A reference
   nobody can check is not a reference.

Step 4 is why ten runs per host and not three. It is also why a failed run is
worth sending: a metric that is unstable on one host and steady on the other two
is a finding about that host.

---

## 7. Known conditions to expect

**`web.static` will score far above 1000 on a large host.** Measured on a
72-thread development machine: Web 6745, with `throughput.large` at 75 GiB/s
over loopback — which is a memcpy, not a web server. On a 16-thread DARC-REF-1
this will be far closer to the anchors, and that is the point of anchoring on a
machine people buy. It is on the backlog as a shape question independent of
calibration.

**`network.transfer` depends on somebody else's network.** Its across-host CV
will be the worst of any module, and the two vendors' transit will differ. Step
4 exists to catch that; expect network metrics to need the most investigation.

**A `deep` run needs a container daemon.** Without one, `database.oltp`,
`database.cache`, `wordpress.site` and `deployment.container` report themselves
not measured. Check `docker info` before starting a ten-run sequence rather than
after it.
