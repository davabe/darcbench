# Benchmark module manifests

Machine-readable manifests for every first-party module, mirroring the
`ModuleManifest` the agent compiles in. They exist so that the module catalogue
can be read, diffed and validated without running the agent.

The agent's compiled manifest is authoritative. `scripts/check-manifests.sh`
fails CI if a manifest here disagrees with what `darcbench modules` reports.

| Module | Category | Safety | Status |
|---|---|---|---|
| [`cpu.mixed`](cpu/cpu.mixed.json) | compute | compute_intensive | Implemented |
| [`memory.bandwidth`](memory/memory.bandwidth.json) | memory | compute_intensive | Implemented |
| [`storage.mixed`](storage/storage.mixed.json) | storage | writes_temporary_files | Implemented |
| [`network.transfer`](network/network.transfer.json) | network | uses_network | Implemented |
| [`web.static`](web/web.static.json) | web | provisions_services | Implemented |
| [`php.runtime`](web/php.runtime.json) | web | provisions_services | Implemented |
| [`node.runtime`](web/node.runtime.json) | web | provisions_services | Implemented |

`network.transfer` is the only module that contacts anything outside the
machine, and its manifest carries the allow-list of hosts it can reach. That
list is a copy of a compile-time table for reading; the table in
`crates/darcbench-modules/src/network_endpoints.rs` is what actually constrains
the module.

`web.static` is the only module that *starts* something. Its origin binds
`127.0.0.1` on a port the OS assigns, serves generated bodies from memory, and
is destroyed when the module returns. It never targets an operator-supplied URL
and never touches the operator's own web server, which
[T-AMPLIFY](../docs/THREAT-MODEL.md) makes a permanent product constraint rather
than a current limitation.

`php.runtime` and `node.runtime` are the modules that *execute* something. They
run the interpreter the operator installed, from a compile-time allow-list of
absolute paths, after
checking that the binary and every directory above it are owned by root and
writable only by root - with a fixed argument vector, no shell and a cleared
environment. [T-EXEC](../docs/THREAT-MODEL.md) and
[ADR-0013](../docs/adr/0013-executing-a-discovered-runtime.md) record why each of
those is necessary. It is deliberately absent from the `standard` profile:
most machines have no PHP, and a standard run must not report the profile's own
assumptions as a fault of the machine.

Planned modules (`database.oltp`,
`database.cache`, `wordpress.*`, `deployment.container`) get a manifest in the
same change that implements them - never before. See
[docs/ROADMAP.md](../docs/ROADMAP.md).
