//! The scheduling and limit rules of the deterministic memory limit, on the tracker alone.
//!
//! What the model counts is the interpreter's business (`monty`'s `tests/model_memory_costs.rs`). These tests pin
//! when the interpreter is told to count, because that schedule is what makes a script stop at the same step
//! everywhere: every rule below depends on counters the program produced and on nothing else.

use monty_types::{ResourceError, ResourceLimits, ResourceTracker};

const LIMIT: usize = 1_000_000;

fn tracker() -> ResourceTracker {
    ResourceTracker::new(ResourceLimits::default().max_model_memory(LIMIT))
}

#[test]
fn without_the_limit_nothing_is_ever_due() {
    let tracker = ResourceTracker::default();
    assert!(!tracker.has_model_limit());
    tracker.add_steps(10_000_000);
    tracker.note_model_growth(usize::MAX);
    assert!(!tracker.model_walk_due());
    assert_eq!(tracker.max_model_memory(), None);
}

#[test]
fn a_model_limit_alone_counts_as_a_limit_for_the_dispatch_loop() {
    // The interpreter reads this once per run to decide whether its checkpoints have anything to do.
    assert!(tracker().has_memory_time_limit());
}

#[test]
fn a_small_heap_is_walked_when_the_bound_could_reach_the_limit() {
    let tracker = tracker();
    tracker.record_model_walk(0, 10).unwrap();
    // 1,000,000 bytes at 12 a step is 83,333 steps; spacing is 256 steps at the least.
    tracker.add_steps(255);
    assert!(!tracker.model_walk_due(), "below the spacing");
    tracker.add_steps(80_000);
    assert!(!tracker.model_walk_due(), "the bound is still under the limit");
    tracker.add_steps(3_400);
    assert!(tracker.model_walk_due(), "83,655 steps at 12 bytes is over a million");
}

#[test]
fn what_the_last_walk_found_counts_toward_the_bound() {
    let tracker = tracker();
    tracker.record_model_walk(LIMIT - 12 * 300, 10).unwrap();
    tracker.add_steps(299);
    assert!(!tracker.model_walk_due());
    tracker.add_steps(2);
    assert!(tracker.model_walk_due());
}

#[test]
fn a_big_heap_is_walked_less_often() {
    let tracker = tracker();
    // A hundred thousand live objects: walks are at least half as many steps apart.
    tracker.record_model_walk(LIMIT - 1, 100_000).unwrap();
    tracker.add_steps(49_999);
    assert!(
        !tracker.model_walk_due(),
        "within the spacing, however close to the limit"
    );
    tracker.add_steps(1);
    assert!(tracker.model_walk_due());
}

#[test]
fn what_is_taken_on_counts_toward_the_bound_but_not_against_the_spacing_of_a_bulk_allocation() {
    let tracker = tracker();
    tracker.record_model_walk(LIMIT / 2, 100_000).unwrap();
    // Smaller than an eighth of the limit: it waits for the spacing like everything else.
    tracker.note_model_growth(LIMIT / 8 - 1);
    assert!(!tracker.model_walk_due());
    // An eighth of the limit that could carry the session over it is measured at once.
    tracker.note_model_growth(LIMIT / 2);
    assert!(tracker.model_walk_due());
    // An eighth of the limit that cannot is not worth a walk.
    let tracker = self::tracker();
    tracker.record_model_walk(0, 100_000).unwrap();
    tracker.note_model_growth(LIMIT / 8);
    assert!(!tracker.model_walk_due());
}

#[test]
fn a_walk_is_charged_to_the_step_count_and_restarts_the_schedule() {
    let tracker = tracker();
    tracker.add_steps(1000);
    tracker.note_model_growth(500);
    // Exactly at the limit, so that the bound is over it the moment the spacing allows a walk.
    tracker.record_model_walk(LIMIT, 4_000).unwrap();
    // One step for every four live objects.
    assert_eq!(tracker.steps(), 1000 + 1000);
    assert_eq!(tracker.model_used(), LIMIT);
    assert_eq!(tracker.model_pending(), 0);
    // The spacing is half the live objects, 2,000 steps, measured from the end of the walk, charge included.
    tracker.add_steps(1_999);
    assert!(!tracker.model_walk_due());
    tracker.add_steps(1);
    assert!(tracker.model_walk_due());
}

