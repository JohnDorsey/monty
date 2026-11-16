//! Native operations that do a lot of work in one call charge steps in proportion to the size of what they
//! touch, so a loop of them cannot hide inside a step budget.
//!
//! Rates (see `monty::native_work`): one step per 16 bytes of text, one step per item touched by reference
//! counting or hashing, one step per 16 items merely moved. The assertions are lower bounds with a small
//! slack for the statement's own instructions, because the point is that the work is charged, not the last digit.

mod step_support;

use step_support::{assert_step_error, charged, limits, run_probed};

/// The instructions of a statement and a loop header are far below this; charges are compared at this slack.
const SLACK: u64 = 40;

/// The operation charges at least `expected` steps (a floor, since some operations also poll the tracker per
/// item inside) and not absurdly more: at most twice that plus the slack.
fn assert_charges(setup: &str, operation: &str, expected: u64) {
    let steps = charged(setup, operation);
    assert!(
        steps >= expected && steps <= expected * 2 + SLACK,
        "`{operation}` charged {steps} steps, expected {expected} (+{SLACK}, at most twice)"
    );
}

/// The operation charges at least `expected` steps; it also polls the tracker per item in its own loop, so
/// there is no useful ceiling.
fn assert_at_least(setup: &str, operation: &str, expected: u64) {
    let steps = charged(setup, operation);
    assert!(
        steps >= expected,
        "`{operation}` charged {steps} steps, expected at least {expected}"
    );
}

const TEXT: &str = "s = 'ab' * 50000\nt = 'ab' * 50000"; // 100,000 bytes
const BYTES: &str = "s = b'ab' * 50000\nt = b'ab' * 50000";

#[test]
fn text_methods_charge_one_step_per_sixteen_bytes() {
    for operation in [
        "s.upper()",
        "s.lower()",
        "s.swapcase()",
        "s.title()",
        "s.count('ba')",
        "s.replace('a', 'c')",
        "s.split('x')",
        "s.strip()",
        "s.encode()",
        "s.partition('x')",
        "s.isalpha()",
    ] {
        assert_charges(TEXT, operation, 100_000 / 16);
    }
    for operation in [
        "s.upper()",
        "s.lower()",
        "s.count(b'ba')",
        "s.split(b'x')",
        "s.strip()",
        "s.decode()",
        "s.hex()",
    ] {
        assert_charges(BYTES, operation, 100_000 / 16);
    }
    // The bytes `replace` also polls once per match, which is the larger part of what it charges.
    assert_at_least(BYTES, "s.replace(b'a', b'c')", 100_000 / 16);
}

#[test]
fn searches_charge_the_text_they_scan_not_the_whole_string() {
    // A miss scans everything; a hit at the start scans nothing; a hit later scans up to it.
    assert_charges(TEXT, "s.find('x')", 100_000 / 16);
    assert_charges(TEXT, "s.rfind('x')", 100_000 / 16);
    assert_charges(TEXT, "'x' in s", 100_000 / 16);
    assert_charges(TEXT, "s.find('a')", 0);
    assert_charges(TEXT, "s.index('ab')", 0);
    // `in` cannot see where a hit lies, so it charges the whole haystack hit or miss: never too little.
    assert_charges(TEXT, "'ab' in s", 100_000 / 16);
    assert_charges("s = 'a' * 80000 + 'b'", "s.find('b')", 80_000 / 16);
    // A search from a start offset pays only for the stretch after it.
    assert_charges("s = 'a' * 80000 + 'b'", "s.find('b', 79000)", 1_000 / 16);
}

#[test]
fn text_operators_charge_their_operands_or_result() {
    assert_charges(TEXT, "s == t", 100_000 / 16);
    assert_charges(TEXT, "s < t", 100_000 / 16);
    assert_charges(TEXT, "s + t", 200_000 / 16);
    assert_charges(TEXT, "s * 3", 300_000 / 16);
    assert_charges(TEXT, "hash(s)", 100_000 / 16);
    assert_charges(TEXT, "repr(s)", 100_000 / 16);
    assert_charges(TEXT, "s[1:]", 100_000 / 16);
    // A stepped slice also polls once per character.
    assert_at_least(TEXT, "s[::-1]", 100_000 / 16);
    assert_charges(BYTES, "s == t", 100_000 / 16);
    assert_charges(BYTES, "s + t", 200_000 / 16);
    assert_charges(BYTES, "s * 3", 300_000 / 16);
    // Slicing bytes also polls once per byte.
    assert_at_least(BYTES, "s[1:]", 100_000 / 16);
}

#[test]
fn padding_and_repetition_charge_the_result() {
    assert_charges("s = 'x'", "s.center(400000)", 400_000 / 16);
    assert_charges("s = 'x'", "s.ljust(400000)", 400_000 / 16);
    assert_charges("s = 'x'", "s.zfill(400000)", 400_000 / 16);
    assert_charges("s = 'x'", "s * 400000", 400_000 / 16);
    assert_charges("s = b'x'", "s * 400000", 400_000 / 16);
}

const ITEMS: &str = "s = list(range(100000))\nt = list(range(100000))";

