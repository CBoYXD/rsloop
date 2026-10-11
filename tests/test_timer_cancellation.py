"""Cancelled timers release Python ownership before their heap deadline (#118)."""

from __future__ import annotations

import asyncio
import contextvars
import gc
import threading
import weakref

import pytest
import rsloop


@pytest.fixture(params=["asyncio", "rsloop", "uvloop"])
def loop(request):
    factory = {
        "asyncio": asyncio.new_event_loop,
        "rsloop": rsloop.new_event_loop,
    }.get(request.param)
    if factory is None:
        factory = pytest.importorskip("uvloop").new_event_loop
    event_loop = factory()
    yield event_loop
    event_loop.close()


class Payload:
    def __init__(self):
        self.blob = bytearray(4096)

    def __call__(self, *args):
        pytest.fail("cancelled callback executed")


def make_timer(loop, ownership, many=False):
    payload = Payload()
    reference = weakref.ref(payload)
    if ownership == "callback":
        handle = loop.call_later(3600, payload)
    elif ownership == "args":
        args = (payload, payload) if many else (payload,)
        handle = loop.call_later(3600, lambda *args: None, *args)
    else:
        var = contextvars.ContextVar("timer_payload")
        context = contextvars.Context()
        context.run(var.set, payload)
        handle = loop.call_later(3600, lambda: None, context=context)
    return handle, reference


@pytest.mark.parametrize("ownership", ["callback", "args", "context"])
@pytest.mark.parametrize("running", [False, True])
def test_cancel_releases_python_references_immediately(loop, ownership, running):
    def check():
        sentinel = loop.call_later(1800, lambda: None)
        try:
            handle, reference = make_timer(loop, ownership)
            assert reference() is not None
            handle.cancel()
            handle.cancel()
            # No GC or loop turn is needed for these acyclic payloads.
            if ownership == "context" and not isinstance(loop, rsloop.Loop):
                # CPython and uvloop retain Context on the user-held handle.
                assert reference() is not None
            else:
                assert reference() is None
            assert handle.cancelled()
        finally:
            sentinel.cancel()

    async def main():
        check()

    if running:
        loop.run_until_complete(main())
    else:
        check()


def test_cancel_releases_many_arguments_and_captured_context(loop):
    sentinel = loop.call_later(1800, lambda: None)
    try:
        handle, reference = make_timer(loop, "args", many=True)
        handle.cancel()
        assert reference() is None
        var = contextvars.ContextVar("implicit_timer_payload")
        payload = Payload()
        reference = weakref.ref(payload)
        token = var.set(payload)
        handle = loop.call_later(3600, lambda: None)
        var.reset(token)
        del payload
        handle.cancel()
        if isinstance(loop, rsloop.Loop):
            assert reference() is None
        else:
            assert reference() is not None
    finally:
        sentinel.cancel()


def test_cancelled_burst_behind_live_timer_releases_all_payloads(loop):
    async def main():
        sentinel = loop.call_later(1800, lambda: None)
        try:
            references = []
            for _ in range(1000):
                handle, reference = make_timer(loop, "callback")
                references.append(reference)
                handle.cancel()
            await asyncio.sleep(0)
            gc.collect()
            assert all(reference() is None for reference in references)
        finally:
            sentinel.cancel()

    loop.run_until_complete(main())


@pytest.mark.parametrize("mode", ["timeout", "wait_for"])
def test_completed_timeout_tasks_and_results_are_released(loop, mode):
    if mode == "timeout" and not hasattr(asyncio, "timeout"):
        pytest.skip("asyncio.timeout requires Python 3.11")

    async def request():
        if mode == "timeout":
            async with asyncio.timeout(3600):
                await asyncio.sleep(0)
        else:
            await asyncio.wait_for(asyncio.sleep(0), 3600)
        return Payload()

    async def main():
        sentinel = loop.call_later(1800, lambda: None)
        try:
            tasks = [loop.create_task(request()) for _ in range(100)]
            await asyncio.gather(*tasks)
            references = [weakref.ref(task) for task in tasks]
            references.extend(weakref.ref(task.result()) for task in tasks)
            del tasks
            # Release gather's completion callbacks; no deadline may expire.
            for _ in range(3):
                await asyncio.sleep(0)
            gc.collect()
            assert all(reference() is None for reference in references)
        finally:
            sentinel.cancel()

    loop.run_until_complete(main())


