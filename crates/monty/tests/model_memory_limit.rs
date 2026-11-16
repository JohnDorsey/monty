//! `max_model_memory`: a memory limit that stops a script at the same step on every machine.
//!
//! The allocator-backed `max_memory` reads what the whole process has requested, so the step at which a
//! runaway script is stopped moves with every other allocation in the process. This limit reads the
//! deterministic cost table instead (`tests/model_memory_costs.rs`), so it must not.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use monty::{Dump, MontyRepl, Session, SessionRef, dump};
use monty_types::{CompileOptions, ExcType, MontyException, PrintWriter, ResourceLimits, ResourceTracker};

/// A time limit so long that only a broken memory limit ever meets it, short enough that a failing test ends.
const SAFETY_NET: Duration = Duration::from_secs(30);

const LIMIT: usize = 1_000_000;

/// Scripts that never stop on their own: each grows the session one way until the limit ends it.
const RUNAWAYS: [(&str, &str); 6] = [
    ("small lists", "x = []\nwhile True:\n    x.append([0])\n"),
    (
        // Long enough that each is an object of its own: short strings live inside the value that holds them.
        "strings",
        "x = []\ni = 0\nwhile True:\n    x.append('prefix-%d-abcdefghijklmnopqrstuvwxyz' % i)\n    i += 1\n",
    ),
    ("dict of ints", "d = {}\ni = 0\nwhile True:\n    d[i] = i\n    i += 1\n"),
    ("one growing list", "x = []\nwhile True:\n    x.append(0)\n"),
    (
        "big integers",
        "x = []\nn = 3 ** 1000\nwhile True:\n    x.append(n + len(x))\n",
    ),
    (
        // Nothing is on the heap here: what grows is the source and code every `exec` keeps for the session.
        "exec",
        "while True:\n    exec('x = 1')\n",
    ),
];

fn limits(limit: usize) -> ResourceLimits {
    ResourceLimits::default()
        .max_model_memory(limit)
        .max_feed_duration(SAFETY_NET)
}

fn new_repl(limit: usize) -> MontyRepl {
    MontyRepl::new(
        "model.py",
        ResourceTracker::new(limits(limit)),
        CompileOptions::default(),
    )
}

fn feed(repl: &mut MontyRepl, code: &str) -> Result<(), MontyException> {
    repl.feed_run(code, vec![], PrintWriter::Disabled).map(|_| ())
}

/// Where a runaway script was stopped: the steps it had taken, and the error it was stopped with.
fn trip(code: &str, limit: usize) -> (u64, MontyException) {
    let mut repl = new_repl(limit);
    let error = feed(&mut repl, code).expect_err("a runaway script must be stopped");
    (repl.tracker().steps(), error)
}

/// The number a model memory error reports as used, from its message.
fn used_bytes(error: &MontyException) -> usize {
    let message = error.message().unwrap_or_default();
    message
        .strip_prefix("model memory limit exceeded: ")
        .and_then(|rest| rest.split(' ').next())
        .and_then(|number| number.parse().ok())
        .unwrap_or_else(|| panic!("not a model memory error: {message}"))
}

#[test]
fn every_runaway_is_stopped_with_a_memory_error() {
    for (name, code) in RUNAWAYS {
        let (_, error) = trip(code, LIMIT);
        assert_eq!(error.exc_type(), ExcType::MemoryError, "{name}: {error}");
        let used = used_bytes(&error);
        assert!(used > LIMIT, "{name}: used {used} is not above the limit");
        assert_eq!(
            error.message().unwrap(),
            format!("model memory limit exceeded: {used} bytes > {LIMIT} bytes"),
            "{name}"
        );
    }
}

