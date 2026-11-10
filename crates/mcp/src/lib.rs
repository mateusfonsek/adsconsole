// `ToolError` (public contract, fixed fields) exceeds the lint limit; the contract
// returns `Result<_, ToolError>` everywhere, and a tool error is not a hot path.
#![allow(clippy::result_large_err)]

pub mod adapter;
pub mod diff;
pub mod engine;
pub mod error;
pub mod fake;
pub mod google;
pub mod http;
pub mod meta;
pub mod registry;
pub mod server;
pub mod truncate;

pub use adapter::{
    AccountRef, Applied, Call, PlatformAdapter, Progress, ToolKind, ToolSpec, Validated, schema_for,
};
pub use engine::{Engine, EngineEvent, SharedConfig};
pub use error::ToolError;
pub use registry::ToolRegistry;
