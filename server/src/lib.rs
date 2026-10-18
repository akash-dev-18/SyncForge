//! SyncEngine coordination server: the `/v1` control-plane API and the
//! end-to-end-opaque relay.
//!
//! The server stores metadata only (SRD §9): device identities, pairing
//! state, relationships, folder memberships, and relay-session bookkeeping.
//! It never stores file contents or device-local absolute paths, and the
//! relay never inspects payloads.

pub mod api;
pub mod auth;
pub mod db;
pub mod error;
pub mod models;
pub mod relay;
pub mod state;

pub use api::app;
pub use state::AppState;
