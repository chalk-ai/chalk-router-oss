"""Contract tests for the mock's OpenAI Chat Completions and Anthropic endpoints.

These exercise the FastAPI handlers directly so the test service keeps its
small runtime dependency set: no HTTP test client is required.
"""

from __future__ import annotations

import json
import unittest

from fastapi import Request

import app


def make_request(path: str, payload: dict) -> Request:
    raw_body = json.dumps(payload).encode()
    delivered = False

    async def receive():
        nonlocal delivered
        if delivered:
            return {"type": "http.disconnect"}
        delivered = True
        return {"type": "http.request", "body": raw_body, "more_body": False}

    return Request(
        {
            "type": "http",
            "asgi": {"version": "3.0"},
            "http_version": "1.1",
            "method": "POST",
            "scheme": "http",
            "path": path,
            "raw_path": path.encode(),
            "query_string": b"",
            "headers": [(b"content-type", b"application/json")],
            "client": ("testclient", 50000),
            "server": ("testserver", 80),
        },
        receive,
    )


class OpenAIChatCompletionsTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        app.STATS = app.Stats()
        app._SEMAPHORES.clear()

    async def test_chat_completions_returns_openai_shape(self):
        body = await app.chat_completions(
            make_request(
                "/v1/chat/completions",
                {
                    "model": "mock/t3",
                    "messages": [{"role": "user", "content": "Say hello"}],
                },
            )
        )

        self.assertEqual(body["object"], "chat.completion")
        self.assertTrue(body["id"].startswith("chatcmpl-"))
        self.assertEqual(body["model"], "mock/t3")
        self.assertEqual(len(body["choices"]), 1)
        choice = body["choices"][0]
        self.assertEqual(choice["index"], 0)
        self.assertEqual(choice["message"]["role"], "assistant")
        self.assertEqual(len(choice["message"]["content"].split()), 3)
        self.assertEqual(choice["finish_reason"], "stop")
        usage = body["usage"]
        self.assertGreater(usage["prompt_tokens"], 0)
        self.assertEqual(usage["completion_tokens"], 3)
        self.assertEqual(
            usage["total_tokens"], usage["prompt_tokens"] + usage["completion_tokens"]
        )

    async def test_chat_completions_streams_deltas_finish_and_done(self):
        response = await app.chat_completions(
            make_request(
                "/v1/chat/completions",
                {
                    "model": "mock/t3",
                    "stream": True,
                    # Usage chunks are covered by the incoming test_app.py suite.
                    "stream_options": {"include_usage": False},
                    "messages": [{"role": "user", "content": "hello"}],
                },
            )
        )
        self.assertEqual(response.media_type, "text/event-stream")
        stream = "".join([chunk async for chunk in response.body_iterator])
        events = [part for part in stream.strip().split("\n\n") if part]

        self.assertEqual(len(events), 5)
        self.assertEqual(events[-1], "data: [DONE]")
        self.assertTrue(all(event.startswith("data: ") for event in events))
        chunks = [json.loads(event.removeprefix("data: ")) for event in events[:-1]]
        for chunk in chunks:
            self.assertEqual(chunk["object"], "chat.completion.chunk")
            self.assertEqual(chunk["model"], "mock/t3")
            self.assertEqual(chunk["id"], chunks[0]["id"])
            self.assertEqual(len(chunk["choices"]), 1)
            self.assertEqual(chunk["choices"][0]["index"], 0)
        for chunk in chunks[:-1]:
            choice = chunk["choices"][0]
            self.assertEqual(len(choice["delta"]["content"].split()), 1)
            self.assertIsNone(choice["finish_reason"])
        self.assertEqual(
            chunks[-1]["choices"][0],
            {"index": 0, "delta": {}, "finish_reason": "stop"},
        )

    async def test_chat_completions_uses_openai_error_envelope(self):
        response = await app.chat_completions(
            make_request(
                "/v1/chat/completions",
                {
                    "model": "mock/e100",
                    "messages": [{"role": "user", "content": "fail"}],
                },
            )
        )
        body = json.loads(response.body)

        self.assertEqual(response.status_code, 500)
        self.assertEqual(set(body), {"error"})
        self.assertEqual(body["error"]["type"], "server_error")
        self.assertEqual(body["error"]["code"], 500)
        self.assertTrue(body["error"]["message"])

    async def test_chat_completions_rate_limit_includes_retry_after(self):
        response = await app.chat_completions(
            make_request(
                "/v1/chat/completions",
                {
                    "model": "mock/r100",
                    "messages": [{"role": "user", "content": "throttle"}],
                },
            )
        )
        body = json.loads(response.body)

        self.assertEqual(response.status_code, 429)
        self.assertEqual(response.headers["Retry-After"], "1")
        self.assertEqual(set(body), {"error"})
        self.assertEqual(body["error"]["type"], "rate_limit_error")
        self.assertEqual(body["error"]["code"], 429)
        self.assertTrue(body["error"]["message"])


class AnthropicMessagesTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        app.STATS = app.Stats()
        app._SEMAPHORES.clear()

    async def test_messages_returns_native_anthropic_shape(self):
        response = await app.anthropic_messages(
            make_request(
                "/v1/messages",
                {
                    "model": "mock/fast",
                    "max_tokens": 64,
                    "system": "Be concise.",
                    "messages": [{"role": "user", "content": "Say hello"}],
                },
            )
        )
        body = response

        self.assertEqual(body["type"], "message")
        self.assertEqual(body["role"], "assistant")
        self.assertEqual(body["model"], "mock/fast")
        self.assertEqual(body["stop_reason"], "end_turn")
        self.assertEqual(body["content"][0]["type"], "text")
        self.assertEqual(body["usage"]["input_tokens"], 4)
        self.assertEqual(body["usage"]["output_tokens"], 20)

    async def test_messages_streams_the_anthropic_event_sequence(self):
        response = await app.anthropic_messages(
            make_request(
                "/v1/messages",
                {
                    "model": "mock/t3",
                    "max_tokens": 64,
                    "stream": True,
                    "messages": [{"role": "user", "content": "hello"}],
                },
            )
        )
        stream = "".join([chunk async for chunk in response.body_iterator])
        events = [part for part in stream.strip().split("\n\n") if part]

        self.assertEqual(
            [event.splitlines()[0] for event in events],
            [
                "event: message_start",
                "event: content_block_start",
                "event: content_block_delta",
                "event: content_block_delta",
                "event: content_block_delta",
                "event: content_block_stop",
                "event: message_delta",
                "event: message_stop",
            ],
        )
        message_start = json.loads(events[0].split("data: ", 1)[1])
        message_delta = json.loads(events[-2].split("data: ", 1)[1])
        self.assertEqual(message_start["message"]["model"], "mock/t3")
        self.assertEqual(message_start["message"]["usage"]["input_tokens"], 1)
        self.assertEqual(message_delta["usage"]["output_tokens"], 3)

    async def test_count_tokens_returns_a_positive_deterministic_estimate(self):
        response = await app.anthropic_count_message_tokens(
            make_request(
                "/v1/messages/count_tokens",
                {
                    "model": "mock/fast",
                    "system": [{"type": "text", "text": "Be concise"}],
                    "messages": [{"role": "user", "content": "Count these words"}],
                },
            )
        )
        self.assertEqual(response, {"input_tokens": 5})

    async def test_messages_uses_anthropic_error_envelope(self):
        response = await app.anthropic_messages(
            make_request(
                "/v1/messages",
                {
                    "model": "mock/e100",
                    "max_tokens": 64,
                    "messages": [{"role": "user", "content": "fail"}],
                },
            )
        )
        body = json.loads(response.body)

        self.assertEqual(response.status_code, 500)
        self.assertEqual(body["type"], "error")
        self.assertEqual(body["error"]["type"], "api_error")
        self.assertTrue(body["request_id"].startswith("req_mock_"))


if __name__ == "__main__":
    unittest.main()
