//! Concurrency stress tests for the swarm messaging layer.
//!
//! These tests spawn many tokio tasks against shared state and assert
//! message-count invariants. They are the detector for races that miri
//! cannot see (miri runs a single thread) and that single-threaded
//! unit tests do not exercise.
//!
//! Run with `--release` when iterating; debug mode works but is slower.
//! `cargo nextest run -p kod-swarm --test it --release -- stress::`

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use kod_swarm::communication::{AgentCommunicationHub, MessageContent};
use kod_swarm::irc_bus::{Delivery, IrcBus};
use kod_swarm::yield_queue::YieldQueue;
use kod_types::{AgentId, Priority};

// ---------------------------------------------------------------------------
// IrcBus
// ---------------------------------------------------------------------------

/// One writer per recipient, each writing below MAILBOX_CAP. Every
/// message that was enqueued must come back out exactly once. This
/// exercises the bus's lock and drain path without triggering the
/// documented cap-drop.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn irc_bus_concurrent_sends_do_not_lose_messages() {
    let bus = Arc::new(IrcBus::new());
    let n_pairs = 8usize;
    // Stay strictly below MAILBOX_CAP (100) so no cap-drop can fire.
    let per_pair = 80usize;
    let total = n_pairs * per_pair;

    for i in 0..n_pairs {
        bus.register(format!("inbox{i}"), true).await;
    }

    let drained = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    let mut drainers = Vec::new();
    for i in 0..n_pairs {
        let bus = bus.clone();
        let drained = drained.clone();
        let stop = stop.clone();
        drainers.push(tokio::spawn(async move {
            let id = format!("inbox{i}");
            loop {
                let msgs = bus.drain(&id).await;
                drained.fetch_add(msgs.len(), Ordering::Relaxed);
                if stop.load(Ordering::Relaxed) {
                    // Final drain after the writers have stopped so
                    // the very last message is not missed.
                    let msgs = bus.drain(&id).await;
                    drained.fetch_add(msgs.len(), Ordering::Relaxed);
                    break;
                }
                tokio::task::yield_now().await;
            }
        }));
    }

    let mut writers = Vec::new();
    for w in 0..n_pairs {
        let bus = bus.clone();
        writers.push(tokio::spawn(async move {
            let from = format!("w{w}");
            let to = format!("inbox{w}");
            for i in 0..per_pair {
                let _ = bus
                    .send(
                        from.clone(),
                        to.clone(),
                        format!("{w}-{i}"),
                        Delivery::Aside,
                    )
                    .await;
                if i % 8 == 0 {
                    tokio::task::yield_now().await;
                }
            }
        }));
    }

    for h in writers {
        h.await.unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    for h in drainers {
        h.await.unwrap();
    }

    // Safety sweep in case a drainer finished before a late send.
    for i in 0..n_pairs {
        let msgs = bus.drain(&format!("inbox{i}")).await;
        drained.fetch_add(msgs.len(), Ordering::Relaxed);
    }

    let final_count = drained.load(Ordering::Relaxed);
    assert_eq!(
        final_count, total,
        "messages lost: sent={total} drained={final_count}"
    );
}

/// Flood a single mailbox past MAILBOX_CAP. The bus drops the oldest
/// entries by design; the invariant is not "no loss" but
/// `sent == drained + DroppedOldest`.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn irc_bus_cap_drops_are_accounted_for() {
    use kod_swarm::irc_bus::Receipt;
    let bus = Arc::new(IrcBus::new());
    bus.register("inbox", true).await;

    let senders = 8usize;
    let per_sender = 300usize;
    let total = senders * per_sender;

    let drops = Arc::new(AtomicUsize::new(0));

    let mut writers = Vec::new();
    for w in 0..senders {
        let bus = bus.clone();
        let drops = drops.clone();
        writers.push(tokio::spawn(async move {
            let from = format!("w{w}");
            for i in 0..per_sender {
                match bus
                    .send(from.clone(), "inbox", format!("{w}-{i}"), Delivery::Aside)
                    .await
                {
                    Receipt::DroppedOldest => {
                        drops.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {}
                }
                if i % 8 == 0 {
                    tokio::task::yield_now().await;
                }
            }
        }));
    }

    for h in writers {
        h.await.unwrap();
    }

    // No concurrent drainers on this test, so every enqueue past the
    // cap reports DroppedOldest. The mailbox holds exactly cap.
    let final_mailbox_len = bus.mailbox_len("inbox").await;
    let dropped = drops.load(Ordering::Relaxed);
    let survivors = final_mailbox_len;

    assert_eq!(
        survivors + dropped,
        total,
        "cap accounting mismatch: sent={total} survivors={survivors} dropped={dropped}"
    );
    assert!(
        survivors <= kod_swarm::irc_bus::MAILBOX_CAP,
        "mailbox exceeded cap: {survivors}"
    );
}

