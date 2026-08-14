/**
 * DARCBench `node.runtime` workload.
 *
 * Embedded in the agent binary at compile time and written to the agent's own
 * scratch directory before each run. Never read from a caller-supplied path,
 * never assembled from anything but this file. See
 * docs/adr/0013-executing-a-discovered-runtime.md.
 *
 * CommonJS on purpose, and named `.cjs` so it stays CommonJS whatever a
 * `package.json` further up the tree says. The `module_load` workload measures
 * `require`, which is what a dependency tree costs on a cold start, and an ESM
 * rewrite would measure a different loader.
 *
 * Contract with the Rust side:
 *
 *     node bench.cjs describe    -> {"kind":"describe", "version":..., ...}
 *     node bench.cjs noop        -> {"kind":"noop"}
 *     node bench.cjs <workload> <iterations>
 *                                -> {"kind":"measure","ops":N,"elapsed_ms":F,"checksum":...}
 *
 * `elapsed_ms` times the workload only. Process creation, V8 start-up and the
 * compile of this file are outside it deliberately: they are a real cost, they
 * are what `startup.cold` measures on its own, and folding them into a
 * throughput figure would make every workload look slower on a machine with a
 * slow disk.
 *
 * `checksum` exists so the Rust side can assert the work was actually done.
 * V8 is an optimising compiler with escape analysis; a loop whose result is
 * never observed is a loop it is entitled to delete, and a benchmark that let
 * it would report a spectacular number for nothing.
 */

'use strict';

const fs = require('node:fs');
const fsp = require('node:fs/promises');
const os = require('node:os');
const path = require('node:path');
const crypto = require('node:crypto');

function emit(payload) {
  process.stdout.write(JSON.stringify(payload) + '\n');
}

/**
 * Everything the bundle must disclose about the runtime being measured.
 *
 * The methodology requires the PHP equivalent of this and the same reasoning
 * applies: two Node results from different major versions are not comparable,
 * because V8 changes what the same JavaScript costs between them. `jitless`
 * matters most of all - a build with the JIT disabled runs an order of
 * magnitude slower and would otherwise read as a slow machine.
 */
function describe() {
  return {
    kind: 'describe',
    version: process.versions.node,
    v8: process.versions.v8,
    uv: process.versions.uv,
    modules: process.versions.modules,
    arch: process.arch,
    platform: process.platform,
    // A jitless or lite-mode build is not the Node anyone serves traffic with.
    jitless: Boolean(process.config?.variables?.v8_enable_lite_mode),
    pointer_compression: Boolean(process.config?.variables?.v8_enable_pointer_compression),
    // The libuv thread pool size decides how much filesystem work overlaps.
    // It is set by `UV_THREADPOOL_SIZE`, and the agent clears the environment
    // before exec - so the pool is always at its default here, and reading the
    // variable would report a value this process can never see. Recorded as a
    // constant so the bundle says what the measurement was taken under rather
    // than implying it observed something.
    uv_threadpool_size: 'default (4); the agent clears the environment',
    cpus: os.cpus().length,
    // Heap limits change when a workload starts to garbage-collect rather than
    // allocate, which is the difference between two very different numbers.
    heap_size_limit_bytes: require('node:v8').getHeapStatistics().heap_size_limit,
    execArgv: process.execArgv,
  };
}

/** A deterministic record, identical on every machine and every run. */
function fixture(index) {
  return {
    id: index,
    sku: 'DARC-' + String(index % 9973).padStart(6, '0'),
    title: 'Product ' + index + ' with a reasonably long descriptive name',
    price: (index % 50000) / 100,
    tags: ['alpha', 'beta', 'gamma', 'delta'],
    stock: index % 137,
    active: index % 3 !== 0,
    meta: { weight: (index % 900) / 10, origin: { country: 'ES', region: 'Valencia' } },
  };
}

function escapeHtml(value) {
  return String(value)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}

/** Where this workload may create files: the directory it was written into. */
const WORK_DIR = path.join(__dirname, 'darcbench-node-work');

/**
 * Generates the module tree `module_load` requires.
 *
 * Sixty-four small files, which is the shape of a modest dependency tree and
 * enough that resolution and compilation dominate the per-file overhead.
 */
function prepareModules(count) {
  fs.mkdirSync(WORK_DIR, { recursive: true, mode: 0o700 });
  const files = [];
  for (let i = 0; i < count; i++) {
    const file = path.join(WORK_DIR, `mod${i}.cjs`);
    // Big enough to be worth compiling, small enough that this is a compile
    // measurement rather than a disk one.
    const body = [
      `'use strict';`,
      `const table = ${JSON.stringify(Array.from({ length: 48 }, (_, j) => (i * 31 + j * 17) % 1009))};`,
      `function step${i}(x) { let a = x; for (const v of table) { a = (a * 33 + v) >>> 0; } return a; }`,
      `module.exports = { step${i}, size: table.length };`,
    ].join('\n');
    fs.writeFileSync(file, body, { mode: 0o600 });
    files.push(file);
  }
  return files;
}

function cleanUp() {
  try {
    fs.rmSync(WORK_DIR, { recursive: true, force: true });
  } catch {
    // Best effort. The Rust side removes the whole scratch script anyway, and
    // failing the measurement over a cleanup error would be the wrong trade.
  }
}

