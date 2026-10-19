# Benchmarks

## Four-loop comparison on Linux

Remeasured October 14, 2026 using runtime commit `97fa7cf` on an Intel Core i9-9900K,
Linux 7.0.0-38-generic (x86_64), with rsloop 0.1.64 built in release mode,
uvloop 0.22.1 and zuvloop 0.0.17. The GIL-enabled runs use CPython 3.14.7;
the free-threaded runs use CPython 3.14.0 with `PYTHON_GIL=0` for all four loops.
The rsloop extension matches the verified completed-report build. Its reactor
is io_uring. All four requested loops completed every comparison.

Each general-workload result is the median of seven measured runs after two
warmups, with every run in a fresh subprocess. Cyclic GC is disabled during
each run. All builds and package installation finished before measurement;
no builds or tests from this session ran alongside the benchmarks.

The workloads schedule 1,000,000 callbacks as one batch, run 200,000 tiny tasks
that each await `asyncio.sleep(0)` in batches of 5,000, and perform 20,000 TCP
round trips with 1,024-byte payloads. TCP uses rsloop's native fast streams;
the other loops use stdlib asyncio streams.

Times are milliseconds (lower is better). Memory tables report the median
of each process's peak RSS in MiB, including the interpreter and libraries.
Linux memory is read from the child's `VmHWM` counter, avoiding inherited
parent high-water marks in `ru_maxrss`. The benchmark helper was corrected
for this refresh; runtime code is unchanged.
CPU affinity applies to each process and its threads. CPUs 2 and 3 are separate
physical cores on this host; their SMT siblings are CPUs 10 and 11. Allowing
two cores still runs one event loop per process.

### One core, GIL enabled (CPU 2)

Median time (ms):

| Workload | asyncio | uvloop | zuvloop | rsloop |
|---|---:|---:|---:|---:|
| 1,000,000 callbacks | 540.21 | 255.99 | 182.70 | 174.51 |
| 200,000 tasks | 560.91 | 340.24 | 294.93 | 306.45 |
| 20,000 TCP round trips | 566.74 | 467.26 | 377.84 | 359.31 |

Peak RSS (MiB):

| Workload | asyncio | uvloop | zuvloop | rsloop |
|---|---:|---:|---:|---:|
| 1,000,000 callbacks | 201.0 | 234.6 | 125.9 | 103.9 |
| 200,000 tasks | 31.7 | 34.6 | 33.4 | 34.6 |
| 20,000 TCP round trips | 24.4 | 27.8 | 26.6 | 28.4 |

### Two cores, GIL enabled (CPUs 2–3)

Median time (ms):

| Workload | asyncio | uvloop | zuvloop | rsloop |
|---|---:|---:|---:|---:|
| 1,000,000 callbacks | 533.23 | 254.45 | 180.93 | 173.72 |
| 200,000 tasks | 556.59 | 341.76 | 299.91 | 303.58 |
| 20,000 TCP round trips | 567.25 | 472.58 | 379.03 | 361.42 |

Peak RSS (MiB):

| Workload | asyncio | uvloop | zuvloop | rsloop |
|---|---:|---:|---:|---:|
| 1,000,000 callbacks | 201.0 | 234.6 | 125.9 | 103.9 |
| 200,000 tasks | 31.6 | 34.5 | 33.3 | 34.6 |
| 20,000 TCP round trips | 24.4 | 27.8 | 26.6 | 28.4 |

### One core, free-threaded (CPU 2)

Median time (ms):

| Workload | asyncio | uvloop | zuvloop | rsloop |
|---|---:|---:|---:|---:|
| 1,000,000 callbacks | 594.68 | 296.78 | 199.94 | 212.44 |
| 200,000 tasks | 638.17 | 397.60 | 351.12 | 353.73 |
| 20,000 TCP round trips | 600.29 | 502.85 | 398.72 | 387.49 |

Peak RSS (MiB):

