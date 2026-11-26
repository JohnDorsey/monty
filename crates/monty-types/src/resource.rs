//! Resource limits: the [`ResourceTracker`] used by the interpreter heap/VM
//! and its [`ResourceLimits`] configuration.

#[cfg(target_family = "wasm")]
use std::hint;
#[cfg(not(target_family = "wasm"))]
use std::thread;
#[cfg(all(target_family = "wasm", not(target_os = "unknown")))]
use std::time::Instant;
use std::{
    cell::Cell,
    error::Error,
    fmt,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use web_time::Instant;

mod execution_clock;
use execution_clock::ExecutionInstant;

/// Exit code a worker uses when it exceeded its memory limit or the allocator
/// refused an allocation, so the client can report `MemoryError` instead of an
/// unclassifiable `SIGABRT`.
///
/// `EX_DATAERR` from BSD `sysexits.h`. See <https://man.freebsd.org/cgi/man.cgi?query=sysexits>.
pub const OOM_EXIT_CODE: i32 = 65;
/// Allocator-backed live bytes requested through the global allocator
pub static LIVE_MEMORY: AtomicUsize = AtomicUsize::new(0);
/// What the worker costs to exist: the leanest the process has ever been at an
/// arming point, plus structures it keeps for life (see [`allocate_into_baseline`]).
pub static BASELINE_MEMORY: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Runs `build` and adds what it leaves allocated to [`BASELINE_MEMORY`], so a
/// structure the worker keeps for life (such as typeshed) is not charged to the
/// session that happened to build it first.
///
/// Only accurate while no other thread changes [`LIVE_MEMORY`]. An unset baseline is
/// left alone: the first arming counts the structure anyway.
pub fn allocate_into_baseline<T>(build: impl FnOnce() -> T) -> T {
    measure_into_baseline(build).0
}

/// [`allocate_into_baseline`], also returning the bytes the structure cost.
///
/// For a structure the worker later frees: hand the cost to [`release_from_baseline`]
/// when it goes, or the baseline keeps it and whatever leak fills the gap is never charged.
/// The cost includes any process-lifetime data `build` initializes on first use, so releasing it
/// undercounts the baseline by those (about 30 KB of typeshed for the prewarmed type checker).
pub fn measure_into_baseline<T>(build: impl FnOnce() -> T) -> (T, usize) {
    let before = LIVE_MEMORY.load(Ordering::Relaxed);
    let value = build();
    let cost = LIVE_MEMORY.load(Ordering::Relaxed).saturating_sub(before);
    // `Err` just means the baseline is still unset
    let _ = BASELINE_MEMORY.try_update(Ordering::Relaxed, Ordering::Relaxed, |baseline| {
        (baseline != usize::MAX).then(|| baseline.saturating_add(cost))
    });
    (value, cost)
}

/// Takes `cost` back out of [`BASELINE_MEMORY`] once the structure
/// [`measure_into_baseline`] measured is freed.
///
/// Call it before the next arming: `fetch_min` alone would settle on the floor plus
/// any session residue, folding that residue into the baseline.
pub fn release_from_baseline(cost: usize) {
    // `Err` just means the baseline is still unset
    let _ = BASELINE_MEMORY.try_update(Ordering::Relaxed, Ordering::Relaxed, |baseline| {
        (baseline != usize::MAX).then(|| baseline.saturating_sub(cost))
    });
}

/// Headroom for exception machinery and work between interpreter checkpoints.
const MEMORY_LIMIT_HEADROOM: usize = 4 * 1024 * 1024;
/// Extra headroom for type-checker stubs and caches outside Python execution.
const TYPE_CHECK_MEMORY_LIMIT_HEADROOM: usize = 32 * 1024 * 1024;

/// Converts an interpreter soft memory limit into a worker allocator budget.
///
/// The returned budget includes operational headroom but remains relative to
/// the worker baseline, which `monty-alloc` adds when arming its hard ceiling.
#[must_use]
pub fn memory_limit_with_headroom(max_memory: Option<usize>, type_check: bool) -> Option<usize> {
    max_memory.map(|bytes| {
        let headroom = if type_check {
            TYPE_CHECK_MEMORY_LIMIT_HEADROOM
        } else {
            MEMORY_LIMIT_HEADROOM
        };
        bytes.saturating_add(headroom)
    })
}

/// Threshold in bytes above which `check_large_result` is called.
///
/// Operations that may produce results larger than this threshold (100KB) should call
/// `check_large_result` before performing the operation. This prevents DoS attacks
/// where operations like `2 ** 10_000_000` allocate huge amounts of memory before
/// the memory check can catch them.
pub const LARGE_RESULT_THRESHOLD: usize = 100_000;

/// The fewest steps between two walks of the session for the deterministic memory limit.
const MODEL_WALK_MIN_INTERVAL: u64 = 256;
/// A walk visits every heap slot, so walks are spaced at least this many slots per step apart: two slot
/// visits per step at the most, however close the session sits to its limit.
const MODEL_WALK_SLOTS_PER_STEP: u64 = 2;
/// Heap slots one step of the budget pays for when a walk visits them.
const MODEL_WALK_SLOTS_PER_CHARGED_STEP: usize = 4;
/// The most an existing buffer grows in a step without the model being told. A push is a call, the value
/// it pushes and a few loads, five instructions or more for a list (16 bytes) and nine for a dict entry (72),
/// so a sustained rate is under 8 bytes a step; this allows half as much again. A bound used only to decide
/// when to walk, so being too low costs precision, never correctness.
const MODEL_IN_PLACE_BYTES_PER_STEP: u64 = 12;
/// What is taken on since the last walk, as a fraction of the limit, that makes a walk due at once whatever
/// the spacing: a bulk allocation is worth measuring as soon as it has happened.
const MODEL_WALK_BULK_DIVISOR: usize = 8;
/// Error returned when a resource limit is exceeded during execution.
///
/// This allows the sandbox to enforce strict limits on execution time
/// and memory usage.
///
/// All variants except `Recursion` are **uncatchable** inside the sandbox:
/// untrusted code must never intercept resource enforcement. `Recursion`
/// surfaces as a catchable `RecursionError`, matching CPython.
#[derive(Debug, Clone)]
pub enum ResourceError {
    /// One of the two execution-time budgets was exceeded; `scope` says which.
    Time {
        scope: TimeLimitScope,
        limit: Duration,
        elapsed: Duration,
    },
    /// One of the two step budgets was exceeded; `scope` says which. The scope type is shared with
    /// the time budgets because the two pairs reset at the same moments.
    Steps {
        scope: TimeLimitScope,
        limit: u64,
        used: u64,
    },
    /// Maximum memory usage exceeded.
    Memory { limit: usize, used: usize },
    /// The deterministic memory limit, [`ResourceLimits::max_model_memory`], was exceeded.
    ModelMemory { limit: usize, used: usize },
    /// Maximum recursion depth exceeded.
    Recursion { limit: usize, depth: usize },
}

/// Which of the two nested execution-time budgets a [`ResourceError::Time`]
/// refers to.
///
/// Both read the same clock and differ only in when they are reset, so a
/// turn's time is charged to its feed as well: `Feed >= Turn` always holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeLimitScope {
    /// [`ResourceLimits::max_feed_duration`] — reset at each feed.
    Feed,
    /// [`ResourceLimits::max_turn_duration`] — reset at each feed and each
    /// resume.
    Turn,
}

