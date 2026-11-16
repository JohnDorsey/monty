//! Floating-point functions whose results do not depend on the platform.
//!
//! Rust's `f64::sin`, `f64::exp`, `f64::powf` and friends call the C library of the target
//! (glibc, the Windows UCRT, musl, Apple's libm). Those libraries are allowed to differ by a
//! few units in the last place, and they do: a script that prints `math.sin(x)` can give a
//! different last digit on Windows than on Linux, and a seeded `random.gauss` stream diverges
//! the same way. Every transcendental function the interpreter needs therefore goes through
//! this module, which forwards to the `libm` crate, a pure Rust port that runs the same
//! instructions on every platform.
//!
//! `sqrt`, `floor`, `ceil`, `trunc`, `abs`, `copysign` and `%` are exactly rounded or exact by
//! definition, so they are not wrapped. `f64::powi` is not wrapped either: it is
//! repeated multiplication supplied by `compiler-builtins`, not by the C library.
//!
//! `clippy.toml` bans the platform methods listed here (`disallowed-methods`), so a new call
//! site cannot bring the platform back unnoticed.
//!
//! The values are those of the pinned `libm` version: upgrading the crate can change last
//! digits, so a `Cargo.lock` bump of `libm` is a behaviour change to be noted in a changelog.

/// Sine of `x` (radians).
pub(crate) fn sin(x: f64) -> f64 {
    libm::sin(x)
}

/// Cosine of `x` (radians).
pub(crate) fn cos(x: f64) -> f64 {
    libm::cos(x)
}

/// Tangent of `x` (radians).
pub(crate) fn tan(x: f64) -> f64 {
    libm::tan(x)
}

/// Arc sine of `x`.
pub(crate) fn asin(x: f64) -> f64 {
    libm::asin(x)
}

/// Arc cosine of `x`.
pub(crate) fn acos(x: f64) -> f64 {
    libm::acos(x)
}

/// Arc tangent of `x`.
pub(crate) fn atan(x: f64) -> f64 {
    libm::atan(x)
}

/// Four-quadrant arc tangent of `y / x`.
pub(crate) fn atan2(y: f64, x: f64) -> f64 {
    libm::atan2(y, x)
}

/// Hyperbolic sine of `x`.
pub(crate) fn sinh(x: f64) -> f64 {
    libm::sinh(x)
}

/// Hyperbolic cosine of `x`.
pub(crate) fn cosh(x: f64) -> f64 {
    libm::cosh(x)
}

/// Hyperbolic tangent of `x`.
pub(crate) fn tanh(x: f64) -> f64 {
    libm::tanh(x)
}

/// Inverse hyperbolic sine of `x`.
pub(crate) fn asinh(x: f64) -> f64 {
    libm::asinh(x)
}

/// Inverse hyperbolic cosine of `x`.
pub(crate) fn acosh(x: f64) -> f64 {
    libm::acosh(x)
}

/// Inverse hyperbolic tangent of `x`.
pub(crate) fn atanh(x: f64) -> f64 {
    libm::atanh(x)
}

/// `e` raised to `x`.
pub(crate) fn exp(x: f64) -> f64 {
    libm::exp(x)
}

/// 2 raised to `x`.
pub(crate) fn exp2(x: f64) -> f64 {
    libm::exp2(x)
}

/// `e` raised to `x`, minus one, accurate for small `x`.
pub(crate) fn exp_m1(x: f64) -> f64 {
    libm::expm1(x)
}

/// Natural logarithm of `x`.
pub(crate) fn ln(x: f64) -> f64 {
    libm::log(x)
}

/// Natural logarithm of `1 + x`, accurate for small `x`.
pub(crate) fn ln_1p(x: f64) -> f64 {
    libm::log1p(x)
}

/// Base 2 logarithm of `x`.
pub(crate) fn log2(x: f64) -> f64 {
    libm::log2(x)
}

/// Base 10 logarithm of `x`.
pub(crate) fn log10(x: f64) -> f64 {
    libm::log10(x)
}

/// `base` raised to `exponent`, with the special cases of C99 `pow`.
pub(crate) fn powf(base: f64, exponent: f64) -> f64 {
    libm::pow(base, exponent)
}

/// Cube root of `x`, defined for negative `x`.
pub(crate) fn cbrt(x: f64) -> f64 {
    libm::cbrt(x)
}

/// Length of the hypotenuse of a right triangle with legs `x` and `y`.
pub(crate) fn hypot(x: f64, y: f64) -> f64 {
    libm::hypot(x, y)
}

/// `a * b + c` with a single rounding, in software when the hardware has no fused instruction.
pub(crate) fn mul_add(a: f64, b: f64, c: f64) -> f64 {
    libm::fma(a, b, c)
}