| Workload | asyncio | uvloop | zuvloop | rsloop |
|---|---:|---:|---:|---:|
| 1,000,000 callbacks | 208.0 | 256.7 | 133.1 | 186.8 |
| 200,000 tasks | 39.0 | 41.5 | 42.7 | 43.0 |
| 20,000 TCP round trips | 30.7 | 34.4 | 33.8 | 36.3 |

### Interpretation

One core, GIL enabled (CPU 2): rsloop led callbacks, zuvloop led tasks, and rsloop led TCP round trips.

Two cores, GIL enabled (CPUs 2–3): rsloop led callbacks, zuvloop led tasks, and rsloop led TCP round trips.

One core, free-threaded (CPU 2): zuvloop led callbacks, zuvloop led tasks, and rsloop led TCP round trips.

These local microbenchmarks ran on a shared host. Results include run-to-run
variation; different CPU affinities and Python patch versions do not isolate
the effects of core count or the GIL. Application workloads below can rank
the loops differently from callback and task microbenchmarks.

## Network workloads: all four loops

Remeasured on the same release builds and host, using CPython 3.14.7 with the
GIL enabled and CPU 2. Each entry is the median of seven measured runs after
two warmups. The matrix uses warm measurement mode: one child process per
loop/scenario runs all nine samples sequentially. Each sample creates a new
loop and server. GC remains enabled in this matrix.

Each run uses 16 concurrent connections with 500 operations per connection
(8,000 operations). HTTP bodies are 4,096 bytes; WebSocket frames cycle through
32, 256 and 4,096 bytes. Timings include connection setup and teardown, with
eight SHA-256 iterations per HTTP request to model application work.

Median time (ms):

| Workload | asyncio | uvloop | zuvloop | rsloop |
|---|---:|---:|---:|---:|
| HTTP keep-alive | 250.58 | 147.90 | 126.63 | 137.83 |
| TLS HTTP | 517.24 | 315.19 | 333.92 | 350.97 |
| Raw WebSocket | 1673.65 | 1588.72 | 1572.70 | 1595.93 |
| Raw TLS WebSocket | 1913.15 | 1755.70 | 1764.08 | 1850.40 |

## Short TCP connections: all four loops

Remeasured with CPython 3.14.7, the GIL enabled, and CPU 2. Each sample opens
8,000 fresh connections at concurrency 16, validates a one-byte echo, then
closes them. Timing includes connection setup, exchange, teardown and server
shutdown, and excludes listener creation. GC is disabled. Each loop has two
warmups and seven measured samples in its own process, with a new loop and
server per sample. All loops execute the same `connection_churn.measure`
workload through their own loop factory.

Median time (ms):

| Workload | asyncio | uvloop | zuvloop | rsloop |
|---|---:|---:|---:|---:|
| 8,000 short TCP connections | 1195.19 | 876.07 | 793.05 | 825.60 |

## Native reader lifecycle

This diagnostic measures rsloop's internal `PyFastStreamReader`, rather than
comparing event loops. The current release build runs on CPU 2 with CPython
3.14.7 and the GIL enabled. Each method has a 1,000-round warmup followed by
seven samples of 100,000 rounds. Waiting reads include dispatch of Future
done callbacks. GC remains enabled.

Median nanoseconds per round (lower is better):

| Workload | rsloop (ns) |
|---|---:|
| Buffered `read` | 282.9 |
| Buffered `readexactly` | 281.6 |
| Buffered `readuntil` | 306.4 |
| Waiting `read` | 2310.4 |
| Waiting `readexactly` | 1482.5 |
| Waiting `readuntil` | 1543.1 |

## Reproduction and raw samples

For a clean GIL-enabled environment:

```bash
uv venv --python 3.14.7 target/bench-gil
uv pip install --python target/bench-gil/bin/python maturin uvloop==0.22.1 zuvloop==0.0.17
VIRTUAL_ENV="$PWD/target/bench-gil" target/bench-gil/bin/maturin develop --release --locked
taskset -c 2 target/bench-gil/bin/python benches/compare_event_loops.py \
  --loops asyncio,uvloop,zuvloop,rsloop --repeat 7 --warmups 2 \
  --callbacks 1000000 --tasks 200000 --task-batch-size 5000 \
  --tcp-roundtrips 20000 --payload-size 1024 \
  --json-output target/event-loops-97fa7cf-one-core.json
```

