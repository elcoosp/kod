use super::*;

impl KodEngine {
    /// Is a language server available for this path?
    ///
    /// Delegates to `kod_lsp::binary_for_path`. Kept as an associated
    /// function because `kod-tui` and the `lsp_*` tools call it to
    /// decide whether an "install X" message is appropriate before
    /// making a request.
    pub fn lsp_binary_for(path: &std::path::Path) -> Option<&'static str> {
        kod_lsp::binary_for_path(path)
    }

    /// The engine's LSP pool. Exposed so callers (the `lsp_*` tools,
    /// `kod doctor`, the post-write diagnostics hook) can reach the
    /// same `LspManager` the engine uses without going through the
    /// engine's higher-level methods.
    pub fn lsp_manager(&self) -> &Arc<kod_lsp::LspManager> {
        &self.lsp_manager
    }

    /// Return LSP diagnostics for `path`.
    ///
    /// Empty on any failure (no server, spawn error, protocol error,
    /// timeout). The caller treats empty as "no LSP feedback" and
    /// falls back to `CheckTool::run_check`, which disambiguates
    /// "clean" from "unreachable".
    ///
    /// `content` is what to analyze. The caller reads the file it just
    /// wrote; passing the content avoids a re-read that could race a
    /// concurrent write.
    pub async fn lsp_diagnostics(
        &self,
        path: &std::path::Path,
        content: &str,
        overall_timeout: std::time::Duration,
    ) -> Vec<kod_lsp::Diagnostic> {
        self.lsp_manager
            .diagnostics(path, content, overall_timeout)
            .await
    }

    /// LSP: definition at `(line, column)` in `path`. 1-based
    /// coordinates (the tool layer already uses them). Empty on any
    /// failure.
    pub async fn lsp_definition(
        &self,
        path: &std::path::Path,
        line: u32,
        column: u32,
    ) -> Vec<kod_lsp::Location> {
        let pos = kod_lsp::Position { line, column };
        self.lsp_manager.definition(path, pos).await
    }

    /// LSP: all references to the symbol at `(line, column)`.
    pub async fn lsp_references(
        &self,
        path: &std::path::Path,
        line: u32,
        column: u32,
        include_declaration: bool,
    ) -> Vec<kod_lsp::Location> {
        let pos = kod_lsp::Position { line, column };
        self.lsp_manager
            .references(path, pos, include_declaration)
            .await
    }

    /// LSP: hover text at `(line, column)`.
    pub async fn lsp_hover(
        &self,
        path: &std::path::Path,
        line: u32,
        column: u32,
    ) -> Option<kod_lsp::Hover> {
        let pos = kod_lsp::Position { line, column };
        self.lsp_manager.hover(path, pos).await
    }

    /// Shut down every language server. Called by `shutdown()`.
    pub(crate) async fn lsp_shutdown(&self) {
        self.lsp_manager.shutdown_all().await;
        tracing::info!("LSP servers shut down");
    }

    /// Capture the project's diagnostics into the baseline. Called at
    /// engine start (best-effort) and by any caller that wants the
    /// next auto-check to treat the current state as "before".
    ///
    /// Silent on failure: no project, no toolchain, or a timeout all
    /// leave the baseline at its previous value. A `None` baseline
    /// means the next auto-check reports every diagnostic it sees;
    /// that is the correct behavior for a session that never had a
    /// chance to establish a baseline.
    pub async fn refresh_check_baseline(&self) {
        // H-E13: run the check against the transcript's working
        // directory, not the engine's. A swarm agent
        // writing in its worktree was getting
        // diagnostics (and baseline overwrites) from
        // the main repo — the model saw errors it did
        // not introduce.
        match kod_tools::CheckTool::run_check(&self.working_dir, self.tool_context.timeout_secs)
            .await
        {
            Ok(outcome) => {
                let n = outcome.diagnostics.len();
                *self.check_baseline.write().await = Some(outcome.diagnostics);
                tracing::debug!(count = n, "check baseline refreshed");
            }
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "check baseline not refreshed (no project or toolchain)"
                );
            }
        }
    }

    /// The current baseline, if one has been captured. Public for
    /// tests and for a caller that wants to display what the engine
    /// considers "pre-existing".
    pub async fn check_baseline(&self) -> Option<Vec<kod_tools::check::Diagnostic>> {
        self.check_baseline.read().await.clone()
    }
}
