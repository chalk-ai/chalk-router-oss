"""Mock OpenAI- and Anthropic-compatible server for load-testing chalk-router.

Not part of the router build -- a deployable test service for pointing a
dedicated `openai-compatible` provider connection at something we control, so
load tests can exercise the router's own behavior (routing, streaming,
timeouts, error/status pass-through, connection handling) against known,
deterministic upstream behavior instead of a real provider's variable
latency/availability.

Scenario knobs are encoded in the `model` string instead of custom body
fields, so any real OpenAI or Anthropic client (chalk-router's upstream client
included) can drive them unmodified:

    mock/l<base_ms>-j<jitter_ms>-t<tokens>-e<err_pct>-r<429_pct>-c<max_concurrency>

Each value may also be a range `<lo>-<hi>`, resampled per request, e.g.
`mock/t100-2000` gives every response a random length between 100 and 2000
"tokens" (words). All fields optional, defaults: l0 j0 t50 e0 r0 c0
(c0 = unlimited). Presets: mock/fast, mock/slow, mock/huge, mock/flaky,
mock/throttled.

Endpoints: POST /v1/chat/completions and POST /v1/messages (both sync + SSE
streaming), POST /v1/messages/count_tokens, POST /v1/embeddings,
GET /v1/models, GET /health, GET /debug/stats.

See README.md in this directory for how to run it locally and use its
same-cluster benchmark deployment.
"""

from __future__ import annotations

import asyncio
import hashlib
import json
import random
import re
import time
import uuid
from collections.abc import AsyncIterable
from dataclasses import dataclass, field

from fastapi import FastAPI, Request
from fastapi.responses import JSONResponse, StreamingResponse
from starlette.types import Receive, Scope, Send

app = FastAPI()

PRESETS = {
    "fast": "l10-j5-t20-e0-r0-c0",
    "slow": "l3000-j500-t50-e0-r0-c0",
    "huge": "l500-j100-t4000-e0-r0-c0",
    "flaky": "l100-j50-t50-e15-r0-c0",
    "throttled": "l50-j20-t50-e0-r40-c0",
}

_PROFILE_RE = re.compile(r"(?P<key>[ljterc])(?P<lo>\d+)(?:-(?P<hi>\d+))?")


@dataclass
class Profile:
    base_ms: int = 0
    jitter_ms: int = 0
    tokens: int = 50
    error_pct: int = 0
    throttle_pct: int = 0
    max_concurrency: int = 0


@dataclass
class Stats:
    in_flight: int = 0
    total: int = 0
    by_profile: dict = field(default_factory=dict)


STATS = Stats()
_SEMAPHORES: dict[int, asyncio.Semaphore] = {}


def parse_model(model: str) -> Profile:
    """Parse a `mock/<profile>` model string.

    Each field is `<key><value>`, where `<value>` is either a fixed integer
    (`t100`) or an inclusive range `<lo>-<hi>` (`t100-500`) sampled fresh per
    request -- e.g. `mock/l50-500-t100-2000` gives every request its own
    random latency in [50, 500]ms and response length in [100, 2000] tokens.
    """
    profile_str = model.split("/", 1)[1] if "/" in model else model
    profile_str = PRESETS.get(profile_str, profile_str)
    p = Profile()
    for m in _PROFILE_RE.finditer(profile_str):
        key = m.group("key")
        lo, hi = int(m.group("lo")), m.group("hi")
        val = random.randint(lo, int(hi)) if hi is not None else lo
        if key == "l":
            p.base_ms = val
        elif key == "j":
            p.jitter_ms = val
        elif key == "t":
            p.tokens = val
        elif key == "e":
            p.error_pct = val
        elif key == "r":
            p.throttle_pct = val
        elif key == "c":
            p.max_concurrency = val
    return p


def _semaphore_for(max_concurrency: int) -> asyncio.Semaphore | None:
    if max_concurrency <= 0:
        return None
    sem = _SEMAPHORES.get(max_concurrency)
    if sem is None:
        sem = asyncio.Semaphore(max_concurrency)
        _SEMAPHORES[max_concurrency] = sem
    return sem


