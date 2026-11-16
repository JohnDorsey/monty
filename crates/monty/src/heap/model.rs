//! The deterministic memory model: what a session holds, counted by a fixed cost table.
//!
//! The allocator-backed `max_memory` measures bytes requested from the process's global allocator, so the
//! same script reaches its limit at a different point when anything else in the process allocates, when a
//! type is laid out differently on another platform, or when a standard-library version changes how a buffer
//! grows. This model measures nothing of the machine. It adds up a fixed price for every live heap object,
//! computed from the object's *logical* contents (its length, not its capacity), so the number is a function
//! of the program's state alone and is identical on every platform.
//!
//! The prices approximate what a 64-bit build really costs, but they are a convention, not a measurement:
//! changing one is a deliberate edit, pinned by `tests/model_memory_costs.rs`.
//!
//! [`HeapData::model_bytes`] matches every variant with no wildcard arm, so adding a payload type to the heap
//! forces a decision about its price here.

use super::{EMPTY_TUPLE_ID, Heap, HeapEntry};
use crate::heap_data::HeapData;

/// What a heap object costs before its contents: the entry holding its reference count, reader count, payload
/// and collector colour. This is `size_of::<HeapEntry>()` on a 64-bit build, checked by a test.
pub(crate) const OBJECT_BYTES: usize = 96;

/// One value slot in a container, a global or the operand stack. This is `size_of::<Value>()` on every
/// target the interpreter builds for, checked by a test.
pub(crate) const VALUE_BYTES: usize = 16;

/// One dict entry: the key, the value and the hash (40 bytes), the index word beside it (8), and the spare
/// room a growing table keeps, which averages half as much again. Also one attribute of an instance, class or
/// module, which are dicts.
pub(crate) const DICT_ENTRY_BYTES: usize = 72;

/// One set element: the value and the hash (24 bytes), the index word (8), and the same spare room.
pub(crate) const SET_ENTRY_BYTES: usize = 40;

/// The separate allocation behind a heap object whose payload is boxed: an instance (104 bytes), a named tuple
/// (104), a partial (64), a cache (120), a match (160), a class or module (152). One price for all of them.
pub(crate) const BOXED_BODY_BYTES: usize = 112;

/// One entry of a `functools.lru_cache`: call key, result, hash and recency stamp.
pub(crate) const CACHE_ENTRY_BYTES: usize = 56;

/// One 64-bit limb of an integer or decimal coefficient.
pub(crate) const LIMB_BYTES: usize = 8;

/// One heap reference held by a container, such as a closure's captured cell or a gather's task.
pub(crate) const REFERENCE_BYTES: usize = 8;

/// The state of a `random.Random` generator: the Mersenne Twister's 624 words and its position.
pub(crate) const RANDOM_STATE_BYTES: usize = 2_560;

/// A compiled regular expression before its pattern text. The compiled program grows with the pattern, which
/// the per-character price covers.
pub(crate) const PATTERN_BASE_BYTES: usize = 2_048;

/// What compiling one character of a regular expression adds to the program.
pub(crate) const PATTERN_CHARACTER_BYTES: usize = 64;

/// A `hashlib` object's running state and block buffer.
pub(crate) const HASH_STATE_BYTES: usize = 256;

/// What each `eval` or `exec` call that compiles adds to the session whatever the source says: the retained
/// source text's record, the code object and the tables behind it. Nothing is shared between calls, so a loop of
/// `exec` grows the session without bound.
pub(crate) const COMPILED_SNIPPET_BYTES: usize = 640;

/// What each byte of source compiled by `eval` or `exec` adds: the text itself and the bytecode, constants and
/// names compiled from it.
pub(crate) const COMPILED_SOURCE_BYTE_BYTES: usize = 12;

/// One call frame of the interpreter, besides the values it keeps on the operand stack.
pub(crate) const FRAME_BYTES: usize = 128;

impl HeapData {
    /// The model price of this object: [`OBJECT_BYTES`] plus what it holds, counted from its logical size.
    ///
    /// What other heap objects it refers to is not included; each is its own entry and is priced once, however
    /// many references point to it.
    pub(crate) fn model_bytes(&self) -> usize {
        OBJECT_BYTES + self.model_contents_bytes()
    }

