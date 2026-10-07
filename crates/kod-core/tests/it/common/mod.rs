// Shared test helpers, declared once as submodules of this module.
//
// The previous shape `#[path = "common/<name>.rs"] mod <name>_mod;`
// was correct when each helper lived in its own top-level test binary.
// After consolidation into one `it` binary, the same file was loaded
// as a module up to seven times — harmless at the type level, but
// clippy-flagged and a maintenance trap.
//
// Now each helper is a normal submodule. Consumers `use
// crate::common::<name>::<fn>;`.

pub mod install_named_provider;
pub mod install_test_provider;
