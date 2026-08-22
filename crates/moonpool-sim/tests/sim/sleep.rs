use moonpool_sim::{Event, SimWorld};
use std::time::Duration;

#[tokio::test]
async fn test_basic_sleep() {
    let mut sim = SimWorld::new();

    // Verify initial state
    assert_eq!(sim.current_time(), Duration::ZERO);

    // Start sleep operation
    let sleep_future = sim.sleep(Duration::from_millis(100));

    // Verify sleep hasn't completed yet - time should still be zero
    assert_eq!(sim.current_time(), Duration::ZERO);

    // Verify the Wake event was scheduled
    assert!(sim.has_pending_events());
    assert_eq!(sim.pending_event_count(), 1);

    // Process the wake event
    sim.run_until_empty();

    // Now time should have advanced
    assert_eq!(sim.current_time(), Duration::from_millis(100));

    // Sleep future should now complete
    sleep_future.await.unwrap();
}

#[tokio::test]
async fn test_multiple_sleeps_sequential() {
    let mut sim = SimWorld::new();

    // First sleep
    let sleep1 = sim.sleep(Duration::from_millis(50));
    sim.run_until_empty();
    sleep1.await.unwrap();
    assert_eq!(sim.current_time(), Duration::from_millis(50));

    // Second sleep (should add to current time)
    let sleep2 = sim.sleep(Duration::from_millis(30));
    sim.run_until_empty();
    sleep2.await.unwrap();
    assert_eq!(sim.current_time(), Duration::from_millis(80));

    // Third sleep
    let sleep3 = sim.sleep(Duration::from_millis(20));
    sim.run_until_empty();
    sleep3.await.unwrap();
    assert_eq!(sim.current_time(), Duration::from_millis(100));
}

#[tokio::test]
async fn test_multiple_sleeps_concurrent() {
    let mut sim = SimWorld::new();

    // Schedule multiple concurrent sleeps
    let sleep1 = sim.sleep(Duration::from_millis(100));
    let sleep2 = sim.sleep(Duration::from_millis(50));
    let sleep3 = sim.sleep(Duration::from_millis(150));

    // Verify all events are scheduled
    assert_eq!(sim.pending_event_count(), 3);

    // Process all events
    sim.run_until_empty();

    // Time should advance to the latest event
    assert_eq!(sim.current_time(), Duration::from_millis(150));

    // All sleeps should complete successfully
    let (result1, result2, result3) = tokio::join!(sleep1, sleep2, sleep3);
    assert!(result1.is_ok());
    assert!(result2.is_ok());
    assert!(result3.is_ok());
}

#[tokio::test]
async fn test_event_ordering_and_processing() {
    let mut sim = SimWorld::new();

    // Schedule multiple wake events at different times using direct scheduling
    sim.schedule_event(Event::Timer { task_id: 1 }, Duration::from_millis(100));
    sim.schedule_event(Event::Timer { task_id: 2 }, Duration::from_millis(50));
    sim.schedule_event(Event::Timer { task_id: 3 }, Duration::from_millis(150));

    // Process events one by one
    assert!(sim.step()); // Should process task 2 first (50ms)
    assert_eq!(sim.current_time(), Duration::from_millis(50));

    assert!(sim.step()); // Should process task 1 next (100ms)  
    assert_eq!(sim.current_time(), Duration::from_millis(100));

    assert!(!sim.step()); // Should process task 3 last (150ms)
    assert_eq!(sim.current_time(), Duration::from_millis(150));
}

#[tokio::test]
async fn test_sleep_zero_duration() {
    let mut sim = SimWorld::new();

    // Sleep for zero duration
    let sleep_future = sim.sleep(Duration::ZERO);

    // Process events
    sim.run_until_empty();

    // Time should still be zero (event scheduled at current time)
    assert_eq!(sim.current_time(), Duration::ZERO);

    // Sleep should complete
    sleep_future.await.unwrap();
}

#[tokio::test]
async fn test_sleep_future_can_be_awaited_after_event_processing() {
    let mut sim = SimWorld::new();

    // Create sleep future
    let sleep_future = sim.sleep(Duration::from_millis(100));

    // Process events before awaiting
    sim.run_until_empty();
    assert_eq!(sim.current_time(), Duration::from_millis(100));

    // Future should complete immediately when awaited after event processing
    sleep_future.await.unwrap();
}

#[tokio::test]
async fn test_task_id_generation_is_unique() {
    let sim = SimWorld::new();

    // Create multiple sleep futures to verify unique task IDs
    let _sleep1 = sim.sleep(Duration::from_millis(10));
    let _sleep2 = sim.sleep(Duration::from_millis(20));
    let _sleep3 = sim.sleep(Duration::from_millis(30));

    // Should have 3 scheduled events (one per sleep)
    assert_eq!(sim.pending_event_count(), 3);

    // All should have different task IDs (verified by having 3 distinct events)
    // If task IDs were reused, we'd have fewer events due to deduplication
}

#[tokio::test]
async fn test_weak_sim_world_sleep_handling() {
    let sim = SimWorld::new();
    let _weak_sim = sim.downgrade();

    // Create sleep using the weak reference through the original sim
    let sleep_future = sim.sleep(Duration::from_millis(100));

    // Drop the strong reference
    drop(sim);

    // The sleep future should fail when polled because sim is dropped
    let result = sleep_future.await;
    assert!(result.is_err());
}
