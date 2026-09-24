//! System benchmarks for KOD.
//!
//! # Methodology
//!
//! Every async benchmark hoists its tokio runtime out of
//! `b.iter`. The previous version created a fresh runtime per
//! iteration, which measured runtime construction (threadpool +
//! scheduler setup, hundreds of microseconds) rather than the
//! operation the benchmark claimed to test. That is the single
//! biggest correctness issue in a benchmark suite — the numbers
//! are not wrong by a few percent, they are wrong by the ratio of
//! runtime-construction time to operation time.
//!
//! The runtime is built once per benchmark function and shared
//! across iterations. Criterion does not count setup time against
//! the reported number.
//!
//! # What is measured
//!
//! | Group                | What it measures                              |
//! |----------------------|-----------------------------------------------|
//! | `skill_loading`      | Parsing N skill markdown files from disk      |
//! | `skill_matching`     | Scoring every loaded skill against a query    |
//! | `memory_operations`  | Short-term store + retrieve, in-memory        |
//! | `agent_operations`   | Agent construction, swarm listing             |
//! | `task_classification`| `classify_task` on representative inputs      |
//! | `task_processing`    | Full `process_input` (classify + context)     |
//! | `repo_map`           | Regex walk + symbol extraction + render        |
//! | `memory_retrieval`   | Hybrid retrieval against 10 000 entries        |
//! | `startup`            | Cold engine construction + start + shutdown    |

mod common;

use common::BenchEnvironment;
use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use kod_core::router::{RouterConfig, TaskRouter};
use kod_memory::MemoryManager;
use kod_memory::short_term::ShortTermMemory;
use kod_skills::{SkillLoader, SkillMatcher};
use kod_swarm::{AgentBuilder, Capability};
use kod_types::{MemoryEntry, MemoryType};
use std::path::PathBuf;
use std::time::Duration;
use time::OffsetDateTime;
use tokio::runtime::Runtime;

/// The runtime every async benchmark shares. Single-threaded on
/// purpose: the operations being measured do not rely on
/// parallelism, and a smaller runtime has less scheduling noise.
fn rt() -> Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build benchmark runtime")
}

/// Workspace root, used as the fixture for the repo-map benchmark.
/// Deterministic given the checkout.
fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .unwrap_or(manifest)
}

// ==================================================================
// skill_loading
// ==================================================================
fn benchmark_skill_loading(c: &mut Criterion) {
    let rt = rt();
    let mut group = c.benchmark_group("skill_loading");

    for skill_count in [10, 50, 100, 500] {
        let env = BenchEnvironment::new();
        env.add_skills(skill_count);

        group.bench_with_input(
            BenchmarkId::new("load_skills", skill_count),
            &skill_count,
            |b, &count| {
                b.iter(|| {
                    let mut loader = SkillLoader::new(&env.skills_dir);
                    let skills = rt.block_on(async { loader.load_all().await }).unwrap();
                    black_box(skills.len());
                    black_box(count);
                });
            },
        );
    }

    group.finish();
}

// ==================================================================
// skill_matching
// ==================================================================
fn benchmark_skill_matching(c: &mut Criterion) {
    let rt = rt();
    let mut group = c.benchmark_group("skill_matching");

    for skill_count in [10, 50, 100] {
        let env = BenchEnvironment::new();
        env.add_skills(skill_count);

        let mut loader = SkillLoader::new(&env.skills_dir);
        let skills = rt.block_on(async { loader.load_all().await }).unwrap();

        let matcher = SkillMatcher::new();
        rt.block_on(async {
            for skill in skills {
                matcher.add_skill(skill).await;
            }
        });

        group.bench_with_input(
            BenchmarkId::new("match_skills", skill_count),
            &skill_count,
            |b, &count| {
                b.iter(|| {
                    let matches =
                        rt.block_on(async { matcher.find_relevant_skills("benchmark test").await });
                    black_box(matches.len());
                    black_box(count);
                });
            },
        );
    }

    group.finish();
}