#[test]
fn a_walk_over_the_limit_is_the_error() {
    let tracker = tracker();
    assert!(
        tracker.record_model_walk(LIMIT, 1).is_ok(),
        "exactly at the limit is allowed"
    );
    let error = tracker.record_model_walk(LIMIT + 1, 1).unwrap_err();
    assert!(matches!(error, ResourceError::ModelMemory { limit: LIMIT, used } if used == LIMIT + 1));
    assert_eq!(
        error.to_string(),
        "model memory limit exceeded: 1000001 bytes > 1000000 bytes"
    );
}

#[test]
fn one_request_bigger_than_the_whole_limit_is_refused_and_a_smaller_one_is_not() {
    let tracker = tracker();
    tracker.record_model_walk(300_000, 1).unwrap();
    // The refusal reports what was held at the last walk plus the request.
    let error = tracker.check_allocation(LIMIT + 1).unwrap_err();
    assert!(matches!(
        error,
        ResourceError::ModelMemory { limit: LIMIT, used } if used == 300_000 + LIMIT + 1
    ));
    // A request that would not fit beside what is held is not refused up front: the last walk may be stale.
    tracker.check_allocation(LIMIT).expect("judged at the next walk");
    assert_eq!(tracker.model_pending(), LIMIT);
}

#[test]
fn the_pre_checks_that_come_from_buffer_capacities_never_reach_the_model() {
    // A vector doubling or a hash table rehashing asks for an amount that depends on how the buffer grew, on
    // the standard library and on the platform's table layout. If the model heard it, a restored list (capacity
    // equal to its length) and an appended one (capacity a power of two) would schedule walks differently.
    let tracker = tracker();
    tracker
        .check_pending_allocation(10 * LIMIT)
        .expect("not the model's business");
    assert_eq!(tracker.model_pending(), 0);
    tracker.check_growth(1 << 30, 1 << 30, 16).expect("nor this");
    assert_eq!(tracker.model_pending(), 0);
}

#[test]
fn the_schedule_survives_serialization() {
    let tracker = tracker();
    tracker.add_steps(7_000);
    tracker.record_model_walk(400_000, 1_234).unwrap();
    tracker.note_model_growth(5_000);
    let restored = round_trip(&tracker);
    assert_eq!(restored.model_used(), 400_000);
    assert_eq!(restored.model_pending(), 5_000);
    assert_eq!(restored.steps(), tracker.steps());
    assert_eq!(restored.model_walk_due(), tracker.model_walk_due());
    assert_eq!(restored.max_model_memory(), Some(LIMIT));
    // The same walk is due at the same step in both, which is what makes a restored session go on as the
    // original would have.
    for _ in 0..600 {
        tracker.add_steps(100);
        restored.add_steps(100);
        assert_eq!(restored.model_walk_due(), tracker.model_walk_due());
    }
}

/// A serde round trip through JSON, which is enough to show every field carries.
fn round_trip(tracker: &ResourceTracker) -> ResourceTracker {
    let text = serde_json::to_string(tracker).unwrap();
    serde_json::from_str(&text).unwrap()
}

#[test]
fn a_tracker_saved_before_the_limit_existed_loads_without_it() {
    // The new fields are `#[serde(default)]`, so older saved sessions still load.
    let mut value = serde_json::to_value(ResourceTracker::default()).unwrap();
    let object = value.as_object_mut().unwrap();
    for key in ["model_used", "model_pending", "model_walk_steps", "model_walk_entries"] {
        assert!(object.remove(key).is_some(), "{key} is serialized");
    }
    object
        .get_mut("limits")
        .and_then(|limits| limits.as_object_mut())
        .unwrap()
        .remove("max_model_memory");
    let tracker: ResourceTracker = serde_json::from_value(value).unwrap();
    assert_eq!(tracker.max_model_memory(), None);
    assert_eq!(tracker.model_used(), 0);
}
