#![cfg(test)]
    use super::*;
    use kod_swarm::file_touch::{FileTouchBus, FileTouchService};

    #[test]
    fn swarm_file_hook_records_and_publishes() {
        let bus = std::sync::Arc::new(FileTouchBus::new());
        let service = std::sync::Arc::new(FileTouchService::new());
        let mut rx = bus.subscribe();

        let hook = KodEngine::build_swarm_file_hook(bus.clone(), service.clone());
        hook.call(
            "swarm:agent-1",
            std::path::Path::new("/tmp/f.rs"),
            kod_tools::context::FileOp::Write,
            None,
        );

        // Service recorded it.
        assert!(service.has_touched("swarm:agent-1", &std::path::PathBuf::from("/tmp/f.rs")));

        // And the bus delivered a matching event.
        let ev = rx.try_recv().expect("one published event");
        let kod_swarm::file_touch::SwarmBusEvent::FileTouch(t) = ev;
        assert_eq!(t.agent_id, "swarm:agent-1");
        assert_eq!(t.path, std::path::PathBuf::from("/tmp/f.rs"));
        assert_eq!(t.op, kod_swarm::file_touch::FileOp::Write);
    }

    #[test]
    fn swarm_file_hook_uses_the_holder_arg_not_a_captured_value() {
        // The closure's `holder` argument is the source of truth for
        // the agent id. A context that gets re-used under a different
        // holder (which the engine does not currently do, but the
        // hook contract permits) must attribute to the caller.
        let bus = std::sync::Arc::new(FileTouchBus::new());
        let service = std::sync::Arc::new(FileTouchService::new());
        let hook = KodEngine::build_swarm_file_hook(bus, service.clone());

        hook.call(
            "swarm:first",
            std::path::Path::new("/a.rs"),
            kod_tools::context::FileOp::Read,
            None,
        );
        hook.call(
            "swarm:second",
            std::path::Path::new("/b.rs"),
            kod_tools::context::FileOp::Write,
            None,
        );

        assert!(service.has_touched("swarm:first", &std::path::PathBuf::from("/a.rs")));
        assert!(service.has_touched("swarm:second", &std::path::PathBuf::from("/b.rs")));
        assert!(!service.has_touched("swarm:first", &std::path::PathBuf::from("/b.rs")));
    }

    #[test]
    fn swarm_file_hook_reads_do_not_conflict_with_writes() {
        // End-to-end through the hook: two agents, one reads, one
        // writes the same file. The writer's conflict view is empty
        // (the reader did not modify anything); the reader's view
        // names the writer.
        let bus = std::sync::Arc::new(FileTouchBus::new());
        let service = std::sync::Arc::new(FileTouchService::new());
        let hook = KodEngine::build_swarm_file_hook(bus, service.clone());

        hook.call(
            "reader",
            std::path::Path::new("/f.rs"),
            kod_tools::context::FileOp::Read,
            None,
        );
        hook.call(
            "writer",
            std::path::Path::new("/f.rs"),
            kod_tools::context::FileOp::Write,
            None,
        );

        let writer_view = service.conflicts_for(&std::path::PathBuf::from("/f.rs"), "writer");
        assert!(
            writer_view.is_empty(),
            "reader is not a conflict for writer"
        );

        let reader_view = service.conflicts_for(&std::path::PathBuf::from("/f.rs"), "reader");
        assert_eq!(reader_view.len(), 1);
        assert_eq!(reader_view[0].peer, "writer");
    }
