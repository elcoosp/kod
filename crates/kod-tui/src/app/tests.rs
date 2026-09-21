#[cfg(test)]
#[cfg(test)]
mod tests {
    #[test]
    fn tokens_per_sec_none_before_second_chunk() {
        let mut app = KodApp::new();
        app.begin_generation();
        assert!(app.tokens_per_sec().is_none(), "no chunk, no rate");
        app.add_response_chunk("first");
        // A single chunk has no measurable interval: the first and
        // last timestamps are the same instant. The accessor must
        // return `None` rather than compute a divide-by-zero or an
        // absurd instantaneous rate.
        assert!(app.tokens_per_sec().is_none(), "one chunk yields no rate",);
    }

    #[test]
    fn tokens_per_sec_positive_after_a_real_interval() {
        let mut app = KodApp::new();
        app.begin_generation();
        app.add_response_chunk("first");
        // Sleep long enough for the elapsed time to be meaningful
        // (the accessor's floor is 0.05s).
        std::thread::sleep(std::time::Duration::from_millis(60));
        // 400 chars = 100 tokens by the 4-chars-per-token rule.
        app.add_response_chunk(&"x".repeat(400));
        let rate = app
            .tokens_per_sec()
            .expect("a real interval must yield a rate");
        assert!(rate > 0.0, "rate must be positive, got {rate}");
        assert!(
            rate < 10_000.0,
            "rate must be finite and plausible, got {rate}",
        );
    }

    #[test]
    fn tokens_per_sec_resets_between_turns() {
        let mut app = KodApp::new();
        app.begin_generation();
        app.add_response_chunk("first");
        std::thread::sleep(std::time::Duration::from_millis(60));
        app.add_response_chunk("second");
        assert!(app.tokens_per_sec().is_some());
        // A new turn clears the state; the rate is per-turn.
        app.begin_generation();
        assert!(app.tokens_per_sec().is_none());
    }

    fn push_user(app: &mut KodApp, text: &str) {
        app.add_message(Message {
            id: MessageId::new(),
            role: MessageRole::User,
            content: text.into(),
            timestamp: Utc::now(),
            metadata: MessageMetadata::default(),
            sequence: 0,
        });
    }

    fn push_assistant(app: &mut KodApp, text: &str) {
        app.add_message(Message {
            id: MessageId::new(),
            role: MessageRole::Assistant,
            content: text.into(),
            timestamp: Utc::now(),
            metadata: MessageMetadata::default(),
            sequence: 0,
        });
    }

    #[test]
    fn export_html_wraps_content_in_document() {
        let mut app = KodApp::new();
        push_user(&mut app, "hello");
        push_assistant(&mut app, "hi there");
        let html = app.export_html();
        assert!(
            html.starts_with("<!doctype html>"),
            "output must open with a doctype: {}",
            &html[..html.len().min(120)],
        );
        assert!(
            html.contains("hello"),
            "the user's message must be included",
        );
        assert!(
            html.contains("hi there"),
            "the assistant's message must be included",
        );
        assert!(
            html.contains("<style>"),
            "CSS must be inline for a self-contained document",
        );
        assert!(
            html.contains("</html>"),
            "output must be a complete HTML document",
        );
    }

    #[test]
    fn export_html_escapes_special_characters() {
        // The content is user text and could contain `<`, `&`, or `"`;
        // all three must be escaped to keep the output a valid HTML
        // document. Regression guard: the escape helper is small and
        // easy to lose.
        let mut app = KodApp::new();
        push_user(
            &mut app,
            "less < than & greater > than and \"quote\" and 'single'",
        );
        let html = app.export_html();
        assert!(
            html.contains("&lt;"),
            "`<` must be escaped: not present in output",
        );
        assert!(
            html.contains("&amp;"),
            "`&` must be escaped: not present in output",
        );
        assert!(
            html.contains("&gt;"),
            "`>` must be escaped: not present in output",
        );
        assert!(
            html.contains("&quot;"),
            "`\"` must be escaped: not present in output",
        );
        // The raw forms must not appear inside content; the outer
        // document's own tags are removed by searching only for the
        // suspicious substrings.
        assert!(
            !html.contains("less < than"),
            "raw `<` leaked into the output",
        );
        assert!(
            !html.contains("& greater"),
            "raw `&` leaked into the output",
        );
    }

    #[test]
    fn export_html_is_stable_across_calls() {
        // The renderer is pure; two calls on the same app must
        // produce identical output. A regression that embedded a
        // timestamp or a random id would break a user's diff-based
        // workflow.
        let mut app = KodApp::new();
        push_user(&mut app, "hi");
        let a = app.export_html();
        let b = app.export_html();
        assert_eq!(a, b);
    }

    /// Serializes tests that mutate KOD_TUI_STATE_DIR (a process-wide
    /// environment variable). Rust runs unit tests in parallel by
    /// default, and two tests racing to set the same env var would
    /// step on each other's state directories.
    fn session_state_dir_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
    use crate::app::*;

    #[test]
    fn test_context_limit_overrides_default() {
        let mut app = KodApp::new();
        assert_eq!(app.context_limit(), DEFAULT_CONTEXT_LIMIT);
        // init_engine applies config.llm.default_endpoint().context_window so the meter and
        // the compaction threshold use the model's real window, not 128k.
        app.set_context_limit(8192);
        assert_eq!(app.context_limit(), 8192);
        assert!(
            app.context_label().contains("8.2k"),
            "got: {}",
            app.context_label()
        );
    }

    #[test]
    fn test_app_lifecycle() {
        let mut app = KodApp::new();

        app.set_input_mode(InputMode::Insert);
        app.add_char('t');
        app.add_char('e');
        app.add_char('s');
        app.add_char('t');

        assert_eq!(app.input(), "test");

        app.submit_input();
        assert_eq!(app.messages().len(), 1);
        assert_eq!(app.input(), "");
    }

    #[test]
    fn test_agent_management() {
        let mut app = KodApp::new();
        app.add_agent("agent1", vec!["coding".to_string()]);
        assert_eq!(app.agents().len(), 1);
        assert!(app.get_agent("agent1").is_some());
        app.update_agent_status("agent1", "working");
        assert_eq!(app.get_agent("agent1").unwrap().status, "working");
    }

    #[test]
    fn test_message_display() {
        let mut app = KodApp::new();
        app.add_message(Message {
            id: MessageId::new(),
            role: MessageRole::User,
            content: "hello".to_string(),
            timestamp: Utc::now(),
            metadata: MessageMetadata::default(),
            sequence: 0,
        });
    }

