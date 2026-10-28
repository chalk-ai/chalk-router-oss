//! Pure API-format conversion and event encoding, without backend execution.

pub mod anthropic_chat;
pub(crate) mod chat_controls;
pub mod decisions_openai;
pub(crate) mod gemini_embeddings;
pub mod responses_chat;
