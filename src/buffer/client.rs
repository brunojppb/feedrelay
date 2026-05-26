//! Buffer HTTP client.
//!
//! [`BufferClient`] wraps a `reqwest::Client` and holds the API key and base URL.
//! One client instance is reused across requests — `reqwest::Client` is `Arc`-backed
//! and safe to clone / share across threads.

/// Buffer API client.  Clone-cheap (inner `reqwest::Client` is Arc-backed).
#[derive(Debug, Clone)]
pub struct BufferClient {
    pub(crate) http: reqwest::Client,
    pub(crate) api_key: String,
    /// Base URL without trailing slash.  Default: `"https://api.buffer.com"`.
    pub(crate) base_url: String,
}

impl BufferClient {
    /// Construct a client targeting the default Buffer endpoint (`https://api.buffer.com`).
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url(api_key, "https://api.buffer.com")
    }

    /// Construct a client with a custom base URL — wiremock tests use this.
    pub fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::default(),
            api_key: api_key.into(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
        }
    }
}
