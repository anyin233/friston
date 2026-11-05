//! Client side of the native engine protocol.
//!
//! Meeting Master runs two Swift helper processes next to the GPUI app:
//!
//! * `mm-meetingd` – capture (ScreenCaptureKit / AVFoundation / Core Audio taps),
//!   streaming ASR, diarization, spatial cues, voiceprints, calendar (EventKit).
//! * `mm-embedd`   – EmbeddingGemma 2 multimodal embeddings (and optional local VLM).
//!
//! They are separate processes because (a) their MLX package graphs pin different
//! `mlx-swift-lm` versions and (b) background indexing must never starve the
//! real-time caption path. Both speak newline-delimited JSON-RPC 2.0 over stdio,
//! the same framing the Codex app-server uses.

pub mod client;
pub mod methods;
pub mod protocol;

pub use client::{EngineClient, EngineEvent};
pub use protocol::{Incoming, RpcError};