    fn model_contents_bytes(&self) -> usize {
        match self {
            Self::Str(text) => text.as_str().len(),
            Self::Bytes(bytes) => bytes.as_slice().len(),
            Self::List(list) => list.len() * VALUE_BYTES,
            Self::Deque(deque) => deque.len() * VALUE_BYTES,
            Self::Tuple(tuple) => tuple.as_slice().len() * VALUE_BYTES,
            Self::Dict(dict) => dict.len() * DICT_ENTRY_BYTES,
            Self::Set(set) => set.len() * SET_ENTRY_BYTES,
            Self::FrozenSet(set) => set.len() * SET_ENTRY_BYTES,
            Self::Closure(closure) => closure.cells.len() * REFERENCE_BYTES + closure.defaults.len() * VALUE_BYTES,
            Self::FunctionDefaults(defaults) => defaults.defaults.len() * VALUE_BYTES,
            Self::LongInt(integer) => usize::try_from(integer.bits().div_ceil(64))
                .unwrap_or(usize::MAX)
                .saturating_mul(LIMB_BYTES),
            Self::Decimal(decimal) => decimal.coefficient_words() * LIMB_BYTES,
            Self::Coroutine(coroutine) => coroutine.namespace.len() * VALUE_BYTES,
            Self::Path(path) => path.as_str().len(),
            Self::Itertools(iterator) => iterator.buffered_values() * VALUE_BYTES,
            // Payloads kept in a box of their own, with what they hold.
            Self::NamedTuple(tuple) => BOXED_BODY_BYTES + tuple.len() * VALUE_BYTES,
            Self::Instance(instance) => BOXED_BODY_BYTES + instance.attrs().len() * DICT_ENTRY_BYTES,
            Self::Class(class) => BOXED_BODY_BYTES + class.namespace().len() * DICT_ENTRY_BYTES,
            Self::Module(module) => BOXED_BODY_BYTES + module.attrs().len() * DICT_ENTRY_BYTES,
            Self::HostClass(class) => BOXED_BODY_BYTES + class.attribute_count() * DICT_ENTRY_BYTES,
            Self::GatherFuture(gather) => BOXED_BODY_BYTES + gather.items.len() * REFERENCE_BYTES,
            Self::RePattern(pattern) => {
                BOXED_BODY_BYTES + PATTERN_BASE_BYTES + pattern.pattern_len() * PATTERN_CHARACTER_BYTES
            }
            Self::ReMatch(matched) => BOXED_BODY_BYTES + matched.text_bytes() + matched.group_count() * VALUE_BYTES,
            Self::Partial(partial) => BOXED_BODY_BYTES + partial.slot_count() * VALUE_BYTES,
            Self::LruCache(cache) => BOXED_BODY_BYTES + cache.entry_count() * CACHE_ENTRY_BYTES,
            // Boxed objects whose state is the box: the generator's and the hash's state is the price.
            Self::Random(_) => RANDOM_STATE_BYTES,
            Self::HashObject(_) => HASH_STATE_BYTES,
            // Boxed objects of fixed size.
            Self::NamedTupleClass(_)
            | Self::HostClassType(_)
            | Self::DataclassField(_)
            | Self::DataclassMethod(_)
            | Self::ExternalFuture(_)
            | Self::OpenFile(_)
            | Self::ForwardRef(_)
            | Self::DirScan(_)
            | Self::Csv(_) => BOXED_BODY_BYTES,
            // Objects held inline in their entry, of fixed size, or that only refer to others (their
            // referents are priced as their own entries). Listed one by one, with no wildcard, so that a
            // payload gaining a growable buffer is noticed here.
            Self::DictKeysView(_)
            | Self::DictItemsView(_)
            | Self::DictValuesView(_)
            | Self::Cell(_)
            | Self::Range(_)
            | Self::Slice(_)
            | Self::Exception(_)
            | Self::BoundMethod(_)
            | Self::ListIterator(_)
            | Self::DequeIterator(_)
            | Self::TupleIterator(_)
            | Self::StringIterator(_)
            | Self::BytesIterator(_)
            | Self::RangeIterator(_)
            | Self::DictKeyIterator(_)
            | Self::DictItemIterator(_)
            | Self::DictValueIterator(_)
            | Self::SetIterator(_)
            | Self::CallableIterator(_)
            | Self::Complex(_)
            | Self::ExtFunction(_)
            | Self::Date(_)
            | Self::DateTime(_)
            | Self::TimeDelta(_)
            | Self::TimeZone(_)
            | Self::DataclassParams(_)
            | Self::Time(_)
            | Self::GenericAlias(_)
            | Self::Union(_)
            | Self::DecimalContext(_)
            | Self::DecimalSignalDict(_)
            | Self::DecimalContextManager(_) => 0,
        }
    }
}

/// The payload of `entry`, for pricing it.
///
/// A free function rather than a method, because `HeapEntry` derives `Deserialize` and clippy asks that such a
/// type have no methods that use `unsafe`.
fn payload(entry: &HeapEntry) -> &HeapData {
    // SAFETY: only called while the heap is borrowed shared and no payload is borrowed mutably, which the
    // `HeapReader` API guarantees at every point the interpreter measures (the dispatch checkpoint, and
    // between feeds). This is the same read-only access `Debug` takes.
    unsafe { &*entry.data.0.get() }
}

impl Heap {
    /// The model price of every live heap object, and how many there were: what the session holds on the
    /// heap, by the cost table.
    ///
    /// Walks every entry, so it costs time proportional to the heap. The empty-tuple singleton every heap
    /// holds is not counted, so an empty session costs nothing. The count of objects, unlike the number of
    /// slots the heap has grown to, is the same after a dump and restore, which is why walks are scheduled and
    /// charged by it.
    pub(crate) fn model_walk(&self) -> (usize, usize) {
        self.entries
            .iter()
            .filter(|(id, _)| *id != EMPTY_TUPLE_ID)
            .fold((0, 0), |(bytes, count), (_, entry)| {
                (bytes + payload(entry).model_bytes(), count + 1)
            })
    }

    /// The model price of every live heap object. See [`model_walk`](Self::model_walk).
    pub(crate) fn model_bytes(&self) -> usize {
        self.model_walk().0
    }
}

#[cfg(test)]
mod tests {
    use std::mem::size_of;

    use super::{HeapEntry, OBJECT_BYTES, VALUE_BYTES};
    use crate::value::Value;

    /// The prices are conventions, but these two are meant to be what a 64-bit build really is, so that the
    /// model stays an approximation of real use. A change to either layout fails here, and the price is then a
    /// deliberate decision (every pinned count in `tests/model_memory_costs.rs` moves with it).
    #[test]
    #[cfg(target_pointer_width = "64")]
    fn the_object_and_value_prices_match_the_real_layout() {
        assert_eq!(size_of::<HeapEntry>(), OBJECT_BYTES);
        assert_eq!(size_of::<Value>(), VALUE_BYTES);
    }
}
