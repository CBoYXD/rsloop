# Benchmarks

Measured October 19, 2026 at commit `45a7eaa`: rsloop 0.1.65 (release build,
io_uring), uvloop 0.22.1 and zuvloop 0.0.17 on an Intel Core i9-9900K,
Linux 7.0.0-38-generic x86_64. GIL-enabled runs use CPython 3.14.7;
free-threaded runs use CPython 3.14.0 with `PYTHON_GIL=0` for every loop.

Results are medians of seven runs after two warmups unless stated otherwise.
Times are milliseconds; lower is better. Benchmarks ran sequentially after
builds finished. This is a shared host, so small differences may be noise.
The different Python patch versions prevent isolating the GIL's effect.

## Callbacks, tasks and TCP

Each sample uses a fresh process with cyclic GC disabled:

- Callbacks: schedule 1,000,000 callbacks in one batch.
- Tasks: run 200,000 tasks that await `asyncio.sleep(0)`, in batches of 5,000.
- TCP: exchange 20,000 1,024-byte messages over one connection. rsloop uses
  native fast streams; the other loops use asyncio streams.

Peak RSS includes the interpreter and libraries, measured with Linux `VmHWM`.
CPU affinity includes worker threads. CPUs 2 and 3 are separate physical
cores; allowing two cores still runs one event loop per process.

### One core, GIL enabled (CPU 2)

Time (ms):

| Workload | asyncio | uvloop | zuvloop | rsloop |
|---|---:|---:|---:|---:|
| 1,000,000 callbacks | 543.76 | 251.22 | 180.84 | 172.42 |
| 200,000 tasks | 556.13 | 343.16 | 298.41 | 312.17 |
| 20,000 TCP round trips | 566.80 | 470.57 | 375.18 | 416.03 |

Peak RSS (MiB):

| Workload | asyncio | uvloop | zuvloop | rsloop |
|---|---:|---:|---:|---:|
| 1,000,000 callbacks | 200.9 | 234.5 | 125.9 | 103.8 |
| 200,000 tasks | 31.6 | 34.2 | 33.3 | 34.7 |
| 20,000 TCP round trips | 24.3 | 27.7 | 26.5 | 28.3 |

### Two cores, GIL enabled (CPUs 2–3)

Time (ms):

| Workload | asyncio | uvloop | zuvloop | rsloop |
|---|---:|---:|---:|---:|
| 1,000,000 callbacks | 544.80 | 255.77 | 179.15 | 173.40 |
| 200,000 tasks | 557.89 | 342.04 | 299.83 | 315.82 |
| 20,000 TCP round trips | 563.22 | 467.68 | 381.18 | 405.75 |

Peak RSS (MiB):

| Workload | asyncio | uvloop | zuvloop | rsloop |
|---|---:|---:|---:|---:|
| 1,000,000 callbacks | 200.9 | 234.5 | 125.9 | 103.7 |
| 200,000 tasks | 31.5 | 34.2 | 33.1 | 34.6 |
| 20,000 TCP round trips | 24.3 | 27.8 | 26.5 | 28.3 |

### One core, free-threaded (CPU 2)

Time (ms):

| Workload | asyncio | uvloop | zuvloop | rsloop |
|---|---:|---:|---:|---:|
| 1,000,000 callbacks | 597.72 | 297.20 | 197.54 | 214.96 |
| 200,000 tasks | 642.47 | 410.10 | 353.02 | 357.53 |
| 20,000 TCP round trips | 601.00 | 504.12 | 393.52 | 389.10 |

Peak RSS (MiB):

| Workload | asyncio | uvloop | zuvloop | rsloop |
|---|---:|---:|---:|---:|
| 1,000,000 callbacks | 207.9 | 256.5 | 132.9 | 186.2 |
| 200,000 tasks | 38.8 | 41.4 | 42.4 | 42.3 |
| 20,000 TCP round trips | 30.6 | 34.3 | 33.7 | 35.8 |

## Network workloads

CPython 3.14.7, GIL enabled, CPU 2. HTTP and WebSocket workloads use
16 connections with 500 operations each (8,000 total). HTTP uses 4,096-byte
bodies and eight SHA-256 iterations per request; raw WebSocket frames cycle
through 32, 256 and 4,096 bytes. GC is enabled. Each loop/scenario uses one
process for all samples, creating a fresh loop and server per sample.

Short TCP connections use 8,000 fresh connections at concurrency 16, checking
a one-byte echo before closing. Each loop uses its own process, with a fresh
loop and server per sample and GC disabled.

