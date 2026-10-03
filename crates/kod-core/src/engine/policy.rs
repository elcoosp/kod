use super::*;

impl KodEngine {
    /// Install a per-project policy (D3-C1). Called by the CLI/TUI
    /// at startup from `PolicyEngine::load`. When this is not called,
    /// the engine falls back to the legacy `confirm_writes` gate for
    /// `write_file`/`patch_file` and leaves every other tool alone —
    /// the pre-D3 behaviour.
    pub async fn set_policy(&self, policy: Arc<kod_config::PolicyEngine>) {
        // F2b-10: cache the git-history knob so the per-call context
        // builder can thread it without re-reading the policy.
        self.git_history_protected.store(
            policy.git_history_protected(),
            std::sync::atomic::Ordering::Relaxed,
        );
        *self.policy.write().await = Some(policy);
    }

    /// Install the MCP host (D6.1). Called by the CLI/TUI at startup
    /// when `[mcp.servers]` is non-empty. When this is not called,
    /// no MCP tools are registered — a session without MCP is the
    /// default, and a session that only uses built-in tools pays
    /// nothing for the feature.
    pub async fn set_mcp_host(&self, host: Arc<crate::mcp_adapters::McpHost>) {
        *self.mcp.write().await = Some(host);
    }

    /// The installed MCP host, if any. Read by the CLI/TUI and by a
    /// future `/mcp` command; not consulted by the tool loop, which
    /// only sees the adapters registered at `start()` time.
    pub async fn mcp_host(&self) -> Option<Arc<crate::mcp_adapters::McpHost>> {
        self.mcp.read().await.clone()
    }

    /// The installed policy, if any.
    pub async fn policy(&self) -> Option<Arc<kod_config::PolicyEngine>> {
        self.policy.read().await.clone()
    }

    /// Register a session-scoped "never" rule (the `a` choice on the
    /// approval dialog). Consulted before every policy layer; a
    /// matching call is denied without a prompt, for the rest of the
    /// process.
    pub async fn add_deny_rule(&self, rule: kod_config::SessionDeny) {
        self.deny_rules.write().await.insert(rule);
    }

    /// The current set of session deny rules. Read by the TUI to show
    /// `kod policy show`-style summaries, and by tests.
    pub async fn deny_rules(&self) -> Vec<kod_config::SessionDeny> {
        let mut rules: Vec<kod_config::SessionDeny> =
            self.deny_rules.read().await.iter().cloned().collect();
        // `HashSet` iteration order is unspecified. `kod policy forget
        // <n>` names rules by their position in this list, so the list
        // must be deterministic: sort by tool, then by path pattern.
        rules.sort_by(|a, b| {
            (a.tool.as_str(), a.path_pattern.as_deref())
                .cmp(&(b.tool.as_str(), b.path_pattern.as_deref()))
        });
        rules
    }

    /// The session deny rule at 1-based index `n`, in the stable order
    /// `deny_rules()` returns. `None` when `n` is 0 or past the end.
    ///
    /// The set is a `HashSet<SessionDeny>` (order-undefined), so the
    /// index is meaningful only in combination with `deny_rules()`:
    /// this accessor sorts internally the same way that method does,
    /// so `kod policy forget <n>` and the listing a user reads agree
    /// on which rule index `n` names.
    pub async fn deny_rule_at(&self, n: usize) -> Option<kod_config::SessionDeny> {
        if n == 0 {
            return None;
        }
        let mut rules: Vec<kod_config::SessionDeny> =
            self.deny_rules.read().await.iter().cloned().collect();
        // Stable ordering — the same shape `deny_rules()` returns.
        // `SessionDeny` derives `Hash + Eq` but not `Ord`; sort on
        // the fields we can order to get a deterministic listing.
        rules.sort_by(|a, b| {
            (a.tool.as_str(), a.path_pattern.as_deref())
                .cmp(&(b.tool.as_str(), b.path_pattern.as_deref()))
        });
        rules.into_iter().nth(n - 1)
    }

    /// Drop a session deny rule. Returns `true` when the rule was
    /// present. The equality is the derived `Hash + Eq` on
    /// `SessionDeny`; two rules built from the same tool and path
    /// pattern compare equal, so a caller that fetched the rule via
    /// `deny_rule_at` can hand it straight back.
    pub async fn remove_deny_rule(&self, rule: &kod_config::SessionDeny) -> bool {
        self.deny_rules.write().await.remove(rule)
    }

    /// The provider for the current model, resolved through the
    /// registry. `None` when no registry is installed or the endpoint
    /// is unknown. Public because the swarm runner needs it and does
    /// not hold a registry reference itself.
    pub async fn current_provider(&self) -> Option<Arc<dyn LlmProvider>> {
        let registry = self.registry.read().await.clone();
        let model = self.current_model.read().await.clone();
        registry.as_ref().and_then(|r| r.resolve(&model).ok())
    }

    /// Answer a pending approval request. Returns `true` when the id
    /// matched a pending request and the decision was delivered; `false`
    /// when the id was unknown (a stale click, a consumer that raced a
    /// timeout). The consumer does not have to care which — a `false`
    /// just means the engine already gave up on this request.
    pub async fn respond_to_approval(&self, id: u64, decision: ApprovalDecision) -> bool {
        let sender = self.pending_approvals.write().await.remove(&id);
        match sender {
            Some(tx) => tx.send(decision).is_ok(),
            None => false,
        }
    }

