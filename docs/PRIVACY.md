# Privacy

## Principle

Benchmark results get shared — in tickets, forums, blog posts and leaderboards.
A hostname or public IP in a shared report is a permanent disclosure the person
sharing it did not intend to make.

So redaction is **the default, enforced by the type system**, not a checkbox
somebody has to remember.

## How it is enforced

Identifying values are wrapped in `Sensitive<T>`. Its `Serialize` implementation
emits `[redacted]` unless a thread-scoped policy says otherwise, and its `Debug`
does the same so a value cannot leak through a log line.

```rust
pub hostname: Sensitive<String>,
pub mac: Sensitive<String>,
```

The consequence that matters: **the failure mode of forgetting to think about
privacy is over-redaction**, not a leaked hostname on a public page. Revealing
requires calling `expose()` — a deliberate, greppable act — or entering an
explicit `with_policy(Reveal, …)` scope.

## What is collected

| Category | Collected | Default in output |
|---|---|---|
| CPU model, topology, cache, flags, governor | ✅ | Visible |
| Memory size, swap configuration | ✅ | Visible |
| Storage devices, models, transport, filesystem, mount options | ✅ | Visible |
| Network link speed, MTU, operstate | ✅ | Visible |
| OS, kernel, distribution | ✅ | Visible |
| Virtualization, container runtime, cgroup limits | ✅ | Visible |
| Installed panels, web servers, databases, runtimes | ✅ | Visible |
| Listening port numbers | ✅ | Visible |
| DMI vendor and product name | ✅ | Visible |
| **Hostname** | ✅ | **Redacted** |
| **MAC addresses** | ✅ | **Redacted** |
| **IP addresses** | Only where needed | **Redacted / coarsened to /24 or /32** |
| **DMI serial numbers, chassis and product UUIDs** | ❌ **Never collected** | — |
| **Cloud instance ids, account ids** | ❌ **Never collected** | — |
| **Site content, database contents, customer data** | ❌ **Never** | — |
| **Environment variables, credentials, config file contents** | ❌ **Never** | — |
| **Usernames, paths inside home directories** | ❌ **Never** | — |

Listening port *numbers* are collected because they determine which ports the
agent may bind and are a production signal. Which process owns them is not
collected.

## No cloud metadata queries

DARCBench never contacts `169.254.169.254` or any equivalent. Those endpoints
return IAM credentials, and a benchmark tool with a habit of reading them is a
credential-harvesting tool waiting for a bug. Cloud platform is inferred from
DMI strings only.

## Run identifiers

`run_` plus 128 bits of CSPRNG output. **Not derived from any host property** —
not the hostname, not a MAC, not a machine id. Two runs on the same machine
cannot be linked by their ids alone.

The `environment_digest` is a SHA-256 over performance-relevant facts only (CPU
model, topology, memory size, kernel, virtualization, cgroup limits). It
deliberately excludes both volatile values and identifying ones, which is what
makes it safe to publish while still detecting a machine changing mid-run.

## Revealing

```bash
darcbench inspect --include-sensitive      # local operator, explicit
```

Over the API, `?include_sensitive=true` is honoured **only on a loopback bind**.
Over a tunnel or a public listener the answer is always redacted, because the
agent cannot know who is at the other end.

## Standalone mode

**Sends nothing about you or your machine, anywhere.** No telemetry, no update
check, no analytics, no crash reporting. The agent is useful, complete and
silent.

One qualification, added when `network.transfer` shipped: a benchmark that
measures a network has to use one. That module opens connections to a
**compile-time allow-list** of public measurement endpoints — the table lives in
`crates/darcbench-modules/src/network_endpoints.rs` and there is no
configuration file, environment variable or API field that can add to it. What
those hosts see is an ordinary HTTPS client asking for a fixed number of padding
bytes. They are sent no inventory, no measurement, no run id, no installation
id, no hostname and nothing that distinguishes one DARCBench user from another;
what they can observe is what any web server observes about any client, namely
its IP address and the time it connected.

The volume is bounded by a ceiling the module enforces against a running total,
and preflight names both the volume and every operator before the run starts.
The `quick` profile excludes the module entirely, so the first run anyone makes
on an unfamiliar server opens no outbound connection at all.

## Connected mode (Phase 5)

Opt-in, per run. What is uploaded is exactly the bundle, already redacted at
serialisation time. Additionally:

- Public sharing is a separate, explicit action; uploading is not publishing.
- Unpublishing removes the public page. The underlying evidence is retained for
  audit — a leaderboard where invalidated results vanish is a leaderboard nobody
  can check.
- Provider attribution is user-supplied. DARCBench does not infer "this is
  customer X at provider Y" from anything.

## Data subject rights (Phase 5)

Export, deletion of account and published reports, and correction. Retention of
anonymised aggregate results after account deletion will be stated explicitly in
the privacy policy before the control plane launches — not decided afterwards.

## Verified in tests

- `redaction_is_the_default`
- `reveal_is_opt_in_and_scoped`
- `debug_output_is_also_redacted`
- `nested_policy_restores_the_outer_value`
- `hostname_is_not_serialised_by_default`
- `mac_addresses_are_redacted_by_default`
- `performance_digest_ignores_volatile_and_identifying_values`

Confirmed live: a bundle fetched over the API renders `hostname: [redacted]` and
`mac: [redacted]`.
