use super::*;

impl KodEngine {
    /// Execute one round of model-requested tool calls.
    ///
    /// Failures become `ToolResult::Error` text so the model sees denials
    /// instead of stalling the loop.
    ///
    /// A round containing any mutating tool (`write_files` or
    /// `execute_commands` in its declared permissions) runs serially in
    /// caller order, so `[write_file(a), read_file(a)]` cannot race and
    /// the read is guaranteed to observe the write. All-read-only rounds
    /// still run concurrently — their results cannot depend on each other
    /// or on external state they did not observe themselves.
    /// S10 phase 1: run the post-tool hooks for every non-denied call.
    /// Extracted from `run_tool_calls` so the sequence (deny → execute
    /// → post-hook → session-log) is a chain of named phases, not a
    /// 1,400-line block.
    ///
    /// Post-hooks are best-effort: a failure inside `run_post` is
    /// logged by that method and never propagated, so a formatting
    /// failure after a successful write cannot turn the write into a
    /// failed tool call.
    pub(crate) async fn run_post_hooks(
        &self,
        calls: &[ToolCall],
        hook_runner: Option<&std::sync::Arc<crate::hooks::HookRunner>>,
        hook_denied: &std::collections::HashMap<usize, String>,
    ) {
        let Some(runner) = hook_runner else { return };
        if !runner.is_enabled() {
            return;
        }
        for (i, call) in calls.iter().enumerate() {
            if hook_denied.contains_key(&i) {
                continue;
            }
            runner.run_post(call).await;
        }
    }

    /// S10 phase 2: append one JSONL entry per tool call. Best-effort —
    /// a write failure is logged and the run continues.
    fn record_session_tool_calls(
        &self,
        calls: &[ToolCall],
        raw_results: &[(Result<ToolResult>, u64)],
        holder: &str,
    ) {
        let Ok(guard) = self.session_recorder.read() else {
            return;
        };
        let Some(recorder) = guard.as_ref() else {
            return;
        };
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        for (i, call) in calls.iter().enumerate() {
            let Some((result, ms)) = raw_results.get(i) else {
                continue;
            };
            let result_json = match result {
                Ok(ToolResult::Success(v)) => serde_json::json!({ "success": v }),
                Ok(ToolResult::Error(e)) => serde_json::json!({ "error": e }),
                Ok(ToolResult::RequiresConfirmation { description, .. }) => {
                    serde_json::json!({ "requires_confirmation": description })
                }
                Err(e) => serde_json::json!({ "error": e.to_string() }),
            };
            let entry = kod_core_state::session_log::SessionEntry::ToolCall {
                timestamp_ms: now_ms,
                holder: holder.to_string(),
                tool_name: call.tool_name.clone(),
                arguments: call.arguments.clone(),
                duration_ms: *ms,
                result: result_json,
            };
            if let Err(e) = recorder.record(&entry) {
                tracing::warn!(
                    error = %e,
                    path = %recorder.path().display(),
                    "could not append session log entry"
                );
            }
        }
    }

    /// S10 phase 3: attach a unified diff to every successful
    /// `write_file` / `patch_file` result that had a pre-call
    /// snapshot. Failures are silent skips — a missing snapshot, an
    /// unreadable file, or a binary diff just means "no diff on this
    /// row", not a broken round.
    fn attach_write_diffs(
        &self,
        calls: &[ToolCall],
        snapshot_ids: &[Option<String>],
        raw_results: &mut [(Result<ToolResult>, u64)],
    ) {
        let Some(cp) = self.checkpoints.as_ref() else {
            return;
        };
        for (i, call) in calls.iter().enumerate() {
            if !matches!(call.tool_name.as_str(), "write_file" | "patch_file") {
                continue;
            }
            let Some(sid) = snapshot_ids.get(i).and_then(|o| o.as_ref()) else {
                continue;
            };
            let Some(snap) = cp.find(sid).ok().flatten() else {
                continue;
            };
            let Ok(new_content) = std::fs::read_to_string(&snap.path) else {
                continue;
            };
            let diff = kod_tools::patch::render_unified_diff(
                &snap.content,
                &new_content,
                &snap.path.display().to_string(),
            );
            if let Some(entry) = raw_results.get_mut(i)
                && let Ok(ToolResult::Success(v)) = &mut entry.0
                && let Some(obj) = v.as_object_mut()
            {
                obj.insert("diff".to_string(), serde_json::Value::String(diff));
            }
        }
    }

    /// S10 phase 4: build the structured `Role::Assistant` +
    /// `Role::Tool` messages the provider sees for this round. Pure
    /// function of `calls` and `results` — no I/O, no logging.
    ///
    /// Ids are preserved when the provider emitted them; a provider
    /// that did not (some local servers omit them) gets a synthesized
    /// `call_N` so the transcript is well-formed on every wire.
    ///
    /// H-E2 caps each tool result before it goes on the wire so a
    /// 256 KB `read_file` repeated over 40 rounds cannot grow the
    /// transcript past the endpoint's window.
    fn build_round_messages(
        calls: &[ToolCall],
        results: &[ToolResult],
    ) -> Vec<kod_types::ChatMessage> {
        const STRUCTURED_TOOL_MSG_CAP: usize = 16 * 1024;
        let mut messages: Vec<kod_types::ChatMessage> = Vec::new();
        let mut assistant_msg = kod_types::ChatMessage::text(
            kod_types::MessageId::new(),
            kod_types::MessageRole::Assistant,
            String::new(),
            time::OffsetDateTime::now_utc(),
        );
        for (i, call) in calls.iter().enumerate() {
            let id = call.id.clone().unwrap_or_else(|| format!("call_{i}"));
            assistant_msg.tool_calls.push(kod_types::ToolCall {
                id: Some(id),
                tool_name: call.tool_name.clone(),
                arguments: call.arguments.clone(),
            });
        }
        // Only push the assistant message when there was at least one
        // call — an empty assistant turn is not a legal wire shape.
        if !assistant_msg.tool_calls.is_empty() {
            messages.push(assistant_msg);
        }
        for (i, call) in calls.iter().enumerate() {
            let id = call.id.clone().unwrap_or_else(|| format!("call_{i}"));
            // P3-d: does the model consent to a large result? The
            // flag is in the schema of every tool, so a call that
            // wants the full output sets it. Absent or false means
            // the context guard withholds.
            let accepts_large = call
                .arguments
                .get("accept_large_output")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let rendered = match results.get(i) {
                Some(kod_types::ToolResult::Success(v)) => {
                    let raw = v.to_string();
                    if raw.len() > STRUCTURED_TOOL_MSG_CAP {
                        if accepts_large {
                            // Consented: truncate at the cap anyway,
                            // because the endpoint's window is the
                            // hard limit — the flag buys the model
                            // the result up to the wire cap, not past
                            // it.
                            format!(
                                "{}…[truncated at the {} byte wire cap; the \
                                 full result was {} bytes — narrow the query \
                                 to see the rest]",
                                truncate_chars(&raw, STRUCTURED_TOOL_MSG_CAP),
                                STRUCTURED_TOOL_MSG_CAP,
                                raw.len(),
                            )
                        } else {
                            // Withheld, not truncated. The model gets
                            // the size and the flag, so its next move
                            // is informed: narrow the query, or
                            // re-issue with consent.
                            format!(
                                "[result withheld: {} bytes exceeds the {} byte \
                                 cap. Narrow the query, or re-issue this call \
                                 with `accept_large_output: true` to receive it \
                                 truncated to the cap.]",
                                raw.len(),
                                STRUCTURED_TOOL_MSG_CAP,
                            )
                        }
                    } else {
                        raw
                    }
                }
                Some(kod_types::ToolResult::Error(e)) => format!("error: {e}"),
                Some(kod_types::ToolResult::RequiresConfirmation { description, .. }) => {
                    format!("requires confirmation: {description}")
                }
                None => String::new(),
            };
            let mut tool_msg = kod_types::ChatMessage::text(
                kod_types::MessageId::new(),
                kod_types::MessageRole::Tool,
                rendered,
                time::OffsetDateTime::now_utc(),
            );
            tool_msg.tool_call_id = Some(id);
            // Delta §7.7 item 1: flag a result that told the model
            // nothing (an empty read, a grep with no hits, a clean
            // diff). A §3 pruning pass drops these without
            // re-inspecting the payload. Only a `Success` can be
            // useless; an error or a confirmation carries a message.
            if let Some(kod_types::ToolResult::Success(v)) = results.get(i)
                && kod_types::is_useless(&call.tool_name, v)
            {
                tool_msg.metadata.useless = true;
            }
            messages.push(tool_msg);
        }
        messages
    }

