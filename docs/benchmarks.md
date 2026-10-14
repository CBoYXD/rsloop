# Benchmarks

## Four-loop comparison on Linux

Measured on October 6, 2026 at commit `43f624a` on an Intel Core i9-9900K,
Linux 7.0.0-38-generic (x86_64), with rsloop 0.1.57 built in release mode,
uvloop 0.22.1, and zuvloop 0.0.17. The GIL-enabled runs use CPython 3.14.7;
the free-threaded run uses CPython 3.14.0 with the GIL disabled for all four loops.
Each entry is the median of seven measured runs after two warmups, with each
run in a fresh subprocess. Cyclic garbage collection is disabled during each run.

The workloads schedule 1,000,000 callbacks as one batch, run 200,000 tiny tasks
that each await `asyncio.sleep(0)` in batches of 5,000, and perform 20,000 TCP
round trips with 1,024-byte payloads. TCP uses rsloop's native fast streams;
the other loops use stdlib asyncio streams.

Times are milliseconds (lower is better). Memory tables report the median
of each process's peak RSS in MiB, including the interpreter and libraries.
CPU affinity applies to each process and its threads. Allowing two physical
cores still runs one event loop per process; it does not partition callbacks
or tasks across two loops.

### One core, GIL enabled (CPU 2)

Median time (ms):

| Workload | asyncio | uvloop | zuvloop | rsloop |
|---|---:|---:|---:|---:|
| 1,000,000 callbacks | 550.3 | 258.7 | 179.5 | 182.1 |
| 200,000 tasks | 566.2 | 341.7 | 307.6 | 325.2 |
| 20,000 TCP round trips | 565.7 | 485.4 | 379.6 | 335.4 |

Peak RSS (MiB):

| Workload | asyncio | uvloop | zuvloop | rsloop |
|---|---:|---:|---:|---:|
| 1,000,000 callbacks | 201.2 | 234.7 | 126.0 | 111.7 |
| 200,000 tasks | 31.6 | 34.5 | 33.5 | 34.8 |
| 20,000 TCP round trips | 29.8 | 29.8 | 29.8 | 29.8 |

### Two cores, GIL enabled (CPUs 2–3)

Median time (ms):

| Workload | asyncio | uvloop | zuvloop | rsloop |
|---|---:|---:|---:|---:|
| 1,000,000 callbacks | 674.3 | 260.7 | 184.2 | 195.2 |
| 200,000 tasks | 571.6 | 350.3 | 305.7 | 324.0 |
| 20,000 TCP round trips | 569.3 | 473.8 | 381.2 | 327.9 |

Peak RSS (MiB):

| Workload | asyncio | uvloop | zuvloop | rsloop |
|---|---:|---:|---:|---:|
| 1,000,000 callbacks | 201.2 | 234.8 | 126.0 | 111.6 |
| 200,000 tasks | 31.6 | 34.4 | 33.5 | 34.6 |
| 20,000 TCP round trips | 29.7 | 29.7 | 29.7 | 29.7 |

### One core, free-threaded (CPU 2)

Median time (ms):

| Workload | asyncio | uvloop | zuvloop | rsloop |
|---|---:|---:|---:|---:|
| 1,000,000 callbacks | 913.1 | 575.9 | 216.5 | 226.8 |
| 200,000 tasks | 671.1 | 422.3 | 378.7 | 398.3 |
| 20,000 TCP round trips | 627.8 | 519.5 | 406.3 | 345.4 |

Peak RSS (MiB):

| Workload | asyncio | uvloop | zuvloop | rsloop |
|---|---:|---:|---:|---:|
| 1,000,000 callbacks | 207.8 | 256.2 | 132.8 | 193.5 |
| 200,000 tasks | 38.8 | 41.3 | 42.4 | 42.4 |
| 20,000 TCP round trips | 36.4 | 36.4 | 36.4 | 36.4 |

### Interpretation and reproduction

Zuvloop led callbacks and tasks in all three runs; rsloop led TCP. On one core
with the GIL enabled, rsloop took 1.5% longer for callbacks and 5.7% longer for
tasks than zuvloop, while delivering 13.2% higher TCP throughput. Its callback
peak RSS was 111.7 MiB versus zuvloop's 126.0 MiB. With the GIL disabled,
rsloop took 4.8% longer for callbacks and 5.2% longer for tasks, with 17.6%
higher TCP throughput; its callback peak RSS rose to 193.5 MiB versus
zuvloop's 132.8 MiB.

These local microbenchmarks ran on a shared host. Callback timings were
particularly variable: the two-core rsloop runs ranged from 185.4 to 310.0 ms,
and free-threaded asyncio ranged from 690.4 to 1,080.4 ms. The separate runs
and different Python patch versions do not isolate the effects of core count,
the GIL, or the latest commit. These are not general application performance
claims.

To reproduce the GIL-enabled setup from the repository root:

```bash
uv venv --python 3.14.7 target/bench-gil
uv pip install --python target/bench-gil/bin/python maturin uvloop==0.22.1 zuvloop==0.0.17
VIRTUAL_ENV="$PWD/target/bench-gil" target/bench-gil/bin/maturin develop --release --locked
taskset -c 2 target/bench-gil/bin/python benches/compare_event_loops.py \
  --loops asyncio,uvloop,zuvloop,rsloop --repeat 7 --warmups 2 \
  --callbacks 1000000 --tasks 200000 --task-batch-size 5000 \
  --tcp-roundtrips 20000 --payload-size 1024 \
  --json-output target/event-loops-43f624a-one-core.json
```

For two physical cores on this host, replace `taskset -c 2` with
`taskset -c 2,3` and use a different output filename. Check CPU topology before
choosing affinity on another machine.

For the free-threaded setup:

```bash
uv venv --python 3.14.0t target/bench-free-threaded
uv pip install --python target/bench-free-threaded/bin/python maturin uvloop==0.22.1 zuvloop==0.0.17
VIRTUAL_ENV="$PWD/target/bench-free-threaded" target/bench-free-threaded/bin/maturin develop --release --locked
PYTHON_GIL=0 taskset -c 2 target/bench-free-threaded/bin/python benches/compare_event_loops.py \
  --loops asyncio,uvloop,zuvloop,rsloop --repeat 7 --warmups 2 \
  --callbacks 1000000 --tasks 200000 --task-batch-size 5000 \
  --tcp-roundtrips 20000 --payload-size 1024 \
  --json-output target/event-loops-43f624a-free-threaded.json
```


## Stream optimizations from ANALYSIS.md

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
