use kod_swarm::agent::{Agent, AgentBuilder, AgentState, Capability};
use std::time::Duration;

#[tokio::test]
async fn test_agent_creation() {
    let agent = Agent::new("architect")
        .with_capability(Capability::Planning)
        .build();

    assert_eq!(agent.name(), "architect");
    assert_eq!(agent.state(), AgentState::Idle);
    // ID should be a valid AgentId
    assert!(!agent.id().to_string().is_empty());
}

#[tokio::test]
async fn test_agent_builder() {
    let agent = AgentBuilder::new("coder")
        .with_capability(Capability::Coding)
        .with_capability(Capability::Testing)
        .with_model("codellama:13b")
        .with_max_context_tokens(8192)
        .build();

    assert_eq!(agent.name(), "coder");
    assert!(agent.has_capability(&Capability::Coding));
    assert!(agent.has_capability(&Capability::Testing));
    assert!(!agent.has_capability(&Capability::Planning));
}

#[tokio::test]
async fn test_agent_lifecycle() {
    let mut agent = Agent::new("worker").build();

    // Initial state
    assert_eq!(agent.state(), AgentState::Idle);

    // Start agent
    agent.start().await.unwrap();
    assert_eq!(agent.state(), AgentState::Running);

    // Pause agent
    agent.pause().await.unwrap();
    assert_eq!(agent.state(), AgentState::Paused);

    // Resume
    agent.resume().await.unwrap();
    assert_eq!(agent.state(), AgentState::Running);

    // Stop
    agent.stop().await.unwrap();
    assert_eq!(agent.state(), AgentState::Stopped);
}

#[tokio::test]
async fn test_agent_capabilities() {
    let agent = AgentBuilder::new("fullstack")
        .with_capability(Capability::Coding)
        .with_capability(Capability::Testing)
        .with_capability(Capability::Documentation)
        .with_capability(Capability::CodeReview)
        .build();

    let capabilities = agent.capabilities();
    assert_eq!(capabilities.len(), 4);
    assert!(agent.has_capability(&Capability::Coding));
    assert!(agent.has_capability(&Capability::Documentation));
}

#[tokio::test]
async fn test_agent_heartbeat() {
    let mut agent = Agent::new("worker").build();

    agent.start().await.unwrap();

    // Agent should track last heartbeat
    let heartbeat = agent.last_heartbeat();
    assert!(heartbeat.is_some());

    // Update heartbeat
    tokio::time::sleep(Duration::from_millis(10)).await;
    agent.record_heartbeat();

    let new_heartbeat = agent.last_heartbeat();
    assert!(new_heartbeat.is_some());

    // New heartbeat should be later than old
    if let (Some(old), Some(new)) = (heartbeat, new_heartbeat) {
        assert!(new > old);
    }
}

#[tokio::test]
async fn test_agent_is_idle_timeout() {
    let mut agent = Agent::new("worker").build();
    agent.start().await.unwrap();

    // Fresh heartbeat means not timed out
    agent.record_heartbeat();
    assert!(!agent.is_timed_out(Duration::from_secs(60)));

    // Simulate old heartbeat by not updating
    agent.set_last_heartbeat_for_test(std::time::Instant::now() - Duration::from_secs(120));
    assert!(agent.is_timed_out(Duration::from_secs(60)));
}

#[test]
fn test_capability_equality() {
    assert_eq!(Capability::Coding, Capability::Coding);
    assert_ne!(Capability::Coding, Capability::Testing);

    // Test string representation
    assert_eq!(Capability::Coding.as_str(), "coding");
    assert_eq!(Capability::Planning.as_str(), "planning");
}

#[test]
fn test_capability_from_str() {
    assert_eq!("coding".parse::<Capability>().unwrap(), Capability::Coding);
    assert_eq!("testing".parse::<Capability>().unwrap(), Capability::Testing);
    assert!("invalid".parse::<Capability>().is_err());
}
