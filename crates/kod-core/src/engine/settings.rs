use super::*;

impl KodEngine {
    #[cfg(test)]
    pub(crate) async fn install_test_provider(&self, provider: Arc<dyn LlmProvider>) {
        let mut reg = kod_provider::ProviderRegistry::new();
        reg.insert(
            "default",
            provider,
            kod_provider::ProviderCapabilities::conservative(),
            "",
        );
        self.set_registry(
            Arc::new(reg),
            kod_provider::ModelRef::new("default", ""),
            None,
        )
        .await;
    }

    /// Create a new engine
    pub fn new(config: RouterConfig, db_path: PathBuf) -> Result<Self> {
        let working_dir = config.working_dir.clone();
        // Capture the caller's context_window before `config` moves
        // into the router. Used by `budget_hint_for`.
        let config_window = config.context_window;
        // Git operations default to enabled because the git tools that
        // exist today (`git_status`, `git_diff`) are read-only. A
        // future mutating git tool (commit, branch, checkout) must
        // reconsider this default — today, disabling the flag turns
        // off `git status` for a caller that wants it, which is the
        // wrong trade.
        // `network_access` stays off in the default context. The
        // `web_fetch` tool is registered either way; a caller that
        // wants the agent to reach the network must construct a
        // context with `network_access: true`. Wiring this to the
        // `LlmConfig::network_access` flag is a follow-up — the
        // context is built before the config is available here, and
        // the CLI/TUI do not currently pass a context in.
        // TaskRouter first: the `memory://` handler needs a handle to
        // it, and the protocol router's construction depends on the
        // handler.
        let router = TaskRouter::new(config, db_path)?;
        let router_for_handler = Arc::new(router);
        // The tool registry is created here (rather than as a struct
        // literal field) so the xd:// handler below can share it.
        let tools_for_router: Arc<ToolRegistry> = Arc::new(ToolRegistry::new());
        // Delta §7.5: the artifact store + the protocol router. The
        // handler is stored on the engine so offload sites
        // (`store_artifact`) can write; the router is installed into
        // the base `tool_context` so every derived per-call context
        // sees the same handler.
        let artifact_handler = Arc::new(kod_tools::ArtifactHandler::new());
        let protocol_router = kod_tools::ProtocolRouter::new()
            .register(Arc::clone(&artifact_handler) as Arc<dyn kod_tools::ProtocolHandler>);
        // Delta §7.5: `memory://search/<query>` delegates to the
        // router's long-term retrieval. Registered unconditionally —
        // when memory is disabled the handler returns a clear
        // "memory is disabled" error, which is more useful to the
        // model than a "unknown scheme" one.
        let memory_handler = Arc::new(crate::memory_handler::MemoryHandler::new(Arc::clone(
            &router_for_handler,
        )));
        let protocol_router =
            protocol_router.register(memory_handler as Arc<dyn kod_tools::ProtocolHandler>);
        // Delta §6: the `xd://` scheme mounts discoverable tools. It
        // shares the engine's registry, so a tool demoted from the
        // tools array is still reachable through read/write.
        let xd_handler = Arc::new(kod_tools::xd_handler::XdHandler::new(Arc::clone(
            &tools_for_router,
        )));
        let protocol_router =
            protocol_router.register(xd_handler as Arc<dyn kod_tools::ProtocolHandler>);
        // Delta §7.7 item 4: the `conflict://` scheme. Reading a file
        // with merge markers through it lists the blocks with stable
        // ids; writing `conflict://<id>` splices a resolution. The
        // store is session-scoped so an id from one read resolves in
        // the next call.
        let conflict_store = Arc::new(kod_tools::conflict_handler::ConflictStore::new());
        let conflict_handler = Arc::new(kod_tools::conflict_handler::ConflictHandler::new(
            Arc::clone(&conflict_store),
        ));
        let protocol_router =
            protocol_router.register(conflict_handler as Arc<dyn kod_tools::ProtocolHandler>);
        // Delta §5: the minimizer + the artifact-store hook. The
        // hook captures the `Arc<ArtifactHandler>` directly, so it
        // does not need a reference to the engine (which would be a
        // cycle: the engine owns the base context owns the hook).
        let minimizer = Arc::new(kod_tools::kod_minimize::Minimizer::with_builtins());
        let artifact_store_hook: kod_tools::context::ArtifactStoreHook = {
            let handler = Arc::clone(&artifact_handler);
            kod_tools::context::ArtifactStoreHook::new(
                move |id: String, text: String, mime: String, owner: String| {
                    let handler = Arc::clone(&handler);
                    Box::pin(async move {
                        handler
                            .store_for(id, text, mime, owner)
                            .await
                            .map_err(|e| KodError::InvalidParameters {
                                reason: format!("artifact store: {e}"),
                            })
                    })
                },
            )
        };
        let edit_store = std::sync::Arc::new(std::sync::Mutex::new(
            kod_tools::edit_hashline::EditStore::new(),
        ));
        let tool_context = ToolContext::new(working_dir.clone())
            .with_edit_store(std::sync::Arc::clone(&edit_store))
            .with_permissions(ToolPermissions {
                read_files: true,
                write_files: true,
                execute_commands: true,
                network_access: false,
                git_access: kod_types::GitAccess::Write,
                allowed_paths: Vec::new(),
                forbidden_paths: Vec::new(),
            })
            .with_protocol_router(protocol_router)
            .with_minimizer(minimizer, artifact_store_hook);
        let lock_table = Arc::new(PathLockTable::new());
        // Snapshots are best-effort: a session without a home directory
        // still runs, it just cannot roll back. The manager is
        // per-working-directory, so two sessions on different projects
        // do not see each other's checkpoints.
        let checkpoints =
            crate::checkpoint::CheckpointManager::for_working_dir(&working_dir).map(Arc::new);

        Ok(Self {
            router: router_for_handler,
            registry: RwLock::new(None),
            current_model: RwLock::new(ModelRef::new("default", "")),
            routing: RwLock::new(None),
            retry_config: RwLock::new(kod_config::RetryConfig::default()),
            is_running: Arc::new(RwLock::new(false)),
            tools: tools_for_router,
            tool_context,
            artifact_handler,
            lock_table,
            working_dir: working_dir.clone(),
            steers: std::sync::Arc::new(RwLock::new(HashMap::new())),
            cancels: parking_lot::RwLock::new(std::collections::HashMap::new()),
            pause_gate: std::sync::Arc::new(crate::pause_gate::PauseGate::new()),
            goal_runtime: RwLock::new(crate::goals::GoalRuntime::new()),
            async_delivery: std::sync::Arc::new(parking_lot::Mutex::new(
                crate::async_delivery::AsyncDelivery::new(),
            )),
            run_collector: std::sync::Arc::new(parking_lot::Mutex::new(
                crate::run_collector::RunCollector::new(),
            )),
            stats: std::sync::Arc::new(parking_lot::Mutex::new(
                kod_stats::request::Aggregates::new(),
            )),
            telemetry: RwLock::new(kod_telemetry::Telemetry::disabled()),
            mental_models: RwLock::new(kod_memory::mental_models::MentalModels::new()),
            ttsr: RwLock::new(kod_provider::ttsr::TtsrEngine::new(Vec::new())),
            retention_cursors: RwLock::new(HashMap::new()),
            deferred_diagnostics: std::sync::Arc::new(
                crate::deferred_diagnostics::DeferredDiagnostics::new(),
            ),
            decisions_cursors: RwLock::new(HashMap::new()),
            behavioral: std::sync::Arc::new(parking_lot::Mutex::new(
                kod_stats::behavioral::BehavioralSignals::default(),
            )),
            history: RwLock::new(HashMap::new()),
            observed_usage: RwLock::new(HashMap::new()),
            context_gauges: RwLock::new(HashMap::new()),
            advisor_guard: std::sync::Arc::new(parking_lot::Mutex::new(
                kod_swarm::advisor::EmissionGuard::new(),
            )),
            tool_loop_guards: RwLock::new(HashMap::new()),
            todo_trackers: RwLock::new(HashMap::new()),
            compaction_dispatcher: std::sync::Arc::new(
                crate::compaction_dispatcher::CompactionDispatcher::new(vec![
                    // The engine's dispatcher fires only after
                    // `maybe_compact_for` has already decided the
                    // transcript is over threshold — i.e. "reduce
                    // now, or a summary call follows." Under that
                    // regime the default shake config's 16,000-token
                    // protect window and 4,000-token savings gate
                    // are miscalibrated: they assume an
                    // *opportunistic* call on an otherwise-clean
                    // session, where "no reduction" is a valid
                    // outcome. Here, no reduction means the model
                    // gets a round-trip. `aggressive()` (protect
                    // 4,000, savings gate 0) matches the situation.
                    // Delta §4.1: the *engine's* ladder ordering
                    // differs from the design's default. The
                    // design's `[remote, snapcompact, handoff,
                    // shake, soft]` is the right order for a
                    // deliberate compaction (`/compact`), where the
                    // caller asked for the best reduction and is
                    // willing to wait for it.
                    //
                    // The engine's dispatch regime is different: it
                    // fires *after* `maybe_compact_for` decided the
                    // transcript is over threshold — "reduce now, or
                    // a summary call follows". Under that pressure a
                    // cheap elision that clears the threshold is
                    // strictly better than an expensive LLM call that
                    // does not need to happen. So the mechanical
                    // rungs go first, and handoff catches the case
                    // where neither mechanical rung can reduce.
                    Box::new(crate::compaction_dispatcher::ShakeMethod::new(
                        crate::shake::ShakeConfig::aggressive(),
                    )),
                    // Prune keeps its defaults: a supersede prune
                    // only fires when a read is provably replaced by
                    // a newer read of the same path, so its
                    // 40,000-token protect window is not the same
                    // "protect the recent tail at all costs" thing
                    // shake's is. Shrinking it would blank reads
                    // whose only failing is being recent.
                    Box::new(crate::compaction_dispatcher::PruneMethod::new(
                        crate::prune::PruneConfig::default(),
                    )),
                    // The LLM rung last: it runs only when the
                    // mechanical rungs found nothing to elide, which
                    // is exactly the case where the alternative is
                    // the existing post-dispatcher summary path.
                    // Paying one call for a handoff document — which
                    // preserves meaning a shake cannot — is worth it
                    // there.
                    Box::new(crate::compaction_dispatcher::HandoffMethod::new()),
                ]),
            ),
            injected_memory_at: RwLock::new(HashMap::new()),
            prewarmed: RwLock::new(std::collections::HashSet::new()),
            pending_summaries: std::sync::Arc::new(RwLock::new(HashMap::new())),
            summaries_in_flight: std::sync::Arc::new(RwLock::new(std::collections::HashSet::new())),
            transcript_working_dirs: RwLock::new(HashMap::new()),
            plans: RwLock::new(HashMap::new()),
            prewalks: RwLock::new(HashMap::new()),
            sharpshooter_due: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            plan_mode: RwLock::new(std::collections::HashSet::new()),
            plan_reference_paths: RwLock::new(HashMap::new()),
            decision_logs: RwLock::new(HashMap::new()),
            sharpshooter_deltas: RwLock::new(Vec::new()),
            transcript_write_globs: RwLock::new(HashMap::new()),
            history_budget: std::sync::atomic::AtomicUsize::new(DEFAULT_HISTORY_CHAR_BUDGET),
            last_prompt: RwLock::new(HashMap::new()),
            generation_defaults: RwLock::new(GenerationDefaults::default()),
            session_recorder: std::sync::RwLock::new(None),
            current_requests: RwLock::new(HashMap::new()),
            jev_client: std::sync::RwLock::new(None),
            hooks: std::sync::RwLock::new(
                std::sync::Arc::new(crate::hooks::HookRunner::disabled()),
            ),
            // 2 == SandboxMode::Auto: use the best platform primitive
            // when available, silently fall through otherwise. The CLI
            // and TUI escalate to Require via `--sandbox`; a caller
            // that never touches this gets the design's safe default.
            sandbox_mode_atomic: std::sync::atomic::AtomicU8::new(2),
            read_protection: std::sync::RwLock::new(None),
            git_history_protected: std::sync::atomic::AtomicBool::new(true),
            redactor: std::sync::Arc::new(kod_types::redact::Redactor::default()),
            // Delta §14.1: the vault is installed by the CLI/TUI via
            // `set_secret_vault` after construction. A default
            // engine — every test, every embedder — sees `None` and
            // is unaffected.
            native_compaction_blocks: RwLock::new(HashMap::new()),
            image_render_cache: RwLock::new(HashMap::new()),
            speculative_reads: RwLock::new(true),
            image_frames: RwLock::new(HashMap::new()),
            secret_vault: RwLock::new(None),
            cost_tracker: crate::cost::CostTracker::new(),
            state_store: std::sync::RwLock::new(None),
            tool_counts: std::sync::Arc::new(crate::tool_quota::ToolCounts::new()),
            tool_quotas: std::sync::RwLock::new(None),
            tool_filter_states: RwLock::new(HashMap::new()),
            tool_surface_fingerprint: RwLock::new(HashMap::new()),
            cache_ledger: std::sync::Mutex::new(crate::cache_ledger::CacheLedger::new()),
            current_sensitivity: RwLock::new(crate::sensitivity::Sensitivity::Public),
            endpoint_trust: RwLock::new(std::collections::HashMap::new()),
            endpoint_health: std::sync::Mutex::new(
                crate::endpoint_health::EndpointHealth::default(),
            ),
            rate_limit_wait_budget_secs: std::sync::atomic::AtomicU64::new(
                kod_config::DEFAULT_RATE_LIMIT_WAIT_SECS,
            ),
            background: std::sync::Arc::new(crate::background::BackgroundJobRunner::default()),
            background_mode: std::sync::atomic::AtomicBool::new(false),
            fidelity_cache: RwLock::new(HashMap::new()),

            tool_inventory: std::sync::Arc::new(std::sync::RwLock::new(
                kod_tools::tool_search::ToolInventory::default(),
            )),
            budget_hint: std::sync::RwLock::new({
                let d = kod_config::LlmConfig::default();
                let ep = d.default_endpoint();
                (ep.context_window, ep.max_tokens.unwrap_or(2048))
            }),
            config_window: config_window,

            // Empty until a caller fetches `list_models()`. Every
            // `budget_hint_for` lookup degrades to the endpoint
            // config on a miss, so this starts invisible.
            model_catalog: std::sync::RwLock::new(std::collections::HashMap::new()),
            next_turn_id: std::sync::atomic::AtomicU64::new(1),
            turn_trace_writer: std::sync::RwLock::new(None),
            taint: std::sync::RwLock::new(kod_types::trust::TrustLevel::Assistant),
            network_access_atomic: std::sync::atomic::AtomicBool::new(false),
            auto_check_atomic: std::sync::atomic::AtomicBool::new(false),
            auto_lsp_atomic: std::sync::atomic::AtomicBool::new(true),
            policy: RwLock::new(None),
            deny_rules: RwLock::new(std::collections::HashSet::new()),
            learned_allows: RwLock::new(std::collections::HashSet::new()),
            lsp_manager: Arc::new(kod_lsp::LspManager::new(working_dir.clone())),
            check_baseline: Arc::new(RwLock::new(None)),
            next_approval_id: std::sync::atomic::AtomicU64::new(1),
            pending_approvals: RwLock::new(std::collections::HashMap::new()),
            pending_questions: RwLock::new(std::collections::HashMap::new()),
            next_question_id: std::sync::atomic::AtomicU64::new(1),
            swarm_hub: Arc::new(kod_swarm::AgentCommunicationHub::new()),
            swarm_file_bus: RwLock::new(None),
            blackboard: kod_swarm::Blackboard::new(),
            blackboard_viewers: RwLock::new(std::collections::HashSet::new()),
            session_id: kod_types::SessionId::new(),
            background_session_id: kod_types::BackgroundSessionId::new(),

            swarm_coordinator_id: kod_types::AgentId::new(),
            todo_list: kod_tools::new_todo_list(),
            checkpoints,
            memory_consolidation_task: tokio::sync::RwLock::new(None),
            mcp: RwLock::new(None),
        })
    }

