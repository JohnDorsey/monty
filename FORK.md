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
