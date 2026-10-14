# Report implementation audit

This maps the updated local performance report to the implementation. Repeated
recommendations and the final Top-10 table are covered by the same entries.
All 55 recommendations have an implementation. The report's alternative patches
are adapted to preserve ownership, cancellation and Python context behavior.

## Implementation map

| Recommendation | Implementation | Source |
|---|---|---|
| stream write pool | Coalesced Python bytes materialize into the write pool. | [core_write.rs](../src/transport/stream/core_write.rs) |
| adaptive write coalescing | Isolated small writes remain direct; repeated writes predict batches. | [core_write.rs](../src/transport/stream/core_write.rs) |
| atomic write state | Write status reads a coherent atomic snapshot. | [core_write.rs](../src/transport/stream/core_write.rs) |
| immediate read cache | Exclusively owned immediate awaitables are reused on GIL builds. | [fast.rs](../src/transport/stream/fast.rs) |
| read waiter cache | Read completion callbacks are cached per reader. | [fast.rs](../src/transport/stream/fast.rs) |
| drain waiter cache | Drain completion callbacks are cached per protocol. | [fast.rs](../src/transport/stream/fast.rs) |
| skip immediate watch | Immediate reads skip waiter attachment. | [fast.rs](../src/transport/stream/fast.rs) |
| exact read direct fill | Exact reads fill their accumulator from incoming owned storage. | [fast.rs](../src/transport/stream/fast.rs) |
| return empty read pool slots | Fully consumed pooled storage is returned immediately. | [fast.rs](../src/transport/stream/fast.rs) |
| tls plaintext drain | TLS plaintext is drained through BufRead without zero filling. | [tls_session.rs](../src/transport/stream/tls_session.rs) |
| tls unlock before pool wait | TLS locks are released before waiting for read buffers. | [tls_session.rs](../src/transport/stream/tls_session.rs) |
| connect without dup | Native connect watches borrow the original live descriptor. | [connect.rs](../src/transport/stream/connect.rs) |
| writelines inline refs | Only initialized Python byte references occupy the inline array. | [py_transport.rs](../src/transport/stream/py_transport.rs) |
| stream python caches and interning | Stream helper imports and CFunctions are cached; method names are interned. | [fast.rs](../src/transport/stream/fast.rs) |
| server connection notification | A closed server is notified only when its last connection exits. | [server_core.rs](../src/transport/stream/server_core.rs) |
| empty callback context reuse | Empty callback contexts are reused after exclusive-reference checks. | [callbacks.rs](../src/engine/callbacks.rs) |
| stack pinned park sleep | Direct driver deadlines supersede both boxed and stack Sleep parking. | [loop_core.rs](../src/engine/loop_core.rs) |
| empty timer fast path | Empty local timer queues avoid heap work. | [loop_core.rs](../src/engine/loop_core.rs) |
| atomic loop flags | Running, stopping and closed checks use atomic snapshots. | [loop_core.rs](../src/engine/loop_core.rs) |
| discard callback result | Callback results are decref'd after context exit without constructing Py wrappers. | [callbacks.rs](../src/engine/callbacks.rs) |
| local server accept | Server accept starts and stops are routed to the loop runtime. | [loop_core.rs](../src/engine/loop_core.rs) |
| local pipe setup | Pipe setup executes on the local runtime. | [pipes.rs](../src/bindings/loop_api/pipes.rs) |
| reactor fd watchers | Reader and writer notifications use native tasks instead of new threads. | [dispatcher.rs](../src/engine/dispatcher.rs) |
| blocking thread pool | Blocking calls reuse smol's shared pool and preserve panic handling. | [blocking.rs](../src/blocking.rs) |
| nested context string | Nested context checks avoid copying the error text into a Rust String. | [context.rs](../src/context.rs) |
| socket operation fd reuse | One duplicate and reactor registration survive all socket-operation retries. | [socket_operation.rs](../src/bindings/loop_api/socket_operation.rs) |
| lifecycle python caches | Lifecycle helpers and weak loop-bound callbacks are cached. | [asyncio_cache.rs](../src/bindings/loop_api/asyncio_cache.rs) |
| socket family handoff | Known families pass through plain and TLS transport creation. | [io_targets.rs](../src/transport/stream/io_targets.rs) |
| shared unix endpoints | Unix reader and direct writer share their owned endpoint. | [io_targets.rs](../src/transport/stream/io_targets.rs) |
| local unix reader | Unix socket readers run on the Python loop's native runtime. | [loop_core.rs](../src/engine/loop_core.rs) |
| iocp operation pool | AFD and OVERLAPPED packets are pooled only after acknowledgement. | [iocp.rs](../src/vibeio/driver/iocp.rs) |
| iocp waker vector cache | Completion scratch vectors are retained; user destructors run outside state borrows. | [iocp.rs](../src/vibeio/driver/iocp.rs) |
| driver wake deduplication | Apple datagram wakes coalesce and consume one datagram per acknowledgement. | [kqueue.rs](../src/vibeio/driver/kqueue.rs) |
| kqueue stack registration | Kqueue registration uses two inline kevent slots. | [kqueue.rs](../src/vibeio/driver/kqueue.rs) |
| plain read uninitialized buffer | Plain reads initialize only the returned prefix of Vec spare capacity. | [io_targets.rs](../src/transport/stream/io_targets.rs) |
| server tls config cache | Server configurations reuse Arc storage and invalidate on context mutation. | [mod.rs](../src/transport/tls/mod.rs) |
| blocking double box | The default pool consumes the existing boxed closure. | [default.rs](../src/vibeio/blocking/default.rs) |
| batch allocator replace cache | The last eligible returned allocation replaces stale cached storage. | [batch_allocator.rs](../src/vibeio/batch_allocator.rs) |
| raw std accept | Accept returns raw standard sockets without constructing a native stream wrapper. | [listener.rs](../src/vibeio/net/tcp/listener.rs) |
| accepted flags avoid reset | Accept paths preserve kernel-provided flags without resetting them. | [accept.rs](../src/transport/stream/accept.rs) |
| uring conditional cancel | Idle registrations skip synchronous cancellation; active storage remains retained. | [uring.rs](../src/vibeio/driver/uring.rs) |
| uring zero timeout poll | Zero-timeout polls avoid empty kernel entries while servicing pending kernel work. | [uring.rs](../src/vibeio/driver/uring.rs) |
| tcp unclaimed readiness | Already-delivered readiness proceeds directly to the TCP read. | [stream.rs](../src/vibeio/net/tcp/stream.rs) |
| cached root waker | Runtime roots reuse their notification allocation and waker. | [executor.rs](../src/vibeio/executor.rs) |
| shared wake atomics | Shared wake state embeds its atomic flags rather than separately allocating them. | [executor.rs](../src/vibeio/executor.rs) |
| runtime batch reuse | Scheduler batches reuse runtime-owned storage in normal builds. | [batch_allocator.rs](../src/vibeio/batch_allocator.rs) |
| spawn detached | Fire-and-forget native tasks omit JoinState and join wrapper allocations. | [executor.rs](../src/vibeio/executor.rs) |
| fs owned buffers | File reads reuse stable Vec storage; owned writes move their allocation. | [mod.rs](../src/vibeio/fs/mod.rs) |
| local task wake fast path | Newly queued local task wakes use the current runtime without thread-ID checks or weak upgrades. | [task.rs](../src/vibeio/task.rs) |
| park driver deadline | Parking deadlines go straight to the driver timeout. | [executor.rs](../src/vibeio/executor.rs) |
| timer pop root fast path | Expired roots are removed directly and sifted down once. | [timer.rs](../src/vibeio/timer.rs) |
| vectored io stack buffers | Up to 16 native descriptors use inline storage; Linux completion arrays stay stable. | [io_util.rs](../src/vibeio/op/io_util.rs) |
| pipe vectored direct | Borrowed pipe writes pass IoSlice descriptors directly to writev. | [pipe.rs](../src/vibeio/io/pipe.rs) |
| pidfd support cache | Pidfd support is probed once per process. | [reaper.rs](../src/vibeio/process/reaper.rs) |
| sync zombie reaper lookup | Reaper lookup is synchronous and no longer boxes a future. | [executor.rs](../src/vibeio/executor.rs) |