#[test]
fn the_overshoot_is_a_tenth_of_the_limit_at_most() {
    // Walks are spaced by the size of the heap, so that measuring costs a bounded share of the run: a heap of
    // thousands of objects is measured every few thousand steps, and may pass the limit by what a runaway
    // allocates in between.
    for (name, code) in RUNAWAYS {
        let (_, error) = trip(code, LIMIT);
        let over = used_bytes(&error) - LIMIT;
        assert!(over < LIMIT / 10, "{name}: stopped {over} bytes past the limit");
    }
}

#[test]
fn a_few_big_objects_are_stopped_within_kilobytes() {
    // One list growing in place has one heap object, so it is measured at every checkpoint: it can pass the
    // limit by what 256 instructions add, at most 16 bytes a push.
    let (_, error) = trip(
        "x = []
while True:
    x.append(0)
",
        LIMIT,
    );
    assert!(used_bytes(&error) - LIMIT < 8192);
}

#[test]
fn the_overshoot_is_small_when_the_heap_is_big() {
    // Two hundred thousand live objects: a walk of this heap is not done at every checkpoint, but at most
    // every hundred thousand steps (half a visit per object per step). A runaway adds about 18 bytes a step,
    // so it could pass the limit by about two megabytes, 5 percent; measured, it passes by 1.4 percent, because
    // what it has allocated since the last walk cuts the stretch short.
    let limit = 40_000_000;
    let setup = "keep = [[0] for i in range(200_000)]\n";
    let mut repl = new_repl(limit);
    feed(&mut repl, setup).expect("the setup fits");
    let error = feed(&mut repl, "x = []\nwhile True:\n    x.append([0])\n").unwrap_err();
    assert_eq!(error.exc_type(), ExcType::MemoryError);
    let over = used_bytes(&error) - limit;
    assert!(over < limit / 10, "stopped {over} bytes past a limit of {limit}");
}

#[test]
fn a_runaway_stops_at_the_same_step_every_time() {
    for (name, code) in RUNAWAYS {
        let (steps, error) = trip(code, LIMIT);
        for _ in 0..3 {
            let (again, same) = trip(code, LIMIT);
            assert_eq!(again, steps, "{name}");
            assert_eq!(same.message(), error.message(), "{name}");
        }
    }
}

/// The step each runaway is stopped at, and the count it was stopped with, under a limit of a million bytes.
///
/// These are the tripwire for the claim that the stopping point is the same on every platform: they were read
/// off on Windows and must hold on Linux, and they move only when the cost table, the bytecode compiler, the
/// step charges or the walk schedule do. A change that moves them is a change of the model, not a flake.
#[test]
fn runaways_stop_at_pinned_steps() {
    let pinned: [(&str, u64, usize); 6] = [
        ("small lists", 69_672, 1_007_872),
        ("strings", 97_283, 1_012_262),
        ("dict of ints", 152_832, 1_000_584),
        ("one growing list", 437_504, 1_000_256),
        ("big integers", 36_526, 1_046_688),
        ("exec", 14_336, 1_004_088),
    ];
    assert_eq!(pinned.len(), RUNAWAYS.len());
    for ((name, code), (pinned_name, steps, used)) in RUNAWAYS.iter().zip(pinned) {
        assert_eq!(*name, pinned_name);
        let (got_steps, error) = trip(code, LIMIT);
        assert_eq!((got_steps, used_bytes(&error)), (steps, used), "{name}");
    }
}

