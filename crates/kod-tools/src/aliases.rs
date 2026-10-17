//! Tool-name aliases — re-exported from `kod-types`.
//!
//! The table moved to `kod-types` so `kod-provider` (which does not
//! depend on `kod-tools`) can canonicalize replayed tool names the
//! same way the live engine does. This module keeps the `kod_tools::
//! aliases::…` path working for existing callers.

pub use kod_types::aliases::resolve_tool_name;
