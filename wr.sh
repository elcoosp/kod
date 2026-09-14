#!/usr/bin/env bash
set -uo pipefail

run_with_timeout() {
    local secs="$1"; shift
    if command -v timeout >/dev/null 2>&1; then
        timeout "$secs" "$@"; return $?
    fi
    if command -v gtimeout >/dev/null 2>&1; then
        gtimeout "$secs" "$@"; return $?
    fi
    "$@" &
    local pid=$!
    ( sleep "$secs"
      if kill -0 "$pid" 2>/dev/null; then
          kill -TERM "$pid" 2>/dev/null
          sleep 2
          kill -KILL "$pid" 2>/dev/null
      fi ) &
    local watchdog=$!
    wait "$pid"; local rc=$?
    kill "$watchdog" 2>/dev/null; wait "$watchdog" 2>/dev/null
    [ "$rc" -ge 128 ] && return 124
    return "$rc"
}

COMPILE_OK=true
INCOMPLETE=false
TARGET=crates/kod-swarm/src/swarm.rs

if [ ! -f Cargo.toml ] || [ ! -f "$TARGET" ]; then
    echo "ERROR: run from the kod workspace root ($TARGET missing)"
    exit 1
fi

echo "Patching $TARGET: graceful remove_agent + shutdown"

python3 - "$TARGET" << 'PYEOF'
import os
import sys

target = sys.argv[1]
with open(target, "r") as f:
    content = f.read()

def patch(old, new, label, expect=1):
    global content
    n = content.count(old)
    if n == 0:
        print(f"ERROR: old snippet not found: {label}")
        sys.exit(2)
    if expect and n != expect:
        print(f"ERROR: expected {expect} occurrence(s) of {label}, found {n}")
        sys.exit(2)
    content = content.replace(old, new, expect if expect else n)
    print(f"Patched: {label}")

# --- 1. remove_agent: stop first, then unregister ----------------------
patch(
    '''    /// Remove an agent from the swarm
    pub async fn remove_agent(&self, agent_id: &AgentId) -> Result<()> {
        let mut agents = self.agents.write().await;
        if agents.remove(agent_id).is_none() {
            return Err(KodError::InvalidState(format!(
                "Agent {} not in swarm",
                agent_id
            )));
        }
        drop(agents);
        self.communication.unregister_agent(agent_id).await;
        Ok(())
    }''',
    '''    /// Remove an agent from the swarm.
    ///
    /// Stops the agent first if it is not already stopped, so anyone
    /// watching its state channel observes the proper
    /// Running → Stopping → Stopped transition instead of a channel
    /// that closes mid-flight. Then unregisters it from the
    /// communication hub so peers stop addressing it.
    pub async fn remove_agent(&self, agent_id: &AgentId) -> Result<()> {
        // Take the agent out under the write lock, but do the stop
        // outside so a slow stop does not hold the swarm's map.
        let agent = {
            let mut agents = self.agents.write().await;
            match agents.remove(agent_id) {
                Some(a) => a,
                None => {
                    return Err(KodError::InvalidState(format!(
                        "Agent {} not in swarm",
                        agent_id
                    )));
                }
            }
        };
        // Best-effort graceful stop: an agent that is already stopped
        // returns Ok; one that is Failed returns an error we do not
        // want to surface as a removal failure. The important
        // invariant is that the agent leaves the swarm.
        if let Err(e) = agent.stop().await {
            tracing::warn!(
                agent = %agent_id,
                error = %e,
                "stop() during remove_agent did not complete cleanly; \\
                 removing anyway"
            );
        }
        self.communication.unregister_agent(agent_id).await;
        Ok(())
    }

    /// Stop every agent and clear the swarm.
    ///
    /// Called during graceful shutdown so a session does not leak
    /// agents whose state machines were left mid-flight. Best-effort:
    /// an agent whose stop fails is still removed from the swarm and
    /// the failure is logged; callers get `Ok(())` as long as the
    /// swarm ends empty, because a shutdown that refuses to finish
    /// because one agent misbehaved is worse than a shutdown that
    /// reports the problem and continues.
    pub async fn shutdown(&self) -> Result<()> {
        // Swap the map out so we do not hold the write lock across
        // each stop's await.
        let agents: Vec<(AgentId, std::sync::Arc<Agent>)> = {
            let mut map = self.agents.write().await;
            map.drain().collect()
        };
        let n = agents.len();
        for (id, agent) in &agents {
            if let Err(e) = agent.stop().await {
                tracing::warn!(
                    agent = %id,
                    error = %e,
                    "agent stop failed during swarm shutdown"
                );
            }
            self.communication.unregister_agent(id).await;
        }
        tracing::info!(count = n, "agent swarm shut down");
        Ok(())
    }''',
    "remove_agent graceful stop + shutdown",
)

