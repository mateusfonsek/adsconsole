//! Núcleo do app, sem dependência do Tauri: testável com `cargo test`.

pub mod changes;
pub mod clients;
pub mod connections;
pub mod error;
pub mod events;
pub mod runtime;
pub mod services;
pub mod settings;

pub use error::AppError;
