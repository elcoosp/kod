# kod-risk

Command blast-radius classification for KOD.

Classifies shell commands by blast radius: read-only, additive,
destructive, or unknown. Combined with the user's configured approval
preset, this decides whether a command is auto-approved, prompted, or
refused before the engine dispatches it.

## Usage

```rust
use kod_risk::{assess, RiskContext, RiskLevel};
use std::path::PathBuf;

let ctx = RiskContext {
    working_dir: PathBuf::from("/work/proj"),
    home_dir: PathBuf::from("/Users/dev"),
    scratch_dir: PathBuf::from("/tmp"),
};

// A catastrophic command is refused outright.
assert_eq!(assess("rm -rf /", &ctx).level, RiskLevel::Catastrophic);

// A safe command runs without a prompt.
assert!(assess("ls -la", &ctx).level.runs_immediately());
```

## Part of KOD

Reached transitively through `kod-core-tools`.

See the [KOD workspace](https://github.com/elcoosp/kod).

## License

MIT
