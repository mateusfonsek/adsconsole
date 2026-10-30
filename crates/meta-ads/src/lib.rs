//! Biblioteca de operação da Meta Marketing API (Graph v26.0), sem MCP: login no
//! navegador do sistema, tokens, cliente Graph, erros, retry, rate limiter, upload e
//! campos editáveis.
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