@pytest.mark.parametrize("ownership", ["callback", "args", "context"])
def test_cancel_finalizers_can_reenter_and_close_loop(loop, ownership):
    if not isinstance(loop, rsloop.Loop):
        pytest.skip("reentrant close is a separate rsloop lifecycle contract")
    events = []
    handles = []

    class Callback:
        def __call__(self):
            pytest.fail("cancelled callback executed")

        def __del__(self):
            events.append("released")
            handles[0].cancel()
            if not loop.is_closed():
                loop.call_later(0, lambda: None).cancel()
                loop.close()

    if ownership == "callback":
        handles.append(loop.call_later(3600, Callback()))
    elif ownership == "args":
        handles.append(loop.call_later(3600, lambda arg: None, Callback()))
    else:
        var = contextvars.ContextVar("finalizer_payload")
        context = contextvars.Context()
        context.run(var.set, Callback())
        handles.append(loop.call_later(3600, lambda: None, context=context))
        del context
    handles[0].cancel()
    released_immediately = events == ["released"]
    if not released_immediately:
        loop.close()
    assert released_immediately
    assert handles[0].cancelled()
    assert loop.is_closed()


def test_cancel_after_dequeue_skips_callback(loop):
    events = []
    when = loop.time()
    # call_later's deadlines give the cancellation callback a strictly earlier
    # position on loops without stable ordering for equal deadlines.
    loop.call_at(when - 1, lambda: handle.cancel())
    handle = loop.call_at(when, lambda: events.append("unexpected"))
    loop.run_until_complete(asyncio.sleep(0))
    assert events == []


def test_concurrent_repeated_cancel_releases_payload_once(loop):
    handle, reference = make_timer(loop, "callback")
    barrier = threading.Barrier(9)

    def cancel():
        barrier.wait(timeout=5)
        for _ in range(100):
            handle.cancel()

    threads = [threading.Thread(target=cancel) for _ in range(8)]
    for thread in threads:
        thread.start()
    barrier.wait(timeout=5)
    for thread in threads:
        thread.join(timeout=5)
        assert not thread.is_alive()
    assert handle.cancelled()
    assert reference() is None


def test_cancel_running_callback_preserves_arguments_and_context(loop):
    entered = threading.Event()
    cancelled = threading.Event()
    var = contextvars.ContextVar("running_timer_value")
    context = contextvars.Context()
    context.run(var.set, "captured")
    events = []

    def callback(payload):
        entered.set()
        assert cancelled.wait(timeout=5)
        assert payload.blob == bytearray(4096)
        assert var.get() == "captured"
        events.append("finished")

    payload = Payload()
    reference = weakref.ref(payload)
    handle = loop.call_later(0, callback, payload, context=context)
    del payload

    def cancel():
        if entered.wait(timeout=5):
            handle.cancel()
        cancelled.set()

    thread = threading.Thread(target=cancel)
    thread.start()
    try:
        loop.run_until_complete(asyncio.sleep(0))
        assert events == ["finished"]
        gc.collect()
        assert reference() is None
    finally:
        cancelled.set()
        thread.join(timeout=5)
        assert not thread.is_alive()


@pytest.mark.parametrize("mode", ["timeout", "wait_for"])
def test_expired_timeout_still_cancels_awaited_work(loop, mode):
    if mode == "timeout" and not hasattr(asyncio, "timeout"):
        pytest.skip("asyncio.timeout requires Python 3.11")

    async def main():
        finalized = []

        async def work():
            try:
                await asyncio.Future()
            finally:
                finalized.append("cancelled")

        with pytest.raises(asyncio.TimeoutError):
            if mode == "timeout":
                async with asyncio.timeout(0.01):
                    await work()
            else:
                await asyncio.wait_for(work(), 0.01)
        assert finalized == ["cancelled"]
        assert len(asyncio.all_tasks()) == 1

    loop.run_until_complete(main())


def test_periodic_timers_do_not_retain_cancelled_payloads(loop):
    async def main():
        started = [asyncio.Event(), asyncio.Event()]

        async def heartbeat(index):
            started[index].set()
            while True:
                await asyncio.sleep(0.01)

        beats = [loop.create_task(heartbeat(index)) for index in range(2)]
        try:
            await asyncio.gather(*(event.wait() for event in started))
            references = []
            for _ in range(100):
                handle, reference = make_timer(loop, "callback")
                handle.cancel()
                references.append(reference)
            for _ in range(3):
                await asyncio.sleep(0.01)
            gc.collect()
            assert all(reference() is None for reference in references)
        finally:
            for beat in beats:
                beat.cancel()
            await asyncio.gather(*beats, return_exceptions=True)

    loop.run_until_complete(main())
