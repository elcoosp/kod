//! `kod fixture` subcommands — golden-transcript capture and replay
//! for prompt regressions.

use super::*;

/// Replay a saved fixture against the current engine, printing any
/// divergence in the request shape (Tier 1.5). Returns the number of
/// divergent rounds; zero means a clean replay.
pub async fn run_fixture_replay(name: &str, strict: bool, first_round_only: bool) -> Result<i32> {
    use kod_provider::replay::{ReplayProvider, ReplayRound, ReplayToolCall};

    let path = kod_core::Fixture::default_path(name)
        .ok_or_else(|| KodError::Config("could not determine fixtures directory".to_string()))?;
    let fixture = kod_core::Fixture::load_from(&path)
        .map_err(|e| KodError::Config(format!("could not load fixture {}: {e}", path.display())))?;
    eprintln!(
        "Replaying fixture {} ({} rounds, created at {})",
        fixture.name,
        fixture.rounds.len(),
        fixture.created_at_ms,
    );

    // Build the replay provider from the fixture.
    let rounds: Vec<ReplayRound> = fixture
        .rounds
        .iter()
        .map(|r| ReplayRound {
            text: r.response.text.clone(),
            tool_calls: r
                .response
                .tool_calls
                .iter()
                .map(|c| ReplayToolCall {
                    id: c.id.clone(),
                    name: c.name.clone(),
                    arguments: c.arguments.clone(),
                })
                .collect(),
            usage: r.response.usage.as_ref().map(|u| kod_provider::TokenUsage {
                prompt_tokens: u.prompt_tokens,
                completion_tokens: u.completion_tokens,
                total_tokens: u.prompt_tokens + u.completion_tokens,
            }),
        })
        .collect();
    // Two handles to the same provider: the concrete one for
    // `.captured()`, and an `Arc<dyn LlmProvider>` to install on the
    // registry.
    let replay_concrete = std::sync::Arc::new(ReplayProvider::new(rounds));
    let replay: std::sync::Arc<dyn kod_provider::LlmProvider> = replay_concrete.clone();

    // Build a fresh engine with the replay provider installed as the
    // only endpoint. We assemble the registry directly with
    // `ProviderRegistry::insert` — the in-crate `install_test_provider`
    // shim is `pub(crate)` and not reachable from kod-cli.
    let tmp_root = std::env::temp_dir().join(format!(
        "kod-fixture-replay-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    std::fs::create_dir_all(&tmp_root).map_err(KodError::Io)?;
    let cfg = kod_core::RouterConfig {
        working_dir: std::env::current_dir()
            .map_err(|e| KodError::Internal(format!("cwd: {e}")))?,
        enable_memory: false,
        ..kod_core::RouterConfig::default()
    };
    let engine = kod_core::KodEngine::new(cfg, tmp_root.join("replay.redb"))
        .map_err(|e| KodError::Internal(format!("engine: {e}")))?;
    let mut registry = kod_provider::ProviderRegistry::new();
    registry.insert(
        "replay",
        replay.clone(),
        kod_provider::ProviderCapabilities {
            tools: true,
            streaming_tools: true,
            ..kod_provider::ProviderCapabilities::conservative()
        },
        "replay-fixture",
    );
    engine
        .set_registry(
            std::sync::Arc::new(registry),
            kod_provider::ModelRef::new("replay", "replay-fixture"),
            None,
        )
        .await;
    engine
        .start()
        .await
        .map_err(|e| KodError::Internal(format!("start: {e}")))?;

    // Drive each round's user prompt through the engine. The
    // ReplayProvider answers with the recorded response, and captures
    // the request the engine built.
    let mut divergent = 0_i32;
    let rounds_to_run = if first_round_only {
        fixture
            .rounds
            .iter()
            .position(|r| !r.user_prompt.is_empty())
            .map(|i| i + 1)
            .unwrap_or(fixture.rounds.len())
    } else {
        fixture.rounds.len()
    };
    for (idx, round) in fixture.rounds.iter().take(rounds_to_run).enumerate() {
        if round.user_prompt.is_empty() {
            continue;
        }
        if let Err(e) = engine.process_for("session", &round.user_prompt).await {
            eprintln!("  round {idx}: engine error: {e}");
            divergent += 1;
            continue;
        }
    }
    let _ = engine.shutdown().await;
    // Best-effort cleanup of the scratch directory.
    let _ = std::fs::remove_dir_all(&tmp_root);

    // Compare the captured requests to the fixture's summaries.
    let captured = replay_concrete.captured();
    let cmp_len = captured.len().min(fixture.rounds.len());
    for i in 0..cmp_len {
        let want = &fixture.rounds[i].request_summary;
        let got = kod_core::RequestSummary::from_request(&captured[i]);
        if want.hash() != got.hash() {
            divergent += 1;
            eprintln!();
            eprintln!(
                "✗ round {i} request mismatch (expected hash {}, got {})",
                &fixture.rounds[i].request_hash[..8.min(fixture.rounds[i].request_hash.len())],
                &got.hash()[..8],
            );
            let diff = kod_core::diff_rounds(
                &fixture.rounds[i],
                &kod_core::RoundFixture {
                    seq: i as u32,
                    user_prompt: fixture.rounds[i].user_prompt.clone(),
                    request_hash: got.hash(),
                    request_summary: got.clone(),
                    response: kod_core::ResponseFixture {
                        text: String::new(),
                        tool_calls: Vec::new(),
                        usage: None,

                        tool_results: Vec::new(),
                    },
                    at_ms: 0,
                },
            );
            eprint!("{diff}");
        }
    }

    // Round-count mismatch is a soft divergence unless strict. When
    // `--first-round-only` is set, we only ran one round; comparing
    // counts is not meaningful.
    if !first_round_only && captured.len() != fixture.rounds.len() {
        let msg = format!(
            "round count: fixture has {}, replay produced {}",
            fixture.rounds.len(),
            captured.len(),
        );
        if strict {
            eprintln!("✗ {msg} (strict)");
            divergent += 1;
        } else {
            eprintln!("⚠ {msg}");
        }
    }

    if divergent == 0 {
        eprintln!("✓ replay clean: {} round(s) matched", cmp_len,);
        Ok(0)
    } else {
        eprintln!("✗ replay diverged on {} round(s)", divergent);
        Ok(1)
    }
}

/// Tier 1.5 — save turn traces as a fixture, taking the turns path
/// explicitly. Prefers `<home>/.kod/sessions/turns.jsonl` when
/// `turns` is `None`.
///
/// Refuses to overwrite an existing fixture unless `force` is set.
pub async fn run_fixture_save_v2(
    name: &str,
    turns: Option<std::path::PathBuf>,
    force: bool,
) -> Result<()> {
    let turns_path = match turns {
        Some(p) => p,
        None => {
            let home = dirs::home_dir()
                .ok_or_else(|| KodError::Config("no home directory".to_string()))?;
            home.join(".kod").join("sessions").join("turns.jsonl")
        }
    };
    if !turns_path.exists() {
        return Err(KodError::Config(format!(
            "no turn traces at {} — run a session with \
             KOD_SESSION_LOG unset first, or pass --turns <path>",
            turns_path.display(),
        )));
    }
    let dest = kod_core::Fixture::default_path(name)
        .ok_or_else(|| KodError::Config("could not determine fixtures directory".to_string()))?;
    if dest.exists() && !force {
        return Err(KodError::Config(format!(
            "fixture {} already exists; pass --force to overwrite",
            dest.display(),
        )));
    }
    run_fixture_save(name, &turns_path).await
}

/// Tier 1.5 — list the fixtures under `~/.kod/fixtures/`.
pub async fn run_fixture_list() -> Result<()> {
    let Some(dir) = kod_core::Fixture::fixtures_dir() else {
        return Err(KodError::Config("no home directory".to_string()));
    };
    if !dir.exists() {
        eprintln!("no fixtures directory at {}", dir.display());
        return Ok(());
    }
    let mut rows: Vec<(String, u64, usize)> = Vec::new();
    for entry in std::fs::read_dir(&dir).map_err(KodError::Io)? {
        let entry = entry.map_err(KodError::Io)?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("(unknown)")
            .to_string();
        match kod_core::Fixture::load_from(&path) {
            Ok(f) => rows.push((name, f.created_at_ms, f.rounds.len())),
            Err(e) => {
                eprintln!("WARN: could not load {}: {e}", path.display());
                rows.push((name, 0, 0));
            }
        }
    }
    if rows.is_empty() {
        eprintln!("no fixtures in {}", dir.display());
        return Ok(());
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    println!("Fixtures in {}", dir.display());
    for (name, ms, n) in &rows {
        let when = if *ms > 0 {
            format!("{ms}")
        } else {
            "(unreadable)".to_string()
        };
        println!("  {name:<32}  rounds={n:<5}  created_at_ms={when}");
    }
    Ok(())
}

/// Save the current session's turns as a fixture.
pub async fn run_fixture_save(name: &str, turns_path: &std::path::Path) -> Result<()> {
    let traces = kod_core::read_traces(turns_path)
        .map_err(|e| KodError::Config(format!("could not read turn traces: {e}")))?;
    if traces.is_empty() {
        return Err(KodError::Config(
            "no turn traces recorded; set KOD_SESSION_LOG and run a session first".to_string(),
        ));
    }
    let mut fixture = kod_core::Fixture::new(name);
    for (i, t) in traces.iter().enumerate() {
        let summary = kod_core::RequestSummary {
            system_chars: t.prompt_chars,
            message_count: t.rounds.first().map(|_| 1).unwrap_or(0),
            tool_names: Vec::new(),
            model: "unknown".to_string(),
            endpoint: "unknown".to_string(),
        };
        let hash = summary.hash();
        // Tier 1.5 — copy every tool call and result from the
        // trace's rounds into the fixture. A single trace turn can
        // produce many rounds; we flatten them into one
        // `ResponseFixture` for that turn, which is what replay
        // needs (the engine sees a turn as one round-trip).
        let mut tool_calls = Vec::new();
        let mut tool_results = Vec::new();
        for r in &t.rounds {
            for c in &r.tool_calls {
                tool_calls.push(kod_core::fixture::ToolCallFixture {
                    id: None,
                    name: c.name.clone(),
                    // Tier 1.5 — the args now flow through the
                    // trace verbatim, so a fixture can drive the
                    // tool round trip on replay.
                    arguments: c.arguments.clone(),
                });
                tool_results.push(kod_core::ToolResultFixture {
                    tool_name: c.name.clone(),
                    is_error: matches!(c.outcome, kod_core::trace::ToolOutcomeKind::Error),
                    value: serde_json::json!({
                        "duration_ms": c.duration_ms,
                        "output_bytes": c.output_bytes,
                        "elided_lines": c.elided_lines,
                        "summary": c.result_summary,
                    }),
                });
            }
        }
        fixture.rounds.push(kod_core::RoundFixture {
            seq: i as u32,
            user_prompt: t.user_prompt.clone(),
            request_hash: hash,
            request_summary: summary,
            response: kod_core::ResponseFixture {
                text: String::new(),
                tool_calls,
                usage: None,
                tool_results,
            },
            at_ms: t.ended_at_ms,
        });
    }
    let path = kod_core::Fixture::default_path(name)
        .ok_or_else(|| KodError::Config("could not determine fixtures directory".to_string()))?;
    fixture.save_to(&path).map_err(KodError::Io)?;
    eprintln!(
        "Wrote fixture {} ({} rounds)",
        path.display(),
        fixture.rounds.len()
    );
    Ok(())
}
