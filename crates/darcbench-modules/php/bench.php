<?php
/**
 * DARCBench `php.runtime` workload.
 *
 * Embedded in the agent binary at compile time and written to the agent's own
 * scratch directory before each run. Never read from a caller-supplied path,
 * never assembled from anything but this file. See
 * docs/adr/0013-executing-a-discovered-runtime.md.
 *
 * Contract with the Rust side:
 *
 *     php bench.php describe
 *         -> {"kind":"describe", "version":..., "sapi":..., ...}
 *
 *     php bench.php <workload> <iterations>
 *         -> {"kind":"measure", "ops":N, "elapsed_ms":F, "checksum":...}
 *
 * `elapsed_ms` times the workload only. Interpreter start-up and script
 * compilation are outside it deliberately: they are a real cost, they are what
 * `startup.cold` measures on its own, and folding them into a throughput figure
 * would make every workload look slower on a machine with a slow disk.
 *
 * `checksum` exists so the Rust side can assert the work was actually done. An
 * optimiser - or a future rewrite of a workload - that quietly computed nothing
 * would otherwise report a spectacular result.
 */

declare(strict_types=1);

function emit(array $payload): void {
    echo json_encode($payload, JSON_UNESCAPED_SLASHES), "\n";
}

/**
 * Everything the bundle must disclose about the runtime being measured.
 *
 * `docs/BENCHMARK-METHODOLOGY.md`: "PHP runs must disclose the runtime (native,
 * container, panel-managed, FPM, Apache module, LiteSpeed), OPcache state,
 * worker count and resource limits." Two PHP results from differently
 * configured interpreters are not comparable, and this is the evidence that
 * lets the comparison be refused rather than misread.
 */
function describe(): array {
    $opcache_loaded = extension_loaded('Zend OPcache');
    $opcache_enabled = false;
    $opcache_jit = 'unavailable';
    if ($opcache_loaded && function_exists('opcache_get_status')) {
        // `false` asks it not to return the whole script cache, which on a busy
        // FPM pool is large and is none of our business.
        $status = @opcache_get_status(false);
        $opcache_enabled = is_array($status) && ($status['opcache_enabled'] ?? false);
        if (is_array($status) && isset($status['jit'])) {
            $opcache_jit = ($status['jit']['enabled'] ?? false) ? 'enabled' : 'disabled';
        }
    }

    return [
        'kind' => 'describe',
        'version' => PHP_VERSION,
        'version_id' => PHP_VERSION_ID,
        'sapi' => PHP_SAPI,
        'int_size' => PHP_INT_SIZE,
        'debug_build' => (bool) (defined('PHP_DEBUG') && PHP_DEBUG),
        'zts' => (bool) (defined('PHP_ZTS') && PHP_ZTS),
        'opcache_loaded' => $opcache_loaded,
        'opcache_enabled' => $opcache_enabled,
        'opcache_enable_cli' => (string) ini_get('opcache.enable_cli'),
        'opcache_jit' => $opcache_jit,
        'memory_limit' => (string) ini_get('memory_limit'),
        'max_execution_time' => (string) ini_get('max_execution_time'),
        // Which extensions are loaded changes what a real application costs,
        // and is the first thing to look at when two machines disagree.
        'extensions' => array_values(array_slice(get_loaded_extensions(), 0, 64)),
    ];
}

/** A deterministic record, identical on every machine and every run. */
function fixture(int $index): array {
    return [
        'id' => $index,
        'sku' => 'DARC-' . str_pad((string) ($index % 9973), 6, '0', STR_PAD_LEFT),
        'title' => 'Product ' . $index . ' with a reasonably long descriptive name',
        'price' => ($index % 50000) / 100.0,
        'tags' => ['alpha', 'beta', 'gamma', 'delta'],
        'stock' => $index % 137,
        'active' => ($index % 3) !== 0,
        'meta' => [
            'weight' => ($index % 900) / 10.0,
            'origin' => ['country' => 'ES', 'region' => 'Valencia'],
        ],
    ];
}

/**
 * Workloads.
 *
 * Framework-free on purpose, per the roadmap. A framework benchmark measures
 * the framework's authors, and the number a hosting buyer needs is what this
 * machine does with the four things every PHP application spends its time on:
 * building arrays, moving JSON, hashing passwords, and concatenating a page.
 *
 * Each returns an integer checksum, so nothing can be optimised away unnoticed.
 */
