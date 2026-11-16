//! The deterministic step budget: a script is cut off after the same amount of work on every machine.
//!
//! A step is one bytecode instruction, or one item of native work (see `ResourceTracker::check_time_every`).
//! These tests read the count at a host call (`probe()`), so they work without a limit set, and they use a
//! time limit only as a safety net so that a broken counter fails an assertion instead of hanging.

use std::{
    hint,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use monty::{MontyRun, RunProgress};
use monty_types::{CompileOptions, ExcType, MontyException, MontyObject, PrintWriter, ResourceLimits, ResourceTracker};

/// A time limit so long that only a broken step counter ever meets it, short enough that a failing test ends.
const SAFETY_NET: Duration = Duration::from_secs(20);

/// What the tracker said at one `probe()` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Reading {
    session: u64,
    feed: u64,
    turn: u64,
}

/// Runs `code`, which may call the host function `probe()`, and returns every reading, or the error.
fn run_probed(code: &str, limits: ResourceLimits) -> Result<Vec<Reading>, MontyException> {
    let runner = MontyRun::new(
        code.to_owned(),
        "step_budget.py",
        vec!["probe".to_owned()],
        CompileOptions::default(),
    )?;
    let tracker = ResourceTracker::new(limits);
    let mut progress = runner.start(
        vec![MontyObject::function("probe".to_owned(), None)],
        tracker,
        PrintWriter::Disabled,
    )?;
    let mut readings = Vec::new();
    loop {
        match progress {
            RunProgress::FunctionCall(mut call) => {
                let tracker = call.tracker_mut();
                readings.push(Reading {
                    session: tracker.steps(),
                    feed: tracker.feed_steps(),
                    turn: tracker.turn_steps(),
                });
                progress = call.resume(MontyObject::none(), PrintWriter::Disabled)?;
            }
            RunProgress::Complete(_) => return Ok(readings),
            other => panic!("unexpected progress: {other:?}"),
        }
    }
}

fn limits() -> ResourceLimits {
    ResourceLimits::default().max_feed_duration(SAFETY_NET)
}

/// Asserts that `error` is the step-limit error of the given scope word ("feed" or "turn").
fn assert_step_error(error: &MontyException, scope: &str) {
    assert_eq!(error.exc_type(), ExcType::TimeoutError, "{error}");
    let message = error.message().unwrap_or_default();
    assert!(
        message.starts_with(&format!("{scope} step limit exceeded: ")),
        "expected a {scope} step limit error, got: {message}"
    );
}

/// A script that mixes the interpreter's main kinds of work: loops, calls, containers, strings and a callback.
const MIXED: &str = "
def square(x):
    return x * x

total = 0
for i in range(300):
    total += square(i) % 7
words = [str(i) for i in range(200)]
joined = ','.join(words)
pairs = sorted(range(500), key=lambda n: -n)
table = {i: i + 1 for i in range(100)}
probe()
";

#[test]
fn counting_works_without_any_limit_and_every_kind_of_work_counts() {
    let readings = run_probed(MIXED, limits()).unwrap();
    assert_eq!(readings.len(), 1);
    let steps = readings[0].session;
    // 300 loop iterations of several instructions, 200 and 100 comprehension iterations, and 500 callbacks:
    // at least 4,000 is a floor any sensible counting rule clears.
    assert!(
        steps >= 4_000,
        "only {steps} steps counted for a script with thousands of instructions"
    );
    assert_eq!(readings[0].feed, steps, "one feed: its count is the session's");
    assert_eq!(readings[0].turn, steps, "one turn: its count is the session's");
}

#[test]
fn the_count_is_the_same_on_every_run_even_with_other_threads_competing() {
    let first = run_probed(MIXED, limits()).unwrap();
    let second = run_probed(MIXED, limits()).unwrap();
    assert_eq!(first, second);

    let cores = thread::available_parallelism().map_or(2, usize::from);
    let stop = Arc::new(AtomicBool::new(false));
    let spinners: Vec<_> = (0..cores * 2)
        .map(|_| {
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    hint::spin_loop();
                }
            })
        })
        .collect();
    let busy = run_probed(MIXED, limits()).unwrap();
    stop.store(true, Ordering::Relaxed);
    for spinner in spinners {
        spinner.join().unwrap();
    }
    assert_eq!(first, busy, "machine load changed the step count");
}

/// Scripts whose step counts are pinned. They were recorded once from the first working build and then
/// checked on Linux and in debug and release builds; a change means the counting rule or the compiler changed,
/// which changes every script's budget (see the step-budget plan: pin the Monty release line).
const GOLDEN_SCRIPTS: [(&str, &str); 3] = [
    ("counting loop", "x = 0\nfor i in range(1000):\n    x += i\nprobe()"),
    (
        "function calls",
        "def f(a):\n    return a + 1\nx = 0\nfor i in range(500):\n    x = f(x)\nprobe()",
    ),
    ("comprehension", "x = sum([i * i for i in range(2000)])\nprobe()"),
];

/// Pinned counts for [`GOLDEN_SCRIPTS`], in order.
const GOLDEN_STEPS: [u64; 3] = [7008, 5510, 15993];

#[test]
fn golden_step_counts() {
    let got: Vec<u64> = GOLDEN_SCRIPTS
        .iter()
        .map(|(_, code)| run_probed(code, limits()).unwrap()[0].session)
        .collect();
    assert_eq!(got, GOLDEN_STEPS, "actual counts: {got:?}");
}

