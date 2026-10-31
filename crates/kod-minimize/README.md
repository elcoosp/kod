# kod-minimize

Shell output minimizer for KOD.

Compresses long shell output before it enters the conversation
context: drops boilerplate, keeps the head and tail of long streams,
and preserves error lines. Reduces token cost without hiding failures
the model needs to see.

## Usage

```rust
use kod_minimize::Minimizer;

let m = Minimizer::with_builtins();
let raw = "On branch main\n\tmodified:   src/lib.rs\n";
let r = m.minimize("git status", raw, 0);
assert_eq!(r.filter.as_deref(), Some("git-status"));
assert!(r.text.contains("On branch main"));
```

A `Minimizer` holds the built-in defs (git status, git log, cargo
check, cargo test, pytest) plus any registered by the caller.
`minimize` classifies the command, looks up the matching def, and runs
its pipeline; unrecognized commands — including pipes and `&&` chains,
where the model would be guessing at the shell's output — pass through
raw. The result carries `filter` (`None` on pass-through) and both
sizes so a caller can decide whether to keep the rewrite.

## Part of KOD

Reached transitively through `kod-tools` and `kod-core-tools`.

See the [KOD workspace](https://github.com/elcoosp/kod).

## License

MIT
