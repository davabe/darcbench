# Operations

## Running the agent

```bash
darcbench serve                          # loopback, generated token
darcbench serve --port 9000              # alternative port
darcbench serve --token "$LONG_SECRET"   # fixed token (>= 32 chars)
darcbench serve --json                   # machine-readable startup info
```

Startup prints the dashboard URL including a one-time token. Nothing runs until
someone asks for a run.

### Remote access

**Use an SSH tunnel.** It is the recommended path and requires no firewall
change:

```bash
ssh -N -L 7842:127.0.0.1:7842 user@server
```

Binding a non-loopback address is possible and produces a loud warning: the
token is the only protection and it travels in clear over plain HTTP. If you do
it, put TLS in front.

### As a systemd service

```ini
[Unit]
Description=DARCBench agent
After=network.target

[Service]
Type=simple
User=darcbench
Group=darcbench
Environment=DARCBENCH_HOME=/var/lib/darcbench
Environment=DARCBENCH_TOKEN=%%TOKEN%%
ExecStart=/usr/local/bin/darcbench serve --port 7842
Restart=on-failure

# The agent needs none of this. Deny it.
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/lib/darcbench
ProtectKernelTunables=true
ProtectControlGroups=true
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
LockPersonality=true
MemoryDenyWriteExecute=true

[Install]
WantedBy=multi-user.target
```

Store the token in a systemd credential or an `EnvironmentFile` with mode 0600 —
not in the unit file, which is world-readable.

## Before running on production

1. `darcbench doctor` — read the risk class and every finding.
2. Confirm the estimated duration and bytes written are acceptable.
3. Prefer a maintenance window. `cpu.mixed` saturates every core.
4. Have the cancel path ready: the dashboard button, or
   `POST /api/v1/runs/{id}/cancel`.

The agent will refuse to start a heavy run on a machine that looks live unless
you explicitly acknowledge it. That refusal is the feature.

## Interpreting results

| Observation | Likely meaning |
|---|---|
| High CV (>15%) | Noisy neighbour, or competing local work |
| Steal time >1% sustained | Oversubscription, or burst credits exhausted |
| Multi-core ≪ single-core × threads | SMT, thermal limits, or shared vCPUs |
| CPU clock declining across a run | Thermal/power throttling, or credit exhaustion |
| `Partial` verdict | Required categories missing — expected in this build |
| `Invalid` verdict | Read `verdict.reasons`; the run was cancelled or failed a check |

A high CV is not a failed benchmark. On shared infrastructure it is frequently
the most useful thing on the page.

## Artifacts

```
$DARCBENCH_HOME/runs/<run_id>/
  bundle.json     signed result — the thing to keep
  report.html     self-contained, safe to open offline or print
  events.ndjson   full event stream, one envelope per line
```

Back up `bundle.json`; everything else regenerates from it.

`agent.key` is the agent's Ed25519 identity, mode 0600. Losing it means future
bundles are signed by a new identity; it does not invalidate past ones. **Do not
copy it between machines** — one key, one agent.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `port 7842 already has a listener` | Something else is bound | `--port 9000`. The agent will not displace a service |
| `401 unauthenticated` | Token missing or wrong | Open the URL the agent printed, with `?token=` |
| `403 csrf_protection` | Mutation attempted with cookie/query auth | Send `Authorization: Bearer` |
| `409 run_in_progress` | Another run active | Wait or cancel it; concurrent runs measure each other |
| `410 replay_unavailable` | SSE position evicted from the buffer | Refetch the run, then resubscribe |
| Dashboard shows the built-in console | UI not compiled in | `(cd apps/web && pnpm build) && cargo build --release` |
| Preflight blocks on free space | Under the estimate plus 2 GiB margin | Free space, or use a profile that writes nothing |
| `verify` exits 3 | Signature or recomputation failed | The bundle was edited or is corrupt |

Logs go to **stderr** (`--log debug`), so `--json` output on stdout stays a
clean parseable document.

## Security notes for operators

- The token appears in your shell scrollback and, behind a reverse proxy, in its
  access log. Prefer generated per-start tokens over a fixed one.
- Never expose the dashboard publicly without TLS.
- Run as an unprivileged user; the modules in this build need no root.
- `darcbench uninstall` removes only the state directory — that is genuinely all
  the agent ever created. No configuration was modified, so none needs undoing.

## Upgrading

Replace the binary. Bundles are versioned by schema, protocol and scoring model,
so old bundles remain readable and `verify` keeps working. A scoring model change
never rewrites a stored score; it produces an additional one.
