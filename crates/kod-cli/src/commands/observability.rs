//! Inspection subcommands: `kod jev`, `kod budget`, `kod limits`,
//! `kod plan`, `kod decisions`, `kod trace`.
//!
//! These read state that the engine already produces (turn traces,
//! budget allocations, decision logs) and print it. None of them
//! drive a live turn except `kod trace replay`, which builds a fresh
//! engine and re-runs one prompt.

use super::*;

/// Find the newest `.jsonl` under `~/.kod/sessions/`.
/// `kod jev status` — print the effective Jev config.
/// `kod jev stats` — read JevDecision entries from a session log.
/// `kod jev test` — ping the endpoint. Exits non-zero on failure.
pub async fn run_jev_test() -> Result<()> {
    let cfg = KodConfig::load_default()?;
    let client = kod_core::JevClient::from_config(&cfg.jev)
        .map_err(|e| KodError::Config(format!("Jev config: {e}")))?
        .ok_or_else(|| {
            KodError::Config("Jev is disabled in config; enable [jev] first".to_string())
        })?;
    let state = kod_core::jev::build_state("The sky is blue on a clear day.", &[]);
    let started = std::time::Instant::now();
    match client
        .evaluate_yes_no(
            &state,
            "Is the sky described here as blue? Answer yes or no.",
        )
        .await
    {
        Ok(d) => {
            let ms = started.elapsed().as_millis();
            println!(
                "✓ Jev responded in {ms}ms: value={} confidence={:.2}",
                d.value, d.confidence
            );
            Ok(())
        }
        Err(e) => {
            eprintln!("✗ Jev call failed: {e}");
            Err(KodError::InvalidState(format!("jev test: {e}")))
        }
    }
}

/// `kod jev tune` — print thresholds.
pub async fn run_jev_tune_show() -> Result<()> {
    let cfg = KodConfig::load_default()?;
    println!("Jev thresholds");
    for name in kod_config::JevThresholds::NAMES {
        if let Some(v) = cfg.jev.thresholds.get(name) {
            println!("  {name:<24} {v:.2}");
        }
    }
    Ok(())
}

/// `kod jev tune set <name> <value>` — set a threshold and persist.
pub async fn run_jev_tune_set(name: &str, value: f32) -> Result<()> {
    let mut cfg = KodConfig::load_default()?;
    if !cfg.jev.thresholds.set(name, value) {
        return Err(KodError::InvalidParameters {
            reason: format!(
                "unknown threshold {name:?}; known: {}",
                kod_config::JevThresholds::NAMES.join(", "),
            ),
        });
    }
    cfg.jev.thresholds.clamp();
    let path = KodConfig::config_dir()?.join("config.toml");
    cfg.save_to(&path)?;
    eprintln!("Set {name} = {value:.2} in {}", path.display());
    Ok(())
}

/// `kod jev tune reset` — restore defaults.
pub async fn run_jev_tune_reset() -> Result<()> {
    let mut cfg = KodConfig::load_default()?;
    cfg.jev.thresholds = kod_config::JevThresholds::default();
    let path = KodConfig::config_dir()?.join("config.toml");
    cfg.save_to(&path)?;
    eprintln!("Reset every threshold to default in {}", path.display());
    Ok(())
}

