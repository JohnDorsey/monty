//! Deterministic step charges for native operations whose cost grows with the size of their operands.
//!
//! The interpreter counts one step per bytecode instruction and one per item of a native loop that polls the
//! tracker (`ResourceTracker::check_time_every`). That leaves operations that do a lot of work in a single
//! call: `'x' * 10**8`, `text.upper()`, `list.insert(0, x)`, `dict.copy()`. A script could repeat one of them
//! in a loop at a few steps an iteration and take hours of real time inside a budget meant to bound it.
//! Such a call is charged here, before it does its work, from sizes that are known in constant time, so the
//! charge is identical on every machine and a budget refuses the call instead of running it.
//!
//! The rates were chosen from measurements (`docs/history/spikes/scripting/monty_native_audit_results_2026-11-16.txt`
//! in the Voxunami repository) so that no operation costs much more than about 100 ns per step against
//! about 4 ns for an ordinary instruction:
//!
//! - text and bytes: one step per [`BYTES_PER_STEP`] bytes (the slowest scans take 2 to 3 ns a byte);
//! - items that are copied, hashed or reference counted: one step per item (7 to 60 ns each);
//! - items that are only moved in memory: one step per [`MOVED_ITEMS_PER_STEP`] items.
//!
//! Operands smaller than a rate divide down to zero, so ordinary scripts keep the counts they had.

use monty_types::ResourceError;

use crate::{args::ArgValues, bytecode::VM, heap_data::HeapData, intern::StaticStrings, value::Value};

/// Bytes of text or bytes processed per step.
pub(crate) const BYTES_PER_STEP: usize = 16;

/// Items moved in memory without being inspected, per step.
pub(crate) const MOVED_ITEMS_PER_STEP: usize = 16;

/// The kind of container an operand is, which decides how a method's cost is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// `str` or `bytes`, measured in bytes.
    Text,
    List,
    Tuple,
    Dict,
    /// `set` or `frozenset`.
    Set,
    Deque,
}

/// A value whose size is known in constant time.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Operand {
    pub kind: Kind,
    pub len: usize,
}

/// Reads the kind and size of `value` without touching its contents, or `None` for anything else
/// (numbers, small inline strings, user objects, iterators).
pub(crate) fn operand(value: &Value, vm: &VM<'_>) -> Option<Operand> {
    let (kind, len) = match value {
        Value::InternString(id) => (Kind::Text, vm.interns.get_str(*id).len()),
        Value::InternBytes(id) => (Kind::Text, vm.interns.get_bytes(*id).len()),
        Value::Ref(id) => match vm.heap.get(*id) {
            HeapData::Str(text) => (Kind::Text, text.as_str().len()),
            HeapData::Bytes(bytes) => (Kind::Text, bytes.as_slice().len()),
            HeapData::List(list) => (Kind::List, list.len()),
            HeapData::Tuple(tuple) => (Kind::Tuple, tuple.as_slice().len()),
            HeapData::Dict(dict) => (Kind::Dict, dict.len()),
            HeapData::Set(set) => (Kind::Set, set.len()),
            HeapData::FrozenSet(set) => (Kind::Set, set.len()),
            HeapData::Deque(deque) => (Kind::Deque, deque.len()),
            _ => return None,
        },
        _ => return None,
    };
    Some(Operand { kind, len })
}

/// Big-integer cost models, in 64-bit limbs so the charge is the same on every platform (the interpreter's own
/// integer words are pointer-sized, which differs on wasm32). Each divides a count of limb operations by a rate
/// measured on the slowest platform tried, so no operation runs at much more than about 100 ns a step.
pub(crate) mod integer {
    /// Bits in one limb of the cost model.
    const LIMB_BITS: u64 = 64;

    fn limbs(bits: u64) -> u64 {
        bits.div_ceil(LIMB_BITS)
    }