#[test]
fn item_copies_charge_one_step_per_item() {
    assert_charges(ITEMS, "s.copy()", 100_000);
    assert_charges(ITEMS, "s + t", 200_000);
    assert_charges(ITEMS, "s * 2", 200_000);
    let tuples = "s = tuple(range(100000))\nt = tuple(range(100000))";
    assert_charges(tuples, "s + t", 200_000);
    assert_charges(tuples, "s * 2", 200_000);
    let dicts = "s = {i: i for i in range(100000)}\nt = {i: i for i in range(100000)}";
    // Table copies poll the tracker once per entry in blocks of 64, so they come to a few steps under the
    // length.
    assert_charges(dicts, "s.copy()", 99_000);
    assert_charges(dicts, "dict(s)", 99_000);
    assert_charges(dicts, "{**s}", 100_000);
    // A union of two dicts is charged in each helper it passes through: the floor is both operands.
    assert_charges(dicts, "s | t", 200_000);
    // `update` charges the pairs it snapshots, counted a few short of the dict's length.
    assert_charges(dicts, "s.update(t)", 99_000);
    let sets = "s = set(range(100000))\nt = set(range(100000))";
    assert_charges(sets, "s.copy()", 99_000);
    assert_charges(sets, "set(s)", 99_000);
    assert_charges(sets, "frozenset(s)", 99_000);
    assert_charges(sets, "s.union(t)", 199_000);
    // `isdisjoint` copies both operands' tables.
    assert_charges(sets, "s.isdisjoint(t)", 199_000);
    assert_charges(sets, "s.update(t)", 99_000);
}

#[test]
fn item_moves_charge_one_step_per_sixteen_items() {
    assert_charges(ITEMS, "s.insert(0, 1)", 100_000 / 16);
    assert_charges(ITEMS, "s.pop(0)", 100_000 / 16);
    assert_charges(ITEMS, "s.reverse()", 100_000 / 16);
    assert_charges(ITEMS, "s.sort()", 100_000);
    assert_charges(ITEMS, "s.clear()", 100_000 / 16);
    assert_charges("s = {i: i for i in range(100000)}", "s.clear()", 100_000 / 16);
    assert_charges(
        "import collections\ns = collections.deque(range(100000))",
        "s.rotate(5)",
        100_000 / 16,
    );
}

#[test]
fn constant_time_operations_stay_free_on_large_receivers() {
    for (setup, operation) in [
        (ITEMS, "s.append(1)"),
        (ITEMS, "s.pop()"),
        (ITEMS, "s[5]"),
        (ITEMS, "len(s)"),
        (ITEMS, "s.insert(len(s), 1)"),
        (TEXT, "len(s)"),
        (TEXT, "s[5]"),
        (TEXT, "s.startswith('ab')"),
        (TEXT, "s.endswith('ab')"),
        ("s = {i: i for i in range(100000)}", "s.get(5)"),
        ("s = {i: i for i in range(100000)}", "s.setdefault(5, 1)"),
        ("s = {i: i for i in range(100000)}", "s.keys()"),
        ("s = {i: i for i in range(100000)}", "5 in s"),
        ("s = set(range(100000))", "s.add(5)"),
        ("s = set(range(100000))", "s.discard(5)"),
        ("s = set(range(100000))", "5 in s"),
    ] {
        let steps = charged(setup, operation);
        assert!(
            steps <= SLACK,
            "`{operation}` should cost next to nothing, charged {steps}"
        );
    }
}

#[test]
fn small_operands_charge_nothing_extra() {
    // Below the rate (16 bytes, one item per step, 16 items moved) the division gives zero: ordinary scripts
    // keep the pre-existing counts exactly.
    for operation in ["s.upper()", "s + s", "s == s", "s.find('z')", "s[1:]", "hash(s)"] {
        let steps = charged("s = 'hello'", operation);
        assert!(steps <= SLACK, "`{operation}` on a short string charged {steps}");
    }
}

#[test]
fn a_loop_of_large_native_calls_hits_the_budget_instead_of_running_for_hours() {
    // 200 KB of text: a loop of a million such calls would take hours, and each iteration counted only a few
    // instructions before the charges existed.
    for body in [
        "s.upper()",
        "s.count('ba')",
        "s.replace('a', 'c')",
        "s == t",
        "s + t",
        "s.find('x')",
    ] {
        let code = format!("s = 'ab' * 100000\nt = 'ab' * 100000\nfor i in range(1000000):\n    x = {body}");
        let error = run_probed(&code, limits().max_turn_steps(5_000_000)).unwrap_err();
        assert_step_error(&error, "turn");
    }
    for body in ["s.copy()", "s + s", "s.insert(0, 1)", "s.pop(0)", "s.sort()"] {
        let code = format!("s = list(range(100000))\nfor i in range(1000000):\n    x = {body}");
        let error = run_probed(&code, limits().max_turn_steps(5_000_000)).unwrap_err();
        assert_step_error(&error, "turn");
    }
}

#[test]
fn stdlib_functions_that_scan_text_charge_the_size_of_their_text_arguments() {
    let bytes = "import hashlib, base64, binascii\ns = b'ab' * 50000";
    for operation in [
        "hashlib.sha256(s).hexdigest()",
        "hashlib.md5(s).hexdigest()",
        "base64.b64encode(s)",
        "binascii.hexlify(s)",
    ] {
        assert_at_least(bytes, operation, 100_000 / 16);
    }
    let text = "import re, unicodedata, json, fnmatch\ns = 'ab' * 50000";
    for operation in [
        "unicodedata.normalize('NFD', s)",
        "re.search('a*c', s)",
        "re.sub('c', 'd', s)",
        "re.findall('c', s)",
        "re.split('c', s)",
        "fnmatch.fnmatch(s, '*c')",
    ] {
        assert_at_least(text, operation, 100_000 / 16);
    }
    assert_at_least(
        "import json\ns = '\"' + 'ab' * 50000 + '\"'",
        "json.loads(s)",
        100_000 / 16,
    );
    // A short argument costs nothing extra.
    let steps = charged("import hashlib", "hashlib.sha256(b'abc').hexdigest()");
    assert!(steps <= SLACK, "{steps}");
}