/// `kod budget show` — total cost from the newest session log.
pub async fn run_budget_show(log: Option<std::path::PathBuf>) -> Result<()> {
    let path = match log {
        Some(p) => p,
        None => newest_session_log()?.ok_or_else(|| {
            KodError::Config("no session log found under ~/.kod/sessions/".to_string())
        })?,
    };
    let entries = kod_core::session_log::read_session(&path)?;
    let mut total_cost = 0.0_f64;
    let mut per_endpoint: std::collections::BTreeMap<String, (usize, usize, f64)> =
        Default::default();
    for e in &entries {
        if let kod_core::session_log::SessionEntry::Cost {
            endpoint,
            prompt_tokens,
            completion_tokens,
            cost_usd,
            ..
        } = e
        {
            total_cost += *cost_usd;
            let slot = per_endpoint.entry(endpoint.clone()).or_insert((0, 0, 0.0));
            slot.0 += prompt_tokens;
            slot.1 += completion_tokens;
            slot.2 += cost_usd;
        }
    }
    println!("Session cost for {}", path.display());
    println!("  total: ${total_cost:.4}");
    if !per_endpoint.is_empty() {
        println!();
        println!("  endpoint                in tok   out tok        cost");
        for (ep, (p, c, cost)) in &per_endpoint {
            println!("  {ep:<24} {p:>7} {c:>9}  ${cost:.4}");
        }
    }
    let cfg = KodConfig::load_default()?;
    if cfg.limits.max_cost_usd_per_session > 0.0 {
        let cap = cfg.limits.max_cost_usd_per_session;
        let pct = total_cost / cap * 100.0;
        println!();
        println!("  session cap: ${cap:.2} ({pct:.1}% used)");
    }
    Ok(())
}

/// `kod limits show` — effective `[limits]` config.
pub async fn run_limits_show() -> Result<()> {
    let cfg = KodConfig::load_default()?;
    let l = &cfg.limits;
    println!("Limits");
    println!(
        "  max_cost_usd_per_session:   {}",
        l.max_cost_usd_per_session
    );
    println!("  max_cost_usd_per_turn:      {}", l.max_cost_usd_per_turn);
    println!(
        "  max_input_tokens_per_turn:  {}",
        l.max_input_tokens_per_turn
    );
    println!(
        "  max_output_tokens_per_turn: {}",
        l.max_output_tokens_per_turn
    );
    println!("  on_exhausted:               {:?}", l.on_exhausted);
    println!("  soft_warn_at:               {}", l.soft_warn_at);
    if !l.tools.is_empty() {
        println!();
        println!("Per-tool quotas");
        for (tool, q) in &l.tools {
            println!(
                "  {tool:<24} per_turn={} per_session={} per_command={}",
                q.per_turn, q.per_session, q.per_command,
            );
        }
    }
    Ok(())
}

/// `kod plan show` — read state.json's plan for the default
/// transcript.
pub async fn run_plan_show(state: Option<std::path::PathBuf>) -> Result<()> {
    let path = match state {
        Some(p) => p,
        None => kod_core::StateStore::default_path()
            .ok_or_else(|| KodError::Config("no home directory for state.json".to_string()))?,
    };
    let store = kod_core::StateStore::open(path.clone());
    let loaded = store.load();
    let Some(plan) = loaded.plans.get("session") else {
        eprintln!("No plan recorded in {}", path.display());
        return Ok(());
    };
    println!("Plan for: {}", plan.goal);
    println!();
    for step in &plan.steps {
        let marker = match step.status {
            kod_core::PlanStatus::Done => "✓",
            kod_core::PlanStatus::InProgress => "→",
            kod_core::PlanStatus::Blocked => "!",
            kod_core::PlanStatus::Skipped => "·",
            kod_core::PlanStatus::Pending => " ",
        };
        println!("{} {}. {}", marker, step.id + 1, step.text);
    }
    println!();
    println!("Progress: {:.0}%", plan.progress() * 100.0);
    Ok(())
}

/// `kod plan json` — plan as JSON.
pub async fn run_plan_json(state: Option<std::path::PathBuf>) -> Result<()> {
    let path = match state {
        Some(p) => p,
        None => kod_core::StateStore::default_path()
            .ok_or_else(|| KodError::Config("no home directory for state.json".to_string()))?,
    };
    let store = kod_core::StateStore::open(path);
    let loaded = store.load();
    let plan = loaded.plans.get("session");
    let s =
        serde_json::to_string_pretty(&plan).map_err(|e| KodError::Serialization(e.to_string()))?;
    println!("{s}");
    Ok(())
}