Times include connection setup and teardown. Short TCP timing also includes
server shutdown and excludes listener creation.

| Workload | asyncio | uvloop | zuvloop | rsloop |
|---|---:|---:|---:|---:|
| HTTP keep-alive | 246.61 | 149.77 | 127.38 | 139.65 |
| TLS HTTP | 513.99 | 319.40 | 333.97 | 345.32 |
| Raw WebSocket | 1675.11 | 1585.60 | 1570.79 | 1609.34 |
| Raw TLS WebSocket | 1920.41 | 1747.61 | 1777.40 | 1877.24 |
| 8,000 short TCP connections | 1187.17 | 875.43 | 782.51 | 796.91 |

## Native reader lifecycle

rsloop only, CPython 3.14.7, GIL enabled, CPU 2, GC enabled. Each method has a
1,000-round warmup and seven samples of 100,000 rounds. Waiting reads include
Future completion callbacks. Values are median nanoseconds per round.

| Operation | Buffered (ns) | Waiting (ns) |
|---|---:|---:|
| `read` | 275.6 | 2646.5 |
| `readexactly` | 272.4 | 1887.4 |
| `readuntil` | 295.2 | 1932.7 |

## Reproduce

Run from the repository root. Build both environments before measuring:

```bash
uv venv --python 3.14.7 target/bench-gil
uv venv --python 3.14.0t target/bench-free-threaded
for env in bench-gil bench-free-threaded; do
  uv pip install --python target/$env/bin/python maturin uvloop==0.22.1 zuvloop==0.0.17
  VIRTUAL_ENV="$PWD/target/$env" target/$env/bin/maturin develop --release --locked
done
uv run --no-project python scripts/generate_test_tls_certs.py tests/fixtures/tls
mkdir -p target/benchmarks
```

Run the general workloads with each interpreter and affinity:

```bash
compare() {
  taskset -c "$1" "$2" benches/compare_event_loops.py \
    --loops asyncio,uvloop,zuvloop,rsloop --repeat 7 --warmups 2 \
    --callbacks 1000000 --tasks 200000 --task-batch-size 5000 \
    --tcp-roundtrips 20000 --payload-size 1024 \
    --json-output "target/benchmarks/$3.json"
}
PYTHON_GIL=1 compare 2 target/bench-gil/bin/python gil-one-core
PYTHON_GIL=1 compare 2,3 target/bench-gil/bin/python gil-two-cores
PYTHON_GIL=0 compare 2 target/bench-free-threaded/bin/python free-threaded-one-core
```

Check CPU topology before choosing affinity on another host. Run the network
matrix and reader diagnostic with the GIL-enabled interpreter:

```bash
PYTHON_GIL=1 taskset -c 2 target/bench-gil/bin/python benches/workload_matrix.py \
  --loops asyncio,uvloop,zuvloop,rsloop \
  --scenarios http_keepalive,tls_http,websocket_messages,websocket_tls \
  --concurrency 16 --requests-per-connection 500 --warmups 2 --repeat 7 \
  --measurement-mode warm --json-output target/benchmarks/network.json
PYTHON_GIL=1 taskset -c 2 target/bench-gil/bin/python benches/stream_read_lifecycle.py \
  --rounds 100000 --repeat 7 --output target/benchmarks/reads.json
```

For short connections, run each loop in a separate process:

```bash
for loop in asyncio uvloop zuvloop rsloop; do
  PYTHON_GIL=1 taskset -c 2 target/bench-gil/bin/python - "$loop" <<'PYTHON'
import asyncio
import gc
import json
import statistics
import sys
from pathlib import Path

sys.path.insert(0, "benches")
from compare_event_loops import loop_factory_for
from connection_churn import measure

gc.collect()
gc.disable()
samples = [
    asyncio.run(asyncio.wait_for(measure(8000, 16), 120),
                loop_factory=loop_factory_for(sys.argv[1]))
    for _ in range(9)
][2:]
Path(f"target/benchmarks/churn-{sys.argv[1]}.json").write_text(json.dumps(samples))
print(sys.argv[1], statistics.median(samples) * 1000, "ms")
PYTHON
done
```

Raw samples and logs for this refresh are local build artifacts in
`target/benchmarks-2026-10-19/`: `gil-one-core.json`, `gil-two-cores.json`,
`free-threaded-one-core.json`, `network.json`, `churn-<loop>.json` and
`reads.json`. The `environment-*.json` files record Python/package versions,
GIL state, build profile, reactor and native binary hashes.
