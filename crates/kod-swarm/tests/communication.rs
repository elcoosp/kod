use kod_swarm::communication::{AgentCommunicationHub, MessageContent, MessageDestination};
use kod_types::{AgentId, Priority, TaskStatus};

#[tokio::test]
async fn test_direct_message() {
    let hub = AgentCommunicationHub::new();

    let agent_a = AgentId::new();
    let agent_b = AgentId::new();

    // Register agents
    hub.register_agent(agent_a.clone()).await.unwrap();
    hub.register_agent(agent_b.clone()).await.unwrap();

    // Send direct message
    hub.send_direct(
        &agent_a,
        &agent_b,
        MessageContent::TaskAssignment {
            description: "Implement user auth".to_string(),
            priority: Priority::High,
        },
    ).await.unwrap();

    // Agent B should receive the message
    let receiver = hub.get_agent_receiver(&agent_b).await.unwrap();

    let message = receiver.recv().await.unwrap();
    assert_eq!(message.from, agent_a);
    assert_eq!(message.to, MessageDestination::Agent(agent_b.clone()));

    match message.content {
        MessageContent::TaskAssignment { description, priority } => {
            assert_eq!(description, "Implement user auth");
            assert_eq!(priority, Priority::High);
        }
        _ => panic!("Expected TaskAssignment message"),
    }
}

#[tokio::test]
async fn test_broadcast_message() {
    let hub = AgentCommunicationHub::new();

    let agent_a = AgentId::new();
    let agent_b = AgentId::new();
    let agent_c = AgentId::new();

    // Register agents
    hub.register_agent(agent_a.clone()).await.unwrap();
    hub.register_agent(agent_b.clone()).await.unwrap();
    hub.register_agent(agent_c.clone()).await.unwrap();

    // Broadcast from agent A
    hub.broadcast(
        &agent_a,
        MessageContent::KnowledgeShare {
            information: "Found a bug in auth module".to_string(),
            tags: vec!["auth".to_string(), "bug".to_string()],
        },
    ).await.unwrap();

    // Both B and C should receive
    let receiver_b = hub.get_agent_receiver(&agent_b).await.unwrap();
    let message_b = receiver_b.recv().await.unwrap();
    assert_eq!(message_b.from, agent_a);

    let receiver_c = hub.get_agent_receiver(&agent_c).await.unwrap();
    let message_c = receiver_c.recv().await.unwrap();
    assert_eq!(message_c.from, agent_a);
}

#[tokio::test]
async fn test_agent_not_found() {
    let hub = AgentCommunicationHub::new();

    let agent_a = AgentId::new();
    let nonexistent = AgentId::new();

    hub.register_agent(agent_a.clone()).await.unwrap();

    // Try to send to non-existent agent
    let result = hub.send_direct(
        &agent_a,
        &nonexistent,
        MessageContent::ProgressUpdate {
            status: TaskStatus::InProgress,
            details: "Test".to_string(),
        },
    ).await;

    assert!(result.is_err());
}

#[tokio::test]
async fn test_agent_offline() {
    let hub = AgentCommunicationHub::new();

    let agent_a = AgentId::new();
    let agent_b = AgentId::new();

    hub.register_agent(agent_a.clone()).await.unwrap();
    hub.register_agent(agent_b.clone()).await.unwrap();

    // Mark agent B as offline
    hub.set_agent_offline(&agent_b).await;

    // Try to send to offline agent
    let result = hub.send_direct(
        &agent_a,
        &agent_b,
        MessageContent::HelpRequest {
            question: "Can you help?".to_string(),
            context: "Auth implementation".to_string(),
        },
    ).await;

    assert!(result.is_err());
}

#[tokio::test]
async fn test_unregister_agent() {
    let hub = AgentCommunicationHub::new();

    let agent_a = AgentId::new();
    let agent_b = AgentId::new();

    hub.register_agent(agent_a.clone()).await.unwrap();
    hub.register_agent(agent_b.clone()).await.unwrap();

    // Unregister agent B
    hub.unregister_agent(&agent_b).await;

    // Try to send to unregistered agent
    let result = hub.send_direct(
        &agent_a,
        &agent_b,
        MessageContent::ResultDelivery {
            result: "Done".to_string(),
        },
    ).await;

    assert!(result.is_err());
}

#[tokio::test]
async fn test_message_history() {
    let hub = AgentCommunicationHub::new();

    let agent_a = AgentId::new();
    let agent_b = AgentId::new();

    hub.register_agent(agent_a.clone()).await.unwrap();
    hub.register_agent(agent_b.clone()).await.unwrap();

    // Send multiple messages
    hub.send_direct(
        &agent_a,
        &agent_b,
        MessageContent::TaskAssignment {
            description: "Task 1".to_string(),
            priority: Priority::Medium,
        },
    ).await.unwrap();

    hub.send_direct(
        &agent_b,
        &agent_a,
        MessageContent::ProgressUpdate {
            status: TaskStatus::Completed,
            details: "Finished task 1".to_string(),
        },
    ).await.unwrap();

    // Get history for agent A
    let history_a = hub.get_agent_history(&agent_a).await;
    assert_eq!(history_a.len(), 2); // Sent + received

    // Get history for agent B
    let history_b = hub.get_agent_history(&agent_b).await;
    assert_eq!(history_b.len(), 2);
}

#[tokio::test]
async fn test_coordination_messages() {
    let hub = AgentCommunicationHub::new();

    let agent_a = AgentId::new();
    let agent_b = AgentId::new();

    hub.register_agent(agent_a.clone()).await.unwrap();
    hub.register_agent(agent_b.clone()).await.unwrap();

    hub.send_direct(
        &agent_a,
        &agent_b,
        MessageContent::Coordination {
            action: kod_types::CoordinationAction::RequestingSync,
        },
    ).await.unwrap();

    let receiver = hub.get_agent_receiver(&agent_b).await.unwrap();
    let message = receiver.recv().await.unwrap();

    match message.content {
        MessageContent::Coordination { action } => {
            match action {
                kod_types::CoordinationAction::RequestingSync => {}
                _ => panic!("Expected RequestingSync"),
            }
        }
        _ => panic!("Expected Coordination message"),
    }
}