    /// Steps for multiplying numbers of `a_bits` and `b_bits` bits: the quadratic limb count over 1024, which
    /// stays above the cost of the Karatsuba and Toom products `num-bigint` uses for large operands.
    pub(crate) fn multiply_steps(a_bits: u64, b_bits: u64) -> u64 {
        limbs(a_bits).saturating_mul(limbs(b_bits)) / 1024
    }

    /// Steps for raising to a power whose result has `result_bits` bits: dominated by the last squarings.
    pub(crate) fn power_steps(result_bits: u64) -> u64 {
        multiply_steps(result_bits, result_bits)
    }

    /// Steps for dividing a number of `dividend_bits` bits by one of `divisor_bits` bits: the quotient limbs
    /// times the divisor limbs over 256, plus one pass over the dividend (a big number divided by a small one
    /// is cheap).
    pub(crate) fn divide_steps(dividend_bits: u64, divisor_bits: u64) -> u64 {
        let quotient_limbs = limbs(dividend_bits).saturating_sub(limbs(divisor_bits)) + 1;
        quotient_limbs.saturating_mul(limbs(divisor_bits)) / 256 + limbs(dividend_bits) / 32
    }

    /// Steps for shifting to a result of `result_bits` bits: a memory move, sixteen limbs a step.
    pub(crate) fn shift_steps(result_bits: u64) -> u64 {
        limbs(result_bits) / 16
    }

    /// Steps for the greatest common divisor of numbers of `a_bits` and `b_bits` bits: Euclid's algorithm.
    pub(crate) fn gcd_steps(a_bits: u64, b_bits: u64) -> u64 {
        limbs(a_bits).saturating_mul(limbs(b_bits)) / 8
    }

    /// Steps for the integer square root of a number of `bits` bits: a few divisions of that size.
    pub(crate) fn isqrt_steps(bits: u64) -> u64 {
        limbs(bits).saturating_mul(limbs(bits)) / 128
    }

    /// Steps for `comb` and `perm`: `k` times a multiplication and a division of the running value by a
    /// small number, which is on average half the final size.
    pub(crate) fn falling_product_steps(k: u64, result_bits: u64) -> u64 {
        k.saturating_mul(limbs(result_bits)) / 32
    }

    /// Steps for one exponent bit of a modular power: a squaring and a reduction at the modulus's size.
    pub(crate) fn modular_power_bit_steps(modulus_bits: u64) -> u64 {
        limbs(modulus_bits).saturating_mul(limbs(modulus_bits)) / 128
    }
}

/// `Decimal` cost models, from the context's precision in digits, measured at 250 to 2000 digits where `exp`,
/// `ln` and a real power grow as about the precision to the power 2.5 and a square root as its square.
pub(crate) mod decimal {
    /// Steps for `exp`, `ln` and a power with a non-integer or oversized exponent at `precision` digits.
    pub(crate) fn transcendental_steps(precision: i64) -> u64 {
        let precision = u64::try_from(precision).unwrap_or(0);
        precision.saturating_mul(precision).saturating_mul(precision.isqrt()) / 600
    }

    /// Steps for a square root at `precision` digits.
    pub(crate) fn root_steps(precision: i64) -> u64 {
        let precision = u64::try_from(precision).unwrap_or(0);
        precision.saturating_mul(precision) / 2000
    }
}

/// Steps for processing `bytes` of text or bytes.
pub(crate) fn text_steps(bytes: usize) -> u64 {
    to_steps(bytes / BYTES_PER_STEP)
}

/// Steps for copying, hashing or reference counting `items` items.
pub(crate) fn item_steps(items: usize) -> u64 {
    to_steps(items)
}

/// Steps for moving `items` items in memory.
pub(crate) fn moved_steps(items: usize) -> u64 {
    to_steps(items / MOVED_ITEMS_PER_STEP)
}

fn to_steps(count: usize) -> u64 {
    u64::try_from(count).unwrap_or(u64::MAX)
}