#[test]
fn a_turn_budget_kills_with_an_uncatchable_error() {
    // `except BaseException` and `finally` run during ordinary errors; a resource error must bypass both.
    let code = "
try:
    while True:
        pass
except BaseException:
    probe()
finally:
    probe()
";
    let error = run_probed(code, limits().max_turn_steps(10_000)).unwrap_err();
    assert_step_error(&error, "turn");
}

#[test]
fn a_feed_budget_kills_with_an_uncatchable_error() {
    let error = run_probed("while True:\n    pass", limits().max_feed_steps(10_000)).unwrap_err();
    assert_step_error(&error, "feed");
}

#[test]
fn the_stopping_point_is_exact_to_a_checkpoint_and_the_same_every_time() {
    let budget = 50_000;
    let first = run_probed("while True:\n    pass", limits().max_turn_steps(budget)).unwrap_err();
    let second = run_probed("while True:\n    pass", limits().max_turn_steps(budget)).unwrap_err();
    assert_eq!(
        first.message(),
        second.message(),
        "the same budget stopped at different counts"
    );
    // "N steps > 50000 steps": N is past the budget by less than one checkpoint interval.
    let message = first.message().unwrap();
    let used: u64 = message
        .split("exceeded: ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .and_then(|number| number.parse().ok())
        .unwrap_or_else(|| panic!("no count in: {message}"));
    assert!(
        used > budget && used <= budget + 512,
        "stopped at {used} for a budget of {budget}"
    );
}

#[test]
fn a_new_turn_resets_the_turn_count_but_the_feed_count_carries_on() {
    let phase = "for i in range(400):\n    pass\n";
    let code = format!("{phase}probe()\n{phase}probe()\n{phase}probe()");
    let readings = run_probed(&code, limits()).unwrap();
    assert_eq!(readings.len(), 3);
    let per_phase = readings[0].turn;
    assert!(per_phase >= 400, "a phase of 400 iterations counted {per_phase}");
    // After the host call returns, a new turn starts: the turn count restarts, the feed count does not.
    assert!(
        readings[1].turn < readings[0].turn * 2,
        "the turn count did not reset: {readings:?}"
    );
    assert!(
        readings[1].feed > readings[0].feed && readings[2].feed > readings[1].feed,
        "{readings:?}"
    );
    assert_eq!(readings[2].session, readings[2].feed, "{readings:?}");

    // A turn budget that fits one phase lets all three through; a feed budget that fits one phase does not.
    let budget = per_phase + per_phase / 2;
    run_probed(&code, limits().max_turn_steps(budget)).unwrap();
    let error = run_probed(&code, limits().max_feed_steps(budget)).unwrap_err();
    assert_step_error(&error, "feed");
}

#[test]
fn native_loops_are_charged_so_sum_over_a_huge_range_stops() {
    let budget = 100_000;
    let error = run_probed("sum(range(10**12))", limits().max_turn_steps(budget)).unwrap_err();
    assert_step_error(&error, "turn");
    let again = run_probed("sum(range(10**12))", limits().max_turn_steps(budget)).unwrap_err();
    assert_eq!(
        error.message(),
        again.message(),
        "native work stopped at different counts"
    );
}

#[test]
fn short_callbacks_from_native_code_cannot_dodge_the_budget() {
    // Each callback runs a handful of instructions in a nested interpreter loop, far fewer than one checkpoint
    // interval, so a count taken only at checkpoints would never see them.
    let budget = 1_000_000;
    for code in [
        "sorted(range(10**7), key=lambda x: x)",
        "list(map(lambda x: x, range(10**12)))",
        "[x for x in map(lambda x: x + 1, range(10**12))]",
    ] {
        let error = run_probed(code, limits().max_turn_steps(budget)).unwrap_err();
        assert_step_error(&error, "turn");
    }
}

#[test]
fn counting_short_callbacks_without_a_limit() {
    let readings = run_probed("x = sorted(range(20000), key=lambda n: n)\nprobe()", limits()).unwrap();
    // 20,000 callbacks of at least two instructions each.
    assert!(readings[0].session >= 40_000, "{readings:?}");
}

#[test]
fn callbacks_are_counted_exactly_and_not_only_when_a_checkpoint_happens_to_land_in_them() {
    // Each key call runs at least three instructions in a nested interpreter loop that ends long before a
    // checkpoint. Counting only at checkpoints saw about a quarter of this work (19,968 steps measured with the
    // end-of-loop charge removed), so a script could hide most of its instructions inside callbacks.
    let readings = run_probed(
        "max(range(20000), key=lambda x: -x)
probe()",
        limits(),
    )
    .unwrap();
    assert!(readings[0].session >= 60_000, "{readings:?}");
    let source = "class C:
    def __eq__(self, other):
        return False
xs = [C() for i in range(200)]
for i in range(100):
    C() in xs
probe()";
    let readings = run_probed(source, limits()).unwrap();
    // 100 searches of 200 comparisons, each a call of at least three instructions.
    assert!(readings[0].session >= 60_000, "{readings:?}");
}

#[test]
fn a_script_that_fits_its_budget_is_untouched() {
    let readings = run_probed(MIXED, limits()).unwrap();
    let needed = readings[0].session;
    // The final check happens after the last instruction, so a little room covers the epilogue.
    run_probed(
        MIXED,
        limits().max_turn_steps(needed + 1_000).max_feed_steps(needed + 1_000),
    )
    .unwrap();
    let error = run_probed(MIXED, limits().max_turn_steps(needed / 2)).unwrap_err();
    assert_step_error(&error, "turn");
}
