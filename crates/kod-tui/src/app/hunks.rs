//! Partial-hunk approval state on `KodApp`.
//!
//! Extracted from `app/mod.rs` (S9). When a `patch_file` call comes
//! through, the TUI offers to approve it hunk-by-hunk; this module
//! owns the selection cursor and the "accept only the checked hunks"
//! flow.

use super::*;

impl KodApp {
    pub fn is_selecting_hunks(&self) -> bool {
        self.pending_hunk_selection.is_some()
    }

    pub fn hunk_selection(&self) -> Option<&PendingHunkSelection> {
        self.pending_hunk_selection.as_ref()
    }

    /// Begin hunk selection for the current approval item. No-op when
    /// there is no current item, or the item is not a `patch_file`
    /// call with a non-empty `patch` argument.
    pub fn begin_hunk_selection(&mut self) -> bool {
        let Some(batch) = self.pending_batch.as_ref() else {
            return false;
        };
        let Some(item) = batch.current_item() else {
            return false;
        };
        if item.tool_name != "patch_file" {
            return false;
        }
        let Some(patch) = item.arguments.get("patch").and_then(|v| v.as_str()) else {
            return false;
        };
        let (header, hunks) = split_hunks(patch);
        if hunks.is_empty() {
            return false;
        }
        let selected = vec![true; hunks.len()];
        self.pending_hunk_selection = Some(PendingHunkSelection {
            approval_id: item.id,
            original_arguments: item.arguments.clone(),
            header,
            hunks,
            selected,
            cursor: 0,
        });
        true
    }

    pub fn cancel_hunk_selection(&mut self) {
        self.pending_hunk_selection = None;
    }

    pub fn hunk_toggle(&mut self) {
        if let Some(h) = self.pending_hunk_selection.as_mut() {
            h.toggle_current();
        }
    }

    pub fn hunk_next(&mut self) {
        if let Some(h) = self.pending_hunk_selection.as_mut() {
            h.advance();
        }
    }

    pub fn hunk_prev(&mut self) {
        if let Some(h) = self.pending_hunk_selection.as_mut() {
            h.retreat();
        }
    }

    /// Finish hunk selection: apply the filtered patch to the item's
    /// `arguments.patch`, and return `(id, arguments)` for the caller
    /// to send as an `ApproveWith`. `None` when the selection was
    /// cancelled or the state was inconsistent.
    pub fn hunk_commit(&mut self) -> Option<(u64, serde_json::Value)> {
        let sel = self.pending_hunk_selection.take()?;
        if sel.selected_count() == 0 {
            // Nothing to apply — refuse. The caller sees None and
            // keeps the dialog up.
            self.pending_hunk_selection = Some(sel);
            return None;
        }
        let filtered = sel.build_patch();
        let mut args = sel.original_arguments.clone();
        if let Some(obj) = args.as_object_mut() {
            obj.insert("patch".to_string(), serde_json::Value::String(filtered));
        }
        // Apply to the pending item so the dialog reflects the change.
        if let Some(batch) = self.pending_batch.as_mut()
            && let Some(item) = batch.items.iter_mut().find(|i| i.id == sel.approval_id)
        {
            item.arguments = args.clone();
        }
        Some((sel.approval_id, args))
    }
}