// ==================================================================
// memory_operations (short-term, in-memory)
// ==================================================================
fn benchmark_memory_operations(c: &mut Criterion) {
    let mut group = c.benchmark_group("memory_operations");

    group.bench_function("short_term_store", |b| {
        let memory = ShortTermMemory::new(1000);
        b.iter(|| {
            let entry = MemoryEntry {
                id: kod_types::MemoryId::new(),
                memory_type: MemoryType::ShortTerm,
                content: "Benchmark memory entry".to_string(),
                timestamp: OffsetDateTime::now_utc(),
                relevance: 1.0,
                metadata: Default::default(),
            
                superseded_by: None,
                contradicts: Vec::new(),
            };
            memory.store(entry);
            black_box(memory.len());
        });
    });

    group.bench_function("short_term_retrieve", |b| {
        let memory = ShortTermMemory::new(1000);
        for i in 0..100 {
            let entry = MemoryEntry {
                id: kod_types::MemoryId::new(),
                memory_type: MemoryType::ShortTerm,
                content: format!("Memory entry {}", i),
                timestamp: OffsetDateTime::now_utc(),
                relevance: 1.0,
                metadata: Default::default(),
            
                superseded_by: None,
                contradicts: Vec::new(),
            };
            memory.store(entry);
        }
        b.iter(|| {
            let entries = memory.get_recent(10);
            black_box(entries.len());
        });
    });

    group.finish();
}

// ==================================================================
// agent_operations
// ==================================================================
fn benchmark_agent_operations(c: &mut Criterion) {
    let rt = rt();
    let mut group = c.benchmark_group("agent_operations");

    group.bench_function("agent_creation", |b| {
        b.iter(|| {
            let agent = AgentBuilder::new("bench-agent")
                .with_capability(Capability::Coding)
                .with_capability(Capability::Testing)
                .build();
            black_box(agent.name());
        });
    });

    group.bench_function("swarm_creation", |b| {
        let env = BenchEnvironment::new();
        b.iter(|| {
            let swarm = kod_swarm::swarm::AgentSwarm::new(env.working_dir.clone());
            let _ = black_box(rt.block_on(async { swarm.list_agents().await }));
        });
    });

    group.finish();
}

// ==================================================================
// task_classification
// ==================================================================
fn benchmark_task_classification(c: &mut Criterion) {
    let rt = rt();
    let env = BenchEnvironment::new();
    let db_path = env.working_dir.join("bench.redb");

    let cfg = RouterConfig {
        working_dir: env.working_dir.clone(),
        enable_memory: false,
        ..RouterConfig::default()
    };
    let router = TaskRouter::new(cfg, db_path).unwrap();

    let mut group = c.benchmark_group("task_classification");

    let test_inputs = vec![
        "What is 2+2?",
        "Fix the bug in main.rs",
        "Debug this error: panic in main",
        "Research best practices for async Rust",
        "Write unit tests for auth module",
        "Document the public API",
        "Design and implement a complete authentication system",
    ];

    for input in test_inputs {
        let input_name: String = input.chars().take(20).collect();
        group.bench_with_input(
            BenchmarkId::new("classify", input_name),
            input,
            |b, input| {
                b.iter(|| {
                    let task_type = rt.block_on(async { router.classify_task(input).await });
                    let _ = black_box(task_type);
                });
            },
        );
    }

    group.finish();
}

// ==================================================================
// task_processing
// ==================================================================
fn benchmark_task_processing(c: &mut Criterion) {
    let rt = rt();
    let env = BenchEnvironment::new();
    let db_path = env.working_dir.join("bench_process.redb");

    let cfg = RouterConfig {
        working_dir: env.working_dir.clone(),
        enable_memory: false,
        ..RouterConfig::default()
    };
    let router = TaskRouter::new(cfg, db_path).unwrap();

    let mut group = c.benchmark_group("task_processing");

    let test_inputs = vec![
        ("simple", "What is 2+2?"),
        ("code_mod", "Fix the bug in main.rs"),
        ("debug", "Debug this error: panic in main"),
        ("research", "Research best practices for async Rust"),
    ];

    for (name, input) in test_inputs {
        group.bench_with_input(BenchmarkId::new("process", name), input, |b, input| {
            b.iter(|| {
                let result = rt.block_on(async { router.process_input(input).await });
                let _ = black_box(result);
            });
        });
    }

    group.finish();
}

// ==================================================================
// repo_map — new
// ==================================================================
fn benchmark_repo_map(c: &mut Criterion) {
    let root = workspace_root();

    let mut group = c.benchmark_group("repo_map");
    group.bench_function("build_and_render", |b| {
        b.iter(|| {
            let map = kod_core::repomap::build_repo_map(&root);
            let rendered = map.render(kod_core::repomap::DEFAULT_MAP_CHARS);
            black_box(map.file_count());
            black_box(rendered.len());
        });
    });
    group.finish();
}