/// `kod decisions show` — decisions from state.json.
pub async fn run_decisions_show(state: Option<std::path::PathBuf>, limit: usize) -> Result<()> {
    let path = match state {
        Some(p) => p,
        None => kod_core::StateStore::default_path()
            .ok_or_else(|| KodError::Config("no home directory for state.json".to_string()))?,
    };
    let store = kod_core::StateStore::open(path.clone());
    let loaded = store.load();
    let Some(log) = loaded.decision_logs.get("session") else {
        eprintln!("No decisions recorded in {}", path.display());
        return Ok(());
    };
    let cap = limit.min(200).min(log.entries.len());
    println!("Decisions ({} total, newest {}):", log.entries.len(), cap);
    for d in log.entries.iter().rev().take(cap) {
        let tag = match d.kind {
            kod_core::DecisionKind::UserPreference => "pref",
            kod_core::DecisionKind::Approach => "appr",
            kod_core::DecisionKind::FileChange => "file",
            kod_core::DecisionKind::Constraint => "cons",
            kod_core::DecisionKind::Other => "othr",
        };
        println!("  [{}] {}", tag, d.text);
    }
    Ok(())
}

/// `kod decisions json` — decisions as JSON.
pub async fn run_decisions_json(state: Option<std::path::PathBuf>) -> Result<()> {
    let path = match state {
        Some(p) => p,
        None => kod_core::StateStore::default_path()
            .ok_or_else(|| KodError::Config("no home directory for state.json".to_string()))?,
    };
    let store = kod_core::StateStore::open(path);
    let loaded = store.load();
    let log = loaded.decision_logs.get("session");
    let s =
        serde_json::to_string_pretty(&log).map_err(|e| KodError::Serialization(e.to_string()))?;
    println!("{s}");
    Ok(())
}

/// Default `turns.jsonl` path under the user's home directory.
pub(super) fn default_trace_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".kod").join("sessions").join("turns.jsonl"))
}

/// Resolve the path argument, falling back to the default, or error.
pub(super) fn resolve_trace_path(arg: Option<std::path::PathBuf>) -> Result<std::path::PathBuf> {
    match arg {
        Some(p) => Ok(p),
        None => default_trace_path().ok_or_else(|| {
            KodError::Config("no home directory to resolve the trace path".to_string())
        }),
    }
}

/// `kod trace list` — a compact table of recent turns.
pub async fn run_trace_list(limit: usize, path: Option<std::path::PathBuf>) -> Result<()> {
    let path = resolve_trace_path(path)?;
    if !path.exists() {
        eprintln!("no trace file at {}", path.display());
        eprintln!("  Set KOD_SESSION_LOG and run a session first.");
        return Ok(());
    }
    let traces = kod_core::read_traces(&path)?;
    if traces.is_empty() {
        eprintln!("no turns recorded in {}", path.display());
        return Ok(());
    }
    let cap = limit.min(200).min(traces.len());
    println!(
        "{:<6} {:<14} {:>9} {:>9} {:>10} {:>7}",
        "id", "holder", "duration", "cost", "tokens", "tools"
    );
    // Newest first.
    for t in traces.iter().rev().take(cap) {
        let dur = format!("{:.2}s", t.duration_ms() as f64 / 1000.0);
        let cost = format!("${:.4}", t.cost_usd);
        let toks = format!("{}→{}", t.prompt_tokens, t.completion_tokens);
        println!(
            "{:<6} {:<14} {:>9} {:>9} {:>10} {:>7}",
            t.id,
            truncate_field(&t.holder, 14),
            dur,
            cost,
            toks,
            t.tool_call_count,
        );
    }
    if traces.len() > cap {
        println!();
        println!("  … {} more (use --limit to see them)", traces.len() - cap);
    }
    Ok(())
}

