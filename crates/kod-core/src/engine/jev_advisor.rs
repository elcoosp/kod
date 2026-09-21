//! Jev advisor helpers for [`super::KodEngine`].
//!
//! The `*_with_jev` family used to live inline in `engine/mod.rs`.
//! Moving each one here keeps the entire Jev dependency surface —
//! the network client, the decision log, the request lookup — in one
//! file. As a submodule of `engine`, this file has access to
//! `KodEngine`'s private fields and private helper methods; no
//! visibility changes are required on the parent.

use super::KodEngine;
use super::{format_call_brief, question_marker, AWAIT_APPROVAL_SECS, DEFAULT_TRANSCRIPT_KEY};
use kod_provider::ModelRef;
use kod_types::{ToolCall, ToolDefinition, ToolResult};

impl KodEngine {
    pub async fn classify_chunk_with_jev(&self, holder: &str, buffer_tail: &str) -> Option<String> {
        let jev = self.jev_client()?;
        let request = self.current_request(holder).await.unwrap_or_default();
        let state = crate::jev::build_state(
            &format!(
                "User request: {request}\n\nRecent buffer: {}",
                crate::jev::preview_chars(buffer_tail, 500),
            ),
            &[],
        );
        let labels = &["prose_answer", "reasoning", "restatement", "code_block"];
        let started = std::time::Instant::now();
        let decision = jev
            .evaluate_score(&state, "What kind of text is this streamed chunk?", labels)
            .await
            .ok()?;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        self.log_jev_decision(
            holder,
            "chunk_classify",
            &crate::jev::preview_chars(buffer_tail, 200),
            "chunk_kind",
            serde_json::json!({ "kind": decision.value }),
            decision.confidence,
            elapsed_ms,
            false,
            crate::jev::DecisionSource::Jev,
        );
        Some(decision.value)
    }

