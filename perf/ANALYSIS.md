# Why rsloop 0.1.64 is slower than uvloop on copytrade's investor_api, and the fix

Fork branch `pipeline/rsloop-66130a3e`, based on upstream v0.1.64 (`c9aa601`).
Task: copytrade eff35519 (issues #1120/#1186). All numbers are from the
operator's machine, shared with other agents: an i7-13650HX with 20 logical
CPUs and 31 GiB.
- CPUs 0–11 are 6 hyper-threaded P-cores (4.7–4.9 GHz); 12–19 are E-cores
  (3.6 GHz).
- Agents run under a CPU quota (3 cores for this agent, which is why `nproc`
  reports 3). The benchmark containers do not; each is capped by its own
  `--cpus`.

Every number below carries the 1-minute load average (`load1`) measured with
it. The benchmarks of fixes 4 and 5 also carry the CPU used outside the
benchmark's containers (see [What fixes 4 and 5 cost](#what-fixes-4-and-5-cost)).

**Status of the evidence.** The full-service table
([Latest full run](#latest-full-run-b15c357)) was measured at `b15c357`
(opt 3) and predates fixes 4 and 5. The final quiet-gated rounds that would
have re-measured the services on `6101e34` were **not run** (operator
decision, 2026-10-12), and that table's admin_api round 2 is contaminated by
host load. See [Status and what is unverified](#status-and-what-is-unverified).

## Summary

- **Root cause.** rsloop's loop thread spun for up to 50 µs, with the GIL
  released, waiting for a cross-thread wake before parking
  (`src/engine/loop_core.rs:128-143` and `:1047-1090` at `c9aa601`).
  - Behind Granian, those wakes come from Python threads
    (`loop.call_soon_threadsafe` in `granian/_futures.py`) that still hold the
    GIL when the flag flips. The spinning loop thread therefore queued for the
    GIL behind them, paying up to the 5 ms switch interval per reattach.
  - As a result, request starts and completions reached the loop in bursts.
    investor_api's SQLAlchemy pool (4 + 2 overflow per Granian worker, 8
    requests per worker at c16) then kept discarding returned connections into
    a full idle queue and opening new ones.
  - Measured on the real service at c16: **33–53 new Postgres sessions per 1k
    requests on rsloop vs 3–4 on uvloop**. Each new session costs a SCRAM
    login (PBKDF2), asyncpg setup and a getaddrinfo thread. That was 6–7% of
    all samples in `libcrypto` alone, and in total the whole ~450–650 µs CPU
    per request by which rsloop trailed. The container runs at its 2-CPU
    quota, so CPU per request is throughput.
- **Why the gap had this shape.** At c64 investor_api's pool is permanently
  saturated (waiters always queued), so a check-in rarely finds a full queue:
  rsloop was level there. admin_api's pool (8 + 4) is larger than its
  per-worker c16 concurrency, so the burst mechanism above barely touched it.
- **Second, smaller cause: turns ran to quiescence.** It surfaced on
  admin_api once the spin was gone. Each `run_forever` turn took in the
  callbacks it scheduled itself (every 64 items, up to 1024) and polled the
  reactor only when nothing was left (`src/engine/loop_core.rs:805-857` at
  `c9aa601`).
  - Under saturation, DB responses piled up between polls and completed in
    one burst. More connections came back than the pool's idle queue holds
    (8) before any woken waiter ran.
  - Measured on admin_api `/auth/me` at c64 on `5cf71aa`: about 690
    full-queue check-ins in 20 s and 20 new sessions per 1k requests, vs
    0.1–3.7 sessions per 1k on uvloop (at most a few full-queue check-ins).
- **The cross-thread hop** through the coordination thread on every
  `call_soon_threadsafe` is real but small: about 4 µs of CPU per request on
  the `rsloop` thread, and 11.4 → 7.8 µs per cross-thread callback in a
  microbenchmark.
- **Fixes (3 optimisations, 2 correctness fixes).**
  1. `5d2ce85` enqueues off-thread handles directly into the active run.
  2. `5cf71aa` parks instead of spinning by default (`RSLOOP_WAKE_SPIN_US`
     now defaults to 0 and remains an opt-in).
  3. `b15c357` runs one callback generation per turn, as asyncio's
     `_run_once` does, and polls the reactor with the GIL released between
     turns, as uvloop does.
  4. `ce45ef8` keeps `call_soon` / `call_soon_threadsafe` callbacks in
     scheduling order across the two ready queues (a regression of opt 3
     found by review).
  5. `6101e34` fences the runtime's park against a racing cross-thread wake,
     which v0.1.64 itself loses about once per 300–450 #115 repro runs.
- **Result at `b15c357`** ([Latest full run](#latest-full-run-b15c357),
  3 interleaved rounds, load per row). It predates fixes 4 and 5, which were
  not re-measured on the services (see
  [Status and what is unverified](#status-and-what-is-unverified)):
  - investor_api: +3.9% vs uvloop without the bimodal `/health/` probe
    (geometric mean, worst row +1.5%); every c16 row that was 30–46% behind
    on 0.1.64 is at or above uvloop. Its rows ran at load1 1.1–3.8 with small
    round spreads.
  - admin_api: +2.0% vs uvloop (geometric mean, worst row −2.9%), **but round
    2 of that run was contaminated by host load**, so the "within noise"
    reading of the small differences is not supported.

## Setup

- **Harness.** The #1120 harness method (`~/.cache/ct1120-064/bench/bench.py`):
  - Granian 2.7.5 with 2 workers, CPython 3.14.5, image `--cpus 2`, oha in its
    own container (`--cpus 2`).
  - 15 s warm-up, then 60 s at c16 and at c64 per endpoint.
  - Arms interleaved and rotated per round.
  - Moved to an isolated compose project `rsperf` (own container, network and
    volume names; no host ports) at `~/.cache/rsloop-perf/`, with its own
    Postgres/Redis/NATS seeded by the services' dev entrypoints.
- **Arms.** uvloop 0.22.1, rsloop 0.1.64 (the image's pin), asyncio, and
  **fork**.
  - The fork arm uses the same image and code, with only `PYTHONPATH` putting
    the fork's wheel first.
  - The fork wheel is built with maturin (`--release --locked`, the
    repository's own release profile) on the image's base (Debian trixie,
    `python:3.14.5-slim`).
  - For the full-service runs it is installed into `rsperf-<api>:fork` by the
    compose project (`~/.cache/rsloop-perf/fork-image/Dockerfile`), at
    `/opt/rsloop-fork`.
  - A local build of unmodified `c9aa601` behaves like the PyPI 0.1.64 wheel
    (cross-thread ping-pong 8.9 vs 7.9 µs/op wall time and 15.8 vs 13.6 µs
    CPU, one smoke run, load 5.2).
- **Reactor.** In Docker the loop runs on **epoll**: the default seccomp
  profile refuses `io_uring_setup`, as in production. Microbenchmarks run in
  the same `python:3.14.5-slim` image under `--cpus 2`. The defect repros
  also run on the host, which uses **io_uring**.
- **New-session counter.** The harness now also reports new Postgres sessions
  per 1k requests (`pg_stat_database.sessions` delta), the counter that
  exposed the root cause.

## 1. Baseline: the 0.1.64 lane's numbers (`~/.cache/ct1120-064/run/bench`)

Median rps over 3 rounds:

| api | endpoint | c | uvloop | rsloop 0.1.64 | Δ | CPU-ms/1k uvloop / rsloop | load1 |
|---|---|---|---|---|---|---|---|
| investor_api | `/auth/me` | 16 | 2,057 | 1,150 | −44.1% | 972 / 1,740 | 2.2–6.2 |
| investor_api | `/auth/me` | 64 | 1,965 | 2,150 | +9.4% | 1,018 / 931 | 3.1–6.2 |
| investor_api | `/notifications/unread-count` | 16 | 1,543 | 1,093 | −29.1% | 1,295 / 1,830 | 3.5–6.1 |
| investor_api | `/notifications/unread-count` | 64 | 1,568 | 1,626 | +3.7% | 1,276 / 1,231 | 4.0–6.0 |
| investor_api | `/reports/` | 16 | 1,281 | 1,022 | −20.2% | 1,559 / 1,957 | 4.2–5.7 |
| investor_api | `/reports/` | 64 | 1,289 | 1,369 | +6.2% | 1,551 / 1,462 | 3.9–5.8 |
| admin_api | `/auth/me` | 16 | 1,689 | 1,888 | +11.7% | 1,182 / 1,051 | 3.3–5.4 |
| admin_api | `/positions/pnl-stats` | 16 | 1,003 | 881 | −12.2% | 1,984 / 2,256 | 3.2–5.5 |
| admin_api | `/trade-sessions/` | 16 | 475 | 450 | −5.3% | 4,196 / 4,416 | 3.5–5.2 |

At c16, rps × CPU-ms per request ≈ 2,000 ms/s for **both** loops: the
container is pinned at its 2-CPU quota. The throughput gap is therefore
exactly a CPU-per-request gap (investor `/notifications/unread-count`:
1,830 vs 1,295 ms per 1k requests, about 535 µs per request). The question
became where rsloop spends the extra CPU, and why only at c16.

## 2. Profiles of investor_api at c16 (`/notifications/unread-count`)

Tool: `~/.cache/rsloop-perf/bench/profile.py`. Raw outputs are under
`~/.cache/rsloop-perf/profiles/`.

- It starts the arm through the compose project and holds oha at c16.
- It reads per-thread CPU and context switches from `/proc` over a 20 s window.
- It then runs `strace -c -f` (10 s), `perf record -g` (15 s, 997 Hz) and
  py-spy from a privileged sidecar that joins the container's PID namespace.

### 2a. Where the CPU goes, per thread (20 s window, no tracer attached)

| arm | rps | cgroup CPU µs/req | loop threads (`python`) µs/req | `tokio-rt-worker` (Granian) µs/req | `rsloop` coordination thread µs/req | csw/req on `python` threads | new PG sessions /1k req | load1 |
|---|---|---|---|---|---|---|---|---|
| uvloop | 1,374 | 1,454 | 1,415 | 37 | — | 4.10 | — | 5.7 |
| rsloop 0.1.64 | 1,056 | 1,894 | 1,839 | 36 | 4.3 | 1.93 | — | 7.1 |
| fork `5d2ce85` (opt 1) | 1,087 | 1,839 | 1,789 | 35 | 0.0 | 2.00 | — | 7.6 |
| uvloop (rerun) | 1,645 | 1,215 | 1,186 | 35 | — | 4.02 | **4.2** | 6.8 |
| rsloop 0.1.64 (rerun) | 1,097 | 1,823 | 1,780 | 35 | 3.2 | 1.91 | **40.0** | 6.1 |
| uvloop (rerun) | 1,582 | 1,264 | 1,234 | 38 | — | 4.11 | **4.3** | 6.4 |
| rsloop 0.1.64 (rerun) | 922 | 2,097 | 2,051 | 42 | 3.8 | 2.94 | **33.6** | 8.7 |

- **The extra CPU is on the Python loop threads themselves.** The
  coordination thread (`rsloop`), rsloop's helper threads (`smol`,
  `blocking`) and Granian's tokio threads together account for less than
  10 µs/req of the difference.
- **rsloop context-switches less than uvloop** (1.9 vs 4.1 per request). The
  busy-wait replaced sleeping, so the waste does not show up as scheduler
  activity.

### 2b. perf: CPU by DSO (all threads of both workers, 15 s, ~33k samples each)

| DSO | uvloop | rsloop 0.1.64 |
|---|---|---|
| libpython3.14 | 65.4% | 63.6% |
| kernel | 17.0% | 11.8% |
| **libcrypto.so.3** | — (below 0.1%) | **6.4%** |
| asyncpg `protocol` | 1.7% | **4.7%** |
| libc | 2.4% | 3.0% |
| `_granian` | 5.4% | 2.6% |
| `_hashlib` | — | 0.6% |
| loop extension (`loop` / `_loop`) | 2.0% | 1.6% |

The top rsloop symbols include asyncpg's
`SCRAMAuthentication._bytes_xor` generator and `OPENSSL_cleanse`. In
`perf script`, **4,744 rsloop samples sit inside SCRAM authentication,
against 18 for uvloop**.

Postgres authenticates only when a connection is opened. Under rsloop the
service therefore kept opening database connections, which section 2a's
counter confirms: about 35–40 vs 4 new sessions per 1k requests.

### 2c. strace -c (10 s, both workers)

| syscall | uvloop | rsloop 0.1.64 |
|---|---|---|
| futex | 87,089 | 59,249 |
| epoll_wait / epoll_pwait | 30,865 / 49,822 | 44,151 / — |
| write / sendto | 114,387 / — | 19,226 / 58,899 |
| close | 6 | **1,378** |
| epoll_ctl | 12 | **1,652** |
| ioctl | — | **1,923** |
| newfstatat | 18 | **2,475** |
| threads created during the window | 0 | **275** |

The `close`, `epoll_ctl`, `ioctl` and `newfstatat` counts, and the new
threads (getaddrinfo/connect helpers), are the footprint of the reconnects.
uvloop's 114k `write` calls include its `uv_async` eventfd wakes for
`call_soon_threadsafe`; rsloop sends socket data with `sendto`. Neither
difference matters at this scale.

py-spy `--native` could not attach from the sidecar (empty output). The perf
call graphs above cover the same question.

## 3. Isolating the mechanism

Tool: `perf/bench/pool_churn.py` (also at `~/.cache/rsloop-perf/bench/`). It
reproduces investor_api's database shape on one loop, without Granian or the
application:

- `create_async_engine(pool_size=4, max_overflow=2, pool_pre_ping=True,
  pool_timeout=10)` on asyncpg against the rsperf Postgres;
- 8 driver threads, each posting a request through
  `loop.call_soon_threadsafe` (as Granian's `_CBScheduler` does) and blocking
  on a reply queue (closed loop, like oha c16 across 2 workers);
- each request runs 2 queries in one session.

It reports requests/s, process CPU per request, pool connects per 1k, and
three latencies:

- post → task start (the loop picking up a cross-thread request);
- service (checkout + queries + checkin);
- reply → driver resume.

Docker epoll, `--cpus 1` unless noted. Medians of 3 runs:

| arm | req/s | CPU µs/req | connects /1k | post→start µs | service µs | reply→resume µs | load1 |
|---|---|---|---|---|---|---|---|
| uvloop | 2,166 | 434 | 0.1 | 222 | 3,212 | 225 | 10.6–12.2 |
| rsloop 0.1.64 | 598 | 1,661 | 61.3 | 1,688 | 10,259 | 1,448 | 10.4–14.6 |
| fork opt 1 (direct enqueue) | 641 | 1,534 | 55.0 | 1,724 | 9,391 | 1,495 | 10.1–13.6 |
| fork opt 2 (no spin) | **2,593** | **377** | **0.2** | **39** | 2,996 | **52** | 10.3–12.5 |

The pool trace (`CHURN_TRACE=1`) shows what goes wrong under 0.1.64:

- Connections come back while the idle queue already holds 3–4 of them with
  no waiter: only 2–3 of the 8 requests are inside the database section.
  - At least 343 check-ins found the queue full and were discarded in 6 s.
  - At least 265 connections were created into an empty queue (the
    trace's top rows).
- The rest of the requests sit between the driver and the loop: 1.4–1.7 ms
  post→start, against 0.15–0.22 ms on uvloop.
- uvloop's trace has 2 full-queue check-ins, and its only creations are
  the 6 that first fill the pool.

Hypotheses tested:

| # | Hypothesis | Test | Verdict |
|---|---|---|---|
| H1 | Every off-thread `call_soon_threadsafe` takes two wakeups (channel → coordination thread → run queue → loop thread), `src/engine/loop_core.rs:479-497` and `src/engine/dispatcher.rs:206` at `c9aa601`. | microbench `xthread_pingpong`; per-thread CPU | **Confirmed, minor.** 11.4 → 7.8 µs CPU per cross-thread callback; `rsloop` thread 4.3 → 0 µs/req. Does not touch the churn (61 → 55 /1k). Fixed in opt 1. |
| H2 | The 50 µs loop-thread spin before parking (`loop_core.rs:128-143`, `:1047-1090`) hurts when wakes come from GIL-holding Python threads. | `RSLOOP_WAKE_SPIN_US=0` on the same build | **Confirmed, root cause.** Churn 55 → 0.2 /1k, post→start 1.7 ms → 39 µs, 4× req/s. Fixed in opt 2. |
| H2a | The spin starves the reactor: while consecutive spins keep catching wakes (up to `MAX_CONSECUTIVE_SPINS` = 64) the loop never polls its sockets. | Variant polling the reactor (`poll_once`) after every caught spin | **Refuted.** Still 55–82 connects /1k. |
| H2b | The spin burns the container's CFS quota and gets the cgroup throttled. | Same repro at `--cpus 2` and `--cpus 3` | **Refuted.** Churn identical (53–56 /1k) without a binding quota. |
| H2c | GIL convoy: the spinner sees the wake while its producer still holds the GIL and queues behind the driver threads passing the GIL among themselves (`take_gil` waits up to the 5 ms switch interval before forcing a drop). A parked loop wakes after the producer released the GIL and drains everything queued. | Latency split above; no-spin variant | **Consistent with every measurement.** Post→start and resume latencies drop 30–40×, and no other loop change matters. |
| H3 | Timer heap cost (SQLAlchemy/asyncpg `asyncio.timeout` per query). | microbench `timers_cancel` | **Refuted.** rsloop 1.48 µs/op vs uvloop 2.35. |
| H4 | Ready-queue / `call_soon` cost, GIL attach per callback. | microbench `soon_chain` | **Refuted.** rsloop 0.17 µs/op vs uvloop 0.45 (the drain holds the GIL across a batch). |
| H5 | Transport read/write path (loop-thread reader task, small-write staging for client protocols: `src/transport/stream/builder.rs:154`, `core_write.rs:416`). | `tcp_pingpong`, `tcp_pingpong_c8` | **Not material here.** With 8 connections the fork is level or ahead (8.2 vs 9.2 µs CPU/rt). A single strictly serial connection is still slower: 10.5 vs 8.0 µs CPU per round trip (0.1.64: 15.2). Left as is (see Remaining differences). |
| H6 | io_uring vs epoll. | Docker runs epoll (seccomp), as production does | **Not the cause.** The harness gap is on epoll. |
| H8 | Each turn runs to quiescence before the reactor is polled again (`loop_core.rs:805-857` at `c9aa601`), so I/O completions arrive in bursts larger than a pool's idle queue. | Pool trace inside admin_api (section 3a); opt 3 | **Confirmed, second cause.** Surfaced once the spin was gone: about 690 full-queue check-ins in 20 s on `5cf71aa` vs at most a few on uvloop. Fixed in opt 3. |
| H7 | Future/Task bridging with Granian's `CallbackScheduler`. | Granian threads ≈ 35–42 µs/req on every arm | **Refuted.** Same cost on every loop. |

### 3a. The second mechanism: completions in bursts (admin_api)

The first full 3-round run on `5cf71aa` (table under
[Intermediate full run](#intermediate-full-run-at-5cf71aa-before-opt-3))
left admin_api at a geometric mean of −4.6% vs
uvloop. Its new-session counter was still elevated (e.g. `/auth/me` c64:
uvloop 2.6, `5cf71aa` 14.1, 0.1.64 37.8 per 1k).

Three things ruled out the obvious sources:
- admin_api writes every INFO+ log record through a fresh `asyncpg.connect`
  (`infrastructure/observability/persistent_logs.py:97`, `:160`). Both arms
  persisted the same handful of lines during the window, so logging is not
  the difference.
- c16 puts at most 8 requests on a worker, against 8 pooled + 4 overflow
  connections.
- The standalone repro at admin's settings (8 + 4, 32 drivers) does not
  churn on the fork: 0.6 vs 0 per 1k, and it is faster than uvloop there.

So the pool was traced inside the real service. An opt-in `sitecustomize`
(`~/.cache/rsloop-perf/bench/sitecustomize.py`, `RSPERF_TRACE_POOL=1`) counts
every check-in and creation by (idle queue size, waiting getters, overflow)
in each Granian worker. admin_api `/auth/me` c64, 20 s windows, load 2.6–3.7:

| arm | rps | new PG sessions /1k | check-ins | check-ins into a full queue (8/8) | check-ins at 7/8 | pool `close` events |
|---|---|---|---|---|---|---|
| uvloop | 1,756 | 0.1 | 95,800 (top 25 states) | ≤ 3 (0.1 sessions/1k; not among the 25 most frequent states) | not among the top 25 | — |
| fork `5cf71aa` | 1,601 | 20.2 | 90,070 | ~690, with 5–20 getters waiting | ~2,000 | 698 |

On the fork, connections came back while their idle queue already held 7–8
and up to 20 requests were waiting: more check-ins happened in one burst than
woken waiters could run in between. On uvloop the 25 most frequent states all
have 0–2 idle connections.

The burst came from the drain: a turn took in every callback it scheduled
(refill every 64 items, up to 1024) and polled the reactor only when nothing
was left. A whole chain of response → request step → check-in → next
response therefore ran inside one turn. asyncio's `_run_once` runs only what
was ready when the iteration started and polls I/O again before anything
newer.

## 4. Optimisations (one per commit, each measured)

### opt 1: `5d2ce85` off-thread `call_soon_threadsafe` joins the active run directly

`LoopCore::schedule_ready_handle` (`src/engine/loop_core.rs:496`):

- An off-thread handle now goes straight into the active run's pending queue
  and wakes the loop once, while no earlier handle is still on the command
  channel.
- A counter of handles in flight on the channel (`channel_handles`, released
  by the dispatcher in `dispatch_ready_batch`, `src/engine/dispatcher.rs:488`)
  preserves each producer's FIFO order across run start and run end.
- `finish_run` (`dispatcher.rs:511`) now puts the run's queue (older work)
  ahead of commands received since the last dispatch.
- New test:
  `tests/test_fast_callbacks.py::test_threadsafe_producer_order_survives_run_restarts`.

| measure | rsloop (c9aa601 build) | opt 1 | uvloop | load1 |
|---|---|---|---|---|
| `xthread_pingpong` CPU µs/op (wall) | 11.4 (6.3) | 7.8 (5.7) | 5.9 (5.0) | 3.9 |
| `xthread_task_pingpong` CPU µs/op (wall) | 10.4 (6.2) | 6.9 (4.8) | 6.9 (6.9) | 3.9–4.2 |
| `granian_like` CPU µs/req | 56.8 | 50.1 | 48.3 | 4.6 |
| harness-shaped c16 window (profile.py, `/notifications/unread-count`) rps / CPU µs/req | 1,056 / 1,894 | 1,087 / 1,839 | 1,374 / 1,454 | 5.7–7.6 |

### opt 2: `5cf71aa` park instead of spinning by default

`wake_spin_window` (`src/engine/loop_core.rs:141`) now defaults to 0. The
spin code stays behind `RSLOOP_WAKE_SPIN_US`, documented in
`docs/configuration.md`.

Microbenchmarks (Docker epoll, `--cpus 2`, medians of 3, load1 9.9–12.4):

| bench | rsloop (c9aa601) | opt 2 | uvloop |
|---|---|---|---|
| `granian_like` CPU µs/req (wall) | 54.6 (82.2) | **41.1 (77.9)** | 50.0 (81.1) |
| `xthread_pingpong` CPU µs/op (wall) | 14.1 (8.3) | **6.5 (5.3)** | 7.6 (6.3) |
| `xthread_task_pingpong` CPU µs/op | 13.3 | 11.9 | 11.5 |
| `xthread_burst` CPU µs/op | 0.62 | 0.69 | 0.48 |
| `tcp_pingpong` CPU µs/rt | 15.2 | 10.5 | 8.0 |
| `tcp_pingpong_c8` CPU µs/rt | 8.4 | 8.2 | 9.2 |
| `soon_chain` µs/op | 0.17 | 0.17 | 0.45 |
| `timers_cancel` µs/op | 1.48 | 1.48 | 2.35 |
| `pool_churn` req/s, connects /1k | 598, 61.3 | **2,593, 0.2** | 2,166, 0.1 |

Harness, quick form (investor_api, c16, 30 s, 2 rounds, rotated):

| endpoint | round | uvloop | rsloop 0.1.64 | fork opt 2 |
|---|---|---|---|---|
| `/auth/me` | 1 (load 4.9–10.8) | 1,567 | 1,846 | 2,358 |
| `/auth/me` | 2 (load 2.7–3.0) | 2,315 | 1,401 (42.5 sessions/1k) | **2,400** (3.8/1k) |
| `/notifications/unread-count` | 1 (load 4.4–17.6) | 1,347 | 1,680 | 1,839 |
| `/notifications/unread-count` | 2 (load 3.0–3.6) | 1,848 | 1,101 (52.7/1k) | **1,898** (4.0/1k) |
| `/reports/` | 1 (load 3.6–13.7) | 917 | 1,242 | 1,512 |
| `/reports/` | 2 (load 3.2–3.7) | 1,378 | 1,240 (26.9/1k) | **1,553** (4.0/1k) |

### opt 3: `b15c357` one callback generation per turn; poll I/O with the GIL released

`run_forever` (`src/engine/loop_core.rs:841-878`):
- A turn now runs the callbacks queued when it started. Newer ones wait for
  the next turn, after a non-blocking reactor poll.
- That per-turn poll (`loop_core.rs:1044-1052`) runs under `py.detach`, as
  uvloop polls without the GIL.
  - A first version kept the GIL across the poll. Back-to-back turns then
    held it for the whole switch interval while `call_soon_threadsafe`
    producers waited, and the investor-shaped pool repro fell to 851 req/s at
    61 connects per 1k (load 2.1–2.6).
- A queued `stop()` (`stop_requested`) keeps the turn going until its Stop
  item runs, so stop still takes effect in the turn that requested it
  (`tests/test_local_timers.py::test_timer_scheduled_by_stopping_callback_survives_next_run`).
- New test: `tests/test_local_sockets.py::test_tcp_data_is_delivered_between_call_soon_generations`.
  A self-rescheduling `call_soon` chain delayed a readable TCP protocol by
  513 callbacks on `5cf71aa` and by 1 after; the test fails on `5cf71aa` and
  passes now.

Harness-shaped windows (`profile.py`, 20 s at steady oha load), two runs per
arm, interleaved:

| endpoint | c | uvloop rps (sessions/1k) | `5cf71aa` rps (sessions/1k) | `b15c357` rps (sessions/1k) | load1 |
|---|---|---|---|---|---|
| admin_api `/auth/me` | 64 | 1,746 / 1,748 (3.7 / 3.7) | 1,618 / 1,587 (19.9 / 20.1) | **1,936 / 1,892** (3.0 / 1.3) | 2.0–3.8 |
| investor_api `/notifications/unread-count` | 16 | 1,852 / 1,843 (5.0 / 1.4) | — | **1,918 / 1,947** (3.8 / 3.5) | 3.1–3.6 |

Microbenchmarks (Docker epoll, `--cpus 2`, medians of 3, load1 2.0–2.9) show
what the extra turns cost:

| bench | `5cf71aa` | `b15c357` | uvloop |
|---|---|---|---|
| `granian_like` CPU µs/req | 36.7 | 38.9 | 50.1 |
| `xthread_pingpong` CPU µs/op | 5.6 | 5.4 | 5.6 |
| `xthread_task_pingpong` CPU µs/op | 6.1 | 6.6 | 7.9 |
| `tcp_pingpong` CPU µs/rt | 7.4 | 8.3 | 7.6 |
| `tcp_pingpong_c8` CPU µs/rt | 7.9 | 8.0 | 7.5 |
| `soon_chain` µs/op | 0.17 | 0.42 | 0.41 |
| `timers_cancel` µs/op | 1.38 | 1.73 | 2.31 |
| `pool_churn` (4 + 2, 8 drivers) req/s, connects /1k | 2,418 / 3,030; 8.5 / 2.7 | 3,274 / 3,327; 0.5 / 0.2 | 3,310 / 3,200; 0.0 / 0.1 |

`soon_chain` is the deliberate cost: a self-rescheduling callback is now one
turn per step, as in asyncio and uvloop.

### fix 4: `ce45ef8` keep scheduling order across the two ready queues

Found by the independent review of `b15c357`.

- **The defect.** The loop thread queues its own `call_soon` handles in a
  lock-free local queue; other threads queue into the run's pending queue.
  Opt 3 left local handles in place until the next turn, and the next turn
  took the whole pending queue first (`src/engine/loop_core.rs:851-870` at
  `b15c357`). A `call_soon_threadsafe` made after an earlier `call_soon`
  therefore ran first whenever both waited for the same generation, even
  when a thread start, a lock or a queue ordered the two calls.
  - The review's repro: a callback runs `call_soon(record, 'older')`, a later
    one starts and joins a thread that runs
    `call_soon_threadsafe(record, 'newer')`. asyncio gives
    `['older', 'newer']`; `b15c357` gave `['newer', 'older']`.
  - `c9aa601` passes that case but already failed a tighter one: 50
    alternating `call_soon` / thread-posted `call_soon_threadsafe` calls from
    one callback do not interleave in call order there either.
- **The fix.** Every off-thread item records how many items the loop thread
  had queued for itself when it was queued (`LoopCore::local_enqueues`,
  written only by the loop thread with a plain load and store). The drain
  starts each generation by merging both queues on that mark
  (`merge_ready_generation`, `src/engine/loop_core.rs:1868`): an off-thread
  item goes behind exactly the local items queued before it.
  - Marks are stored once per change (`PendingReady`,
    `src/engine/commands.rs:86`), so a burst of off-thread items shares one.
  - Work queued before a run (through the coordination thread) carries the
    mark read before the run could queue anything, so it still goes first.
  - Generations stay bounded, so opt 3's I/O interleaving is unchanged.
- **Tests.** `tests/test_fast_callbacks.py`:
  `test_earlier_call_soon_runs_before_later_threadsafe_call` (0, 5 and 130
  queued callbacks), `test_call_soon_and_threadsafe_calls_interleave_in_scheduling_order`
  and `test_threadsafe_call_runs_before_later_call_soon`, each against
  asyncio and rsloop. Rust unit tests cover the merge
  (`generation_order_tests`).
  - On `b15c357`, 4 of the 5 rsloop cases fail; on `c9aa601`, the
    interleaving case fails.
  - The review's exact test file passes on `ce45ef8` and later on the host
    (io_uring) and in Docker (epoll).

### fix 5: `6101e34` a cross-thread wake could be lost by the runtime park

Found while re-running the #115 repro for fix 4, many more times than
before.

- **Symptom.** In Docker (epoll), `issue1_repro.py` hung (killed by the 30 s
  timeout, no output) in about 1 of 300–450 runs, on every build:

  | build | hung runs |
  |---|---|
  | `c9aa601` (v0.1.64) | 2 of 440 |
  | `b15c357` | 1 of 330 |
  | `ce45ef8` | 1 of 110, 1 of 440, 0 of 418 |

  The earlier 110-run checks had simply not hit it.
- **What was stuck.** A core dump of a hung `c9aa601` run showed:
  - the loop thread cycling through 40–50 ms `epoll_wait` parks, still alive;
  - the coordination thread in `epoll_wait` with timeout −1.

  A SIGALRM probe in a hung run showed the run-2 task of
  `run_until_complete(asyncio.sleep(2))` still at its `def` line: its first
  step, queued before `run_forever`, never ran. Work queued before a run
  reaches the run only through the coordination thread, which was asleep.
- **Root cause.** The runtime's `block_on` and its wakers use a store-then-load
  handshake on two flags (`src/vibeio/executor.rs:290-299` and `:885-895`
  at `c9aa601`):
  - The waker stores the root's `ready` flag, then loads `waiting`.
  - The park stores `waiting`, then loads `ready`.

  Both sides used only release/acquire ordering. That allows store-load
  reordering, as x86's store buffer does. Both loads can read the old
  value: the waker skips the interrupt, and the park sleeps through the
  wake. The loop thread recovers at its next ≤ 50 ms park deadline. The
  coordination thread has no timer and never wakes again. The remote
  task-queue wake (`src/vibeio/task.rs:77-94`) has the same shape.
- **The fix.** A `SeqCst` fence between store and load on the park side and
  on both waker sides. The fence costs one `mfence` per cross-thread wake;
  same-thread wakes skip it.
- **Tests.** `src/vibeio/executor.rs`:
  `block_on_never_misses_a_wake_racing_with_the_park` and
  `spawned_task_never_misses_a_wake_racing_with_the_park`.
  - Each ping-pongs 100,000 wakes between a thread and the park on the mio
    driver, and fails if progress stops for 5 s.
  - Before the fix, the first test stalled at round 4,618. After it, 15
    consecutive runs of both passed (3 M wakes).
  - The #115 repro then ran 2,200 times in Docker without a hang. At the
    old rate, about 5–7 hangs were expected.

### What fixes 4 and 5 cost

Both touch hot paths: every off-thread enqueue now reads the mark, the
drain merges two queues, and every cross-thread wake pays a fence.

Setup and gating:
- Docker epoll, `--cpus 2`, rotated arms.
- Each bench started only once CPU outside the benchmark's containers was
  ≤ 2.0 cores, and was rerun if that went above 2.5 cores over the bench.
- The micro harness starts a container per run, and that start/stop is
  charged to dockerd and the CLI. That is why the column below reads 1.8–2.7
  cores even on a quiet host.

Results (CPU µs per op, wall in parentheses; medians of 3 rounds, 5 for the
`xthread` rows):

| bench | `b15c357` (opt 3) | `6101e34` (fixes 4–5) | uvloop | CPU outside benchmark, cores | load1 |
|---|---|---|---|---|---|
| `xthread_pingpong` | 6.22 (5.06) | 6.13 (4.99) | 6.76 (5.71) | 2.38 | 1.3–1.6 |
| `xthread_task_pingpong` | 6.46 (6.02) | 6.31 (5.91) | 6.84 (6.71) | 2.42 | 1.7–1.8 |
| `xthread_burst` (4 producers, 200k posts) | 0.49 (0.36) | 0.55 (0.38) | 0.47 (0.48) | 2.31 | 1.6 |
| `tcp_pingpong` | 8.16 (11.96) | 7.74 (11.20) | 6.35 (9.95) | 2.66 | 2.1–2.2 |
| `tcp_pingpong_c8` | 7.22 (8.62) | 6.86 (8.22) | 7.22 (8.77) | 2.49 | 1.7–1.8 |
| `granian_like` CPU µs/req | 35.3 (74.6) | 36.6 (74.9) | 38.6 (75.5) | 2.29 | 1.6–1.8 |
| `soon_chain` | 0.46 | 0.45 | 0.44 | 2.10 | 1.6–1.8 |
| `timers_cancel` | 1.83 | 1.82 | 2.31 | 2.06 | 1.8–2.0 |

- Every row is within its round-to-round spread except `xthread_burst`.
  - There 4 Python threads post 50k callbacks each while contending for the
    GIL. The fork is 0.04–0.06 µs per post slower in three separate runs
    (rounds overlap: 0.44–0.56 vs 0.43–0.53).
  - A variant with the counter on its own cache line measured 0.60, so the
    gap is not false sharing on that field. It was not kept.
  - At about 2 off-thread posts per request, that is ~0.1 µs on a request
    costing ~1,000 µs of CPU.
- `pool_churn` (4 + 2 pool, 8 driver threads, `--cpus 1`, 3 rounds, load
  1.4–2.9):

  | | `b15c357` | `6101e34` | uvloop |
  |---|---|---|---|
  | req/s (median) | 3,133 | 3,190 | 3,188 |
  | connects /1k | 0.1–0.2 | 0.2–0.4 | 0.0 |
  | post→task start µs | 101–109 | 184–187 | 148–150 |

  - Throughput and churn are unchanged.
  - Post→start rose because an off-thread request no longer runs ahead of
    loop-thread work queued before it, which is fix 4's point. uvloop sits
    between the two.

## 5. Correctness

| check | where | result |
|---|---|---|
| `cargo test` (`scripts/run_rust_tests.py`, as `just test-rust`) | host | 352 passed after each of opts 1–3; 355 at `ce45ef8` (+ 3 merge tests); 357 at `6101e34` (+ 2 wake-race tests). Needs a short `TMPDIR`: this session's default is too long for Unix-socket test paths (`SUN_LEN`). |
| `cargo test --all-features` (`scripts/run_rust_tests.py --all-features`) | host | 455 at `ce45ef8`, 457 at `6101e34` |
| Python core suite (`scripts/run_python_tests.py tests/test_*.py`) against the release wheel | host io_uring (CPython 3.14.7) and Docker epoll (3.14.5) | 642 passed, 12 skipped on both at `ce45ef8` and `6101e34` (632 + the 10 new ordering cases) |
| Earlier fast subset (`-m "not tooling and not stress and not slow_network"`, as `just test-fast`) | host, io_uring | opt 1: 525 passed. opt 2: 526 (+ the producer-order test). opt 3: 527 (+ the generation test). 141 skipped each time. |
| Stress suite (`--stress-iterations -m stress`, as `just test-stress`) | host | 5 passed at `b15c357`, `ce45ef8` and `6101e34` |
| The review's ordering test (`test_review_ordering.py`, base/head wheels built separately) | host io_uring and Docker epoll | `c9aa601` 2 passed; `b15c357` 1 failed; `ce45ef8` and `6101e34` 2 passed on both |
| `cargo clippy --all-targets --all-features --locked -- -D warnings` (as `just clippy`), `cargo fmt --check`, ruff (incl. `perf/bench/`), pyright on the changed tests | host | clean at `6101e34` |
| #115 greenlet lost wakeup / SIGSEGV (`issue1_repro.py`, every depth) | host io_uring (5 runs/depth) and Docker epoll (10 runs/depth, 110 runs) | 0 stuck, 0 SIGSEGV, 0 nonzero exits at `6101e34` |
| #115 at high repetition (same repro, Docker epoll, 4 containers in parallel) | Docker epoll | `6101e34`: 0 of 2,200 runs hung. Before fix 5: 1 hang per 300–450 runs on every build, `c9aa601` included (fix 5) |
| #116 protocol leak (`issue2_repro.py`) and stdlib StreamReaderProtocol churn | host and Docker | 0 protocols alive, +0 B per connection |
| asyncpg connect/close churn | Docker | 0 protocols alive |
| #118 cancelled TimerHandle retention (`repro.py`, `asyncpg_repro.py`) | host and Docker | 0/20000 alive in A/B/C; 0 statement caches or protocols alive |

The defect script ran on `5cf71aa`, on `b15c357` and on `6101e34`, with
identical results. Script: `~/.cache/rsloop-perf/defects-fork.sh <site>`, the
#1186 commands pointed at the fork build. Results:
`~/.cache/rsloop-perf/run/defects-opt2/`, `run/defects-head-b15c357/` and
`run/defects-head-6101e34/`.

- On `6101e34`, the rsloop part of the Docker `asyncpg_repro.py` step printed
  nothing once, while asyncio and uvloop printed theirs.
  - The step took its full length. No core dump was recorded, and the repro
    flushes every sample, so the container output was lost rather than the
    run crashing.
  - Reruns: 10 runs of the rsloop arm alone and 2 runs of the exact
    three-loop step all exited 0 with 0 statement caches, states and
    protocols alive (`run/defects-head-6101e34/i118_asyncpg_docker_*rerun*.txt`).
- The 2,200 high-repetition #115 runs are in
  `run/defects-head-6101e34/i115_stress/`.

## Status and what is unverified

The last full-service measurement is the `b15c357` run below. Fixes 4
(`ce45ef8`) and 5 (`6101e34`) came after it. A quiet-gated re-run of the
service harness on `6101e34` (the plan: rounds only while the machine is idle
outside the benchmark containers) was started and interrupted, and the
operator decided not to repeat it. No table in this file therefore measures
the head of the branch on the services.

What still stands:
- **The root-cause analysis** (sections 2 and 3, opts 1–3). It rests on
  per-thread profiles, perf/strace, the pool trace inside the services and
  the standalone pool repro, each measured on the arm side by side, not on
  the final table.
- **investor_api at `b15c357`.** Its rows ran at load1 1.1–3.8 with round
  spreads of 0.5–11%; fork at or above uvloop on 7 of 8 rows. The c16 rows
  behind on 0.1.64 are the ones the root cause explains.
- **The cost of fixes 4 and 5 in microbenchmarks** ([What fixes 4 and 5
  cost](#what-fixes-4-and-5-cost)): quiet-gated, every row within its
  round-to-round spread except `xthread_burst`, and `pool_churn` unchanged.
- **Correctness** (section 5): test suites, the review's ordering test and
  the defect repros at `ce45ef8`/`6101e34`.

What is not verified:
- **The service-level performance of `ce45ef8` and `6101e34`.** The micro
  and `pool_churn` figures suggest no material change, but the full harness
  has not been run on them.
- **admin_api vs uvloop.** The `b15c357` table's round 2 was contaminated
  (below), so its +2.0% geometric mean and the per-row differences of a few
  percent cannot be read as "within noise". Rounds 1 and 3 ran at load1
  about 2.5–3.5, but they were not analysed on their own and a clean re-run
  is still needed.
- **A free-threaded interpreter**, and any CPython other than 3.14.
- **Quiet-machine conditions.** The agent host has 3 usable CPUs and ran
  other work throughout; no row of the full tables meets a pinned low-load
  condition.

## Latest full run (`b15c357`)

The independent review of this run (of the commit that introduced it,
`c20aad5`) found the admin_api round 2 contaminated. Its rows ran at
load1 4.2–10.6 for uvloop and up to 8.7 for the fork, against about 2.5–3.5
in rounds 1 and 3. The throughput drops in that round are external
contention, not loop noise:
- fork `/positions/pnl-stats` c64: 985 rps vs 1,173 and 1,184 in the other
  two rounds;
- uvloop `/auth/me` c16: 1,574 vs 1,860 and 1,865.

The medians below include that round, so they and their "within noise"
reading are **not supported**; the per-round values are in the collapsed
tables. The investor_api rows ran at load1 1.1–3.8 and are not affected in
the same way.

Fork at `b15c357` (the branch head's code; wheel baked into
`rsperf-<api>:fork` by the compose project) vs rsloop 0.1.64 (the image pin)
vs uvloop 0.22.1 vs asyncio.

- The #1120 harness method: 3 interleaved rounds with rotated arm order,
  15 s warm-up, 60 s per row, Granian with 2 workers, `--cpus 2`.
- Run from 01:01 to 04:46 UTC on 2026-10-12. `load1` for every row is in the
  table, and per round in the collapsed details.
- "new PG sessions/1k" is the `pg_stat_database.sessions` delta. It also
  counts each service's own per-call connections: admin_api persists every
  INFO+ log line through a new connection, and investor_api's `/health/`
  probe connects per call.

**Verdict.**

| | fork vs uvloop, geometric mean (all rows / without `/health`) | worst row | fork vs 0.1.64 (all / without `/health`) |
|---|---|---|---|
| admin_api | **+2.0% / +2.4%**, at or above uvloop on 5 of 8 rows | −2.9% (`/auth/me` c16; uvloop's own round spread there is 18.5%) | +5.9% / +6.7% |
| investor_api | +24.7% / **+3.9%**, at or above uvloop on 7 of 8 rows | −1.8% (`/health/` c64) | +20.9% / +30.2% |

- **investor_api c16 rows** (the ones behind on 0.1.64):
  - `/auth/me`: +6.3% vs uvloop (0.1.64: −46%).
  - `/notifications/unread-count`: +1.5% (0.1.64: −41%).
  - `/reports/`: +3.7% (0.1.64: −30%).
  - CPU per 1k requests and new sessions per 1k are at uvloop's level.
- **`/health/` c16 is bimodal on every loop** (p50 ≈ 5–15 ms at ~400–650
  rps, or ≈ 250 ms at ~80–100 rps). It is bound by a per-call NATS connect
  and close (`infrastructure/adapters/health_probe.py:57`), not by the loop.
  - uvloop landed in the slow mode in all 3 rounds here, hence the +374% and
    the +24.7% all-rows mean.
  - Extra 20 s windows (load 1.5–1.8): fork 548 and 632 rps, uvloop 668 and
    104.
  - The fork's round-3 row (24 rps, p50 758 ms) is the slow mode under a
    different arm order.
  - Also in those fast-mode windows: the fork opened 11–25 new sessions per
    1k vs uvloop's 2.1, at 1,737–2,054 vs 1,618 µs CPU per request. That
    residual on the probe endpoint is not explained here.

#### `b15c357` admin_api: median rps of 3 interleaved rounds (60 s each)

| endpoint | c | uvloop | asyncio | rsloop 0.1.64 | **fork** | fork vs uvloop | fork vs 0.1.64 | uvloop round spread | CPU-ms/1k uvloop / 0.1.64 / fork | new PG sessions/1k uvloop / 0.1.64 / fork | load1 at rows (min–max) |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `/health` | 16 | 427 | 432 | 422 | **430** | +0.7% | +1.8% | 3.2% | 3,366 / 3,880 / 3,216 | 41.7 / 81.2 / 45.1 | 1.3–10.6 |
| `/health` | 64 | 444 | 448 | 426 | **448** | +0.8% | +5.3% | 3.5% | 4,388 / 4,660 / 4,395 | 99.1 / 156.4 / 103.1 | 2.0–9.0 |
| `/auth/me` | 16 | 1,860 | 1,642 | 1,688 | **1,806** | -2.9% | +7.0% | 18.5% | 1,074 / 1,181 / 1,106 | 3.4 / 19.5 / 4.4 | 2.5–9.0 |
| `/auth/me` | 64 | 1,728 | 1,762 | 1,657 | **1,891** | +9.4% | +14.1% | 14.2% | 1,156 / 1,208 / 1,057 | 2.9 / 17.9 / 5.6 | 2.7–7.7 |
| `/trade-sessions/` | 16 | 507 | 508 | 524 | **497** | -1.9% | -5.1% | 5.3% | 3,942 / 3,811 / 4,016 | 17.2 / 26.1 / 19.0 | 2.8–6.2 |
| `/trade-sessions/` | 64 | 487 | 495 | 480 | **501** | +3.0% | +4.4% | 7.5% | 4,122 / 4,173 / 3,997 | 9.1 / 26.0 / 12.2 | 2.8–8.7 |
| `/positions/pnl-stats` | 16 | 1,098 | 1,080 | 1,037 | **1,092** | -0.6% | +5.2% | 6.7% | 1,814 / 1,919 / 1,826 | 7.6 / 30.3 / 8.1 | 2.9–6.9 |
| `/positions/pnl-stats` | 64 | 1,084 | 1,125 | 1,011 | **1,173** | +8.2% | +16.1% | 2.5% | 1,843 / 1,981 / 1,704 | 4.1 / 26.5 / 7.9 | 3.1–6.9 |

fork ≥ uvloop on 5 of 8 rows; geometric mean fork vs uvloop +2.0%, fork vs rsloop 0.1.64 +5.9%.

<details><summary>admin_api: every row, every round (rps, load1)</summary>

| endpoint | c | arm | r1 | r2 | r3 |
|---|---|---|---|---|---|
| `/health` | 16 | uvloop | 427 (1.3) | 432 (10.6) | 418 (3.9) |
| `/health` | 64 | uvloop | 433 (2.0) | 444 (9.0) | 448 (3.6) |
| `/auth/me` | 16 | uvloop | 1,860 (2.5) | 1,574 (9.0) | 1,865 (3.6) |
| `/auth/me` | 64 | uvloop | 1,733 (2.8) | 1,517 (7.7) | 1,728 (3.6) |
| `/trade-sessions/` | 16 | uvloop | 507 (2.8) | 484 (6.2) | 509 (3.7) |
| `/trade-sessions/` | 64 | uvloop | 488 (2.8) | 454 (5.7) | 487 (3.7) |
| `/positions/pnl-stats` | 16 | uvloop | 1,117 (3.0) | 1,046 (4.9) | 1,098 (3.3) |
| `/positions/pnl-stats` | 64 | uvloop | 1,084 (3.3) | 1,099 (4.2) | 1,072 (3.1) |
| `/health` | 16 | rsloop | 427 (3.0) | 422 (2.9) | 366 (4.5) |
| `/health` | 64 | rsloop | 426 (2.8) | 429 (3.0) | 421 (5.2) |
| `/auth/me` | 16 | rsloop | 1,689 (3.1) | 1,490 (3.1) | 1,688 (5.2) |
| `/auth/me` | 64 | rsloop | 1,657 (3.2) | 1,509 (4.0) | 1,684 (4.0) |
| `/trade-sessions/` | 16 | rsloop | 524 (3.0) | 496 (3.7) | 535 (3.1) |
| `/trade-sessions/` | 64 | rsloop | 481 (3.4) | 472 (3.6) | 480 (3.1) |
| `/positions/pnl-stats` | 16 | rsloop | 1,037 (3.4) | 975 (3.7) | 1,097 (2.9) |
| `/positions/pnl-stats` | 64 | rsloop | 1,011 (3.3) | 1,000 (3.5) | 1,015 (3.3) |
| `/health` | 16 | fork | 434 (3.0) | 398 (3.2) | 430 (3.4) |
| `/health` | 64 | fork | 448 (2.0) | 469 (3.6) | 433 (2.8) |
| `/auth/me` | 16 | fork | 1,806 (2.9) | 1,785 (4.5) | 1,962 (3.5) |
| `/auth/me` | 64 | fork | 1,913 (2.7) | 1,744 (5.5) | 1,891 (3.4) |
| `/trade-sessions/` | 16 | fork | 519 (3.0) | 419 (6.0) | 497 (4.2) |
| `/trade-sessions/` | 64 | fork | 502 (3.1) | 428 (8.7) | 501 (3.5) |
| `/positions/pnl-stats` | 16 | fork | 1,092 (3.1) | 821 (6.9) | 1,092 (3.1) |
| `/positions/pnl-stats` | 64 | fork | 1,173 (3.5) | 985 (6.9) | 1,184 (3.6) |
| `/health` | 16 | asyncio | 430 (2.6) | 449 (6.9) | 432 (3.5) |
| `/health` | 64 | asyncio | 441 (2.9) | 461 (6.7) | 448 (2.4) |
| `/auth/me` | 16 | asyncio | 1,765 (3.1) | 1,642 (4.8) | 1,393 (4.0) |
| `/auth/me` | 64 | asyncio | 1,762 (2.8) | 1,808 (4.9) | 1,759 (4.0) |
| `/trade-sessions/` | 16 | asyncio | 450 (2.9) | 511 (3.8) | 508 (3.8) |
| `/trade-sessions/` | 64 | asyncio | 501 (3.4) | 438 (3.4) | 495 (3.5) |
| `/positions/pnl-stats` | 16 | asyncio | 1,135 (3.4) | 907 (5.3) | 1,080 (3.8) |
| `/positions/pnl-stats` | 64 | asyncio | 1,125 (3.3) | 922 (6.6) | 1,145 (3.6) |

</details>

#### `b15c357` investor_api: median rps of 3 interleaved rounds (60 s each)

| endpoint | c | uvloop | asyncio | rsloop 0.1.64 | **fork** | fork vs uvloop | fork vs 0.1.64 | uvloop round spread | CPU-ms/1k uvloop / 0.1.64 / fork | new PG sessions/1k uvloop / 0.1.64 / fork | load1 at rows (min–max) |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `/health/` | 16 | 86 | 401 | 430 | **405** | +373.9% | -5.7% | 11.1% | 2,174 / 2,088 / 2,064 | 89.0 / 15.5 / 15.1 | 1.8–3.7 |
| `/health/` | 64 | 49 | 48 | 49 | **48** | -1.8% | -0.5% | 0.5% | 1,643 / 1,960 / 1,916 | 91.1 / 92.2 / 92.4 | 1.1–2.3 |
| `/auth/me` | 16 | 2,306 | 2,379 | 1,246 | **2,451** | +6.3% | +96.8% | 10.3% | 867 / 1,605 / 816 | 3.2 / 53.4 / 2.2 | 1.2–3.3 |
| `/auth/me` | 64 | 2,333 | 2,368 | 2,493 | **2,427** | +4.0% | -2.6% | 4.2% | 858 / 803 / 824 | 1.8 / 2.6 / 1.9 | 2.0–3.2 |
| `/notifications/unread-count` | 16 | 1,893 | 1,905 | 1,113 | **1,921** | +1.5% | +72.6% | 7.6% | 1,056 / 1,797 / 1,041 | 3.4 / 51.9 / 4.7 | 2.4–3.7 |
| `/notifications/unread-count` | 64 | 1,873 | 1,891 | 1,949 | **1,932** | +3.1% | -0.9% | 5.0% | 1,068 / 1,027 / 1,036 | 2.3 / 4.2 / 2.3 | 2.2–3.8 |
| `/reports/` | 16 | 1,511 | 1,541 | 1,058 | **1,567** | +3.7% | +48.1% | 1.6% | 1,323 / 1,890 / 1,276 | 6.6 / 42.6 / 3.4 | 2.7–3.5 |
| `/reports/` | 64 | 1,521 | 1,561 | 1,591 | **1,593** | +4.7% | +0.1% | 4.0% | 1,315 / 1,258 / 1,256 | 2.9 / 3.3 / 2.7 | 2.6–3.6 |

fork ≥ uvloop on 7 of 8 rows; geometric mean fork vs uvloop +24.7%, fork vs rsloop 0.1.64 +20.9%.

<details><summary>investor_api: every row, every round (rps, load1)</summary>

| endpoint | c | arm | r1 | r2 | r3 |
|---|---|---|---|---|---|
| `/health/` | 16 | uvloop | 90 (2.7) | 81 (2.0) | 86 (2.2) |
| `/health/` | 64 | uvloop | 49 (1.7) | 49 (1.1) | 49 (1.3) |
| `/auth/me` | 16 | uvloop | 2,306 (1.4) | 2,353 (1.4) | 2,133 (1.2) |
| `/auth/me` | 64 | uvloop | 2,285 (2.8) | 2,333 (2.5) | 2,382 (2.4) |
| `/notifications/unread-count` | 16 | uvloop | 1,796 (2.9) | 1,893 (2.9) | 1,933 (2.6) |
| `/notifications/unread-count` | 64 | uvloop | 1,825 (3.2) | 1,873 (2.9) | 1,916 (2.9) |
| `/reports/` | 16 | uvloop | 1,511 (3.2) | 1,499 (3.5) | 1,522 (2.8) |
| `/reports/` | 64 | uvloop | 1,493 (3.2) | 1,521 (3.6) | 1,552 (2.9) |
| `/health/` | 16 | rsloop | 430 (3.7) | 447 (2.1) | 377 (2.3) |
| `/health/` | 64 | rsloop | 49 (2.2) | 48 (2.3) | 49 (1.9) |
| `/auth/me` | 16 | rsloop | 1,228 (1.3) | 1,246 (2.4) | 2,265 (1.6) |
| `/auth/me` | 64 | rsloop | 2,490 (2.6) | 2,493 (3.2) | 2,495 (2.5) |
| `/notifications/unread-count` | 16 | rsloop | 1,113 (2.9) | 1,110 (3.7) | 1,882 (2.6) |
| `/notifications/unread-count` | 64 | rsloop | 1,921 (2.8) | 1,952 (3.8) | 1,949 (2.4) |
| `/reports/` | 16 | rsloop | 1,058 (2.9) | 1,561 (2.9) | 1,053 (2.9) |
| `/reports/` | 64 | rsloop | 1,588 (3.1) | 1,605 (2.8) | 1,591 (3.0) |
| `/health/` | 16 | fork | 405 (2.3) | 447 (2.0) | 24 (2.6) |
| `/health/` | 64 | fork | 48 (1.9) | 48 (1.6) | 48 (1.1) |
| `/auth/me` | 16 | fork | 2,396 (1.2) | 2,451 (3.3) | 2,487 (1.2) |
| `/auth/me` | 64 | fork | 2,417 (2.2) | 2,427 (2.9) | 2,471 (2.0) |
| `/notifications/unread-count` | 16 | fork | 1,933 (2.6) | 1,825 (3.5) | 1,921 (2.4) |
| `/notifications/unread-count` | 64 | fork | 1,932 (2.7) | 1,929 (3.7) | 1,961 (2.7) |
| `/reports/` | 16 | fork | 1,567 (3.1) | 1,554 (3.5) | 1,594 (2.8) |
| `/reports/` | 64 | fork | 1,574 (3.2) | 1,593 (2.9) | 1,604 (3.2) |
| `/health/` | 16 | asyncio | 357 (1.8) | 405 (1.9) | 401 (2.8) |
| `/health/` | 64 | asyncio | 48 (1.5) | 48 (2.1) | 48 (2.2) |
| `/auth/me` | 16 | asyncio | 2,379 (1.5) | 2,375 (1.6) | 2,428 (1.4) |
| `/auth/me` | 64 | asyncio | 2,324 (2.1) | 2,369 (2.3) | 2,368 (2.3) |
| `/notifications/unread-count` | 16 | asyncio | 1,910 (2.4) | 1,902 (2.7) | 1,905 (2.7) |
| `/notifications/unread-count` | 64 | asyncio | 1,846 (2.2) | 1,891 (2.8) | 1,896 (2.7) |
| `/reports/` | 16 | asyncio | 1,541 (2.7) | 1,587 (3.3) | 1,526 (3.0) |
| `/reports/` | 64 | asyncio | 1,513 (2.6) | 1,571 (3.0) | 1,561 (2.9) |

</details>

### Intermediate full run at `5cf71aa` (before opt 3)

Run from 20:33 to 00:21 UTC, same method. admin_api geometric mean −4.6% vs
uvloop (worst row −15.7%, `/positions/pnl-stats` c64); investor_api +0.7%
(+2.4% without `/health`). That admin_api gap and its session counter led to
section 3a and opt 3.

<details><summary>Full tables for the 5cf71aa run</summary>

#### `5cf71aa` admin_api: median rps of 3 interleaved rounds (60 s each)

| endpoint | c | uvloop | asyncio | rsloop 0.1.64 | **fork** | fork vs uvloop | fork vs 0.1.64 | uvloop round spread | CPU-ms/1k uvloop / 0.1.64 / fork | new PG sessions/1k uvloop / 0.1.64 / fork | load1 at rows (min–max) |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `/health` | 16 | 422 | 421 | 434 | **421** | -0.3% | -3.0% | 3.4% | 3,685 / 3,401 / 3,259 | 37.1 / 56.9 / 46.6 | 1.4–9.5 |
| `/health` | 64 | 442 | 439 | 409 | **443** | +0.3% | +8.4% | 6.8% | 4,425 / 4,851 / 4,445 | 70.6 / 137.1 / 94.4 | 1.9–10.6 |
| `/auth/me` | 16 | 1,641 | 1,493 | 1,301 | **1,405** | -14.4% | +8.0% | 14.0% | 1,208 / 1,533 / 1,326 | 2.9 / 13.8 / 9.0 | 3.0–12.8 |
| `/auth/me` | 64 | 1,686 | 1,627 | 1,188 | **1,563** | -7.3% | +31.6% | 10.4% | 1,185 / 1,684 / 1,280 | 2.6 / 37.8 / 14.1 | 3.6–10.9 |
| `/trade-sessions/` | 16 | 444 | 439 | 451 | **428** | -3.7% | -5.2% | 18.3% | 4,490 / 4,415 / 4,668 | 21.4 / 30.7 / 39.7 | 3.9–8.9 |
| `/trade-sessions/` | 64 | 416 | 448 | 441 | **467** | +12.2% | +5.9% | 19.1% | 4,781 / 4,499 / 4,291 | 7.3 / 25.9 / 21.4 | 3.6–8.3 |
| `/positions/pnl-stats` | 16 | 1,077 | 987 | 836 | **1,023** | -5.0% | +22.3% | 6.8% | 1,842 / 2,274 / 1,950 | 7.4 / 28.0 / 19.2 | 3.6–9.5 |
| `/positions/pnl-stats` | 64 | 1,053 | 1,021 | 1,016 | **888** | -15.7% | -12.6% | 27.4% | 1,895 / 1,954 / 2,219 | 4.2 / 24.2 / 21.9 | 2.8–8.8 |

fork ≥ uvloop on 2 of 8 rows; geometric mean fork vs uvloop -4.6%, fork vs rsloop 0.1.64 +6.1%.

<details><summary>admin_api: every row, every round (rps, load1)</summary>

| endpoint | c | arm | r1 | r2 | r3 |
|---|---|---|---|---|---|
| `/health` | 16 | uvloop | 422 (1.4) | 411 (5.5) | 425 (9.5) |
| `/health` | 64 | uvloop | 442 (1.9) | 430 (6.1) | 459 (10.6) |
| `/auth/me` | 16 | uvloop | 1,730 (3.0) | 1,641 (7.9) | 1,518 (6.7) |
| `/auth/me` | 64 | uvloop | 1,736 (3.6) | 1,686 (6.6) | 1,572 (6.6) |
| `/trade-sessions/` | 16 | uvloop | 509 (4.0) | 444 (5.2) | 430 (5.3) |
| `/trade-sessions/` | 64 | uvloop | 486 (3.7) | 408 (6.1) | 416 (5.9) |
| `/positions/pnl-stats` | 16 | uvloop | 1,116 (3.6) | 1,045 (7.1) | 1,077 (6.1) |
| `/positions/pnl-stats` | 64 | uvloop | 1,079 (2.8) | 847 (5.9) | 1,053 (5.5) |
| `/health` | 16 | rsloop | 432 (3.4) | 463 (4.0) | 434 (4.4) |
| `/health` | 64 | rsloop | 411 (5.4) | 409 (5.2) | 397 (3.7) |
| `/auth/me` | 16 | rsloop | 1,915 (6.6) | 1,301 (8.1) | 1,218 (7.8) |
| `/auth/me` | 64 | rsloop | 1,310 (6.6) | 1,188 (6.5) | 1,018 (10.8) |
| `/trade-sessions/` | 16 | rsloop | 451 (8.5) | 481 (5.5) | 402 (8.1) |
| `/trade-sessions/` | 64 | rsloop | 447 (6.2) | 441 (4.6) | 415 (8.3) |
| `/positions/pnl-stats` | 16 | rsloop | 915 (6.2) | 836 (5.4) | 630 (8.7) |
| `/positions/pnl-stats` | 64 | rsloop | 1,016 (5.9) | 1,035 (4.9) | 699 (8.2) |
| `/health` | 16 | fork | 418 (4.6) | 421 (3.9) | 475 (8.5) |
| `/health` | 64 | fork | 443 (3.9) | 398 (2.9) | 459 (10.1) |
| `/auth/me` | 16 | fork | 2,126 (4.1) | 1,405 (4.5) | 1,374 (12.8) |
| `/auth/me` | 64 | fork | 1,563 (3.6) | 1,567 (5.0) | 1,417 (10.9) |
| `/trade-sessions/` | 16 | fork | 497 (3.9) | 417 (5.6) | 428 (8.9) |
| `/trade-sessions/` | 64 | fork | 476 (3.6) | 467 (5.7) | 399 (7.7) |
| `/positions/pnl-stats` | 16 | fork | 1,023 (3.6) | 1,033 (5.6) | 788 (7.6) |
| `/positions/pnl-stats` | 64 | fork | 888 (4.4) | 1,033 (6.1) | 751 (8.3) |
| `/health` | 16 | asyncio | 421 (4.3) | 428 (4.7) | 402 (7.8) |
| `/health` | 64 | asyncio | 441 (3.6) | 427 (3.9) | 439 (5.7) |
| `/auth/me` | 16 | asyncio | 1,815 (3.6) | 1,493 (5.2) | 1,403 (6.0) |
| `/auth/me` | 64 | asyncio | 1,633 (4.5) | 1,564 (6.7) | 1,627 (7.9) |
| `/trade-sessions/` | 16 | asyncio | 513 (6.2) | 439 (7.0) | 433 (7.2) |
| `/trade-sessions/` | 64 | asyncio | 451 (4.2) | 448 (7.6) | 351 (7.8) |
| `/positions/pnl-stats` | 16 | asyncio | 1,016 (4.9) | 987 (6.1) | 739 (9.5) |
| `/positions/pnl-stats` | 64 | asyncio | 1,165 (5.2) | 1,021 (6.4) | 845 (8.8) |

</details>

#### `5cf71aa` investor_api: median rps of 3 interleaved rounds (60 s each)

| endpoint | c | uvloop | asyncio | rsloop 0.1.64 | **fork** | fork vs uvloop | fork vs 0.1.64 | uvloop round spread | CPU-ms/1k uvloop / 0.1.64 / fork | new PG sessions/1k uvloop / 0.1.64 / fork | load1 at rows (min–max) |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `/health/` | 16 | 456 | 159 | 394 | **419** | -8.0% | +6.5% | 61.9% | 1,768 / 2,101 / 2,083 | 15.0 / 15.9 / 14.8 | 2.0–11.6 |
| `/health/` | 64 | 49 | 48 | 49 | **48** | -0.5% | -0.2% | 4.2% | 1,828 / 2,030 / 1,946 | 92.6 / 92.3 / 92.5 | 1.3–9.4 |
| `/auth/me` | 16 | 2,342 | 2,390 | 1,236 | **2,343** | +0.0% | +89.5% | 15.9% | 854 / 1,619 / 854 | 2.2 / 52.3 / 4.9 | 1.4–7.6 |
| `/auth/me` | 64 | 2,316 | 2,305 | 2,477 | **2,402** | +3.7% | -3.1% | 8.6% | 864 / 808 / 833 | 1.9 / 2.7 / 2.4 | 2.4–7.4 |
| `/notifications/unread-count` | 16 | 1,762 | 1,913 | 1,117 | **1,832** | +3.9% | +64.0% | 18.8% | 1,135 / 1,792 / 1,092 | 4.9 / 51.2 / 8.3 | 2.7–6.3 |
| `/notifications/unread-count` | 64 | 1,864 | 1,874 | 1,930 | **1,917** | +2.8% | -0.7% | 2.2% | 1,073 / 1,037 / 1,044 | 2.4 / 4.4 / 2.8 | 2.5–5.2 |
| `/reports/` | 16 | 1,451 | 1,567 | 1,031 | **1,461** | +0.7% | +41.8% | 7.7% | 1,378 / 1,941 / 1,369 | 9.2 / 45.9 / 10.2 | 2.6–4.2 |
| `/reports/` | 64 | 1,502 | 1,520 | 1,582 | **1,553** | +3.4% | -1.8% | 30.5% | 1,332 / 1,266 / 1,289 | 2.9 / 3.1 / 3.5 | 2.8–4.2 |

fork ≥ uvloop on 6 of 8 rows; geometric mean fork vs uvloop +0.7%, fork vs rsloop 0.1.64 +20.4%.

<details><summary>investor_api: every row, every round (rps, load1)</summary>

| endpoint | c | arm | r1 | r2 | r3 |
|---|---|---|---|---|---|
| `/health/` | 16 | uvloop | 457 (11.6) | 456 (2.6) | 282 (2.1) |
| `/health/` | 64 | uvloop | 49 (9.4) | 47 (2.2) | 49 (1.6) |
| `/auth/me` | 16 | uvloop | 2,034 (7.6) | 2,342 (1.6) | 2,357 (1.4) |
| `/auth/me` | 64 | uvloop | 2,140 (7.4) | 2,316 (2.7) | 2,324 (2.4) |
| `/notifications/unread-count` | 16 | uvloop | 1,762 (6.3) | 1,601 (2.7) | 1,903 (3.0) |
| `/notifications/unread-count` | 64 | uvloop | 1,832 (5.2) | 1,864 (3.7) | 1,872 (2.5) |
| `/reports/` | 16 | uvloop | 1,428 (3.8) | 1,538 (3.4) | 1,451 (2.6) |
| `/reports/` | 64 | uvloop | 1,164 (3.4) | 1,502 (3.2) | 1,519 (3.2) |
| `/health/` | 16 | rsloop | 394 (6.8) | 391 (2.4) | 404 (2.5) |
| `/health/` | 64 | rsloop | 49 (4.5) | 49 (1.8) | 48 (1.9) |
| `/auth/me` | 16 | rsloop | 1,321 (3.1) | 1,203 (1.5) | 1,236 (1.5) |
| `/auth/me` | 64 | rsloop | 2,310 (4.0) | 2,483 (3.1) | 2,477 (2.9) |
| `/notifications/unread-count` | 16 | rsloop | 1,355 (4.2) | 1,117 (3.2) | 1,114 (2.9) |
| `/notifications/unread-count` | 64 | rsloop | 1,763 (3.8) | 1,935 (3.1) | 1,930 (3.6) |
| `/reports/` | 16 | rsloop | 1,379 (4.2) | 1,023 (3.6) | 1,031 (3.5) |
| `/reports/` | 64 | rsloop | 1,498 (3.7) | 1,597 (4.2) | 1,582 (3.6) |
| `/health/` | 16 | fork | 419 (3.3) | 423 (2.9) | 414 (2.3) |
| `/health/` | 64 | fork | 48 (2.3) | 48 (2.1) | 48 (1.8) |
| `/auth/me` | 16 | fork | 2,226 (1.9) | 2,343 (1.8) | 2,406 (1.8) |
| `/auth/me` | 64 | fork | 2,358 (3.2) | 2,402 (3.5) | 2,459 (2.7) |
| `/notifications/unread-count` | 16 | fork | 1,633 (3.5) | 1,879 (3.4) | 1,832 (3.1) |
| `/notifications/unread-count` | 64 | fork | 1,886 (3.8) | 1,917 (3.5) | 1,953 (3.2) |
| `/reports/` | 16 | fork | 1,404 (3.5) | 1,461 (3.0) | 1,545 (3.3) |
| `/reports/` | 64 | fork | 1,509 (3.2) | 1,553 (3.0) | 1,586 (3.3) |
| `/health/` | 16 | asyncio | 129 (2.9) | 159 (2.0) | 347 (2.1) |
| `/health/` | 64 | asyncio | 48 (3.1) | 48 (1.3) | 49 (1.7) |
| `/auth/me` | 16 | asyncio | 1,996 (2.7) | 2,390 (1.7) | 2,416 (2.4) |
| `/auth/me` | 64 | asyncio | 1,950 (3.2) | 2,305 (2.9) | 2,356 (3.1) |
| `/notifications/unread-count` | 16 | asyncio | 1,832 (5.9) | 1,913 (3.9) | 1,923 (3.0) |
| `/notifications/unread-count` | 64 | asyncio | 1,840 (5.0) | 1,874 (3.5) | 1,885 (3.2) |
| `/reports/` | 16 | asyncio | 1,517 (3.9) | 1,567 (3.6) | 1,592 (3.3) |
| `/reports/` | 64 | asyncio | 1,512 (3.2) | 1,520 (3.5) | 1,559 (2.8) |

</details>

</details>

## Remaining differences and notes

- **Single serial TCP connection.** On a strict one-connection ping-pong the
  fork costs about 0.7–1.4 µs more CPU per round trip than uvloop: 8.3 vs 7.6
  at `b15c357`, and 7.7 vs 6.4 at `6101e34` in a later run. With 8
  connections it is level or ahead (8.0 vs 7.5; 6.9 vs 7.2). That difference
  sits in the
  loop-thread reader task and turn bookkeeping. It is not pursued here: a
  structural change would be needed (e.g. dispatching `data_received`
  straight from the reactor callback, as libuv does), and it does not show on
  the services.
- **uvloop churns too.** It opens 3–9 new sessions per 1k on investor_api at
  c16. The 4 + 2 pool per worker is below the 8 requests per worker that c16
  puts on it.
  - Raising `POOL_SIZE` would help every loop, but it is part of the
    deployment-wide connection budget in `backend/services/database/AGENTS.md`.
  - That is a copytrade decision, not part of this fork.
- **`RSLOOP_READER_SPIN_US` is unchanged** (default 30 µs). It spins in
  GIL-free blocking reader workers used by rsloop's native streams, which the
  services do not use (they keep the stdlib `open_connection`). It did not
  appear in any profile here (`smol`/`blocking` threads < 4 µs/req).
- **Opting back into the spin.** `RSLOOP_WAKE_SPIN_US=50` restores the old
  behaviour. It only pays off when the waking threads never hold the GIL.
- **Upstream.** Nothing has been sent; contact needs the operator's yes.
  - The five commits are self-contained and would apply to upstream `master`
    as they stand.
  - Fix 5 (`6101e34`) matters on its own: v0.1.64 itself loses cross-thread
    wakes of its coordination thread (about 1 in 300–450 runs of the #115
    repro), which can strand callbacks queued before a run.
- **Behaviour change in opt 3.** Callbacks scheduled during a turn now run in
  the next turn, after an I/O poll, which is asyncio's and uvloop's order.
  - rsloop's own suites pass: 642 core tests at `6101e34`, plus the stress
    suite.
  - Code that relied on rsloop running chains to quiescence before polling
    I/O would see I/O interleaved earlier, as it would on asyncio.
  - Since fix 4, `call_soon` and `call_soon_threadsafe` callbacks also run in
    the order they were scheduled. On v0.1.64, off-thread callbacks could
    overtake earlier loop-thread ones.

## Reproducing

All under `~/.cache/rsloop-perf/`:

| path | what |
|---|---|
| `builder/` | wheel builder image (`python:3.14.5-slim` + nightly-2026-09-25 + maturin 1.15.0); `build-wheel.sh <src> <out>` |
| `bench/site.sh` | unpacks a wheel into a `PYTHONPATH` dir under `sites/` |
| `override.yml`, `bin-dc.sh`, `fork-image/` | the isolated `rsperf` compose project and the fork image build |
| `bench/harness.py` | the #1120 harness for `rsperf`. Arms `uvloop`, `rsloop`, `fork`, `fork:<site>`, `<arm>@nospin`. `harness.py api <api> [rounds]`. Quiet-gated since the final run (`QUIET_*` variables); copied into `perf/bench/` |
| `bench/quiet.py` | the quiet-machine probe: CPU used outside the `rsperf_*` containers over a window; copied into `perf/bench/` |
| `bench/final2.sh`, `bench/compare_final.py` | the quiet-gated final run (one rotated round per call) and its table (`compare_final.py _final2`); `compare_final.py` is copied into `perf/bench/`. That run was interrupted and not repeated (see [Status and what is unverified](#status-and-what-is-unverified)). `bench/final.sh` produced the `b15c357` run |
| `fix-fifo/scripts/i115_stress.sh <site> <runs/depth> <out>` | the #115 repro at high repetition in Docker, with a faulthandler dump of any hung run |
| `fix-fifo/diag/i115_alarm.py`, `fix-fifo/cores/` | the SIGALRM probe and the core dumps (with a gdb sysroot) that located fix 5 |
| `fix-fifo/scripts/micro_gated.py` | quiet-gated wrapper around `bench/run_micro.sh` |
| `bench/profile.py` | per-thread CPU, strace, perf and py-spy for one arm under oha |
| `bench/run_micro.sh`, `bench/micro.py`, `bench/churn.sh`, `bench/pool_churn.py` | microbenchmarks and the pool-churn repro; `micro.py` and `pool_churn.py` are copied into `perf/bench/` |
| `defects-fork.sh` | #115/#116/#118 re-checks for a fork build |
| `bench/sitecustomize.py` | opt-in (`RSPERF_TRACE_POOL=1`) SQLAlchemy pool trace inside the service workers (arm variant `@trace`) |
| `profiles/`, `run/` | profile summaries, logs and jsonl results. Raw `perf.data` and `perf.script` were deleted afterwards to save disk (the box was at 88%); the numbers quoted above came from them. |
