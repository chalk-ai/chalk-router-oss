# syntax=docker/dockerfile:1
#
# Standalone image for the mock OpenAI-compatible test service in this
# directory. This is the single image definition used both by `just
# mock-openai` locally and by the benchmark-cluster deployment.

FROM ghcr.io/astral-sh/uv:0.12.5 AS uv

FROM python:3.13-slim

WORKDIR /app

COPY --from=uv /uv /uvx /bin/
COPY pyproject.toml uv.lock ./
RUN uv sync --locked --no-dev --no-install-project

COPY app.py .

EXPOSE 8000

# One worker makes the service's in-memory concurrency caps and debug counters
# unambiguous for the default one-replica benchmark deployment.
CMD ["/app/.venv/bin/python", "-m", "uvicorn", "app:app", "--host", "0.0.0.0", "--port", "8000", "--workers", "1"]
