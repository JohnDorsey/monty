//! `__len__` on user classes under the fork's limits: its work counts against the step budget, and it cannot
//! recurse past the interpreter's depth guard. The Python behaviour itself is in `test_cases/class__len.py`.

mod step_support;

use monty::MontyRun;
use monty_types::CompileOptions;
use step_support::{assert_step_error, charged, limits, run_probed};

/// Runs `code` with no limits and returns the string it evaluates to.
fn evaluate(code: &str) -> String {
    let result = MontyRun::new(code.to_owned(), "len_dunder.py", vec![], CompileOptions::default())
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

    def __len__(self):
        for _ in range(self.turns):
            pass
        return 1
";

#[test]
fn the_work_inside_len_is_charged_from_truth_tests_in_native_loops() {
    let idle = charged(&format!("{BUSY}\nitems = [Busy(0)] * 100"), "all(items)");
    let busy = charged(&format!("{BUSY}\nitems = [Busy(100)] * 100"), "all(items)");
    assert!(
        busy >= idle + 100 * 100,
        "100 calls of a 100-turn __len__ charged {busy} steps against {idle} for an empty one"
    );
}

#[test]
fn the_work_inside_len_is_charged_when_len_is_mapped() {
    let idle = charged(&format!("{BUSY}\nitems = [Busy(0)] * 100"), "list(map(len, items))");
    let busy = charged(&format!("{BUSY}\nitems = [Busy(100)] * 100"), "list(map(len, items))");
    assert!(
        busy >= idle + 100 * 100,
        "100 calls of a 100-turn __len__ charged {busy} steps against {idle} for an empty one"
    );
}

#[test]
fn the_charge_is_the_same_on_every_run() {
    let setup = format!("{BUSY}\nitems = [Busy(10)] * 50");
    assert_eq!(
        charged(&setup, "list(map(len, items))"),
        charged(&setup, "list(map(len, items))")
    );
}

#[test]
fn the_step_limit_inside_len_cannot_be_caught() {
    let code = format!("{BUSY}\ntry:\n    len(Busy(10**9))\nexcept BaseException:\n    pass\nprobe()");
    let error = run_probed(&code, limits().max_feed_steps(20_000)).unwrap_err();
    assert_step_error(&error, "feed");
}

#[test]
fn a_len_that_asks_for_its_own_length_raises_a_recursion_error_instead_of_overflowing_the_stack() {
    let code = "
class Loop:
    def __len__(self):
        return len(self)

outcomes = []
for ask in (len, bool):
    try:
        ask(Loop())
        outcomes.append('returned')
    except RecursionError:
        outcomes.append('recursion error')
', '.join(outcomes)
";
    assert_eq!(evaluate(code), "recursion error, recursion error");
}

#[test]
fn a_len_that_calls_the_host_fails_instead_of_suspending() {
    let code = "
class Asks:
    def __len__(self):
        probe()
        return 1

len(Asks())
";
    let error = run_probed(code, limits()).unwrap_err();
    let message = error.message().unwrap_or_default();
    assert!(message.contains("probe"), "{error}");
}
