// Shared test helpers live one-per-file in this directory.
//
// Test targets include only the helpers they use via:
//
//     #[path = "common/<name>.rs"]
//     mod <name>_mod;
//     use <name>_mod::<name>;
//
// Nothing is declared here; there is no `mod common;` to
// avoid pulling every helper into every test binary.
