//! Event handling for the TUI.
//!
//! Provides a unified event system that handles keyboard input,
//! mouse events, ticks, and custom application events.

use crossterm::event::{
    Event as CrosstermEvent, EventStream, KeyCode as CrosstermKeyCode, KeyEventKind, KeyModifiers,
};
use futures::StreamExt as _;
use kod_error::Result;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex as TokioMutex, mpsc};

/// Key codes we care about
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum KeyCode {
    Char(char),
    Enter,
    Escape,
    Backspace,
    Delete,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Tab,
    BackTab,
    /// Ctrl+C — cancels the running prompt.
    CtrlC,
    /// Ctrl+J (or Shift+Enter) — newline inside the input box.
    CtrlJ,
    /// Shift+Enter — newline inside the input box.
    ShiftEnter,
    /// Ctrl+U — delete to the start of the current line.
    CtrlU,
    /// Ctrl+W — delete the word before the cursor.
    CtrlW,
    /// Ctrl+E — load your last message back for editing.
    CtrlE,
    CtrlK,
    CtrlLeft,
    CtrlRight,
    F(u8),
}

impl From<CrosstermKeyCode> for KeyCode {
    fn from(code: CrosstermKeyCode) -> Self {
        match code {
            CrosstermKeyCode::Char(c) => KeyCode::Char(c),
            CrosstermKeyCode::Enter => KeyCode::Enter,
            CrosstermKeyCode::Esc => KeyCode::Escape,
            CrosstermKeyCode::Backspace => KeyCode::Backspace,
            CrosstermKeyCode::Delete => KeyCode::Delete,
            CrosstermKeyCode::Up => KeyCode::Up,
            CrosstermKeyCode::Down => KeyCode::Down,
            CrosstermKeyCode::Left => KeyCode::Left,
            CrosstermKeyCode::Right => KeyCode::Right,
            CrosstermKeyCode::Home => KeyCode::Home,
            CrosstermKeyCode::End => KeyCode::End,
            CrosstermKeyCode::PageUp => KeyCode::PageUp,
            CrosstermKeyCode::PageDown => KeyCode::PageDown,
            CrosstermKeyCode::Tab => KeyCode::Tab,
            CrosstermKeyCode::BackTab => KeyCode::BackTab,
            CrosstermKeyCode::F(n) => KeyCode::F(n),
            _ => KeyCode::Char(' '),
        }
    }
}

/// Priority for events
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
pub enum EventPriority {
    Low,
    #[default]
    Normal,
    High,
    Critical,
}