impl TimeLimitScope {
    /// The word naming this scope in an error message.
    fn prefix(self) -> &'static str {
        match self {
            Self::Feed => "feed ",
            Self::Turn => "turn ",
        }
    }
}

impl fmt::Display for ResourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Time { scope, limit, elapsed } => {
                write!(f, "{}time limit exceeded: {elapsed:?} > {limit:?}", scope.prefix())
            }
            Self::Steps { scope, limit, used } => {
                write!(f, "{}step limit exceeded: {used} steps > {limit} steps", scope.prefix())
            }
            Self::Memory { limit, used } => {
                write!(f, "memory limit exceeded: {used} bytes > {limit} bytes")
            }
            Self::ModelMemory { limit, used } => {
                write!(f, "model memory limit exceeded: {used} bytes > {limit} bytes")
            }
            Self::Recursion { .. } => {
                write!(f, "maximum recursion depth exceeded")
            }
        }
    }
}

impl Error for ResourceError {}

/// Clock used for execution budgets and execution-time telemetry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionClock {
    /// Elapsed time while the interpreter executes, including descheduling.
    #[default]
    Wall,
    /// Thread CPU time, Linux only. A failed native clock uses conservative elapsed time.
    /// CPU accounting is approximate and does not bound response latency.
    Cpu,
}

impl ExecutionClock {
    /// Whether this thread can read the selected clock. CPU reads require Linux and the `cpu-clock` feature.
    #[must_use]
    pub fn is_available(self) -> bool {
        self == Self::Wall || execution_clock::thread_cpu().is_some()
    }
}

/// Configuration for resource limits.
///
/// The time/memory/GC limits are optional — set to `None` to disable — but
/// recursion depth and the suspension budget are always bounded (defaults
/// [`DEFAULT_MAX_RECURSION_DEPTH`] and [`DEFAULT_MAX_SUSPENSIONS`]): unbounded
/// recursion would let sandboxed code overflow the native stack and abort the
/// process, and unbounded suspensions would let it loop on host calls. Use
/// `ResourceLimits::default()` for the recursion-only defaults, or build
/// custom limits with the builder pattern.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResourceLimits {
    /// Maximum execution time per feed (`feed_start`, `feed_run` or `call_function`),
    /// summed across turns, excluding host suspension. Uses the tracker's clock
    /// (wall time by default, or [`ExecutionClock::Cpu`]). Bounds a snippet, not the session.
    pub max_feed_duration: Option<Duration>,
    /// Maximum execution time for a single host turn, reset at each feed and
    /// each resume. Bounds the stretch of sandbox code between two host round
    /// trips. Use a separate host deadline to bound elapsed response time.
    pub max_turn_duration: Option<Duration>,
    /// Maximum deterministic steps per feed, summed across turns. A step is one bytecode instruction, or
    /// one item of native work (see [`ResourceTracker::check_time_every`]); the count depends only on the
    /// program and its inputs, never on the speed of the machine, so a script stops at the same point
    /// everywhere. Reset at each feed.
    #[serde(default)]
    pub max_feed_steps: Option<u64>,
    /// Maximum deterministic steps for a single host turn, reset at each feed and each resume. See
    /// [`max_feed_steps`](Self::max_feed_steps).
    #[serde(default)]
    pub max_turn_steps: Option<u64>,
    /// Maximum allocator-backed memory in bytes.
    ///
    /// Requires the executable to install and arm `monty-alloc`.
    pub max_memory: Option<usize>,
    /// Maximum memory in bytes by the deterministic cost model, which prices every live heap object from its
    /// logical size and never reads the allocator. A script stops at the same step on every machine, however
    /// busy the process is, and the limit needs no `monty-alloc`. The count is a fixed convention that
    /// approximates a 64-bit build's real use; see the `FORK.md` section "Model memory".
    #[serde(default)]
    pub max_model_memory: Option<usize>,
    /// Run garbage collection every N GC-tracked allocations; `None` uses [`DEFAULT_GC_INTERVAL`].
    pub gc_interval: Option<usize>,
    /// Maximum recursion depth (function call stack depth).
    pub max_recursion_depth: usize,
    /// Maximum suspensions the host may service (default
    /// [`DEFAULT_MAX_SUSPENSIONS`]; always bounded, like recursion depth).
    /// The interpreter only stores this limit; hosts must enforce it.
    pub max_suspensions: usize,
    /// Maximum cumulative sleep under `SleepMode::System`, enforced by the host.
    /// Sleeps do not count toward execution duration; without this limit, sleeping
    /// loops need `max_suspensions` or a host deadline.
    pub max_total_sleep: Option<Duration>,
}

