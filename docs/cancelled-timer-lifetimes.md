# Cancelled timer ownership (issue #118)

[Issue #118](https://github.com/RustedBytes/rsloop/issues/118) reports that
`TimerHandle.cancel()` retains callbacks, arguments and captured context behind
an earlier live timer. The original deadline then determines the lifetime of
finished `asyncio.timeout()` tasks and closed asyncpg statement caches.

## Cause and fix

Previously, `ReadyCallback::cancel()` only changed an atomic flag. The timer
queue visited only expired or cancelled entries at its head; an earlier active
timer hid every cancelled entry behind it.

Timers now use a separate `TimerCallback` with an optional `ReadyCallback`
payload behind a mutex. Cancellation marks the timer, accounts for its queued
shell once and takes the payload. The payload is dropped **after unlocking**,
while the Python handle's `cancel()` call is still attached to Python. This
releases callback, args and context before returning for an unclaimed timer.
Repeated cancellation is harmless, including cancellation by a finalizer.

Dispatch claims the payload under the same mutex, then invokes it without any
lock. If dispatch wins a race with cancellation, that invocation owns its
callback, arguments and context until it finishes. Cancellation prevents future
invocation and cannot invalidate an already claimed callback's objects.

The queue counts cancelled shells under per-timer synchronization. Queue
removal decrements the count once; dropping a queued cancelled timer during
shutdown also balances the count. The atomic loads used by heap inspection and
compaction are statistical hints, not synchronization for Python ownership.

Collection compacts both the immediate FIFO and future-deadline heap when the
cancelled count exceeds 100 **and** half the queue size, matching asyncio's
amortization thresholds. Live deadlines and insertion order for ties survive
compaction. Removed shells transfer to the ready drain so shared callback
ownership is never dropped under a timer queue borrow. Large cancellation
bursts therefore stop accumulating behind a long-lived earlier timer at the
next collection. At or below the thresholds, small or predominantly live queues
can still contain payload-free cancelled entries. A stopped loop retains shells
until its next run or close; Python payload release does not wait for that run.

The existing `ReadyCallback`, Python `Handle` and `call_soon` paths retain their
layout and lock-free invocation. Synchronization and extra state are timer-only;
no dependencies or public Python signatures change.

## TDD and behavioral comparisons

Baseline: master `5cfc0594b734d60e0b1db44033e6175ac202868d` (0.1.63).
The test-only PR commit is
[`067d208`](https://github.com/RustedBytes/rsloop/commit/067d2084b940d53aa8edc59397bb1f51a3f5ee98).

Before the implementation, 12 lifetime regressions failed on rsloop while
asyncio/uvloop controls passed; two rsloop-specific reentrant-close comparisons
were skipped on controls. A Rust regression also observed **257** queued entries
(one live sentinel plus 256 cancelled timers) where it expected **1**.

The expanded regressions cover callback/one-arg/many-arg/context ownership;
explicit and captured contexts; stopped and running loops; retained handles;
earlier live and periodic timers; completed `timeout()`/`wait_for()` tasks and
results; expired timeout cancellation; cancellation after heap dequeue; repeated
cross-thread cancellation; cancel versus dispatch/removal races; invocation
ownership during cancellation; finalizer reentry through scheduling,
cancellation and close; and compaction ordering in both queue types.

One control difference is intentional: CPython and uvloop clear callback and
args on cancellation but keep Context on a user-held handle. rsloop releases all
three payload components when safe. Control tests record that difference rather
than assuming identical Context ownership.

## Memory measurements

The checked-in probe `benches/timer_cancellation.py` runs each case in a fresh
process. A live sentinel has an 1800 s deadline; cancelled timers, timeout blocks
and statement-cache expiry timers have 3600 s deadlines. No measured deadline
expires. Counts are observed after GC and three zero-delay loop turns, allowing
`gather` completion callbacks to release their normal transient ownership.

Local measurements: Linux x86_64, glibc 2.39, CPython 3.14.7, epoll fallback;
both builds compiled from source in release mode with the pinned
`nightly-2026-09-25` toolchain and locked Cargo dependencies.
Python bytes subtract the probe's list/weakref storage, following the issue's
method. RSS includes native allocations and allocator high-water retention.

| Local case (20,000 objects) | Before | After |
|---|---:|---:|
| Cancelled timer payloads alive immediately / after turns | 20,000 / 20,000 | 0 / 0 |
| Timer Python bytes retained after turns | 87,223,891 B | 307 B |
| Timer RSS before workload → after turns | 26.2 → 125.2 MiB | 26.2 → 33.1 MiB |
| Finished timeout tasks alive after turns | 20,000 | 0 |
| Timeout Python bytes retained after turns | 101,011,784 B | 29,893 B |
| Timeout RSS before workload → after turns | 26.1 → 150.4 MiB | 26.0 → 34.1 MiB |

Immediately after the last `gather`, the fixed timeout probe sees 500 tasks
from its last batch, as do asyncio and uvloop. These disappear after completion
callbacks drain. This transient is separate from cancelled timer retention.

Real PostgreSQL measurements ran in GitHub Actions with PostgreSQL 16.15,
asyncpg 0.32.0, uvloop 0.23.0, CPython 3.14.8, Linux x86_64/glibc 2.39.
Each process closes 500 connections, in groups of 10, after three distinct cached
parameterized queries per connection, each under `asyncio.timeout()`.

| Objects remaining after 500 closed connections | asyncio | uvloop | rsloop before | rsloop after |
|---|---:|---:|---:|---:|
| `_StatementCache` | 0 | 0 | 500 | 0 |
| `PreparedStatementState` | 0 | 0 | 1500 | 0 |
| `Protocol` | 0 | 0 | 0 | 0 |

The 50-connection regression failed before the fix with 50 caches and 150 states;
after the fix all **39 PostgreSQL lifecycle tests passed**, including existing
SQLAlchemy transaction, pool exhaustion, concurrent writer and disconnect tests.
[Baseline CI](https://github.com/RustedBytes/rsloop/actions/runs/38126971249)
and [fixed CI](https://github.com/RustedBytes/rsloop/actions/runs/38127314821)
contain JSON measurements and the regression results.

These are fresh-process bounded reproductions, not a repeat of the author's
300 s Granian load test. They do not establish production steady-state RSS.
PostgreSQL was available in CI; local server setup was unavailable in the
restricted environment. Python 3.10 uses `wait_for()` around its queries and
explicitly skips the standalone `asyncio.timeout()` probe, which requires 3.11.

## Timer performance and tradeoff

Same local release builds and interpreter; 1,000,000 operations per sample,
1,000-timer batches, one warmup, seven samples per process, two before/after
pairs with reversed order. The table uses the median of 14 samples per build.
Tracemalloc/GC probes are not enabled in timed spans. These are scheduler
microbenchmarks, not application throughput claims.

| Workload | Before (s) | After (s) | Time change |
|---|---:|---:|---:|
| `call_soon` callback burst | 0.18194 | 0.18122 | -0.4% |
| Zero-delay timers, handles retained | 0.29481 | 0.31178 | +5.8% |
| Zero-delay timers, handles discarded | 0.25788 | 0.28745 | +11.5% |
| Cancelled zero-delay timers | 0.26746 | 0.28294 | +5.8% |
| Cancelled scattered future timers | 0.37740 | 0.36704 | -2.7% |

The extra timer synchronization costs approximately 15–30 ns per operation in
these local workloads. The `call_soon` difference is within measurement noise;
its hot path has no added mutex. The mixed-timer result is not evidence of a
general speed improvement. Safe ownership transfer is the reason for the timer
tradeoff; release-mode and downstream measurements should guide later changes.

## Validation and reproduction

Local validation:

- New differential tests: 51 passed, 6 skipped (the three reentrant-close
  ownership cases are rsloop-specific and skip on both controls).
- Free-threaded CPython 3.14.8: 19 new rsloop tests plus 5 existing local-timer
  tests passed. The Rust synchronization tests also create real concurrent
  threads while the Python loop thread is attached.
- Full Python core suite: 628 passed, 12 skipped; two Unix-socket cases fail with
  `PermissionError` in the local sandbox and fail identically on the baseline.
- Downstream ecosystem suite: 78 passed, 39 PostgreSQL cases skipped locally;
  PostgreSQL executes against the real CI service.
- Rust default suite: 336 passed; 15 environment-restricted io_uring/Unix cases
  fail locally. All-features suite: 432 passed; 19 restricted cases fail locally.
- Clippy: `cargo clippy --all-targets --all-features --locked -- -D warnings`
  passed. Cargo fmt, Ruff and targeted Pyright passed.

The first local incremental all-features link failed with missing generated
monomorphization symbols; rerunning with `CARGO_INCREMENTAL=0` resolved the link.
The initial test-only CI all-features build passed with the standard command;
this local build-cache issue did not require a source workaround.

```sh
# Use the locked test dependencies and build the project first.
uv sync --locked --no-default-groups --group ecosystem --no-install-project
uv run --no-sync maturin develop --release --locked
uv run --no-sync python -m pytest tests/test_timer_cancellation.py tests/test_local_timers.py
uv run --no-sync python scripts/run_rust_tests.py
uv run --no-sync python scripts/run_rust_tests.py --all-features
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo fmt --check

# Run each line in a fresh process; repeat for asyncio and uvloop.
uv run --no-sync python benches/timer_cancellation.py --loop rsloop --case timers
uv run --no-sync python benches/timer_cancellation.py --loop rsloop --case timeout
uv run --no-sync python benches/timer_cancellation.py --loop rsloop --case performance --count 1000000
# Set RSLOOP_POSTGRES_DSN to a dedicated test database for these commands.
uv run --no-sync python benches/timer_cancellation.py --loop rsloop --case postgres --count 500
uv run --no-sync python scripts/run_python_tests.py tests/ecosystem/test_postgresql.py -m ecosystem
```
