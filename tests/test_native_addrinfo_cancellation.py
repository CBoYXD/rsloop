"""Cancelled native lookups must not consume the bounded DNS pool's queue."""

import os
import shutil
import subprocess
import sys
import textwrap

import pytest

pytestmark = pytest.mark.skipif(
    sys.platform != "linux", reason="the getaddrinfo shim uses Linux LD_PRELOAD"
)


@pytest.fixture(scope="module")
def resolver_gate(tmp_path_factory):
    compiler = shutil.which("cc")
    if compiler is None:
        pytest.skip("a C compiler is required for the getaddrinfo shim")
    directory = tmp_path_factory.mktemp("resolver-gate")
    source = directory / "gate.c"
    library = directory / "gate.so"
    source.write_text(
        textwrap.dedent(
            """
            #define _GNU_SOURCE
            #include <dlfcn.h>
            #include <netdb.h>
            #include <pthread.h>
            #include <stdatomic.h>
            #include <string.h>

            static pthread_mutex_t mutex = PTHREAD_MUTEX_INITIALIZER;
            static pthread_cond_t changed = PTHREAD_COND_INITIALIZER;
            static atomic_int started = 0;
            static int released = 0;

            int resolver_started(void) {
                return atomic_load(&started);
            }

            void resolver_release(void) {
                pthread_mutex_lock(&mutex);
                released = 1;
                pthread_cond_broadcast(&changed);
                pthread_mutex_unlock(&mutex);
            }

            int getaddrinfo(const char *node, const char *service,
                            const struct addrinfo *hints,
                            struct addrinfo **result) {
                if (node && strcmp(node, "rsloop-cancel.invalid") == 0) {
                    pthread_mutex_lock(&mutex);
                    atomic_fetch_add(&started, 1);
                    while (!released) {
                        pthread_cond_wait(&changed, &mutex);
                    }
                    pthread_mutex_unlock(&mutex);
                    return EAI_NONAME;
                }
                int (*original)(const char *, const char *,
                                const struct addrinfo *, struct addrinfo **) =
                    dlsym(RTLD_NEXT, "getaddrinfo");
                return original(node, service, hints, result);
            }
            """
        )
    )
    subprocess.run(
        [
            compiler,
            "-std=c11",
            "-shared",
            "-fPIC",
            "-pthread",
            str(source),
            "-ldl",
            "-o",
            str(library),
        ],
        capture_output=True,
        text=True,
        check=True,
    )
    return library


@pytest.mark.parametrize("cancel_task", [False, True], ids=["future", "task"])
def test_cancelled_queued_lookups_never_call_libc(resolver_gate, cancel_task):
    # A fresh process isolates the global pool and interposes only these names.
    # The gate fills every slot without depending on DNS or wall-clock latency.
    source = """
        import asyncio
        import ctypes
        import gc
        import os
        import socket
        import weakref

        import rsloop

        gate = ctypes.CDLL(os.environ["RSLOOP_RESOLVER_GATE"])
        gate.resolver_started.restype = ctypes.c_int
        gate.resolver_release.restype = None

        async def main():
            loop = asyncio.get_running_loop()
            # Match the native pool, unaffected by PYTHON_CPU_COUNT overrides.
            cpus = len(os.sched_getaffinity(0))
            limit = min(32, (cpus or 1) + 4)
            futures = []

            async def lookup():
                future = loop.getaddrinfo(
                    "rsloop-cancel.invalid", 80, type=socket.SOCK_STREAM
                )
                futures.append(future)
                return await future

            if CANCEL_TASK:
                lookups = [asyncio.create_task(lookup()) for _ in range(3 * limit)]
            else:
                lookups = [loop.getaddrinfo(
                    "rsloop-cancel.invalid", 80, type=socket.SOCK_STREAM
                ) for _ in range(3 * limit)]
                futures.extend(lookups)
            references = []
            async def all_slots_started():
                while gate.resolver_started() < limit or len(futures) < 3 * limit:
                    await asyncio.sleep(0.001)

            try:
                await asyncio.wait_for(all_slots_started(), 10)
                assert gate.resolver_started() == limit
                references = [weakref.ref(future) for future in futures]
                for pending in lookups:
                    pending.cancel()
                await asyncio.gather(*lookups, return_exceptions=True)
                # Cancellation reaches the Rust token through done callbacks.
                await asyncio.sleep(0)
                assert all(future.cancelled() for future in futures)
            finally:
                gate.resolver_release()
            # This live lookup queues behind the cancelled ones. No cancelled
            # queued job may call libc while workers drain the queue before it.
            rows = await asyncio.wait_for(
                loop.getaddrinfo("localhost", 81, type=socket.SOCK_STREAM), 10
            )
            assert rows
            assert gate.resolver_started() == limit, gate.resolver_started()
            del lookups, futures, pending
            # Running workers and completed tasks may still be dropping their
            # references. Wait for that cleanup, including on free-threaded builds.
            async def all_futures_released():
                while any(reference() is not None for reference in references):
                    gc.collect()
                    await asyncio.sleep(0.001)

            await asyncio.wait_for(all_futures_released(), 10)

        rsloop.run(main())
    """
    environment = os.environ.copy()
    environment["RSLOOP_RESOLVER_GATE"] = str(resolver_gate)
    environment["LD_PRELOAD"] = os.pathsep.join(
        filter(None, (str(resolver_gate), environment.get("LD_PRELOAD")))
    )
    result = subprocess.run(
        [
            sys.executable,
            "-c",
            f"CANCEL_TASK = {cancel_task}\n" + textwrap.dedent(source),
        ],
        env=environment,
        capture_output=True,
        text=True,
        timeout=30,
        check=False,
    )
    assert result.returncode == 0, result.stdout + result.stderr
