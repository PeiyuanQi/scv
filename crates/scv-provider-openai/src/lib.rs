//! Streaming OpenAI-compatible Responses provider: [`OpenAiProvider`]
//! implements `scv_core::Provider` over a `/responses` endpoint, shaping each
//! request from the conversation and assembling streamed text and tool calls
//! into one assistant message.

#![forbid(unsafe_code)]

mod encode;
mod request;
mod stream;
mod wire;

pub use encode::MAX_IMAGE_BYTES;
pub use request::{OpenAiProvider, ProviderLimits};
