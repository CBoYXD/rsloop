"""Flow-control callbacks may close a transport before the write call returns."""

import asyncio
import socket
import sys
from typing import cast

import pytest
import rsloop


@pytest.mark.skipif(
    sys.platform == "win32" or not hasattr(socket, "AF_UNIX"),
    reason="requires Unix transports",
)
@pytest.mark.parametrize("method", ["write", "writelines"])
@pytest.mark.parametrize("callback", ["pause", "resume"])
@pytest.mark.parametrize("finish", ["close", "write_eof"])
@pytest.mark.parametrize("send_buffer", [4096, 131072])
@pytest.mark.parametrize("prefill", [False, True])
def test_flow_control_callback_preserves_unsent_data(
    callback, finish, method, send_buffer, prefill
):
    # Queue enough staged data to cross high water independently of how
    # much the OS accepts on the first immediate write.
    payload = bytes(range(256)) * 4096

    async def main():
        loop = asyncio.get_running_loop()
        sender, receiver = socket.socketpair()
        sender.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, send_buffer)
        sender.setblocking(False)
        receiver.setblocking(False)
        prefix = bytearray()
        if prefill:
            # A full kernel buffer reproduces the macOS case where the
            # staged flush cannot reach low water until the peer reads.
            try:
                while True:
                    prefix.extend(b"p" * sender.send(b"p" * 4096))
            except BlockingIOError:
                pass
        paused = loop.create_future()
        finished = loop.create_future()
        transport = None

        class Sender(asyncio.Protocol):
            def connection_made(self, transport):
                transport = cast(asyncio.Transport, transport)
                self.transport = transport
                loop.call_soon(self.send)

            def send(self):
                transport = self.transport
                if callback == "resume":
                    transport.set_write_buffer_limits(high=65536, low=65536)
                    if method == "writelines":
                        transport.writelines(
                            payload[offset : offset + 16384]
                            for offset in range(0, len(payload), 16384)
                        )
                    else:
                        for offset in range(0, len(payload), 4096):
                            transport.write(payload[offset : offset + 4096])
                else:
                    transport.set_write_buffer_limits(high=0, low=0)
                    if method == "writelines":
                        transport.writelines([payload[:8192], payload[8192:]])
                    else:
                        transport.write(payload)

            def pause_writing(self):
                if not paused.done():
                    paused.set_result(self.transport.get_write_buffer_size())
                if callback == "pause":
                    self.finish()

            def resume_writing(self):
                if callback == "resume":
                    self.finish()

            def finish(self):
                if not finished.done():
                    # Capture progress as the callback re-enters shutdown.
                    # The writer may already have drained on resume.
                    remaining = self.transport.get_write_buffer_size()
                    finished.set_result(remaining)
                    getattr(self.transport, finish)()

        async def receive_all():
            data = bytearray()
            while chunk := await asyncio.wait_for(loop.sock_recv(receiver, 65536), 5):
                data.extend(chunk)
            return data

        try:
            transport, _ = await loop.create_unix_connection(Sender, sock=sender)
            assert await asyncio.wait_for(paused, 5) > 0
            if callback == "resume":
                # Every resume case needs a reader: kernel buffer sizes and
                # partial-write boundaries differ between Linux and macOS.
                receiving = asyncio.create_task(receive_all())
                try:
                    await asyncio.wait_for(finished, 5)
                    assert await receiving == prefix + payload
                finally:
                    receiving.cancel()
                    await asyncio.gather(receiving, return_exceptions=True)
            else:
                assert await asyncio.wait_for(finished, 5) > 0
                assert await receive_all() == prefix + payload
        finally:
            if transport is not None:
                transport.close()
            sender.close()
            receiver.close()

    rsloop.run(main())


@pytest.mark.skipif(not hasattr(socket, "AF_UNIX"), reason="requires Unix transports")
def test_isolated_small_protocol_write_is_delivered_immediately():
    async def main():
        loop = asyncio.get_running_loop()
        sender, receiver = socket.socketpair()
        sender.setblocking(False)
        receiver.setblocking(False)
        made = loop.create_future()
        payload = b"single-message" * 16

        class Sender(asyncio.Protocol):
            def connection_made(self, transport):
                transport = cast(asyncio.Transport, transport)
                try:
                    transport.write(payload)
                    # An isolated write must reach the socket in this callback,
                    # without waiting for the event loop to flush a batch.
                    assert receiver.recv(len(payload)) == payload
                    made.set_result(transport)
                except (AssertionError, OSError) as error:
                    made.set_exception(error)

        transport = None
        try:
            transport, _ = await loop.create_unix_connection(Sender, sock=sender)
            assert await made is transport
        finally:
            if transport is not None:
                transport.close()
            sender.close()
            receiver.close()

    rsloop.run(main())
