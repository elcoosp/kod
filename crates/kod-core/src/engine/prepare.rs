//! Turn preparation: pre-flight for a turn.
//!
//! Extracted from `engine/mod.rs`.

use super::*;

impl KodEngine {
    pub(crate) async fn prepare_turn(
        &self,
        key: &str,
        input: &str,
        retrieval_log_turn_id: Option<u64>,
        create_plan: bool,
    ) -> Result<TurnPreparation> {
        // Delta §14.1: register any vendor-shaped secret the user
        // pasted directly into the prompt, so the outbound obfuscation
        // pass replaces it. The env scan at install and the tool-result
        // scan cover their sources; a key typed into the message was
        // covered by neither, so it reached the provider in the clear.
        // This runs before any prompt is composed, so the same turn's
        // request already carries the placeholder.
        let _ = self.register_secrets_in_text(input).await;
        // A real turn is starting: the prewarm's speculation is over,
        // and the next fresh turn should warm again.
        self.reset_prewarm(key).await;
        // Delta §12.3 / §12.7: drain the periodic-task flags at the
        // one point every entry point reaches. The sharpshooter pass
        // writes its decisions files; the mental-models refresh
        // rewrites the `AfterConsolidation` blocks. Both must run
        // *before* the prompt is built for this turn, because the
        // model block sits in the cacheable prefix — a mid-turn
        // rewrite would invalidate it. `prepare_turn` runs before
        // either prompt is composed, so this is the correct point.
        self.drain_due_sharpshooter().await;
        self.drain_due_mental_models_refresh().await;

        let response = self
            .classify_and_filter(key, input, retrieval_log_turn_id)
            .await?;
        let (task_type, refined_skills) = self.refine_classification(key, input, &response).await;
        if create_plan {
            let options_for_plan = self.generation_defaults.read().await.to_options();
            if let Ok(provider) = self
                .resolve_provider_for_model_ref(&self.current_model.read().await.clone())
                .await
            {
                self.maybe_create_plan(key, input, task_type, &provider, &options_for_plan)
                    .await;
            }
        }
        // P2-a: compact before rendering, so the current turn pays
        // the smaller prompt. A no-op unless the observed usage has
        // crossed the hard threshold.
        let _ = self.maybe_compact_for(key).await;
        let history = self.render_history_for(key).await;
        self.remember_turn_for(key, true, input).await;
        let (alloc, definitions, pending) = self
            .build_budgeted_prompt(
                key,
                input,
                task_type,
                &history,
                response.memory_context.clone(),
                &refined_skills,
            )
            .await?;
        let system_text = {
            let plan = self
                .router
                .build_prompt_plan_with_skills(
                    input,
                    &task_type,
                    &history,
                    response.memory_context.clone(),
                    alloc.as_ref().ok(),
                    Some(&refined_skills),
                )
                .await?;
            plan.render_text()
        };
        // Delta 7.2: append any late LSP diagnostics a deferred
        // background pass queued since the last turn. A slow
        // language server's answer was not lost — it arrives with
        // the turn after the write that triggered it. Capped at 20
        // entries so a large error set does not dominate the prompt.
        let system_text = {
            let late = self.deferred_diagnostics.take(key);
            if late.is_empty() {
                system_text
            } else {
                let mut s = system_text;
                s.push_str("\n\n## LSP diagnostics (late)\n\n");
                s.push_str(&format!(
                    "{} diagnostic(s) arrived after the previous write:\n\n",
                    late.len(),
                ));
                for d in late.iter().take(20) {
                    s.push_str(&format!(
                        "{}:{}:{} {} {}\n",
                        d.file, d.line, d.column, d.severity, d.message,
                    ));
                }
                s
            }
        };
        let initial_messages: Vec<kod_types::ChatMessage> = {
            let guard = self.history.read().await;
            guard.get(key).cloned().unwrap_or_default()
        };
        Ok(TurnPreparation {
            response,
            task_type,
            refined_skills,
            alloc,
            definitions,
            pending,
            system_text,
            initial_messages,
        })
    }
}