/// Recommended maximum recursion depth if not otherwise specified.
pub const DEFAULT_MAX_RECURSION_DEPTH: usize = 1000;

/// GC-tracked allocations between cycle collections when `gc_interval` is `None`.
/// Builds with monty's `memory-model-checks` feature collect on every allocation instead.
pub const DEFAULT_GC_INTERVAL: usize = 100_000;

/// Maximum suspensions a host services per session if not otherwise
/// specified: a backstop against a sandbox looping on host calls while the
/// execution clock is paused.
pub const DEFAULT_MAX_SUSPENSIONS: usize = 1000;

/// Creates a new ResourceLimits with all limits disabled, except max recursion
/// depth and max suspensions, which are set to 1000.
impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_feed_duration: None,
            max_turn_duration: None,
            max_feed_steps: None,
            max_turn_steps: None,
            max_memory: None,
            max_model_memory: None,
            gc_interval: None,
            max_recursion_depth: DEFAULT_MAX_RECURSION_DEPTH,
            max_suspensions: DEFAULT_MAX_SUSPENSIONS,
            max_total_sleep: None,
        }
    }
}

impl ResourceLimits {
    /// Sets the maximum execution duration for any single feed.
    #[must_use]
    pub fn max_feed_duration(mut self, limit: Duration) -> Self {
        self.max_feed_duration = Some(limit);
        self
    }

    /// Sets the maximum execution duration for any single host turn.
    #[must_use]
    pub fn max_turn_duration(mut self, limit: Duration) -> Self {
        self.max_turn_duration = Some(limit);
        self
    }

    /// Sets allocator-backed maximum memory usage in bytes.
    ///
    /// Requires the executable to install and arm `monty-alloc`; otherwise
    /// the limit is silently not enforced.
    #[must_use]
    pub fn max_memory(mut self, limit: usize) -> Self {
        self.max_memory = Some(limit);
        self
    }

    /// Sets the deterministic memory limit in bytes, by the cost model.
    #[must_use]
    pub fn max_model_memory(mut self, limit: usize) -> Self {
        self.max_model_memory = Some(limit);
        self
    }

    /// Sets the maximum number of deterministic steps for any single feed.
    #[must_use]
    pub fn max_feed_steps(mut self, limit: u64) -> Self {
        self.max_feed_steps = Some(limit);
        self
    }

    /// Sets the maximum number of deterministic steps for any single host turn.
    #[must_use]
    pub fn max_turn_steps(mut self, limit: u64) -> Self {
        self.max_turn_steps = Some(limit);
        self
    }

    /// Sets the garbage collection interval (run GC every N GC-tracked allocations).
    #[must_use]
    pub fn gc_interval(mut self, interval: usize) -> Self {
        self.gc_interval = Some(interval);
        self
    }

    /// Sets the maximum recursion depth (function call stack depth).
    #[must_use]
    pub fn max_recursion_depth(mut self, limit: usize) -> Self {
        self.max_recursion_depth = limit;
        self
    }

    /// Sets the host-enforced maximum number of suspensions.
    #[must_use]
    pub fn max_suspensions(mut self, limit: usize) -> Self {
        self.max_suspensions = limit;
        self
    }

    /// Sets the maximum cumulative time hosts wait on system sleeps; each capped delay is charged before the wait.
    #[must_use]
    pub fn max_total_sleep(mut self, limit: Duration) -> Self {
        self.max_total_sleep = Some(limit);
        self
    }
}

/// A resource tracker that enforces configurable limits.
///
/// Checks allocator-backed memory usage and tracks execution time, returning
/// errors when limits are exceeded. It also schedules garbage collection.
///
/// Uses `Cell` for mutable timing and recursion state behind shared references.
///
/// The two duration limits share one *execution time* clock: it runs only
/// between the outermost `on_execution_start`/`on_execution_stop` pair, so it
/// is paused while suspended on the host and between REPL feeds. They differ
/// only in when their accumulator resets — at
/// [`on_feed_start`](Self::on_feed_start), at [`on_turn_start`](Self::on_turn_start)
/// — and the feed total is serialized, so a session loaded mid-feed resumes
/// that budget rather than restarting from zero.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ResourceTracker {
    /// Clock paired with the accumulated execution time. Legacy dumps use wall time.
    #[serde(default)]
    execution_clock: ExecutionClock,
    limits: ResourceLimits,
    /// Execution time accumulated by completed `on_execution_start`/`stop`
    /// windows. Bounds nothing — it is what [`elapsed`](Self::elapsed) reports
    /// to hosts for telemetry — but is serialized so a loaded session keeps
    /// counting from where it left off.
    total_execution_time: Cell<Duration>,
    /// Execution time accumulated since the last [`on_feed_start`](Self::on_feed_start).
    /// Serialized like `total_execution_time`: a dump taken mid-feed resumes
    /// that feed, so its budget must survive the round trip.
    feed_execution_time: Cell<Duration>,
    /// Execution time accumulated since the last [`on_turn_start`](Self::on_turn_start).
    /// Not serialized — a dump is taken between turns, and the resume that
    /// loads it starts a fresh turn.
    #[serde(skip)]
    turn_execution_time: Cell<Duration>,
    /// Deterministic steps taken over the whole session (telemetry; bounds nothing).
    #[serde(default)]
    total_steps: Cell<u64>,
    /// Steps taken since the last [`on_feed_start`](Self::on_feed_start); serialized like the feed time.
    #[serde(default)]
    feed_steps: Cell<u64>,
    /// Steps taken since the last [`on_turn_start`](Self::on_turn_start); not serialized, like the turn time.
    #[serde(skip)]
    turn_steps: Cell<u64>,
    /// When the current execution window started; `None` while suspended or
    /// idle. Never serialized — a snapshot is by definition taken while not
    /// executing.
    #[serde(skip)]
    running_since: Cell<Option<ExecutionInstant>>,
    /// What the model measured at its last walk of the session, in bytes. Serialized, with the rest of the
    /// measuring schedule, so a restored session is measured exactly as the one that was dumped would be.
    #[serde(default)]
    model_used: Cell<usize>,
    /// Bytes the model has been told were taken on since the last walk. An upper bound on growth: it never
    /// subtracts what was freed, so it can only trigger a measurement, never a verdict.
    #[serde(default)]
    model_pending: Cell<usize>,
    /// The session's step count at the last walk.
    #[serde(default)]
    model_walk_steps: Cell<u64>,
    /// How many heap slots the last walk visited; the next walk is scheduled in proportion to it.
    #[serde(default)]
    model_walk_entries: Cell<usize>,
    /// Optional override applied on top of `limits.max_recursion_depth`.
    ///
    /// `None` (the default — also the value any pre-`test-hooks` snapshot
    /// deserializes to) means "no override, use the configured ceiling".
    /// `Some(N)` means "use `N` as the live recursion ceiling instead", and
    /// is only ever populated by
    /// [`lower_recursion_limit`](Self::lower_recursion_limit)
    /// under the `test-hooks` feature — `sys.setrecursionlimit` uses it to
    /// tighten the bound from Python code without escaping the
    /// host-configured ceiling. `None` means the configured limit applies.
    recursion_limit_override: Cell<Option<usize>>,
}

