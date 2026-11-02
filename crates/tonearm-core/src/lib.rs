//! Platform-independent core of Tonearm: no toolkit, media framework, secret
//! store or desktop bus dependencies. The desktop app and future front ends
//! (an iOS app through `UniFFI`) build on it.

// Public API lints for a library: the modules were ported from the binary
// crate, where these do not apply. Errors are documented on `CoreError`.
#![allow(clippy::must_use_candidate, reason = "ported getters; callers always use the values")]
#![allow(clippy::missing_errors_doc, reason = "errors are network/JSON failures, see the error type")]

pub mod api;
pub mod auth;
pub mod cache;
pub mod config;
pub mod country;
pub mod discover;
pub mod explore;
pub mod error;
pub mod i18n;
pub mod live;
pub mod model;
pub mod playqueue;
pub mod scrobbling;
pub mod service;
pub mod signals;
pub mod state;
pub use error::{CoreError, CoreResult};