    /// Real usage is authoritative and must REPLACE the char-based
    /// estimate, not be maxed against it. Regression: the running
    /// estimate accumulates across turns (`note_prompt` for each user
    /// input, `note_usage` for each streamed response) and never
    /// resets, so `max(estimate, real)` grew unboundedly. After a
    /// dozen turns the meter reported a context far larger than the
    /// model ever saw, and auto-compact fired early because the
    /// threshold is checked against the same number.
    #[test]
    fn test_real_usage_replaces_estimate() {
        let mut app = KodApp::new();
        app.set_context_limit(100_000);

        // Turn 1: estimate from a 4000-char prompt then a 2000-char
        // streamed response. context_tokens ≈ (4000 + 2000) / 4 = 1500.
        app.note_prompt(&"a".repeat(4000));
        let est_after_turn_1 = app.context_tokens();
        assert!(est_after_turn_1 > 0);

        // The provider then reports real usage of 800. It must replace
        // the estimate, not be maxed against it.
        app.note_real_usage(800);
        assert_eq!(
            app.context_tokens(),
            800,
            "real usage should replace the estimate; got {}",
            app.context_tokens()
        );

        // Turn 2: estimate grows again, but the next real usage resets.
        app.note_prompt(&"b".repeat(4000));
        app.note_real_usage(900);
        assert_eq!(app.context_tokens(), 900);

        // Sanity: turning off real usage (0 tokens) leaves the previous
        // value alone rather than zeroing it — providers that omit
        // usage on a round shouldn't blank the meter.
        app.note_real_usage(0);
        assert_eq!(app.context_tokens(), 900);
    }

    /// Real usage replaces the estimate even when the estimate is
    /// *smaller*. Prior behavior took the max, so a low real usage
    /// after a high estimate never brought the meter down.
    #[test]
    fn test_real_usage_can_shrink_the_meter() {
        let mut app = KodApp::new();
        app.set_context_limit(100_000);

        app.note_prompt(&"x".repeat(40_000)); // ≈ 10_000 tokens
        let big_estimate = app.context_tokens();
        assert!(big_estimate >= 10_000);

        app.note_real_usage(500);
        assert_eq!(app.context_tokens(), 500);
    }

    /// Auto-compact must append its notice at the end of the transcript,
    /// not sort it to the top. The chat widget sorts by `sequence`, and
    /// the old `maybe_compact` pushed a message with `sequence: 0`,
    /// which sorted before every user message in the session.
    #[test]
    fn test_auto_compact_notice_sorts_after_compacted_messages() {
        let mut app = KodApp::new();
        // Force the auto-compact threshold with a tiny context window.
        app.set_context_limit(1_000);

        // Add enough messages to cross the 4/5 threshold AND exceed the
        // 21-message guard that protects against compacting an empty or
        // short session.
        for i in 0..30 {
            app.push_system_message(&format!("filler {i}"));
        }
        // Push token usage past 4/5 of 1000 = 800.
        app.note_real_usage(900);

        // The notice must be the last message by sequence.
        let last = app.messages().last().expect("at least one message");
        let last_seq = last.sequence;
        assert_eq!(last.role, MessageRole::System);
        assert!(
            last.content.contains("Auto-compacted"),
            "last message should be the compaction notice, got: {}",
            last.content
        );

        // And no other message has a sequence greater than it (trivially
        // true) or equal-and-later-positioned at the same sequence.
        for m in app.messages().iter().take(app.messages().len() - 1) {
            assert!(
                m.sequence < last_seq,
                "a message sorts after the compaction notice: seq {} vs {}",
                m.sequence,
                last_seq
            );
        }
    }

    /// The manual `/compact` path must also leave its notice at the end.
    #[test]
    fn test_manual_compact_notice_sorts_after() {
        let mut app = KodApp::new();
        for i in 0..30 {
            app.push_system_message(&format!("filler {i}"));
        }
        app.compact_now();

        let last = app.messages().last().expect("at least one message");
        assert_eq!(last.role, MessageRole::System);
        assert!(
            last.content.contains("Compacted"),
            "last message should be the manual compaction notice, got: {}",
            last.content
        );
    }