/// The steps `size` costs, read the way `operand`'s kind is: text in bytes, the rest in items.
pub(crate) fn copy_steps(operand: Operand) -> u64 {
    match operand.kind {
        Kind::Text => text_steps(operand.len),
        Kind::List | Kind::Tuple | Kind::Dict | Kind::Set | Kind::Deque => item_steps(operand.len),
    }
}

/// Charges `steps` against the step budgets.
pub(crate) fn charge(vm: &VM<'_>, steps: u64) -> Result<(), ResourceError> {
    if steps == 0 {
        return Ok(());
    }
    vm.heap.tracker.charge_steps(steps)
}

/// The positional argument at `index`, if the call has one.
fn positional(args: &ArgValues, index: usize) -> Option<&Value> {
    match (args, index) {
        (ArgValues::One(first) | ArgValues::Two(first, _), 0) | (ArgValues::Two(_, first), 1) => Some(first),
        (ArgValues::ArgsKargs { args, .. }, _) => args.get(index),
        _ => None,
    }
}

/// An integer positional argument at `index`, or `None` when absent or not a small integer.
fn integer_argument(args: &ArgValues, index: usize) -> Option<i64> {
    match positional(args, index) {
        Some(Value::Int(value)) => Some(*value),
        _ => None,
    }
}

/// `index` as a position in a sequence of `len` items, with Python's negative indexing, clamped to `0..=len`.
fn clamped_position(len: usize, index: i64) -> usize {
    let len_signed = i64::try_from(len).unwrap_or(i64::MAX);
    let position = if index < 0 {
        (len_signed + index).max(0)
    } else {
        index.min(len_signed)
    };
    usize::try_from(position).unwrap_or(0)
}

/// How many items lie from `index` to the end of a sequence of `len` items.
fn items_from(len: usize, index: i64) -> usize {
    len - clamped_position(len, index)
}

/// Steps to charge before calling `method` on `receiver`, from the sizes of the receiver and the arguments.
///
/// Searches (`find`, `index` and relatives) are charged after the call by [`search_steps`], because a hit
/// near the start costs nothing. Everything not listed here is constant time or already polls the tracker
/// per item, and costs no extra.
pub(crate) fn method_steps(receiver: &Value, method: Option<StaticStrings>, args: &ArgValues, vm: &VM<'_>) -> u64 {
    let (Some(method), Some(receiver)) = (method, operand(receiver, vm)) else {
        return 0;
    };
    let argument = || positional(args, 0).and_then(|value| operand(value, vm));
    match receiver.kind {
        Kind::Text => match method {
            // O(length of the argument) or settled by `search_steps`.
            StaticStrings::Startswith
            | StaticStrings::Endswith
            | StaticStrings::Find
            | StaticStrings::Rfind
            | StaticStrings::Index
            | StaticStrings::Rindex => 0,
            // The result is as wide as the width asked for.
            StaticStrings::Center | StaticStrings::Ljust | StaticStrings::Rjust | StaticStrings::Zfill => {
                let width = integer_argument(args, 0).map_or(0, |width| usize::try_from(width).unwrap_or(0));
                text_steps(width.max(receiver.len))
            }
            _ => text_steps(receiver.len),
        },
        Kind::List => match method {
            StaticStrings::Insert => moved_steps(items_from(receiver.len, integer_argument(args, 0).unwrap_or(0))),
            StaticStrings::Pop => positional(args, 0).map_or(0, |_| {
                moved_steps(items_from(receiver.len, integer_argument(args, 0).unwrap_or(0)).saturating_sub(1))
            }),
            StaticStrings::Reverse | StaticStrings::Clear => moved_steps(receiver.len),
            StaticStrings::Copy | StaticStrings::Sort => item_steps(receiver.len),
            _ => 0,
        },
        Kind::Tuple => 0,
        Kind::Dict => match method {
            StaticStrings::Copy => item_steps(receiver.len),
            StaticStrings::Clear => moved_steps(receiver.len),
            // `update` merges through a helper that charges the pairs it snapshots (see `dict.rs`).
            _ => 0,
        },
        Kind::Set => match method {
            StaticStrings::Copy => item_steps(receiver.len),
            StaticStrings::Clear => moved_steps(receiver.len),
            StaticStrings::Union
            | StaticStrings::Intersection
            | StaticStrings::Difference
            | StaticStrings::SymmetricDifference => {
                item_steps(receiver.len).saturating_add(argument().map_or(0, copy_steps))
            }
            StaticStrings::Update => argument().map_or(0, copy_steps),
            StaticStrings::Isdisjoint | StaticStrings::Issubset | StaticStrings::Issuperset => {
                item_steps(argument().map_or(receiver.len, |other| other.len.min(receiver.len)))
            }
            _ => 0,
        },
        Kind::Deque => match method {
            StaticStrings::Copy | StaticStrings::Count | StaticStrings::Index | StaticStrings::Remove => {
                item_steps(receiver.len)
            }
            StaticStrings::Clear | StaticStrings::Reverse | StaticStrings::Rotate => moved_steps(receiver.len),
            _ => 0,
        },
    }
}

