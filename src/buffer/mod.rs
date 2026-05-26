// Forward-declared for Task 5; suppress dead-code lints until the worker wires this up.
#![allow(dead_code, unused_imports)]
//! Buffer GraphQL client for scheduling Instagram posts.
//!
//! Public API:
//! - [`BufferClient`] — re-exported for callers that need to construct the client.
//! - [`schedule_post`] — schedules an Instagram post via the Buffer GraphQL API.
//! - [`ScheduledPost`] — the successful scheduling result.
//! - [`BufferError`] — error variants.

pub mod client;
pub mod mutations;

pub use client::BufferClient;
pub use mutations::{BufferError, ScheduledPost, schedule_post};
