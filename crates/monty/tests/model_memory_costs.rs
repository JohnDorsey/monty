//! The cost table of the deterministic memory model (see `FORK.md`, "Model memory").
//!
//! `MontyRepl::model_bytes` is what `max_model_memory` limits: a number that depends only on what the session
//! holds, never on the allocator, the platform or the rest of the process. These tests pin what each kind of
//! object costs, so a change to the table is a visible, deliberate edit here.

use monty::{Dump, MontyRepl, Session, SessionRef, dump};
use monty_types::{CompileOptions, PrintWriter, ResourceTracker};

/// What a heap object costs before its contents: the entry (reference count, reader count, payload, collector colour).
const OBJECT: usize = 96;
/// One value slot in a container, or one global or stack slot.
const VALUE: usize = 16;
/// One dict entry (key, value, hash and index word), also an instance attribute or a namespace entry.
const DICT_ENTRY: usize = 72;
/// One set element (value, hash and index word).
const SET_ENTRY: usize = 40;
/// The separate allocation behind a boxed payload (an instance, a named tuple, a partial, a match).
const BOXED: usize = 112;
/// One 64-bit limb of an integer too large for an `i64`.
const LIMB: usize = 8;

fn new_repl() -> MontyRepl {
    MontyRepl::new("model.py", ResourceTracker::default(), CompileOptions::default())
}

fn feed(repl: &mut MontyRepl, code: &str) {
    repl.feed_run(code, vec![], PrintWriter::Disabled)
        .unwrap_or_else(|error| panic!("{code}: {error}"));
}

/// What binding `expression` to the existing global `x` adds to the session, after `setup` has run.
///
/// `x` is bound to an integer first, so its global slot exists on both sides of the difference and the
/// answer is the cost of the object (and everything it owns) alone.
fn cost_of(setup: &str, expression: &str) -> usize {
    let mut repl = new_repl();
    feed(&mut repl, &format!("{setup}\nx = 0"));
    let before = repl.model_bytes();
    feed(&mut repl, &format!("x = {expression}"));
    repl.model_bytes() - before
}

#[test]
fn text_and_bytes_cost_their_length() {
    assert_eq!(cost_of("", "'a' * 1000"), OBJECT + 1000);
    assert_eq!(cost_of("", "b'a' * 500"), OBJECT + 500);
    // Characters are counted as the bytes they take, as the allocator would.
    assert_eq!(cost_of("", "'é' * 100"), OBJECT + 200);
}

#[test]
fn sequences_cost_one_slot_per_item() {
    assert_eq!(cost_of("", "[0] * 10"), OBJECT + 10 * VALUE);
    assert_eq!(cost_of("", "(0,) * 10"), OBJECT + 10 * VALUE);
    assert_eq!(cost_of("", "list(range(100))"), OBJECT + 100 * VALUE);
    assert_eq!(
        cost_of("import collections", "collections.deque(range(10))"),
        OBJECT + 10 * VALUE
    );
}

#[test]
fn a_sequence_costs_its_length_not_its_capacity() {
    // Pushing one at a time leaves spare capacity in the real buffer (four slots, then eight); the model must
    // not see it, so a grown list costs what one built at its final length costs.
    let mut grown = new_repl();
    feed(&mut grown, "y = []\nfor i in range(5):\n    y.append(0)");
    let mut built = new_repl();
    feed(&mut built, "y = [0] * 5\ni = 4");
    assert_eq!(grown.model_bytes(), built.model_bytes());
    assert_eq!(cost_of("", "[0] * 5"), OBJECT + 5 * VALUE);
}

#[test]
fn containers_cost_what_they_hold() {
    // The outer list, the two inner lists with one slot each.
    assert_eq!(cost_of("", "[[0], [0]]"), (OBJECT + 2 * VALUE) + 2 * (OBJECT + VALUE));
    assert_eq!(cost_of("", "{i: i for i in range(10)}"), OBJECT + 10 * DICT_ENTRY);
    assert_eq!(cost_of("", "set(range(10))"), OBJECT + 10 * SET_ENTRY);
    assert_eq!(cost_of("", "frozenset(range(10))"), OBJECT + 10 * SET_ENTRY);
}

#[test]
fn a_shared_object_is_counted_once() {
    // `y` is already in the session; `[y, y]` adds only the outer list.
    assert_eq!(cost_of("y = [0] * 10", "[y, y]"), OBJECT + 2 * VALUE);
}

#[test]
fn big_integers_cost_their_limbs() {
    // 2**200 has 201 bits: four 64-bit limbs.
    assert_eq!(cost_of("", "2 ** 200"), OBJECT + 4 * LIMB);
    assert_eq!(cost_of("", "2 ** 64"), OBJECT + 2 * LIMB);
    assert_eq!(cost_of("", "2 ** 63"), OBJECT + LIMB);
}

#[test]
fn instances_cost_their_attributes() {
    let class = "class C:\n    def __init__(self):\n        self.a = 1\n        self.b = 2\n";
    assert_eq!(cost_of(class, "C()"), OBJECT + BOXED + 2 * DICT_ENTRY);
}

