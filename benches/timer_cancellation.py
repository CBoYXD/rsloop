"""Fresh-process lifetime/RSS and timer throughput probe for issue #118.

Run with the same release build and interpreter for before/after throughput.
PostgreSQL requires RSLOOP_POSTGRES_DSN pointing to an isolated database.
"""

from __future__ import annotations

import argparse
import asyncio
import gc
import importlib
import json
import os
import platform
import statistics
import sys
import time
import tracemalloc
import weakref
from importlib.metadata import version
from pathlib import Path


class Payload:
    def __init__(self):
        self.blob = bytearray(4096)

    def fire(self):
        raise AssertionError("cancelled timer fired")


def rss_bytes():
    if sys.platform == "linux":
        for line in Path("/proc/self/status").read_text().splitlines():
            if line.startswith("VmRSS:"):
                return int(line.split()[1]) * 1024
    return None


def live(references):
    gc.collect()
    return sum(reference() is not None for reference in references)


async def memory_case(count, mode):
    loop = asyncio.get_running_loop()
    sentinel = loop.call_later(1800, lambda: None)
    references = []
    gc.collect()
    rss_before = rss_bytes()
    tracemalloc.start()
    before = tracemalloc.get_traced_memory()[0]
    started = time.perf_counter()
    try:
        if mode == "timers":
            payload = None
            for _ in range(count):
                payload = Payload()
                references.append(weakref.ref(payload))
                loop.call_later(3600, payload.fire).cancel()
            del payload
        else:

            async def request():
                async with asyncio.timeout(3600):
                    await asyncio.sleep(0)
                return Payload()

            for offset in range(0, count, 500):
                tasks = [
                    loop.create_task(request()) for _ in range(min(500, count - offset))
                ]
                await asyncio.gather(*tasks)
                references.extend(weakref.ref(task) for task in tasks)
                del tasks
        immediate = live(references)
        held_immediate = tracemalloc.get_traced_memory()[0] - before
        rss_immediate = rss_bytes()
        for _ in range(3):
            await asyncio.sleep(0)
        after_turn = live(references)
        held_after_turn = tracemalloc.get_traced_memory()[0] - before
        # Subtract only this probe's weakref/list storage, as in the issue.
        probe_bytes = sys.getsizeof(references) + sum(
            sys.getsizeof(ref) for ref in references
        )
        return {
            "count": count,
            "alive_immediately": immediate,
            "alive_after_turn": after_turn,
            "python_bytes_immediately": held_immediate - probe_bytes,
            "python_bytes_after_turn": held_after_turn - probe_bytes,
            "traced_peak_bytes": tracemalloc.get_traced_memory()[1],
            "rss_before": rss_before,
            "rss_immediately": rss_immediate,
            "rss_after_turn": rss_bytes(),
            "seconds": time.perf_counter() - started,
        }
    finally:
        tracemalloc.stop()
        sentinel.cancel()


def postgres_counts():
    gc.collect()
    counts = dict.fromkeys(("_StatementCache", "PreparedStatementState", "Protocol"), 0)
    for obj in gc.get_objects():
        name = type(obj).__name__
        if name in counts:
            counts[name] += 1
    return counts


async def postgres_case(count):
    asyncpg = importlib.import_module("asyncpg")

    loop = asyncio.get_running_loop()
    sentinel = loop.call_later(1800, lambda: None)
    before = postgres_counts()
    rss_before = rss_bytes()
    started = time.perf_counter()

    async def connection():
        conn = await asyncpg.connect(
            os.environ["RSLOOP_POSTGRES_DSN"], max_cached_statement_lifetime=3600
        )
        try:
            for index in range(3):
                async with asyncio.timeout(3600):
                    assert (
                        await conn.fetchval(f"SELECT $1::int + {index}", index)
                        == 2 * index
                    )
        finally:
            await conn.close()

    try:
        for offset in range(0, count, 10):
            await asyncio.gather(
                *(connection() for _ in range(min(10, count - offset)))
            )
        for _ in range(3):
            await asyncio.sleep(0)
        after = postgres_counts()
        return {
            "count": count,
            "seconds": time.perf_counter() - started,
            "live_object_delta": {name: after[name] - before[name] for name in before},
            "rss_before": rss_before,
            "rss_after": rss_bytes(),
            "asyncpg": version("asyncpg"),
        }
    finally:
        sentinel.cancel()


async def performance_case(count):
    # Reuse the repository's workloads, without tracemalloc or GC in timed spans.
    from scheduler_workloads import bench_timers

    samples = {}
    for mode in (
        "timers_retained",
        "timers_discarded",
        "timers_cancelled",
        "timers_mixed",
    ):
        await bench_timers("probe", 1000, 1000, mode)
        samples[mode] = [
            (await bench_timers("probe", count, 1000, mode)).seconds for _ in range(7)
        ]
    return {
        "count": count,
        "samples_seconds": samples,
        "median_seconds": {
            mode: statistics.median(values) for mode, values in samples.items()
        },
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--loop", choices=("asyncio", "rsloop", "uvloop"), required=True
    )
    parser.add_argument(
        "--case",
        choices=("timers", "timeout", "postgres", "performance"),
        required=True,
    )
    parser.add_argument("--count", type=int, default=20000)
    args = parser.parse_args()
    if args.count <= 0:
        parser.error("--count must be positive")
    if args.loop == "asyncio":
        factory = asyncio.new_event_loop
    else:
        factory = __import__(args.loop).new_event_loop
    loop = factory()
    try:
        if args.case == "postgres":
            operation = postgres_case(args.count)
        elif args.case == "performance":
            operation = performance_case(args.count)
        else:
            operation = memory_case(args.count, args.case)
        result = loop.run_until_complete(operation)
    finally:
        loop.close()
    print(
        json.dumps(
            dict(
                loop=args.loop,
                case=args.case,
                python=platform.python_version(),
                platform=platform.platform(),
                loop_version=version(args.loop) if args.loop != "asyncio" else None,
                **result,
            ),
            sort_keys=True,
        )
    )


if __name__ == "__main__":
    main()