def _latency_seconds(p: Profile) -> float:
    jitter = random.uniform(-p.jitter_ms, p.jitter_ms) if p.jitter_ms else 0
    return max(0.0, (p.base_ms + jitter)) / 1000.0


def _lorem_words(n: int, seed: str) -> list[str]:
    vocab = (
        "the quick brown fox jumps over lazy dog chalk router mock openai "
        "feature store inference latency stream token embedding vector"
    ).split()
    rng = random.Random(seed)
    return [rng.choice(vocab) for _ in range(n)]


def _openai_error(status: int, message: str, err_type: str) -> JSONResponse:
    body = {"error": {"message": message, "type": err_type, "code": status}}
    headers = {}
    if status == 429:
        headers["Retry-After"] = "1"
    return JSONResponse(body, status_code=status, headers=headers)


def _anthropic_error(status: int, message: str, err_type: str) -> JSONResponse:
    """Return the native Anthropic error envelope, not an OpenAI-shaped one."""
    body = {
        "type": "error",
        "error": {"type": err_type, "message": message},
        "request_id": f"req_mock_{uuid.uuid4().hex[:20]}",
    }
    headers = {}
    if status == 429:
        headers["Retry-After"] = "1"
    return JSONResponse(body, status_code=status, headers=headers)


def _anthropic_text(value: object) -> str:
    """Collect text fields from a Messages request for deterministic mock output."""
    if isinstance(value, str):
        return value
    if isinstance(value, list):
        return " ".join(_anthropic_text(item) for item in value)
    if isinstance(value, dict):
        return " ".join(
            _anthropic_text(child)
            for key, child in value.items()
            if key in {"content", "text", "input", "system", "messages"}
        )
    return ""


def _anthropic_input_tokens(body: dict) -> int:
    # This is intentionally a rough, transparent estimate: the mock controls
    # behavior rather than claiming Anthropic's tokenizer semantics.
    text = _anthropic_text({"system": body.get("system"), "messages": body.get("messages", [])})
    return len(text.split())


def _anthropic_sse(event: str, payload: dict) -> str:
    return f"event: {event}\ndata: {json.dumps(payload)}\n\n"


@app.get("/health")
async def health():
    return {"status": "ok"}


@app.get("/v1/models")
async def list_models():
    now = int(time.time())
    created_at = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(now))
    return {
        "object": "list",
        # Extra Anthropic list fields are harmless to OpenAI clients and let
        # the native Anthropic backend exercise its model-discovery parser.
        "has_more": False,
        "last_id": None,
        "data": [
            {
                "id": f"mock/{name}",
                "object": "model",
                "created": now,
                "created_at": created_at,
                "owned_by": "mock",
            }
            for name in list(PRESETS) + ["custom"]
        ],
    }


async def _gate(profile: Profile, profile_key: str):
    """Count pending work and acquire a slot without leaking canceled waiters."""
    STATS.in_flight += 1
    STATS.total += 1
    STATS.by_profile[profile_key] = STATS.by_profile.get(profile_key, 0) + 1
    sem = _semaphore_for(profile.max_concurrency)
    try:
        if sem is not None:
            await sem.acquire()
    except BaseException:
        # A canceled acquire owns no permit; asyncio restores any raced grant.
        STATS.in_flight -= 1
        raise
    return sem


def _release(sem: asyncio.Semaphore | None) -> None:
    """Finish one admitted request and return its optional concurrency slot."""
    STATS.in_flight -= 1
    if sem is not None:
        sem.release()


