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
- **Not done yet:** the limits are not carried over the worker pool's wire format (`monty-proto`), so a worker behind
  `monty-pool` runs without a step budget until phase 5.
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