For two physical cores on this host, use `taskset -c 2,3` and a different output
filename. Check CPU topology before choosing affinity on another machine.

For the free-threaded environment:

```bash
uv venv --python 3.14.0t target/bench-free-threaded
uv pip install --python target/bench-free-threaded/bin/python maturin uvloop==0.22.1 zuvloop==0.0.17
VIRTUAL_ENV="$PWD/target/bench-free-threaded" target/bench-free-threaded/bin/maturin develop --release --locked
PYTHON_GIL=0 taskset -c 2 target/bench-free-threaded/bin/python benches/compare_event_loops.py \
  --loops asyncio,uvloop,zuvloop,rsloop --repeat 7 --warmups 2 \
  --callbacks 1000000 --tasks 200000 --task-batch-size 5000 \
  --tcp-roundtrips 20000 --payload-size 1024 \
  --json-output target/event-loops-97fa7cf-free-threaded.json
```

Run the network matrix with the GIL-enabled interpreter:

```bash
taskset -c 2 target/bench-gil/bin/python benches/workload_matrix.py \
  --loops asyncio,uvloop,zuvloop,rsloop \
  --scenarios http_keepalive,tls_http,websocket_messages,websocket_tls \
  --concurrency 16 --requests-per-connection 500 --warmups 2 --repeat 7 \
  --json-output target/event-loops-97fa7cf-network.json
```

The churn CLI defaults to rsloop. To compare the same workload across all four
factories, run this from the repository root with the GIL-enabled interpreter
pinned to CPU 2:

```python
import asyncio
import gc
import statistics
import sys

sys.path.insert(0, "benches")
from compare_event_loops import loop_factory_for
from connection_churn import measure

for name in ("asyncio", "uvloop", "zuvloop", "rsloop"):
    gc.collect()
    gc.disable()
    try:
        samples = [
            asyncio.run(
                asyncio.wait_for(measure(8000, 16), 120),
                loop_factory=loop_factory_for(name),
            )
            for _ in range(9)
        ][2:]
    finally:
        gc.enable()
    print(name, statistics.median(samples) * 1000, "ms")
```

For the reader diagnostic:

```bash
taskset -c 2 target/bench-gil/bin/python benches/stream_read_lifecycle.py \
  --rounds 100000 --repeat 7 --output target/event-loops-97fa7cf-reads.json
```

Local raw measurements, logs and build metadata from this refresh are in
`target/benchmarks-refresh/`: `gil-one-core.json`, `gil-two-cores.json`,
`free-threaded-one-core.json`, `network.json`, `churn-<loop>.json`, `reads.json`
and `environment.json`. These are local build artifacts. All comparison
columns and reader rows above are calculated directly from those samples.
The subprocess memory regression and 34 other benchmark tooling tests pass.

## Historical optimization measurements

The sections below retain earlier before/after experiments against their named
commits. Their numbers and implementation descriptions refer to those stages;
the freshly measured all-loop comparisons above describe the current build.

### Stream optimizations from ANALYSIS.md

Measured October 14, 2026 against baseline commit `c9aa601`, using release
builds of rsloop 0.1.64, CPython 3.14.7 with the GIL enabled, and Linux
7.0.0-38-generic x86_64. Both versions ran on CPU 2. Values below are medians
of seven measured runs; throughput changes are `before / after - 1`.

The implementation recycles materialized Python-backed write batches through
`WriteBufferPool`, reads closing/writable/backpressure state under one lock on
the immutable-bytes write path, caches the weak read-waiter callback, and
reuses consumed immediate-read awaitables when the reader owns their sole
reference. Awaitable reuse is enabled only on GIL builds. Waiting exact reads
copy incoming bytes directly into the private Python bytes accumulator after
reconciling cancellation; trailing data remains available for the next read.
TLS plaintext is copied through rustls `BufRead` without zero-filling the
buffer, and the shared TLS session lock is released before waiting for a read
pool slot.