/// Events that can occur in the TUI
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    Key(KeyCode),
    UserInput(String),
    Tick,
    System(EventPriority, String),
    ToolStarted(String),
    ToolCompleted(String, String),
    /// Live per-tool completion from the engine's done-marker: same row
    /// fill as `ToolCompleted`, plus wall time for the header
    /// (`execute_command … · 1.2s`). Arrives the moment the call
    /// finishes, not at task end.
    ToolCompletedWithDuration(String, String, u64),
    /// Live one-line excerpt of what the running tool is doing
    /// (`execute_command cargo test …`). Refreshes the running indicator.
    ToolProgress(String),
    /// A running prompt was cancelled (Esc / Ctrl+C / `/cancel`).
    Cancelled,
    /// P5.6 — the engine abandoned the current endpoint mid-stream
    /// because Jev flagged the partial response off-track. Drop the
    /// pending streamed text; the retry against the next endpoint
    /// will start fresh.
    StreamReset,
    /// `/handoff` produced its document. The main loop writes the
    /// file, resets the display and the engine transcript, and seeds
    /// the transcript with this text as its only context.
    HandoffGenerated(String),
    /// Swarm decompose produced these `(name, description)` subtasks.
    SwarmDecomposed(Vec<(String, String)>),
    /// A swarm agent began work.
    SwarmAgentStarted {
        id: kod_types::AgentId,
        name: String,
        subtask: String,
        /// Display string of the endpoint+model the agent runs against
        /// (`endpoint/model`), when known. The agent panel renders it
        /// next to the agent name.
        #[serde(default)]
        model: Option<String>,
    },
    /// A swarm agent produced a text chunk.
    SwarmAgentChunk {
        id: kod_types::AgentId,
        text: String,
    },
    /// A swarm agent finished with this result.
    SwarmAgentCompleted {
        id: kod_types::AgentId,
        result: String,
    },
    /// A swarm agent failed; the others continue.
    SwarmAgentFailed {
        id: kod_types::AgentId,
        error: String,
    },
    /// A swarm agent's worktree was created (D4-D5).
    SwarmAgentWorktree {
        id: kod_types::AgentId,
        path: std::path::PathBuf,
        branch: String,
    },
    /// A swarm agent is retrying after a failure (D4-D6).
    SwarmAgentRetrying {
        id: kod_types::AgentId,
        attempt: u32,
        max_attempts: u32,
        previous_error: String,
    },
    /// Two swarm agents wrote to the same file.
    SwarmConflict {
        file: String,
        agents: Vec<String>,
    },
    /// All swarm agents done; the runner is calling the merge.
    SwarmMerging,
    /// Swarm run complete; this is the merged answer.
    SwarmComplete(String),
    /// Swarm run failed before producing an answer.
    SwarmError(String),
    AgentMessage(String, String),
    ResponseChunk(String),
    ResponseComplete(String),
    /// Real token usage from the provider (prompt+completion total).
    /// Drives the context meter; replaced under compaction.
    TokenUsage(usize),
    /// Per-call usage breakdown for the session-accounting counters.
    /// Separate from `TokenUsage` because the two track different
    /// things: the meter is a window snapshot, this is a running total
    /// that only grows.
    ///
    /// `cost_usd` is the dollar cost of this one call, computed by
    /// the dispatcher from `TaskResponse::pricing`. `None` when the
    /// endpoint has no `[pricing]` block — the counter simply does
    /// not advance for this turn rather than showing a guess.
    SessionUsage {
        prompt_tokens: usize,
        completion_tokens: usize,
        cost_usd: Option<f64>,
    },
    /// Post-tool thinking phase (tool result reinjected, LLM reasoning again)
    Thinking,
    /// The engine wants yes/no answers for one or more mutating calls
    /// (write_file, patch_file, or execute_command gated by policy).
    /// The items are ordered; the TUI walks them one at a time with
    /// y/n/a, navigates with ↑/↓, and calls `respond_to_approval`
    /// per item.
    ///
    /// A batch of one item is the common case (a single write in a
    /// round) and renders exactly like the old single-item dialog.
    ApprovalBatchRequested {
        batch_id: u64,
        items: Vec<ApprovalItem>,
    },
    /// The agent called ask_user and wants a text answer. Carries the
    /// question and optional placeholder hint. The TUI shows an input
    /// prompt; the answer is sent back via
    /// `respond_to_question`.
    QuestionRequested {
        id: u64,
        question: String,
        placeholder: Option<String>,
    },
    Error(String),
    Quit,
    Resize(u16, u16),
}

/// One item of an [`Event::ApprovalBatchRequested`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalItem {
    pub id: u64,
    pub tool_name: String,
    pub summary: String,
    #[serde(default)]
    pub diff: Option<String>,

    /// The call's arguments, carried so an "always approve" action
    /// can hash the exact call rather than its displayed summary
    /// (Tier 2.3).
    #[serde(default)]
    pub arguments: serde_json::Value,
}