# --- 2. Tests --------------------------------------------------------
# Only append if the swarm.rs test module does not already contain a
# shutdown test.
if "shutdown_stops_all_agents" not in content:
    patch(
        '''    #[tokio::test]
    async fn lifecycle_methods_error_for_unknown_id() {
        let swarm = AgentSwarm::new(swarm_root());
        let unknown = AgentId::new();
        assert!(swarm.start_agent(&unknown).await.is_err());
        assert!(swarm.stop_agent(&unknown).await.is_err());
    }
}''',
        '''    #[tokio::test]
    async fn lifecycle_methods_error_for_unknown_id() {
        let swarm = AgentSwarm::new(swarm_root());
        let unknown = AgentId::new();
        assert!(swarm.start_agent(&unknown).await.is_err());
        assert!(swarm.stop_agent(&unknown).await.is_err());
    }

    /// remove_agent on a running agent must leave the agent in the
    /// Stopped state (so anyone holding a state watcher sees a clean
    /// transition) and must remove it from the swarm.
    #[tokio::test]
    async fn remove_running_agent_stops_it_first() {
        let swarm = AgentSwarm::new(swarm_root());
        let agent = Agent::new("leaving").build();
        let id = agent.id().clone();
        swarm.add_agent(agent).await.unwrap();

        // Take a handle and a state watcher before removal so we can
        // observe the pre-removal state and the state transition.
        let handle = swarm.get_agent(&id).await.unwrap();
        let mut watcher = handle.watch_state();

        swarm.start_agent(&id).await.unwrap();
        assert_eq!(handle.state(), AgentState::Running);

        swarm.remove_agent(&id).await.unwrap();
        assert!(!swarm.contains_agent(&id).await);
        assert_eq!(
            handle.state(),
            AgentState::Stopped,
            "removed agent should be Stopped, not left Running"
        );

        // The watcher sees a stop transition rather than a closed
        // channel. `wait_for` returns the new value the first time the
        // predicate matches — a Stopped value was set by stop().
        watcher
            .wait_for(|s| *s == AgentState::Stopped)
            .await
            .expect("state watcher should observe Stopped before the channel closes");
    }

    /// remove_agent on an idle (never-started) agent still removes it
    /// cleanly — stop() on Idle is an error per Agent's own state
    /// machine, and removal must not propagate that as a failure.
    #[tokio::test]
    async fn remove_idle_agent_is_best_effort() {
        let swarm = AgentSwarm::new(swarm_root());
        let agent = Agent::new("idle").build();
        let id = agent.id().clone();
        swarm.add_agent(agent).await.unwrap();

        swarm.remove_agent(&id).await.unwrap();
        assert!(!swarm.contains_agent(&id).await);
    }

    /// shutdown stops every agent in the swarm and empties it.
    #[tokio::test]
    async fn shutdown_stops_all_agents() {
        let swarm = AgentSwarm::new(swarm_root());
        let a = Agent::new("a").build();
        let b = Agent::new("b").build();
        let a_id = a.id().clone();
        let b_id = b.id().clone();
        swarm.add_agent(a).await.unwrap();
        swarm.add_agent(b).await.unwrap();

        let a_handle = swarm.get_agent(&a_id).await.unwrap();
        let b_handle = swarm.get_agent(&b_id).await.unwrap();
        swarm.start_agent(&a_id).await.unwrap();
        swarm.start_agent(&b_id).await.unwrap();

        swarm.shutdown().await.unwrap();

        assert!(swarm.list_agents().await.is_empty(), "swarm should be empty");
        assert_eq!(a_handle.state(), AgentState::Stopped);
        assert_eq!(b_handle.state(), AgentState::Stopped);
    }

    /// shutdown on an empty swarm is a no-op, not an error.
    #[tokio::test]
    async fn shutdown_on_empty_swarm_is_ok() {
        let swarm = AgentSwarm::new(swarm_root());
        swarm.shutdown().await.unwrap();
        assert!(swarm.list_agents().await.is_empty());
    }
}''',
        "swarm shutdown tests",
    )
else:
    print("Skipped: shutdown tests already present")

tmp = target + ".tmp"
with open(tmp, "w") as f:
    f.write(content)
os.replace(tmp, target)
print("Patched", target)
PYEOF

if [ $? -ne 0 ]; then
    echo "ERROR: patching failed"
    exit 1
fi

echo "Checking compilation"
if ! cargo check --workspace 2>&1; then
    echo "Compilation failed – will skip commit"
    COMPILE_OK=false
fi

if [ "$INCOMPLETE" = true ] || [ "$COMPILE_OK" = false ]; then
    echo "Skipping tests and commit due to incomplete files or compilation errors"
    exit 1
fi

echo "Running kod-swarm tests (120s wall clock)"
if ! run_with_timeout 120 cargo test -p kod-swarm 2>&1; then
    echo "kod-swarm tests failed or hung. Paste the full output for a surgical fix."
    exit 1
fi

echo "Running workspace tests (300s wall clock)"
if ! run_with_timeout 300 cargo test --workspace 2>&1; then
    echo "Workspace tests failed or hung. Paste the full output for a surgical fix."
    exit 1
fi

echo "Running clippy with -D warnings"
if ! cargo clippy --workspace --all-targets -- -D warnings 2>&1; then
    echo "Clippy failed. Paste the full output for a surgical fix."
    exit 1
fi

echo "All checks passed. Committing."
git add -A
git commit -m "feat(swarm): graceful remove_agent + swarm shutdown

Two cleanup gaps left after agents became addressable:

1. remove_agent just dropped the agent from the map. A running agent
   whose state machine was mid-transition disappeared without a
   Stopped transition — anyone holding a watch_state receiver saw
   the channel close rather than the expected Running → Stopping →
   Stopped sequence. remove_agent now takes the agent out of the
   map, calls stop() on it (best-effort, since an already-Idle agent
   returns an error that should not fail removal), and then
   unregisters it from the communication hub. The stop runs outside
   the write lock so a slow stop does not block other lookups.

2. There was no way to shut down every agent at once. Add
   AgentSwarm::shutdown, which drains the map, stops each agent
   (best-effort, logging failures), unregisters each from the
   communication hub, and returns Ok as long as the swarm ends
   empty. A shutdown that refused to finish because one agent
   misbehaved would be worse than a shutdown that reports the
   problem and continues.

Adds four tests: remove_running_agent_stops_it_first asserts the
watcher sees Stopped before the channel closes,
remove_idle_agent_is_best_effort covers the Idle case,
shutdown_stops_all_agents verifies a two-agent swarm ends empty
with both agents Stopped, and shutdown_on_empty_swarm_is_ok pins
the no-op case."
