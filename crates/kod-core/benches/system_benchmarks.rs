//! System benchmarks for KOD.
//!
//! Benchmarks key operations across skills, memory, agents, and task routing.

mod common;

use common::BenchEnvironment;
use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use kod_core::router::{RouterConfig, TaskRouter};
use kod_memory::short_term::ShortTermMemory;
use kod_skills::{SkillLoader, SkillMatcher};
use kod_swarm::{AgentBuilder, Capability};
use kod_types::{MemoryEntry, MemoryType};
use time::OffsetDateTime;

fn benchmark_skill_loading(c: &mut Criterion) {
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
                    let runtime = tokio::runtime::Runtime::new().unwrap();
                    let skills = runtime.block_on(async { loader.load_all().await }).unwrap();

                    black_box(skills.len());
                    black_box(count);
                });
            },
        );
    }

    group.finish();
}

fn benchmark_skill_matching(c: &mut Criterion) {
    let mut group = c.benchmark_group("skill_matching");

    for skill_count in [10, 50, 100] {
        let env = BenchEnvironment::new();
        env.add_skills(skill_count);

        // Load skills once
        let mut loader = SkillLoader::new(&env.skills_dir);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let skills = runtime.block_on(async { loader.load_all().await }).unwrap();

        // Create matcher with skills
        let matcher = SkillMatcher::new();
        runtime.block_on(async {
            for skill in skills {
                matcher.add_skill(skill).await;
            }
        });

        // Benchmark matching
        group.bench_with_input(
            BenchmarkId::new("match_skills", skill_count),
            &skill_count,
            |b, &count| {
                b.iter(|| {
                    let matches = runtime
                        .block_on(async { matcher.find_relevant_skills("benchmark test").await });
                    black_box(matches.len());
                    black_box(count);
                });
            },
        );
    }

    group.finish();
}

fn benchmark_memory_operations(c: &mut Criterion) {
    let mut group = c.benchmark_group("memory_operations");

    // Short-term memory store
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
            };

            memory.store(entry);
            black_box(memory.len());
        });
    });

    // Short-term memory retrieval
    group.bench_function("short_term_retrieve", |b| {
        let memory = ShortTermMemory::new(1000);

        // Pre-populate
        for i in 0..100 {
            let entry = MemoryEntry {
                id: kod_types::MemoryId::new(),
                memory_type: MemoryType::ShortTerm,
                content: format!("Memory entry {}", i),
                timestamp: OffsetDateTime::now_utc(),
                relevance: 1.0,
                metadata: Default::default(),
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

fn benchmark_agent_operations(c: &mut Criterion) {
    let mut group = c.benchmark_group("agent_operations");

    // Agent creation
    group.bench_function("agent_creation", |b| {
        b.iter(|| {
            let agent = AgentBuilder::new("bench-agent")
                .with_capability(Capability::Coding)
                .with_capability(Capability::Testing)
                .build();

            black_box(agent.name());
        });
    });

    // Agent swarm creation
    group.bench_function("swarm_creation", |b| {
        b.iter(|| {
            let env = BenchEnvironment::new();
            let swarm = kod_swarm::swarm::AgentSwarm::new(env.working_dir.clone());
            let runtime = tokio::runtime::Runtime::new().unwrap();
            let _ = black_box(runtime.block_on(async { swarm.list_agents().await }));
        });
    });

    group.finish();
}

fn benchmark_task_classification(c: &mut Criterion) {
    let env = BenchEnvironment::new();
    let db_path = env.working_dir.join("bench.redb");

    let router = TaskRouter::new(
        RouterConfig {
            enable_swarm: false,
            enable_memory: false,
            max_skills_per_query: 3,
            working_dir: env.working_dir.clone(),
        },
        db_path,
    )
    .unwrap();

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
                    let runtime = tokio::runtime::Runtime::new().unwrap();
                    let task_type = runtime.block_on(async { router.classify_task(input).await });
                    let _ = black_box(task_type);
                });
            },
        );
    }

    group.finish();
}

fn benchmark_task_processing(c: &mut Criterion) {
    let env = BenchEnvironment::new();
    let db_path = env.working_dir.join("bench_process.redb");

    let router = TaskRouter::new(
        RouterConfig {
            enable_swarm: false,
            enable_memory: false,
            max_skills_per_query: 3,
            working_dir: env.working_dir.clone(),
        },
        db_path,
    )
    .unwrap();

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
                let runtime = tokio::runtime::Runtime::new().unwrap();
                let result = runtime.block_on(async { router.process_input(input).await });
                let _ = black_box(result);
            });
        });
    }

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
);

criterion_main!(benches);
