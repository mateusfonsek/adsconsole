//! Núcleo do app, sem dependência do Tauri: testável com `cargo test`.

pub mod error;
pub mod runtime;

pub use error::AppError;
