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
//! Read `path_policy` most carefully: `is_path_allowed` compiles a
//! fresh `globset::Glob` per pattern on every call (see
//! `kod-tools/src/context.rs`), so a walk over 1000 files against 10
//! forbids pays 10,000 glob compiles. If that group is
//! microsecond-scale rather than nanosecond-scale, a compiled-globset
//! cache is a real optimization. If it's already fast, the number
//! documents "do not touch."

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
    for (name, text) in [
        ("clean", "This is ordinary prose with no secrets at all."),
        (
            "one_secret",
            "api_key = sk-abcdef0123456789abcdef0123456789",
        ),
        (
            "many_secrets",
            "sk-abcdef0123456789abcdef0123456789\nghp_abcdefghijklmnopqrstuvwxyz0123456789\nAKIAIOSFODNN7EXAMPLE",
        ),
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
    benchmark_binary_candidates,
);
criterion_main!(action_benches);
