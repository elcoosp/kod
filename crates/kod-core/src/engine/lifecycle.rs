use super::*;

impl KodEngine {
    /// Start the engine
    pub async fn start(&self) -> Result<()> {
        let mut running = self.is_running.write().await;

        if *running {
            return Err(KodError::InvalidState("Engine already running".to_string()));
        }

        *running = true;

        // Delta §12.7: seed and fill the mental models before tools
        // register — a session that opens with a filled preferences
        // block gets it into the very first cached prefix. Best-effort
        // (see the method doc); the seed-and-fill cannot fail start().
        self.bootstrap_mental_models().await;

        // Delta §14.3: install the shipped stream rules. They were
        // plumbed end to end (prose and tool args observed on every
        // streamed chunk) but nothing ever called the installer, so
        // the engine ran with an empty rule set. The two builtins
        // are once-per-session and scoped tightly (a bare `TODO`
        // written by edit/write/patch, a credential-shaped string in
        // the model's own prose).
        self.install_builtin_ttsr_rules().await;

        // Register the built-in tools once (start runs exactly once —
        // second call errors above). Tools fail closed via ToolContext
        // permissions unless explicitly granted in `new()`.
        //
        // P6: a background job's child engine registers only the
        // read-only tools from the whitelist. The
        // `run_tool` gate is the belt; skipping the write-tool
        // registration is the braces — a tool that is not in the
        // registry cannot be called at all, so an
        // `unsafe_code`-level bug in the gate cannot leak.
        let background = self.is_background();
        self.tools.register(Box::new(ReadFileTool::new())).await;
        if !background {
            self.tools.register(Box::new(WriteFileTool::new())).await;
            self.tools.register(Box::new(PatchFileTool::new())).await;
            // Delta §7.1: the hashline edit tool. Its store is shared
            // through the tool context.
            self.tools
                .register(Box::new(kod_tools::EditHashlineTool::new()))
                .await;
        }
        // Swarm coordination tools (D4.3). The blackboard is the
        // engine's `AgentCommunicationHub` — the note tool broadcasts
        // a `KnowledgeShare` and the read tool filters the coordinator's
        // received history.
        self.tools
            .register(Box::new(crate::swarm_adapters::SwarmNoteTool::new(
                self.swarm_hub(),
                self.swarm_coordinator_id.clone(),
            )))
            .await;
        self.tools
            .register(Box::new(crate::swarm_adapters::SwarmReadTool::new(
                self.swarm_hub(),
                self.swarm_coordinator_id.clone(),
            )))
            .await;
        // Delta §7.6: the op-dispatched hub tool (messaging + jobs;
        // process supervision reports unavailable).
        self.tools
            .register(Box::new(crate::hub_tool::HubTool::new(
                self.swarm_hub(),
                self.swarm_coordinator_id.clone(),
                self.background(),
            )))
            .await;
        self.tools.register(Box::new(ListFilesTool::new())).await;
        self.tools.register(Box::new(GrepTool::new())).await;
        // Delta §7.4: semantic search. Uses the Jev judge when one is
        // installed, else a lexical fallback (the result says which).
        self.tools
            .register(Box::new(crate::jfind_tool::JfindTool::new(
                self.jev_client(),
            )))
            .await;
        self.tools.register(Box::new(FileInfoTool::new())).await;
        // P3-e: batch runs N sub-calls in one round. The tool holds a
        // Weak to the registry so it cannot keep the engine alive and
        // cannot outlive the tools it dispatches to. Registered with
        // the rest of the file tools because its common case is
        // batching reads.
        self.tools
            .register(Box::new(kod_tools::batch::BatchTool::new(
                std::sync::Arc::downgrade(&self.tools),
            )))
            .await;
        if !background {
            self.tools
                .register(Box::new(ExecuteCommandTool::new()))
                .await;
        }
        // Git tools. Read-only ones (status/diff/branch list) are
        // gated by `GitAccess::Read`; the two write tools (commit,
        // branch create) by `GitAccess::Write`. Both permission
        // levels come from `ToolContext::permissions.git_access`,
        // which the engine grants at construction.
        // Memory tools (D2-B3a). Registered when the router has a
        // memory manager. The tools handle the disabled case by
        // erroring, but not registering when memory is off keeps the
        // model from seeing a tool that cannot do anything.
        if self.router.has_memory() {
            self.tools
                .register(Box::new(crate::memory_tools::MemorySaveTool::new(
                    self.router.clone(),
                )))
                .await;
            self.tools
                .register(Box::new(crate::memory_tools::MemorySearchTool::new(
                    self.router.clone(),
                )))
                .await;
        }

        // LSP tools (D5-L3a). Registered unconditionally; each tool
        // holds a clone of the shared client slot and returns an
        // error naming the missing server when no LSP binary exists
        // for the file's language. `engine.start()` takes `&self`, so
        // the tools take the slot Arc, not the engine Arc.
        self.tools
            .register(Box::new(
                kod_core_tools::lsp_tools::LspDiagnosticsTool::new(Arc::clone(&self.lsp_manager)),
            ))
            .await;
        self.tools
            .register(Box::new(kod_core_tools::lsp_tools::LspDefinitionTool::new(
                Arc::clone(&self.lsp_manager),
            )))
            .await;
        self.tools
            .register(Box::new(kod_core_tools::lsp_tools::LspReferencesTool::new(
                Arc::clone(&self.lsp_manager),
            )))
            .await;
        self.tools
            .register(Box::new(kod_core_tools::lsp_tools::LspHoverTool::new(
                Arc::clone(&self.lsp_manager),
            )))
            .await;

        // MCP tools (D6.1). Every enabled server is spawned once
        // here, its tools listed, and one `McpToolAdapter` registered
        // per tool under the `mcp:<server>.<tool>` naming policy. A
        // server that fails to start is logged and skipped — one
        // broken plugin does not block the built-in tools, nor the
        // other plugins.
        //
        // This runs inside `start()` (which is idempotent by
        // construction: a second call errors out early) so the MCP
        // servers and the tool registry reach a consistent state at
        // the same moment.
        if let Some(host) = self.mcp.read().await.clone() {
            let tools = host.startup_tools().await;
            let count = tools.len();
            for t in tools {
                self.tools.register(t).await;
            }
            if count > 0 {
                tracing::info!(count, "registered MCP tools");
            }
        }

        // Delta §11.8: the advisor channel. Registered alongside the
        // other swarm tools. The tool holds a `Weak<KodEngine>` so it
        // cannot keep the engine alive; it shares the guard whose
        // `begin_update` the turn loop calls once per turn.
        {
            let sink: std::sync::Arc<dyn crate::advisor_tools::AdvisorSink> =
                std::sync::Arc::new(crate::advisor_tools::SteerQueueSink {
                    steers: std::sync::Arc::clone(&self.steers),
                    is_running: std::sync::Arc::clone(&self.is_running),
                });
            self.tools
                .register(Box::new(crate::advisor_tools::AdviseTool::new(
                    self.advisor_guard.clone(),
                    sink,
                )))
                .await;
        }

        self.tools.register(Box::new(GitStatusTool::new())).await;
        self.tools.register(Box::new(GitDiffTool::new())).await;
        if !background {
            self.tools
                .register(Box::new(kod_tools::GitCommitTool::new()))
                .await;
            self.tools
                .register(Box::new(kod_tools::GitBranchTool::new()))
                .await;
        }
        self.tools
            .register(Box::new(kod_tools::TodoTool::new(self.todo_list.clone())))
            .await;
        self.tools
            .register(Box::new(kod_tools::SearchFilesTool::new()))
            .await;
        self.tools
            .register(Box::new(kod_tools::AskUserTool::new()))
            .await;
        self.tools
            .register(Box::new(kod_tools::PlanTool::new()))
            .await;
        // `web_fetch` is registered unconditionally; the per-context
        // `network_access` permission gates the actual call. This is
        // the same shape the git tools use, and it means a future
        // caller that wants to enable network access for one agent
        // does not have to re-register the tool.
        self.tools
            .register(Box::new(kod_tools::WebFetchTool::new()))
            .await;

        // P3: tool_search lets the model find a tool by description.
        // It reads a shared inventory the engine keeps in sync with
        // the registry, so a tool added later (an MCP server) is
        // visible without re-registering.
        self.tools
            .register(Box::new(kod_tools::tool_search::ToolSearchTool::new(
                self.tool_inventory.clone(),
            )))
            .await;
        // `check` runs the project's compiler/linter and returns
        // structured diagnostics. Registered alongside the other
        // code-aware tools so a model that just wrote a file can ask
        // "did that break the build?" without grepping compiler
        // output.
        self.tools
            .register(Box::new(kod_tools::CheckTool::new()))
            .await;

        // P3: now that every built-in is registered, seed the
        // `tool_search` inventory. A later MCP server attaches its
        // own tools and calls `refresh_tool_inventory` again; this
        // first call covers the built-ins.
        self.refresh_tool_inventory().await;

        // Capture the check baseline in the background. Runs the
        // project's compiler once; the result is stored so the first
        // auto-check can distinguish the model's errors from
        // pre-existing ones. Non-blocking: a big workspace can take
        // tens of seconds and delaying `start` for it would slow the
        // whole session.
        //
        // The handle is not joined on shutdown; the task ends when
        // the compiler exits or the runtime drops. That is fine — the
        // compiler is a child process and Rust's Drop on the engine
        // does not affect it. A `check` running against a project
        // after shutdown writes nothing the session cares about.
        let this = BaselineRefresher {
            working_dir: self.working_dir.clone(),
            check_baseline: Arc::clone(&self.check_baseline),
        };
        tokio::spawn(async move {
            this.refresh_check_baseline().await;
            tracing::debug!("baseline refresh spawned");
        });

        self.start_memory_consolidation_task().await;

        // Delta §11.2: record what a cold revive needs to rebuild the
        // session's tool surface. Written once, here, so a restart can
        // read it back.
        self.record_session_init().await;

        tracing::info!("KOD engine started");
        Ok(())
    }