impl Default for ResourceTracker {
    fn default() -> Self {
        Self::new(ResourceLimits::default())
    }
}

impl ResourceTracker {
    /// Caps restored limits without resetting consumption or relaxing stricter saved limits.
    /// Callers must check clock compatibility first. This retains the saved clock.
    pub fn tighten_limits(&mut self, ceiling: &ResourceLimits) {
        self.limits.max_memory = tighter_limit(self.limits.max_memory, ceiling.max_memory);
        self.limits.max_model_memory = tighter_limit(self.limits.max_model_memory, ceiling.max_model_memory);
        self.limits.max_feed_duration = tighter_limit(self.limits.max_feed_duration, ceiling.max_feed_duration);
        self.limits.max_turn_duration = tighter_limit(self.limits.max_turn_duration, ceiling.max_turn_duration);
        self.limits.max_feed_steps = tighter_limit(self.limits.max_feed_steps, ceiling.max_feed_steps);
        self.limits.max_turn_steps = tighter_limit(self.limits.max_turn_steps, ceiling.max_turn_steps);
        self.limits.max_total_sleep = tighter_limit(self.limits.max_total_sleep, ceiling.max_total_sleep);
        self.limits.gc_interval = tighter_gc_interval(self.limits.gc_interval, ceiling.gc_interval);
        self.limits.max_recursion_depth = self.limits.max_recursion_depth.min(ceiling.max_recursion_depth);
        self.limits.max_suspensions = self.limits.max_suspensions.min(ceiling.max_suspensions);
        if let Some(limit) = self.recursion_limit_override.get() {
            self.recursion_limit_override
                .set(Some(limit.min(self.limits.max_recursion_depth)));
        }
    }

    /// Creates a new ResourceTracker with the given limits.
    ///
    /// The execution-time clock starts at zero and only runs while the VM
    /// executes, so the tracker can be created any amount of time before
    /// the first run without consuming a duration budget. A configured
    /// `max_memory` requires `monty-alloc` installed as the global allocator
    /// and armed via `set_hard_limit(memory_limit_with_headroom(...))`;
    /// otherwise it is silently not enforced.
    #[must_use]
    pub fn new(limits: ResourceLimits) -> Self {
        Self {
            execution_clock: ExecutionClock::Wall,
            limits,
            total_execution_time: Cell::new(Duration::ZERO),
            feed_execution_time: Cell::new(Duration::ZERO),
            turn_execution_time: Cell::new(Duration::ZERO),
            total_steps: Cell::new(0),
            feed_steps: Cell::new(0),
            turn_steps: Cell::new(0),
            running_since: Cell::new(None),
            model_used: Cell::new(0),
            model_pending: Cell::new(0),
            model_walk_steps: Cell::new(0),
            model_walk_entries: Cell::new(0),
            recursion_limit_override: Cell::new(None),
        }
    }

    /// Selects the execution clock before the tracker starts running.
    ///
    /// Off Linux, CPU mode cannot read the thread counter and charges elapsed time instead;
    /// check [`ExecutionClock::is_available`] first to refuse it.
    #[must_use]
    pub fn with_execution_clock(mut self, clock: ExecutionClock) -> Self {
        self.execution_clock = clock;
        self
    }

    /// Returns the live recursion ceiling: the override if one is in effect,
    /// otherwise the configured `max_recursion_depth`.
    #[inline]
    fn active_recursion_limit(&self) -> usize {
        self.recursion_limit_override
            .get()
            .unwrap_or(self.limits.max_recursion_depth)
    }

