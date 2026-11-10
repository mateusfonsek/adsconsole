//! Library for operating the Meta Marketing API (Graph v26.0), without MCP: login in the
//! system browser, tokens, Graph client, errors, retry, rate limiter, upload and
//! editable fields.
#![allow(clippy::result_large_err)]

pub mod client;
pub mod dates;
pub mod endpoints;
pub mod error;
pub mod fields;
pub mod limiter;
pub mod oauth;
pub mod retry;
pub mod token;
pub mod upload;
