"""Exercise request ownership through the service's real ASGI response lifecycle."""

import asyncio
import json
import unittest

import app as mock


WAIT_TIMEOUT = 15


class Exchange:
    """ASGI transport with explicit stream backpressure and client disconnects."""

    def __init__(self, body=None, *, path="/v1/chat/completions", block=False, fail=False, fail_start=False):
        """Configure a request and optional header or first-body transport failure."""
        self.payload = json.dumps(body or {}).encode()
        self.path = path
        self.block = block
        self.fail = fail
        self.fail_start = fail_start
        self.first_body = asyncio.Event()
        self.resume = asyncio.Event()
        self.disconnect = asyncio.Event()
        self.messages = []
        self.request_sent = False

    async def receive(self):
        """Deliver the request once, then wait for an explicit client disconnect."""
        if not self.request_sent:
            self.request_sent = True
            return {"type": "http.request", "body": self.payload, "more_body": False}
        await self.disconnect.wait()
        return {"type": "http.disconnect"}

    async def send(self, message):
        """Record output and hold the first content chunk until the test releases it."""
        self.messages.append(message)
        if message["type"] == "http.response.start" and self.fail_start:
            raise RuntimeError("synthetic transport failure")
        if message["type"] == "http.response.body" and message.get("body"):
            if not self.first_body.is_set():
                self.first_body.set()
                if self.block:
                    await self.resume.wait()
                if self.fail:
                    raise RuntimeError("synthetic transport failure")

    async def run(self):
        """Call FastAPI directly with the disconnect-aware ASGI HTTP protocol."""
        scope = {
            "type": "http",
            "asgi": {"version": "3.0", "spec_version": "2.3"},
            "http_version": "1.1",
            "method": "GET" if self.path == "/debug/stats" else "POST",
            "scheme": "http",
            "path": self.path,
            "raw_path": self.path.encode(),
            "query_string": b"",
            "root_path": "",
            "headers": [(b"content-type", b"application/json")],
            "client": ("127.0.0.1", 12345),
            "server": ("127.0.0.1", 8000),
        }
        await mock.app(scope, self.receive, self.send)