/// Steps for all the text and bytes among the positional arguments of a call: the sum of their sizes.
pub(crate) fn text_arguments_steps(args: &ArgValues, vm: &VM<'_>) -> u64 {
    let text = |value: &Value| match operand(value, vm) {
        Some(Operand { kind: Kind::Text, len }) => text_steps(len),
        _ => 0,
    };
    match args {
        ArgValues::Empty | ArgValues::Kwargs(_) => 0,
        ArgValues::One(first) => text(first),
        ArgValues::Two(first, second) => text(first).saturating_add(text(second)),
        ArgValues::ArgsKargs { args, .. } => args.iter().fold(0, |sum, value| sum.saturating_add(text(value))),
    }
}

/// Steps for reading all of `value` once: text by its bytes, a container by its items. For builtins that
/// walk their argument (`hash` of a tuple or a string).
pub(crate) fn scan_steps(value: &Value, vm: &VM<'_>) -> u64 {
    operand(value, vm).map_or(0, copy_steps)
}

/// Steps for reading all of `value` if it is text or bytes, and nothing for containers, whose own element
/// loops already poll the tracker (`repr` of a list).
pub(crate) fn text_scan_steps(value: &Value, vm: &VM<'_>) -> u64 {
    match operand(value, vm) {
        Some(Operand { kind: Kind::Text, len }) => text_steps(len),
        _ => 0,
    }
}

/// Steps for the slice `index` took from text, read from the `result`: its bytes. Zero when `index` is not a
/// slice, since an element fetched by position is not a copy. A slice of a list or tuple already polls the
/// tracker once per item, so it is not charged again.
pub(crate) fn slice_steps(index: &Value, result: &Value, vm: &VM<'_>) -> u64 {
    let Value::Ref(id) = index else { return 0 };
    if !matches!(vm.heap.get(*id), HeapData::Slice(_)) {
        return 0;
    }
    match operand(result, vm) {
        Some(Operand { kind: Kind::Text, len }) => text_steps(len),
        _ => 0,
    }
}

/// The shape of a binary operator's cost, for [`operator_steps`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Work {
    /// An operator whose work is already counted (numbers, user classes) or constant.
    None,
    /// `+`: concatenation of text, lists or tuples.
    Add,
    /// `*`: repetition of text, lists or tuples.
    Repeat,
    /// `|`, `&`, `^`, `-`: a new set built from both operands (dicts charge in their own merge helpers).
    Combine,
}

