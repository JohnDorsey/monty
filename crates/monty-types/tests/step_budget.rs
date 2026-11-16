//! The step counters and budgets of `ResourceTracker`, without an interpreter.

use monty_types::{ResourceError, ResourceLimits, ResourceTracker, TimeLimitScope};

#[test]
fn counts_accumulate_in_all_three_scopes_and_reset_where_the_time_clocks_do() {
    let tracker = ResourceTracker::default();
    tracker.add_steps(10);
    assert_eq!(
        (tracker.steps(), tracker.feed_steps(), tracker.turn_steps()),
        (10, 10, 10)
    );
    tracker.on_turn_start();
    tracker.add_steps(5);
    assert_eq!(
        (tracker.steps(), tracker.feed_steps(), tracker.turn_steps()),
        (15, 15, 5)
    );
    tracker.on_feed_start();
    assert_eq!(
        (tracker.steps(), tracker.feed_steps(), tracker.turn_steps()),
        (15, 0, 0)
    );
}

#[test]
fn a_budget_reports_the_scope_the_limit_and_the_count() {
    let tracker = ResourceTracker::new(ResourceLimits::default().max_turn_steps(100).max_feed_steps(150));
    tracker.add_steps(100);
    tracker.check_time().expect("exactly at the limit is allowed");
    tracker.add_steps(1);
    match tracker.check_time() {
        Err(ResourceError::Steps { scope, limit, used }) => {
            assert_eq!((scope, limit, used), (TimeLimitScope::Turn, 100, 101));
        }
        other => panic!("expected a turn step error, got {other:?}"),
    }
    // A new turn clears the turn budget; the feed budget (150) is still being spent.
    tracker.on_turn_start();
    tracker.add_steps(49);
    tracker.check_time().expect("150 of 150 steps");
    tracker.add_steps(1);
    match tracker.check_time() {
        Err(ResourceError::Steps { scope, limit, used }) => {
            assert_eq!((scope, limit, used), (TimeLimitScope::Feed, 150, 151));
        }
        other => panic!("expected a feed step error, got {other:?}"),
    }
    assert_eq!(
        ResourceError::Steps {
            scope: TimeLimitScope::Feed,
            limit: 150,
            used: 151
        }
        .to_string(),
        "feed step limit exceeded: 151 steps > 150 steps"
    );
}

#[test]
fn native_loops_charge_one_interval_per_check() {
    let tracker = ResourceTracker::new(ResourceLimits::default().max_turn_steps(1_000));
    for i in 0..640 {
        tracker.check_time_every(i).unwrap();
    }
    assert_eq!(tracker.steps(), 640, "ten full intervals of 64 items");
    // A partial interval is not charged until it completes.
    for i in 0..63 {
        tracker.check_time_every(i).unwrap();
    }
    assert_eq!(tracker.steps(), 640);
    let mut stopped_at = None;
    for i in 0..10_000 {
        if tracker.check_time_every(i).is_err() {
            stopped_at = Some(i);
            break;
        }
    }
    assert!(stopped_at.is_some(), "the budget was never enforced");
    assert!(tracker.steps() > 1_000 && tracker.steps() <= 1_000 + 64 + 64);
}

#[test]
fn a_limit_is_enforced_by_check_memory_time_too() {
    let tracker = ResourceTracker::new(ResourceLimits::default().max_feed_steps(10));
    assert!(tracker.has_step_limit());
    assert!(
        tracker.has_memory_time_limit(),
        "the interpreter hoists this test, so it must know about steps"
    );
    tracker.add_steps(11);
    assert!(matches!(tracker.check_memory_time(), Err(ResourceError::Steps { .. })));
}

#[test]
fn restored_limits_never_loosen_the_step_budgets() {
    let mut tracker = ResourceTracker::new(ResourceLimits::default().max_turn_steps(500));
    tracker.tighten_limits(&ResourceLimits::default().max_turn_steps(100).max_feed_steps(900));
    assert_eq!(tracker.max_turn_steps(), Some(100));
    assert_eq!(tracker.max_feed_steps(), Some(900));
    tracker.tighten_limits(&ResourceLimits::default().max_turn_steps(700));
    assert_eq!(
        tracker.max_turn_steps(),
        Some(100),
        "a looser ceiling must not raise a stricter saved limit"
    );
}

#[test]
fn a_snapshot_keeps_the_session_and_feed_counts_but_starts_a_fresh_turn() {
    let tracker = ResourceTracker::new(ResourceLimits::default().max_feed_steps(1_000));
    tracker.add_steps(300);
    let bytes = minicbor_serde::to_vec(&tracker).unwrap();
    let restored: ResourceTracker = minicbor_serde::from_slice(&bytes).unwrap();
    assert_eq!(restored.steps(), 300);
    assert_eq!(
        restored.feed_steps(),
        300,
        "a dump taken mid-feed resumes that feed's budget"
    );
    assert_eq!(
        restored.turn_steps(),
        0,
        "the resume that loads a dump starts a fresh turn"
    );
    assert_eq!(restored.max_feed_steps(), Some(1_000));
}

#[test]
fn a_dump_written_before_step_budgets_existed_still_loads() {
    // The dump payload names its fields, so a tracker without the new keys must load with zero steps.
    let current = ResourceTracker::default();
    let value: serde_json::Value = serde_json::to_value(&current).unwrap();
    let mut object = value.as_object().unwrap().clone();
    for key in ["total_steps", "feed_steps"] {
        object
            .remove(key)
            .unwrap_or_else(|| panic!("{key} is not a serialized field"));
    }
    let limits = object.get_mut("limits").unwrap().as_object_mut().unwrap();
    for key in ["max_feed_steps", "max_turn_steps"] {
        limits.remove(key);
    }
    let old: ResourceTracker = serde_json::from_value(serde_json::Value::Object(object)).unwrap();
    assert_eq!((old.steps(), old.feed_steps(), old.turn_steps()), (0, 0, 0));
    assert_eq!((old.max_feed_steps(), old.max_turn_steps()), (None, None));
}