/// `kod trace show <id>` — one turn's full tree.
pub async fn run_trace_show(id: u64, path: Option<std::path::PathBuf>) -> Result<()> {
    let path = resolve_trace_path(path)?;
    let traces = kod_core::read_traces(&path)?;
    let Some(t) = traces.iter().find(|t| t.id == id) else {
        eprintln!("no trace with id {id} in {}", path.display());
        return Ok(());
    };
    println!(
        "Turn #{} ({})   {:.2}s   ${:.4}   {} in → {} out",
        t.id,
        t.holder,
        t.duration_ms() as f64 / 1000.0,
        t.cost_usd,
        t.prompt_tokens,
        t.completion_tokens,
    );
    println!(
        "  prompt: {} chars   reply: {} chars   tools: {}",
        t.prompt_chars, t.reply_chars, t.tool_call_count,
    );
    println!(
        "  jev: {} decisions ({} cached)",
        t.jev_decisions, t.jev_cache_hits,
    );
    println!("  outcome: {:?}", t.outcome);
    if let Some(r) = &t.reason {
        println!("  reason: {r}");
    }
    if t.rounds.is_empty() {
        return Ok(());
    }
    println!();
    println!("Rounds");
    for (i, r) in t.rounds.iter().enumerate() {
        println!(
            "  {:>2}  {:<12} {:<24} {:>6}ms  {} in → {} out",
            i + 1,
            format!("{:?}", r.kind).to_lowercase(),
            format!("{}/{}", r.endpoint, r.model),
            r.duration_ms,
            r.input_tokens,
            r.output_tokens,
        );
        for c in &r.tool_calls {
            println!(
                "       tool  {:<16} {}ms  {}  {} bytes",
                c.name,
                c.duration_ms,
                match c.outcome {
                    kod_core::ToolOutcomeKind::Success => "ok",
                    kod_core::ToolOutcomeKind::Error => "err",
                    kod_core::ToolOutcomeKind::Denied => "denied",
                    kod_core::ToolOutcomeKind::RequiresConfirmation => "ask",
                },
                c.output_bytes,
            );
        }
        for rr in &r.retries {
            println!(
                "       retry  {} → {}  ({})",
                rr.from_endpoint, rr.to_endpoint, rr.reason,
            );
        }
    }
    Ok(())
}

/// `kod trace replay <id>` — re-drive one turn against a fresh
/// engine. The trace carries the user prompt; the engine rebuilds
/// the same request from scratch. Returns `Ok(())` on a match, or
/// `Err` on a divergence (in strict mode) or an engine error.
pub async fn run_trace_replay(
    id: u64,
    path: Option<std::path::PathBuf>,
    strict: bool,
) -> Result<()> {
    let path = resolve_trace_path(path)?;
    let traces = kod_core::read_traces(&path)?;
    let Some(t) = traces.iter().find(|t| t.id == id) else {
        eprintln!("no trace with id {id} in {}", path.display());
        return Ok(());
    };
    if t.user_prompt.is_empty() {
        return Err(KodError::Config(format!(
            "trace {id} has no user prompt (written by an older kod); \
             cannot replay",
        )));
    }

    // Build an in-memory fixture with the recorded request summary.
    // No tool-call records (the trace's tool calls do not carry
    // enough information to re-execute them safely in a replay), so
    // the engine will run only the first round and stop.
    let want = kod_core::RequestSummary {
        system_chars: t.prompt_chars,
        message_count: t.rounds.first().map(|_| 1).unwrap_or(0),
        tool_names: Vec::new(),
        model: t
            .rounds
            .first()
            .map(|r| r.model.clone())
            .unwrap_or_default(),
        endpoint: t
            .rounds
            .first()
            .map(|r| r.endpoint.clone())
            .unwrap_or_default(),
    };

    // Drive a fresh engine through the same prompt, capture the
    // request the engine actually sent, and compare.
    let (captured, engine_error) = drive_prompt_through_fresh_engine(&t.user_prompt).await;
    if let Some(e) = engine_error {
        eprintln!("engine error during replay: {e}");
        if strict {
            return Err(KodError::InvalidState(format!("replay engine: {e}")));
        }
        return Ok(());
    }

    let Some(captured_first) = captured.into_iter().next() else {
        eprintln!("no request captured during replay");
        return Ok(());
    };
    let got = kod_core::RequestSummary::from_request(&captured_first);
    if want.hash() == got.hash() {
        println!("✓ turn {id} replay matches (hash {})", &want.hash()[..8],);
        return Ok(());
    }
    eprintln!(
        "✗ turn {id} replay diverged (expected {}, got {})",
        &want.hash()[..8],
        &got.hash()[..8],
    );
    eprintln!(
        "  system_chars:  {} → {}",
        want.system_chars, got.system_chars
    );
    eprintln!(
        "  message_count: {} → {}",
        want.message_count, got.message_count
    );
    eprintln!("  model:         {} → {}", want.model, got.model);
    if strict {
        return Err(KodError::InvalidState(format!(
            "replay diverged on turn {id}"
        )));
    }
    Ok(())
}

