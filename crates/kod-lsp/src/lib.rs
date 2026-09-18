//! Minimal Language Server Protocol client for KOD.
//!
//! # Why this exists
//!
//! `cargo check` and its peers answer "did the whole project compile?"
//! The editor experience — squiggly lines under the exact wrong token,
//! updated as you type — comes from a language server. A coding agent
//! that never sees those signals is working with coarser feedback than
//! the human sitting next to it.
//!
//! This crate gives the harness the same channel. It is deliberately
//! minimal:
//!
//! - One server per `LspClient`. Lifetime = the client.
//! - One method of interest: `textDocument/publishDiagnostics`,
//!   collected by [`LspClient::collect_diagnostics`].
//! - Every other server-to-client request is answered
//!   `{"result": null}` so the server does not block waiting for
//!   capabilities we do not implement.
//!
//! Adding completion, hover, or go-to-definition is a matter of
//! sending the corresponding request and parsing the response; the
//! transport and handshake below already do the hard part.

pub mod client;
pub mod manager;
pub mod types;

pub use client::{LspClient, LspError};
pub use manager::{LspManager, binary_for_path};
pub use types::{
    Diagnostic, Hover, Location, Position, Range, language_id_for,
};