Buffered and waiting read lifecycle benchmark (nanoseconds per round, lower
is better). Each method has a 1,000-round warmup and 100,000 measured rounds
per run. Waiting reads include dispatch of completion callbacks.

| Workload | Before (ns) | After (ns) | Throughput change |
|---|---:|---:|---:|
| Buffered `read` | 316.4 | 290.0 | +9.1% |
| Buffered `readexactly` | 319.2 | 292.0 | +9.3% |
| Buffered `readuntil` | 346.2 | 323.2 | +7.1% |
| Waiting `read` | 2,407.9 | 2,389.7 | +0.8% |
| Waiting `readexactly` | 1,543.0 | 1,500.0 | +2.9% |
| Waiting `readuntil` | 1,603.0 | 1,514.7 | +5.8% |

Network workload matrix: two warmups, seven measured runs, 16 concurrent
connections, and 500 operations per connection (8,000 operations per run).
HTTP uses 4,096-byte bodies; WebSocket frames cycle through 32, 256, and
4,096 bytes. Timings include connection setup and teardown.

| Workload | Before (ms) | After (ms) | Throughput change |
|---|---:|---:|---:|
| HTTP keep-alive | 147.36 | 144.55 | +2.0% |
| TLS HTTP | 374.43 | 355.25 | +5.4% |
| Raw WebSocket | 1,592.68 | 1,599.06 | -0.4% |
| Raw TLS WebSocket | 1,882.32 | 1,862.54 | +1.1% |

These are local shared-host measurements of the combined changes, without
isolation of each optimization. Some runs overlapped build/test activity on
the host. Small changes, especially the WebSocket results, should be treated
as noise: plain WebSocket ranges overlap (1,575.9–1,601.4 ms before versus
1,591.5–1,637.6 ms after). TLS HTTP ranged from 371.9–379.4 ms before and
352.4–363.6 ms after. The analysis's predicted 15–25% TLS gain was not observed
in this workload.

Validation: 358 Python stream/TLS tests and 84 Rust stream tests passed,
including regressions for retained awaitables, cancellation before its deferred
callback, pooled write reuse, and TLS drain boundaries/close-notify. Clippy
passed with warnings denied.

Reproduce each version from its repository root, changing `before` to `after`
for the modified build:

```bash
VIRTUAL_ENV="$PWD/.venv" .venv/bin/maturin develop --release --locked
taskset -c 2 .venv/bin/python benches/stream_read_lifecycle.py \
  --rounds 100000 --repeat 7 --output target/analysis-before-reads.json
taskset -c 2 .venv/bin/python benches/workload_matrix.py \
  --loops rsloop \
  --scenarios http_keepalive,tls_http,websocket_messages,websocket_tls \
  --concurrency 16 --requests-per-connection 500 --warmups 2 --repeat 7 \
  --json-output target/analysis-before-network.json
```

Raw measurements from this session are saved in
`target/analysis-{before,after}-{reads,network}.json` (local build artifacts).


### Updated report: accept routing and runtime overhead

Measured October 14, 2026 against commit `4eb28e1` (which already contains the
stream optimizations above). Both versions use rsloop 0.1.64 release builds,
CPython 3.14.7 with the GIL enabled, and CPU 2 on the same Linux host. Each
value is the median of seven measured runs after two warmups. These benchmarks
ran without concurrent build or test work from this session.

The updated report's highest-priority accept findings apply to the current
code. Server startup uses a background bridge; its accept-start command now
routes to the active loop runtime, while hostname resolution remains on its
existing path. Accept cancellation also works between loop runs. TCP and Unix
listeners can hand off a standard owned socket directly, skipping temporary
async-stream registration/deregistration and unused peer-address queries.
Linux raw accepts use multishot accept with CLOEXEC and NONBLOCK set by the
kernel; the existing address-returning listener APIs retain their behavior.