    /// Returns cumulative execution time using the configured clock, excluding
    /// time suspended on the host or idle between feeds. Includes the running window.
    ///
    /// Nothing is bounded by this — it is reported to the host for telemetry.
    /// The budgets are [`feed_elapsed`](Self::feed_elapsed) and
    /// [`turn_elapsed`](Self::turn_elapsed).
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.total_execution_time.get() + self.running_window()
    }

    /// Returns the execution time consumed by the current feed; see
    /// [`elapsed`](Self::elapsed), which reports the same clock over the
    /// whole session.
    #[must_use]
    pub fn feed_elapsed(&self) -> Duration {
        self.feed_execution_time.get() + self.running_window()
    }

    /// Returns the execution time consumed by the current host turn; see
    /// [`elapsed`](Self::elapsed), which reports the same clock over the
    /// whole session.
    #[must_use]
    pub fn turn_elapsed(&self) -> Duration {
        self.turn_execution_time.get() + self.running_window()
    }

    /// The in-progress execution window, or zero when not executing. Read
    /// once per check so both budgets are compared against one clock reading
    /// rather than two.
    #[inline]
    fn running_window(&self) -> Duration {
        let Some(mut window) = self.running_since.get() else {
            return Duration::ZERO;
        };
        let elapsed = window.elapsed();
        self.running_since.set(Some(window));
        elapsed
    }

    /// Returns the clock saved with this tracker's accumulated consumption.
    #[must_use]
    pub fn execution_clock(&self) -> ExecutionClock {
        self.execution_clock
    }

    /// Returns the configured per-feed execution time limit, if any.
    #[must_use]
    pub fn max_feed_duration(&self) -> Option<Duration> {
        self.limits.max_feed_duration
    }

    /// Returns the configured per-turn execution time limit, if any.
    #[must_use]
    pub fn max_turn_duration(&self) -> Option<Duration> {
        self.limits.max_turn_duration
    }

    /// Returns the configured per-feed step limit, if any.
    #[must_use]
    pub fn max_feed_steps(&self) -> Option<u64> {
        self.limits.max_feed_steps
    }

    /// Returns the configured per-turn step limit, if any.
    #[must_use]
    pub fn max_turn_steps(&self) -> Option<u64> {
        self.limits.max_turn_steps
    }

    /// Returns the deterministic steps taken over the whole session. Nothing is bounded by this.
    #[must_use]
    pub fn steps(&self) -> u64 {
        self.total_steps.get()
    }

    /// Returns the deterministic steps taken by the current feed.
    #[must_use]
    pub fn feed_steps(&self) -> u64 {
        self.feed_steps.get()
    }

    /// Returns the deterministic steps taken by the current host turn.
    #[must_use]
    pub fn turn_steps(&self) -> u64 {
        self.turn_steps.get()
    }

    /// Records `count` steps of work. Only counts; the limits are tested by [`check_time`](Self::check_time),
    /// so the dispatch loop can add steps cheaply and test them at its checkpoint.
    #[inline]
    pub fn add_steps(&self, count: u64) {
        self.total_steps.set(self.total_steps.get().saturating_add(count));
        self.feed_steps.set(self.feed_steps.get().saturating_add(count));
        self.turn_steps.set(self.turn_steps.get().saturating_add(count));
    }

    /// Records `count` steps for one native operation whose work grows with the size of its operands, and
    /// tests the step budgets at once. The caller computes `count` from sizes it already knows, so the
    /// charge is deterministic and the operation is refused before it does its work.
    ///
    /// # Errors
    /// Returns [`ResourceError::Steps`] when a step budget is exceeded.
    #[inline]
    pub fn charge_steps(&self, count: u64) -> Result<(), ResourceError> {
        self.add_steps(count);
        self.check_steps()
    }

    /// Returns whether either step budget is configured.
    #[must_use]
    #[inline]
    pub fn has_step_limit(&self) -> bool {
        self.limits.max_feed_steps.is_some() || self.limits.max_turn_steps.is_some()
    }

    /// Returns the configured memory budget, if any. Hosts that bound a worker
    /// process from outside the interpreter size that bound from this.
    #[must_use]
    pub fn max_memory(&self) -> Option<usize> {
        self.limits.max_memory
    }

    /// Returns the host-enforced suspension budget (default
    /// [`DEFAULT_MAX_SUSPENSIONS`]; never unlimited).
    #[must_use]
    pub fn max_suspensions(&self) -> usize {
        self.limits.max_suspensions
    }

    /// Cumulative sleep limit for the host to enforce, including after restore.
    #[must_use]
    pub fn max_total_sleep(&self) -> Option<Duration> {
        self.limits.max_total_sleep
    }

    /// Returns whether the VM has a memory or time limit configured.
    #[must_use]
    pub fn has_memory_time_limit(&self) -> bool {
        self.limits.max_memory.is_some()
            || self.limits.max_model_memory.is_some()
            || self.has_time_limit()
            || self.has_step_limit()
    }

    /// Returns whether either execution-time budget is configured.
    ///
    /// Public so callers that pay for finer-grained clock polling only when a
    /// budget exists (`fstring`'s incremental large-result path) can ask.
    #[must_use]
    #[inline]
    pub fn has_time_limit(&self) -> bool {
        self.limits.max_feed_duration.is_some() || self.limits.max_turn_duration.is_some()
    }

    /// Sets the per-feed execution limit as a fresh budget from now, resetting
    /// the feed (and so the turn) clock.
    ///
    /// This lets a host enforce a different (typically shorter) time limit
    /// for a resumed phase — e.g. allowing a long build phase, then giving
    /// `repr()` of the result only a few milliseconds. Time spent suspended
    /// in the host never counts toward the budget either way.
    pub fn set_max_feed_duration(&mut self, duration: Duration) {
        self.limits.max_feed_duration = Some(duration);
        self.on_feed_start();
    }

    /// Sets the per-turn execution limit as a fresh budget from now, resetting
    /// the turn clock.
    pub fn set_max_turn_duration(&mut self, duration: Duration) {
        self.limits.max_turn_duration = Some(duration);
        self.on_turn_start();
    }

    /// Checks whether one up-front allocation fits the memory budget.
    ///
    /// Use this before reserving a buffer that could cross both the soft and
    /// hard allocator limits before execution reaches another checkpoint.
    #[inline]
    pub fn check_allocation(&self, additional: usize) -> Result<(), ResourceError> {
        self.check_allocator_memory(additional)?;
        if let Some(limit) = self.limits.max_model_memory {
            // One request larger than the whole limit can be refused before it is made. A request that merely
            // would not fit beside what the session already holds is judged at the next measurement instead:
            // the last one may be stale, and refusing on a stale count could stop a script that fits.
            let used = self.model_used.get().saturating_add(additional);
            if additional > limit {
                return Err(ResourceError::ModelMemory { limit, used });
            }
            self.note_model_growth(additional);
        }
        Ok(())
    }

    /// The allocator-backed part of [`check_allocation`](Self::check_allocation).
    #[inline]
    fn check_allocator_memory(&self, additional: usize) -> Result<(), ResourceError> {
        if let Some(limit) = self.limits.max_memory {
            let used = probe_memory().saturating_add(additional);
            if used > limit {
                return Err(ResourceError::Memory { limit, used });
            }
        }
        Ok(())
    }

    /// Called periodically to check allocator-backed memory and time limits.
    ///
    /// Returns `Ok(())` while configured limits are respected, or the relevant
    /// resource error once either limit is exceeded.
    ///
    /// Takes `&self` rather than `&mut self` because checking elapsed time is a
    /// read-only operation. This allows time checks in contexts that only have
    /// an immutable heap reference, such as `py_repr_fmt`.
    #[inline]
    pub fn check_memory_time(&self) -> Result<(), ResourceError> {
        if let Some(limit) = self.limits.max_memory {
            let used = probe_memory();
            if used > limit {
                return Err(ResourceError::Memory { limit, used });
            }
        }

        self.check_time()
    }

    /// Called periodically to check both execution-time budgets.
    ///
    /// Each clock is monotonic within its scope, so once a budget is exceeded
    /// every later call in that scope fails too. The feed budget is tested
    /// first, since it is the one a new turn cannot recover from.
    #[inline]
    pub fn check_time(&self) -> Result<(), ResourceError> {
        self.check_steps()?;
        if !self.has_time_limit() {
            return Ok(());
        }
        // One clock reading shared by both comparisons.
        let running = self.running_window();
        check_budget(
            TimeLimitScope::Feed,
            self.limits.max_feed_duration,
            self.feed_execution_time.get() + running,
        )
        .and_then(|()| {
            check_budget(
                TimeLimitScope::Turn,
                self.limits.max_turn_duration,
                self.turn_execution_time.get() + running,
            )
        })
    }

    /// Tests both step budgets against the steps taken so far, feed first like the time budgets.
    #[inline]
    fn check_steps(&self) -> Result<(), ResourceError> {
        check_step_budget(TimeLimitScope::Feed, self.limits.max_feed_steps, self.feed_steps.get())
            .and_then(|()| check_step_budget(TimeLimitScope::Turn, self.limits.max_turn_steps, self.turn_steps.get()))
    }

    /// Items processed between full checks in amortized per-item Rust loops
    /// (see [`check_time_every`](Self::check_time_every)). A limit can be
    /// overshot by up to this many items' work before the next check — an
    /// accepted trade for cheap loops; the process-level hard limits backstop
    /// pathological cases.
    pub const LOOP_CHECK_INTERVAL: usize = 64;

    /// Amortized per-item time check for Rust-side loops: a full clock read
    /// once per [`LOOP_CHECK_INTERVAL`](Self::LOOP_CHECK_INTERVAL) calls,
    /// free otherwise. Key `i` on the loop's index or a monotonically
    /// increasing counter. Fires at the *end* of each block (`i % N == N-1`)
    /// so loops shorter than the interval pay no clock read at all — the VM
    /// dispatch checkpoint covers cadence between short calls.
    #[inline]
    pub fn check_time_every(&self, i: usize) -> Result<(), ResourceError> {
        if i % Self::LOOP_CHECK_INTERVAL == Self::LOOP_CHECK_INTERVAL - 1 {
            self.add_steps(Self::LOOP_CHECK_INTERVAL as u64);
            self.check_time()
        } else {
            Ok(())
        }
    }

    /// Amortized per-item memory + time check; the memory-probing sibling of
    /// [`check_time_every`](Self::check_time_every), for loops that allocate
    /// per item. Between full checks the allocator's hard limit still bounds
    /// runaway growth.
    #[inline]
    pub fn check_memory_time_every(&self, i: usize) -> Result<(), ResourceError> {
        if i % Self::LOOP_CHECK_INTERVAL == Self::LOOP_CHECK_INTERVAL - 1 {
            self.add_steps(Self::LOOP_CHECK_INTERVAL as u64);
            self.check_memory_time()
        } else {
            Ok(())
        }
    }

    /// Preflights the reallocation that pushing one more element onto a dense
    /// buffer causes; a push that fits the existing capacity costs nothing.
    ///
    /// A `Vec` charges its whole doubling in one allocation, so a push
    /// straddling the soft limit can land past the allocator's fixed
    /// hard-limit headroom, killing the worker with no checkpoint in between
    /// at which to raise `MemoryError`. Only for the one-push shape: a bulk
    /// reservation needs [`ResourceTracker::check_allocation`] sized for the
    /// whole result, since preflighting less than the final buffer — one
    /// operand of a merge, say — leaves the same window open.
    #[inline]
    pub fn check_growth(&self, len: usize, capacity: usize, elem_size: usize) -> Result<(), ResourceError> {
        self.check_pending_allocation(Self::growth_bytes(len, capacity, elem_size))
    }

    /// The bytes [`check_growth`](Self::check_growth) would preflight, or zero
    /// if the push allocates nothing.
    ///
    /// Split out for containers that grow two buffers on one insertion, such
    /// as a dict's entry vector and index table: checking each increment alone
    /// passes both while their sum clears the headroom, so the caller sums
    /// them and passes the total to
    /// [`check_pending_allocation`](Self::check_pending_allocation).
    #[inline]
    #[must_use]
    pub fn growth_bytes(len: usize, capacity: usize, elem_size: usize) -> usize {
        if len < capacity {
            0
        } else {
            // A buffer growing from nothing jumps straight to
            // `RawVec::MIN_NON_ZERO_CAP`, which is also what stops the
            // increment coming out as zero.
            let min_non_zero_capacity = match elem_size {
                1 => 8,
                2..=1024 => 4,
                _ => 1,
            };
            let new_capacity = capacity
                .saturating_mul(2)
                .max(len.saturating_add(1))
                .max(min_non_zero_capacity);
            new_capacity.saturating_sub(capacity).saturating_mul(elem_size)
        }
    }

    /// [`check_allocation`](Self::check_allocation) for preflights whose
    /// increment may be zero: a push that allocates nothing must not pay for
    /// the usage probe.
    ///
    /// Only the allocator-backed limit sees these. The increments come from buffer capacities (a vector
    /// doubling, a hash table rehashing), which depend on how the buffer got to its size, on the standard
    /// library and on the platform's table layout, so a limit that must not depend on any of those cannot
    /// use them. The deterministic memory limit counts a push by its logical size instead.
    #[inline]
    pub fn check_pending_allocation(&self, additional: usize) -> Result<(), ResourceError> {
        if additional == 0 {
            Ok(())
        } else {
            self.check_allocator_memory(additional)
        }
    }

    /// Returns the configured deterministic memory limit, if any.
    #[must_use]
    pub fn max_model_memory(&self) -> Option<usize> {
        self.limits.max_model_memory
    }

    /// Returns whether the deterministic memory limit is configured.
    #[must_use]
    #[inline]
    pub fn has_model_limit(&self) -> bool {
        self.limits.max_model_memory.is_some()
    }

    /// What the model counted at its last walk of the session, in bytes. Zero before the first walk, and
    /// always zero without a [`max_model_memory`](ResourceLimits::max_model_memory) limit, which is the
    /// only thing that makes the interpreter walk. For the exact current count of a session between feeds,
    /// ask the session.
    #[must_use]
    pub fn model_used(&self) -> usize {
        self.model_used.get()
    }

    /// What the session has told the tracker it took on since the last walk, in bytes: an upper bound on its
    /// growth, never reduced by what was freed.
    #[must_use]
    pub fn model_pending(&self) -> usize {
        self.model_pending.get()
    }

    /// Records that the session took on `bytes` the model will count: a new heap object, or a buffer
    /// about to grow. Only a hint that a walk may be due, and so never wrong by being too large.
    #[inline]
    pub fn note_model_growth(&self, bytes: usize) {
        self.model_pending.set(self.model_pending.get().saturating_add(bytes));
    }

    /// Whether the interpreter should walk the session and report what the model counts, by
    /// [`record_model_walk`](Self::record_model_walk). Reads only counters, so it is cheap.
    ///
    /// A walk is due when what the session could hold now, at the most, is over the limit. That upper bound is
    /// the last walk's count, plus what the session has since told the tracker it took on (every new heap
    /// object, every bulk allocation), plus a fixed allowance per step for the one thing it is not told about:
    /// a push into a buffer that already has room. Frees are not subtracted, so the bound can only be too
    /// high, which costs a walk that finds nothing, never a missed limit.
    ///
    /// Walks are also spaced: at least 256 steps apart, and at least half as many steps as there are heap
    /// slots, so that walking costs a bounded share of the script's time however large the heap is. The one
    /// exception is a bulk allocation of an eighth of the limit or more, which is measured as soon as it has
    /// happened.
    ///
    /// All of it is counters the program produced, so a walk falls at the same point on every machine.
    #[must_use]
    #[inline]
    pub fn model_walk_due(&self) -> bool {
        let Some(limit) = self.limits.max_model_memory else {
            return false;
        };
        let pending = self.model_pending.get();
        let used = self.model_used.get();
        if pending >= limit / MODEL_WALK_BULK_DIVISOR && used.saturating_add(pending) > limit {
            return true;
        }
        let since = self.total_steps.get().saturating_sub(self.model_walk_steps.get());
        let spacing = MODEL_WALK_MIN_INTERVAL
            .max(u64::try_from(self.model_walk_entries.get()).unwrap_or(u64::MAX) / MODEL_WALK_SLOTS_PER_STEP);
        if since < spacing {
            return false;
        }
        let in_place = usize::try_from(since.saturating_mul(MODEL_IN_PLACE_BYTES_PER_STEP)).unwrap_or(usize::MAX);
        used.saturating_add(pending).saturating_add(in_place) > limit
    }

    /// Records a walk that found the session worth `used` bytes by the model across `entries` heap slots:
    /// charges the walk to the step count, restarts the schedule, and tests the limit.
    ///
    /// # Errors
    /// Returns [`ResourceError::ModelMemory`] when `used` is over the limit.
    pub fn record_model_walk(&self, used: usize, entries: usize) -> Result<(), ResourceError> {
        // One step for every few slots visited, so a script that keeps a large heap near its limit pays for
        // the measuring in the budget that bounds its work.
        self.add_steps(u64::try_from(entries / MODEL_WALK_SLOTS_PER_CHARGED_STEP).unwrap_or(u64::MAX));
        self.model_used.set(used);
        self.model_pending.set(0);
        self.model_walk_steps.set(self.total_steps.get());
        self.model_walk_entries.set(entries);
        match self.limits.max_model_memory {
            Some(limit) if used > limit => Err(ResourceError::ModelMemory { limit, used }),
            _ => Ok(()),
        }
    }

    /// Called before pushing a new call frame to check recursion depth.
    ///
    /// Returns `Ok(())` if within recursion limit, or `Err(ResourceError::Recursion)`
    /// if the limit would be exceeded. `current_depth` is the call stack depth
    /// before the new frame is pushed.
    #[inline]
    pub fn check_recursion_depth(&self, current_depth: usize) -> Result<(), ResourceError> {
        let limit = self.active_recursion_limit();
        // current_depth is before push, so new depth would be current_depth + 1
        if current_depth >= limit {
            return Err(ResourceError::Recursion {
                limit,
                depth: current_depth + 1,
            });
        }
        Ok(())
    }

    /// Called before operations that may produce large results (>100KB).
    ///
    /// This allows pre-emptive rejection of operations like `2 ** 10_000_000`
    /// before the memory is actually allocated. The check only happens for
    /// estimated result sizes above [`LARGE_RESULT_THRESHOLD`] to avoid overhead
    /// on small operations.
    #[inline]
    pub fn check_large_result(&self, estimated_bytes: usize) -> Result<(), ResourceError> {
        self.check_allocation(estimated_bytes)
    }

    /// Returns the configured garbage collection interval, in GC-tracked
    /// allocations.
    ///
    /// The cycle collector runs at most once per `gc_interval` GC-tracked
    /// allocations, and additionally short-circuits when no cycle candidates
    /// are pending — so programs that never form cycles pay no collector
    /// cost regardless of their allocation rate. `None` tells the heap to use
    /// its built-in default scheduling threshold.
    #[must_use]
    #[inline]
    pub fn gc_interval(&self) -> Option<usize> {
        self.limits.gc_interval
    }

    /// Called when the VM enters its execution loop from a host boundary
    /// (`VM::run_external`), starting one execution window.
    ///
    /// Paired with [`on_execution_stop`](Self::on_execution_stop) and never
    /// nested — VM-internal re-entry (task switches, host-initiated function
    /// evaluation) uses the raw run loop, so its time falls inside the
    /// enclosing window. The execution-time clock runs between the pair; it is
    /// *not* running while execution is suspended waiting on the host
    /// (external function calls) or between feeds.
    pub fn on_execution_start(&self) {
        debug_assert!(
            self.running_since.get().is_none(),
            "nested on_execution_start: VM-internal re-entry must use the raw run loop, not run_external"
        );
        let remaining = [
            (self.limits.max_feed_duration, self.feed_execution_time.get()),
            (self.limits.max_turn_duration, self.turn_execution_time.get()),
        ]
        .into_iter()
        .filter_map(|(limit, used)| limit.map(|limit| limit.saturating_sub(used)))
        .min();
        self.running_since
            .set(Some(ExecutionInstant::now(self.execution_clock, remaining)));
    }

    /// Called when the VM leaves its execution loop — on completion, error,
    /// or suspension at an external call. See [`on_execution_start`](Self::on_execution_start).
    pub fn on_execution_stop(&self) {
        if let Some(mut started) = self.running_since.take() {
            let window = started.elapsed_at_stop();
            for clock in [
                &self.total_execution_time,
                &self.feed_execution_time,
                &self.turn_execution_time,
            ] {
                clock.set(clock.get() + window);
            }
        }
    }

    /// Called when the host begins a new feed, resetting the `max_feed_duration`
    /// budget (and, since a feed opens a turn, the `max_turn_duration` one).
    ///
    /// A feed spans every turn its snippet takes, so this must fire only at
    /// the snippet's first turn — not at the resumes that continue it.
    pub fn on_feed_start(&self) {
        self.feed_execution_time.set(Duration::ZERO);
        self.feed_steps.set(0);
        self.on_turn_start();
    }

    /// Called when the host hands control back to the interpreter — at a feed
    /// or a resume — resetting the `max_turn_duration` budget.
    ///
    /// Continuations the VM resolves without the host (an already-settled
    /// future, a task switch) stay inside the turn that started them.
    pub fn on_turn_start(&self) {
        self.turn_execution_time.set(Duration::ZERO);
        self.turn_steps.set(0);
    }

    /// Performs a standard-execution sleep without charging `max_feed_duration`.
    /// Restarts the execution clock only if it was running before the wait.
    /// All interpreter waits use `block_for` for platform-specific blocking.
    pub fn sandbox_sleep(&self, duration: Duration) {
        let was_running = self.running_since.get().is_some();
        self.on_execution_stop();
        block_for(duration);
        if was_running {
            self.on_execution_start();
        }
    }

    /// Lowers the live recursion ceiling to `new_limit`, refusing to raise it.
    ///
    /// Exposed under the `test-hooks` feature so `sys.setrecursionlimit` can
    /// tighten the depth ceiling from inside fixture code. The constructed
    /// limit (`limits.max_recursion_depth`) acts as the hard upper bound —
    /// raising it would let sandboxed code escape the host-imposed safety
    /// bound. A `new_limit` above the active ceiling is rejected with
    /// `Err(current)`, which callers surface as a `ValueError` in the
    /// Python layer.
    #[cfg(feature = "test-hooks")]
    pub fn lower_recursion_limit(&self, new_limit: usize) -> Result<(), usize> {
        let limit = self.active_recursion_limit();
        if new_limit > limit {
            return Err(limit);
        }
        self.recursion_limit_override.set(Some(new_limit));
        Ok(())
    }
}