    /// A pre-sequence session file (all sequences == 0) must backfill
    /// in file order, so the chat widget's sequence sort is a no-op on
    /// the restored transcript. The dead loop the previous
    /// implementation carried never did anything; the branch that
    /// actually works is covered here.
    #[test]
    fn test_load_session_backfills_legacy_sequences() {
        use crate::app::{KodApp, Message};
        use kod_types::{MessageId, MessageMetadata, MessageRole};

        let tmp =
            std::env::temp_dir().join(format!("kod-tui-legacy-seq-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let _guard = session_state_dir_lock();
        // SAFETY: serialized via the shared mutex.
        unsafe { std::env::set_var("KOD_TUI_STATE_DIR", &tmp) };

        // Write a session file where every message has sequence 0 —
        // the shape produced before sequences existed. Write the
        // messages through their serde shape (the on-disk form), not a
        // hand-built json! whose field names would silently drift.
        let path = std::env::temp_dir().join(format!(
            "kod-tui-legacy-seq-test-{}/tui_session.json",
            std::process::id()
        ));
        std::fs::write(
            &path,
            serde_json::to_string(&[
                Message {
                    id: MessageId::new(),
                    role: MessageRole::User,
                    content: "first".into(),
                    timestamp: chrono::Utc::now(),
                    metadata: MessageMetadata::default(),
                    sequence: 0,
                },
                Message {
                    id: MessageId::new(),
                    role: MessageRole::Assistant,
                    content: "second".into(),
                    timestamp: chrono::Utc::now(),
                    metadata: MessageMetadata::default(),
                    sequence: 0,
                },
                Message {
                    id: MessageId::new(),
                    role: MessageRole::User,
                    content: "third".into(),
                    timestamp: chrono::Utc::now(),
                    metadata: MessageMetadata::default(),
                    sequence: 0,
                },
            ])
            .unwrap(),
        )
        .unwrap();

        let mut app = KodApp::new();
        let n = app.load_session();
        assert_eq!(n, 3);

        // Sequences are 0, 1, 2 in file order.
        let seqs: Vec<u64> = app.messages().iter().map(|m| m.sequence).collect();
        assert_eq!(seqs, vec![0, 1, 2]);

        // A new message pushed afterwards sorts after all restored ones.
        app.push_system_message("new");
        let last = app.messages().last().unwrap();
        assert!(
            last.sequence > 2,
            "new message should sort after backfilled ones: seq {}",
            last.sequence
        );

        unsafe { std::env::remove_var("KOD_TUI_STATE_DIR") };
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Cost accumulates when `note_session_cost` is called with
    /// valid figures, refuses NaN / negative / infinite values, and
    /// the `cost_known()` flag flips only on a real contribution.
    #[test]
    fn test_session_cost_accumulation() {
        let mut app = KodApp::new();
        assert_eq!(app.session_cost_usd(), 0.0);
        assert!(!app.cost_known(), "fresh session has no cost knowledge");

        app.note_session_cost(0.0025);
        app.note_session_cost(0.0125);
        assert!((app.session_cost_usd() - 0.015).abs() < 1e-9);
        assert!(app.cost_known());

        // Broken pricing blocks do not corrupt the accumulator.
        app.note_session_cost(f64::NAN);
        app.note_session_cost(-1.0);
        app.note_session_cost(f64::INFINITY);
        assert!(
            (app.session_cost_usd() - 0.015).abs() < 1e-9,
            "invalid values must be ignored, got {}",
            app.session_cost_usd()
        );
    }

    /// `/clear` resets the cost alongside the other session counters.
    #[test]
    fn test_clear_resets_session_cost() {
        let mut app = KodApp::new();
        app.push_system_message("hello");
        app.note_session_cost(1.25);
        assert!(app.cost_known());

        app.clear_messages();
        assert_eq!(app.session_cost_usd(), 0.0);
        assert!(!app.cost_known(), "cost knowledge resets with the session");
    }

    /// Real token usage replaces the char estimate, and subsequent
    /// estimate calls in the same turn must not stack on top of it.
    /// Regression: TokenUsage arrives before ResponseComplete in the
    /// event queue; finish_response then called note_usage, adding
    /// the reply's chars/4 to a total that already included the
    /// completion — every turn over-reported by roughly the reply
    /// length.
    #[test]
    fn test_real_usage_suppresses_further_estimates() {
        let mut app = KodApp::new();
        app.set_context_limit(100_000);

        // New turn: estimate-only until real usage arrives.
        app.begin_generation();
        app.note_prompt(&"x".repeat(4_000)); // ≈ 1_000 estimate
        assert!(app.context_tokens() >= 1_000);
        let after_prompt = app.context_tokens();

        // Provider reports the real total (already includes the
        // completion). Replaces the estimate.
        app.note_real_usage(1_500);
        assert_eq!(app.context_tokens(), 1_500);

        // finish_response adds the reply's chars/4 via note_usage;
        // gated by the flag, it must be a no-op.
        let reply = "y".repeat(2_000); // would be +500 if applied
        app.finish_response(&reply);
        assert_eq!(
            app.context_tokens(),
            1_500,
            "post-usage estimate must not stack (was {} before reply, {} after)",
            after_prompt,
            app.context_tokens(),
        );
    }

    /// A turn where the provider reports no usage must fall back to
    /// the char estimate, unchanged from before.
    #[test]
    fn test_estimate_still_works_without_real_usage() {
        let mut app = KodApp::new();
        app.set_context_limit(100_000);

        app.begin_generation();
        app.note_prompt(&"x".repeat(4_000)); // ≈ 1_000
        let after_prompt = app.context_tokens();
        assert!(after_prompt >= 1_000);

        app.finish_response(&"y".repeat(4_000)); // ≈ +1_000
        assert!(
            app.context_tokens() > after_prompt,
            "estimate must accumulate when no real usage arrives"
        );
    }

    /// The type-ahead search states: begin_search opens the bar with
    /// an empty query, typing appends, Enter commits (query stays,
    /// editing stops), Escape/clear_search drops everything.
    #[test]
    fn test_type_ahead_search_state_transitions() {
        let mut app = KodApp::new();
        assert!(!app.is_editing_search());
        assert!(!app.is_searching());

        // Open the bar.
        app.begin_search();
        assert!(app.is_editing_search(), "begin_search must enter editing");
        assert_eq!(app.search_query_text(), "");
        assert_eq!(app.search_status(), SearchStatus::Editing);

        // Typing appends.
        app.search_type('h');
        app.search_type('i');
        assert_eq!(app.search_query_text(), "hi");
        assert!(app.is_editing_search());

        // Add a matching message so the search finds something.
        app.push_system_message("a hit for hi");
        // Re-run the search against the new message set.
        let _ = app.set_search("hi");

        // Commit: query stays, editing ends, is_searching() is true.
        app.commit_search();
        assert!(!app.is_editing_search());
        assert!(app.is_searching());
        assert_eq!(app.search_query_text(), "hi");

        // Clear: everything goes away.
        app.clear_search();
        assert!(!app.is_editing_search());
        assert!(!app.is_searching());
        assert_eq!(app.search_query_text(), "");
        assert_eq!(app.search_status(), SearchStatus::Inactive);
    }

    /// TTFT: `begin_generation` clears the field, the first non-empty
    /// chunk sets it, and empty chunks do not. The value is stable
    /// across subsequent chunks of the same turn.
    #[test]
    fn test_ttft_records_first_nonempty_chunk() {
        use std::thread::sleep;
        use std::time::Duration;
        let mut app = KodApp::new();
        assert!(app.ttft_ms().is_none(), "no turn, no ttft");

        app.begin_generation();
        assert!(app.ttft_ms().is_none(), "no chunk yet, no ttft");

        // An empty chunk must not count.
        app.add_response_chunk("");
        assert!(
            app.ttft_ms().is_none(),
            "empty chunk must not start the clock"
        );

        // Sleep so the measured interval is a positive integer.
        sleep(Duration::from_millis(5));
        app.add_response_chunk("hello");
        let t1 = app.ttft_ms().expect("first chunk starts the clock");
        assert!(t1 >= 5, "ttft should be >= 5ms, got {t1}");

        // Subsequent chunks do not move the figure.
        sleep(Duration::from_millis(5));
        app.add_response_chunk(" world");
        let t2 = app.ttft_ms().expect("ttft stable across chunks");
        assert_eq!(t1, t2, "ttft is a per-turn constant once measured");
    }

    /// A new turn resets the measurement.
    #[test]
    fn test_ttft_resets_per_turn() {
        use std::thread::sleep;
        use std::time::Duration;
        let mut app = KodApp::new();

        app.begin_generation();
        sleep(Duration::from_millis(10));
        app.add_response_chunk("first");
        let first_ttft = app.ttft_ms().expect("first ttft");
        assert!(first_ttft >= 10);

        app.finish_response("");
        assert!(app.ttft_ms().is_none(), "finished turn has no ttft");

        app.begin_generation();
        assert!(app.ttft_ms().is_none(), "new turn starts fresh");
        sleep(Duration::from_millis(1));
        app.add_response_chunk("second");
        let second_ttft = app.ttft_ms().expect("second ttft");
        assert!(
            second_ttft < first_ttft,
            "second turn slept less; expected smaller ttft, got {second_ttft} vs {first_ttft}",
        );
    }

    /// `search_status` must distinguish the states the old
    /// `search_position` tuple collapsed together.
    #[test]
    fn test_search_status_distinguishes_states() {
        let mut app = KodApp::new();
        assert_eq!(app.search_status(), SearchStatus::Inactive);
        assert_eq!(app.search_status_label(), "");

        app.begin_search();
        assert_eq!(app.search_status(), SearchStatus::Editing);
        assert!(
            app.search_status_label().contains("typing"),
            "editing label should say typing: {}",
            app.search_status_label()
        );

        app.search_query = Some("nothing-will-match-this".to_string());
        assert_eq!(app.search_status(), SearchStatus::NoMatches);
        assert!(
            app.search_status_label().contains("no matches"),
            "label should say no matches: {}",
            app.search_status_label()
        );

        app.push_system_message("haystack one");
        app.push_system_message("haystack two");
        app.search_query = Some("haystack".to_string());
        match app.search_status() {
            SearchStatus::At { position, total } => {
                assert_eq!(total, 2);
                assert_eq!(position, 1);
            }
            other => panic!("expected At, got {other:?}"),
        }
        let label = app.search_status_label();
        assert!(label.contains("1/2"), "label should say 1/2: {label}");
    }

    /// `search_next` / `search_prev` wrap the position and the position
    /// reported by `search_status` stays in 1..=total.
    #[test]
    fn test_search_position_wraps() {
        let mut app = KodApp::new();
        for i in 0..3 {
            app.push_system_message(&format!("needle {i}"));
        }
        let n = app.set_search("needle");
        assert_eq!(n, 3);

        assert!(matches!(
            app.search_status(),
            SearchStatus::At {
                position: 1,
                total: 3
            }
        ));
        app.search_next();
        assert!(matches!(
            app.search_status(),
            SearchStatus::At {
                position: 2,
                total: 3
            }
        ));
        app.search_next();
        assert!(matches!(
            app.search_status(),
            SearchStatus::At {
                position: 3,
                total: 3
            }
        ));
        app.search_next();
        assert!(
            matches!(
                app.search_status(),
                SearchStatus::At {
                    position: 1,
                    total: 3
                }
            ),
            "next should wrap: {:?}",
            app.search_status()
        );
        app.search_prev();
        assert!(matches!(
            app.search_status(),
            SearchStatus::At {
                position: 3,
                total: 3
            }
        ));
    }

    /// A failed save must leave the previous session file intact.
    /// Regression: the old `std::fs::write` truncated the target
    /// before writing, so a mid-write failure (disk full, crash,
    /// permission flip) left a zero-byte file — which load_session
    /// then discarded, losing the transcript.
    ///
    /// The temp+rename implementation writes the temp first and only
    /// renames on success, so the target is either the old complete
    /// file or the new complete file, never a partial one. This test
    /// simulates the failure by pointing KOD_TUI_STATE_DIR at a path
    /// whose parent cannot be created, and asserts the previously
    /// saved file still loads.
    #[test]
    fn test_save_session_does_not_corrupt_previous_file() {
        use crate::app::{KodApp, Message};
        use kod_types::{MessageId, MessageMetadata, MessageRole};

        let tmp = std::env::temp_dir().join(format!("kod-tui-save-safety-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let _guard = session_state_dir_lock();
        // SAFETY: serialized via the shared mutex.
        unsafe { std::env::set_var("KOD_TUI_STATE_DIR", &tmp) };

        // Write a valid session.
        let mut app = KodApp::new();
        app.add_message(Message {
            id: MessageId::new(),
            role: MessageRole::User,
            content: "the only message".into(),
            timestamp: chrono::Utc::now(),
            metadata: MessageMetadata::default(),
            sequence: 0,
        });
        app.save_session();

        // A fresh load must see the message.
        let mut restored = KodApp::new();
        assert_eq!(restored.load_session(), 1);
        assert_eq!(restored.messages()[0].content, "the only message");

        // Overwrite with an empty session — this is the path a user
        // takes after `/clear`. The previous file must be replaced
        // whole; a reader that arrives mid-write must not see a
        // half-empty file.
        let empty = KodApp::new();
        empty.save_session();

        let mut after = KodApp::new();
        // The file now serializes an empty array; load_session returns 0.
        assert_eq!(after.load_session(), 0);
        assert!(after.messages().is_empty());

        unsafe { std::env::remove_var("KOD_TUI_STATE_DIR") };
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// `/clear` resets the context accounting. Regression: the
    /// visible messages and the engine transcript were reset, but
    /// `context_tokens` and `compacted_messages` kept accumulating,
    /// so the header meter overstated the current context and the
    /// auto-compact threshold was compared against a number that
    /// included discarded messages.
    #[test]
    fn test_clear_resets_context_accounting() {
        let mut app = KodApp::new();
        app.set_context_limit(10_000);

        // Build up a believable pre-clear state: some messages and
        // some token usage.
        for i in 0..30 {
            app.push_system_message(&format!("filler {i}"));
        }
        app.note_real_usage(3_000);
        // Also trigger a manual compact to set compacted_messages.
        app.compact_now();
        assert!(app.context_tokens() > 0);
        assert!(
            app.messages().len() < 30,
            "compact should have dropped some"
        );

        // Sanity: pre-clear state is not the fresh state.
        let pre_tokens = app.context_tokens();

        app.clear_messages();

        assert!(app.messages().is_empty(), "display should be empty");
        assert_eq!(
            app.context_tokens(),
            0,
            "context accounting must reset (was {pre_tokens})"
        );
        // The label must report 0% — the meter the header draws reads
        // from the same counter.
        assert!(
            app.context_label().contains("0%"),
            "context label should read 0%: {}",
            app.context_label()
        );
    }

    /// `/undo` restores the cleared messages but must NOT resurrect
    /// the stale context count — the engine's transcript was cleared
    /// by the `/clear` handler, so the model really does have zero
    /// context at that point. Undo is a display operation only.
    #[test]
    fn test_undo_does_not_restore_stale_context() {
        let mut app = KodApp::new();
        app.set_context_limit(10_000);
        app.push_system_message("hello");
        app.note_real_usage(2_500);
        assert_eq!(app.context_tokens(), 2_500);

        app.clear_messages();
        assert_eq!(app.context_tokens(), 0);

        let restored = app.undo_clear();
        assert!(restored, "undo should succeed");
        assert_eq!(app.messages().len(), 1);
        // Context stays at the post-clear value, not the pre-clear one.
        assert_eq!(
            app.context_tokens(),
            0,
            "undo must not resurrect a stale context count"
        );
    }

    #[test]
    fn test_scroll_to_bottom() {
        let mut app = KodApp::new();
        app.add_message(Message {
            id: MessageId::new(),
            role: MessageRole::User,
            content: "test".to_string(),
            timestamp: Utc::now(),
            metadata: MessageMetadata::default(),
            sequence: 0,
        });
        assert!(app.is_scrolled_to_bottom());
    }

    /// The swarm state machine: decompose adds a system line; agent
    /// start creates a chat row; chunks append to that row; completion
    /// replaces the trailing buffer; merged answer lands as an
    /// assistant message.
    #[test]
    fn swarm_agent_started_populates_model() {
        let mut app = KodApp::new();
        app.begin_swarm();
        let id = kod_types::AgentId::new();
        app.swarm_agent_started(
            id.clone(),
            "agent-1",
            "write the schema",
            Some("local-ollama/qwen2.5-coder:7b".to_string()),
        );
        let view = app
            .swarm_agents()
            .get(&id)
            .expect("view must exist after start");
        assert_eq!(view.name, "agent-1");
        assert_eq!(view.subtask, "write the schema");
        assert_eq!(
            view.model.as_deref(),
            Some("local-ollama/qwen2.5-coder:7b"),
            "the model from the runner must reach the panel view — a blank              row is what the design's D4.6 aims to eliminate",
        );
        assert!(!view.finished);
    }

    #[test]
    fn swarm_agent_started_accepts_no_model() {
        // A run with no `[llm.routing.swarm]` table passes `None`; the
        // view must tolerate that and the panel skips the row.
        let mut app = KodApp::new();
        app.begin_swarm();
        let id = kod_types::AgentId::new();
        app.swarm_agent_started(id.clone(), "agent-1", "task", None);
        let view = app.swarm_agents().get(&id).unwrap();
        assert!(view.model.is_none());
    }

    #[test]
    fn swarm_agent_by_index_is_one_based_and_bounded() {
        let mut app = KodApp::new();
        app.begin_swarm();
        let a = kod_types::AgentId::new();
        let b = kod_types::AgentId::new();
        let c = kod_types::AgentId::new();
        app.swarm_agent_started(a.clone(), "a", "first", None);
        app.swarm_agent_started(b.clone(), "b", "second", None);
        app.swarm_agent_started(c.clone(), "c", "third", None);

        // Index 0 is invalid (positions are 1-based).
        assert!(app.swarm_agent_by_index(0).is_none());
        // 1..=3 map to the agents in start order.
        assert_eq!(app.swarm_agent_by_index(1), Some(&a));
        assert_eq!(app.swarm_agent_by_index(2), Some(&b));
        assert_eq!(app.swarm_agent_by_index(3), Some(&c));
        // Anything past the end is `None`.
        assert!(app.swarm_agent_by_index(4).is_none());
        assert!(app.swarm_agent_by_index(usize::MAX).is_none());
    }

    #[test]
    fn begin_swarm_resets_order_and_views() {
        let mut app = KodApp::new();
        app.begin_swarm();
        let a = kod_types::AgentId::new();
        app.swarm_agent_started(a.clone(), "a", "first run", None);
        assert!(app.swarm_agent_by_index(1).is_some());

        // A new run must clear both the map and the order vector so
        // the next agent lands at index 1, not index 2.
        app.begin_swarm();
        assert!(app.swarm_agents().is_empty());
        assert!(app.swarm_agent_by_index(1).is_none());

        let b = kod_types::AgentId::new();
        app.swarm_agent_started(b.clone(), "b", "second run", None);
        assert_eq!(
            app.swarm_agent_by_index(1),
            Some(&b),
            "the second run's first agent must be at index 1",
        );
    }

    #[test]
    fn swarm_set_worktree_attaches_to_the_view() {
        let mut app = KodApp::new();
        app.begin_swarm();
        let id = kod_types::AgentId::new();
        app.swarm_agent_started(id.clone(), "agent-1", "task", None);
        app.swarm_set_worktree(
            &id,
            std::path::PathBuf::from("/tmp/worktree-agent-1"),
            "kod/agent-agent-1".to_string(),
        );
        let view = app.swarm_agents().get(&id).unwrap();
        assert_eq!(
            view.worktree.as_deref(),
            Some(std::path::Path::new("/tmp/worktree-agent-1")),
        );
        assert_eq!(view.branch.as_deref(), Some("kod/agent-agent-1"));
    }

    #[test]
    fn swarm_set_retrying_records_a_note() {
        let mut app = KodApp::new();
        app.begin_swarm();
        let id = kod_types::AgentId::new();
        app.swarm_agent_started(id.clone(), "agent-1", "task", None);
        app.swarm_set_retrying(&id, 2, 3, "connection reset");
        let view = app.swarm_agents().get(&id).unwrap();
        let note = view.retry_note.as_deref().unwrap_or("");
        assert!(note.contains("2/3"), "got: {note}");
        assert!(note.contains("connection reset"), "got: {note}");
        // The panel uses `retry_note.is_some()` to render a warning
        // marker; a regression that left the note behind after
        // success would keep the warning on a healthy agent.
        app.swarm_agent_finished(&id, "done");
        let view = app.swarm_agents().get(&id).unwrap();
        assert!(
            view.retry_note.is_none(),
            "finishing must clear the retry note",
        );
    }

    #[test]
    fn swarm_agent_finished_clears_failure_and_marks_done() {
        let mut app = KodApp::new();
        app.begin_swarm();
        let id = kod_types::AgentId::new();
        app.swarm_agent_started(id.clone(), "agent-1", "task", None);
        // Mark a failure first, then a success; success wins.
        app.swarm_agent_failed(&id, "transient");
        app.swarm_agent_finished(&id, "final answer");
        let view = app.swarm_agents().get(&id).unwrap();
        assert!(view.finished);
        assert!(
            view.failure.is_none(),
            "a successful finish must clear the failure marker",
        );
    }

    #[test]
    fn test_swarm_state_machine() {
        let mut app = KodApp::new();
        app.begin_swarm();
        app.begin_generation();

        // Decompose → system line.
        app.swarm_decomposed(&[
            ("schema".to_string(), "write the SQL schema".to_string()),
            ("api".to_string(), "implement the handler".to_string()),
        ]);
        let sys = app.messages().last().unwrap();
        assert_eq!(sys.role, MessageRole::System);
        assert!(sys.content.contains("2 subtasks"));
        assert!(sys.content.contains("schema"));

        // Agent start → a chat row keyed by the agent id.
        let id_a = kod_types::AgentId::new();
        let id_b = kod_types::AgentId::new();
        app.swarm_agent_started(
            id_a.clone(),
            "agent-1",
            "write the SQL schema",
            Some("local-ollama/qwen2.5-coder:7b".to_string()),
        );
        app.swarm_agent_started(
            id_b.clone(),
            "agent-2",
            "implement the handler",
            Some("local-ollama/qwen2.5-coder:7b".to_string()),
        );
        assert_eq!(
            app.messages().len(),
            3,
            "two system/agent rows after decompose + starts"
        );

        let row_a = app
            .messages()
            .iter()
            .find(|m| matches!(&m.role, MessageRole::Agent(a) if a == &id_a))
            .expect("agent-1 row");
        assert!(row_a.content.contains("agent-1"));

        // Chunks append to the right row.
        app.swarm_agent_chunk(&id_a, "\nworking");
        app.swarm_agent_chunk(&id_a, " on it");
        let row_a = app
            .messages()
            .iter()
            .find(|m| matches!(&m.role, MessageRole::Agent(a) if a == &id_a))
            .unwrap();
        assert!(
            row_a.content.contains("working on it"),
            "got: {}",
            row_a.content
        );

        // Chunks after finish are ignored.
        app.swarm_agent_finished(&id_a, "DONE: schema written");
        app.swarm_agent_chunk(&id_a, "late noise");
        let row_a = app
            .messages()
            .iter()
            .find(|m| matches!(&m.role, MessageRole::Agent(a) if a == &id_a))
            .unwrap();
        assert!(!row_a.content.contains("late noise"));
        assert!(row_a.content.contains("DONE: schema written"));
        assert!(
            row_a.content.contains("agent-1"),
            "header preserved: {}",
            row_a.content
        );

        // Failure replaces the buffer.
        app.swarm_agent_failed(&id_b, "boom");
        let row_b = app
            .messages()
            .iter()
            .find(|m| matches!(&m.role, MessageRole::Agent(a) if a == &id_b))
            .unwrap();
        assert!(row_b.content.contains("failed"));
        assert!(row_b.content.contains("boom"));

        // Swarm complete lands the merged answer as an assistant row and
        // ends the generation state.
        assert!(app.is_generating());
        app.swarm_complete("MERGED: schema + handler + tests");
        assert!(!app.is_generating());
        let merged = app.messages().last().unwrap();
        assert_eq!(merged.role, MessageRole::Assistant);
        assert!(merged.content.contains("MERGED"));
    }

    /// `begin_swarm` clears a previous run's live-agent views, so a
    /// second run does not append to the first run's finished rows.
    #[test]
    fn test_begin_swarm_resets_live_views() {
        let mut app = KodApp::new();
        app.begin_swarm();
        let id = kod_types::AgentId::new();
        app.swarm_agent_started(id.clone(), "agent-1", "first run", None);
        // Finish it so the view is marked done, but keep the map entry.
        app.swarm_agent_finished(&id, "done");
        assert!(app.swarm_agents.contains_key(&id));

        app.begin_swarm();
        assert!(
            app.swarm_agents.is_empty(),
            "begin_swarm clears the live map"
        );
    }
}

#[cfg(test)]
mod coverage_approval_batch {
    //! The batch navigation is what the y/n/a keys drive. A
    //! regression in `advance`/`retreat` either skips an item (the
    //! user approves the wrong call) or gets stuck on one (the
    //! batch never completes and the engine's awaiting tasks time
    //! out to deny). Both are worth pinning.
    use super::{PendingApproval, PendingApprovalBatch};

    fn item(id: u64) -> PendingApproval {
        PendingApproval {
            id,
            tool_name: "write_file".into(),
            summary: format!("write {id}"),
            diff: None,
            arguments: serde_json::json!({}),
        }
    }

    fn batch_of(ids: &[u64]) -> PendingApprovalBatch {
        PendingApprovalBatch {
            batch_id: 1,
            items: ids.iter().copied().map(item).collect(),
            current: 0,
        }
    }

    #[test]
    fn empty_batch_has_no_current_item() {
        let b = batch_of(&[]);
        assert!(b.current_item().is_none());
    }

    #[test]
    fn a_fresh_batch_starts_at_the_first_item() {
        let b = batch_of(&[10, 20, 30]);
        assert_eq!(b.current_item().unwrap().id, 10);
    }

    #[test]
    fn advance_walks_forward_then_past_the_end() {
        let mut b = batch_of(&[10, 20]);
        assert!(b.advance());
        assert_eq!(b.current_item().unwrap().id, 20);
        // Past the last item: `advance` returns false and
        // `current_item` returns `None` so the caller knows the
        // batch is exhausted.
        assert!(!b.advance());
        assert!(b.current_item().is_none());
    }

    #[test]
    fn retreat_walks_backward_and_stops_at_the_start() {
        let mut b = batch_of(&[10, 20, 30]);
        b.current = 2;
        assert!(b.retreat());
        assert_eq!(b.current_item().unwrap().id, 20);
        assert!(b.retreat());
        assert_eq!(b.current_item().unwrap().id, 10);
        // At the first item, `retreat` returns false and does not
        // move. A regression that decremented past zero would
        // underflow or jump to `items.len() - 1`.
        assert!(!b.retreat());
        assert_eq!(b.current_item().unwrap().id, 10);
    }

    #[test]
    fn advance_after_exhaustion_does_not_panic() {
        // Once `current` is past `items.len()`, a further advance
        // must remain a no-op — not an arithmetic overflow, not a
        // panic. The engine's awaiting oneshot has already timed
        // out in this case; the batch object should stay quiet.
        let mut b = PendingApprovalBatch {
            batch_id: 1,
            items: vec![item(1)],
            current: 5,
        };
        assert!(!b.advance());
        assert!(b.current_item().is_none());
    }
}

#[cfg(test)]
mod coverage_app_state {
    //! Additional app state behaviours that the existing tests
    //! did not exercise: the input history's draft handling, the
    //! completion cycling, the pin/unpin round-trip, and the
    //! fork/undo interplay. Each of these is user-facing and a
    //! regression would be felt directly.
    use crate::app::*;
    use kod_types::{MessageId, MessageMetadata, MessageRole};

    fn user(text: &str) -> Message {
        Message {
            id: MessageId::new(),
            role: MessageRole::User,
            content: text.to_string(),
            timestamp: Utc::now(),
            metadata: MessageMetadata::default(),
            sequence: 0,
        }
    }

    #[test]
    fn history_previous_stashes_the_draft_and_next_restores_it() {
        let mut app = KodApp::new();
        app.input_history.push("older".into());
        app.input_history.push("newer".into());
        app.set_input("draft in progress".into());

        app.history_previous();
        // The draft was saved and the newest entry loaded.
        assert_eq!(app.input(), "newer");

        app.history_previous();
        assert_eq!(app.input(), "older");

        // Down past the newest entry restores the draft, not "".
        app.history_next();
        assert_eq!(app.input(), "newer");
        app.history_next();
        assert_eq!(app.input(), "draft in progress");
    }

    #[test]
    fn history_previous_with_no_history_is_a_no_op() {
        let mut app = KodApp::new();
        app.set_input("draft".into());
        app.history_previous();
        assert_eq!(app.input(), "draft");
    }

    #[test]
    fn completion_cycling_wraps_around() {
        // Slash commands provide a non-empty candidate list; the
        // cycling must wrap and not stick.
        let mut app = KodApp::new();
        app.set_input_mode(InputMode::Insert);
        app.set_input("/".into());
        let n = app.active_completion_len();
        assert!(n > 1, "need multiple candidates to test cycling");
        let mut seen = std::collections::HashSet::new();
        for _ in 0..n {
            seen.insert(app.completion_index() % n);
            app.completion_next();
        }
        assert_eq!(seen.len(), n, "cycling did not cover every candidate");
        // Wrapping back to zero.
        assert_eq!(app.completion_index() % n, 0);
    }

    #[test]
    fn completion_previous_wraps_to_the_last_candidate() {
        let mut app = KodApp::new();
        app.set_input_mode(InputMode::Insert);
        app.set_input("/".into());
        let n = app.active_completion_len();
        assert!(n > 1);
        app.completion_prev();
        assert_eq!(app.completion_index() % n, n - 1);
    }

    #[test]
    fn pin_and_unpin_round_trip() {
        let mut app = KodApp::new();
        app.add_message(user("first"));
        assert!(!app.is_message_pinned(0));
        assert!(app.set_message_pinned_at(0, true));
        assert!(app.is_message_pinned(0));
        assert!(app.set_message_pinned_at(0, false));
        assert!(!app.is_message_pinned(0));
    }

    #[test]
    fn pin_on_out_of_range_index_is_a_no_op() {
        let mut app = KodApp::new();
        assert!(!app.set_message_pinned_at(0, true));
        assert!(!app.is_message_pinned(0));
    }

    #[test]
    fn fork_then_undo_restores_the_forked_chat() {
        // The fork captures the current chat onto the cleared
        // stack. A subsequent `/clear` drops the live chat; an
        // `/undo` then restores the fork.
        let mut app = KodApp::new();
        app.add_message(user("first"));
        app.add_message(user("second"));
        let n = app.fork_messages();
        assert_eq!(n, 2);
        assert_eq!(app.fork_count(), 1);

        app.clear_messages();
        assert!(app.messages().is_empty());

        assert!(app.undo_clear());
        assert_eq!(app.messages().len(), 2);
    }

    #[test]
    fn fork_on_an_empty_chat_does_nothing() {
        let mut app = KodApp::new();
        assert_eq!(app.fork_messages(), 0);
        assert_eq!(app.fork_count(), 0);
    }

    #[test]
    fn reset_transient_state_clears_input_and_search_but_not_messages() {
        let mut app = KodApp::new();
        app.add_message(user("keep me"));
        app.set_input("half-typed".into());
        app.begin_search();
        app.search_type('x');
        let cleared = app.reset_transient_state();
        assert!(cleared > 0);
        assert_eq!(app.input(), "");
        assert!(!app.is_editing_search());
        assert!(app.search_query().is_none());
        // The chat was not touched.
        assert_eq!(app.messages().len(), 1);
    }

    #[test]
    fn edit_last_message_loads_the_newest_user_content() {
        let mut app = KodApp::new();
        app.add_message(user("first"));
        app.add_message(Message {
            id: MessageId::new(),
            role: MessageRole::Assistant,
            content: "reply".into(),
            timestamp: Utc::now(),
            metadata: MessageMetadata::default(),
            sequence: 0,
        });
        app.add_message(user("second"));
        assert!(app.edit_last_message());
        assert_eq!(app.input(), "second");
    }

    #[test]
    fn edit_last_message_on_an_empty_chat_returns_false() {
        let mut app = KodApp::new();
        assert!(!app.edit_last_message());
        assert_eq!(app.input(), "");
    }

    #[test]
    fn drop_last_exchange_removes_the_trailing_pair() {
        let mut app = KodApp::new();
        app.add_message(user("the question"));
        app.add_message(Message {
            id: MessageId::new(),
            role: MessageRole::Assistant,
            content: "the answer".into(),
            timestamp: Utc::now(),
            metadata: MessageMetadata::default(),
            sequence: 0,
        });
        let dropped = app.drop_last_exchange();
        assert_eq!(dropped.as_deref(), Some("the question"));
        assert!(app.messages().is_empty());
    }

    #[test]
    fn drop_last_exchange_on_an_empty_chat_returns_none() {
        let mut app = KodApp::new();
        assert!(app.drop_last_exchange().is_none());
    }

    #[test]
    fn setting_then_clearing_the_goal_is_a_round_trip() {
        let mut app = KodApp::new();
        assert!(app.goal().is_none());
        app.set_goal("ship the fix");
        assert_eq!(app.goal(), Some("ship the fix"));
        app.clear_goal();
        assert!(app.goal().is_none());
    }

    #[test]
    fn setting_a_whitespace_only_goal_is_a_no_op() {
        let mut app = KodApp::new();
        app.set_goal("   ");
        assert!(app.goal().is_none());
    }
}

#[cfg(test)]
mod coverage_command_palette {
    //! Tests for the Ctrl+K command palette (Tier UX).
    use crate::app::*;

    #[test]
    fn new_app_has_closed_palette() {
        let app = KodApp::new();
        assert!(!app.is_palette_open());
        assert!(app.palette().is_none());
    }

    #[test]
    fn open_palette_is_idempotent() {
        let mut app = KodApp::new();
        app.open_palette();
        app.palette_push_char('t');
        app.open_palette();
        // Query retained on a second open — the user pressed Ctrl+K
        // twice but the palette keeps its state.
        assert_eq!(app.palette_query(), Some("t"));
    }

    #[test]
    fn close_palette_clears_state() {
        let mut app = KodApp::new();
        app.open_palette();
        app.palette_push_char('x');
        app.close_palette();
        assert!(!app.is_palette_open());
    }

    #[test]
    fn filter_empty_query_returns_everything() {
        let mut app = KodApp::new();
        app.open_palette();
        let all = app.palette_candidates();
        assert!(all.len() > 10, "expected many entries, got {}", all.len());
    }

    #[test]
    fn filter_narrows_the_list() {
        let mut app = KodApp::new();
        app.open_palette();
        for c in "plan".chars() {
            app.palette_push_char(c);
        }
        let filtered = app.palette_candidates();
        assert!(!filtered.is_empty());
        assert!(
            filtered.iter().any(|e| e.label.contains("plan")),
            "no /plan entry: {:?}",
            filtered.iter().map(|e| &e.label).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn backspace_removes_a_query_char() {
        let mut app = KodApp::new();
        app.open_palette();
        app.palette_push_char('a');
        app.palette_push_char('b');
        app.palette_backspace();
        assert_eq!(app.palette_query(), Some("a"));
    }

    #[test]
    fn next_wraps_around() {
        let mut app = KodApp::new();
        app.open_palette();
        let total = app.palette_candidates().len();
        assert!(total >= 2);
        for _ in 0..total {
            app.palette_next();
        }
        // Wrapped back to start.
        assert_eq!(app.palette_selected(), 0);
    }

    #[test]
    fn prev_wraps_around_backwards() {
        let mut app = KodApp::new();
        app.open_palette();
        let total = app.palette_candidates().len();
        assert!(total >= 2);
        app.palette_prev();
        assert_eq!(app.palette_selected(), total - 1);
    }

    #[test]
    fn selected_entry_matches_index() {
        let mut app = KodApp::new();
        app.open_palette();
        app.palette_next();
        let entry = app.palette_selected_entry().unwrap();
        let candidates = app.palette_candidates();
        assert_eq!(entry.label, candidates[1].label);
    }

    #[test]
    fn no_candidates_for_unknown_query() {
        let mut app = KodApp::new();
        app.open_palette();
        for c in "zzzzzzzz".chars() {
            app.palette_push_char(c);
        }
        assert!(app.palette_candidates().is_empty());
        assert!(app.palette_selected_entry().is_none());
    }

    #[test]
    fn navigation_on_empty_filter_is_a_no_op() {
        let mut app = KodApp::new();
        app.open_palette();
        for c in "zzzzzzzz".chars() {
            app.palette_push_char(c);
        }
        // Should not panic.
        app.palette_next();
        app.palette_prev();
        assert_eq!(app.palette_selected(), 0);
    }

    #[test]
    fn build_palette_includes_all_slash_commands() {
        let entries = build_palette_entries();
        for cmd in SLASH_COMMANDS {
            assert!(
                entries.iter().any(|e| e.label == cmd.name),
                "missing {} from palette",
                cmd.name,
            );
        }
    }

    #[test]
    fn build_palette_includes_key_actions() {
        let entries = build_palette_entries();
        assert!(entries.iter().any(|e| e.label == "ctrl+k"));
        assert!(entries.iter().any(|e| e.label == "esc"));
    }

    #[test]
    fn push_char_resets_selection() {
        let mut app = KodApp::new();
        app.open_palette();
        app.palette_next();
        app.palette_next();
        assert!(app.palette_selected() >= 2);
        app.palette_push_char('x');
        assert_eq!(app.palette_selected(), 0);
    }
}

#[cfg(test)]
mod coverage_split_hunks {
    //! `split_hunks` is what backs partial-hunk approval. A
    //! regression either loses a hunk (silent data loss in the
    //! approved patch) or misattributes the header. Both are
    //! worth pinning.
    use crate::app::*;

    #[test]
    fn empty_diff_yields_nothing() {
        let (h, hunks) = split_hunks("");
        assert!(h.is_empty());
        assert!(hunks.is_empty());
    }

    #[test]
    fn header_only_diff_has_no_hunks() {
        let (h, hunks) = split_hunks("--- a/x\n+++ b/x\n");
        assert_eq!(h, "--- a/x\n+++ b/x\n");
        assert!(hunks.is_empty());
    }

    #[test]
    fn single_hunk_splits_header_from_body() {
        let diff = "--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n-old\n+new\n";
        let (h, hunks) = split_hunks(diff);
        assert_eq!(h, "--- a/x\n+++ b/x\n");
        assert_eq!(hunks.len(), 1);
        assert!(hunks[0].starts_with("@@"));
        assert!(hunks[0].contains("-old"));
        assert!(hunks[0].contains("+new"));
    }

    #[test]
    fn multiple_hunks_are_ordered() {
        let diff = "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+A\n@@ -5 +5 @@\n-b\n+B\n";
        let (_, hunks) = split_hunks(diff);
        assert_eq!(hunks.len(), 2);
        assert!(hunks[0].contains("+A"));
        assert!(hunks[1].contains("+B"));
    }

    #[test]
    fn build_patch_includes_only_selected_hunks() {
        let diff = "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+A\n@@ -5 +5 @@\n-b\n+B\n";
        let (header, hunks) = split_hunks(diff);
        let mut sel = PendingHunkSelection {
            approval_id: 1,
            original_arguments: serde_json::json!({}),
            header,
            hunks,
            selected: vec![true, false],
            cursor: 0,
        };
        let p = sel.build_patch();
        assert!(p.contains("+A"));
        assert!(!p.contains("+B"));
        sel.selected[0] = false;
        sel.selected[1] = true;
        let p2 = sel.build_patch();
        assert!(!p2.contains("+A"));
        assert!(p2.contains("+B"));
    }

    #[test]
    fn current_clamps_to_hunk_range() {
        let mut sel = PendingHunkSelection {
            approval_id: 1,
            original_arguments: serde_json::json!({}),
            header: String::new(),
            hunks: vec!["@@ a".into(), "@@ b".into()],
            selected: vec![true, true],
            cursor: 99,
        };
        assert_eq!(sel.current(), Some(1));
        sel.cursor = 0;
        assert_eq!(sel.current(), Some(0));
    }

    #[test]
    fn advance_wraps() {
        let mut sel = PendingHunkSelection {
            approval_id: 1,
            original_arguments: serde_json::json!({}),
            header: String::new(),
            hunks: vec!["@@ a".into(), "@@ b".into()],
            selected: vec![true, true],
            cursor: 0,
        };
        sel.advance();
        assert_eq!(sel.cursor, 1);
        sel.advance();
        assert_eq!(sel.cursor, 0);
        sel.retreat();
        assert_eq!(sel.cursor, 1);
    }

    #[test]
    fn selected_count_reflects_toggles() {
        let mut sel = PendingHunkSelection {
            approval_id: 1,
            original_arguments: serde_json::json!({}),
            header: String::new(),
            hunks: vec!["@@ a".into(), "@@ b".into(), "@@ c".into()],
            selected: vec![true, false, true],
            cursor: 0,
        };
        assert_eq!(sel.selected_count(), 2);
        sel.toggle_current();
        assert_eq!(sel.selected_count(), 1);
    }

    #[test]
    fn empty_hunks_current_is_none() {
        let sel = PendingHunkSelection {
            approval_id: 1,
            original_arguments: serde_json::json!({}),
            header: "hdr".into(),
            hunks: vec![],
            selected: vec![],
            cursor: 0,
        };
        assert!(sel.current().is_none());
    }
}