    /// Delta §11.2: append a `SessionInit` entry to the session log.
    ///
    /// The entry carries the endpoint, model, tool names, and a
    /// fingerprint of the tool surface + working dir. It is written
    /// once at `start()`; a caller that resumes a session reads it
    /// with `session_log::session_init_for`.
    pub(crate) async fn record_session_init(&self) {
        let model = self.current_model().await;
        let defs = self.tools.get_definitions().await;
        let tool_names: Vec<String> = defs.into_iter().map(|d| d.name).collect();
        let system_prompt_hash = self.surface_fingerprint(&tool_names).await;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let entry = kod_core_state::session_log::SessionEntry::SessionInit {
            timestamp_ms: now_ms,
            holder: String::new(),
            endpoint: model.endpoint.clone(),
            model: model.model.clone(),
            tool_names,
            system_prompt_hash,
        };
        let guard = self.session_recorder.read();
        if let Ok(g) = guard
            && let Some(rec) = g.as_ref()
            && let Err(e) = rec.record(&entry)
        {
            tracing::warn!(error = %e, "could not record session_init");
        }
    }

    /// Delta §11.2: the surface fingerprint — the working dir plus
    /// the sorted tool set, FNV-1a-64. Shared by
    /// `record_session_init` (which writes it) and
    /// `verify_cold_revive_surface` (which compares against it), so
    /// the two cannot drift.
    pub(crate) async fn surface_fingerprint(&self, tool_names: &[String]) -> u64 {
        let mut input = format!("{}\n", self.working_dir.display());
        let mut sorted: Vec<&String> = tool_names.iter().collect();
        sorted.sort();
        for n in sorted {
            input.push_str(n);
            input.push('\n');
        }
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in input.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h
    }