#[test]
fn nothing_else_in_the_process_moves_the_stopping_step() {
    let expected: Vec<_> = RUNAWAYS.iter().map(|(_, code)| trip(code, LIMIT)).collect();

    // Another part of the process allocating and freeing all the while, as a game would.
    let stop = Arc::new(AtomicBool::new(false));
    let noise = {
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            let mut seed = 12345_u64;
            let mut held: Vec<Vec<u8>> = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                seed = seed
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                held.push(vec![0_u8; 20_000 + (seed >> 40) as usize % 150_000]);
                if held.len() > 3 {
                    held.remove(0);
                }
                thread::sleep(Duration::from_micros(100));
            }
        })
    };
    // And the same scripts running in four threads at once.
    let workers: Vec<_> = (0..4)
        .map(|_| {
            thread::spawn(|| {
                RUNAWAYS
                    .iter()
                    .map(|(_, code)| {
                        let (steps, error) = trip(code, LIMIT);
                        (steps, error.message().unwrap_or_default().to_owned())
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let results: Vec<_> = workers.into_iter().map(|worker| worker.join().unwrap()).collect();
    stop.store(true, Ordering::Relaxed);
    noise.join().unwrap();

    for result in results {
        for (((name, _), (steps, error)), (got_steps, got_message)) in RUNAWAYS.iter().zip(&expected).zip(result) {
            assert_eq!(got_steps, *steps, "{name}");
            assert_eq!(&got_message, error.message().unwrap(), "{name}");
        }
    }
}

#[test]
fn garbage_does_not_count() {
    // Each pass builds and drops a list four times the size of the limit's tenth: far over the limit in total,
    // never over it at once.
    let mut repl = new_repl(LIMIT);
    feed(&mut repl, "for i in range(2000):\n    x = [0] * 6000\n").expect("freed lists must not accumulate");
}

#[test]
fn a_session_may_use_most_of_its_limit_and_no_more() {
    // 60,000 slots of 16 bytes is 960,000: under a limit of 1,000,000 with the list object itself.
    let mut repl = new_repl(LIMIT);
    feed(&mut repl, "x = [0] * 60_000").expect("under the limit");
    let error = feed(&mut repl, "y = [0] * 60_000\nfor i in range(1000):\n    pass").unwrap_err();
    assert_eq!(error.exc_type(), ExcType::MemoryError);
}

#[test]
fn emptying_a_container_gives_its_memory_back() {
    // Shrinking in place is invisible until the next measurement, which must then see it, not stay stuck on
    // the old high mark.
    let mut repl = new_repl(LIMIT);
    let code = "x = [0] * 50_000\nfor i in range(40):\n    x.clear()\n    x.extend([0] * 50_000)\n";
    feed(&mut repl, code).expect("a buffer reused forty times fits");
}

#[test]
fn one_huge_operation_is_refused_before_it_allocates() {
    // 160 gigabytes of slots, and ten gigabytes of text: refused up front. If either were attempted the
    // allocation itself would fail and end this process.
    for code in [
        "x = [0] * 10_000_000_000",
        "x = 'a' * 10_000_000_000",
        "x = bytes(10_000_000_000)",
    ] {
        let (_, error) = trip(code, LIMIT);
        assert_eq!(error.exc_type(), ExcType::MemoryError, "{code}");
    }
}

#[test]
fn a_bulk_allocation_is_measured_at_once_even_in_a_big_heap() {
    // Walks of a heap of 200,000 objects are at least 100,000 steps apart. A bulk allocation is measured as
    // soon as it has happened, so this feed ends with the session over its limit and an error, not unseen.
    // (Mutation experiment: removing that rule leaves this test passing, because `bytes(n)` charges a step for
    // every 16 bytes and 20 MB of them carries the walk past its spacing anyway; the rule is a second guard
    // for an allocation that charges fewer steps, and `monty-types`'s own test pins it.)
    let limit = 40_000_000;
    let mut repl = new_repl(limit);
    feed(&mut repl, "keep = [[0] for i in range(200_000)]").expect("the setup fits");
    let error = feed(&mut repl, "y = bytes(20_000_000)").unwrap_err();
    assert_eq!(error.exc_type(), ExcType::MemoryError);
    assert!(used_bytes(&error) > limit);
}

#[test]
fn the_limit_holds_across_feeds() {
    // Each feed keeps one more 160 kilobyte list alive. No feed is near the limit; the session is. Six of them
    // (960,576 bytes, and the list that holds them) fit in a limit of a million, the seventh does not.
    let mut repl = new_repl(LIMIT);
    feed(&mut repl, "keep = []").unwrap();
    let mut stopped_at = None;
    for round in 0..10 {
        if feed(
            &mut repl,
            "keep.append([0] * 10_000)
for i in range(300):
    pass",
        )
        .is_err()
        {
            stopped_at = Some(round);
            break;
        }
    }
    assert_eq!(stopped_at, Some(6));
}

#[test]
fn a_feed_that_ends_over_the_limit_is_stopped_at_its_end() {
    // No loop, so no checkpoint: the measurement at the end of the feed is what catches it.
    let mut repl = new_repl(LIMIT);
    feed(&mut repl, "x = [0] * 60_000").expect("under the limit");
    let error = feed(&mut repl, "y = [0] * 60_000").unwrap_err();
    assert_eq!(error.exc_type(), ExcType::MemoryError);
    assert!(error.message().unwrap().starts_with("model memory limit exceeded: "));
}

#[test]
fn a_restored_session_stops_where_the_original_would_have() {
    // The measuring schedule is part of the saved session, so a session restored from a dump goes on exactly as
    // the one that was dumped: same step, same message.
    let mut original = new_repl(LIMIT);
    feed(
        &mut original,
        "x = []
for i in range(3000):
    x.append([0])
",
    )
    .expect("under the limit");
    let bytes = dump("model.py", None, &[], SessionRef::Idle(&original)).unwrap();
    let Session::Idle(mut restored) = Dump::load(&bytes).unwrap().state else {
        panic!("dumped while idle");
    };
    assert_eq!(restored.model_bytes(), original.model_bytes());

    let runaway = "while True:
    x.append([0])
";
    let from_original = feed(&mut original, runaway).unwrap_err();
    let from_restored = feed(&mut restored, runaway).unwrap_err();
    assert_eq!(from_original.exc_type(), ExcType::MemoryError);
    assert_eq!(from_restored.message(), from_original.message());
    assert_eq!(restored.tracker().steps(), original.tracker().steps());
}

#[test]
fn a_step_limit_that_comes_first_wins() {
    let limits = ResourceLimits::default()
        .max_model_memory(100_000_000)
        .max_feed_steps(5000);
    let mut repl = MontyRepl::new("model.py", ResourceTracker::new(limits), CompileOptions::default());
    let error = feed(&mut repl, "x = []\nwhile True:\n    x.append([0])\n").unwrap_err();
    assert_eq!(error.exc_type(), ExcType::TimeoutError);
}

#[test]
fn restored_limits_can_only_tighten() {
    let mut tracker = ResourceTracker::new(limits(2_000_000));
    tracker.tighten_limits(&limits(1_000_000));
    assert_eq!(tracker.max_model_memory(), Some(1_000_000));
    tracker.tighten_limits(&limits(3_000_000));
    assert_eq!(tracker.max_model_memory(), Some(1_000_000));
    tracker.tighten_limits(&ResourceLimits::default());
    assert_eq!(tracker.max_model_memory(), Some(1_000_000));
    let mut unlimited = ResourceTracker::default();
    unlimited.tighten_limits(&limits(500_000));
    assert_eq!(unlimited.max_model_memory(), Some(500_000));
}

#[test]
fn without_the_limit_nothing_changes() {
    // The step count of a script with no model limit has no measurement in it.
    let steps = |limits: ResourceLimits| {
        let mut repl = MontyRepl::new("model.py", ResourceTracker::new(limits), CompileOptions::default());
        feed(&mut repl, "x = []\nfor i in range(20_000):\n    x.append([i])\n").unwrap();
        repl.tracker().steps()
    };
    let plain = steps(ResourceLimits::default());
    assert_eq!(plain, steps(ResourceLimits::default().max_feed_duration(SAFETY_NET)));
    // A limit far above use measures, and the measuring is charged: more steps, never fewer.
    assert!(steps(limits(1_000_000_000)) >= plain);
}