/// Drive a fresh engine with `prompt` and return the captured
/// requests. Uses an empty provider registry — we only need the
/// engine to build the request, not to answer it.
pub(super) async fn drive_prompt_through_fresh_engine(
    prompt: &str,
) -> (Vec<kod_provider::CompletionRequest>, Option<String>) {
    use kod_provider::replay::{ReplayProvider, ReplayRound};

    // A provider that answers with a single empty text round. The
    // engine's first request is what we want; the answer does not
    // matter.
    //
    // Two handles to the same provider: the concrete one is what
    // `captured()` lives on, and the `dyn LlmProvider` is what the
    // registry stores.
    let replay_concrete = std::sync::Arc::new(ReplayProvider::new(vec![ReplayRound {
        text: String::new(),
        tool_calls: Vec::new(),
        usage: None,
    }]));
    let provider: std::sync::Arc<dyn kod_provider::LlmProvider> = replay_concrete.clone();

    let mut registry = kod_provider::ProviderRegistry::new();
    registry.insert(
        "replay",
        provider.clone(),
        kod_provider::ProviderCapabilities {
            tools: true,
            streaming_tools: true,
            ..kod_provider::ProviderCapabilities::conservative()
        },
        "replay-fixture",
    );

    let tmp = std::env::temp_dir().join(format!(
        "kod-trace-replay-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    if let Err(e) = std::fs::create_dir_all(&tmp) {
        return (Vec::new(), Some(format!("tempdir: {e}")));
    }
    let cfg = match std::env::current_dir() {
        Ok(cwd) => kod_core::RouterConfig {
            working_dir: cwd,
            enable_memory: false,
            ..kod_core::RouterConfig::default()
        },
        Err(e) => return (Vec::new(), Some(format!("cwd: {e}"))),
    };
    let engine = match kod_core::KodEngine::new(cfg, tmp.join("replay.redb")) {
        Ok(e) => e,
        Err(e) => return (Vec::new(), Some(format!("engine: {e}"))),
    };
    engine
        .set_registry(
            std::sync::Arc::new(registry),
            kod_provider::ModelRef::new("replay", "replay-fixture"),
            None,
        )
        .await;
    if let Err(e) = engine.start().await {
        return (Vec::new(), Some(format!("start: {e}")));
    }
    let _ = engine.process_for("session", prompt).await;
    let _ = engine.shutdown().await;
    let _ = std::fs::remove_dir_all(&tmp);
    // The concrete handle is what `captured()` lives on; the
    // provider we gave the registry shares the same inner state.
    let captured = replay_concrete.captured();
    (captured, None)
}

/// `kod trace json` — every turn as a JSON array.
pub async fn run_trace_json(path: Option<std::path::PathBuf>) -> Result<()> {
    let path = resolve_trace_path(path)?;
    let traces = kod_core::read_traces(&path)?;
    let s = serde_json::to_string_pretty(&traces)
        .map_err(|e| KodError::Serialization(e.to_string()))?;
    println!("{s}");
    Ok(())
}

/// Truncate a display field to `max` chars, adding `…` on cut.
pub(super) fn truncate_field(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}
