//! App core, with no Tauri dependency: testable with `cargo test`.

pub mod changes;
pub mod clients;
pub mod connections;
pub mod demo;
pub mod error;
pub mod events;
pub mod jobs;
pub mod runtime;
pub mod services;
pub mod settings;

pub use error::AppError;