    /// S10 phase 5: the policy gate. Decides every tool call before
    /// any of them runs; returns the denied set, the "needs an
    /// interactive approval" set, and the full decision log.
    ///
    /// The precedence is: learned allow > taint escalation > policy
    /// engine > "no policy installed" default. Only `Deny` and `Ask`
    /// decisions have side effects here — an `Allow` is just recorded.
    pub(crate) async fn gate_tool_calls(
        &self,
        calls: &[ToolCall],
        working_dir: &std::path::Path,
    ) -> PolicyGateResult {
        let policy = self.policy.read().await.clone();
        let deny_rules: std::collections::HashSet<kod_config::SessionDeny> =
            self.deny_rules.read().await.clone();
        let mut denied: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
        let mut need_approval: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut decisions: Vec<(usize, kod_config::PolicyDecision)> = Vec::new();

        for (i, call) in calls.iter().enumerate() {
            // Tier 2.3 — a learned allow short-circuits every gate.
            if self.is_learned_allowed(call).await {
                decisions.push((
                    i,
                    kod_config::PolicyDecision {
                        outcome: kod_config::Decision::Allow,
                        rule: "session learned allow (Tier 2.3)".to_string(),
                        source: kod_config::PolicySource::Preset,
                    },
                ));
                continue;
            }
            // Tier 1.1 — a tainted round forces `Ask` regardless.
            if self.requires_approval_for_taint(call) {
                let decision = kod_config::PolicyDecision {
                    outcome: kod_config::Decision::Ask,
                    rule: format!(
                        "taint escalation: {} under {:?}",
                        call.tool_name,
                        self.taint_level(),
                    ),
                    source: kod_config::PolicySource::SessionDeny,
                };
                need_approval.insert(i);
                decisions.push((i, decision));
                continue;
            }
            // P2-c: blast-radius gate for shell commands, ahead of
            // the policy engine. The policy engine decides *project
            // intent* ("is this command allowed here"); this decides
            // *blast radius* ("does this command delete the user's
            // home directory"). They are orthogonal — the policy may
            // allow `rm` in the workdir while this refuses `rm -rf ~`
            // — so the risk gate runs first and its Catastrophic
            // verdict is not overridable by a permissive policy.
            if call.tool_name == "execute_command"
                && let Some(cmd) = call.arguments.get("command").and_then(|c| c.as_str())
            {
                let ctx = kod_risk::RiskContext {
                    working_dir: working_dir.to_path_buf(),
                    home_dir: std::env::var_os("HOME")
                        .map(std::path::PathBuf::from)
                        .unwrap_or_else(|| std::path::PathBuf::from("/")),
                    scratch_dir: std::env::temp_dir(),
                };
                let assessment = kod_risk::assess(cmd, &ctx);
                match assessment.level {
                    kod_risk::RiskLevel::Catastrophic => {
                        // Refused outright. The policy engine cannot
                        // widen this.
                        let reason = assessment
                            .findings
                            .iter()
                            .map(|f| f.reason.clone())
                            .collect::<Vec<_>>()
                            .join("; ");
                        denied.insert(
                            i,
                            format!(
                                "command-risk: {reason}. If you genuinely need this, \
                                 run it yourself outside the agent."
                            ),
                        );
                        decisions.push((
                            i,
                            kod_config::PolicyDecision {
                                outcome: kod_config::Decision::Deny,
                                rule: format!("command-risk: {reason}"),
                                source: kod_config::PolicySource::Preset,
                            },
                        ));
                        continue;
                    }
                    kod_risk::RiskLevel::Confirm => {
                        // Not denied — asked. The finding names what
                        // is uncertain, so the user's approval is
                        // informed rather than a rubber stamp.
                        let reason = assessment
                            .findings
                            .iter()
                            .map(|f| f.reason.clone())
                            .collect::<Vec<_>>()
                            .join("; ");
                        need_approval.insert(i);
                        decisions.push((
                            i,
                            kod_config::PolicyDecision {
                                outcome: kod_config::Decision::Ask,
                                rule: format!("command-risk: {reason}"),
                                source: kod_config::PolicySource::Preset,
                            },
                        ));
                        continue;
                    }
                    // Safe / Low: fall through to the policy engine
                    // unchanged.
                    _ => {}
                }
            }
            let decision = match &policy {
                Some(p) => p.decide(&call.tool_name, &call.arguments, working_dir, &deny_rules),
                None => kod_config::PolicyDecision {
                    outcome: kod_config::Decision::Allow,
                    rule: "no policy installed".to_string(),
                    source: kod_config::PolicySource::Preset,
                },
            };
            match decision.outcome {
                kod_config::Decision::Allow => {}
                kod_config::Decision::Deny => {
                    denied.insert(i, decision.rule.clone());
                }
                kod_config::Decision::Ask => {
                    need_approval.insert(i);
                }
            }
            decisions.push((i, decision));
        }

        PolicyGateResult {
            denied,
            need_approval,
            decisions,
            policy,
        }
    }

    /// S10 phase 5 (cont.): write one `SessionEntry::PolicyDecision`
    /// per gate result so the JSONL carries the audit trail even if
    /// the run is interrupted mid-way.
    fn log_policy_decisions(
        &self,
        calls: &[ToolCall],
        holder: &str,
        decisions: &[(usize, kod_config::PolicyDecision)],
    ) {
        let Ok(guard) = self.session_recorder.read() else {
            return;
        };
        let Some(rec) = guard.as_ref() else {
            return;
        };
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        for (i, d) in decisions {
            let Some(call) = calls.get(*i) else { continue };
            let outcome = match d.outcome {
                kod_config::Decision::Allow => "allow",
                kod_config::Decision::Deny => "deny",
                kod_config::Decision::Ask => "ask",
            };
            let entry = kod_core_state::session_log::SessionEntry::PolicyDecision {
                timestamp_ms: now_ms,
                holder: holder.to_string(),
                tool_name: call.tool_name.clone(),
                outcome: outcome.to_string(),
                rule: d.rule.clone(),
                source: format!("{:?}", d.source).to_lowercase(),
            };
            let _ = rec.record(&entry);
        }
    }

    /// S10 phase 6: the `ask_user` interception. The tool itself
    /// cannot reach the chunk channel (its `execute` signature does
    /// not carry one), so the engine does the marker + await and
    /// hands the answer back as the tool result. A call with no
    /// `chunk_tx` (non-streaming `process`) becomes a placeholder
    /// answer the model can act on.
    pub(crate) async fn answer_ask_user_calls(
        &self,
        calls: &[ToolCall],
        holder: &str,
        chunk_tx: Option<&tokio::sync::mpsc::Sender<String>>,
    ) -> std::collections::HashMap<usize, String> {
        let mut answers: std::collections::HashMap<usize, String> =
            std::collections::HashMap::new();
        for (i, call) in calls.iter().enumerate() {
            if call.tool_name != "ask_user" {
                continue;
            }
            // P3.5 — try to answer from context first.
            let question_text = call
                .arguments
                .get("question")
                .and_then(|v| v.as_str())
                .unwrap_or("(no question)")
                .to_string();
            if let Some(auto_answer) = self
                .try_answer_question_from_context(holder, &question_text)
                .await
            {
                answers.insert(i, auto_answer);
                continue;
            }
            let Some(tx) = chunk_tx else {
                answers.insert(
                    i,
                    "(ask_user requires an interactive consumer; use kod tui or kod chat)"
                        .to_string(),
                );
                continue;
            };
            let placeholder = call
                .arguments
                .get("placeholder")
                .and_then(|v| v.as_str())
                .map(String::from);
            let req = kod_tools::ask::QuestionRequest {
                question: question_text,
                placeholder,
            };
            let json = serde_json::to_string(&req).unwrap_or_else(|_| "{}".to_string());
            let id = self
                .next_question_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let (otx, orx) = tokio::sync::oneshot::channel();
            self.pending_questions.write().await.insert(id, otx);
            let _ = tx.send(question_marker(id, &json)).await;
            let answer =
                tokio::time::timeout(std::time::Duration::from_secs(AWAIT_APPROVAL_SECS), orx)
                    .await;
            // F2c-8: remove the oneshot sender once the wait resolves.
            // Pre-fix a timed-out (unanswered) question left its entry
            // in `pending_questions` forever — one leaked sender per
            // dialog a user walked away from. A late answer then finds
            // nothing and reports false, which is the honest result
            // for an id that has already timed out.
            self.pending_questions.write().await.remove(&id);
            match answer {
                Ok(Ok(text)) => {
                    answers.insert(i, text);
                }
                Ok(Err(_)) => {
                    answers.insert(i, "(question cancelled)".to_string());
                }
                Err(_) => {
                    answers.insert(
                        i,
                        format!(
                            "(no answer within {}s — the user is away)",
                            AWAIT_APPROVAL_SECS
                        ),
                    );
                }
            }
        }
        answers
    }

