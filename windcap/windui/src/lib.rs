//! The data half of the Windrecorder window: what a search returns, what a day holds, what the
//! recorder is configured to do, and what the settings and AI pages read and write.
//!
//! This is a library because two different front ends have to agree on all of it. `src/main.rs` is
//! the egui/eframe window; `winduiweb` is a Tauri window that draws the same six screens in HTML.
//! The alternative — the second front end re-implementing the query, the pagination, the thumbnail
//! decode and the settings validation — is precisely the divergence this workspace refuses
//! elsewhere: two readers of `video_text` that disagree about what a row means, or two validators
//! of `windai`'s keys where a rename silently leaves one of them reading a default.
//!
//! What is deliberately *not* here is anything that draws. `view`, `app`, `textures`, `thumbs` and
//! `workers` stay in the binary: egui texture handles, a frame callback and a repaint signal have
//! no meaning to a webview, and an `egui::TextureHandle` leaking into this layer is the thing the
//! module comments in [`model`] already rule out — a row carries decoded pixels, not a handle, so
//! the decoder needs no `Context` and the test can assert on bytes.
//!
//! Reads go through `wind-store`, always. That is the contract the recorder's live databases depend
//! on: a front end that opened `video_text` directly is how a segment commit gets blocked, and both
//! windows inherit the rule from [`backend`] rather than restating it.

pub mod ai;
pub mod backend;
pub mod flags;
pub mod highlight;
pub mod model;
pub mod play;
pub mod record;
pub mod segments;
pub mod settings;
pub mod wordcloud;

#[cfg(test)]
mod scratch;