/// Combines optional ceilings, where an absent limit permits any value.
fn tighter_limit<T: Ord>(existing: Option<T>, ceiling: Option<T>) -> Option<T> {
    match (existing, ceiling) {
        (Some(existing), Some(ceiling)) => Some(existing.min(ceiling)),
        (existing, ceiling) => existing.or(ceiling),
    }
}

/// [`tighter_limit`] for `gc_interval`, where `None` is [`DEFAULT_GC_INTERVAL`], not unlimited.
fn tighter_gc_interval(existing: Option<usize>, ceiling: Option<usize>) -> Option<usize> {
    let effective = existing
        .unwrap_or(DEFAULT_GC_INTERVAL)
        .min(ceiling.unwrap_or(DEFAULT_GC_INTERVAL));
    existing.or(ceiling).map(|_| effective)
}

/// Reports `elapsed` against one optional budget, naming `scope` in the error.
#[inline]
fn check_budget(scope: TimeLimitScope, limit: Option<Duration>, elapsed: Duration) -> Result<(), ResourceError> {
    match limit {
        Some(limit) if elapsed > limit => Err(ResourceError::Time { scope, limit, elapsed }),
        _ => Ok(()),
    }
}

/// Reports `used` steps against one optional budget, naming `scope` in the error.
#[inline]
fn check_step_budget(scope: TimeLimitScope, limit: Option<u64>, used: u64) -> Result<(), ResourceError> {
    match limit {
        Some(limit) if used > limit => Err(ResourceError::Steps { scope, limit, used }),
        _ => Ok(()),
    }
}

/// Returns memory used in bytes
fn probe_memory() -> usize {
    LIVE_MEMORY
        .load(Ordering::Relaxed)
        .saturating_sub(BASELINE_MEMORY.load(Ordering::Relaxed))
}

#[cfg(not(target_family = "wasm"))]
fn block_for(duration: Duration) {
    thread::sleep(duration);
}

/// Blocks for `duration` on wasm by spinning on the monotonic clock:
/// `std::thread::sleep` needs `wasi:io/poll`, which a browser host serves
/// only asynchronously. Only standard execution waits here; the wasm worker
/// suspends its sleeps to the JavaScript host instead.
#[cfg(target_family = "wasm")]
fn block_for(duration: Duration) {
    let started = Instant::now();
    while started.elapsed() < duration {
        hint::spin_loop();
    }
}
