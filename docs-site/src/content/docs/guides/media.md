---
title: Embeddings, media, moderation, and files
---

These operations depend on the selected route's capabilities. Native JSON envelopes for images and speech preserve fields beyond the typed view. Provider acceptance is not established by deserializing a request locally.

## Embeddings

`POST /v1/embeddings` uses [CreateEmbeddingRequest](/reference/http-types/#createembeddingrequest) and returns [CreateEmbeddingResponse](/reference/http-types/#createembeddingresponse). `model` and `input` are required. Input can be a string, strings, token IDs, token-ID batches, or multimodal text/image parts. A multimodal part array represents **one document**, not a batch of independent embeddings. Supported forms, dimensions, and encoding depend on the model/provider. Output vectors can be float arrays or base64 strings. Missing upstream indexes are assigned by response order. [Example](/reference/examples/#text-embeddings).

## Images

`POST /v1/images/generations` uses the [CreateImageRequest](/reference/http-types/#createimagerequest) view and [ImageResponse](/reference/http-types/#imageresponse). `prompt` is required; omitted `model` uses the default connection/provider model. Native providers may support extra parameters. Responses contain `data` items with URL/base64/revised-prompt fields; Google-compatible responses can omit `created`, which defaults to zero in the typed view.

`background` and `output_format` target providers such as OpenAI's image family. `aspect_ratio` and `image_size` are provider-specific extensions, not universal OpenAI fields; Google's compatibility endpoint uses `size` and rejects top-level `image_size`. `stream:true` returns 501. Image edits and variations are 501 placeholders with JSON extractors, not working multipart APIs. [Example](/reference/examples/#image-generation).

## Audio

`POST /v1/audio/speech` accepts [CreateSpeechRequest](/reference/http-types/#createspeechrequest): `model`, `input`, and `voice`, plus optional format/speed and native extra fields. The response is provider audio bytes/content type, not the Chat JSON envelope.

`POST /v1/audio/transcriptions` and `/v1/audio/translations` accept multipart form data with exactly one nonempty UTF-8 `model` field and at least one `file`. Native multipart forwarding preserves order, repeated fields, MIME types, filenames, and file bytes. Additional fields and response formats are provider-specific. [TranscriptionResponse](/reference/http-types/#transcriptionresponse) describes the simple JSON text view; it is not the full native response contract.

```sh
curl --fail-with-body "$ROUTER_URL/v1/audio/transcriptions" \
  -H "Authorization: Bearer $ROUTER_API_KEY" \
  -F 'model=example-connection/example-transcription' \
  -F 'file=@sample.wav;type=audio/wav' \
  -F 'response_format=json'
```

Audio request/response bodies are buffered, including upstream SSE. Do not expect incremental audio streaming through this implementation. The server applies a 100 MiB body limit, with extraction/provider limits potentially tighter.

## Moderation

`POST /v1/moderations` uses [CreateModerationRequest](/reference/http-types/#createmoderationrequest) and returns [CreateModerationResponse](/reference/http-types/#createmoderationresponse). It supports text input (one string or strings), with default model `omni-moderation-latest`. Permission/capability checks apply; there is no fallback. Requests are metered with zero token usage. [Example](/reference/examples/#text-moderation).

## File upload and unsupported lifecycle APIs

`POST /v1/files` is disabled with 501 for every caller after normal authentication,
without parsing or storing the body. No object-store setting or admin credential
enables it. File-specific permissions, bounded resource admission, owned IDs,
deletion and retention are prerequisites for any future upload support.

| Surface | Current behavior |
| --- | --- |
| Files list/delete/content | 501 |
| File retrieve | 404 |
| Assistants list/create/delete | 501 |
| Assistant retrieve/modify | 404 |
| Batch and Fine-tuning create/list | 501 |
| Batch and Fine-tuning retrieve/cancel | 404 |

These are post-authentication/extraction results. Pagination structs for placeholder list handlers do not mean pagination works. See [all mounted methods](/reference/http/) for exact paths.

Sources: [media handlers](https://github.com/chalk-ai/chalk-router/tree/main/src/handlers), [native fidelity tests](https://github.com/chalk-ai/chalk-router/blob/main/tests/integration/native_fidelity_test.rs), [audio tests](https://github.com/chalk-ai/chalk-router/blob/main/tests/integration/native_audio_test.rs), [unsupported endpoint tests](https://github.com/chalk-ai/chalk-router/blob/main/tests/integration/unsupported_endpoints_test.rs).
