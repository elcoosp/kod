//! Jev advisor helpers for [`super::KodEngine`].
//!
//! The `*_with_jev` family used to live inline in `engine/mod.rs`.
//! Moving each one here keeps the entire Jev dependency surface —
//! the network client, the decision log, the request lookup — in one
//! file. As a submodule of `engine`, this file has access to
//! `KodEngine`'s private fields and private helper methods; no
//! visibility changes are required on the parent.

use super::KodEngine;
use kod_types::{ToolCall, ToolResult};

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
}