    /// S10 phase 7: the approval flow — auto-approval via Jev, then
    /// the batched interactive dialog. Mutates three sets of state
    /// the caller needs back:
    ///
    /// * `denied` gains a reason for every call the user (or the
    ///   approval timeout) refused.
    /// * `need_approval` loses every call Jev auto-approved.
    /// * `edited_args` gains the arguments for every
    ///   `ApproveWith { arguments }` decision.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_approval_flow(
        &self,
        calls: &[ToolCall],
        holder: &str,
        chunk_tx: Option<&tokio::sync::mpsc::Sender<String>>,
        snapshot_ids: &[Option<String>],
        mut need_approval: std::collections::HashSet<usize>,
        mut denied: std::collections::HashMap<usize, String>,
        edited_args: &mut std::collections::HashMap<usize, serde_json::Value>,
    ) -> (
        std::collections::HashSet<usize>,
        std::collections::HashMap<usize, String>,
    ) {
        // Jev-gated auto-approval (P3.1). Before emitting a dialog,
        // ask Jev whether the user would almost certainly approve
        // each `Ask` call. Calls that clear both the
        // `likely_approved` threshold and the risk gate are removed
        // from `need_approval` and logged as auto-approved
        // `SessionEntry::Approval` entries, so the audit trail is
        // identical to a user approving them by hand.
        if !need_approval.is_empty() {
            let auto = self
                .auto_approve_with_jev(holder, calls, &need_approval)
                .await;
            if !auto.is_empty() {
                for i in &auto {
                    if let Some(call) = calls.get(*i)
                        && let Ok(guard) = self.session_recorder.read()
                        && let Some(rec) = guard.as_ref()
                    {
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        let entry = kod_core_state::session_log::SessionEntry::Approval {
                            timestamp_ms: now_ms,
                            holder: holder.to_string(),
                            tool_name: call.tool_name.clone(),
                            decision: "auto-approve".to_string(),
                            edit: None,
                        };
                        let _ = rec.record(&entry);
                    }
                }
                for i in &auto {
                    need_approval.remove(i);
                }
            }
        }

        if need_approval.is_empty() {
            return (need_approval, denied);
        }

        // P3.2 — ask Jev to group the pending approvals by logical
        // change. Result logged for `/jev stats`.
        {
            let pending_calls: Vec<ToolCall> = need_approval
                .iter()
                .filter_map(|i| calls.get(*i).cloned())
                .collect();
            let _groups = self.group_approvals_with_jev(holder, &pending_calls).await;
        }

        let Some(tx) = chunk_tx else {
            for i in &need_approval {
                denied.insert(
                    *i,
                    "policy requires approval but this execution \
                     path has no interactive consumer. Use the TUI, \
                     or install a permissive policy."
                        .to_string(),
                );
            }
            return (need_approval, denied);
        };

        // Phase 1 — build every request and register every oneshot
        // up-front so out-of-order answers are buffered.
        let mut items: Vec<ApprovalRequest> = Vec::new();
        let mut awaiting: Vec<(usize, u64, tokio::sync::oneshot::Receiver<ApprovalDecision>)> =
            Vec::new();
        for i in &need_approval {
            let Some(call) = calls.get(*i) else { continue };
            let summary = format_call_brief(&call.tool_name, &call.arguments);
            let diff = snapshot_ids
                .get(*i)
                .and_then(|o| o.as_ref())
                .and_then(|id| {
                    self.checkpoints
                        .as_ref()
                        .and_then(|cp| cp.find(id).ok().flatten())
                })
                .map(|snap| match call.tool_name.as_str() {
                    "write_file" => {
                        let new_content = call
                            .arguments
                            .get("content")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        kod_tools::patch::render_unified_diff(
                            &snap.content,
                            new_content,
                            &snap.path.display().to_string(),
                        )
                    }
                    "patch_file" => call
                        .arguments
                        .get("patch")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    _ => String::new(),
                });
            let id = self
                .next_approval_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let (otx, orx) = tokio::sync::oneshot::channel();
            self.pending_approvals.write().await.insert(id, otx);
            items.push(ApprovalRequest {
                tool_name: call.tool_name.clone(),
                arguments: call.arguments.clone(),
                diff,
                summary,
                id: Some(id),
            });
            awaiting.push((*i, id, orx));
        }

        // Phase 2 — emit ONE batch marker.
        let batch = ApprovalBatch { items };
        let json = serde_json::to_string(&batch).unwrap_or_else(|_| "{\"items\":[]}".to_string());
        let batch_id = self
            .next_approval_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let _ = tx.send(tool_approval_batch_marker(batch_id, &json)).await;

        // Phase 3 — await each in order.
        for (i, id, orx) in awaiting {
            let decision =
                tokio::time::timeout(std::time::Duration::from_secs(AWAIT_APPROVAL_SECS), orx)
                    .await;
            // F2c-8: evict the oneshot sender once the wait resolves —
            // an unanswered approval otherwise leaked its map entry.
            self.pending_approvals.write().await.remove(&id);
            let tool_name = calls
                .get(i)
                .map(|c| c.tool_name.clone())
                .unwrap_or_default();
            let (log_decision, allow) = match decision {
                Ok(Ok(ApprovalDecision::Approve)) => ("approve", true),
                Ok(Ok(ApprovalDecision::ApproveWith { arguments })) => {
                    edited_args.insert(i, arguments);
                    ("approve-edited", true)
                }
                Ok(Ok(ApprovalDecision::Deny)) => ("deny", false),
                Ok(Ok(ApprovalDecision::DenyAlways)) => ("deny-always", false),
                Ok(Err(_)) => ("cancelled", false),
                Err(_) => ("timeout", false),
            };
            if let Ok(guard) = self.session_recorder.read()
                && let Some(rec) = guard.as_ref()
            {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                let edit_snapshot = edited_args.get(&i).cloned();
                let entry = kod_core_state::session_log::SessionEntry::Approval {
                    timestamp_ms: now_ms,
                    holder: holder.to_string(),
                    tool_name: tool_name.clone(),
                    decision: log_decision.to_string(),
                    edit: edit_snapshot,
                };
                let _ = rec.record(&entry);
            }
            if !allow {
                if log_decision == "deny-always"
                    && let Some(call) = calls.get(i)
                {
                    let path_pattern = call
                        .arguments
                        .get("path")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    let rule = kod_config::SessionDeny {
                        tool: call.tool_name.clone(),
                        path_pattern,
                    };
                    self.add_deny_rule(rule).await;
                }
                let reason = match log_decision {
                    "deny" | "deny-always" => "denied by user",
                    "cancelled" => "approval cancelled",
                    "timeout" => "no approval answer within timeout — denied",
                    other => other,
                };
                denied.insert(i, reason.to_string());
            }
        }
        (need_approval, denied)
    }

    /// Delta §9.4: observe a completed tool round against the
    /// transcript's loop guard, and — on a detected loop — append a
    /// System corrective to `messages` so the next model call sees
    /// it.
    ///
    /// Called right after every `run_tool_calls` in both the
    /// collected and streaming loops. The corrective is pushed to
    /// the request-shaped `messages`, not to `self.history` — the
    /// corrective steers the *current* turn; persisting it across
    /// turns would let a single loop pollute every future prompt.
    /// The guard's own streak state is per-transcript, so a loop
    /// that resumes after an intervening non-loop round fires its
    /// next corrective exactly when the streak rebuilds.
    pub(crate) async fn maybe_emit_loop_corrective(
        &self,
        key: &str,
        calls: &[kod_types::ToolCall],
        results: &[kod_types::ToolResult],
        messages: &mut Vec<kod_types::ChatMessage>,
    ) {
        let mut guards = self.tool_loop_guards.write().await;
        let guard = guards
            .entry(key.to_string())
            .or_insert_with(kod_core_tools::tool_loop_guard::ToolLoopGuard::new);
        let Some(corrective) = guard.observe_round(calls, results) else {
            return;
        };
        // Drop the guard lock before touching `messages` (a caller
        // may hold an unrelated lock; keeping this one held would
        // invite a lock-ordering issue in a future refactor).
        drop(guards);
        let body = format!(
            "[tool-loop corrective] You have called `{}` {} rounds in a \
             row with the same arguments. The result is not changing; \
             stopping the loop is the right move. Arguments: {}. Last \
             result: {}. Try a different tool, different arguments, or \
             ask the user for guidance.",
            corrective.tool_name,
            corrective.count,
            corrective.arguments_summary,
            corrective.result_summary,
        );
        messages.push(kod_types::ChatMessage::text(
            kod_types::MessageId::new(),
            kod_types::MessageRole::System,
            body,
            time::OffsetDateTime::now_utc(),
        ));
    }

    /// Execute one round of model-requested tool calls.
    ///
    /// Failures become `ToolResult::Error` text so the model sees
    /// denials instead of stalling the loop.
    ///
    /// A round containing any mutating tool (`write_files` or
    /// `execute_commands` in its declared permissions) runs serially in
    /// caller order, so `[write_file(a), read_file(a)]` cannot race and
    /// the read is guaranteed to observe the write. All-read-only rounds
    /// still run concurrently — their results cannot depend on each other
    /// or on external state they did not observe themselves.
    pub(crate) async fn run_tool_calls(
        &self,
        calls: &[ToolCall],
        holder: &str,
        chunk_tx: Option<&tokio::sync::mpsc::Sender<String>>,
    ) -> ToolRound {
        // No speculations: the wrapper exists so the many test call
        // sites (and any caller without a streaming round behind it)
        // keep the pre-§10 signature. The streaming loop uses the
        // 4-arg form directly.
        self.run_tool_calls_with_speculations(calls, holder, chunk_tx, &[])
            .await
    }

    /// Delta §10: `run_tool_calls` plus the speculative reads the
    /// streaming round produced. `speculations` is indexed parallel
    /// to `calls` — `speculations[i]` is the pre-fetched read for
    /// `calls[i]`, or `None`.
    ///
    /// For each call with a speculation, the coordinator validates
    /// the file's identity (TOCTOU digest check) and, on match, puts
    /// the pre-fetched bytes on the per-call `ToolContext` so
    /// `ReadFileTool` uses them without re-reading. On mismatch the
    /// speculation is dropped and the tool reads as usual.
    pub(crate) async fn run_tool_calls_with_speculations(
        &self,
        calls: &[ToolCall],
        holder: &str,
        chunk_tx: Option<&tokio::sync::mpsc::Sender<String>>,
        speculations: &[Option<kod_core_tools::speculation::SpeculativeRead>],
    ) -> ToolRound {
        // Delta §9.8: the pause gate's tool-round boundary.
        self.pause_gate.wait_if_paused().await;
        // The tool context is scoped per transcript — a swarm agent
        // gets its own working dir and write-globs.
        let effective_holder: &str = if holder.is_empty() { "session" } else { holder };
        let per_transcript_wd = self.working_dir_for(effective_holder).await;
        let mut tool_context = self
            .tool_context
            .clone()
            .with_locks(Arc::clone(&self.lock_table), effective_holder)
            .with_sandbox(self.sandbox_setting());
        // Tier 1.3 — thread the engine's read-protection and
        // redactor into the per-call context.
        if let Ok(guard) = self.read_protection.read() {
            tool_context.read_protection = guard.clone();
        }
        tool_context.redactor = Some(self.redactor.clone());
        // F2b-10: thread the config's `.git` write protection.
        tool_context.git_history_protected = self
            .git_history_protected
            .load(std::sync::atomic::Ordering::Relaxed);
        if per_transcript_wd != self.working_dir {
            tool_context.working_dir = per_transcript_wd;
        }
        // Per-transcript write set (D4.2).
        if let Some(globs) = self.write_globs_for(effective_holder).await {
            tool_context.allowed_write_globs = Some(globs);
        }
        tool_context.permissions.network_access = self.network_access_setting();

        // P2-d: let `execute_command` hand a `run_in_background`
        // request to the job runner. Installed for every transcript —
        // a swarm agent starting a long build is as legitimate as the
        // session doing it.
        self.install_background_hook(&mut tool_context);

        // P1-c: a swarm transcript observes every file touch. The
        // interactive session (holder `""` → `effective_holder`
        // `"session"`) never gets a hook, so a single-user turn pays
        // nothing. `swarm_file_bus` is `None` outside a swarm run
        // regardless of the holder, so this is a no-op then too.
        if effective_holder != "session"
            && let Some(sfb) = self.swarm_file_bus.read().await.clone()
        {
            tool_context.on_file_touch = Some(Self::build_swarm_file_hook(sfb.bus, sfb.service));
        }

        // Any mutating tool in the round forces the serial path so
        // `[write_file(a), read_file(a)]` cannot race.
        let mut any_mutating = false;
        for call in calls {
            if let Some(perms) = self.tools.get_permissions(&call.tool_name).await
                && (perms.write_files || perms.execute_commands)
            {
                any_mutating = true;
                break;
            }
        }

        // Pre-tool hooks. A failing hook denies only its own call.
        let hook_runner = self.hooks.read().ok().map(|g| g.clone());
        let mut hook_denied: std::collections::HashMap<usize, String> =
            std::collections::HashMap::new();
        if let Some(runner) = hook_runner.as_ref()
            && runner.is_enabled()
        {
            for (i, call) in calls.iter().enumerate() {
                if let Err(e) = runner.run_pre(call).await {
                    hook_denied.insert(i, e.to_string());
                }
            }
        }
        let any_mutating = any_mutating || !hook_denied.is_empty();

        // Snapshot every mutating call's target BEFORE any of them
        // run. The snapshot ids feed the diff-augmentation phase and
        // the approval dialog.
        let mut snapshot_ids: Vec<Option<String>> = vec![None; calls.len()];
        if any_mutating && let Some(cp) = self.checkpoints.as_ref() {
            for (i, call) in calls.iter().enumerate() {
                if matches!(call.tool_name.as_str(), "write_file" | "patch_file")
                    && let Some(p) = call.arguments.get("path").and_then(|v| v.as_str())
                {
                    let abs = if std::path::Path::new(p).is_absolute() {
                        std::path::PathBuf::from(p)
                    } else {
                        tool_context.working_dir.join(p)
                    };
                    match cp.snapshot_before(&abs, &call.tool_name) {
                        Ok(id) => snapshot_ids[i] = id,
                        Err(e) => tracing::warn!(
                            path = %abs.display(),
                            error = %e,
                            "checkpoint snapshot failed"
                        ),
                    }
                }
            }
        }

        // Delta §10: validate every speculative read up-front. A
        // speculation whose evidence still describes the file becomes
        // a `PrefetchedRead` keyed by call index; a stale one is
        // dropped here and the tool reads as usual. Doing this once,
        // before either dispatch branch, means the serial and
        // parallel paths agree on which calls have a prefetch.
        let mut prefetches: std::collections::HashMap<usize, kod_tools::context::PrefetchedRead> =
            std::collections::HashMap::new();
        for (i, spec) in speculations.iter().enumerate() {
            let Some(spec) = spec.as_ref() else { continue };
            match kod_core_tools::speculation::validate(&spec.path, &spec.evidence) {
                Ok(true) => {
                    prefetches.insert(
                        i,
                        kod_tools::context::PrefetchedRead {
                            path: spec.path.clone(),
                            text: spec.text.clone(),
                        },
                    );
                }
                Ok(false) => {
                    tracing::debug!(
                        path = %spec.path.display(),
                        "speculative read discarded: file changed since the read",
                    );
                }
                Err(e) => {
                    tracing::debug!(
                        path = %spec.path.display(),
                        error = %e,
                        "speculative read discarded: validation failed",
                    );
                }
            }
        }

        // Policy gate (S10 phase 5).
        let gate = self.gate_tool_calls(calls, &tool_context.working_dir).await;
        self.log_policy_decisions(calls, effective_holder, &gate.decisions);
        let denied = gate.denied;
        let need_approval = gate.need_approval;
        let policy = gate.policy;
        let mut edited_args: std::collections::HashMap<usize, serde_json::Value> =
            std::collections::HashMap::new();

        // Approval flow (S10 phase 7). Auto-approval via Jev, then
        // the batched interactive dialog.
        let (_need_approval, mut denied) = self
            .run_approval_flow(
                calls,
                effective_holder,
                chunk_tx,
                &snapshot_ids,
                need_approval,
                denied,
                &mut edited_args,
            )
            .await;

        // ask_user interception (S10 phase 6).
        let mut answers = self
            .answer_ask_user_calls(calls, effective_holder, chunk_tx)
            .await;

        // Tier 2.3 — apply any argument edits captured by the
        // approval loop. Empty map means "dispatch as proposed".
        let mut calls_for_dispatch: Vec<ToolCall> = if edited_args.is_empty() {
            calls.to_vec()
        } else {
            calls
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    if let Some(new_args) = edited_args.get(&i) {
                        let mut c2 = c.clone();
                        c2.arguments = new_args.clone();
                        c2
                    } else {
                        c.clone()
                    }
                })
                .collect()
        };
        // Delta §14.1: deobfuscate placeholders in the *dispatch*
        // copy of every call's arguments. A model that read
        // `«Credential-abc»` and writes that string back in an
        // `edit`/`write_file` argument gets the raw value
        // substituted here — the tool sees the real bytes; the
        // transcript does not.
        //
        // This MUST run on `calls_for_dispatch`, not on `calls`. The
        // pre-fix shape mutated the caller's `calls` vec before
        // calling this function, and `build_round_messages` (below)
        // clones `calls_for_dispatch` into the assistant message it
        // pushes into the transcript — so the raw secret ended up
        // in the transcript and every subsequent provider request
        // carried it. Running the substitution here means the raw
        // secret lives only inside this function's dispatch loop;
        // `calls` is untouched, so the message built at S10 phase 4
        // still carries the placeholder.
        for call in calls_for_dispatch.iter_mut() {
            let n = self.deobfuscate_json(&mut call.arguments).await;
            if n > 0 {
                tracing::debug!(
                    tool = %call.tool_name,
                    count = n,
                    "deobfuscated secret placeholders in tool arguments",
                );
            }
        }
        // `build_round_messages` reads the *original* `calls` vec, not
        // the dispatch copy — the assistant message's `tool_calls`
        // field stays with the placeholder.
        // Tier 2.3 — re-run the policy gate on every edited call. An
        // edit that would have been DENIED by the current policy is
        // refused even though the user pressed `e` then `Enter`. The
        // user's consent to a specific edit is not consent to bypass
        // the session's policy preset. A taint escalation is *not*
        // re-asked because the user is the approving party here.
        if !edited_args.is_empty() {
            let deny_rules_snapshot: std::collections::HashSet<kod_config::SessionDeny> =
                self.deny_rules.read().await.clone();
            for i in edited_args.keys() {
                let Some(call) = calls_for_dispatch.get(*i) else {
                    continue;
                };
                let decision = match &policy {
                    Some(p) => p.decide(
                        &call.tool_name,
                        &call.arguments,
                        &tool_context.working_dir,
                        &deny_rules_snapshot,
                    ),
                    None => kod_config::PolicyDecision {
                        outcome: kod_config::Decision::Allow,
                        rule: "no policy installed".to_string(),
                        source: kod_config::PolicySource::Preset,
                    },
                };
                if matches!(decision.outcome, kod_config::Decision::Deny) {
                    denied.insert(
                        *i,
                        format!("edited call denied by policy: {}", decision.rule),
                    );
                    tracing::warn!(
                        index = *i,
                        rule = %decision.rule,
                        "approval edit refused by policy"
                    );
                }
            }
        }
        let mut raw_results: Vec<(Result<ToolResult>, u64)> = if any_mutating {
            let mut out = Vec::with_capacity(calls.len());
            for (i, call) in calls_for_dispatch.iter().enumerate() {
                if let Some(reason) = hook_denied.get(&i) {
                    out.push((
                        Ok(ToolResult::Error(format!(
                            "pre_tool_use hook denied this call: {reason}"
                        ))),
                        0,
                    ));
                    continue;
                }
                if let Some(reason) = denied.get(&i) {
                    self.run_collector
                        .lock()
                        .observe_tool(&call.tool_name, crate::run_collector::ToolStatus::Blocked);
                    out.push((Ok(ToolResult::Error(format!("write denied: {reason}"))), 0));
                    continue;
                }
                // P3.4 — per-command sandbox decision. Only
                // `execute_command` is negotiable; other tools keep
                // the round's configured mode. A clone of the
                // context is used so the decision does not leak to
                // sibling calls.
                let mut call_ctx = if call.tool_name == "execute_command"
                    && let Some(cmd) = call.arguments.get("command").and_then(|v| v.as_str())
                {
                    let chosen = self
                        .choose_sandbox_mode_for_command(
                            effective_holder,
                            cmd,
                            tool_context.sandbox,
                        )
                        .await;
                    let mut c = tool_context.clone();
                    c.sandbox = chosen;
                    c
                } else {
                    tool_context.clone()
                };
                // Delta §10: attach the pre-validated speculative read
                // for this call, if any. `ReadFileTool` checks the
                // path matches before using it.
                if let Some(pf) = prefetches.get(&i) {
                    call_ctx.prefetched_read = Some(pf.clone());
                }
                // Tier 2.5 — quota check before dispatch.
                let command = if call.tool_name == "execute_command" {
                    call.arguments.get("command").and_then(|v| v.as_str())
                } else {
                    None
                };
                let quota = self.quota_for(&call.tool_name);
                match crate::tool_quota::check(
                    &self.tool_counts,
                    &call.tool_name,
                    quota.as_ref(),
                    command,
                ) {
                    crate::tool_quota::QuotaVerdict::Hard { reason } => {
                        out.push((
                            Ok(ToolResult::Error(format!("quota exceeded: {reason}"))),
                            0,
                        ));
                        continue;
                    }
                    crate::tool_quota::QuotaVerdict::Soft { reason } => {
                        tracing::warn!(tool = %call.tool_name, "tool quota soft: {reason}");
                    }
                    crate::tool_quota::QuotaVerdict::Ok => {}
                }
                self.tool_counts.record(&call.tool_name, command);
                // Delta §14.5: mark the start so a crash mid-call
                // leaves a start with no completion — the resume
                // signal.
                self.record_tool_execution_start(
                    effective_holder,
                    &call.tool_name,
                    call.id.as_deref(),
                );
                let start = std::time::Instant::now();
                let res = self
                    .tools
                    .execute_tool(&call.tool_name, &call.arguments, &call_ctx)
                    .await;
                // Delta §9.11: record the call's status for /stats.
                {
                    use crate::run_collector::ToolStatus;
                    let status = match &res {
                        Ok(ToolResult::Success(_)) => ToolStatus::Ok,
                        Ok(ToolResult::Error(_)) => ToolStatus::Error,
                        Ok(ToolResult::RequiresConfirmation { .. }) => ToolStatus::Skipped,
                        Err(_) => ToolStatus::Error,
                    };
                    self.run_collector
                        .lock()
                        .observe_tool(&call.tool_name, status);
                    // Delta §14.5: mirror into OTLP when a handle is
                    // installed. `try_read` keeps the tool loop from
                    // blocking on a writer.
                    if let Ok(t) = self.telemetry.try_read() {
                        t.record_tool(kod_telemetry::ToolRecord {
                            name: call.tool_name.clone(),
                            status: match status {
                                crate::run_collector::ToolStatus::Ok => "ok",
                                crate::run_collector::ToolStatus::Error => "error",
                                crate::run_collector::ToolStatus::Skipped => "skipped",
                                crate::run_collector::ToolStatus::Blocked => "blocked",
                                crate::run_collector::ToolStatus::Timeout => "timeout",
                                crate::run_collector::ToolStatus::Aborted => "aborted",
                            }
                            .to_string(),
                            duration_ms: start.elapsed().as_millis() as u64,
                        });
                    }
                }
                out.push((res, start.elapsed().as_millis() as u64));
            }
            out
        } else {
            // Read-only round: no approvals are involved (approval is
            // only requested for write_file / patch_file, both of
            // which set `any_mutating` above), so the concurrent path
            // is unchanged.
            // Tier 2.5 — count the read-only dispatches before they
            // run. Enforcement (refusal) is serial-only; a
            // read-only round that hits a hard cap still runs its
            // peers so the model gets a complete answer.
            for call in calls.iter() {
                self.tool_counts.record(&call.tool_name, None);
            }
            // P0-2: honour policy / hook denials in the parallel
            // read-only branch too. Pre-fix mapped every call
            // straight to a future and never consulted `denied` /
            // `hook_denied`, so a denied read_file / grep /
            // web_fetch still ran whenever the round contained no
            // mutating calls.
            let n = calls_for_dispatch.len();
            let mut slots: Vec<Option<(Result<ToolResult>, u64)>> = (0..n).map(|_| None).collect();
            let mut run_indices: Vec<usize> = Vec::with_capacity(n);
            for i in 0..n {
                if let Some(reason) = hook_denied.get(&i) {
                    slots[i] = Some((
                        Ok(ToolResult::Error(format!(
                            "pre_tool_use hook denied this call: {reason}"
                        ))),
                        0,
                    ));
                    continue;
                }
                if let Some(reason) = denied.get(&i) {
                    slots[i] = Some((Ok(ToolResult::Error(format!("denied: {reason}"))), 0));
                    continue;
                }
                run_indices.push(i);
            }
            let futs: Vec<_> = run_indices
                .iter()
                .map(|&i| {
                    let call = &calls_for_dispatch[i];
                    // Delta §14.5: start marker (see the serial
                    // branch). Emitted before the await so a crash
                    // mid-call leaves the unmatched start.
                    self.record_tool_execution_start(
                        effective_holder,
                        &call.tool_name,
                        call.id.as_deref(),
                    );
                    let start = std::time::Instant::now();
                    let mut ctx = tool_context.clone();
                    // Delta §10: same prefetch attach as the serial
                    // branch. The index into `prefetches` is the call
                    // index, not the dispatch order.
                    if let Some(pf) = prefetches.get(&i) {
                        ctx.prefetched_read = Some(pf.clone());
                    }
                    async move {
                        let res = self
                            .tools
                            .execute_tool(&call.tool_name, &call.arguments, &ctx)
                            .await;
                        (res, start.elapsed().as_millis() as u64)
                    }
                })
                .collect();
            let done = futures::future::join_all(futs).await;
            for (slot_i, result) in run_indices.iter().zip(done) {
                slots[*slot_i] = Some(result);
            }
            slots
                .into_iter()
                .map(|s| {
                    s.unwrap_or_else(|| {
                        (
                            Ok(ToolResult::Error(
                                "internal: parallel branch left a slot unset".to_string(),
                            )),
                            0,
                        )
                    })
                })
                .collect()
        };

        // Delta §11.12: after the tool calls, check every call for
        // the mutating-tool trigger that fires an armed prewalk. The
        // engine splices the nudge, switches the model, and pushes
        // the checklist — once per prewalk.
        for call in calls.iter() {
            self.maybe_fire_prewalk(effective_holder, &call.tool_name)
                .await;
        }

        // Post-tool hooks (S10 phase 1).
        self.run_post_hooks(calls, hook_runner.as_ref(), &hook_denied)
            .await;

        // Session log (S10 phase 2).
        self.record_session_tool_calls(calls, &raw_results, effective_holder);

        // Tier 1.1 — every tool that ran escalates the round's
        // taint to the worse of the current level and the tool's
        // declared trust. The escalation is synchronous and cheap.
        for call in calls.iter() {
            if let Some(t) = self.tool_trust_level(&call.tool_name).await {
                self.escalate_taint(t);
            }
        }

        // Tier 2.1 — plan_update interception. The tool cannot reach
        // the engine's plan map, so we apply the update here and
        // replace whatever the tool returned.
        for (i, call) in calls.iter().enumerate() {
            if call.tool_name != "plan_update" {
                continue;
            }
            let update =
                serde_json::from_value::<kod_core_state::plan::PlanUpdate>(call.arguments.clone());
            let answer = match update {
                Ok(u) => self.apply_plan_update(effective_holder, u).await,
                Err(e) => format!("plan_update: invalid arguments: {e}"),
            };
            answers.insert(i, answer);
        }

        // P5.3 — semantic outcome classification for interesting

        // tool calls. Runs after the raw entries are written so the
        // syntactic trail is always on disk even if Jev is down.
        // Every call is a no-op when Jev is disabled.
        for (i, call) in calls.iter().enumerate() {
            let Some((result, ms)) = raw_results.get(i) else {
                continue;
            };
            // Only successful or errored calls are worth classifying;
            // a `RequiresConfirmation` never actually ran.
            let Ok(result) = result.as_ref() else {
                continue;
            };
            self.classify_tool_outcome_with_jev(effective_holder, call, result, *ms)
                .await;
        }

        // Diff augmentation (S10 phase 3).
        self.attach_write_diffs(calls, &snapshot_ids, &mut raw_results);

        let mut results = Vec::with_capacity(calls.len());
        let mut elapsed_ms = Vec::with_capacity(calls.len());
        let mut block = String::from("## Tool results\n");
        for (i, (call, (res, ms))) in calls.iter().zip(raw_results).enumerate() {
            elapsed_ms.push(ms);
            let result = match res {
                Ok(r) => r,
                Err(e) => ToolResult::Error(e.to_string()),
            };
            // ask_user: replace whatever the tool returned (an error
            // from its fallback) with the answer the user gave.
            let result = if let Some(answer) = answers.get(&i) {
                ToolResult::Success(serde_json::json!({ "answer": answer }))
            } else {
                result
            };
            // Cap for the prompt block the model sees. Byte count, not
            // tokens, but at the workspace's 4-chars-per-token rule of
            // thumb this is ≈2k tokens — comfortably under any model's
            // per-round budget once history, identity, and tools are
            // added on top.
            const RENDERED_RESULT_CAP: usize = 8_000;
            // P2.4 — for write_file / patch_file, ask Jev to
            // triage the diff's hunks before the prompt block is
            // built. `None` means "leave the diff alone" and the
            // original result flows through unchanged.
            let result_for_prompt: ToolResult =
                if matches!(call.tool_name.as_str(), "write_file" | "patch_file") {
                    self.filter_diff_hunks_with_jev(effective_holder, &result)
                        .await
                        .unwrap_or_else(|| result.clone())
                } else if call.tool_name == "read_file" {
                    // P2.2 — compress large read_file results by
                    // dropping lines Jev judges irrelevant. `None`
                    // means leave the original untouched.
                    self.compress_tool_result_with_jev(effective_holder, call, &result)
                        .await
                        .unwrap_or_else(|| result.clone())
                } else if matches!(call.tool_name.as_str(), "grep" | "search_files") {
                    // P2.3 — rank the search hits before the prompt
                    // block is built. `None` means the ranking did not
                    // run (disabled Jev, few hits, or Jev error); the
                    // original result flows through unchanged.
                    self.rank_search_results_with_jev(effective_holder, call, &result)
                        .await
                        .unwrap_or_else(|| result.clone())
                } else {
                    result.clone()
                };
            let result = result_for_prompt;
            let rendered = match &result {
                // list_files raw JSON is one quoted path per entry; a repo
                // with a target/ dir produces 40k+ entries and the model
                // sees a few KB of quoted paths ending in "[truncated
                // 1523k chars]" — no count, no sense of scale.
                // summarize_tool_result renders "4852 entries in src/:
                // · main.rs · lib.rs … and 4840 more", which is what the
                // model can actually reason about.
                ToolResult::Success(_) if call.tool_name == "list_files" => {
                    summarize_tool_result(&call.tool_name, &result)
                }
                // read_file and grep keep their structured payloads —
                // the model needs the actual content and (file, line,
                // text) tuples. cap_rendered_result trims the long
                // string fields *inside* the JSON rather than cutting
                // the serialized form mid-token, so the model always
                // gets parseable JSON with every metadata field
                // (path, line numbers, the `truncated` flag) intact.
                ToolResult::Success(_) => cap_rendered_result(
                    &result,
                    RENDERED_RESULT_CAP,
                    self.prompt_redactor_if_enabled(),
                ),
                ToolResult::Error(e) => format!("error: {e}"),
                ToolResult::RequiresConfirmation { description, .. } => {
                    format!("requires confirmation (auto-skipped in TUI): {description}")
                }
            };
            // Tier 1.1 — wrap the rendered result in a source-trust
            // marker so the model can tell tool output from its own
            // prior text. `run_tool_calls` does not carry the
            // definitions slice; look up the level by name.
            let trust = self
                .tool_trust_level(&call.tool_name)
                .await
                .unwrap_or(kod_types::trust::TrustLevel::ToolTrusted);
            block.push_str(&format!("\n### {} {}\n", call.tool_name, call.arguments));
            block.push_str(&trust.open_marker(&call.tool_name, None));
            block.push('\n');
            block.push_str(&rendered);
            block.push('\n');
            block.push_str(kod_types::trust::TrustLevel::close_marker());
            block.push('\n');
            results.push(result);
        }
        // Tier 3.5 — publish every file the round touched to the
        // blackboard so sibling agents can see it.
        for call in calls.iter() {
            let path = call.arguments.get("path").and_then(|v| v.as_str());
            if let Some(p) = path {
                self.note_file_seen(
                    effective_holder,
                    p,
                    &format!("{} by {}", call.tool_name, effective_holder),
                );
            }
        }
        // Tier 3.5.1: a successful write is harness-observed evidence
        // for whatever todo the model is working on. The model cannot
        // set its own confidence — this is where the harness raises
        // it, from work it actually saw land.
        //
        // Only when exactly one todo is `in_progress`: with two, the
        // evidence's owner is ambiguous, and attributing it to the
        // wrong one is worse than leaving both Speculative.
        {
            let wrote = calls.iter().zip(results.iter()).any(|(c, r)| {
                matches!(c.tool_name.as_str(), "write_file" | "patch_file")
                    && matches!(r, kod_types::ToolResult::Success(_))
            });
            if wrote && let Some(todo_id) = kod_tools::todo::in_progress_todo(&self.todo_list) {
                let files: Vec<&str> = calls
                    .iter()
                    .filter(|c| matches!(c.tool_name.as_str(), "write_file" | "patch_file"))
                    .filter_map(|c| c.arguments.get("path").and_then(|p| p.as_str()))
                    .collect();
                let note = if files.is_empty() {
                    "a write landed".to_string()
                } else {
                    format!("wrote {}", files.join(", "))
                };
                let _ = kod_tools::todo::note_evidence(
                    &self.todo_list,
                    todo_id,
                    note,
                    kod_tools::todo::ConfidenceState::Corroborated,
                );
            }
        }

        // Structured transcript slice (S10 phase 4).
        let messages = Self::build_round_messages(calls, &results);

        // Auto-check: when enabled, and at least one of the calls was
        // a successful write_file / patch_file, run the project's
        // compiler/linter and append its diagnostics to the prompt
        // block. The model sees breakage on the same turn as the write,
        // instead of having to ask for a check itself.
        //
        // Failures are silent except for a `tracing::debug!`: a
        // missing toolchain, an empty directory, or a timeout should
        // not make the write itself look like a problem.
        // Auto-check / auto-LSP diagnostics. The two flags are
        // independent:
        //
        //   - `auto_lsp` (default true): run the language server's
        //     diagnostics pass on the touched file and append a
        //     `## LSP diagnostics` block when the server returns any.
        //     Cheap (sub-second per file) and per-file.
        //   - `auto_check` (default false): run the project's
        //     compiler/linter and append `## Auto-check`. Thorough
        //     but slower and per-project.
        //
        // When both are on and the file's language has a server, LSP
        // runs first; an empty LSP response falls through to the
        // compiler (which disambiguates "clean" from "unreachable").
        // When only `auto_lsp` is on and LSP is empty or unavailable,
        // no block is emitted — the compiler is not consulted on the
        // user's behalf.
        if self.auto_check_setting() || self.auto_lsp_setting() {
            // Every successful write in this round. We read the file
            // from disk rather than trusting the call's `content`
            // argument: `patch_file` does not carry one, and a
            // `write_file` may have been transformed by a hook.
            let writes: Vec<(std::path::PathBuf, String)> = calls
                .iter()
                .zip(results.iter())
                .filter_map(|(c, r)| {
                    if !matches!(c.tool_name.as_str(), "write_file" | "patch_file") {
                        return None;
                    }
                    if !matches!(r, ToolResult::Success(_)) {
                        return None;
                    }
                    let p = c.arguments.get("path").and_then(|v| v.as_str())?;
                    let abs = if std::path::Path::new(p).is_absolute() {
                        std::path::PathBuf::from(p)
                    } else {
                        tool_context.working_dir.join(p)
                    };
                    let content = std::fs::read_to_string(&abs).unwrap_or_default();
                    Some((abs, content))
                })
                .collect();

            if !writes.is_empty() {
                // `auto_lsp` on the engine is the per-session override
                // the CLI/TUI apply; the config's
                // `[lsp] auto_diagnostics` is the persistent choice. The
                // pass runs only when both are true — a user who set
                // either to false has asked not to be charged the
                // per-write diagnostics cost.
                let lsp_config_auto = kod_config::KodConfig::load_cached()
                    .ok()
                    .map(|c| c.lsp.auto_diagnostics)
                    .unwrap_or(true);
                let lsp_wanted = self.auto_lsp_setting() && lsp_config_auto;
                let compiler_wanted = self.auto_check_setting();
                // Delta 7.2 batch mode: a round that writes several
                // files runs the LSP pass on the *last* one (the
                // design's "flush on the last write of a call") rather
                // than on no file at all. The model gets feedback on
                // the file it touched most recently; running a full
                // settle per file would cost N times the wait.
                let last_write = writes.last().expect("writes is non-empty");
                let batch = writes.len() > 1;
                let lsp_eligible = lsp_wanted && Self::lsp_binary_for(&last_write.0).is_some();

                let diags: Vec<kod_tools::check::Diagnostic>;
                let source: String;
                let used_lsp: bool;

                if lsp_eligible {
                    let (path, content) = last_write;
                    let binary = Self::lsp_binary_for(path).unwrap_or("lsp");
                    // `settle_ms` from `[lsp]` bounds how long we
                    // wait for the server to publish before accepting
                    // an empty answer as final. Read from the same
                    // config load used for `auto_diagnostics` above;
                    // default 1500 ms if the config is unreadable.
                    let settle_ms = kod_config::KodConfig::load_cached()
                        .ok()
                        .map(|c| c.lsp.settle_ms)
                        .unwrap_or(500);
                    // Batch rounds take a shorter inline wait: the
                    // model wants a fast answer on the last file, not
                    // a thorough one. Deferred diagnostics still cover
                    // a slow server.
                    let settle_ms = if batch { settle_ms.min(400) } else { settle_ms };
                    let lsp_diags = self
                        .lsp_diagnostics(path, content, std::time::Duration::from_millis(settle_ms))
                        .await;
                    // Delta 7.2: when the short inline wait came up
                    // empty, a slow server's answer is not lost — a
                    // background pass keeps watching for
                    // deferred_settle_ms and queues anything it sees.
                    // prepare_turn drains the queue into the next
                    // turn's prompt. Guarded by an mtime check so a
                    // rewrite during the wait does not queue stale
                    // diagnostics.
                    if lsp_diags.is_empty() {
                        let (deferred_enabled, deferred_ms) = kod_config::KodConfig::load_cached()
                            .ok()
                            .map(|c| (c.lsp.deferred_enabled, c.lsp.deferred_settle_ms))
                            .unwrap_or((true, 12_000));
                        if deferred_enabled && deferred_ms > 0 {
                            let mtime_at_spawn =
                                std::fs::metadata(path).and_then(|m| m.modified()).ok();
                            let mgr = std::sync::Arc::clone(&self.lsp_manager);
                            let q = std::sync::Arc::clone(&self.deferred_diagnostics);
                            let holder_owned = effective_holder.to_string();
                            let path_owned = path.clone();
                            let content_owned = content.to_string();
                            tokio::spawn(async move {
                                let later = mgr
                                    .diagnostics(
                                        &path_owned,
                                        &content_owned,
                                        std::time::Duration::from_millis(deferred_ms),
                                    )
                                    .await;
                                if later.is_empty() {
                                    return;
                                }
                                let now_mtime = std::fs::metadata(&path_owned)
                                    .and_then(|m| m.modified())
                                    .ok();
                                if now_mtime != mtime_at_spawn {
                                    tracing::debug!(
                                        path = %path_owned.display(),
                                        "deferred LSP: file changed during wait; discarding",
                                    );
                                    return;
                                }
                                let converted: Vec<kod_tools::check::Diagnostic> = later
                                    .iter()
                                    .map(|d| kod_tools::check::Diagnostic {
                                        file: d.file.clone(),
                                        line: d.line,
                                        column: d.column,
                                        severity: d.severity.clone(),
                                        code: d.code.clone(),
                                        message: d.message.clone(),
                                    })
                                    .collect();
                                let added = q.push(&holder_owned, &converted);
                                if added > 0 {
                                    tracing::info!(
                                        holder = %holder_owned,
                                        added,
                                        "deferred LSP diagnostics queued",
                                    );
                                }
                            });
                        }
                    }
                    if !lsp_diags.is_empty() {
                        diags = lsp_diags
                            .iter()
                            .map(|d| kod_tools::check::Diagnostic {
                                file: d.file.clone(),
                                line: d.line,
                                column: d.column,
                                severity: d.severity.clone(),
                                code: d.code.clone(),
                                message: d.message.clone(),
                            })
                            .collect();
                        source = binary.to_string();
                        used_lsp = true;
                    } else if compiler_wanted {
                        // Empty LSP answer: could mean "clean" or
                        // "unreachable". Fall through to the
                        // compiler, which disambiguates.
                        // H-E13: run the check against the transcript's working
                        // directory, not the engine's. A swarm agent
                        // writing in its worktree was getting
                        // diagnostics (and baseline overwrites) from
                        // the main repo — the model saw errors it did
                        // not introduce.
                        match kod_tools::CheckTool::run_check(
                            &tool_context.working_dir,
                            tool_context.timeout_secs,
                        )
                        .await
                        {
                            Ok(outcome) => {
                                source = outcome.command.clone();
                                diags = outcome.diagnostics;
                                used_lsp = false;
                            }
                            Err(e) => {
                                tracing::debug!(
                                    error = %e,
                                    "auto-check compiler fallback did not run"
                                );
                                return ToolRound {
                                    results,
                                    prompt_block: block,
                                    elapsed_ms,
                                    messages: messages.clone(),
                                };
                            }
                        }
                    } else {
                        // Only auto_lsp is on and LSP returned empty:
                        // either the server found nothing or is
                        // unreachable, and without compiler
                        // confirmation we cannot say which. Emit no
                        // block rather than inventing one.
                        return ToolRound {
                            results,
                            prompt_block: block,
                            elapsed_ms,
                            // Auto-check error path: no structured transcript slice.
                            messages: messages.clone(),
                        };
                    }
                } else if compiler_wanted {
                    // H-E13: run the check against the transcript's working
                    // directory, not the engine's. A swarm agent
                    // writing in its worktree was getting
                    // diagnostics (and baseline overwrites) from
                    // the main repo — the model saw errors it did
                    // not introduce.
                    match kod_tools::CheckTool::run_check(
                        &tool_context.working_dir,
                        tool_context.timeout_secs,
                    )
                    .await
                    {
                        Ok(outcome) => {
                            source = outcome.command.clone();
                            diags = outcome.diagnostics;
                            used_lsp = false;
                        }
                        Err(e) => {
                            tracing::debug!(
                                error = %e,
                                "auto-check compiler did not run"
                            );
                            return ToolRound {
                                results,
                                prompt_block: block,
                                elapsed_ms,
                                // Auto-check error path: no structured transcript slice.
                                messages: messages.clone(),
                            };
                        }
                    }
                } else {
                    // LSP was wanted but not eligible (no server for
                    // this file's language, or more than one file in
                    // the round), and the compiler is off. Nothing to
                    // emit — the user has chosen LSP-only.
                    return ToolRound {
                        results,
                        prompt_block: block,
                        elapsed_ms,
                        // Auto-check error path: no structured transcript slice.
                        messages: messages.clone(),
                    };
                }

                // LSP diagnostics are per-file and live from the
                // server; the baseline diff (a per-project compiler
                // snapshot) does not apply. Emit them verbatim under
                // their own header.
                if used_lsp {
                    block.push_str("\n## LSP diagnostics\n\n");
                    if diags.is_empty() {
                        block.push_str(&format!("`{}`: no diagnostics on this file.\n", source,));
                    } else {
                        block.push_str(&format!(
                            "`{}` reported {} diagnostic(s) on this file:\n\n",
                            source,
                            diags.len(),
                        ));
                        render_diagnostics(&mut block, &diags, 20);
                    }
                } else {
                    // Compiler diagnostics keep the baseline diff so
                    // the model sees *new* errors, not pre-existing
                    // ones.
                    let baseline = self.check_baseline.read().await.clone();
                    let baseline_keys: std::collections::HashSet<(String, Option<String>, String)> =
                        baseline
                            .as_ref()
                            .map(|v| v.iter().map(diag_key).collect())
                            .unwrap_or_default();
                    let current_keys: std::collections::HashSet<(String, Option<String>, String)> =
                        diags.iter().map(diag_key).collect();

                    let syntactically_new: Vec<&kod_tools::check::Diagnostic> = diags
                        .iter()
                        .filter(|d| !baseline_keys.contains(&diag_key(d)))
                        .collect();
                    // P4.4 — ask Jev which of these are genuinely
                    // new versus shifted copies of a baseline
                    // diagnostic. Falls through to "all new" when
                    // Jev is disabled or errors, so the pre-Jev
                    // behaviour is the fallback.
                    let new_diags: Vec<&kod_tools::check::Diagnostic> =
                        if let Some(b) = baseline.as_ref() {
                            let keep = self
                                .classify_new_diagnostics_with_jev(
                                    effective_holder,
                                    &syntactically_new,
                                    b,
                                )
                                .await;
                            syntactically_new
                                .into_iter()
                                .enumerate()
                                .filter(|(i, _)| keep.contains(i))
                                .map(|(_, d)| d)
                                .collect()
                        } else {
                            syntactically_new
                        };
                    let resolved_count = if baseline.is_some() {
                        baseline_keys
                            .iter()
                            .filter(|k| !current_keys.contains(*k))
                            .count()
                    } else {
                        0
                    };

                    block.push_str("\n## Auto-check\n\n");
                    match baseline {
                        None => {
                            if diags.is_empty() {
                                block.push_str(&format!("`{}` is clean.\n", source));
                            } else {
                                block.push_str(&format!(
                                    "`{}` reported {} diagnostic(s). \
                                     (No baseline was captured, so all are shown — \
                                     some may pre-date this write.)\n\n",
                                    source,
                                    diags.len(),
                                ));
                                render_diagnostics(&mut block, &diags, 20);
                            }
                        }
                        Some(_) if !new_diags.is_empty() => {
                            block.push_str(&format!(
                                "`{}` reported {} NEW diagnostic(s) from this write:\n\n",
                                source,
                                new_diags.len(),
                            ));
                            let owned: Vec<kod_tools::check::Diagnostic> =
                                new_diags.iter().map(|d| (*d).clone()).collect();
                            render_diagnostics(&mut block, &owned, 20);
                            if resolved_count > 0 {
                                block.push_str(&format!(
                                    "\n({} pre-existing diagnostic(s) resolved.)\n",
                                    resolved_count,
                                ));
                            }
                            block.push_str("\nFix the new errors before continuing.\n");
                        }
                        Some(_) if resolved_count > 0 => {
                            block.push_str(&format!(
                                "`{}`: your write introduced no new errors and \
                                 resolved {} pre-existing diagnostic(s).\n",
                                source, resolved_count,
                            ));
                        }
                        Some(_) if diags.is_empty() => {
                            block.push_str(&format!("`{}` is clean.\n", source));
                        }
                        Some(_) => {
                            block.push_str(&format!(
                                "`{}`: your write introduced no new errors. \
                                 {} pre-existing diagnostic(s) remain, \
                                 unrelated to this change.\n",
                                source,
                                diags.len(),
                            ));
                        }
                    }

                    // One `SessionEntry::Diagnostics` per file (AD-15).
                    // The counts aggregate the round's diagnostics by
                    // file, so a multi-file compiler pass yields one
                    // entry per file rather than a single project-wide
                    // lump.
                    if let Ok(guard) = self.session_recorder.read()
                        && let Some(rec) = guard.as_ref()
                    {
                        use std::collections::BTreeMap;
                        let mut per_file: BTreeMap<String, (usize, usize)> = BTreeMap::new();
                        for d in &diags {
                            let entry = per_file.entry(d.file.clone()).or_insert((0, 0));
                            match d.severity.as_str() {
                                "error" => entry.0 += 1,
                                "warning" => entry.1 += 1,
                                _ => {}
                            }
                        }
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        for (file, (errs, warns)) in per_file {
                            let entry = kod_core_state::session_log::SessionEntry::Diagnostics {
                                timestamp_ms: now_ms,
                                file,
                                error_count: errs,
                                warning_count: warns,
                            };
                            let _ = rec.record(&entry);
                        }
                    }

                    // The post-write state becomes the new baseline.
                    *self.check_baseline.write().await = Some(diags);
                }
            }
        }

        // MemoryWrite audit for the tool channel (AD-15). Every
        // successful `memory_save` writes one entry, with the id and
        // tags the tool returned. The extraction path logs under the
        // "extraction" channel; the user's `/remember` under "user".
        if let Ok(guard) = self.session_recorder.read()
            && let Some(rec) = guard.as_ref()
        {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            for (call, result) in calls.iter().zip(results.iter()) {
                if call.tool_name != "memory_save" {
                    continue;
                }
                let ToolResult::Success(v) = result else {
                    continue;
                };
                let Some(id) = v.get("id").and_then(|v| v.as_str()) else {
                    continue;
                };
                let tags: Vec<String> = v
                    .get("tags")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect()
                    })
                    .unwrap_or_default();
                let entry = kod_core_state::session_log::SessionEntry::MemoryWrite {
                    timestamp_ms: now_ms,
                    memory_id: id.to_string(),
                    channel: "tool".to_string(),
                    tags,
                };
                let _ = rec.record(&entry);
            }
        }

        ToolRound {
            results,
            prompt_block: block,
            elapsed_ms,
            messages,
        }
    }
}