class _ConcurrencyLimitedStreamingResponse(StreamingResponse):
    """Own an admitted request until ASGI streaming ends, including disconnects."""

    def __init__(self, content: AsyncIterable[str], sem: asyncio.Semaphore | None):
        """Take ownership of the caller's acquired slot after construction."""
        super().__init__(content, media_type="text/event-stream")
        self._sem = sem

    async def __call__(self, scope: Scope, receive: Receive, send: Send) -> None:
        """Release after iteration and sending, even if either fails or is canceled."""
        try:
            await super().__call__(scope, receive, send)
        finally:
            _release(self._sem)


@app.get("/debug/stats")
async def debug_stats():
    return {
        "in_flight": STATS.in_flight,
        "total": STATS.total,
        "by_profile": STATS.by_profile,
    }


@app.post("/v1/chat/completions")
async def chat_completions(request: Request):
    """Hold the request's slot through completion or the entire streaming response."""
    body = await request.json()
    model = body.get("model", "mock/fast")
    profile = parse_model(model)
    sem = await _gate(profile, model)
    owns_slot = True
    try:
        if random.randint(1, 100) <= profile.throttle_pct:
            return _openai_error(429, "rate limit exceeded (mock)", "rate_limit_error")
        await asyncio.sleep(_latency_seconds(profile))
        if random.randint(1, 100) <= profile.error_pct:
            return _openai_error(500, "mock upstream error", "server_error")

        messages = body.get("messages", [])
        seed = json.dumps(messages)[:200]
        words = _lorem_words(profile.tokens, seed)
        completion_id = f"chatcmpl-mock-{uuid.uuid4().hex[:20]}"
        created = int(time.time())
        usage = {
            "prompt_tokens": len(seed.split()),
            "completion_tokens": len(words),
            "total_tokens": len(seed.split()) + len(words),
        }

        if body.get("stream"):

            async def gen():
                """Emit content and optional terminal usage before releasing the slot."""
                # Generic router connections strip stream_options, so report
                # usage by default; direct clients can opt out for failure tests.
                include_usage = (body.get("stream_options") or {}).get("include_usage", True)
                for i, w in enumerate(words):
                    chunk = {
                        "id": completion_id,
                        "object": "chat.completion.chunk",
                        "created": created,
                        "model": model,
                        "choices": [
                            {
                                "index": 0,
                                "delta": {"content": (w + " ")},
                                "finish_reason": None,
                            }
                        ],
                    }
                    if include_usage:
                        chunk["usage"] = None
                    yield f"data: {json.dumps(chunk)}\n\n"
                    if profile.base_ms or profile.jitter_ms:
                        await asyncio.sleep(_latency_seconds(profile) / max(len(words), 1))
                final = {
                    "id": completion_id,
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": model,
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                }
                if include_usage:
                    final["usage"] = None
                yield f"data: {json.dumps(final)}\n\n"
                if include_usage:
                    yield f"data: {json.dumps({**final, 'choices': [], 'usage': usage})}\n\n"
                yield "data: [DONE]\n\n"

            response = _ConcurrencyLimitedStreamingResponse(gen(), sem)
            owns_slot = False
            return response

        content = " ".join(words)
        return {
            "id": completion_id,
            "object": "chat.completion",
            "created": created,
            "model": model,
            "choices": [
                {
                    "index": 0,
                    "message": {"role": "assistant", "content": content},
                    "finish_reason": "stop",
                }
            ],
            "usage": usage,
        }
    finally:
        if owns_slot:
            _release(sem)