Other changes reuse empty callback contexts only when the GIL is held, the
context remains empty after exit, and no retained token references it. Loop
parking borrows its wake state and pins Sleep on the stack. Runtime::block_on
reuses its root notifier/waker, local task wakes skip Weak::upgrade and thread
ID lookup, and empty timer heaps skip the clock read. The runtime's task-batch
allocation policy remains as before; these changes do not make all parking
allocation-free.

Time is milliseconds; negative differences are faster. Throughput change is
`before / after - 1`.

| Workload | Before (ms) | After (ms) | Difference (ms) | Throughput change |
|---|---:|---:|---:|---:|
| callbacks | 176.36 | 175.37 | -0.99 | +0.57% |
| tasks | 310.11 | 311.88 | +1.77 | -0.57% |
| tcp_streams | 363.20 | 361.78 | -1.42 | +0.39% |
| http_keepalive | 142.55 | 142.06 | -0.50 | +0.35% |
| tls_http | 356.84 | 356.43 | -0.41 | +0.12% |
| websocket_messages | 1598.36 | 1590.18 | -8.17 | +0.51% |
| websocket_tls | 1871.70 | 1843.38 | -28.32 | +1.54% |
| 8,000 short TCP connections | 975.57 | 863.89 | -111.68 | +12.93% |

Callbacks schedule 1,000,000 items; tasks execute 200,000 tiny tasks in batches
of 5,000; TCP performs 20,000 round trips with 1,024-byte payloads. The network
matrix uses the same 16 connections and 500 operations per connection as the
previous section. Short-connection churn uses 16 concurrent clients, each
opening, validating a one-byte echo, and closing 500 fresh connections. Its
timer includes connection setup, data exchange, connection teardown, and
server shutdown, and excludes listener creation. GC is disabled in that
benchmark. Each churn sample uses a new loop and server.

The short-connection result is the clearest improvement: before samples ranged
from 970.41–985.35 ms, and after samples from 860.36–868.16 ms. General workload
changes of roughly 0–1.5% remain within local shared-host variability; the
report's predicted large callback/steady-state gains were not demonstrated.
The combined measurements do not isolate individual optimizations.

A separate `strace -f -qq -c` sample with 128 fresh connections confirms the
accept path removes calls in the selected syscall categories below. Traced
runs are excluded from the timing table; these counts do not cover all
syscalls and do not establish the report's estimate of 5–6 calls per accept.

| Selected syscall | Before calls | After calls |
|---|---:|---:|
| `fcntl` | 520 | 392 |
| `getpeername` | 384 | 256 |
| `io_uring_register` | 515 | 387 |
| `getsockname` | 770 | 770 |

Trace summaries are saved in `target/report-v2-{before,after}-syscalls.txt`.

Validation: 637 Python tests passed (13 skipped, 176 deselected); all 356 Rust
unit tests passed. New regressions cover tokens retaining an empty context,
accepted socket flags/data/ownership, and successive borrowed block_on roots
after a panic. Clippy passed with warnings denied, and the embedded runtime
compiled for x86_64 Windows GNU. The profile-enabled build also compiled.
Runtime behavior was tested on Linux.

Reproduce against each revision after building it in release mode:

```bash
taskset -c 2 .venv/bin/python benches/compare_event_loops.py \
  --loops rsloop --repeat 7 --warmups 2 --callbacks 1000000 --tasks 200000 \
  --task-batch-size 5000 --tcp-roundtrips 20000 --payload-size 1024 \
  --json-output target/report-v2-after-loops.json
taskset -c 2 .venv/bin/python benches/workload_matrix.py \
  --loops rsloop \
  --scenarios http_keepalive,tls_http,websocket_messages,websocket_tls \
  --concurrency 16 --requests-per-connection 500 --warmups 2 --repeat 7 \
  --json-output target/report-v2-after-network.json
taskset -c 2 .venv/bin/python benches/connection_churn.py \
  --connections 8000 --concurrency 16 --repeat 7 --warmups 2 \
  --output target/report-v2-after-churn.json
```