    /// M-20: deny every pending approval at once. The ACP fail-closed
    /// path calls this when an approval batch cannot be parsed, so a
    /// corrupt batch denies immediately instead of hanging every item
    /// for `AWAIT_APPROVAL_SECS`. Returns the number denied.
    pub async fn deny_all_pending_approvals(&self) -> usize {
        let mut map = self.pending_approvals.write().await;
        let n = map.len();
        for (_, tx) in map.drain() {
            let _ = tx.send(ApprovalDecision::Deny);
        }
        n
    }

    /// Answer a pending ask_user question. Returns `true` when the id
    /// matched and the answer was delivered.
    pub async fn respond_to_question(&self, id: u64, answer: String) -> bool {
        let sender = self.pending_questions.write().await.remove(&id);
        match sender {
            Some(tx) => tx.send(answer).is_ok(),
            None => false,
        }
    }

    /// Install the shell hooks the engine runs around tool calls. A
    /// caller that never calls this gets a disabled runner.
    pub fn set_hooks(&self, config: kod_config::HooksConfig) {
        if let Ok(mut guard) = self.hooks.write() {
            *guard = std::sync::Arc::new(crate::hooks::HookRunner::new(config));
        }
    }

    /// Install a session log. Every tool call and its result is
    /// appended to the file the recorder holds. A caller that never
    /// calls this gets no log.
    pub fn set_session_recorder(&self, recorder: Arc<crate::session_log::SessionRecorder>) {
        if let Ok(mut slot) = self.session_recorder.write() {
            *slot = Some(recorder);
        }
    }

    /// Install a turn-trace writer (Tier 1.4). One `TurnTrace` per
    /// `process_*` call is appended to the file the writer holds.
    pub fn set_turn_trace_writer(&self, writer: std::sync::Arc<crate::trace_writer::TraceWriter>) {
        if let Ok(mut slot) = self.turn_trace_writer.write() {
            *slot = Some(writer);
        }
    }

    /// The trace log path, when a writer is installed.
    pub fn trace_path(&self) -> Option<std::path::PathBuf> {
        self.turn_trace_writer
            .read()
            .ok()
            .and_then(|g| g.as_ref().map(|w| w.path().to_path_buf()))
    }

    /// Allocate the next turn id. Monotonic; scoped to the session.
    /// The background job runner (P6). One per engine.
    pub fn background(&self) -> std::sync::Arc<crate::background::BackgroundJobRunner> {
        self.background.clone()
    }

    /// Spawn a cross-model review of a completed turn (P6).
    pub async fn spawn_background_review(
        &self,
        subject: crate::trace::TurnId,
        subject_text: String,
    ) -> crate::background::JobId {
        let id = self.background.allocate_id();
        let current = self.current_model().await;
        let alternative = self
            .resolve_model_ref_for_capability(&kod_swarm::Capability::CodeReview)
            .await
            .filter(|m| m.endpoint != current.endpoint);
        let endpoint = alternative.unwrap_or_else(|| {
            tracing::warn!(
                endpoint = %current.display(),
                "cross-model review unavailable; running same-model",
            );
            current.clone()
        });
        self.background.register(
            id,
            crate::background::JobKind::Review {
                subject,
                endpoint: endpoint.clone(),
            },
        );

        // Resolve the provider Arc and the generation options before
        // the spawn so the task does not need to hold `&self`. The
        // provider is an `Arc<dyn LlmProvider>` — cheap to clone and
        // safe across the task boundary.
        let provider = match self.resolve_provider_for_model_ref(&endpoint).await {
            Ok(p) => p,
            Err(e) => {
                self.background.fail(
                    id,
                    format!("could not resolve endpoint {}: {e}", endpoint.display()),
                );
                return id;
            }
        };
        let options = self.generation_defaults.read().await.to_options();
        let runner = self.background.clone();
        let subject_clone = subject_text.clone();
        let endpoint_display = endpoint.display();

        let runner_for_watch = std::sync::Arc::clone(&runner);
        runner_for_watch.spawn_guarded(id, async move {
            let _permit = runner.acquire_permit().await;
            // The review prompt: the reviewer's role preamble plus the
            // turn being reviewed. No tools — a first-pass review is a
            // read-and-critique, not a code change. A richer form that
            // lets the reviewer look up extra context (read a file the
            // turn mentioned) is a follow-up; it needs a child engine
            // with the read-only tool registry, which the
            // `background_mode` gate now supports.
            let prompt = format!(
                "You are a reviewer. A different model produced the \
                 assistant turn below. Read it and reply with a short \
                 critique: correctness issues, missing edge cases, and \
                 anything you would have done differently. Be concise; \
                 three to six bullet points. Do not restate the turn.\n\n\
                 ## Turn under review (on endpoint {endpoint_display})\n\n\
                 {subject_clone}",
            );
            let summary = match provider.generate(&prompt, &options).await {
                Ok(text) => {
                    let trimmed = text.trim();
                    if trimmed.is_empty() {
                        "(review returned an empty reply)".to_string()
                    } else {
                        trimmed.to_string()
                    }
                }
                Err(e) => {
                    runner.fail(id, format!("review generation failed: {e}"));
                    return;
                }
            };
            runner.complete(id, summary);
        });

        id
    }
}