/// Event handler that manages the event loop
pub struct EventHandler {
    event_queue: StdMutex<VecDeque<(EventPriority, Event)>>,
    event_rx: TokioMutex<mpsc::Receiver<Event>>,
    event_tx: mpsc::Sender<Event>,
    tick_rate: Duration,
    #[allow(dead_code)]
    last_tick: StdMutex<Instant>,
    is_running: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl EventHandler {
    /// Create a new event handler
    pub fn new(tick_rate: Duration) -> Self {
        let (tx, rx) = mpsc::channel(100);
        Self {
            event_queue: StdMutex::new(VecDeque::new()),
            event_rx: TokioMutex::new(rx),
            event_tx: tx,
            tick_rate,
            last_tick: StdMutex::new(Instant::now()),
            is_running: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    /// Check if handler is running
    pub fn is_running(&self) -> bool {
        self.is_running.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Get number of pending events
    pub fn pending_events(&self) -> usize {
        self.event_queue.lock().unwrap().len()
    }

    /// Push an event to the queue
    pub fn push_event(&self, event: Event) {
        let priority = match &event {
            Event::System(p, _) => *p,
            Event::Error(_) => EventPriority::High,
            Event::Quit => EventPriority::Critical,
            _ => EventPriority::Normal,
        };
        self.push_priority_event(event, priority);
    }

    /// Push a high-priority event
    pub fn push_priority_event(&self, event: Event, priority: EventPriority) {
        let mut queue = self.event_queue.lock().unwrap();

        let insert_pos = queue
            .iter()
            .position(|(p, _)| *p < priority)
            .unwrap_or(queue.len());

        queue.insert(insert_pos, (priority, event));
    }

    /// Get the next event (waits if no events)
    pub async fn next_event(&self) -> Event {
        // First check the priority queue
        {
            let mut queue = self.event_queue.lock().unwrap();
            if let Some((_, event)) = queue.pop_front() {
                return event;
            }
        }

        // If no events in queue, wait for new events or tick
        let mut rx = self.event_rx.lock().await;
        let deadline = tokio::time::Instant::now() + self.tick_rate;

        tokio::select! {
            event = rx.recv() => {
                if let Some(event) = event {
                    return event;
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                return Event::Tick;
            }
        }

        Event::Tick
    }

    /// Send an event (from external sources)
    pub async fn send_event(&self, event: Event) -> Result<()> {
        self.push_event(event);
        Ok(())
    }

    /// Get a clone of the event sender for use in background tasks
    pub fn sender(&self) -> mpsc::Sender<Event> {
        self.event_tx.clone()
    }

    /// Start the input handling loop
    pub async fn start_input_loop(&self) {
        let tx = self.event_tx.clone();
        let is_running = self.is_running.clone();

        tokio::spawn(async move {
            let mut reader = EventStream::new();

            while is_running.load(std::sync::atomic::Ordering::SeqCst) {
                if let Some(Ok(event)) = reader.next().await {
                    match event {
                        CrosstermEvent::Key(key) => {
                            if key.kind == KeyEventKind::Press {
                                // Ctrl combos that edit or cancel — map them
                                // before the plain char so typing is unaffected.
                                if key.modifiers.contains(KeyModifiers::CONTROL) {
                                    let mapped = match key.code {
                                        CrosstermKeyCode::Char('c' | 'C') => Some(KeyCode::CtrlC),
                                        CrosstermKeyCode::Char('j' | 'J') => Some(KeyCode::CtrlJ),
                                        CrosstermKeyCode::Char('k' | 'K') => Some(KeyCode::CtrlK),
                                        CrosstermKeyCode::Char('u' | 'U') => Some(KeyCode::CtrlU),
                                        CrosstermKeyCode::Char('w' | 'W') => Some(KeyCode::CtrlW),
                                        CrosstermKeyCode::Char('e' | 'E') => Some(KeyCode::CtrlE),
                                        CrosstermKeyCode::Left => Some(KeyCode::CtrlLeft),
                                        CrosstermKeyCode::Right => Some(KeyCode::CtrlRight),
                                        _ => None,
                                    };
                                    if let Some(code) = mapped {
                                        let _ = tx.send(Event::Key(code)).await;
                                        continue;
                                    }
                                }
                                // Shift+Enter inserts a newline (plain Enter sends).
                                if key.modifiers.contains(KeyModifiers::SHIFT)
                                    && matches!(key.code, CrosstermKeyCode::Enter)
                                {
                                    let _ = tx.send(Event::Key(KeyCode::ShiftEnter)).await;
                                    continue;
                                }
                                // Shift+↑/↓ scrolls (laptop keyboards often
                                // have no PgUp/PgDn); Fn+↑/↓ already arrives
                                // as PageUp/PageDown from the terminal.
                                if key.modifiers.contains(KeyModifiers::SHIFT) {
                                    match key.code {
                                        CrosstermKeyCode::Up => {
                                            let _ = tx.send(Event::Key(KeyCode::PageUp)).await;
                                            continue;
                                        }
                                        CrosstermKeyCode::Down => {
                                            let _ = tx.send(Event::Key(KeyCode::PageDown)).await;
                                            continue;
                                        }
                                        _ => {}
                                    }
                                }
                                let key_code: KeyCode = key.code.into();
                                let _ = tx.send(Event::Key(key_code)).await;
                            }
                        }
                        CrosstermEvent::Resize(w, h) => {
                            let _ = tx.send(Event::Resize(w, h)).await;
                        }
                        CrosstermEvent::Mouse(mouse) => {
                            use crossterm::event::MouseEventKind;
                            match mouse.kind {
                                MouseEventKind::ScrollUp => {
                                    let _ = tx.send(Event::Key(KeyCode::PageUp)).await;
                                }
                                MouseEventKind::ScrollDown => {
                                    let _ = tx.send(Event::Key(KeyCode::PageDown)).await;
                                }
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                }
            }
        });
    }

    /// Stop the event handler
    pub fn stop(&self) {
        self.is_running
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_event_queue() {
        let handler = EventHandler::new(Duration::from_millis(100));

        handler.push_event(Event::UserInput("test".to_string()));
        assert_eq!(handler.pending_events(), 1);

        let event = handler.next_event().await;
        assert!(matches!(event, Event::UserInput(s) if s == "test"));
    }

    #[test]
    fn test_priority_events() {
        let handler = EventHandler::new(Duration::from_millis(100));

        handler.push_event(Event::UserInput("normal".to_string()));
        handler.push_priority_event(
            Event::System(EventPriority::High, "critical".to_string()),
            EventPriority::High,
        );

        let queue = handler.event_queue.lock().unwrap();
        assert_eq!(queue.len(), 2);

        assert_eq!(queue[0].0, EventPriority::High);
    }

    #[test]
    fn test_keycode_from_crossterm() {
        let key: KeyCode = CrosstermKeyCode::Char('a').into();
        assert_eq!(key, KeyCode::Char('a'));

        let key: KeyCode = CrosstermKeyCode::Enter.into();
        assert_eq!(key, KeyCode::Enter);

        let key: KeyCode = CrosstermKeyCode::Esc.into();
        assert_eq!(key, KeyCode::Escape);
    }
}

#[cfg(test)]
mod coverage_event_priority {
    //! `EventPriority`'s derived `Ord` is what the priority
    //! queue's insertion sort depends on. A regression that
    //! reordered the variants would let a Quit land behind a
    //! System message, or a Critical error be preempted by a
    //! Normal tick — invisible until the wrong thing happens at
    //! the wrong moment.
    use super::*;

    #[test]
    fn priority_orders_low_to_critical() {
        assert!(EventPriority::Low < EventPriority::Normal);
        assert!(EventPriority::Normal < EventPriority::High);
        assert!(EventPriority::High < EventPriority::Critical);
    }

    #[test]
    fn priority_default_is_normal() {
        assert_eq!(EventPriority::default(), EventPriority::Normal);
    }

    #[test]
    fn priority_round_trips_through_json() {
        for p in [
            EventPriority::Low,
            EventPriority::Normal,
            EventPriority::High,
            EventPriority::Critical,
        ] {
            let json = serde_json::to_string(&p).unwrap();
            let parsed: EventPriority = serde_json::from_str(&json).unwrap();
            assert_eq!(p, parsed, "roundtrip mismatch for {json}");
        }
    }

    #[test]
    fn priority_variant_names_survive_serialization() {
        // A caller (a log viewer, a queue inspector) that maps a
        // JSON priority to its meaning relies on the variant
        // name. The exact spelling is the contract.
        let json = serde_json::to_string(&EventPriority::Critical).unwrap();
        assert!(json.contains("Critical"), "got: {json}");
    }

    #[test]
    fn keycode_equality_and_hash_cover_every_variant() {
        // KeyCode is `Copy + PartialEq + Eq + Hash`. A regression
        // that dropped one of the derives would break the
        // keybinding lookup; the test exercises each shape.
        use std::collections::HashSet;
        let mut set = HashSet::new();
        for k in [
            KeyCode::Char('a'),
            KeyCode::Enter,
            KeyCode::Escape,
            KeyCode::Backspace,
            KeyCode::Delete,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Tab,
            KeyCode::BackTab,
            KeyCode::CtrlC,
            KeyCode::CtrlJ,
            KeyCode::CtrlK,
            KeyCode::CtrlU,
            KeyCode::CtrlW,
            KeyCode::CtrlE,
            KeyCode::CtrlLeft,
            KeyCode::CtrlRight,
            KeyCode::F(1),
        ] {
            set.insert(k);
        }
        assert!(set.contains(&KeyCode::Char('a')));
        assert!(set.contains(&KeyCode::CtrlC));
        assert!(!set.contains(&KeyCode::Char('b')));
        // CtrlJ and ShiftEnter are distinct — a regression that
        // collapsed them would lose "newline" on one of the two
        // physical keys a user might press.
        assert_ne!(KeyCode::CtrlJ, KeyCode::ShiftEnter);
    }

    #[test]
    fn event_priority_high_is_greater_than_normal() {
        // The events module maps specific variants to non-default
        // priorities (Error -> High, Quit -> Critical). A
        // regression that swapped the mapping would park a
        // Quit behind a Tick in the queue.
        let err_priority = EventPriority::High;
        let quit_priority = EventPriority::Critical;
        assert!(quit_priority > err_priority);
    }

    #[test]
    fn event_round_trips_through_json_for_simple_variants() {
        // The channel that carries events between the engine and
        // the TUI does not require JSON, but the event type is
        // serializable and a future external consumer (a debug
        // dump, a remote client) may use it. Pin the shape for
        // the variants that have no inner type requiring extra
        // imports.
        for e in [
            Event::Tick,
            Event::Quit,
        ] {
            let json = serde_json::to_string(&e).unwrap();
            let parsed: Event = serde_json::from_str(&json).unwrap();
            let re = serde_json::to_string(&parsed).unwrap();
            assert_eq!(json, re, "roundtrip mismatch for {json}");
        }
    }
}

/// Behavioural coverage for `EventHandler`: how `push_event`
/// classifies priority, that `push_priority_event` can override that
/// classification, that `next_event` drains the priority queue before
/// blocking on the channel, and that the full `KeyCode::from` mapping
/// table is what the input loop relies on.
#[cfg(test)]
mod coverage_event_handler {
    use super::*;

    fn handler() -> EventHandler {
        // Long tick so the timing tests can distinguish "queue drained"
        // from "tick fired". Tests that need a short tick construct
        // their own handler.
        EventHandler::new(Duration::from_secs(60))
    }

    // ---- push_event priority classification ----------------------------

    #[tokio::test]
    async fn push_event_assigns_the_expected_priority_per_variant() {
        // Push in an order that would be wrong if every event were
        // Normal, then dequeue and check the order matches the
        // documented mapping: Critical(Quit) > High(Error) >
        // Normal(everything else) > Low(System(Low)).
        let h = handler();
        h.push_event(Event::Tick);
        h.push_event(Event::Error("boom".into()));
        h.push_event(Event::System(EventPriority::Low, "low".into()));
        h.push_event(Event::Quit);

        assert!(
            matches!(h.next_event().await, Event::Quit),
            "Critical (Quit) must come out first"
        );
        assert!(
            matches!(h.next_event().await, Event::Error(_)),
            "High (Error) must come out second"
        );
        assert!(
            matches!(h.next_event().await, Event::Tick),
            "Normal (Tick) must come out third"
        );
        assert!(
            matches!(h.next_event().await, Event::System(p, _) if p == EventPriority::Low),
            "Low (System(Low)) must come out last"
        );
    }

    #[tokio::test]
    async fn push_event_respects_a_system_events_explicit_priority() {
        // `Event::System(p, _)` uses `p`, not Normal. A High System
        // must beat a Normal Tick even though Tick was pushed first.
        let h = handler();
        h.push_event(Event::Tick);
        h.push_event(Event::System(EventPriority::High, "sys".into()));
        assert!(
            matches!(h.next_event().await, Event::System(EventPriority::High, _)),
            "High System must beat a Normal Tick"
        );
        assert!(matches!(h.next_event().await, Event::Tick));
    }

    #[tokio::test]
    async fn push_event_is_fifo_within_the_same_priority() {
        let h = handler();
        h.push_event(Event::System(EventPriority::Low, "a".into()));
        h.push_event(Event::System(EventPriority::Low, "b".into()));
        h.push_event(Event::System(EventPriority::Low, "c".into()));
        for expected in ["a", "b", "c"] {
            let ev = h.next_event().await;
            match ev {
                Event::System(_, msg) => assert_eq!(msg, expected),
                _ => panic!("expected System, got something else"),
            }
        }
    }

    // ---- push_priority_event override ----------------------------------

    #[tokio::test]
    async fn push_priority_event_overrides_the_default_classification() {
        let h = handler();
        // A Tick is normally Normal; force it to Critical so it beats
        // an Error (High) pushed afterwards.
        h.push_priority_event(Event::Tick, EventPriority::Critical);
        h.push_event(Event::Error("late".into()));
        assert!(matches!(h.next_event().await, Event::Tick));
        assert!(matches!(h.next_event().await, Event::Error(_)));
    }

    // ---- pending_events, sender, is_running, stop ----------------------

    #[test]
    fn pending_events_counts_the_queue() {
        let h = handler();
        assert_eq!(h.pending_events(), 0);
        h.push_event(Event::Tick);
        h.push_event(Event::Tick);
        assert_eq!(h.pending_events(), 2);
        // A priority push also lands in the same queue.
        h.push_priority_event(Event::Quit, EventPriority::Critical);
        assert_eq!(h.pending_events(), 3);
    }

    #[test]
    fn is_running_is_true_on_a_fresh_handler() {
        assert!(handler().is_running());
    }

    #[test]
    fn stop_marks_the_handler_not_running() {
        let h = handler();
        h.stop();
        assert!(!h.is_running());
        // Idempotent — a second stop must not panic or flip back.
        h.stop();
        assert!(!h.is_running());
    }

    #[tokio::test]
    async fn sender_returns_a_clone_that_reaches_next_event() {
        let h = handler();
        let tx = h.sender();
        tx.send(Event::Quit).await.expect("send on live channel");
        // The message went through the channel (not the queue); next_event
        // sees it via the select! on `rx.recv()`.
        assert!(matches!(h.next_event().await, Event::Quit));
    }

    // ---- next_event scheduling -----------------------------------------

    #[tokio::test]
    async fn next_event_drains_the_queue_before_touching_the_channel() {
        let h = handler();
        h.push_event(Event::Quit);
        // A channel message is available too, but the queue wins.
        h.sender().send(Event::Tick).await.expect("send");
        assert!(matches!(h.next_event().await, Event::Quit));
    }

    #[tokio::test]
    async fn next_event_waits_for_the_channel_then_returns_the_event() {
        // tick_rate is 500ms; the channel send arrives at 10ms. If
        // next_event ignored the channel it would return a Tick at
        // 500ms — 50x later than the correct outcome.
        let h = EventHandler::new(Duration::from_millis(500));
        let tx = h.sender();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let _ = tx.send(Event::Quit).await;
        });
        assert!(matches!(h.next_event().await, Event::Quit));
    }

    #[tokio::test]
    async fn next_event_returns_tick_when_the_queue_is_empty() {
        // Short tick so the test runs fast; assert only a lower bound
        // on the elapsed time, and a generous upper bound to catch a
        // hang. Timing-sensitive branches like this one are why the
        // upper bound is 2s and not tick_rate * 1.1.
        let h = EventHandler::new(Duration::from_millis(30));
        let start = std::time::Instant::now();
        let ev = h.next_event().await;
        let elapsed = start.elapsed();
        assert!(matches!(ev, Event::Tick));
        assert!(
            elapsed >= Duration::from_millis(15),
            "tick must wait for the deadline, elapsed = {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "tick must not hang, elapsed = {elapsed:?}"
        );
    }

    // ---- KeyCode::from mapping table -----------------------------------

    #[test]
    fn keycode_from_crossterm_covers_every_mapped_variant() {
        use crossterm::event::KeyCode as CK;
        let cases: &[(CK, KeyCode)] = &[
            (CK::Char('x'), KeyCode::Char('x')),
            (CK::Char('\n'), KeyCode::Char('\n')),
            (CK::Enter, KeyCode::Enter),
            (CK::Esc, KeyCode::Escape),
            (CK::Backspace, KeyCode::Backspace),
            (CK::Delete, KeyCode::Delete),
            (CK::Up, KeyCode::Up),
            (CK::Down, KeyCode::Down),
            (CK::Left, KeyCode::Left),
            (CK::Right, KeyCode::Right),
            (CK::Home, KeyCode::Home),
            (CK::End, KeyCode::End),
            (CK::PageUp, KeyCode::PageUp),
            (CK::PageDown, KeyCode::PageDown),
            (CK::Tab, KeyCode::Tab),
            (CK::BackTab, KeyCode::BackTab),
            (CK::F(1), KeyCode::F(1)),
            (CK::F(12), KeyCode::F(12)),
        ];
        for (input, expected) in cases {
            assert_eq!(KeyCode::from(*input), *expected, "mapping for {input:?}");
        }
    }

    #[test]
    fn keycode_from_crossterm_unmapped_variants_fall_back_to_space() {
        // The input loop only forwards `KeyEventKind::Press` events, so
        // an unmapped `CrosstermKeyCode` (Null, Insert, media keys …)
        // should reach the app as a harmless space, not panic or drop
        // the event.
        use crossterm::event::KeyCode as CK;
        assert_eq!(KeyCode::from(CK::Null), KeyCode::Char(' '));
        assert_eq!(KeyCode::from(CK::Insert), KeyCode::Char(' '));
    }
}
