# Voxunami fork of Monty

This fork of [pydantic/monty](https://github.com/pydantic/monty) exists for one reason: **scripts must give the same bits on
every platform.** Voxunami's simulation must be bit-identical on Windows and Linux, and a script that prints `math.sin(x)` or
draws `random.gauss()` may feed the simulation.

Upstream's `math` module, `**` on floats, complex powers and `random` call Rust's `f64::sin`, `f64::exp`, `f64::powf` and
friends, which call the C library of the target (glibc, the Windows UCRT, Apple's libm). Those differ in the last place for a
few percent of inputs. Measured on 2026-11-14 with 5,000 random inputs of 21 functions, Windows against glibc: 6.2 percent of
results differed (`sin` 3.2, `cos` 3.5, `exp` 0.6, `pow` 0.2, `sinh`/`cosh`/`cbrt` about 27). A hand-picked probe of 11
inputs showed no difference at all, so the tests here use thousands of generated inputs.

## What this branch changes

Branch `deterministic-math`, based on tag `v1.6.0`.

- `crates/monty/src/portable_math.rs` (new): one function per platform-dependent operation, each forwarding to the pure Rust
  `libm` crate (`sin cos tan asin acos atan atan2 sinh cosh tanh asinh acosh atanh exp exp2 exp_m1 ln ln_1p log2 log10 powf
  cbrt hypot mul_add`). `mul_add` is `libm::fma`, which is exact on every target.
- Every call site in `math`, `random`, complex numbers, `**` on floats and the f-string digit count now calls it.
- `clippy.toml` bans the 25 platform methods (`disallowed-methods`), so a new call site cannot reintroduce the platform. Two
  sites outside the sandbox (a duration printer in `monty-runtime`, telemetry JSON in `monty-pool`) are exact integer
  arithmetic and carry a reason.
- `crates/monty/tests/portable_math.rs` (new): every one-argument `math` function, `log` with and without a base,
  `pow`, `**`, `atan2`, `hypot`, `fma` compared bit for bit with `libm` over 6,000 generated inputs each, plus a golden test of
  seeded `random` distributions and complex powers.

A third change, a deterministic memory limit, is described under "Third change" below.

## Second change: a deterministic step budget (phases 0 to 3 of the plan)

A time limit cuts a script off at a different instruction on a slow machine than on a fast one. A **step budget** counts
work instead, so the cut-off is the same everywhere. Plan and reasons: `docs/monty-step-budget-plan.md` in the Voxunami
repository.

- `ResourceLimits::max_feed_steps` and `max_turn_steps` (builders of the same names). Exceeding one raises an uncatchable
  `TimeoutError` ("turn step limit exceeded: N steps > M steps"), like the time limits. Time limits stay.
- A step is one bytecode instruction, plus 64 for every 64 items a native loop works through (`check_time_every`). The run
  loop charges its instructions at each checkpoint and again when it returns, so a short callback called from native code
  (a sort key, a `map` function, `__eq__` in a list search) is counted too; without that charge a quarter to a third of
  such work went uncounted (measured).
- `ResourceTracker::steps()`, `feed_steps()`, `turn_steps()` read the counts; they count even when no limit is set. The
  feed and session counts are saved in a dump; the turn count starts again, like the turn time. A dump written before this
  change still loads (the fields have defaults).
- The count depends on the bytecode compiler: a Monty release that compiles differently changes every script's count. The
  three pinned counts in `crates/monty/tests/step_budget.rs` (`GOLDEN_STEPS`) show it at once.
- **Over the wire (phase 5):** see "Step budgets over the worker protocol" below.
- Cost, measured on Windows with a release build: tight loops (`fib(27)`, a 5M-iteration loop) within 1 percent of
  unpatched; a callback-heavy sort (300,000 key calls) about 6 percent slower, the price of charging every nested call.

### Native operations that do a lot of work in one call (phase 4)

A loop polls the budget, but one call such as `'x' * 10**8`, `text.upper()`, `list.insert(0, x)`, `a * a` on a big
integer or `Decimal ** n` at high precision does its work between two polls, so a script could repeat it a million times
inside a budget. Measured on 2026-11-16 (about 100 operations and about 300 method calls, results in the Voxunami
repository under `docs/history/spikes/scripting/`): a plain instruction costs about 4 ns a step, and these operations
cost 1 to 3,000,000 ns per step before the charges. `crates/monty/src/native_work.rs` charges each one, before it does
its work, from sizes known in constant time, so the charge is the same on every machine:

- text and bytes: 1 step per 16 bytes. Methods, `+`, `*`, comparisons, slices, `hash`, `repr`, and the text arguments of
  `hashlib`, `base64`, `binascii`, `unicodedata`, `re`, `json`, `fnmatch`, `tomllib`, `csv`. A search (`find`, `index`,
  `rfind`, `rindex`) is charged after the call for the stretch it scanned; `in` on text charges the whole haystack;
- containers: 1 step per item copied or hashed (`copy`, `a + b`, `a * n`, `set(s)`, `{**d}`, set algebra); 1 step per 16
  items moved (`insert`, `pop(i)`, `reverse`, `clear`, `deque.rotate`);
- big integers, in 64-bit limbs on every platform: `limbs_a * limbs_b / 1024` for `*` and `**`; quotient limbs times
  divisor limbs over 256 for `//`, `%`, `divmod`; `limbs^2 / 8` for `gcd`; `/ 128` for `isqrt`; per exponent bit
  `modulus_limbs^2 / 128` for three-argument `pow` (which now also stops partway when a budget runs out); a shift costs
  1 step per 16 limbs; `comb` and `perm` charge `k * limbs / 32`;
- `Decimal`: `p^2 * isqrt(p) / 600` for `exp`, `ln`, `log10` and powers, `p^2 / 2000` for `sqrt` (p = the context
  precision), which is what 250 to 2000 digits measured and what 20,000 digits confirmed: a power at precision 20,000
  took 15.6 s for 94 million steps, 166 ns each.

Sizes below a rate divide down to zero, so ordinary scripts keep exactly the counts they had (the golden counts in
`tests/step_budget.rs` did not move). Tests: `tests/native_work.rs`, `native_work_numbers.rs`, `native_work_decimal.rs`;
each failed before its charge existed.

Known gaps: `re.sub` on a large string takes more than linear time (1 MB about 2 s, 4 MB 40 to 70 s) for a reason not
found, probably in `fancy-regex` or its `captures_iter`; the text charge makes a budget stop a loop of such calls, but one
huge call cannot be interrupted. List and tuple slices, `bytes` slices, `list.index`, `sorted` and similar already polled
per item and are not charged twice.

### Step budgets over the worker protocol (phase 5)

`monty-pool` drives worker processes over the protobuf protocol of `monty-proto`, so the budgets have to cross it.

- `ResourceLimits` in `monty.proto` gains `optional uint64 max_feed_steps = 9` and `max_turn_steps = 10`; unset means
  unlimited, and an unset field is not encoded, so a client that sets neither sends the bytes it always did. The
  checked-in generated code (`src/generated`, `tests/oracle`) is regenerated with `make generate-proto`, which needs a
  nightly rustfmt for the import style.
- The worker enforces the budgets alone. There is no client-side backstop and no echo on `WorkerEvent`: a step budget is
  deterministic, so there is nothing for the pool to supervise the way it supervises a time budget on a stalled worker.
  A restored session keeps the budget in its dump and the worker's own `Configure` can only tighten it.
- `PROTOCOL_VERSION` is 10 (upstream main uses 9 for an unrelated change). Only the worker checks versions, so this is
  what makes an old worker refuse a new client: an old worker would otherwise ignore the unknown fields and run the script
  with no step budget at all. `MIN_SUPPORTED_PROTOCOL_VERSION` stays 7. **If upstream later uses 10 for something else,
  renumber ours when merging**, and keep the rule that a client sending step limits must declare a version that a worker
  without them rejects.
- Options: `--max-feed-steps` and `--max-turn-steps` on the standalone `monty` CLI; `max_feed_steps` and `max_turn_steps` in
  the Python `ResourceLimits` dict; `maxFeedSteps` and `maxTurnSteps` in the JavaScript `ResourceLimits`.
- **Not supported:** the WebAssembly component interface (WIT) has no step fields, so a wasm session runs without them;
  the JavaScript wasm transport throws if either is set rather than dropping it.
- Tests: wire round trip and field independence (`monty-proto/tests/roundtrip.rs`); a configured worker stops a runaway
  feed, two workers stop at the same count, a restored session keeps and can only tighten its budget
  (`monty-proto/tests/dispatch.rs`); two real worker processes through `monty-pool` stop a script at the same step
  (`monty-pool/tests/pool_test.rs`). The Python and JavaScript tests were written but not run (they need the extension
  and node builds).

## Third change: a deterministic memory limit, `max_model_memory`

`max_memory` reads the bytes the whole process has requested from its global allocator, so the step at which it stops a
runaway script moves with anything else that allocates. Measured on Windows (a debug build, `max_memory` of 1 MB under
`monty-alloc`, the step at which `MemoryError` fired for a script appending small lists):

| Condition                                          | Step              |
|----------------------------------------------------|-------------------|
| Quiet process, four runs, two processes            | 55,296 every time |
| Another thread allocating and freeing 20-170 KB    | 32,776 to 41,216  |
| The host grew by 300 KB after the limit was armed  | 36,864            |

Without `monty-alloc` as the global allocator (an embedding in a game, which cannot give its allocator to Monty) the limit
only refuses single oversized requests and never sees a loop that keeps small objects.

So there is a second limit beside it, not a replacement: `ResourceLimits::max_model_memory` (`max_model_memory` in
Python, `maxModelMemory` in JavaScript, `--max-model-memory` on the CLI). It leaves `max_memory` alone, which stays the
allocator-backed hard backstop in a worker and costs upstream merges nothing. The two can be set together.

### What it counts

A fixed cost table over the logical size of every live heap object (`crates/monty/src/heap/model.rs`; the prices are
pinned by `tests/model_memory_costs.rs`):

| Object                         | Price in bytes                                                                |
|--------------------------------|-------------------------------------------------------------------------------|
| any heap object                | 96, plus its contents                                                         |
| `str`, `bytes`, `Path`         | its length in bytes                                                           |
| `list`, `tuple`, `deque`       | 16 per item (the length, not the spare capacity)                              |
| `dict`, and instance, class, module attributes | 72 per entry                                                  |
| `set`, `frozenset`             | 40 per element                                                                |
| integer too big for `i64`; `Decimal` | 8 per 64-bit word                                                       |
| boxed payload (instance, named tuple, partial, cache, match, class, module) | 112 more                         |
| closure                        | 8 per captured cell, 16 per default                                           |
| `lru_cache`                    | 56 per remembered call                                                        |
| compiled regular expression    | 2,048 plus 64 per character of the pattern                                    |
| `random.Random`                | 2,560                                                                         |
| buffering `itertools` adaptor  | 16 per value it holds (`cycle`, `tee`, `product`, `combinations`, `permutations`) |
| globals and stack              | 16 per slot; 128 per call frame                                               |
| source compiled by `eval`/`exec` | 640 per call plus 12 per byte of source, kept for the session; none if it does not compile |

Small ints, floats, bools, `None` and strings short enough to live inside a value cost nothing beyond their slot. An
object that two names point to is priced once. Garbage that nothing points to stops counting when it is freed; an
unreachable cycle counts until the collector runs, as it does for the allocator.

`HeapData::model_bytes` matches every payload type with no wildcard arm, so a new type cannot be added without being priced.
A unit test checks that the two prices that are meant to be real layouts (the entry and the value) still are.

### When it is measured

At the dispatch checkpoint, every 256 instructions, and at the end of every host turn, a counter-only test says whether a
walk of the heap is due. A walk is due when the last count, plus what the session has told the tracker it took on since
(every new heap object, every pre-checked bulk allocation), plus 12 bytes for each step taken (for pushes into a buffer
that already has room, which nothing reports), could be over the limit. Walks are spaced at least 256 steps and half as
many steps as there are live objects apart, so walking costs a bounded share of the run whatever the heap size, except
that a bulk allocation of an eighth of the limit is measured at once. A walk is charged to the step budget at one step per
four live objects. A single request larger than the whole limit is refused before it is made.

Every input to that decision is a count the program produced (steps, bytes by the table, live objects), so a walk falls at
the same point on every machine.

### What it guarantees and what it does not

- **The step at which a script stops is a function of the program and its inputs.** The same on every platform that
  builds this table, whatever else the process does, in any worker, after a dump and restore (the schedule is part of
  the saved tracker). Measured: five runaway scripts stop at pinned steps (`runaways_stop_at_pinned_steps`), identically
  in four threads at once beside a thread that allocates and frees all the while, in separate runs, and in two real worker
  processes behind `monty-pool`.
- **It is a convention, not a measurement.** Against the allocator (`LIVE_MEMORY` growth, Windows, 64-bit, debug build) the
  model ran from 0.75 to 0.97 of real use and never above it: 10,000 small lists 0.85, 10,000 strings 0.89, a 10,000-entry
  dict 0.86, 1,000 instances 0.97, 10,000 tuples 0.88, a 10,000-element set 0.94, 1,000 big integers 0.93, a 1 MB string
  0.96, a list of 100,000 ints 0.75. The shortfall is mostly spare capacity in lists grown by pushes (up to twice the
  length), which the model ignores on purpose because capacity depends on how a buffer grew and on the standard library.
  Real use can be up to about twice the model's count, plus the allocator's own overhead. Keep `max_memory` armed in a
  worker if the process itself must stay bounded.
- **The limit can be passed before the script is stopped.** A heap of a few hundred objects is measured every 256 steps
  and passed by a few kilobytes at most. A heap of 200,000 objects is measured every 100,000 steps, so a runaway that
  adds 18 bytes a step could pass the limit by about 5 percent (measured: 1.4 percent of 40 MB). By the constants,
  the worst case is an eighth of the limit for what the session was told about, plus about a sixteenth of the session's
  size for what it was not (read from the constants, not measured as a bound).
- **Spiking inside one instruction is not seen.** The count is taken between instructions, so a call that holds a large
  temporary while it runs is only seen if the temporary is still there afterwards.
- **Source compiled by `eval` and `exec` is counted**, because each call keeps its source and code for the life of the
  session and shares nothing with the last (2,000 `exec('x = 1')` kept 1.29 MB, measured, while the model saw 16 bytes).
  Measured against the allocator the price is 0.95 to 1.08 of real growth (2,000 `exec`s of one tiny source, of distinct
  tiny sources, of 200-byte sources, and of `eval`). The code the host feeds is the host's to bound and is not counted.
- **Not counted:** the host's print buffers, the operand stack's spare capacity, imported module files, and the contents of `csv`, `os.scandir` and open-file objects, which are priced as fixed objects (96 bytes,
  or 208 when the payload is boxed).
- **Capacity must never reach the schedule.** The growth pre-checks that come from buffer capacities (`check_growth`,
  `check_pending_allocation`) are deliberately kept out of the model: a restored list has capacity equal to its length,
  a grown one does not, and an early version that counted them stopped a restored session 80 steps later than the
  original. Likewise a walk is scheduled by the number of live objects, not heap slots, which a restore compacts.

### Cost

Release build, Windows, median of seven runs, against a clean checkout of the commit before this change (nanoseconds per
step): a plain arithmetic loop 4.20 before and 4.23 after with no limit set, 4.24 with a model limit far away; an
allocating loop 13.04 and 12.50 and 12.54; the same over a live heap of 100,000 lists 12.95, 12.79 and 13.28. With a
100,000-object heap kept within 15 percent of the limit the run took 33 percent longer, the price of measuring a large
heap often because it is near the limit.

### Over the worker protocol

`ResourceLimits` gains `optional uint64 max_model_memory = 11`, converted both ways (saturating on 32-bit, never dropping
the limit) and enforced by the worker alone, with no echo. `PROTOCOL_VERSION` is 11 so that a worker built before the
field existed refuses a client that sends it instead of running the script unlimited; it must be renumbered at a merge if
upstream takes 11. The WebAssembly component interface has no field for it, so a wasm session runs without it and the
JavaScript wasm transport throws if it is set.

Tests: `tests/model_memory_costs.rs` (17, the price of each kind of object), `tests/model_memory_limit.rs` (17, stopping
points, independence from the process, overshoot, garbage, shrinking in place, restore equivalence, tightening), the wire
round trip, the worker dispatch (the same message in three workers; a restored session keeps and tightens it), two real
worker processes behind `monty-pool`, and the Python and JavaScript limits tests.

## Verification status (2026-11-16)

- Windows, after the memory limit: `monty`, `monty-types` and `monty-runtime` 1,497 tests, `monty-proto` (with the
  `worker` feature) 156, `monty-pool` 195, none failing, including the golden step counts. Clippy (denying warnings) and
  nightly rustfmt are clean for those crates.
- Linux (WSL), the same: `monty`, `monty-types`, `monty-runtime` 1,526 passed and 3 failed, `monty-proto` 156, `monty-pool`
  248. The three failures are the compile-fail snapshot tests `readonly_mutable_projection`, `smuggle_and_swap_reader` and
  `smuggle_heap_read` in `heap_reader_compile_fail` (Linux only, and not run under `memory-model-checks`, which is how the
  earlier Linux runs were configured): they fail identically on an untouched copy of upstream `main` with Rust 1.99, which
  prints one more "this error originates in the macro" note than the stored snapshots have. This change does not move the
  line numbers those snapshots print (`mod model` is declared at the bottom of `heap/mod.rs` for that reason).
- The pinned stopping steps of the memory limit (`runaways_stop_at_pinned_steps`) were read on Windows and hold on Linux.
- Python and JavaScript, run for the first time on 2026-11-16 (`uv sync`, `maturin develop`, `npm install`). On Linux the
  whole Python suite passes (1,743 passed, 15 skipped) and the whole JavaScript vitest suite passes (772 passed, 3
  skipped), the step and memory limit tests among them. On Windows `test_limits.py` has 37 passing and the JavaScript suite
  passes; the whole Python suite there has 56 failed and 82 errors, all in filesystem, path, mount, zoneinfo and
  README-example tests, none in limits, and none on Linux, so they are the Windows environment.
- Running the Python suite for the first time found a miss of phase 5: `test_remote.py` kept a snapshot of the accepted
  limit keys that lacked the step keys. It lists the memory key too now.
- Mutation experiments on the memory limit's tests: 12 rules broken one at a time, each noticed by at least one test; one
  rule (measure a bulk allocation at once) is noticed only by a unit test, because step charges make it redundant in
  practice. Results are in the Voxunami repository, `docs/history/spikes/scripting/monty_mutation_results_2026-11-16.txt`.
- Known failures that are not ours, each reproduced on a pristine copy: `monty-proto`'s `dispatch` test binary aborts with
  "`Value::Ref` dropped without calling `drop_with()`" in `async_calls_suspend_without_accepting_streamed_calls` when
  `monty` is built with `memory-model-checks` (upstream `main`, Windows and Linux; run that binary without the feature).
  `monty-fs` fails clippy `unnecessary_wraps` on Windows and `monty-pool`'s tests have Windows-only unused imports.
- A test that builds its own big numbers must stay small: test builds are unoptimized, and an early modular-power case ran
  past a 20 second safety net on Linux.

## Tracking upstream

The branch was cut from the `v1.6.0` tag. Upstream `main` had moved 36 commits by 2026-11-16 with no newer release tag.
`deterministic-math` now follows upstream `main` (owner's decision 2026-11-16): it merged it (three small conflicts) and the
golden step counts did not move, so the compiler is unchanged. One thing the merge changed: upstream's new shared table copy (`hash_table.rs`, `copy_table`) polls the
tracker once per entry, so the method-level charges for dict and set copies and set algebra were dropped there to avoid
counting them twice; on the `v1.6.0` line they are still needed. Count drift is the risk of following upstream: watch
`GOLDEN_STEPS` in `tests/step_budget.rs` at every merge.

## What is deliberately not changed

- `sqrt`, `floor`, `ceil`, `trunc`, `abs`, `copysign`, `%` are exact by definition.
- `f64::powi` is repeated multiplication supplied by `compiler-builtins`, not the C library.
- `math.gamma`, `lgamma`, `erf`, `erfc` already used `libm` upstream.
- `Decimal` has its own arithmetic.

## Things to know

- The results are those of the `libm` version in `Cargo.lock`. The crate is not always the closest-rounded answer: it differs
  from the platform libraries in the last place on many inputs (for example `atanh` on about 39 percent of random inputs against
  the Windows UCRT). That is the price of one answer everywhere. A `libm` upgrade can change last digits, so the embedding
  project should pin it.
- Where CPython prints a different last digit than this fork for `math.sin(x)`, this fork is the one that is the same
  everywhere.
- To update: fetch upstream, check out the new release tag as a new branch, re-apply with `git cherry-pick` of the commits of
  this branch, and run `cargo test -p monty --features memory-model-checks`.

The same fix could be offered upstream as an optional feature; it is not a Voxunami-specific change.
