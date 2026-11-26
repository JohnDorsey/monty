//! `__bool__` on user classes under the fork's limits: its work counts against the step budget, and it cannot
//! recurse past the interpreter's depth guard. The Python behaviour itself is in `test_cases/class__bool.py`.

mod step_support;

use monty::MontyRun;
use monty_types::CompileOptions;
use step_support::{assert_step_error, charged, limits, run_probed};

/// Runs `code` with no limits and returns the string it evaluates to.
fn evaluate(code: &str) -> String {
    let result = MontyRun::new(code.to_owned(), "bool_dunder.py", vec![], CompileOptions::default())
        .expect("the script compiles")
        .run_no_limits(vec![])
        .expect("the script runs");
    result
        .as_ref()
        .as_str()
        .expect("the script returns a string")
        .to_owned()
}

const BUSY: &str = "
class Busy:
    def __init__(self, turns):
        self.turns = turns

    def __bool__(self):
        for _ in range(self.turns):
            pass
        return True
";

#[test]
fn the_work_inside_bool_is_charged_from_native_loops_too() {
    let idle = charged(&format!("{BUSY}\nitems = [Busy(0)] * 100"), "all(items)");
    let busy = charged(&format!("{BUSY}\nitems = [Busy(100)] * 100"), "all(items)");
    assert!(
        busy >= idle + 100 * 100,
        "100 calls of a 100-turn __bool__ charged {busy} steps against {idle} for an empty one"
    );
}

#[test]
fn the_charge_is_the_same_on_every_run() {
    let setup = format!("{BUSY}\nitems = [Busy(10)] * 50");
    assert_eq!(charged(&setup, "all(items)"), charged(&setup, "all(items)"));
}

#[test]
fn a_loop_on_an_always_true_instance_meets_the_step_limit() {
    let code = format!("{BUSY}\nforever = Busy(0)\nwhile forever:\n    pass");
    let error = run_probed(&code, limits().max_feed_steps(20_000)).unwrap_err();
    assert_step_error(&error, "feed");
}

#[test]
fn the_step_limit_inside_bool_cannot_be_caught() {
    let code = format!("{BUSY}\ntry:\n    bool(Busy(10**9))\nexcept BaseException:\n    pass\nprobe()");
    let error = run_probed(&code, limits().max_feed_steps(20_000)).unwrap_err();
    assert_step_error(&error, "feed");
}

#[test]
fn a_bool_that_tests_itself_raises_a_recursion_error_instead_of_overflowing_the_stack() {
    let code = "
class Loop:
    def __bool__(self):
        return not self

try:
    bool(Loop())
    outcome = 'returned'
except RecursionError:
    outcome = 'recursion error'
outcome
";
    assert_eq!(evaluate(code), "recursion error");
}

#[test]
fn a_bool_that_calls_the_host_fails_instead_of_suspending() {
    let code = "
class Asks:
    def __bool__(self):
        probe()
        return True

bool(Asks())
";
    let error = run_probed(code, limits()).unwrap_err();
    let message = error.message().unwrap_or_default();
    assert!(message.contains("probe"), "{error}");
}
