//! Operações do Google Ads (REST). Biblioteca simples: não depende de `adsconsole-mcp` nem de `rmcp`.

pub mod client;
pub mod client_json;
pub mod endpoints;
pub mod error;
pub mod fields;
pub mod gaql;
pub mod images;
pub mod meter;
pub mod oauth;
pub mod resources;
pub mod retry;
pub mod token;
pub mod upload;
