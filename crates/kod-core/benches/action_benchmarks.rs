//! Benchmarks for per-action functions.
//!
//! `system_benchmarks.rs` covers engine-level operations: startup,
//! task classification, memory retrieval, skill matching, repo map
//! builds. This file covers the operations that run **per user
//! action** -- once per tool call, once per file walk, once per
//! streamed chunk:
//!
//!   - `kod_risk::assess`: the blast-radius gate before any shell
//!     command runs (one call per `execute_command`)
//!   - `ToolContext::is_path_allowed`: the path policy check before
//!     any file read or walk (one call per file in `grep` /
//!     `list_files` / `search_files`)
//!   - `Redactor::redact`: the secret detector (one call per read,
//!     per tool result with text, per streamed text chunk in
//!     redaction modes)
//!   - `binary_candidates`: which programs a shell command runs (one
//!     call per `execute_command`, input to the forbidden-binaries
//!     policy check)
//!
//! Read `path_policy` most carefully. Before `ea7c42f7`,
//! `is_path_allowed` compiled a fresh `globset::GlobSet` per
//! forbidden pattern on every call, and this group measured the
//! result: 243 us at one forbid, 1.72 ms at ten -- a walk over 1000
//! files against 10 forbids paid ~1.7 s in glob compilation alone.
//! `ea7c42f7` added a `OnceLock`-guarded per-pattern cache in
//! `kod-tools/src/context.rs`, and the same group now reads 172 ns
//! and 1.66 us. The bench's baseline is the *post-fix* number: a
//! regression to millisecond scale means the cache was removed or
//! short-circuited.

use criterion::{Criterion, black_box, criterion_group, criterion_main};

fn benchmark_risk_assess(c: &mut Criterion) {
    use kod_risk::{RiskContext, assess};
    let ctx = RiskContext {
        working_dir: std::path::PathBuf::from("/tmp/work"),
        home_dir: std::path::PathBuf::from("/Users/test"),
        scratch_dir: std::path::PathBuf::from("/tmp"),
    };
    let mut group = c.benchmark_group("risk_assess");
    for (name, cmd) in [
        ("catastrophic", "rm -rf /Users/test"),
        ("shell_compound", "echo hi && cd /tmp && ls"),
        ("git_push_force", "git push --force origin main"),
        ("trivial", "echo hello"),
        ("heredoc_unterminated", "cat <<EOF\nhello\n"),
    ] {
        group.bench_function(name, |b| {
            b.iter(|| {
                let a = assess(black_box(cmd), &ctx);
                black_box(a.level);
            });
        });
    }
    group.finish();
}

fn benchmark_path_policy(c: &mut Criterion) {
    use kod_tools::context::ToolContext;
    use kod_types::ToolPermissions;

    let empty = ToolContext::new("/tmp/work").with_permissions(ToolPermissions {
        read_files: true,
        ..Default::default()
    });
    let one = ToolContext::new("/tmp/work").with_permissions(ToolPermissions {
        read_files: true,
        forbidden_paths: vec!["**/secrets/**".to_string()],
        ..Default::default()
    });
    let ten = ToolContext::new("/tmp/work").with_permissions(ToolPermissions {
        read_files: true,
        forbidden_paths: (0..10).map(|i| format!("**/dir{i}/**")).collect(),
        ..Default::default()
    });

    let matching = std::path::PathBuf::from("/tmp/work/secrets/creds.txt");
    let non_matching = std::path::PathBuf::from("/tmp/work/src/main.rs");

    let mut group = c.benchmark_group("path_policy");
    group.bench_function("empty_non_matching", |b| {
        b.iter(|| {
            let _ = empty.is_path_allowed(black_box(&non_matching));
        });
    });
    group.bench_function("one_forbidden_non_matching", |b| {
        b.iter(|| {
            let _ = one.is_path_allowed(black_box(&non_matching));
        });
    });
    group.bench_function("one_forbidden_matching", |b| {
        b.iter(|| {
            let _ = one.is_path_allowed(black_box(&matching));
        });
    });
    group.bench_function("ten_forbidden_non_matching", |b| {
        b.iter(|| {
            let _ = ten.is_path_allowed(black_box(&non_matching));
        });
    });
    group.finish();
}