/**
 * Workloads.
 *
 * The roadmap asks for API, SSR, async I/O and build. Framework-free, for the
 * same reason as PHP: a framework benchmark measures the framework's authors.
 *
 * Each returns a number that depends on the work, so V8 cannot delete the loop.
 */
async function runWorkload(name, iterations) {
  switch (name) {
    case 'json_stringify': {
      // Serialising a response body. Every API request ends here.
      let checksum = 0;
      for (let i = 0; i < iterations; i++) {
        checksum += JSON.stringify(fixture(i)).length;
      }
      return checksum;
    }

    case 'json_parse': {
      // Parsing a request body or a cached blob. Built once outside the loop,
      // because the measurement is parsing rather than serialising.
      const documents = [];
      for (let i = 0; i < 32; i++) {
        documents.push(JSON.stringify(fixture(i * 977)));
      }
      let checksum = 0;
      for (let i = 0; i < iterations; i++) {
        checksum += JSON.parse(documents[i & 31]).stock;
      }
      return checksum;
    }

    case 'ssr_render': {
      // Server-side rendering, reduced to what every framework compiles down
      // to: walking a tree and concatenating escaped strings.
      let checksum = 0;
      for (let i = 0; i < iterations; i++) {
        const row = fixture(i);
        let html =
          '<li class="product" data-id="' + row.id + '">' +
          '<h3>' + escapeHtml(row.title) + '</h3>' +
          '<span class="price">' + row.price.toFixed(2) + '</span><ul>';
        for (const tag of row.tags) html += '<li>' + tag + '</li>';
        html += '</ul></li>';
        checksum += html.length;
      }
      return checksum;
    }

    case 'crypto_hash': {
      // Sessions, ETags, cache keys, integrity checks.
      const payload = Buffer.from('darcbench-'.repeat(1024));
      let checksum = 0;
      for (let i = 0; i < iterations; i++) {
        const hash = crypto.createHash('sha256');
        hash.update(payload);
        hash.update(String(i));
        checksum += hash.digest()[0];
      }
      return checksum;
    }

    case 'async_fileio': {
      // The event loop and the libuv thread pool, which is what an API server
      // spends its time on between the CPU work above.
      //
      // Small files and a batch of sixteen on purpose: the point is the async
      // machinery - syscall, thread pool hand-off, loop turn - not the device.
      // `storage.mixed` measures the device, and doing it again here would put
      // the same disk in two categories.
      fs.mkdirSync(WORK_DIR, { recursive: true, mode: 0o700 });
      const payload = Buffer.alloc(4096, 0x61);
      const batch = 16;
      let checksum = 0;
      for (let i = 0; i < iterations; i += batch) {
        const pending = [];
        for (let j = 0; j < batch && i + j < iterations; j++) {
          const file = path.join(WORK_DIR, `io${j}.bin`);
          pending.push(
            fsp
              .writeFile(file, payload, { mode: 0o600 })
              .then(() => fsp.readFile(file))
              .then((read) => read.length)
          );
        }
        const lengths = await Promise.all(pending);
        for (const length of lengths) checksum += length;
      }
      return checksum;
    }

    case 'module_load': {
      // What a dependency tree costs on a cold start: resolution, read,
      // compile and first execution, per file.
      //
      // The require cache is cleared between iterations so each one pays the
      // compile again. The files stay in the page cache, which is right - this
      // is a compile measurement, and a machine is not slow at starting Node
      // because its disk is cold on the first read of the day.
      const files = prepareModules(64);
      let checksum = 0;
      for (let i = 0; i < iterations; i++) {
        for (const file of files) {
          delete require.cache[require.resolve(file)];
          const loaded = require(file);
          checksum += loaded.size;
        }
      }
      return checksum;
    }

    default:
      emit({ kind: 'error', message: 'unknown workload' });
      process.exit(2);
  }
}

async function main() {
  const argv = process.argv;
  if (argv.length < 3) {
    emit({ kind: 'error', message: 'usage: bench.cjs describe | bench.cjs <workload> <iterations>' });
    process.exit(2);
  }

  if (argv[2] === 'describe') {
    emit(describe());
    return;
  }

  // The cheapest possible script: start the process, compile this file, emit
  // one line, exit. Timed from the outside, that *is* what a serverless
  // invocation or a `node script.js` in a deploy pipeline pays.
  if (argv[2] === 'noop') {
    emit({ kind: 'noop' });
    return;
  }

  if (argv.length < 4) {
    emit({ kind: 'error', message: 'missing iteration count' });
    process.exit(2);
  }
  const name = String(argv[2]);
  const iterations = Number.parseInt(argv[3], 10);
  if (!Number.isFinite(iterations) || iterations < 1) {
    emit({ kind: 'error', message: 'iteration count must be positive' });
    process.exit(2);
  }

  // `hrtime.bigint` rather than `Date.now`: it is monotonic, so a clock
  // adjustment during the run cannot produce a negative or absurd interval.
  const started = process.hrtime.bigint();
  const checksum = await runWorkload(name, iterations);
  const elapsedNs = process.hrtime.bigint() - started;

  cleanUp();
  emit({
    kind: 'measure',
    workload: name,
    ops: iterations,
    elapsed_ms: Number(elapsedNs) / 1e6,
    checksum,
    heap_used_bytes: process.memoryUsage().heapUsed,
  });
}

main().catch((error) => {
  cleanUp();
  emit({ kind: 'error', message: String(error && error.stack ? error.stack : error) });
  process.exit(1);
});
