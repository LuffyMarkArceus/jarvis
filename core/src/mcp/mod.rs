//! MCP (Model Context Protocol) client.
//!
//! Starts configured stdio servers, discovers tools, and executes them.
//! Tools are namespaced as `mcp.<server>.<tool>` and go through the same
//! permission system as built-in tools.

pub mod client;
pub mod transport;
pub mod types;