@app.post("/v1/messages")
async def anthropic_messages(request: Request):
    """Minimal native Anthropic Messages API for router upstream tests."""
    body = await request.json()
    model = body.get("model", "mock/fast")
    profile = parse_model(model)
    sem = await _gate(profile, model)
    try:
        if random.randint(1, 100) <= profile.throttle_pct:
            return _anthropic_error(429, "rate limit exceeded (mock)", "rate_limit_error")
        if not body.get("stream"):
            await asyncio.sleep(_latency_seconds(profile))
        if random.randint(1, 100) <= profile.error_pct:
            return _anthropic_error(500, "mock upstream error", "api_error")

        input_tokens = _anthropic_input_tokens(body)
        words = _lorem_words(profile.tokens, _anthropic_text(body)[:200])
        message_id = f"msg_mock_{uuid.uuid4().hex[:20]}"

        if body.get("stream"):

            async def gen():
                yield _anthropic_sse(
                    "message_start",
                    {
                        "type": "message_start",
                        "message": {
                            "id": message_id,
                            "type": "message",
                            "role": "assistant",
                            "content": [],
                            "model": model,
                            "stop_reason": None,
                            "stop_sequence": None,
                            "usage": {"input_tokens": input_tokens, "output_tokens": 0},
                        },
                    },
                )
                yield _anthropic_sse(
                    "content_block_start",
                    {
                        "type": "content_block_start",
                        "index": 0,
                        "content_block": {"type": "text", "text": ""},
                    },
                )
                for word in words:
                    yield _anthropic_sse(
                        "content_block_delta",
                        {
                            "type": "content_block_delta",
                            "index": 0,
                            "delta": {"type": "text_delta", "text": f"{word} "},
                        },
                    )
                    if profile.base_ms or profile.jitter_ms:
                        await asyncio.sleep(_latency_seconds(profile) / max(len(words), 1))
                yield _anthropic_sse(
                    "content_block_stop",
                    {"type": "content_block_stop", "index": 0},
                )
                yield _anthropic_sse(
                    "message_delta",
                    {
                        "type": "message_delta",
                        "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                        "usage": {"output_tokens": len(words)},
                    },
                )
                yield _anthropic_sse("message_stop", {"type": "message_stop"})

            return StreamingResponse(gen(), media_type="text/event-stream")

        return {
            "id": message_id,
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": " ".join(words)}],
            "model": model,
            "stop_reason": "end_turn",
            "stop_sequence": None,
            "usage": {"input_tokens": input_tokens, "output_tokens": len(words)},
        }
    finally:
        STATS.in_flight -= 1
        if sem is not None:
            sem.release()


@app.post("/v1/messages/count_tokens")
async def anthropic_count_message_tokens(request: Request):
    """Return a deterministic, approximate count for native Anthropic calls."""
    body = await request.json()
    model = body.get("model", "mock/fast")
    profile = parse_model(model)
    sem = await _gate(profile, model)
    try:
        if random.randint(1, 100) <= profile.throttle_pct:
            return _anthropic_error(429, "rate limit exceeded (mock)", "rate_limit_error")
        await asyncio.sleep(_latency_seconds(profile))
        if random.randint(1, 100) <= profile.error_pct:
            return _anthropic_error(500, "mock upstream error", "api_error")
        return {"input_tokens": _anthropic_input_tokens(body)}
    finally:
        STATS.in_flight -= 1
        if sem is not None:
            sem.release()


@app.post("/v1/embeddings")
async def embeddings(request: Request):
    """Hold a concurrency slot until the synthetic embedding result is ready."""
    body = await request.json()
    model = body.get("model", "mock/fast")
    profile = parse_model(model)
    sem = await _gate(profile, model)
    try:
        if random.randint(1, 100) <= profile.throttle_pct:
            return _openai_error(429, "rate limit exceeded (mock)", "rate_limit_error")
        await asyncio.sleep(_latency_seconds(profile))
        if random.randint(1, 100) <= profile.error_pct:
            return _openai_error(500, "mock upstream error", "server_error")

        inputs = body.get("input", "")
        inputs = inputs if isinstance(inputs, list) else [inputs]
        dim = max(profile.tokens, 8)
        data = []
        for i, text in enumerate(inputs):
            h = hashlib.sha256(str(text).encode()).digest()
            rng = random.Random(h)
            vec = [rng.uniform(-1, 1) for _ in range(dim)]
            data.append({"object": "embedding", "index": i, "embedding": vec})
        return {
            "object": "list",
            "data": data,
            "model": model,
            "usage": {"prompt_tokens": sum(len(str(t)) for t in inputs), "total_tokens": 0},
        }
    finally:
        _release(sem)