    /// Delta §11.2: the verdict of a cold-revive surface check.
    pub async fn verify_cold_revive_surface(
        &self,
        log_path: &std::path::Path,
        holder: &str,
    ) -> ColdReviveVerdict {
        let Some((_endpoint, _model, tool_names, persisted_hash)) =
            Self::session_init_from_log(log_path, holder).ok().flatten()
        else {
            // No init entry: a log written before the variant existed
            // or one for a different holder. The caller falls back to
            // a fresh-surface revive; nothing to compare.
            return ColdReviveVerdict::NoInitEntry;
        };
        let current_names: Vec<String> = self
            .tools
            .get_definitions()
            .await
            .into_iter()
            .map(|d| d.name)
            .collect();
        let current_hash = self.surface_fingerprint(&current_names).await;
        if current_hash == persisted_hash {
            ColdReviveVerdict::SurfaceMatches
        } else {
            let mut missing: Vec<String> = tool_names
                .iter()
                .filter(|n| !current_names.contains(n))
                .cloned()
                .collect();
            let mut added: Vec<String> = current_names
                .iter()
                .filter(|n| !tool_names.contains(n))
                .cloned()
                .collect();
            missing.sort();
            added.sort();
            ColdReviveVerdict::SurfaceDrifted { missing, added }
        }
    }

