# Field corpus, August 2026

Six signed bundles from three bare-metal hosts. These are the measurements
[`../../docs/FIELD-EVIDENCE.md`](../../docs/FIELD-EVIDENCE.md) argues from, published
so that every number in it can be checked rather than taken on trust.

```bash
for b in corpus/2026-08/*.json; do darcbench verify "$b"; done
```

All six verify: signature valid, scores recomputed from the raw metrics, verdict
reproduced. They are published **byte for byte as the agent wrote them**, and
`.gitattributes` marks this directory `-text` so a checkout on any platform is
the same file that was signed.

Verification does not in fact depend on that: a signature covers the canonical
re-serialisation of the *parsed* bundle, not the bytes of the file, so even a
CRLF checkout verifies. Editing the *content* is what breaks a bundle — which is
why the one flaw disclosed below is disclosed rather than patched out.

## What is here

| File | Host | Profile | Notes |
|---|---|---|---|
| `run_50437d01…` | H1 — Xeon E-2274G, 4c/8t | `quick` | |
| `run_80a1e5d0…` | H2 — Xeon E5-1620 v2, 4c/8t | `quick` | `schedutil`, load1 1.46 |
| `run_f63fb2c0…` | H3 — 2 × Xeon E5-2699 v3, 36c/72t | `quick` | 2 NUMA nodes |
| `run_90e392e0…` | H1 — Xeon E-2274G, 4c/8t | `deep` | |
| `run_74bbe75a…` | H2 — Xeon E5-1620 v2, 4c/8t | `deep` | |
| `run_091150b5…` | H3 — 2 × Xeon E5-2699 v3, 36c/72t | `deep` | |

The `quick` runs were made as root, the `deep` runs as an ordinary user. All six
came from one static musl binary,
`sha256:2e8308c5717b1f2ec8093318ecf9704574d2f5d88ad5a150e921aeb74e0e248a`, so
`build_target` and `agent_build_hash` match across all of them.

**This is not a calibration set** and nothing in the scoring model is derived
from it. See `FIELD-EVIDENCE.md` for why, and for what it is good for instead.

## Privacy

Checked against [`../../docs/PRIVACY.md`](../../docs/PRIVACY.md) before publishing.
No hostnames, MAC addresses, IP addresses, serial numbers, chassis or product
UUIDs, cloud instance identifiers, or environment variables — the first two are
redacted by the agent, the rest are never collected.

What *is* here, by design, is hardware: CPU model and topology, memory and
storage devices, link speed, kernel and distribution, DMI vendor and product,
and the numbers of listening TCP ports. That is the visible column of the table
in `PRIVACY.md`, and it is the point of a benchmark corpus.

**One exception, and it is ours.** `run_74bbe75a…` carries the string
`/home/ubuntu/.local/state/darcbench/scratch` inside a `node.runtime`
precondition failure. `PRIVACY.md` says usernames and paths inside home
directories are *never* collected, so that is a breach of a guarantee this
project states and tests for — found by scanning this corpus before publishing
it, which is the argument for publishing corpora. It is kept rather than
patched out, because patching it would invalidate the signature and a bundle
nobody can verify is worth less than a disclosed flaw. `ubuntu` is the default
account name on millions of Ubuntu images and locates nothing.

The leak is fixed at source: `runtime_exec::elide_home` renders such a path as
`~/.local/state/darcbench/scratch`, and a test asserts no output of it can name
a home directory. Bundles written after that fix do not carry the string.

Each bundle also carries the agent's Ed25519 public key. It is per-installation,
not per-machine — it says two runs came from the same install, which is what
makes a result attributable, and it reveals nothing about where that install is.