    /// Set the total chars of history rendered into prompts. Called by
    /// the TUI and CLI after construction with a value derived from the
    /// model's context window (roughly `context_window * 3`, which is
    /// the char-count version of the 4-chars-per-token approximation
    /// with headroom for prompt scaffolding).
    ///
    /// Silently clamps below [`MIN_HISTORY_CHAR_BUDGET`]: a caller who
    /// passes a tiny value would otherwise produce an engine that
    /// forgets every turn before it finishes.
    pub fn set_history_budget(&self, chars: usize) {
        let clamped = chars.max(MIN_HISTORY_CHAR_BUDGET);
        self.history_budget
            .store(clamped, std::sync::atomic::Ordering::Relaxed);
    }

    /// Set the generation defaults the engine will pass to the provider
    /// on each call. Called by the CLI/TUI after construction from
    /// `LlmConfig`. Transitional until D1.
    pub fn set_generation_defaults(&self, temperature: Option<f32>, max_tokens: Option<usize>) {
        if let Ok(mut guard) = self.generation_defaults.try_write() {
            *guard = GenerationDefaults {
                temperature,
                max_tokens,
            };
        }
    }

    /// The current history budget in chars (for `/debug` and tests).
    pub fn history_budget(&self) -> usize {
        self.history_budget
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Set the sandbox mode for shell commands. `Required` wraps every
    /// `execute_command` in `bwrap` or `sandbox-exec`; a missing
    /// primitive fails each such call with a named reason.
    /// Install read-protection rules (Tier 1.3).
    /// The session cost accumulator (Tier 1.2).
    /// The current round's taint (Tier 1.1). `Assistant` when no
    /// tool has run in this round.
    pub fn taint_level(&self) -> kod_types::trust::TrustLevel {
        match self.taint.read() {
            Ok(g) => *g,
            Err(p) => *p.into_inner(),
        }
    }

    /// Public taint reset — the user reviewed the content.
    pub fn clear_taint(&self) {
        if let Ok(mut g) = self.taint.write() {
            *g = kod_types::trust::TrustLevel::Assistant;
        }
    }

    pub(crate) fn reset_taint(&self) {
        if let Ok(mut g) = self.taint.write() {
            *g = kod_types::trust::TrustLevel::Assistant;
        }
    }

    pub(crate) fn escalate_taint(&self, level: kod_types::trust::TrustLevel) {
        if let Ok(mut g) = self.taint.write()
            && level > *g
        {
            *g = level;
        }
    }

    /// Tier 1.1 — a call requires escalation only when the round's
    /// taint is one of the two adversarial levels AND the call is one
    /// of the high-impact tools. The policy engine has already had
    /// its chance to deny; this gate converts `Allow` into `Ask`
    /// under a tainted round.
    pub(crate) fn requires_approval_for_taint(&self, call: &ToolCall) -> bool {
        let t = self.taint_level();
        if !t.is_tainting() {
            return false;
        }
        matches!(
            call.tool_name.as_str(),
            "execute_command" | "write_file" | "patch_file" | "git_commit" | "git_branch_create"
        )
    }

    /// Snapshot of per-tool counters, for `/limits`.
    pub fn tool_count_snapshot(&self) -> Vec<(String, usize, usize)> {
        self.tool_counts.snapshot()
    }

    /// Reset per-session tool counters. `/limits reset`.
    pub fn reset_tool_counts(&self) {
        self.tool_counts.reset();
    }

    /// The resolved quota for a tool, if any is installed. Explicit
    /// entry first, then the `"default"` entry.
    pub(crate) fn quota_for(&self, tool: &str) -> Option<kod_config::ToolQuota> {
        let g = self.tool_quotas.read().ok()?;
        let map = g.as_ref()?;
        map.get(tool)
            .or_else(|| map.get("default"))
            .filter(|q| q.per_turn > 0 || q.per_session > 0 || q.per_command > 0)
            .cloned()
    }
}
