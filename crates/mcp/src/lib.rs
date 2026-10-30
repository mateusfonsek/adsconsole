// `ToolError` (contrato público, campos fixos) passa do limite do lint; o contrato
// devolve `Result<_, ToolError>` em toda parte, e erro de ferramenta não é caminho quente.
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
