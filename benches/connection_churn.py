"""Content-checked short TCP connections to measure accept and teardown costs."""

import argparse
import asyncio
import gc
import json
import platform
import statistics
import time
from pathlib import Path

import rsloop
import rsloop._loop as native


async def measure(connections, concurrency):
    async def echo(reader, writer):
        try:
            assert await reader.readexactly(1) == b"x"
            writer.write(b"x")
            await writer.drain()
        finally:
            writer.close()
            await writer.wait_closed()

    server = await asyncio.start_server(echo, "127.0.0.1", 0)
    port = server.sockets[0].getsockname()[1]

    async def client(count):
        for _ in range(count):
            reader, writer = await asyncio.open_connection("127.0.0.1", port)
            try:
                writer.write(b"x")
                await writer.drain()
                assert await reader.readexactly(1) == b"x"
            finally:
                writer.close()
                await writer.wait_closed()

    started = time.perf_counter()
    try:
        await asyncio.gather(
            *(client(connections // concurrency) for _ in range(concurrency))
        )
    finally:
        server.close()
        await server.wait_closed()
    return time.perf_counter() - started


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--connections", type=int, default=8000)
    parser.add_argument("--concurrency", type=int, default=16)
    parser.add_argument("--repeat", type=int, default=7)
    parser.add_argument("--warmups", type=int, default=2)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if min(args.connections, args.concurrency, args.repeat) <= 0 or args.warmups < 0:
        parser.error("counts must be positive and warmups nonnegative")
    if args.connections % args.concurrency:
        parser.error("connections must be divisible by concurrency")
    samples = []
    was_enabled = gc.isenabled()
    gc.disable()
    try:
        for index in range(args.warmups + args.repeat):
            seconds = rsloop.run(
                asyncio.wait_for(measure(args.connections, args.concurrency), 120)
            )
            if index >= args.warmups:
                samples.append(seconds)
    finally:
        if was_enabled:
            gc.enable()
    result = {
        "python": platform.python_version(),
        "native_module": native.__file__,
        "settings": {**vars(args), "output": str(args.output)},
        "seconds": samples,
    }
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    median = statistics.median(samples)
    print(
        f"{args.connections} connections: {median * 1000:.2f} ms, "
        f"{args.connections / median:,.0f} connections/s",
        flush=True,
    )


if __name__ == "__main__":
    main()