#[test]
fn named_tuples_and_decimals_cost_what_they_hold() {
    let named = "import collections\nP = collections.namedtuple('P', 'a b')";
    assert_eq!(cost_of(named, "P(1, 2)"), OBJECT + BOXED + 2 * VALUE);
    // Forty digits fill three words of nineteen digits. The first `Decimal` of a session also creates the
    // session's context object, so the setup makes one first and the price is the number alone.
    assert_eq!(
        cost_of(
            "import decimal
decimal.Decimal(1)",
            "decimal.Decimal('1' * 40)"
        ),
        OBJECT + 3 * LIMB
    );
}

#[test]
fn closures_partials_and_regular_expressions_have_their_price() {
    let closure = "def make(a, b):\n    def inner(c=1):\n        return a + b + c\n    return inner";
    // Two captured cells (each its own object holding one value), and one default.
    assert_eq!(cost_of(closure, "make(1, 2)"), (OBJECT + 2 * 8 + VALUE) + 2 * OBJECT);
    assert_eq!(
        cost_of(
            "import functools\ndef f(*a, **k):\n    pass",
            "functools.partial(f, 1, 2, k=3)"
        ),
        OBJECT + BOXED + 4 * VALUE
    );
    // A compiled program costs a base plus 64 bytes for each character of its pattern.
    assert_eq!(
        cost_of("import re", "re.compile('abc')"),
        OBJECT + BOXED + 2048 + 3 * 64
    );
    // The matched text and one record per group; the subject is shared, so it is not counted again.
    assert_eq!(
        cost_of("import re", "re.match('a(b)', 'ab')"),
        OBJECT + BOXED + 2 + 1 + VALUE
    );
}

#[test]
fn buffering_adaptors_and_caches_cost_what_they_have_buffered() {
    let mut repl = new_repl();
    feed(
        &mut repl,
        "import itertools, functools\nc = itertools.cycle(range(100))\n",
    );
    let fresh = repl.model_bytes();
    feed(&mut repl, "for i in range(100):\n    next(c)");
    // `cycle` remembers every item it has produced, so that it can replay them. (`i` is a new global.)
    assert_eq!(repl.model_bytes() - fresh, VALUE + 100 * VALUE);

    feed(
        &mut repl,
        "@functools.lru_cache(maxsize=None)\ndef square(n):\n    return n * n\n",
    );
    let empty_cache = repl.model_bytes();
    feed(&mut repl, "for n in range(10):\n    square(n)");
    // Ten remembered calls, and the loop's new global `n`.
    assert_eq!(repl.model_bytes() - empty_cache, 10 * 56 + VALUE);
}

#[test]
fn a_random_generator_costs_its_state() {
    assert_eq!(cost_of("import random", "random.Random(1)"), OBJECT + 2560);
}

#[test]
fn small_values_cost_nothing_on_the_heap() {
    for expression in ["1", "1.5", "True", "None", "'abc'", "()"] {
        assert_eq!(cost_of("", expression), 0, "{expression}");
    }
}

#[test]
fn a_global_costs_one_slot() {
    let mut repl = new_repl();
    feed(&mut repl, "a = 0");
    let before = repl.model_bytes();
    feed(&mut repl, "b = 0");
    assert_eq!(repl.model_bytes() - before, VALUE);
}

#[test]
fn freed_objects_stop_counting() {
    let mut repl = new_repl();
    feed(&mut repl, "x = 0");
    let before = repl.model_bytes();
    feed(&mut repl, "x = [0] * 1000");
    assert_eq!(repl.model_bytes() - before, OBJECT + 1000 * VALUE);
    feed(&mut repl, "x = 0");
    assert_eq!(repl.model_bytes(), before);
}

#[test]
fn shrinking_a_container_in_place_is_seen() {
    let mut repl = new_repl();
    feed(&mut repl, "x = [0] * 1000");
    let full = repl.model_bytes();
    feed(&mut repl, "x.clear()");
    assert_eq!(full - repl.model_bytes(), 1000 * VALUE);
}

#[test]
fn an_unreachable_cycle_counts_until_it_is_collected() {
    let mut repl = new_repl();
    feed(&mut repl, "x = 0");
    let before = repl.model_bytes();
    feed(&mut repl, "a = []\na.append(a)");
    // The list holds one slot (itself) and the global `a` holds the list.
    assert_eq!(repl.model_bytes() - before, VALUE + OBJECT + VALUE);
}

#[test]
fn the_count_is_the_same_after_a_dump_and_restore() {
    let mut repl = new_repl();
    feed(
        &mut repl,
        "x = [str(i) * 3 for i in range(50)]\nd = {i: [i] for i in range(20)}\nbig = 3 ** 500",
    );
    let before = repl.model_bytes();
    let bytes = dump("model.py", None, &[], SessionRef::Idle(&repl)).unwrap();
    let Session::Idle(restored) = Dump::load(&bytes).unwrap().state else {
        panic!("dumped while idle");
    };
    assert_eq!(restored.model_bytes(), before);
}