/// Steps to charge for applying `work` to `lhs` and `rhs`. An in-place operator on a list, set or dict
/// changes the left operand where it is, so only the right operand's items are processed.
pub(crate) fn operator_steps(work: Work, lhs: &Value, rhs: &Value, in_place: bool, vm: &VM<'_>) -> u64 {
    match work {
        Work::None => 0,
        Work::Add => {
            let (Some(left), Some(right)) = (operand(lhs, vm), operand(rhs, vm)) else {
                return 0;
            };
            match (left.kind, right.kind) {
                (Kind::Text, Kind::Text) => text_steps(left.len.saturating_add(right.len)),
                (Kind::List, Kind::List) if in_place => item_steps(right.len),
                (Kind::List, Kind::List) | (Kind::Tuple, Kind::Tuple) => item_steps(left.len.saturating_add(right.len)),
                _ => 0,
            }
        }
        Work::Repeat => {
            let (sequence, count) = match (lhs, rhs) {
                (_, Value::Int(count)) => (operand(lhs, vm), *count),
                (Value::Int(count), _) => (operand(rhs, vm), *count),
                _ => return 0,
            };
            let Some(sequence) = sequence else { return 0 };
            let length = sequence.len.saturating_mul(usize::try_from(count).unwrap_or(0));
            match sequence.kind {
                Kind::Text => text_steps(length),
                Kind::List | Kind::Tuple => item_steps(length),
                _ => 0,
            }
        }
        Work::Combine => {
            let (Some(left), Some(right)) = (operand(lhs, vm), operand(rhs, vm)) else {
                return 0;
            };
            // Dict merges charge the pairs they copy in `dict.rs`; sets are charged here.
            let both = matches!((left.kind, right.kind), (Kind::Set, Kind::Set));
            match (both, in_place) {
                (false, _) => 0,
                (true, true) => item_steps(right.len),
                (true, false) => item_steps(left.len.saturating_add(right.len)),
            }
        }
    }
}

/// Steps for comparing two values: text compares byte by byte, up to the shorter length for an ordering
/// and, for equality, only when the lengths agree. Containers compare element by element through loops that
/// already poll the tracker.
pub(crate) fn comparison_steps(equality: bool, lhs: &Value, rhs: &Value, vm: &VM<'_>) -> u64 {
    let (Some(left), Some(right)) = (operand(lhs, vm), operand(rhs, vm)) else {
        return 0;
    };
    if left.kind != Kind::Text || right.kind != Kind::Text || (equality && left.len != right.len) {
        return 0;
    }
    text_steps(left.len.min(right.len))
}

/// Steps for `needle in haystack` on text: the whole haystack, since a hit's position is not known here. A
/// hit costs less in real time, so this never undercharges.
pub(crate) fn membership_steps(haystack: &Value, vm: &VM<'_>) -> u64 {
    match operand(haystack, vm) {
        Some(Operand { kind: Kind::Text, len }) => text_steps(len),
        _ => 0,
    }
}

/// Whether `method` on text scans for a needle and stops at the first hit, so its cost is only known
/// afterwards.
pub(crate) fn is_search(method: Option<StaticStrings>) -> bool {
    matches!(
        method,
        Some(StaticStrings::Find | StaticStrings::Rfind | StaticStrings::Index | StaticStrings::Rindex)
    )
}

/// Where a search began, read from the arguments before the call consumes them (the optional `start`).
pub(crate) fn search_start(args: &ArgValues) -> i64 {
    integer_argument(args, 1).unwrap_or(0)
}

/// Steps for a search on text of `len` bytes that began at `start` and found `found_at` (`None` for a miss),
/// scanning forward when `forward` and from the end otherwise.
pub(crate) fn search_steps(len: usize, start: i64, found_at: Option<usize>, forward: bool) -> u64 {
    let from = clamped_position(len, start);
    let scanned = match (found_at, forward) {
        (None, _) => len.saturating_sub(from),
        (Some(position), true) => position.saturating_sub(from),
        (Some(position), false) => len.saturating_sub(position),
    };
    text_steps(scanned)
}
