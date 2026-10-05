//! kod-core-serve: Unix-socket daemon and ACP bridge.
//!
//! Extracted from kod-core. Both modules drive a `KodEngine` from an
//! external client — `serve` over a Unix socket with NDJSON framing,
//! `acp` over stdio with LSP-style Content-Length framing.
//!
//! Depends on kod-core for the engine and swarm runner; nothing in
//! kod-core depends back except through this crate's own re-exports
//! (there are none — the CLI imports from here directly).

pub mod acp;
pub mod serve;