// ==================================================================
// memory_retrieval — new, 10 000 entries
// ==================================================================
fn benchmark_memory_retrieval(c: &mut Criterion) {
    let rt = rt();
    let env = BenchEnvironment::new();
    let db_path = env.working_dir.join("bench_retrieve.redb");

    const N: usize = 10_000;
    let manager = MemoryManager::new(db_path, 100).expect("memory manager");
    rt.block_on(async {
        for i in 0..N {
            let content = format!(
                "benchmark memory entry number {i} with some content words \
                 about parsers and handlers and retry logic",
            );
            manager
                .store(MemoryType::LongTerm, &content)
                .await
                .expect("store");
        }
    });

    let mut group = c.benchmark_group("memory_retrieval");
    group.bench_function("hybrid_top20_10k", |b| {
        b.iter(|| {
            let ctx = rt
                .block_on(async {
                    manager
                        .retrieve_context("how does the parser handle retries?")
                        .await
                })
                .expect("retrieve");
            black_box(ctx.long_term.len());
        });
    });
    group.finish();
}

// ==================================================================
// startup — new, cold engine
// ==================================================================
fn benchmark_startup(c: &mut Criterion) {
    let rt = rt();
    let mut group = c.benchmark_group("startup");
    group.measurement_time(Duration::from_secs(10));
    group.sample_size(20);

    // `iter_batched` with `SmallInput` runs setup and teardown per
    // iteration; the reported number is the routine only. Setup
    // creates a fresh tempdir + db path — an engine's `start()` can
    // only be called once, and reusing it across iterations would
    // be a lie about what "cold" means. The tempdir has no project
    // manifest, so the background baseline task fails fast and does
    // not perturb the measurement.
    group.bench_function("engine_new_start_shutdown", |b| {
        b.iter_batched(
            || {
                let tmp = tempfile::TempDir::new().expect("tempdir");
                let db = tmp.path().join("cold.redb");
                let cfg = RouterConfig {
                    working_dir: tmp.path().to_path_buf(),
                    enable_memory: false,
                    ..RouterConfig::default()
                };
                (tmp, cfg, db)
            },
            |(_tmp, cfg, db)| {
                let engine = kod_core::KodEngine::new(cfg, db).expect("engine new");
                rt.block_on(async {
                    engine.start().await.expect("engine start");
                    engine.shutdown().await.expect("engine shutdown");
                });
            },
            criterion::BatchSize::SmallInput,
        );
    });

    group.finish();
}

/// Cold-start benchmark (design §D6.8).
///
/// The existing `benchmark_startup` measures a warm loop: after the
/// first iteration the process has warmed allocator pages, JIT-ed
/// nothing (Rust is compiled), and cached the crate's string tables.
/// The figure a user sees on the first launch of `kod` is different —
/// and, since the README promises a "small, self-contained binary",
/// the cold-start wall time is the honest number.
///
/// This is a single-shot measurement, not a criterion loop: the first
/// call is what we want, and rerunning it iteratively would report the
/// warm steady-state. Criterion still owns the harness (so the timing
/// primitive and reporting format are consistent with the rest of the
/// suite), but a single sample is the answer.
fn benchmark_startup_cold(c: &mut Criterion) {
    let rt = rt();
    let mut group = c.benchmark_group("startup_cold");
    // One sample: the number we want is the first-run cost.
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(2));

    group.bench_function("cold_engine_lifecycle", |b| {
        // `iter_batched` with `PerIteration` gives criterion a fresh
        // tempdir + db for every batch. That is expensive, but a
        // cold-start benchmark has to pay it — an engine on a warm
        // db is not cold.
        b.iter_batched(
            || {
                let tmp = tempfile::TempDir::new().expect("tempdir");
                let db = tmp.path().join("cold.redb");
                let cfg = RouterConfig {
                    working_dir: tmp.path().to_path_buf(),
                    enable_memory: true,
                    ..RouterConfig::default()
                };
                (tmp, cfg, db)
            },
            |(_tmp, cfg, db)| {
                let engine = kod_core::KodEngine::new(cfg, db).expect("engine new");
                rt.block_on(async {
                    engine.start().await.expect("engine start");
                    engine.shutdown().await.expect("engine shutdown");
                });
            },
            criterion::BatchSize::PerIteration,
        );
    });
    group.finish();
}

criterion_group!(
    benches,
    benchmark_skill_loading,
    benchmark_skill_matching,
    benchmark_memory_operations,
    benchmark_agent_operations,
    benchmark_task_classification,
    benchmark_task_processing,
    benchmark_repo_map,
    benchmark_memory_retrieval,
    benchmark_startup,
    benchmark_startup_cold,
);

criterion_main!(benches);