    /// Spawn the periodic memory-consolidation task (design D2.5).
    /// No-op when `compaction_interval_secs == 0` or memory is disabled.
    ///
    /// The task holds a clone of the router `Arc` (the manager lives
    /// inside it). `shutdown()` aborts and awaits this handle before
    /// closing redb, so the router's unique-holder path is available
    /// at that moment.
    pub(crate) async fn start_memory_consolidation_task(&self) {
        let interval_secs = match kod_config::KodConfig::load_cached() {
            Ok(c) => c.memory.compaction_interval_secs,
            Err(_) => 0,
        };
        if interval_secs == 0 || !self.router.has_memory() {
            return;
        }
        // Already started (idempotent: start() is only callable once,
        // but be defensive against a future caller).
        if self.memory_consolidation_task.read().await.is_some() {
            return;
        }

        let router = Arc::clone(&self.router);
        // Delta §12.3: the periodic tick cannot reach the engine, so
        // it flags the sharpshooter consolidation for the next turn.
        // The engine's `drain_due_sharpshooter` runs it there, on the
        // engine's own async context.
        let sharpshooter_due = std::sync::Arc::clone(&self.sharpshooter_due);
        let handle = tokio::spawn(async move {
            let interval = std::time::Duration::from_secs(interval_secs);
            loop {
                tokio::time::sleep(interval).await;
                match router.consolidate_memory().await {
                    Ok(report) if report.archived > 0 || report.fused > 0 => {
                        tracing::info!(
                            archived = report.archived,
                            fused = report.fused,
                            "memory consolidation pass",
                        );
                        // The report moved facts around; the
                        // sharpshooter's target files should be
                        // refreshed on the next turn.
                        sharpshooter_due.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    Ok(_) => {
                        tracing::debug!("memory consolidation pass: nothing to do");
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "memory consolidation failed; will retry next tick",
                        );
                    }
                }
            }
        });
        *self.memory_consolidation_task.write().await = Some(handle);
        tracing::debug!(interval_secs, "memory consolidation task started",);

        // P2: rehydrate the transcript from a prior process's session
        // log when one is available. The path comes from the installed
        // recorder (set by the CLI/TUI before `start()`); a process
        // with no recorder has no log to read and this is a no-op.
        // Failures are logged, not propagated: a corrupt or unreadable
        // log must not prevent the engine from starting.
        if let Some(log_path) = self.session_log_path() {
            match self
                .rehydrate_from_log_for(DEFAULT_TRANSCRIPT_KEY, &log_path)
                .await
            {
                Ok(0) => tracing::debug!("no transcript to rehydrate"),
                Ok(n) => tracing::info!(
                    count = n,
                    path = %log_path.display(),
                    "rehydrated transcript from session log",
                ),
                Err(e) => tracing::warn!(
                    error = %e,
                    path = %log_path.display(),
                    "could not rehydrate transcript; starting fresh",
                ),
            }
        }
    }