    pub(crate) async fn extract_decisions_with_jev(
        &self,
        key: &str,
        turn_id: u64,
        input: &str,
        reply: &str,
    ) -> usize {
        let Some(jev) = self.jev_client() else {
            return 0;
        };
        if input.len() < 30 || reply.len() < 30 {
            return 0;
        }
        let state = crate::jev::build_state(
            &format!(
                "User request: {}\n\nAssistant reply: {}",
                crate::jev::preview_chars(input, 400),
                crate::jev::preview_chars(reply, 400),
            ),
            &[],
        );
        let pairs = [(
            "is_decision".to_string(),
            "Does this exchange contain a durable decision, preference, \
             or constraint worth remembering for future turns? A one-off \
             factual answer or a plain status update does not count."
                .to_string(),
        )];
        let started = std::time::Instant::now();
        let rows = match jev.evaluate_yes_no_batch(&state, &pairs).await {
            Ok(r) => r,
            Err(_) => return 0,
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let p = rows.first().map(|(_, p)| *p).unwrap_or(0.0);
        if p < 0.7 {
            return 0;
        }
        // Kind classification.
        let labels = &[
            "user_preference",
            "approach",
            "file_change",
            "constraint",
            "other",
        ];
        let kind_decision = jev
            .evaluate_score(&state, "What kind of durable decision is this?", labels)
            .await
            .ok();
        let kind = match kind_decision.as_ref().map(|d| d.value.as_str()) {
            Some("user_preference") => crate::decisions::DecisionKind::UserPreference,
            Some("approach") => crate::decisions::DecisionKind::Approach,
            Some("file_change") => crate::decisions::DecisionKind::FileChange,
            Some("constraint") => crate::decisions::DecisionKind::Constraint,
            _ => crate::decisions::DecisionKind::Other,
        };
        let text = format!(
            "{} → {}",
            crate::jev::preview_chars(input, 150),
            crate::jev::preview_chars(reply, 150),
        );
        self.log_jev_decision(
            key,
            "decision_extract",
            &crate::jev::preview_chars(input, 200),
            "is_decision,kind",
            serde_json::json!({
                "is_decision": p,
                "kind": kind_decision.as_ref().map(|d| d.value.clone()),
            }),
            p,
            elapsed_ms,
            false,
            crate::jev::DecisionSource::Jev,
        );
        self.add_decision(
            key,
            turn_id,
            kind,
            text,
            crate::decisions::DecisionAuthor::Assistant,
        )
        .await;
        1
    }

    pub(crate) async fn rank_skills_with_jev(
        &self,
        holder: &str,
        input: &str,
        router_match: &[String],
    ) -> Option<Vec<String>> {
        const MAX_SKILLS: usize = 30;
        let jev = self.jev_client()?;
        let all = self.loaded_skill_details().await;
        if all.is_empty() {
            return None;
        }
        let slice: Vec<(String, String)> = all.into_iter().take(MAX_SKILLS).collect();
        let questions: Vec<(String, String)> = slice
            .iter()
            .enumerate()
            .map(|(i, (name, desc))| {
                (
                    format!("skill_{i}"),
                    format!(
                        "Is this skill relevant to the user's request? Skill: {name} — {}",
                        crate::jev::preview_chars(desc, 300),
                    ),
                )
            })
            .collect();
        let state = crate::jev::build_state(input, &[]);
        let started = std::time::Instant::now();
        let rows = jev.evaluate_yes_no_batch(&state, &questions).await.ok()?;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let threshold = jev.thresholds().memory_filter_min;

        let mut merged: Vec<String> = router_match.to_vec();
        let mut answers = serde_json::Map::new();
        for (i, (name, _)) in slice.iter().enumerate() {
            let p = rows
                .iter()
                .find(|(id, _)| id == &format!("skill_{i}"))
                .map(|(_, p)| *p)
                .unwrap_or(0.0);
            answers.insert(format!("skill_{i}"), serde_json::json!(p));
            if p >= threshold && !merged.contains(name) {
                merged.push(name.clone());
            }
        }
        self.log_jev_decision(
            holder,
            "skill_match",
            input,
            "relevance_per_skill",
            serde_json::Value::Object(answers),
            1.0,
            elapsed_ms,
            false,
            crate::jev::DecisionSource::Jev,
        );
        Some(merged)
    }

    pub(crate) async fn rank_search_results_with_jev(
        &self,
        holder: &str,
        call: &ToolCall,
        result: &ToolResult,
    ) -> Option<ToolResult> {
        const MIN_RESULTS_TO_RANK: usize = 20;
        const MAX_RESULTS_TO_RANK: usize = 60;
        let jev = self.jev_client()?;
        let request = self.current_request(holder).await?;
        let ToolResult::Success(v) = result else {
            return None;
        };
        let arr = v.get("results").and_then(|r| r.as_array())?;
        if arr.len() < MIN_RESULTS_TO_RANK {
            return None;
        }
        let slice: Vec<&serde_json::Value> = arr.iter().take(MAX_RESULTS_TO_RANK).collect();

        // One yes/no per hit; the "question" carries the search line.
        let questions: Vec<(String, String)> = slice
            .iter()
            .enumerate()
            .map(|(i, hit)| {
                let line = hit
                    .get("text")
                    .or_else(|| hit.get("line"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                let path = hit
                    .get("file")
                    .or_else(|| hit.get("path"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                (
                    format!("hit_{i}"),
                    format!(
                        "Is this {} result relevant to the request? \"{}\" in {}: {}",
                        call.tool_name,
                        request,
                        path,
                        crate::jev::preview_chars(line, 200),
                    ),
                )
            })
            .collect();

        let state = crate::jev::build_state(&request, &[]);
        let started = std::time::Instant::now();
        let rows = jev.evaluate_yes_no_batch(&state, &questions).await.ok()?;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let threshold = jev.thresholds().memory_filter_min;

        let mut keep_idx: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut answers = serde_json::Map::new();
        for (i, hit) in slice.iter().enumerate() {
            let p = rows
                .iter()
                .find(|(id, _)| id == &format!("hit_{i}"))
                .map(|(_, p)| *p)
                .unwrap_or(1.0);
            answers.insert(format!("hit_{i}"), serde_json::json!(p));
            if p >= threshold {
                keep_idx.insert(i);
            }
            let _ = hit;
        }
        // Keep everything beyond the classification window.
        for i in slice.len()..arr.len() {
            keep_idx.insert(i);
        }
        let dropped = arr.len() - keep_idx.len();
        if dropped == 0 {
            return None;
        }
        self.log_jev_decision(
            holder,
            "search_rank",
            &crate::jev::preview_chars(&request, 200),
            &format!("{} hits", arr.len()),
            serde_json::Value::Object(answers),
            1.0,
            elapsed_ms,
            false,
            crate::jev::DecisionSource::Jev,
        );

        let filtered: Vec<serde_json::Value> = arr
            .iter()
            .enumerate()
            .filter(|(i, _)| keep_idx.contains(i))
            .map(|(_, v)| v.clone())
            .collect();
        let mut new_v = v.clone();
        if let Some(obj) = new_v.as_object_mut() {
            obj.insert("results".to_string(), serde_json::Value::Array(filtered));
            obj.insert(
                "ranked_by_jev".to_string(),
                serde_json::json!({
                    "dropped": dropped,
                    "kept": keep_idx.len(),
                }),
            );
        }
        Some(ToolResult::Success(new_v))
    }

    pub(crate) async fn filter_diff_hunks_with_jev(
        &self,
        holder: &str,
        result: &ToolResult,
    ) -> Option<ToolResult> {
        let jev = self.jev_client()?;
        let request = self.current_request(holder).await?;
        let ToolResult::Success(v) = result else {
            return None;
        };
        let diff = v.get("diff").and_then(|d| d.as_str())?;
        if diff.is_empty() {
            return None;
        }
        // Split into header + hunks. The header is everything before
        // the first `@@ ` line.
        let mut lines = diff.lines();
        let mut header_lines: Vec<&str> = Vec::new();
        let mut hunks: Vec<Vec<&str>> = Vec::new();
        let mut cur: Vec<&str> = Vec::new();
        let mut seen_hunk = false;
        for line in lines.by_ref() {
            if line.starts_with("@@") {
                if !cur.is_empty() {
                    hunks.push(std::mem::take(&mut cur));
                }
                cur.push(line);
                seen_hunk = true;
            } else if seen_hunk {
                cur.push(line);
            } else {
                header_lines.push(line);
            }
        }
        if !cur.is_empty() {
            hunks.push(cur);
        }
        if hunks.len() < 2 {
            return None;
        }

        // One score per hunk, in a single batch of yes/no for the
        // binary "is this context-only vs relevant?".
        const MAX_HUNKS: usize = 20;
        let evaluated = hunks.iter().take(MAX_HUNKS);
        let questions: Vec<(String, String)> = evaluated
            .enumerate()
            .map(|(i, h)| {
                (
                    format!("hunk_{i}"),
                    format!(
                        "Is this diff hunk relevant to the user's request, or context-only? Request: {request}. Hunk:\n{}",
                        h.iter().take(30).copied().collect::<Vec<_>>().join("\n"),
                    ),
                )
            })
            .collect();
        let state = crate::jev::build_state(&request, &[]);
        let started = std::time::Instant::now();
        let rows = jev.evaluate_yes_no_batch(&state, &questions).await.ok()?;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let threshold = jev.thresholds().memory_filter_min;
        let mut elided = 0_usize;
        let mut kept_hunks: Vec<&Vec<&str>> = Vec::new();
        let mut answers = serde_json::Map::new();
        for (i, h) in hunks.iter().enumerate() {
            if i >= MAX_HUNKS {
                // Past the cap: keep, do not classify.
                kept_hunks.push(h);
                continue;
            }
            let p = rows
                .iter()
                .find(|(id, _)| id == &format!("hunk_{i}"))
                .map(|(_, p)| *p)
                .unwrap_or(1.0);
            answers.insert(format!("hunk_{i}"), serde_json::json!(p));
            if p < threshold {
                elided += h.len();
            } else {
                kept_hunks.push(h);
            }
        }
        if elided == 0 {
            // Nothing to compress — leave the result untouched.
            return None;
        }

        self.log_jev_decision(
            holder,
            "diff_triage",
            &crate::jev::preview_chars(diff, 200),
            &format!("{} hunks", hunks.len()),
            serde_json::Value::Object(answers),
            1.0,
            elapsed_ms,
            false,
            crate::jev::DecisionSource::Jev,
        );

        let mut filtered = header_lines.join("\n");
        for h in kept_hunks {
            filtered.push('\n');
            filtered.push_str(&h.join("\n"));
        }
        if elided > 0 {
            filtered.push_str(&format!(
                "\n… ({} lines of context-only hunks elided by Jev)",
                elided,
            ));
        }

        // Rebuild the result with the filtered diff. The TUI still
        // sees the original via the raw results map — only the
        // prompt block the model reads goes through here.
        let mut new_v = v.clone();
        if let Some(obj) = new_v.as_object_mut() {
            obj.insert("diff".to_string(), serde_json::Value::String(filtered));
        }
        Some(ToolResult::Success(new_v))
    }

    pub(crate) async fn classify_tool_outcome_with_jev(
        &self,
        holder: &str,
        call: &ToolCall,
        result: &ToolResult,
        duration_ms: u64,
    ) {
        const MIN_JEV_CLASSIFY_MS: u64 = 100;
        let failed = matches!(result, ToolResult::Error(_));
        if duration_ms < MIN_JEV_CLASSIFY_MS && !failed {
            return;
        }
        let Some(jev) = self.jev_client() else {
            return;
        };
        let Some(request_text) = self.current_request(holder).await else {
            return;
        };
        let result_text = match result {
            ToolResult::Success(v) => serde_json::to_string(v).unwrap_or_default(),
            ToolResult::Error(e) => format!("ERROR: {e}"),
            _ => String::new(),
        };
        let state = crate::jev::build_state(
            &format!(
                "User request: {request_text}\nTool: {}\nArguments: {}\nDuration: {duration_ms}ms\nResult: {}",
                call.tool_name,
                serde_json::to_string(&call.arguments).unwrap_or_default(),
                crate::jev::preview_chars(&result_text, 800),
            ),
            &[],
        );
        let outcome_labels = &["success", "partial", "failure", "irrelevant"];
        let impact_labels = &["none", "minor", "significant", "critical"];
        let started = std::time::Instant::now();
        let outcome = jev
            .evaluate_score(
                &state,
                "What was the outcome of this tool call?",
                outcome_labels,
            )
            .await;
        let impact = jev
            .evaluate_score(
                &state,
                "How significant is this tool call's impact on the user's task?",
                impact_labels,
            )
            .await;
        let elapsed_ms = started.elapsed().as_millis() as u64;

        let (outcome_label, impact_label, confidence, source) = match (outcome, impact) {
            (Ok(o), Ok(i)) => (
                o.value,
                i.value,
                o.confidence.max(i.confidence),
                crate::jev::DecisionSource::Jev,
            ),
            _ => (
                if failed {
                    "failure".to_string()
                } else {
                    "success".to_string()
                },
                "minor".to_string(),
                1.0,
                crate::jev::DecisionSource::Heuristic,
            ),
        };

        if let Ok(guard) = self.session_recorder.read()
            && let Some(rec) = guard.as_ref()
        {
            let entry = crate::session_log::SessionEntry::ToolOutcome {
                timestamp_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
                holder: holder.to_string(),
                tool_name: call.tool_name.clone(),
                outcome: outcome_label,
                user_visible_impact: impact_label,
                confidence,
                latency_ms: elapsed_ms,
                source: source.as_str().to_string(),
            };
            let _ = rec.record(&entry);
        }
    }

    pub(crate) async fn check_response_quality_with_jev(
        &self,
        key: &str,
        request: &str,
        response: &str,
    ) -> Option<String> {
        if response.len() < 200 {
            return None;
        }
        let jev = self.jev_client()?;
        let state = crate::jev::build_state(
            &format!("User request: {request}\n\nAssistant response: {response}"),
            &[],
        );
        let pairs = [(
            "answers_the_question".to_string(),
            "Does the assistant's response answer the user's request?".to_string(),
        )];
        let started = std::time::Instant::now();
        let result = jev.evaluate_yes_no_batch(&state, &pairs).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let (p_yes, source) = match result {
            Ok(rows) => (
                rows.first().map(|(_, p)| *p).unwrap_or(1.0),
                crate::jev::DecisionSource::Jev,
            ),
            Err(_) => (1.0, crate::jev::DecisionSource::Heuristic),
        };
        self.log_jev_decision(
            key,
            "quality_gate",
            &crate::jev::preview_chars(request, 200),
            "answers_the_question",
            serde_json::json!({ "answers_the_question": p_yes }),
            p_yes,
            elapsed_ms,
            false,
            source,
        );
        if p_yes < 0.5 {
            Some(format!(
                "\n\n---\n\n*Note: Jev flagged this reply as possibly off-track                  (confidence the response answers the request: {:.0}%). If it missed                  the point, /regenerate to try again.*",
                p_yes * 100.0,
            ))
        } else {
            None
        }
    }

    pub(crate) async fn classify_new_diagnostics_with_jev(
        &self,
        key: &str,
        new_diags: &[&kod_tools::check::Diagnostic],
        baseline: &[kod_tools::check::Diagnostic],
    ) -> std::collections::HashSet<usize> {
        let all: std::collections::HashSet<usize> = (0..new_diags.len()).collect();
        let Some(jev) = self.jev_client() else {
            return all;
        };
        if new_diags.is_empty() {
            return all;
        }
        // One state carrying the new diagnostics and a compact
        // rendering of the baseline. The baseline is bounded so a
        // project with hundreds of pre-existing diagnostics does
        // not produce a pathological request.
        let baseline_preview: String = baseline
            .iter()
            .take(40)
            .map(|d| {
                format!(
                    "{}:{} [{}] {}",
                    d.file,
                    d.line,
                    d.code.as_deref().unwrap_or("?"),
                    d.message
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let current_preview: String = new_diags
            .iter()
            .enumerate()
            .map(|(i, d)| {
                format!(
                    "[{i}] {}:{} [{}] {}",
                    d.file,
                    d.line,
                    d.code.as_deref().unwrap_or("?"),
                    d.message
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let state = crate::jev::build_state(
            &format!(
                "Baseline diagnostics (before the write):\n{baseline_preview}\n\nCurrent diagnostics (after the write):\n{current_preview}"
            ),
            &[],
        );
        // One yes/no per entry: is this genuinely new, or a shifted
        // duplicate of one in the baseline?
        let questions: Vec<(String, String)> = new_diags
            .iter()
            .enumerate()
            .map(|(i, d)| {
                (
                    format!("diag_{i}"),
                    format!(
                        "Is diagnostic [{i}] ({file}:{line} [{code}] {msg}) genuinely new, or the same as one of the baseline diagnostics just at a different line?",
                        i = i,
                        file = d.file,
                        line = d.line,
                        code = d.code.as_deref().unwrap_or("?"),
                        msg = d.message,
                    ),
                )
            })
            .collect();
        let started = std::time::Instant::now();
        let result = jev.evaluate_yes_no_batch(&state, &questions).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let (rows, source) = match result {
            Ok(r) => (r, crate::jev::DecisionSource::Jev),
            Err(_) => (Vec::new(), crate::jev::DecisionSource::Heuristic),
        };
        let threshold = jev.thresholds().task_classify_min;
        // H-E12: seed `truly_new` with every index, then *remove*
        // the ones Jev explicitly scored below threshold. The pre-fix
        // code built it from the returned rows only, so a batch that
        // answered 1 of 10 hid 9 genuinely-new compiler errors — a
        // fail-closed default where the module doc promised
        // fail-open. Any id Jev did not answer stays "new".
        let mut truly_new: std::collections::HashSet<usize> = all.clone();
        let mut answers = serde_json::Map::new();
        for (id, p) in &rows {
            answers.insert(id.clone(), serde_json::json!(p));
            if let Some(rest) = id.strip_prefix("diag_")
                && let Ok(idx) = rest.parse::<usize>()
                && *p < threshold
            {
                truly_new.remove(&idx);
            }
        }
        if rows.is_empty() {
            return all;
        }
        self.log_jev_decision(
            key,
            "diagnostic_triage",
            &format!("{} new diagnostics", new_diags.len()),
            "genuinely_new",
            serde_json::Value::Object(answers),
            1.0,
            elapsed_ms,
            false,
            source,
        );
        truly_new
    }

    pub(crate) async fn augment_input_with_jev_ambiguity_check(
        &self,
        key: &str,
        input: &str,
        chunk_tx: Option<&tokio::sync::mpsc::Sender<String>>,
    ) -> String {
        if input.len() < 10 {
            return input.to_string();
        }
        let Some(jev) = self.jev_client() else {
            return input.to_string();
        };
        let state = crate::jev::build_state(input, &[]);
        let pairs = [(
            "is_ambiguous".to_string(),
            "Is this request ambiguous without more context?".to_string(),
        )];
        let started = std::time::Instant::now();
        let result = jev.evaluate_yes_no_batch(&state, &pairs).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let (p_amb, source) = match result {
            Ok(rows) => (
                rows.first().map(|(_, p)| *p).unwrap_or(0.0),
                crate::jev::DecisionSource::Jev,
            ),
            Err(_) => (0.0, crate::jev::DecisionSource::Heuristic),
        };
        self.log_jev_decision(
            key,
            "ambiguity",
            input,
            "is_ambiguous",
            serde_json::json!({ "is_ambiguous": p_amb }),
            p_amb,
            elapsed_ms,
            false,
            source,
        );
        let threshold = jev.thresholds().ambiguity_min;
        if p_amb < threshold {
            return input.to_string();
        }
        // Non-streaming callers cannot ask the user — the log entry
        // above is the only side effect for them.
        let Some(tx) = chunk_tx else {
            return input.to_string();
        };
        let question_text = format!(
            "Your request may be ambiguous. Add any missing context, \
             or press Enter to continue as-is:\n\n> {input}"
        );
        let req = kod_tools::ask::QuestionRequest {
            question: question_text,
            placeholder: Some("(clarification)".to_string()),
        };
        let json = serde_json::to_string(&req).unwrap_or_else(|_| "{}".to_string());
        let id = self
            .next_question_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (otx, orx) = tokio::sync::oneshot::channel();
        self.pending_questions.write().await.insert(id, otx);
        let _ = tx.send(question_marker(id, &json)).await;
        let answer =
            tokio::time::timeout(std::time::Duration::from_secs(AWAIT_APPROVAL_SECS), orx).await;
        match answer {
            Ok(Ok(text))
                if !text.is_empty() && text != "(cancelled)" && text != "(question cancelled)" =>
            {
                format!("{input}\n\nAdditional clarification from user: {text}")
            }
            _ => input.to_string(),
        }
    }

    pub(crate) async fn group_approvals_with_jev(
        &self,
        holder: &str,
        calls: &[ToolCall],
    ) -> std::collections::HashMap<String, Vec<usize>> {
        let mut out: std::collections::HashMap<String, Vec<usize>> =
            std::collections::HashMap::new();
        if calls.len() < 2 {
            return out;
        }
        let Some(jev) = self.jev_client() else {
            return out;
        };
        let state =
            crate::jev::build_state(&format!("Pending approvals: {} items", calls.len()), &[]);
        let labels = &["change_a", "change_b", "change_c", "standalone"];
        for (i, call) in calls.iter().enumerate() {
            let question = format!(
                "Which logical change does this approval belong to? Call: {} {}",
                call.tool_name,
                crate::jev::preview_chars(
                    &serde_json::to_string(&call.arguments).unwrap_or_default(),
                    200,
                ),
            );
            let decision = match jev.evaluate_score(&state, &question, labels).await {
                Ok(d) => d,
                Err(_) => return out,
            };
            out.entry(decision.value).or_default().push(i);
        }
        self.log_jev_decision(
            holder,
            "approval_group",
            &format!("{} approvals", calls.len()),
            "group_per_call",
            serde_json::json!({
                "groups": out.iter().map(|(k, v)| (k.clone(), v.len())).collect::<std::collections::HashMap<_,_>>(),
            }),
            1.0,
            0,
            false,
            crate::jev::DecisionSource::Jev,
        );
        out
    }

    pub async fn extract_handoff_facts_with_jev(&self, transcript: &str) -> Vec<String> {
        const MAX_MESSAGES: usize = 40;
        let Some(jev) = self.jev_client() else {
            return Vec::new();
        };
        // Split the transcript into paragraph-sized chunks. The
        // handoff input comes in as one markdown string; splitting on
        // double newlines produces one message per turn in the
        // format the TUI exports.
        let chunks: Vec<&str> = transcript
            .split("\n\n")
            .filter(|s| s.trim().len() > 20)
            .take(MAX_MESSAGES)
            .collect();
        if chunks.is_empty() {
            return Vec::new();
        }

        let state = crate::jev::build_state(transcript, &[]);
        let mut questions: Vec<(String, String)> = Vec::new();
        for (i, chunk) in chunks.iter().enumerate() {
            questions.push((
                format!("decision_{i}"),
                format!(
                    "Does this message contain a durable decision or a concrete chosen approach? Message: {}",
                    crate::jev::preview_chars(chunk, 400),
                ),
            ));
            questions.push((
                format!("unfinished_{i}"),
                format!(
                    "Does this message describe an unfinished task, a TODO, or the next step? Message: {}",
                    crate::jev::preview_chars(chunk, 400),
                ),
            ));
            questions.push((
                format!("file_{i}"),
                format!(
                    "Does this message reference a specific file path? Message: {}",
                    crate::jev::preview_chars(chunk, 400),
                ),
            ));
        }
        let started = std::time::Instant::now();
        let rows = match jev.evaluate_yes_no_batch(&state, &questions).await {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let threshold = jev.thresholds().task_classify_min;

        let mut facts: Vec<String> = Vec::new();
        for (i, chunk) in chunks.iter().enumerate() {
            let get = |prefix: &str| {
                rows.iter()
                    .find(|(id, _)| id == &format!("{prefix}_{i}"))
                    .map(|(_, p)| *p)
                    .unwrap_or(0.0)
            };
            let mut tags: Vec<&str> = Vec::new();
            if get("decision") >= threshold {
                tags.push("decision");
            }
            if get("unfinished") >= threshold {
                tags.push("unfinished");
            }
            if get("file") >= threshold {
                tags.push("file");
            }
            if !tags.is_empty() {
                facts.push(format!(
                    "[{}] {}",
                    tags.join(","),
                    crate::jev::preview_chars(chunk, 400),
                ));
            }
        }
        self.log_jev_decision(
            DEFAULT_TRANSCRIPT_KEY,
            "handoff_extract",
            &crate::jev::preview_chars(transcript, 200),
            "decision,unfinished,file",
            serde_json::json!({
                "chunks": chunks.len(),
                "facts": facts.len(),
            }),
            1.0,
            elapsed_ms,
            false,
            crate::jev::DecisionSource::Jev,
        );
        facts
    }

    pub(crate) async fn filter_mcp_tools_with_jev(
        &self,
        holder: &str,
        input: &str,
        definitions: Vec<ToolDefinition>,
    ) -> Vec<ToolDefinition> {
        const MIN_MCP_TOOLS_TO_FILTER: usize = 5;
        const KEEP_TOP_N: usize = 3;
        // Partition into MCP and non-MCP. The non-MCP half is
        // untouched by this filter.
        let (mcp, other): (Vec<ToolDefinition>, Vec<ToolDefinition>) = definitions
            .into_iter()
            .partition(|d| d.name.starts_with(crate::mcp_adapters::MCP_TOOL_PREFIX));
        if mcp.len() < MIN_MCP_TOOLS_TO_FILTER {
            let mut out = other;
            out.extend(mcp);
            return out;
        }
        let Some(jev) = self.jev_client() else {
            let mut out = other;
            out.extend(mcp);
            return out;
        };
        // One yes/no per MCP tool name. Bounded to avoid a
        // pathological request.
        const MAX_TOOL_NAMES: usize = 30;
        let slice: Vec<ToolDefinition> = mcp.iter().take(MAX_TOOL_NAMES).cloned().collect();
        let questions: Vec<(String, String)> = slice
            .iter()
            .enumerate()
            .map(|(i, d)| {
                (
                    format!("tool_{i}"),
                    format!(
                        "Would the tool `{}` (whose description is {}) be useful for this request?",
                        d.name,
                        crate::jev::preview_chars(&d.description, 200),
                    ),
                )
            })
            .collect();
        let state = crate::jev::build_state(input, &[]);
        let started = std::time::Instant::now();
        let rows = match jev.evaluate_yes_no_batch(&state, &questions).await {
            Ok(r) => r,
            Err(_) => {
                let mut out = other;
                out.extend(mcp);
                return out;
            }
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let threshold = jev.thresholds().tool_filter_min;
        // Sort MCP tools by p descending, keep top N above threshold.
        let mut scored: Vec<(usize, f32)> = slice
            .iter()
            .enumerate()
            .map(|(i, _)| {
                let p = rows
                    .iter()
                    .find(|(id, _)| id == &format!("tool_{i}"))
                    .map(|(_, p)| *p)
                    .unwrap_or(0.0);
                (i, p)
            })
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let kept_mcp: Vec<ToolDefinition> = scored
            .iter()
            .take(KEEP_TOP_N)
            .filter(|(_, p)| *p >= threshold)
            .filter_map(|(i, _)| slice.get(*i).cloned())
            .collect();
        // If the filter dropped everything, keep the original set
        // — an LLM with no MCP tool is worse than one with ten.
        if kept_mcp.is_empty() {
            let mut out = other;
            out.extend(mcp);
            return out;
        }
        self.log_jev_decision(
            holder,
            "mcp_filter",
            input,
            "per_mcp_tool_relevance",
            serde_json::json!({
                "total_mcp": mcp.len(),
                "kept": kept_mcp.iter().map(|d| d.name.clone()).collect::<Vec<_>>(),
            }),
            1.0,
            elapsed_ms,
            false,
            crate::jev::DecisionSource::Jev,
        );
        let mut out = other;
        out.extend(kept_mcp);
        out
    }

    pub async fn detect_phase_change_with_jev(&self, holder: &str) -> Option<(String, String)> {
        const PHASE_WINDOW: usize = 6;
        let jev = self.jev_client()?;
        // Pull the recent transcript. Bounded so a long session
        // does not blow the state.
        let history = {
            let g = self.history.read().await;
            g.get(holder).cloned().unwrap_or_default()
        };
        if history.len() < 3 {
            return None;
        }
        let tail: Vec<String> = history
            .iter()
            .rev()
            .take(PHASE_WINDOW)
            .map(|m| {
                let role = match m.role {
                    kod_types::MessageRole::User => "user",
                    kod_types::MessageRole::Assistant => "assistant",
                    _ => "other",
                };
                format!("{role}: {}", crate::jev::preview_chars(&m.content, 300))
            })
            .collect();
        let transcript_tail = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
        let state = crate::jev::build_state(&transcript_tail, &[]);

        // Ask the current phase, then the previous phase. Compare
        // and gate on confidence.
        let labels = &[
            "exploring",
            "coding",
            "debugging",
            "testing",
            "refactoring",
            "documenting",
        ];
        let started = std::time::Instant::now();
        let now = jev
            .evaluate_score(&state, "What phase is the session currently in?", labels)
            .await
            .ok()?;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        // Previous phase: use the earliest half of the window as the
        // state for a second question. Reuses the same Jev client.
        let earlier: String = transcript_tail
            .lines()
            .take(transcript_tail.lines().count() / 2)
            .collect::<Vec<_>>()
            .join("\n");
        let state2 = crate::jev::build_state(&earlier, &[]);
        let prev = jev
            .evaluate_score(
                &state2,
                "What phase was the session in before the most recent turn?",
                labels,
            )
            .await
            .ok()?;
        let confidence = now.confidence.min(prev.confidence);
        self.log_jev_decision(
            holder,
            "phase_detect",
            &crate::jev::preview_chars(&transcript_tail, 200),
            "current_phase,previous_phase",
            serde_json::json!({
                "current_phase": now.value,
                "previous_phase": prev.value,
                "confidence": confidence,
            }),
            confidence,
            elapsed_ms,
            false,
            crate::jev::DecisionSource::Jev,
        );
        if now.value == prev.value {
            return None;
        }
        if confidence < jev.thresholds().auto_approve_min {
            return None;
        }
        Some((prev.value, now.value))
    }

    pub(crate) async fn compress_tool_result_with_jev(
        &self,
        holder: &str,
        call: &ToolCall,
        result: &ToolResult,
    ) -> Option<ToolResult> {
        const MIN_LINES_TO_COMPRESS: usize = 60;
        const MAX_LINES_TO_SCORE: usize = 200;
        let jev = self.jev_client()?;
        let request = self.current_request(holder).await?;
        let ToolResult::Success(v) = result else {
            return None;
        };
        if call.tool_name != "read_file" {
            return None;
        }
        let content = v.get("content").and_then(|c| c.as_str())?;
        let lines: Vec<&str> = content.lines().collect();
        if lines.len() < MIN_LINES_TO_COMPRESS {
            return None;
        }
        let slice = &lines[..lines.len().min(MAX_LINES_TO_SCORE)];

        let state = crate::jev::build_state(&request, &[]);
        let questions: Vec<(String, String)> = slice
            .iter()
            .enumerate()
            .map(|(i, line)| {
                (
                    format!("line_{i}"),
                    format!(
                        "Is this file line relevant to the request? Line: {}",
                        crate::jev::preview_chars(line, 160),
                    ),
                )
            })
            .collect();
        let started = std::time::Instant::now();
        let rows = jev.evaluate_yes_no_batch(&state, &questions).await.ok()?;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let threshold = jev.thresholds().memory_filter_min;

        let mut keep_idx: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut answers = serde_json::Map::new();
        for (i, line) in slice.iter().enumerate() {
            let p = rows
                .iter()
                .find(|(id, _)| id == &format!("line_{i}"))
                .map(|(_, p)| *p)
                .unwrap_or(1.0);
            answers.insert(format!("line_{i}"), serde_json::json!(p));
            if p >= threshold {
                keep_idx.insert(i);
            }
            let _ = line;
        }
        // Keep everything past the classification window.
        for i in slice.len()..lines.len() {
            keep_idx.insert(i);
        }
        let dropped = lines.len() - keep_idx.len();
        if dropped < 10 {
            // Not worth the substitution — keep the original.
            return None;
        }
        self.log_jev_decision(
            holder,
            "tool_result_compress",
            &request,
            "per_line_relevance",
            serde_json::Value::Object(answers),
            1.0,
            elapsed_ms,
            false,
            crate::jev::DecisionSource::Jev,
        );

        // Rebuild content with elision markers.
        let mut out_lines: Vec<String> = Vec::with_capacity(keep_idx.len());
        let mut i = 0_usize;
        let mut elided_run = 0_usize;
        while i < lines.len() {
            if keep_idx.contains(&i) {
                if elided_run > 0 {
                    out_lines.push(format!("… {elided_run} lines elided by Jev"));
                    elided_run = 0;
                }
                out_lines.push(lines[i].to_string());
            } else {
                elided_run += 1;
            }
            i += 1;
        }
        if elided_run > 0 {
            out_lines.push(format!("… {elided_run} lines elided by Jev"));
        }
        let new_content = out_lines.join("\n");

        let mut new_v = v.clone();
        if let Some(obj) = new_v.as_object_mut() {
            obj.insert(
                "content".to_string(),
                serde_json::Value::String(new_content),
            );
            obj.insert(
                "lines_elided_by_jev".to_string(),
                serde_json::json!(dropped),
            );
        }
        Some(ToolResult::Success(new_v))
    }

    pub async fn pick_endpoint_with_jev(&self, task_key: &str) -> Option<ModelRef> {
        let jev = self.jev_client()?;
        let registry = self.registry.read().await.clone()?;
        let names = registry.names();
        if names.len() < 2 {
            return None;
        }
        let state = crate::jev::build_state(&format!("Task type: {task_key}"), &[]);
        let started = std::time::Instant::now();
        let labels: Vec<&str> = names.iter().map(String::as_str).collect();
        let decision = jev
            .evaluate_score(&state, "Which endpoint should handle this task?", &labels)
            .await
            .ok()?;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let model = registry.default_model(&decision.value)?;
        self.log_jev_decision(
            DEFAULT_TRANSCRIPT_KEY,
            "endpoint_routing",
            task_key,
            "endpoint",
            serde_json::json!({ "endpoint": decision.value }),
            decision.confidence,
            elapsed_ms,
            false,
            crate::jev::DecisionSource::Jev,
        );
        Some(ModelRef::new(decision.value, model))
    }

    pub(crate) async fn reallocate_with_jev(
        &self,
        holder: &str,
        input: &str,
        base: &crate::budget::Allocation,
    ) -> crate::budget::Allocation {
        let Some(jev) = self.jev_client() else {
            return *base;
        };
        if input.len() < 20 {
            return *base;
        }
        let state = crate::jev::build_state(input, &[]);
        let pairs = [
            (
                "needs_repomap".to_string(),
                "Does this request need the repository's symbol map?".to_string(),
            ),
            (
                "needs_history".to_string(),
                "Does this request need the full conversation history?".to_string(),
            ),
            (
                "needs_skills".to_string(),
                "Does this request need skill instructions?".to_string(),
            ),
        ];
        let started = std::time::Instant::now();
        let rows = match jev.evaluate_yes_no_batch(&state, &pairs).await {
            Ok(r) => r,
            Err(_) => return *base,
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let get = |k: &str| {
            rows.iter()
                .find(|(id, _)| id == k)
                .map(|(_, p)| *p)
                .unwrap_or(1.0)
        };
        let wants = [
            get("needs_history"),
            get("needs_skills"),
            1.0_f32,
            get("needs_repomap"),
        ];
        let threshold = jev.thresholds().task_classify_min;
        let total_share: u32 =
            base.history as u32 + base.skills as u32 + base.memory as u32 + base.repomap as u32;
        let mut yes_count = 0_u32;
        for w in &wants {
            if *w >= threshold {
                yes_count += 1;
            }
        }
        if yes_count == 0 || total_share == 0 {
            return *base;
        }
        let per_section = total_share / yes_count;
        let mut leftover = total_share - per_section * yes_count;

        let mut out = crate::budget::Allocation {
            request: base.request,
            history: 0,
            skills: 0,
            memory: 0,
            repomap: 0,
        };
        let mut give = |slot: &mut usize| {
            let mut add = per_section as usize;
            if leftover > 0 {
                add += 1;
                leftover -= 1;
            }
            *slot = add;
        };
        if wants[0] >= threshold {
            give(&mut out.history);
        }
        if wants[1] >= threshold {
            give(&mut out.skills);
        }
        if wants[2] >= threshold {
            give(&mut out.memory);
        }
        if wants[3] >= threshold {
            give(&mut out.repomap);
        }

        self.log_jev_decision(
            holder,
            "budget_reweight",
            input,
            "needs_history,needs_skills,needs_repomap",
            serde_json::json!({
                "needs_history": wants[0],
                "needs_skills": wants[1],
                "needs_repomap": wants[3],
                "alloc": {
                    "history": out.history,
                    "skills": out.skills,
                    "memory": out.memory,
                    "repomap": out.repomap,
                },
            }),
            1.0,
            elapsed_ms,
            false,
            crate::jev::DecisionSource::Jev,
        );
        out
    }

    pub(crate) async fn filter_memory_context_with_jev(
        &self,
        holder: &str,
        ctx: Option<kod_types::MemoryContext>,
    ) -> Option<kod_types::MemoryContext> {
        let mut ctx = ctx?;
        let jev = self.jev_client()?;
        let Some(request) = self.current_request(holder).await else {
            return Some(ctx);
        };
        // Skip when the total content is trivial — nothing to save.
        let total_chars = ctx
            .working_memory
            .iter()
            .chain(ctx.long_term.iter())
            .map(|e| e.content.len())
            .sum::<usize>();
        if total_chars < 400 {
            return Some(ctx);
        }

        // Build the id→text list for both halves in one request.
        let mut pairs: Vec<(String, String)> = Vec::new();
        for e in &ctx.working_memory {
            pairs.push((format!("w:{}", e.id), e.content.clone()));
        }
        for e in &ctx.long_term {
            pairs.push((format!("l:{}", e.id), e.content.clone()));
        }
        let kept = self.filter_memory_entries_with_jev(pairs).await;
        let keep_set: std::collections::HashSet<String> =
            kept.into_iter().map(|(id, _)| id).collect();

        ctx.working_memory
            .retain(|e| keep_set.contains(&format!("w:{}", e.id)));
        ctx.long_term
            .retain(|e| keep_set.contains(&format!("l:{}", e.id)));

        // Recompute the token estimate from the survivors. The exact
        // formula is not important here — this is a bookkeeping
        // value the budget pass reads.
        ctx.total_tokens = ctx
            .working_memory
            .iter()
            .chain(ctx.long_term.iter())
            .map(|e| e.content.len() / 4)
            .sum();
        let _ = request; // documented use above, kept for symmetry
        let _ = jev;
        Some(ctx)
    }

    pub async fn filter_memory_entries_with_jev(
        &self,
        entries: Vec<(String, String)>,
    ) -> Vec<(String, String)> {
        if entries.is_empty() {
            return entries;
        }
        let Some(jev) = self.jev_client() else {
            return entries;
        };
        let Some(request) = self.current_request(DEFAULT_TRANSCRIPT_KEY).await else {
            return entries;
        };
        let threshold = jev.thresholds().memory_filter_min;

        // Build one yes/no per entry, keyed by id. Bounded to 30
        // entries per call so a runaway memory store cannot produce
        // a pathological round-trip.
        const MAX_ENTRIES: usize = 30;
        let slice: Vec<(String, String)> = entries.iter().take(MAX_ENTRIES).cloned().collect();
        let state = crate::jev::build_state(&request, &[]);
        let questions: Vec<(String, String)> = slice
            .iter()
            .map(|(id, text)| {
                (
                    id.clone(),
                    format!(
                        "Is this memory entry relevant to the request? Entry: {}",
                        crate::jev::preview_chars(text, 400)
                    ),
                )
            })
            .collect();
        let started = std::time::Instant::now();
        let result = jev.evaluate_yes_no_batch(&state, &questions).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;

        let (keep_ids, answers, source) = match result {
            Ok(rows) => {
                let mut keep = std::collections::HashSet::<String>::new();
                let mut map = serde_json::Map::new();
                for (id, p) in &rows {
                    map.insert(id.clone(), serde_json::json!(p));
                    if *p >= threshold {
                        keep.insert(id.clone());
                    }
                }
                (
                    keep,
                    serde_json::Value::Object(map),
                    crate::jev::DecisionSource::Jev,
                )
            }
            Err(_) => (
                entries.iter().map(|(id, _)| id.clone()).collect(),
                serde_json::json!({}),
                crate::jev::DecisionSource::Heuristic,
            ),
        };

        self.log_jev_decision(
            DEFAULT_TRANSCRIPT_KEY,
            "memory_filter",
            &request,
            "relevance_per_entry",
            answers,
            1.0,
            elapsed_ms,
            false,
            source,
        );

        // H-E12: keep the original entries when the filter dropped
        // everything. The doc comment said exactly this and the code
        // did the opposite — an over-eager Jev (or a threshold set
        // too high) emptied the prompt's memory block entirely.
        // Snapshot the original before consuming it.
        let original = entries.clone();
        let filtered: Vec<(String, String)> = entries
            .into_iter()
            .filter(|(id, _)| keep_ids.contains(id))
            .collect();
        if filtered.is_empty() {
            return original;
        }
        filtered
    }

    pub(crate) async fn auto_approve_with_jev(
        &self,
        key: &str,
        calls: &[ToolCall],
        ask_indices: &std::collections::HashSet<usize>,
    ) -> std::collections::HashSet<usize> {
        let mut approved: std::collections::HashSet<usize> = std::collections::HashSet::new();
        if ask_indices.is_empty() {
            return approved;
        }
        let Some(jev) = self.jev_client() else {
            return approved;
        };
        let Some(request_text) = self.current_request(key).await else {
            return approved;
        };
        let threshold = jev.thresholds().auto_approve_min;

        for &i in ask_indices {
            let Some(call) = calls.get(i) else { continue };
            let state = crate::jev::build_state(
                &format!(
                    "User request: {request_text}\nTool: {}\nArguments: {}",
                    call.tool_name,
                    serde_json::to_string(&call.arguments).unwrap_or_default()
                ),
                &[],
            );
            let started = std::time::Instant::now();

            // Ask both questions in one round-trip via the batch API
            // for yes/no; risk_level is a separate score call because
            // it has an ordered label set.
            let pairs = [(
                "likely_approved".to_string(),
                "Would the user almost certainly approve this tool call?".to_string(),
            )];
            let yes = jev.evaluate_yes_no_batch(&state, &pairs).await;
            let elapsed_ms = started.elapsed().as_millis() as u64;

            let (p_yes, yes_source) = match yes {
                Ok(rows) => (
                    rows.first().map(|(_, p)| *p).unwrap_or(0.0),
                    crate::jev::DecisionSource::Jev,
                ),
                Err(_) => (0.0, crate::jev::DecisionSource::Heuristic),
            };

            // Short-circuit: skip the second call when the first is
            // already below the threshold — the risk question costs a
            // network round-trip and cannot flip a "no".
            let (risk_label, risk_source) = if p_yes >= threshold {
                let risk_labels = &["read_only", "reversible", "destructive", "irreversible"];
                match jev
                    .evaluate_score(&state, "How risky is this operation?", risk_labels)
                    .await
                {
                    Ok(d) => (Some(d.value), crate::jev::DecisionSource::Jev),
                    Err(_) => (None, crate::jev::DecisionSource::Heuristic),
                }
            } else {
                (None, crate::jev::DecisionSource::Heuristic)
            };

            let risk_blocking = matches!(
                risk_label.as_deref(),
                Some("destructive") | Some("irreversible")
            );
            let final_decision = p_yes >= threshold && !risk_blocking;

            if final_decision {
                approved.insert(i);
            }

            let source = if matches!(yes_source, crate::jev::DecisionSource::Heuristic)
                || matches!(risk_source, crate::jev::DecisionSource::Heuristic)
            {
                crate::jev::DecisionSource::Heuristic
            } else {
                crate::jev::DecisionSource::Jev
            };
            let answers = serde_json::json!({
                "likely_approved": p_yes,
                "risk_level": risk_label,
                "auto_approved": final_decision,
            });
            self.log_jev_decision(
                key,
                "auto_approve",
                &format!(
                    "{} {}",
                    call.tool_name,
                    format_call_brief(&call.tool_name, &call.arguments)
                ),
                "likely_approved,risk_level",
                answers,
                p_yes,
                elapsed_ms,
                false,
                source,
            );
        }

        approved
    }

    pub(crate) async fn filter_tool_definitions_with_jev(
        &self,
        key: &str,
        input: &str,
        definitions: Vec<ToolDefinition>,
    ) -> Vec<ToolDefinition> {
        let Some(jev) = self.jev_client() else {
            return definitions;
        };
        let threshold = jev.thresholds().tool_filter_min;
        let state = crate::jev::build_state(input, &[]);
        let labels = kod_types::ToolCategory::all_labels();
        let questions: Vec<(String, String)> = labels
            .iter()
            .map(|l| {
                (
                    (*l).to_string(),
                    format!("Does the user's request require a {l} tool?"),
                )
            })
            .collect();
        let started = std::time::Instant::now();
        let decision = jev.evaluate_yes_no_batch(&state, &questions).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;

        let (categories, answers_json, confidence, source) = match decision {
            Ok(pairs) => {
                let mut cats: Vec<kod_types::ToolCategory> = Vec::new();
                let mut map = serde_json::Map::new();
                let mut min_conf = 1.0_f32;
                for (label, p) in &pairs {
                    map.insert(label.clone(), serde_json::json!(p));
                    if *p >= threshold
                        && let Some(c) = kod_types::ToolCategory::from_label(label)
                    {
                        cats.push(c);
                    }
                    min_conf = min_conf.min((*p - 0.5).abs() * 2.0);
                }
                (
                    cats,
                    serde_json::Value::Object(map),
                    min_conf,
                    crate::jev::DecisionSource::Jev,
                )
            }
            Err(_) => (
                labels
                    .iter()
                    .filter_map(|l| kod_types::ToolCategory::from_label(l))
                    .collect(),
                serde_json::json!({}),
                1.0,
                crate::jev::DecisionSource::Heuristic,
            ),
        };

        let filtered: Vec<ToolDefinition> = definitions
            .iter()
            .filter(|d| categories.contains(&d.category))
            .cloned()
            .collect();

        let questions_summary = labels.join(",");
        self.log_jev_decision(
            key,
            "tool_filter",
            input,
            &questions_summary,
            answers_json,
            confidence,
            elapsed_ms,
            false,
            source,
        );

        if filtered.is_empty() {
            definitions
        } else {
            filtered
        }
    }

    pub(crate) async fn refine_task_type_with_jev(
        &self,
        key: &str,
        input: &str,
        heuristic: crate::router::TaskType,
    ) -> crate::router::TaskType {
        let Some(jev) = self.jev_client() else {
            return heuristic;
        };
        let state =
            crate::jev::build_state(input, &[("heuristic_task_type", heuristic.as_label())]);
        let question = "Which task type best describes this request?";
        let labels = crate::router::TaskType::all_labels();
        let started = std::time::Instant::now();
        let decision = jev.evaluate_choice(&state, question, labels).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;

        let (final_type, confidence, source) = match decision {
            Ok(d) => {
                let parsed = crate::router::TaskType::from_label(&d.value);
                let threshold = jev.thresholds().task_classify_min;
                match parsed {
                    Some(t) if d.confidence >= threshold => {
                        (t, d.confidence, crate::jev::DecisionSource::Jev)
                    }
                    _ => (
                        heuristic,
                        d.confidence,
                        crate::jev::DecisionSource::Heuristic,
                    ),
                }
            }
            Err(_) => (heuristic, 1.0, crate::jev::DecisionSource::Heuristic),
        };

        let answers = serde_json::json!({
            "task_type": final_type.as_label(),
            "confidence": confidence,
        });
        let questions_summary = "task_type".to_string();
        self.log_jev_decision(
            key,
            "task_classify",
            input,
            &questions_summary,
            answers,
            confidence,
            elapsed_ms,
            false,
            source,
        );

        final_type
    }
}
