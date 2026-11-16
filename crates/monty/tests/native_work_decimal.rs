//! `Decimal` arithmetic at a high precision does a lot of work in one call: `exp`, `ln` and a real power run
//! series and Newton loops on integers of the precision's size, about p^2.5 limb operations, and a square root
//! about p^2. They charge steps from the context's precision p (in digits): `p * p * isqrt(p) / 600` for the
//! transcendental functions and `p * p / 2000` for a square root, measured at 250 to 2000 digits.

mod step_support;

use step_support::{assert_step_error, charged, limits, run_probed};

fn setup(precision: u64) -> String {
    format!("from decimal import Decimal, getcontext\ngetcontext().prec = {precision}")
}

fn transcendental(precision: u64) -> u64 {
    precision * precision * precision.isqrt() / 600
}

fn assert_between(precision: u64, operation: &str, floor: u64) {
    let steps = charged(&setup(precision), operation);
    assert!(
        steps >= floor && steps <= floor * 4 + 100,
        "`{operation}` at precision {precision} charged {steps}, expected about {floor}"
    );
}

#[test]
fn transcendental_functions_charge_by_precision() {
    let floor = transcendental(1000);
    assert_between(1000, "x = Decimal('1.5').exp()", floor);
    assert_between(1000, "x = Decimal(2).ln()", floor);
    // A fractional power also runs an `exp` and an `ln` inside, so it charges about three times as much.
    assert_between(1000, "x = Decimal(2) ** Decimal('0.5')", floor);
    assert_between(1000, "x = Decimal('1.0001') ** 1000", floor);
    // log10 is an ln and a division.
    assert_between(1000, "x = Decimal(2).log10()", floor);
}

#[test]
fn square_roots_charge_a_quadratic_in_the_precision() {
    assert_between(4000, "x = Decimal(2).sqrt()", 4000 * 4000 / 2000);
}

#[test]
fn the_default_precision_costs_nothing_extra() {
    for operation in [
        "x = Decimal('1.5').exp()",
        "x = Decimal(2).ln()",
        "x = Decimal(2).sqrt()",
        "x = Decimal(2) ** Decimal('0.5')",
    ] {
        let steps = charged("from decimal import Decimal", operation);
        assert!(steps <= 100, "`{operation}` at precision 28 charged {steps}");
    }
}

#[test]
fn a_loop_of_high_precision_decimal_calls_hits_the_budget() {
    for body in [
        "Decimal('1.5').exp()",
        "Decimal(2).ln()",
        "Decimal(2) ** Decimal('0.5')",
        "Decimal(2).sqrt()",
    ] {
        let code = format!("{}\nfor i in range(10 ** 6):\n    x = {body}", setup(1000));
        let error = run_probed(&code, limits().max_turn_steps(200_000)).unwrap_err();
        assert_step_error(&error, "turn");
    }
}

#[test]
fn one_enormous_decimal_power_stops_at_its_budget() {
    // At precision 20,000 this single power ran for 15.8 seconds in a release build and could not be
    // interrupted; with a budget it is refused before the work starts.
    let code = format!("{}\nx = Decimal('1.0001') ** 100000", setup(20_000));
    let error = run_probed(&code, limits().max_turn_steps(300_000)).unwrap_err();
    assert_step_error(&error, "turn");
}