/// Concurrent `send_await` callers against one server that drains and
/// replies. Every awaited send must resolve to a reply, never to a
/// timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn irc_bus_send_await_under_concurrent_replies() {
    let bus = Arc::new(IrcBus::new());
    bus.register("server", true).await;

    let n_waiters = 4usize;
    let per_waiter = 50usize;

    let stop = Arc::new(AtomicBool::new(false));
    let server = {
        let bus = bus.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            loop {
                let msgs = bus.drain("server").await;
                for m in &msgs {
                    if let Some(corr) = m.reply_to {
                        bus.reply(corr, "ack").await;
                    }
                }
                if stop.load(Ordering::Relaxed) {
                    // Final sweep so a message that landed between
                    // the last drain and the stop flag still gets a
                    // reply.
                    let msgs = bus.drain("server").await;
                    for m in &msgs {
                        if let Some(corr) = m.reply_to {
                            bus.reply(corr, "ack").await;
                        }
                    }
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
    };

    let mut waiters = Vec::new();
    for w in 0..n_waiters {
        let bus = bus.clone();
        waiters.push(tokio::spawn(async move {
            let from = format!("client{w}");
            let mut ok = 0usize;
            for i in 0..per_waiter {
                let r = bus
                    .send_await(
                        from.clone(),
                        "server",
                        format!("req-{w}-{i}"),
                        Duration::from_secs(10),
                    )
                    .await;
                if r.is_ok() {
                    ok += 1;
                }
            }
            ok
        }));
    }

    let mut total_ok = 0usize;
    for h in waiters {
        total_ok += h.await.unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    server.await.unwrap();

    assert_eq!(
        total_ok,
        n_waiters * per_waiter,
        "some send_await calls did not get a reply"
    );
}

/// Register / set_receiver / mark_dead / send / drain racing across
/// many agents. No invariant beyond "did not panic / did not deadlock",
/// plus: an agent that was tombstoned stays tombstoned.
//
// The two `.iter().cloned()` loops look redundant, but each loop body
// pushes a `tokio::spawn(async move { ... })` that must capture the
// loop variable by value (`tokio::spawn` requires `'static`). Clippy's
// `unnecessary_to_owned` cannot see through the async-move capture.
#[allow(clippy::unnecessary_to_owned)]
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn irc_bus_register_and_mark_dead_race() {
    let bus = Arc::new(IrcBus::new());
    let n_agents = 16usize;
    let ids: Vec<String> = (0..n_agents).map(|i| format!("a{i}")).collect();
    for id in &ids {
        bus.register(id.clone(), true).await;
    }

    let mut tasks = Vec::new();
    for id in ids.iter().cloned() {
        let bus = bus.clone();
        tasks.push(tokio::spawn(async move {
            for _ in 0..100 {
                bus.set_receiver(&id, true).await;
                bus.set_receiver(&id, false).await;
                let _ = bus.send("probe", id.clone(), "hi", Delivery::Aside).await;
                tokio::task::yield_now().await;
            }
        }));
    }

    let half = n_agents / 2;
    {
        let bus = bus.clone();
        let targets: Vec<String> = ids.iter().take(half).cloned().collect();
        tasks.push(tokio::spawn(async move {
            for id in targets {
                bus.mark_dead(&id).await;
                tokio::task::yield_now().await;
            }
        }));
    }

    {
        let bus = bus.clone();
        let ids = ids.clone();
        tasks.push(tokio::spawn(async move {
            for _ in 0..200 {
                for id in &ids {
                    let _ = bus.drain(id).await;
                }
                tokio::task::yield_now().await;
            }
        }));
    }

    for h in tasks {
        h.await.unwrap();
    }

    for id in ids.iter().take(half) {
        assert!(bus.is_dead(id).await, "{id} should be dead");
    }
}

// ---------------------------------------------------------------------------
// AgentCommunicationHub
// ---------------------------------------------------------------------------

/// Every agent sends to every other agent; each recipient's channel
/// must yield exactly the expected count. Exercises the per-agent
/// RwLock map and the bounded history map under contention.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn communication_hub_concurrent_send_does_not_lose_messages() {
    let hub = Arc::new(AgentCommunicationHub::new());
    let n_agents = 6usize;
    let mut ids = Vec::with_capacity(n_agents);
    let mut receivers = Vec::with_capacity(n_agents);
    for _ in 0..n_agents {
        let id = AgentId::new();
        hub.register_agent(id.clone()).await.unwrap();
        let r = hub.get_agent_receiver(&id).await.unwrap();
        ids.push(id);
        receivers.push(r);
    }

    let per_pair = 50usize;
    let expected_per_recipient = (n_agents - 1) * per_pair;

    let mut senders = Vec::new();
    for from_idx in 0..n_agents {
        let hub = hub.clone();
        let from = ids[from_idx].clone();
        let targets = ids.clone();
        senders.push(tokio::spawn(async move {
            for _ in 0..per_pair {
                for to in &targets {
                    if *to == from {
                        continue;
                    }
                    let _ = hub
                        .send_direct(
                            &from,
                            to,
                            MessageContent::TaskAssignment {
                                description: "work".to_string(),
                                priority: Priority::Medium,
                            },
                        )
                        .await;
                }
                tokio::task::yield_now().await;
            }
        }));
    }

    let mut readers = Vec::new();
    for r in receivers {
        readers.push(tokio::spawn(async move {
            let mut count = 0usize;
            while count < expected_per_recipient {
                match r.recv().await {
                    Some(_) => count += 1,
                    None => break,
                }
            }
            count
        }));
    }

    for h in senders {
        h.await.unwrap();
    }
    let mut counts = Vec::with_capacity(n_agents);
    for h in readers {
        counts.push(h.await.unwrap());
    }

    for (i, c) in counts.iter().enumerate() {
        assert_eq!(
            *c, expected_per_recipient,
            "agent {i} received {c} of {expected_per_recipient}"
        );
    }
}

// ---------------------------------------------------------------------------
// YieldQueue
// ---------------------------------------------------------------------------

/// Many writers register unique entries while four drainers race to
/// pull them. Every entry must come out exactly once; none may be
/// lost or duplicated.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn yield_queue_concurrent_push_and_drain() {
    let q = Arc::new(YieldQueue::new());
    let n_writers = 8usize;
    let per_writer = 300usize;
    let total = n_writers * per_writer;

    let collected: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));

    let mut drainers = Vec::new();
    for _ in 0..4 {
        let q = q.clone();
        let collected = collected.clone();
        let stop = stop.clone();
        drainers.push(tokio::spawn(async move {
            loop {
                let batch = q.drain_streaming();
                if !batch.messages.is_empty() {
                    collected.lock().unwrap().extend(batch.messages);
                }
                if stop.load(Ordering::Relaxed) && q.is_empty() {
                    // One last drain so a message pushed between the
                    // drain call and the is_empty() check is not
                    // missed.
                    let batch = q.drain_streaming();
                    if !batch.messages.is_empty() {
                        collected.lock().unwrap().extend(batch.messages);
                    }
                    break;
                }
                tokio::task::yield_now().await;
            }
        }));
    }

    let mut writers = Vec::new();
    for w in 0..n_writers {
        let q = q.clone();
        writers.push(tokio::spawn(async move {
            for i in 0..per_writer {
                let id = format!("{w}-{i}");
                q.register("stress", false, move || Some(id));
                if i % 4 == 0 {
                    tokio::task::yield_now().await;
                }
            }
        }));
    }

    for h in writers {
        h.await.unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    for h in drainers {
        h.await.unwrap();
    }

    // Final drain to catch any last entries.
    let batch = q.drain_streaming();
    collected.lock().unwrap().extend(batch.messages);

    let mut got = collected.lock().unwrap().clone();
    let raw_len = got.len();
    got.sort();
    got.dedup();
    assert_eq!(
        got.len(),
        total,
        "yield queue wrong entry count: expected {total}, got {} (unique {})",
        raw_len,
        got.len()
    );
    assert_eq!(
        raw_len, total,
        "yield queue produced {raw_len} messages for {total} registrations (duplicates or losses)"
    );
}
