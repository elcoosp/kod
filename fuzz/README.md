# Fuzz targets

One target per parser named in `docs/FUZZING.md`. The parsers are
pure functions on bytes/slices and this directory is where they get
fuzzed.

```sh
cargo install cargo-fuzz
cargo fuzz list
cargo fuzz run patch
