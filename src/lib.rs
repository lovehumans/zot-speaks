//! zot-speaks' speaking engine: where the model lives, how it is loaded, what the settings mean
//! to `ptts`, which voices the local cache holds, and how the PCM that comes out reaches a player.
//!
//! The zot extension — the JSON-lines protocol, `/tts`, `/tts-settings` and its panel — is the
//! `zot-speaks` binary next to this library, not part of it. The library is a separate target for
//! one reason: `examples/` then measures the pipeline the extension actually ships (cache
//! precedence, inference-pool sizing, `Quant::F32`, voice registration) instead of a
//! reimplementation of it that can drift. The same reasoning puts the audio sink here rather than
//! inline in the playback loop: its rules are the product's, and they get the tests CI runs.
//!
//! Nothing here reaches the network. `ptts` is built without its `hub` feature, and every model and
//! voice is resolved from the Hugging Face cache read-only.

pub mod cache;
pub mod settings;
pub mod sink;
pub mod tts;
pub mod voices;
