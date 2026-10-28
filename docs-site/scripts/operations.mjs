// Behavior overlay only: routes and types are extracted from Rust. Any new handler
// fails generation until its support status and guide are reviewed here.
const entry = (status, guide, summary, request = [], response = []) => ({ status, guide, summary, request, response });
export const operations = {
  'chat::create_chat_completion': entry('Implemented', 'chat', 'Native Chat or capability-checked translation; JSON or SSE.', ['ChatCompletionRequest'], ['ChatCompletionResponse', 'ChatCompletionChunk']),
  'responses::create_response': entry('Implemented', 'responses', 'Native Responses or limited Chat translation; opaque lifecycle IDs.', ['CreateResponseRequest'], ['CreateResponseResponse']),
  'responses::compact_response': entry('Provider-dependent', 'responses', 'Native/adapted compaction only; arbitrary retained JSON; no fallback.'),
  'responses::retrieve_response': entry('Provider-dependent', 'responses', 'Connection-affine retrieval; stream and starting_after query fields.', ['RetrieveResponseQuery'], ['CreateResponseResponse']),
  'responses::cancel_response': entry('Provider-dependent', 'responses', 'Connection-affine native/adapted cancellation.', [], ['CreateResponseResponse']),
  'responses::delete_response': entry('Provider-dependent', 'responses', 'Connection-affine native/adapted deletion; upstream response body.'),
  'anthropic::proxy_messages': entry('Implemented', 'provider-apis', 'Native Anthropic body or restricted Chat translation; JSON/SSE.'),
  'anthropic::proxy_messages_count_tokens': entry('Provider-dependent', 'provider-apis', 'Native token count, no fallback; provider-shaped JSON.'),
  'gemini::create_interaction': entry('Provider-dependent', 'provider-apis', 'Native Gemini JSON/SSE; agents and background unsupported.'),
  'gemini::unsupported_interaction_operation': entry('Placeholder · 501', 'provider-apis', 'Interaction lifecycle is not implemented.'),
  'decisions::create_decision': entry('Implemented', 'decisions', 'Router extension; canonical decision questions.', ['DecisionsRequest'], ['DecisionsResponse']),
  'decisions::create_openai_decision': entry('Implemented', 'decisions', 'OpenAI-shaped ingress translated to a capable decision model.', ['OpenAiDecisionsRequest'], []),
  'decisions::create_system_one': entry('Implemented', 'decisions', 'TypeSafe-shaped ingress.', ['SystemOneRequest'], ['SystemOneResponse']),
  'traces::ingest_spans': entry('Implemented', 'operations', 'Router extension; client trace ingestion.', ['IngestSpansRequest'], ['IngestSpansResponse']),
  'models::list_models': entry('Implemented', 'models', 'Permission-filtered model catalog with capabilities; no pagination.', [], ['ListResponse', 'Model']),
  'models::get_model': entry('Implemented', 'models', 'Resolve one visible model.', [], ['Model']),
  'models::project_models_for_vscode': entry('Implemented', 'models', 'Router extension; generated VS Code configuration.', ['VscodeProjectionQuery'], ['VscodeModelConfigList']),
  'models::delete_model': entry('Disabled · 501', 'models', 'Use the authorized ModelRouteService/DeleteModelRoute admin RPC; never mutates through inference auth.'),
  'embeddings::create_embedding': entry('Provider-dependent', 'media', 'Text, token, or multimodal embeddings depending on the route.', ['CreateEmbeddingRequest'], ['CreateEmbeddingResponse']),
  'images::create_image': entry('Provider-dependent', 'media', 'JSON image generation; stream:true returns 501.', ['CreateImageRequest'], ['ImageResponse']),
  'images::create_image_edit': entry('Placeholder · 501', 'media', 'Image editing not implemented; current extractor expects JSON.'),
  'images::create_image_variation': entry('Placeholder · 501', 'media', 'Image variation not implemented; current extractor expects JSON.'),
  'audio::create_speech': entry('Provider-dependent', 'media', 'JSON request; binary/provider response buffered.', ['CreateSpeechRequest']),
  'audio::create_transcription': entry('Provider-dependent', 'media', 'Multipart file and model; provider response buffered.', [], ['TranscriptionResponse']),
  'audio::create_translation': entry('Provider-dependent', 'media', 'Multipart file and model; provider response buffered.', [], ['TranscriptionResponse']),
  'moderations::create_moderation': entry('Provider-dependent', 'media', 'Text moderation; default model omni-moderation-latest.', ['CreateModerationRequest'], ['CreateModerationResponse']),
  'files::upload_file': entry('Disabled · 501', 'media', 'Uploads disabled for all callers pending file-specific grants, resource limits, and owned lifecycle.'),
  health_check: entry('Implemented', 'operations', 'Unauthenticated HTTP health; plain text OK.'),
  v1_healthcheck: entry('Implemented', 'operations', 'Unauthenticated HTTP health; fixed profile/status JSON.'),
};
for (const [surface, names] of Object.entries({
  assistants: ['list_assistants','create_assistant','delete_assistant'],
  batch: ['create_batch','list_batches'], fine_tuning: ['create_fine_tuning_job','list_fine_tuning_jobs'],
  files: ['list_files','delete_file','download_file'],
})) for (const name of names) operations[`${surface}::${name}`] = entry('Placeholder · 501', 'media', 'Registered but not implemented.');
for (const [surface, names] of Object.entries({
  assistants: ['get_assistant','modify_assistant'], batch: ['get_batch','cancel_batch'],
  fine_tuning: ['get_fine_tuning_job','cancel_fine_tuning_job'], files: ['get_file'],
})) for (const name of names) operations[`${surface}::${name}`] = entry('Placeholder · 404', 'media', 'Always not found; no implemented resource lifecycle.');
