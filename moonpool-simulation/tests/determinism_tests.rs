use moonpool_simulation::{Event, SimWorld};
use std::time::Duration;

#[test]
fn deterministic_event_execution_order() {
    // Test that the exact same sequence of scheduling produces identical execution
    // across multiple runs (this tests the core determinism guarantee)

    fn run_simulation() -> Vec<Duration> {
        let mut sim = SimWorld::new();
        let mut execution_times = Vec::new();

        // Schedule events in a specific pattern
        sim.schedule_event(Event::Timer { task_id: 1 }, Duration::from_millis(100));
        sim.schedule_event(Event::Timer { task_id: 2 }, Duration::from_millis(50));
        sim.schedule_event(Event::Timer { task_id: 3 }, Duration::from_millis(100)); // Same time as task 1
        sim.schedule_event(Event::Timer { task_id: 4 }, Duration::from_millis(75));
        sim.schedule_event(Event::Timer { task_id: 5 }, Duration::from_millis(100)); // Same time as task 1&3

        // Execute and record the time progression
        // Use a different approach: process all events and record each step
        while sim.has_pending_events() {
            let had_events = sim.step();
            execution_times.push(sim.current_time());
            if !had_events {
                break; // This was the last event
            }
        }

        execution_times
    }

    // Run the same simulation multiple times
    let results: Vec<Vec<Duration>> = (0..10).map(|_| run_simulation()).collect();

    // All executions should produce identical time progressions
    let first_result = &results[0];
    for (i, result) in results.iter().enumerate().skip(1) {
        assert_eq!(
            result,
            first_result,
            "Run {} produced different execution times than the first run. Expected: {:?}, Got: {:?}",
            i + 1,
            first_result,
            result
        );
    }

    // Verify the expected execution order (events should execute in time order,
    // with ties broken by sequence order)
    let expected_times = vec![
        Duration::from_millis(50),  // Task 2
        Duration::from_millis(75),  // Task 4
        Duration::from_millis(100), // Task 1 (first scheduled at t=100)
        Duration::from_millis(100), // Task 3 (second scheduled at t=100)
        Duration::from_millis(100), // Task 5 (third scheduled at t=100)
    ];

    assert_eq!(first_result, &expected_times);
}

#[test]
fn same_time_events_sequence_order() {
    // Test that events scheduled at exactly the same time are processed
    // in the order they were scheduled (sequence-based determinism)

    let mut sim = SimWorld::new();
    let target_time = Duration::from_millis(100);

    // Schedule multiple events at exactly the same time
    sim.schedule_event_at(Event::Timer { task_id: 10 }, target_time);
    sim.schedule_event_at(Event::Timer { task_id: 20 }, target_time);
    sim.schedule_event_at(Event::Timer { task_id: 30 }, target_time);
    sim.schedule_event_at(Event::Timer { task_id: 40 }, target_time);

    assert_eq!(sim.pending_event_count(), 4);
    assert_eq!(sim.current_time(), Duration::ZERO);

    // All events should execute at the same simulation time but in sequence order
    assert!(sim.step());
    assert_eq!(sim.current_time(), target_time); // Time advances to target

    // Next steps should not advance time (same timestamp) but should process in sequence
    assert!(sim.step());
    assert_eq!(sim.current_time(), target_time); // Time stays the same

    assert!(sim.step());
    assert_eq!(sim.current_time(), target_time); // Time stays the same

    assert!(!sim.step()); // Last event
    assert_eq!(sim.current_time(), target_time); // Time stays the same

    // All events processed
    assert!(!sim.has_pending_events());
    assert_eq!(sim.pending_event_count(), 0);
}

