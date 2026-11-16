//! Big-integer arithmetic does super-linear work in one native call (a multiplication of two numbers of n
//! limbs, a division, a modular power), so it is charged by the sizes of its operands in 64-bit limbs, the same
//! on every platform. Rates, from measurements: `limbs_a * limbs_b / 1024` for a multiplication or power,
//! quotient limbs times divisor limbs over 256 for a division, `limbs * limbs / 8` for `gcd`, `/ 128` for
//! `isqrt`, per exponent bit `modulus_limbs^2 / 128` for a modular power, and one step per 16 limbs for a shift.

mod step_support;

use step_support::{assert_step_error, charged, limits, run_probed};

fn assert_at_least(setup: &str, operation: &str, expected: u64) {
    let steps = charged(setup, operation);
    assert!(
        steps >= expected,
        "`{operation}` charged {steps} steps, expected at least {expected}"
    );
}

/// The charge is within a tenth below `expected` (the model counts limbs of the exact bit lengths) and at most
/// four times above it (other charges inside the operation, estimates that round bit counts up).
fn assert_about(setup: &str, operation: &str, expected: u64) {
    let steps = charged(setup, operation);
    assert!(
        steps >= expected * 9 / 10 && steps <= expected * 4 + 100,
        "`{operation}` charged {steps} steps, expected about {expected}"
    );
}

#[test]
fn multiplication_powers_and_division_charge_by_limbs() {
    // 3**100000 has 158,496 bits: 2,477 limbs.
    assert_about("", "x = 3 ** 100000", 2_477 * 2_477 / 1024);
    assert_about("a = 3 ** 100000", "x = a * a", 2_477 * 2_477 / 1024);
    // 3**200000 has 316,993 bits (4,953 limbs), 3**100000 has 2,477: the quotient has 2,477 limbs.
    let division = 2_477 * 2_477 / 256 + 4_953 / 32;
    let setup = "a = 3 ** 200000\nb = 3 ** 100000";
    assert_about(setup, "x = a // b", division);
    assert_about(setup, "x = a % b", division);
    assert_about(setup, "x = divmod(a, b)", division);
    // A big number divided by a small one is a single pass over it.
    assert_about("a = 3 ** 200000", "x = a // 7", 4_953 / 32 + 4_953 / 256);
}

#[test]
fn shifts_charge_by_the_result_limbs() {
    // 10**7 bits are 156,250 limbs, moved sixteen to a step.
    assert_about("", "x = 1 << 10000000", 156_250 / 16);
}

#[test]
fn math_functions_on_big_integers_charge_by_limbs() {
    // 3**100000: 2,477 limbs. 6**60000 has 155,100 bits: 2,424 limbs.
    assert_about(
        "import math\na = 3 ** 100000\nb = 6 ** 60000",
        "x = math.gcd(a, b)",
        2_477 * 2_424 / 8,
    );
    // 10**50000 has 166,097 bits: 2,596 limbs.
    assert_about("import math\na = 10 ** 50000", "x = math.isqrt(a)", 2_596 * 2_596 / 128);
    // The product of 1..30000 is bounded by 30000 * 15 bits: 7,032 limbs.
    assert_about("import math", "x = math.factorial(30000)", 7_032 * 7_032 / 1024);
    // comb multiplies and divides the whole running value once per step of k: 50,000 times, on a value
    // of up to 100,000 bits (1,563 limbs), on average half that.
    assert_at_least("import math", "x = math.comb(100000, 50000)", 50_000 * 1_563 / 32);
}

#[test]
fn modular_power_charges_per_exponent_bit() {
    // Below the polling threshold: 3**3000 + 1 has 4,755 bits (75 limbs), the exponent 4,755 bits.
    let small = "m = 3 ** 3000 + 1\ne = 3 ** 3000";
    assert_about(small, "x = pow(7, e, m)", 4_755 * 75 * 75 / 128);
    // Above it: modulus 3**20000 + 1, 31,700 bits (496 limbs); exponent 3**2000, 3,170 bits.
    let large = "m = 3 ** 20000 + 1\ne = 3 ** 2000";
    assert_about(large, "x = pow(7, e, m)", 3_170 * 496 * 496 / 128);
}

#[test]
fn a_loop_of_big_integer_calls_hits_the_budget() {
    for body in [
        "a * a",
        "a // b",
        "a % b",
        "divmod(a, b)",
        "a ** 2",
        "math.gcd(a, b)",
        "math.isqrt(a)",
        "pow(7, e, m)",
        "1 << 10000000",
    ] {
        // Numbers of 500 limbs: each call is about a millisecond in a debug build, a million of them hours.
        let code = format!(
            "import math\na = 3 ** 20000\nb = 7 ** 10000\nm = 3 ** 5000 + 1\ne = 3 ** 500\nfor i in range(10 ** 6):\n    x = {body}"
        );
        let error = run_probed(&code, limits().max_turn_steps(500_000)).unwrap_err();
        assert_step_error(&error, "turn");
    }
}

#[test]
fn a_single_huge_modular_power_is_refused_before_it_runs_for_hours() {
    // An exponent of 158,000 bits and a modulus of 317,000 bits is billions of limb operations: seconds to
    // hours. With a budget it stops after the first few exponent bits.
    let code = "m = 3 ** 200000 + 1\ne = 3 ** 100000\nx = pow(7, e, m)";
    let error = run_probed(code, limits().max_turn_steps(10_000_000)).unwrap_err();
    assert_step_error(&error, "turn");
}

#[test]
fn small_integers_charge_nothing_extra() {
    for operation in ["x = 3 ** 100", "x = a * a", "x = a // 7", "x = a << 100"] {
        let steps = charged("a = 3 ** 400", operation);
        assert!(steps <= 40, "`{operation}` on small integers charged {steps}");
    }
}
