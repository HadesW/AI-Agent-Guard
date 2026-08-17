pub mod audit;
pub mod claude;
pub mod codex;
pub mod compatible;
pub mod core;
pub mod cursor;
pub mod gemini;
pub mod kiro;
pub mod opencode;
pub mod paths;
pub mod policy;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
