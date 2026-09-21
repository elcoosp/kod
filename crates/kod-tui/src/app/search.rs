//! In-transcript search state on `KodApp`.
//!
//! Extracted from `app/mod.rs` (S9). Owns the search query, the
//! match cursor, and the "target message" the chat widget centers
//! on when a search is active.

use super::*;

impl KodApp {
    /// Start (or replace) a case-insensitive search; returns match count.
    pub fn set_search(&mut self, query: &str) -> usize {
        let q = query.trim();
        if q.is_empty() {
            self.clear_search();
            return 0;
        }
        self.search_query = Some(q.to_string());
        self.search_index = 0;
        let n = self.search_matches().len();
        if n > 0 {
            self.jump_to_search_match(0);
        }
        n
    }

    pub fn clear_search(&mut self) {
        self.search_query = None;
        self.search_index = 0;
        self.search_editing = false;
    }

    pub fn search_query(&self) -> Option<&str> {
        self.search_query.as_deref()
    }

    /// Indices of messages containing the query (case-insensitive).
    pub fn search_matches(&self) -> Vec<usize> {
        let q = match &self.search_query {
            Some(q) => q.to_lowercase(),
            None => return Vec::new(),
        };
        self.messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.content.to_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect()
    }

    fn jump_to_search_match(&mut self, _pos: usize) {
        // Pin to live bottom; the match highlight carries the position.
        // (Viewport math lives in the chat widget, which centers matches
        // when a search is active.)
        self.scroll_to_bottom();
    }

    /// Step to the next match (wraps). Returns (position, total).
    pub fn search_next(&mut self) -> Option<(usize, usize)> {
        let n = self.search_matches().len();
        if n == 0 {
            return None;
        }
        self.search_index = (self.search_index + 1) % n;
        self.jump_to_search_match(self.search_index);
        Some((self.search_index + 1, n))
    }

    pub fn search_prev(&mut self) -> Option<(usize, usize)> {
        let n = self.search_matches().len();
        if n == 0 {
            return None;
        }
        self.search_index = (self.search_index + n - 1) % n;
        self.jump_to_search_match(self.search_index);
        Some((self.search_index + 1, n))
    }

    pub fn current_search_pos(&self) -> Option<(usize, usize)> {
        let n = self.search_matches().len();
        if n == 0 {
            None
        } else {
            Some((self.search_index % n + 1, n))
        }
    }

    /// Open the type-ahead search bar with an empty query.
    ///
    /// While the bar is open, `handle_key` routes printable characters
    /// and Backspace into the query (see `is_editing_search`), Enter
    /// commits (leaving the query navigable with `n`/`N`), and Escape
    /// clears the search outright. This is the state `/search` with no
    /// argument opens.
    pub fn begin_search(&mut self) {
        self.search_query = Some(String::new());
        self.search_index = 0;
        self.search_editing = true;
    }

    /// True while the search bar is open and the user is typing into
    /// it. Distinct from `is_searching()` (which requires a non-empty
    /// query): during editing there are no matches yet and no
    /// match-position to report, but the bar is on screen and every
    /// keystroke is the user's query.
    pub fn is_editing_search(&self) -> bool {
        self.search_editing && self.search_query.is_some()
    }

    /// Leave the editing state without dropping the query: the search
    /// remains active (`is_searching()` is unchanged), but typing
    /// stops going into the query, and `n`/`N` navigate matches.
    /// Enter calls this.
    pub fn commit_search(&mut self) {
        self.search_editing = false;
    }

    /// Type into the active search (appended to the query).
    pub fn search_type(&mut self, c: char) {
        if let Some(q) = &mut self.search_query {
            q.push(c);
        }
        let query = self.search_query_text().to_string();
        let n = self.set_search(&query);
        if n > 0 {
            self.jump_to_search_match(self.search_index);
        }
    }

    /// Backspace the search query.
    pub fn search_backspace(&mut self) {
        if let Some(q) = &mut self.search_query {
            q.pop();
        }
        let query = self.search_query_text().to_string();
        let n = self.set_search(&query);
        if n > 0 {
            self.jump_to_search_match(self.search_index);
        }
    }

    /// True while a search query is active (status bar + highlighting).
    pub fn is_searching(&self) -> bool {
        self.search_query.as_deref().is_some_and(|q| !q.is_empty())
    }

    /// Active search text (empty when no search).
    pub fn search_query_text(&self) -> &str {
        self.search_query.as_deref().unwrap_or("")
    }

    /// What to say about the current search in the status bar.
    ///
    /// The old `search_position() -> (usize, usize)` collapsed three
    /// distinct states into two indistinguishable pairs:
    ///
    ///   * search not active            -> (0, 0)
    ///   * search active, no matches    -> (0, 0)
    ///   * search active, match 1 of 1  -> (1, 1)
    ///
    /// A caller reading `(0, 0)` could not tell whether to say "no
    /// search" or "no matches" — and the widget that renders the search
    /// status guessed "no matches", so a session that had never been
    /// searched still showed "no matches" the moment a search state
    /// existed. The enum below names the states; the widget matches on
    /// it and the label is derived here, once.
    pub fn search_status(&self) -> SearchStatus {
        match self.search_query.as_deref() {
            None => SearchStatus::Inactive,
            Some("") => SearchStatus::Editing,
            Some(_) => {
                let total = self.search_matches().len();
                if total == 0 {
                    SearchStatus::NoMatches
                } else {
                    SearchStatus::At {
                        position: (self.search_index % total) + 1,
                        total,
                    }
                }
            }
        }
    }

    /// The message id currently targeted by the active search, if
    /// any. Returns `None` when no search query is set, when the
    /// query is empty (editing), or when it found no matches.
    ///
    /// Used by the chat widget to scroll the targeted message into
    /// view. The underlying `search_index` is private so the widget
    /// cannot reach it directly; this exposes exactly what the widget
    /// needs (the id of the match to center) without exposing the
    /// index arithmetic.
    pub fn search_target_message_id(&self) -> Option<&kod_types::MessageId> {
        let matches = self.search_matches();
        if matches.is_empty() {
            return None;
        }
        let pos = self.search_index % matches.len();
        let msg_idx = *matches.get(pos)?;
        self.messages().get(msg_idx).map(|m| &m.id)
    }

    /// Display string for the status bar. Built here so the widget does
    /// not re-implement the match on `SearchStatus` and drift.
    pub fn search_status_label(&self) -> String {
        match self.search_status() {
            SearchStatus::Inactive => String::new(),
            SearchStatus::Editing => {
                format!(" /{} — typing… (Esc exits)", self.search_query_text())
            }
            SearchStatus::NoMatches => {
                format!(" /{} — no matches (Esc exits)", self.search_query_text())
            }
            SearchStatus::At { position, total } => format!(
                " /{} — {}/{} (n next · N prev · Esc exits)",
                self.search_query_text(),
                position,
                total
            ),
        }
    }
}