For baseline churn, use the same benchmark script with `4eb28e1`'s extension.
This session built that revision in `target/report-v2-baseline-src` and selected
its Python package with `PYTHONPATH`; the recorded native-module paths confirm
the two measurements loaded different builds. Raw samples are saved locally
in `target/report-v2-{before,after}-{loops,network,churn}.json`.

### Completed performance-report implementation (October 14, 2026)

Compared the release build at `2b9d618` with the completed report implementation
on an Intel Core i9-9900K, Linux 7.0.0-38-generic, CPython 3.14.7 with the GIL,
and the io_uring reactor. Both builds use the pinned Rust toolchain and lockfile.
Processes and their worker threads are pinned to CPU 2. Each result is the
median of seven measured runs after two warmups. Builds and correctness tests
finished before the recorded benchmark pass.

Times are milliseconds. The difference is after minus before; negative values
mean less elapsed time.

| Workload | Before (ms) | After (ms) | Difference (ms) |
|---|---:|---:|---:|
| 1,000,000 callbacks | 175.15 | 174.14 | -1.01 |
| 200,000 tasks | 313.39 | 304.01 | -9.38 |
| 20,000 TCP round trips | 363.81 | 361.53 | -2.28 |
| 8,000 HTTP requests | 141.61 | 139.64 | -1.97 |
| 8,000 TLS HTTP requests | 355.78 | 350.01 | -5.77 |
| 8,000 WebSocket messages | 1586.40 | 1598.72 | +12.33 |
| 8,000 TLS WebSocket messages | 1859.64 | 1857.35 | -2.29 |
| 8,000 TCP connections | 862.70 | 819.90 | -42.80 |

The largest measured reduction is connection churn: 42.80 ms (4.96%). Tasks
improve by 9.38 ms (2.99%). Plain WebSocket messages take 12.33 ms longer (0.78%);
this implementation does not produce a speedup in every workload. The small
changes in the other workloads should be read alongside run-to-run variation.
These timings measure whole workloads rather than individual function calls.

The network matrix uses 16 concurrent connections with 500 requests/messages
per connection, the default HTTP body and WebSocket frame sizes, and warm
measurement mode. TCP round trips use 1,024-byte payloads. Connection churn
checks one-byte echoes on 8,000 newly opened connections at concurrency 16.

The completed implementation covers all 55 report recommendations.
The allocation budget checks pass; Python suites pass on 3.10, 3.14 and
free-threaded 3.14, alongside the default and optional-feature Rust suites.

#### Reproduction

Build `2b9d618` from an isolated archive with `maturin build --release --locked`,
extract its wheel into a separate package directory, then run each command
below twice. Set `PYTHONPATH` to the baseline package directory for the first
pass and the candidate `python` directory for the second. Both passes use the
same `.venv/bin/python` interpreter.

```bash
taskset -c 2 .venv/bin/python benches/compare_event_loops.py \
  --loops rsloop --repeat 7 --warmups 2 --callbacks 1000000 \
  --tasks 200000 --task-batch-size 5000 --tcp-roundtrips 20000 \
  --payload-size 1024 --json-output target/report-full-<build>-loops.json

taskset -c 2 .venv/bin/python benches/workload_matrix.py \
  --loops rsloop --scenarios http_keepalive,tls_http,websocket_messages,websocket_tls \
  --concurrency 16 --requests-per-connection 500 --repeat 7 --warmups 2 \
  --json-output target/report-full-<build>-network.json

taskset -c 2 .venv/bin/python benches/connection_churn.py \
  --connections 8000 --concurrency 16 --repeat 7 --warmups 2 \
  --output target/report-full-<build>-churn.json
```

Replace `<build>` with `before` or `after`. Local raw samples are in
`target/report-full-{before,after}-{loops,network,churn}.json`; native binary
paths and SHA-256 hashes are recorded in `target/report-full-benchmark-builds.json`.