function run_workload(string $name, int $iterations): int {
    switch ($name) {
        case 'json_encode':
            // Encoding a response body. Every API request ends here.
            $checksum = 0;
            for ($i = 0; $i < $iterations; $i++) {
                $checksum += strlen(json_encode(fixture($i)));
            }
            return $checksum;

        case 'json_decode':
            // Decoding a request body or a cached blob. Built once outside the
            // loop, because the measurement is decoding rather than encoding.
            $documents = [];
            for ($i = 0; $i < 32; $i++) {
                $documents[] = json_encode(fixture($i * 977));
            }
            $checksum = 0;
            for ($i = 0; $i < $iterations; $i++) {
                $decoded = json_decode($documents[$i & 31], true);
                $checksum += (int) $decoded['stock'];
            }
            return $checksum;

        case 'array_ops':
            // Sorting, filtering and looking up: the shape of every listing
            // page ever written.
            $checksum = 0;
            for ($i = 0; $i < $iterations; $i++) {
                $rows = [];
                for ($j = 0; $j < 32; $j++) {
                    $rows[] = (($i * 31 + $j * 17) % 1009);
                }
                sort($rows);
                $filtered = array_filter($rows, static fn(int $v): bool => ($v & 1) === 0);
                $checksum += count($filtered) + $rows[0];
            }
            return $checksum;

        case 'string_template':
            // Building an HTML fragment by concatenation, which is what every
            // template engine compiles down to.
            $checksum = 0;
            for ($i = 0; $i < $iterations; $i++) {
                $row = fixture($i);
                $html = '<li class="product" data-id="' . $row['id'] . '">'
                    . '<h3>' . htmlspecialchars($row['title'], ENT_QUOTES) . '</h3>'
                    . '<span class="price">' . number_format($row['price'], 2) . '</span>'
                    . '<ul>';
                foreach ($row['tags'] as $tag) {
                    $html .= '<li>' . $tag . '</li>';
                }
                $html .= '</ul></li>';
                $checksum += strlen($html);
            }
            return $checksum;

        case 'hash_general':
            // Bulk hashing: sessions, ETags, cache keys, integrity checks.
            $payload = str_repeat('darcbench-', 1024);
            $checksum = 0;
            for ($i = 0; $i < $iterations; $i++) {
                $checksum += ord(hash('sha256', $payload . $i, true)[0]);
            }
            return $checksum;

        case 'hash_password':
            // The single most expensive thing a login page does, and the one
            // that decides how many sign-ins a machine survives. The cost is
            // pinned rather than taken from the runtime's default: bcrypt cost
            // is exponential, so comparing a cost-10 machine against a cost-12
            // one would compare the configurations rather than the machines.
            //
            // The checksum folds in the *cost digits* of the hash, not its
            // length: a bcrypt hash is always 60 characters, so a length-only
            // checksum is identical whatever cost was applied. A build or
            // policy that ignored `cost => 8` and used the ini default of 10
            // would then be four times slower with a byte-identical checksum,
            // and would be published as "this machine is slow at PHP".
            // `$2y$08$...` puts the cost at offsets 4 and 5.
            $checksum = 0;
            for ($i = 0; $i < $iterations; $i++) {
                $hash = (string) password_hash('correct horse battery staple', PASSWORD_BCRYPT, ['cost' => 8]);
                $checksum += strlen($hash) + ord($hash[4]) * 256 + ord($hash[5]);
            }
            return $checksum;

        default:
            emit(['kind' => 'error', 'message' => 'unknown workload']);
            exit(2);
    }
}

$argv = $_SERVER['argv'] ?? [];
if (count($argv) < 2) {
    emit(['kind' => 'error', 'message' => 'usage: bench.php describe | bench.php <workload> <iterations>']);
    exit(2);
}

if ($argv[1] === 'describe') {
    emit(describe());
    exit(0);
}

// The cheapest possible script: start the interpreter, compile this file, emit
// one line, exit. Timed from the outside, that *is* the cold-start cost every
// request pays on a runtime without a warm opcode cache, which is why it gets a
// mode of its own rather than being inferred from `describe` - `describe` walks
// the extension list, and a machine with forty extensions would look like a
// machine with a slow interpreter.
if ($argv[1] === 'noop') {
    emit(['kind' => 'noop']);
    exit(0);
}

if (count($argv) < 3) {
    emit(['kind' => 'error', 'message' => 'missing iteration count']);
    exit(2);
}

$name = (string) $argv[1];
$iterations = (int) $argv[2];
if ($iterations < 1) {
    emit(['kind' => 'error', 'message' => 'iteration count must be positive']);
    exit(2);
}

// `hrtime` rather than `microtime`: it is monotonic, so a clock adjustment
// during the run cannot produce a negative or absurd interval.
$started = hrtime(true);
$checksum = run_workload($name, $iterations);
$elapsed_ns = hrtime(true) - $started;

emit([
    'kind' => 'measure',
    'workload' => $name,
    'ops' => $iterations,
    'elapsed_ms' => $elapsed_ns / 1e6,
    'checksum' => $checksum,
    'peak_memory_bytes' => memory_get_peak_usage(true),
]);