## Verification

The release extension passes 647 core Python tests on CPython 3.14.7 (13
skipped). Fresh installed wheels also pass 495 tests on CPython 3.10.18 (167
skipped) and 539 tests on free-threaded CPython 3.14.0 with `PYTHON_GIL=0` (123
skipped). Optional reference-loop cases are skipped when their packages are
absent; Python-version and platform-specific skips are retained.

The default Rust suite passes 371 tests. The suite with
`fs pipe process signal splice stdio blocking-default` passes 462 tests.
Strict Clippy also checks the optional modules and profiling build.

New regression coverage checks callback-result finalizers after context exit
for zero, one and multiple arguments; repeated socket send backpressure;
cancellation before a native readiness poll; TLS server-config reuse and
invalidation; isolated immediate protocol writes; uninitialized read-buffer
prefixes and EOF; inline descriptor boundaries; detached task cleanup;
expired borrowed root futures; and owned filesystem buffer handoff.
Existing tests exercise retained read awaitables, exact-read cancellation,
pooled storage accounting, TLS backpressure, server notification and transport
shutdown reentrancy.

`check_vibeio_allocations.py` enforces zero Rust heap allocations across 1,000
stable-buffer handoffs, borrowed TCP vectored writes, and embedded scheduler
turns with zero and 16 ready tasks. Payload creation, task setup, CPython and
kernel allocations are outside these budgets.

The standalone embedded runtime, including its tests and all optional modules,
cross-compiles for Windows GNU and macOS x86-64. The Windows tests include packet
reuse only after acknowledgement; the macOS tests include datagram wake
coalescing. Native Windows/macOS tests and performance measurements require
those operating systems and were not run on this Linux host.

## Ownership and compatibility

Reference-count based Python object reuse is enabled only on GIL builds, where
exclusive ownership can be established safely. Free-threaded builds keep fresh
immediate awaitables and callback contexts. Cancellation retains completion
payloads until native acknowledgement; Linux completion descriptor arrays stay
boxed even though synchronous polling uses inline arrays.

Native connect waits retain the original Python socket until registration
removal and Future completion. General socket operations retain one owned
duplicate across retries. Unix stream halves share their owned descriptor,
while lazy queued writers can still duplicate once when activated.

Apple wake acknowledgement consumes the pending datagram before clearing the
notification flag, preserving the single-datagram invariant. IOCP recycled
packets become available only after their completion packet is consumed.
User wakers and payload destructors execute outside driver state borrows.

Filesystem writes accept `IntoFileBytes`. Vectors, strings, boxed slices and
owned `Cow` values move their storage. Borrowed `AsRef<[u8]>` inputs copy once
for cancellation safety; custom owned inputs can implement the conversion
trait. `scheduler-batch-cache` remains accepted as a compatibility feature;
scheduler storage reuse is enabled in normal builds.

Times and reproduction details are recorded in [benchmarks.md](benchmarks.md).
The local analysis report remains outside version control.
