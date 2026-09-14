#!/usr/bin/env bash
set -uo pipefail

AGENT=crates/kod-swarm/src/agent.rs

echo "=== Current start/stop bodies ==="
awk '/pub async fn start\(/,/^    \}$/' "$AGENT" | head -35
echo "---"
awk '/pub async fn stop\(/,/^    \}$/' "$AGENT" | head -35

echo
echo "=== All sleeps in agent.rs ==="
grep -n "sleep\|Duration" "$AGENT"

echo
echo "Patching $AGENT"

python3 - "$AGENT" << 'PYEOF'
import os
import sys

target = sys.argv[1]
with open(target, "r") as f:
    src = f.read()

def patch(old, new, label, expect=1):
    global src
    n = src.count(old)
    if n == 0:
        print(f"  SKIP (anchor absent): {label}")
        return False
    if expect and n != expect:
        print(f"  ERROR: expected {expect} occurrence(s) of {label}, found {n}")
        sys.exit(2)
    src = src.replace(old, new, expect if expect else n)
    print(f"  patched: {label}")
    return True

# ----------------------------------------------------------------------
# 1. start(): remove the fake init sleep.
# ----------------------------------------------------------------------
patch(
    '''        self.state
            .send(AgentState::Starting)
            .map_err(|e| KodError::InvalidState(format!("Failed to update state: {:?}", e)))?;

        // Simulate initialization
        tokio::time::sleep(Duration::from_millis(10)).await;

        self.state
            .send(AgentState::Running)
            .map_err(|e| KodError::InvalidState(format!("Failed to update state: {:?}", e)))?;

        self.record_heartbeat();

        Ok(())''',
    '''        // No work happens between Starting and Running today: the
        // agent has no real initialization step. The previous
        // `tokio::time::sleep(Duration::from_millis(10))` labelled
        // "Simulate initialization" was pure waste — every call paid
        // 10 ms of wall clock and every test that drove an agent
        // through its lifecycle paid it too.
        //
        // If a real initialization step is added later (registering
        // with a coordination service, opening a per-agent socket),
        // put its actual await here. The `Starting` state remains in
        // the enum so a caller that subscribes before calling start
        // can observe the transition; today the transition is
        // instantaneous, which is the honest description of the work.
        self.state
            .send(AgentState::Starting)
            .map_err(|e| KodError::InvalidState(format!("Failed to update state: {:?}", e)))?;

        self.state
            .send(AgentState::Running)
            .map_err(|e| KodError::InvalidState(format!("Failed to update state: {:?}", e)))?;

        self.record_heartbeat();

        Ok(())''',
    "start(): remove fake init sleep",
)

# ----------------------------------------------------------------------
# 2. stop(): remove the fake cleanup sleep.
# ----------------------------------------------------------------------
patch(
    '''                self.state.send(AgentState::Stopping).map_err(|e| {
                    KodError::InvalidState(format!("Failed to update state: {:?}", e))
                })?;

                // Cleanup
                tokio::time::sleep(Duration::from_millis(10)).await;

                self.state.send(AgentState::Stopped).map_err(|e| {
                    KodError::InvalidState(format!("Failed to update state: {:?}", e))
                })?;''',
    '''                // Same reasoning as start(): no work happens between
                // Stopping and Stopped today. The previous
                // `tokio::time::sleep(Duration::from_millis(10))` with
                // a "Cleanup" comment was a placeholder for work that
                // does not exist. Add the real await here if a
                // shutdown step is added; today the transition is
                // instantaneous.
                self.state.send(AgentState::Stopping).map_err(|e| {
                    KodError::InvalidState(format!("Failed to update state: {:?}", e))
                })?;

                self.state.send(AgentState::Stopped).map_err(|e| {
                    KodError::InvalidState(format!("Failed to update state: {:?}", e))
                })?;''',
    "stop(): remove fake cleanup sleep",
)

# ----------------------------------------------------------------------
# 3. Tests: start/stop complete quickly.
# ----------------------------------------------------------------------
if "test_start_stop_are_not_sleep_bound" not in src:
    anchor = '''    #[test]
    fn test_capabilities() {'''
    if anchor not in src:
        print("  ERROR: test anchor not found")
        sys.exit(2)
    new_test = '''    /// start() and stop() must not contain artificial delays. They
    /// used to sleep 10 ms each, which added up across a swarm of
    /// agents and made lifecycle tests pay for a wall-clock cost that
    /// did no real work. The bound below is generous (100 ms) so a
    /// busy CI machine does not flake; the assertions fail loudly if
    /// a "Simulate initialization" sleep ever returns.
    #[tokio::test]
    async fn test_start_stop_are_not_sleep_bound() {
        use std::time::{Duration, Instant};

        let agent = Agent::new("no-sleep").build();
        let budget = Duration::from_millis(100);

        let t0 = Instant::now();
        agent.start().await.unwrap();
        let start_elapsed = t0.elapsed();
        assert!(
            start_elapsed < budget,
            "start() took {start_elapsed:?} — expected under {budget:?}"
        );

        let t0 = Instant::now();
        agent.stop().await.unwrap();
        let stop_elapsed = t0.elapsed();
        assert!(
            stop_elapsed < budget,
            "stop() took {stop_elapsed:?} — expected under {budget:?}"
        );

        // State machine is unchanged: Idle -> Running -> Stopped.
        assert_eq!(agent.state(), AgentState::Stopped);
    }

    #[test]
    fn test_capabilities() {'''
    src = src.replace(anchor, new_test, 1)
    print("  added test_start_stop_are_not_sleep_bound")
else:
    print("  test already present")

tmp = target + ".tmp"
with open(tmp, "w") as f:
    f.write(src)
os.replace(tmp, target)
print("Wrote", target)
PYEOF

if [ $? -ne 0 ]; then
    echo "ERROR: patching failed"
    exit 1
fi

echo
echo "=== Post-state: remaining sleeps in agent.rs ==="
grep -n "sleep\|Duration" "$AGENT"

echo
echo "cargo check --workspace --all-targets 2>&1 | tail -15"
if ! cargo check --workspace --all-targets 2>&1 | tail -15; then
    echo "Compilation failed"
    exit 1
fi

cat > /tmp/kod_commit_msg.txt <<'MSG'
fix(swarm): drop the fake init/cleanup sleeps from Agent::start and stop

Agent::start awaited `tokio::time::sleep(Duration::from_millis(10))`
with the comment "Simulate initialization"; Agent::stop awaited the
same with the comment "Cleanup". Nothing runs during either wait —
the agent has no initialization work and no shutdown work today.
Every start and stop paid 10 ms of wall clock, and every test that
drove an agent through a lifecycle (agent.rs tests, swarm.rs tests,
communication.rs tests) paid it too.

Remove both sleeps. The state transitions happen synchronously
through the watch channel, which is the honest description of the
work. The Starting and Stopping intermediate states remain in the
enum so a watcher that subscribes before calling start/stop can
still observe them if a real async step is ever added; the comment
in each method names where such a step would go.

Adds test_start_stop_are_not_sleep_bound: a start + stop cycle must
complete in under 100 ms total, and the state machine ends Stopped.
The bound is generous (a busy CI machine should not flake) but
fails loudly if the fake sleeps come back.
MSG

git add -A
git commit -F /tmp/kod_commit_msg.txt
rm -f /tmp/kod_commit_msg.txt