    /// Stop the consolidation task (if any) and wait for it to actually
    /// drop. Necessary before the redb close: the task holds a router
    /// clone, and `Arc::try_unwrap` needs the last reference.
    pub(crate) async fn stop_memory_consolidation_task(&self) {
        let handle = self.memory_consolidation_task.write().await.take();
        if let Some(h) = handle {
            h.abort();
            let _ = h.await;
            tracing::debug!("memory consolidation task stopped");
        }
    }

    /// Shutdown the engine. Idempotent: a second call returns `Ok(())`
    /// without touching anything. Every step is best-effort and logs on
    /// failure — a shutdown that refuses to finish because one subsystem
    /// misbehaved is worse than one that reports the problem and
    /// continues.
    ///
    /// Order matters:
    /// 1. Flip the running flag and drop the lock early so callers
    ///    blocked on `is_running()` do not stall behind the teardown.
    /// 2. Signal cancellation so any in-flight tool round / goal loop
    ///    observes the stop at its next check.
    /// 3. Shut down the LSP client (may be mid-request).
    /// 4. Release every path-lock cell.
    /// 5. Flush the session recorder (per-line already, belt-and-braces).
    /// 6. Trim checkpoints to their retention cap.
    ///
    /// Redb is intentionally not closed here: `Arc<Database>` closes on
    /// last reference drop, and forcing it would require unwinding the
    /// `Arc<TaskRouter>` held by callers of this engine. Documented in
    /// `LongTermMemory` — the OS will flush on process exit, which is
    /// sufficient for redb's durability guarantee (fsync per commit).
    pub async fn shutdown(&self) -> Result<()> {
        let was_running = {
            let mut running = self.is_running.write().await;
            if !*running {
                return Ok(());
            }
            *running = false;
            true
        };
        if !was_running {
            return Ok(());
        }

        // 2. Signal stop to every running loop. F2c-10: `request_cancel`
        //    targets only the default transcript; a swarm run's
        //    `swarm:<id>` loops (and any other per-key transcript)
        //    kept running through teardown. Cancel every key the
        //    engine knows about, then the default as a belt-and-braces.
        {
            // `cancels` is a parking_lot RwLock — synchronous.
            let keys: Vec<String> = self.cancels.read().keys().cloned().collect();
            for k in keys {
                self.request_cancel_for(&k);
            }
        }
        self.request_cancel();

        // 3. LSP client shutdown (may be mid-request; timeout inside).
        self.lsp_shutdown().await;

        // 3b. MCP server shutdown (D6.1). Best-effort: each spawned
        //     server is killed and reaped; a slow or misbehaving
        //     server is dropped, not awaited indefinitely. A failure
        //     here is logged and does not block the rest of the
        //     shutdown sequence.
        if let Some(host) = self.mcp.read().await.clone() {
            host.shutdown_all().await;
        }

        // 4. Release path-lock cells. Outstanding guards keep their own
        //    Arc and release on drop.
        self.lock_table.release_all().await;

        // Delta §14.5: mark the clean exit before the flush. A
        // session that ends without this marker but has a
        // ToolExecutionStart was interrupted.
        self.record_session_exit("clean", Vec::new());

        // 5. Session recorder flush — best-effort.
        if let Ok(guard) = self.session_recorder.read()
            && let Some(recorder) = guard.as_ref()
            && let Err(e) = recorder.flush()
        {
            tracing::warn!(
                error = %e,
                path = %recorder.path().display(),
                "session log flush failed during shutdown"
            );
        }

        // 6. Checkpoint retention — best-effort.
        if let Some(cp) = self.checkpoints.as_ref()
            && let Err(e) = cp.enforce_retention()
        {
            tracing::warn!(error = %e, "checkpoint retention failed during shutdown");
        }

        // 7. Memory extraction (D2-B3b), opt-in.
        if let Ok(cfg) = kod_config::KodConfig::load_cached()
            && cfg.memory.extract_on_shutdown
            && let Err(e) = self.extract_memories_now("").await
        {
            tracing::warn!(error = %e, "shutdown memory extraction failed");
        }

        // 7a. Delta §12.3: consolidate the friction-gated decision
        //     deltas from this session into the repo's decisions
        //     files. Runs while the provider is still resolvable
        //     (7b tears down the memory task; the provider registry
        //     lives on the engine and is only dropped when the
        //     engine is), so this is the last chance to make the
        //     small-model rewrite call. Best-effort — a failed
        //     consolidation loses the queue but not the shutdown.
        let consolidated = self.consolidate_sharpshooter_now().await;
        if consolidated > 0 {
            tracing::info!(
                files = consolidated,
                "sharpshooter consolidation wrote decisions files"
            );
        }

        // 7b. Stop the consolidation task before any redb close —
        //     it holds a router clone, and `Arc::try_unwrap`
        //     needs the last reference.
        self.stop_memory_consolidation_task().await;

        // 7c. Delta §12.5: wait for any in-flight background embed
        //     tasks. A fact written just before shutdown still gets
        //     its vector; without this, the entry would stay
        //     FTS-only until the next rebuild.
        self.router.flush_embeddings().await;

        // 8. Explicit redb close (design D0.3). Best-effort: the
        // router is behind an `Arc` and a caller that cloned the
        // engine may still hold a reference. `Arc::try_unwrap`
        // returns the value on the unique-holder path — the common
        // case for a CLI/TUI session that has stopped driving the
        // engine — and returns the `Arc` back on the shared path,
        // in which case we fall back to the previous behaviour
        // (the OS file lock releases when the last reference drops).
        match Arc::try_unwrap(self.router.clone()) {
            Ok(router) => router.close_memory(),
            Err(_) => tracing::debug!(
                "router Arc still shared at shutdown; redb will close when \
                 the last reference drops"
            ),
        }

        tracing::info!("KOD engine shutdown complete");
        Ok(())
    }