fn benchmark_redact(c: &mut Criterion) {
    use kod_types::redact::Redactor;
    let r = Redactor::default();
    let mut group = c.benchmark_group("redact");
    // The first three fixtures are ~50 bytes; a `read_file` tool
    // result is 10 KB and up. `redact` runs once per tool result
    // before the result enters the prompt, so the large-input shape
    // is the one that decides whether the per-rule scan is a
    // per-turn cost or a rounding error. `code_10k` is a realistic
    // Rust-ish source line repeated to 10 KB with no secret in it;
    // `code_10k_secret_at_end` is the same with one OpenAI key on
    // the last line.
    let small_clean = "This is ordinary prose with no secrets at all.";
    let code_line = "    let value = compute(input).unwrap_or_else(|e| handle(e)); // note\n";
    let mut code_10k = String::with_capacity(10_240);
    while code_10k.len() < 10_240 {
        code_10k.push_str(code_line);
    }
    let mut code_10k_secret = code_10k.clone();
    code_10k_secret.push_str("// key = sk-abcdef0123456789abcdef0123456789\n");
    for (name, text) in [
        ("clean", small_clean),
        (
            "one_secret",
            "api_key = sk-abcdef0123456789abcdef0123456789",
        ),
        (
            "many_secrets",
            "sk-abcdef0123456789abcdef0123456789\nghp_abcdefghijklmnopqrstuvwxyz0123456789\nAKIAIOSFODNN7EXAMPLE",
        ),
        ("code_10k_clean", code_10k.as_str()),
        ("code_10k_secret", code_10k_secret.as_str()),
    ] {
        group.bench_function(name, |b| {
            b.iter(|| {
                let (out, _) = r.redact(black_box(text));
                black_box(out.len());
            });
        });
    }
    group.finish();
}

fn benchmark_redactor_construction(c: &mut Criterion) {
    // `Redactor::default()` compiles the 13 built-in regexes. Before
    // the `OnceLock` hoist in `with_builtin_rules`, every
    // `KodError::provider_status_with_hint` call paid that cost to
    // strip secrets from an HTTP error body before it reached
    // `Display`; on a retry loop the cost was additive. The bench is
    // the durable record of what a fresh construction costs -- it is
    // NOT on the per-token or per-tool-call path (`redact` and
    // `redact_json` reuse an existing `Redactor`), so this number is
    // a floor, not a per-turn cost.
    let mut group = c.benchmark_group("redactor_construction");
    group.bench_function("default", |b| {
        b.iter(|| {
            let r = kod_types::redact::Redactor::default();
            black_box(r.rule_count());
        });
    });
    group.bench_function("default_clone", |b| {
        let r = kod_types::redact::Redactor::default();
        b.iter(|| {
            let c2 = black_box(&r).clone();
            black_box(c2.rule_count());
        });
    });
    // The "before" number: `builtin_rules()` is what a fresh
    // `with_builtin_rules` used to call. It compiles 13 regexes, one
    // per rule. Kept as a separate fixture so a reader sees both
    // sides -- `default` is now the `OnceLock` hit, this is what it
    // would cost if the hoist were reverted.
    group.bench_function("builtin_rules_unshared", |b| {
        b.iter(|| {
            let rules = kod_types::redact::builtin_rules();
            black_box(rules.len());
        });
    });
    group.finish();
}

fn benchmark_binary_candidates(c: &mut Criterion) {
    let mut group = c.benchmark_group("binary_candidates");
    for (name, cmd) in [
        ("simple", "rm x"),
        ("compound", "cd /tmp && cargo build && ls"),
        ("wrapped", "bash -c 'rm x'"),
        ("env_prefixed", "FOO=bar rm x"),
        ("long_chain", "cd a && cargo build && cargo test && cargo clippy && git diff"),
    ] {
        group.bench_function(name, |b| {
            b.iter(|| {
                let v = kod_config::policy::binary_candidates_for_tests(black_box(cmd));
                black_box(v.len());
            });
        });
    }
    group.finish();
}

criterion_group!(
    action_benches,
    benchmark_risk_assess,
    benchmark_path_policy,
    benchmark_redact,
    benchmark_redactor_construction,
    benchmark_binary_candidates,
);
criterion_main!(action_benches);
