# frontend_test — router smoke-test client

[Documentation index](../docs/README.md) · [Development checks](../docs/development.md#testing)

A small OpenAI-SDK-based test client for exercising the Chalk Router end to end:
chat completions, embeddings, tool calling, and structured output across OpenAI
and Anthropic models. It runs against a **running router** (local or remote) and
prints a pass/fail summary.

> This is a manual, local harness — it is **not** part of `cargo test` and is
> not deployed. The older shell-based OpenCode check has stale credential
> assumptions; see [development checks](../docs/development.md#testing) before use.

## Test coverage

1. **Basic functionality** — chat completions, embeddings
2. **Tool calling** — simple (single function) and complex (multiple functions)
3. **Structured output** — simple JSON schemas and nested schemas with arrays

## Running

Start a local router using the [setup guide](../README.md#building--running)
and configure the required provider connections. These tests can make paid
provider calls. Then, in another terminal:

```bash
cd frontend_test
npm ci
ROUTER_BASE_URL=http://localhost:4001/v1 OPENAI_API_KEY="$ROUTER_API_KEY" npm test
```

`npm test` runs `tsx test-client.ts`. Set `ROUTER_API_KEY` to a router credential
authorized for the tested models. Despite its name, the client's
`OPENAI_API_KEY` is sent to the router; do not pass an upstream provider key.
Provider credentials belong in the router's saved connections.

## Configuring models

By default the client tests a built-in set of OpenAI and Anthropic models.
Override the tool-calling and structured-output model lists via environment
variables (comma-separated; the example uses a connection with the `anthropic/`
prefix):

```bash
export OPENAI_TEST_MODELS="gpt-5.2,gpt-5.1"
export ANTHROPIC_TEST_MODELS="anthropic/claude-opus-4-5,anthropic/claude-sonnet-4-5"
ROUTER_BASE_URL=http://localhost:4001/v1 OPENAI_API_KEY="$ROUTER_API_KEY" npm test
```

The basic chat and embedding checks still use `gpt-4` and
`text-embedding-ada-002` directly; model-list overrides do not change them.

## Environment variables

- `ROUTER_BASE_URL` — the router endpoint to test against. **Always set this**:
  the hardcoded default in `test-client.ts` still points at the retired
  `chalk-router-develop` Cloud Run URL.
- `OPENAI_API_KEY` — the credential this OpenAI SDK client sends to the router.
  The client loads `.env` from its working directory; `ANTHROPIC_API_KEY` is not
  read by the harness.
- `OPENAI_TEST_MODELS` / `ANTHROPIC_TEST_MODELS` — comma-separated model overrides

## Notes on model support

### max_tokens vs max_completion_tokens

Some newer OpenAI models require `max_completion_tokens` instead of `max_tokens`.
The client handles this by model name; to add a model, update the
`USES_MAX_COMPLETION_TOKENS` array in `test-client.ts`.

### Anthropic model names

Use the saved connection's prefix (for example, `anthropic/`) or a published
`chalk/*` route. The router resolves it before forwarding. Model access depends
on both the router key's permissions and the connection's upstream account.