    /// Check if engine is running
    pub async fn is_running(&self) -> bool {
        *self.is_running.read().await
    }

    /// Get router reference
    pub fn router(&self) -> &TaskRouter {
        &self.router
    }

    /// Load skills from a directory into the router's matcher.
    pub async fn load_skills(&self, skills_dir: &std::path::Path) -> Result<usize> {
        self.router.load_skills(skills_dir).await
    }

    /// Load skills from every directory in `dirs`, skipping any that do
    /// not exist. Returns the total number of skill files read across all
    /// directories. Skills sharing a name across directories count once
    /// in the matcher (later dirs shadow earlier ones) but each file is
    /// counted here, so the returned number is "files loaded", not
    /// "distinct skills available" — use [`loaded_skill_names`] for the
    /// deduplicated set.
    pub async fn load_skills_from_dirs(&self, dirs: &[std::path::PathBuf]) -> Result<usize> {
        let mut total = 0;
        for dir in dirs {
            if !dir.is_dir() {
                continue;
            }
            match self.load_skills(dir).await {
                Ok(n) => total += n,
                Err(e) => {
                    tracing::warn!(
                        dir = %dir.display(),
                        error = %e,
                        "Could not load skills from directory"
                    );
                }
            }
        }
        Ok(total)
    }

    /// Watch `skills_dir` for changes and rebuild the router's skill
    /// matcher on each event. Call this after `load_skills` /
    /// `load_skills_from_dirs` when the caller wants newly-added or
    /// edited skills to appear without a restart. No-op when the
    /// directory does not exist or hot reload was already enabled for
    /// it.
    pub async fn enable_hot_reload(&self, skills_dir: &std::path::Path) -> Result<()> {
        self.router.enable_hot_reload(skills_dir).await
    }

    /// Names of all loaded skills (for `/skills` listing).
    pub async fn loaded_skill_names(&self) -> Vec<String> {
        self.router.loaded_skill_names().await
    }

    /// Names + descriptions of all loaded skills (for `/skills` listing).
    pub async fn loaded_skill_details(&self) -> Vec<(String, String)> {
        self.router.loaded_skill_details().await
    }
}
