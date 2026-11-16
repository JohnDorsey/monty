//! Shared helpers of the step-budget tests: run a script that calls the host function `probe()` and read the
//! step counters at each call.

#![allow(
    dead_code,
    reason = "each test binary that shares these helpers uses only some of them"
)]

use std::time::Duration;

use monty::{MontyRun, RunProgress};
use monty_types::{CompileOptions, ExcType, MontyException, MontyObject, PrintWriter, ResourceLimits, ResourceTracker};

/// A time limit so long that only a broken step counter ever meets it, short enough that a failing test ends.
pub const SAFETY_NET: Duration = Duration::from_secs(20);

/// What the tracker said at one `probe()` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reading {
    pub session: u64,
    pub feed: u64,
    pub turn: u64,
}

/// Runs `code`, which may call the host function `probe()`, and returns every reading, or the error.
pub fn run_probed(code: &str, limits: ResourceLimits) -> Result<Vec<Reading>, MontyException> {
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

pub fn limits() -> ResourceLimits {
    ResourceLimits::default().max_feed_duration(SAFETY_NET)
}

/// Asserts that `error` is the step-limit error of the given scope word ("feed" or "turn").
pub fn assert_step_error(error: &MontyException, scope: &str) {
    assert_eq!(error.exc_type(), ExcType::TimeoutError, "{error}");
    let message = error.message().unwrap_or_default();
    assert!(
        message.starts_with(&format!("{scope} step limit exceeded: ")),
        "expected a {scope} step limit error, got: {message}"
    );
}

/// The steps `operation` charges: the count between a `probe()` before and one after it, so the setup is
/// excluded. The operation statement's own few instructions are included.
pub fn charged(setup: &str, operation: &str) -> u64 {
    let code = format!(
        "{setup}
probe()
{operation}
probe()"
    );
    let readings = run_probed(&code, limits()).unwrap_or_else(|error| panic!("{operation}: {error}"));
    readings[1].session - readings[0].session
}