class ConcurrencyTests(unittest.IsolatedAsyncioTestCase):
    """Check slot and counter ownership on success and interrupted requests."""

    async def asyncSetUp(self):
        """Reset process-local metrics between independent event loops."""
        mock.STATS.in_flight = 0
        mock.STATS.total = 0
        mock.STATS.by_profile.clear()
        mock._SEMAPHORES.clear()
        self.tasks = []

    async def asyncTearDown(self):
        """Cancel pending transports even when an assertion fails."""
        for task in self.tasks:
            if not task.done():
                task.cancel()
        await asyncio.gather(*self.tasks, return_exceptions=True)

    def start(self, exchange):
        """Track background ASGI calls so failures cannot leak request tasks."""
        task = asyncio.create_task(exchange.run())
        self.tasks.append(task)
        return task

    async def wait_for_total(self, expected):
        """Yield until requests reach admission, with a bounded failure deadline."""
        async def reached():
            """Allow runnable request tasks to reach the admission counter."""
            while mock.STATS.total < expected:
                await asyncio.sleep(0)

        await asyncio.wait_for(reached(), WAIT_TIMEOUT)

    async def stats(self):
        """Read live metrics through the public endpoint rather than a cached value."""
        exchange = Exchange(path="/debug/stats")
        await asyncio.wait_for(exchange.run(), WAIT_TIMEOUT)
        return json.loads(b"".join(m.get("body", b"") for m in exchange.messages))

    def stream(self, **kwargs):
        """Use a tiny deterministic c1 scenario; transport events control overlap."""
        return Exchange({"model": "mock/l0-j0-t2-e0-r0-c1", "stream": True}, **kwargs)

    async def assert_recovered(self):
        """Prove cleanup restores exactly one slot and a new request can complete."""
        self.assertEqual((await self.stats())["in_flight"], 0)
        sem = mock._SEMAPHORES[1]
        await asyncio.wait_for(sem.acquire(), WAIT_TIMEOUT)
        self.assertTrue(sem.locked(), "cleanup released more than one c1 slot")
        sem.release()
        await asyncio.wait_for(self.stream().run(), WAIT_TIMEOUT)
        self.assertEqual((await self.stats())["in_flight"], 0)

    async def test_stream_holds_slot_and_live_counter_until_finished(self):
        """A second c1 stream cannot emit content while the first is backpressured."""
        first = self.stream(block=True)
        first_task = self.start(first)
        await asyncio.wait_for(first.first_body.wait(), WAIT_TIMEOUT)
        self.assertEqual((await self.stats())["in_flight"], 1)
        second = self.stream(block=True)
        second_task = self.start(second)
        await self.wait_for_total(2)
        self.assertFalse(second.first_body.is_set())
        self.assertEqual((await self.stats())["in_flight"], 2)
        first.resume.set()
        await asyncio.wait_for(first_task, WAIT_TIMEOUT)
        await asyncio.wait_for(second.first_body.wait(), WAIT_TIMEOUT)
        self.assertEqual((await self.stats())["in_flight"], 1)
        second.resume.set()
        await asyncio.wait_for(second_task, WAIT_TIMEOUT)
        await self.assert_recovered()

    async def test_cancel_waiter_does_not_release_active_stream_slot(self):
        """Canceling admission removes its counter without releasing another owner."""
        first = self.stream(block=True)
        first_task = self.start(first)
        await asyncio.wait_for(first.first_body.wait(), WAIT_TIMEOUT)
        waiter = self.start(self.stream())
        await self.wait_for_total(2)
        waiter.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await waiter
        self.assertEqual((await self.stats())["in_flight"], 1)
        self.assertTrue(mock._SEMAPHORES[1].locked())
        third = self.stream()
        third_task = self.start(third)
        await self.wait_for_total(3)
        self.assertFalse(third.first_body.is_set())
        first.resume.set()
        await asyncio.wait_for(first_task, WAIT_TIMEOUT)
        await asyncio.wait_for(third_task, WAIT_TIMEOUT)
        await self.assert_recovered()

    async def test_disconnect_releases_stream_slot(self):
        """An ASGI disconnect interrupts blocked output and restores admission."""
        exchange = self.stream(block=True)
        task = self.start(exchange)
        await asyncio.wait_for(exchange.first_body.wait(), WAIT_TIMEOUT)
        exchange.disconnect.set()
        await asyncio.wait_for(task, WAIT_TIMEOUT)
        await self.assert_recovered()

    async def test_cancel_active_stream_releases_slot(self):
        """Server cancellation during output restores the stream's slot and counter."""
        exchange = self.stream(block=True)
        task = self.start(exchange)
        await asyncio.wait_for(exchange.first_body.wait(), WAIT_TIMEOUT)
        task.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await task
        await self.assert_recovered()

    async def test_send_exception_releases_stream_slot(self):
        """Header and body send failures release ownership exactly once."""
        for fail_start in (True, False):
            with self.subTest(fail_start=fail_start):
                with self.assertRaisesRegex(RuntimeError, "^synthetic transport failure$"):
                    await asyncio.wait_for(
                        self.stream(fail=True, fail_start=fail_start).run(), WAIT_TIMEOUT
                    )
                await self.assert_recovered()

    async def test_nonstream_and_error_paths_restore_ownership(self):
        """Both nonstream endpoints retain successful and synthetic-error cleanup."""
        for path in ("/v1/chat/completions", "/v1/embeddings"):
            for scenario, status in (("e0-r0", 200), ("e100-r0", 500), ("e0-r100", 429)):
                with self.subTest(path=path, scenario=scenario):
                    exchange = Exchange({"model": f"mock/l0-t2-{scenario}-c1"}, path=path)
                    await asyncio.wait_for(exchange.run(), WAIT_TIMEOUT)
                    self.assertEqual(exchange.messages[0]["status"], status)
                    await self.assert_recovered()


class StreamingUsageTests(unittest.IsolatedAsyncioTestCase):
    """Validate the usage contract consumed by the router's accounting layer."""

    async def test_stream_usage_matches_nonstream_and_precedes_done(self):
        """Default/opt-in usage appears once after stop; explicit opt-out omits it."""
        for tokens in (0, 3):
            body = {"model": f"mock/l0-t{tokens}", "messages": [{"role": "user", "content": "hello world"}]}
            normal = Exchange(body)
            await asyncio.wait_for(normal.run(), WAIT_TIMEOUT)
            response = json.loads(b"".join(m.get("body", b"") for m in normal.messages))
            for options in (None, {"include_usage": False}, {"include_usage": True}):
                with self.subTest(tokens=tokens, options=options):
                    exchange = Exchange({**body, "stream": True, "stream_options": options})
                    await asyncio.wait_for(exchange.run(), WAIT_TIMEOUT)
                    payload = b"".join(m.get("body", b"") for m in exchange.messages).decode()
                    events = [line[6:] for line in payload.splitlines() if line.startswith("data: ")]
                    self.assertEqual(events[-1], "[DONE]")
                    chunks = [json.loads(event) for event in events[:-1]]
                    content = "".join(c["choices"][0]["delta"].get("content", "") for c in chunks if c["choices"])
                    self.assertEqual(content.strip(), response["choices"][0]["message"]["content"])
                    if options is None or options["include_usage"]:
                        self.assertEqual(chunks[-1]["choices"], [])
                        self.assertEqual(chunks[-1]["usage"], response["usage"])
                        self.assertEqual(chunks[-1]["usage"]["completion_tokens"], tokens)
                        self.assertEqual(chunks[-2]["choices"][0]["finish_reason"], "stop")
                        self.assertTrue(all(c["usage"] is None for c in chunks[:-1]))
                    else:
                        self.assertTrue(all("usage" not in c for c in chunks))
                        self.assertEqual(chunks[-1]["choices"][0]["finish_reason"], "stop")


if __name__ == "__main__":
    unittest.main()