#[test]
fn interleaved_scheduling_consistency() {
    // Test that different scheduling orders produce the same execution
    // when events have the same timestamps (determinism despite scheduling order)

    fn create_sim_variant_1() -> SimWorld {
        let sim = SimWorld::new();
        sim.schedule_event_at(Event::Timer { task_id: 1 }, Duration::from_millis(100));
        sim.schedule_event_at(Event::Timer { task_id: 2 }, Duration::from_millis(100));
        sim.schedule_event_at(Event::Timer { task_id: 3 }, Duration::from_millis(100));
        sim
    }

    fn create_sim_variant_2() -> SimWorld {
        let sim = SimWorld::new();
        // Schedule in reverse order
        sim.schedule_event_at(Event::Timer { task_id: 3 }, Duration::from_millis(100));
        sim.schedule_event_at(Event::Timer { task_id: 2 }, Duration::from_millis(100));
        sim.schedule_event_at(Event::Timer { task_id: 1 }, Duration::from_millis(100));
        sim
    }

    fn create_sim_variant_3() -> SimWorld {
        let sim = SimWorld::new();
        // Schedule in mixed order
        sim.schedule_event_at(Event::Timer { task_id: 2 }, Duration::from_millis(100));
        sim.schedule_event_at(Event::Timer { task_id: 1 }, Duration::from_millis(100));
        sim.schedule_event_at(Event::Timer { task_id: 3 }, Duration::from_millis(100));
        sim
    }

    // Execute all variants
    let mut sim1 = create_sim_variant_1();
    let mut sim2 = create_sim_variant_2();
    let mut sim3 = create_sim_variant_3();

    sim1.run_until_empty();
    sim2.run_until_empty();
    sim3.run_until_empty();

    // All should end up at the same final time (deterministic execution)
    assert_eq!(sim1.current_time(), Duration::from_millis(100));
    assert_eq!(sim2.current_time(), Duration::from_millis(100));
    assert_eq!(sim3.current_time(), Duration::from_millis(100));

    // All should have processed all events
    assert!(!sim1.has_pending_events());
    assert!(!sim2.has_pending_events());
    assert!(!sim3.has_pending_events());
}

#[test]
fn time_advancement_deterministic() {
    // Test that time advancement follows a predictable, deterministic pattern

    let mut sim = SimWorld::new();

    // Schedule events at various times
    sim.schedule_event(Event::Timer { task_id: 3 }, Duration::from_millis(150));
    sim.schedule_event(Event::Timer { task_id: 1 }, Duration::from_millis(50));
    sim.schedule_event(Event::Timer { task_id: 2 }, Duration::from_millis(100));

    // Time should start at zero and only advance when events are processed
    assert_eq!(sim.current_time(), Duration::ZERO);
    assert_eq!(sim.pending_event_count(), 3);

    // First event should be at t=50 (earliest)
    assert!(sim.step());
    assert_eq!(sim.current_time(), Duration::from_millis(50));
    assert_eq!(sim.pending_event_count(), 2);

    // Second event should be at t=100
    assert!(sim.step());
    assert_eq!(sim.current_time(), Duration::from_millis(100));
    assert_eq!(sim.pending_event_count(), 1);

    // Third event should be at t=150 (no more events after this)
    assert!(!sim.step()); // Returns false because it's the last event
    assert_eq!(sim.current_time(), Duration::from_millis(150));
    assert_eq!(sim.pending_event_count(), 0);

    // No more events to process
    assert!(!sim.has_pending_events());
}

#[test]
fn empty_simulation_behavior() {
    // Test edge case behavior with no events

    let mut sim = SimWorld::new();

    // Initial state
    assert_eq!(sim.current_time(), Duration::ZERO);
    assert!(!sim.has_pending_events());
    assert_eq!(sim.pending_event_count(), 0);

    // Step should return false (no events to process)
    assert!(!sim.step());
    assert_eq!(sim.current_time(), Duration::ZERO); // Time doesn't advance

    // run_until_empty should complete immediately
    sim.run_until_empty();
    assert_eq!(sim.current_time(), Duration::ZERO);
    assert!(!sim.has_pending_events());
}
