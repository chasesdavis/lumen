//! lumen-mcp-server: MCP server exposing Lumen tools to AI agents.

pub mod tools;

mod gc_bridge;
pub mod session;

pub use gc_bridge::{gc_run_blocking, GcContext};
