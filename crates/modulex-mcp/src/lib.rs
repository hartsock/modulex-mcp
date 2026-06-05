//! modulex-mcp — stdio MCP server over the modulex routine engine.
//!
//! Newline-delimited JSON-RPC 2.0, MCP protocol `2024-11-05`. The server is
//! deliberately small: five tools, serial dispatch, and one rule about
//! failure semantics:
//!
//! - **`isError: true` means the ENGINE faulted** (unknown routine, unknown
//!   tool, leash denial of the whole run).
//! - **Per-step failures are data**, not errors: they live inside the
//!   returned report, so an agent can read *which* step failed and why.
//!
//! The crate is a library (plus a thin bin) so the Python bindings can run
//! the same server loop with Python-registered step handlers.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod server;
pub mod tools;

pub use server::Server;
