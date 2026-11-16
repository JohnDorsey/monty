//! The `math` module and `**` must give the same bits on every platform.
//!
//! Rust's float methods call the C library of the target, whose results differ between glibc,
//! the Windows UCRT and Apple's libm in the last place for a few percent of inputs. The
//! interpreter instead calls the `libm` crate (see `portable_math.rs` in the crate source), so each
//! result here must equal the result of calling `libm` directly, bit for bit. Inputs come from
//! a fixed pseudo-random sequence over each function's domain, because a handful of
//! hand-picked inputs showed no platform difference when 5,000 random ones showed about 6 percent.

use std::slice::from_ref;

use monty::MontyRun;
use monty_types::{CompileOptions, MontyObject};

/// Inputs per function. The platform libraries disagree on a few percent of inputs, so a few
/// thousand reliably expose a function that still calls them.
const SAMPLE_COUNT: usize = 6000;

/// A 64-bit linear congruential generator (Knuth's MMIX constants), so the inputs are the same
/// on every run and platform.
struct Sequence(u64);

impl Sequence {
    /// The next value, uniform in `[0, 1)` with 53 random bits.
    fn unit(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        #[expect(clippy::cast_precision_loss, reason = "53 bits fit in an f64 exactly")]
        let bits = (self.0 >> 11) as f64;
        bits / 9_007_199_254_740_992.0
    }

    /// The next value, uniform in `[low, high)`.
    fn between(&mut self, low: f64, high: f64) -> f64 {
        low + (high - low) * self.unit()
    }

    /// A positive value whose binary exponent is uniform in `[-exponent, exponent)`, so the
    /// whole range of magnitudes is covered rather than only the largest.
    fn magnitude(&mut self, exponent: f64) -> f64 {
        libm::exp2(self.between(-exponent, exponent)) * self.between(1.0, 2.0)
    }
}

/// One two-or-three-argument case: label, script, input columns, expected results.
type Case<'a> = (&'a str, &'a str, Vec<&'a [f64]>, Vec<f64>);

/// A function of one argument: its Python name, the `libm` function it must match, and a
/// generator of inputs inside its domain.
struct Unary {
    name: &'static str,
    reference: fn(f64) -> f64,
    input: fn(&mut Sequence) -> f64,
}

fn wide(sequence: &mut Sequence) -> f64 {
    sequence.between(-1e6, 1e6)
}
fn exponent_range(sequence: &mut Sequence) -> f64 {
    sequence.between(-700.0, 700.0)
}
fn small(sequence: &mut Sequence) -> f64 {
    sequence.between(-20.0, 20.0)
}
fn open_unit(sequence: &mut Sequence) -> f64 {
    sequence.between(-0.999_999, 0.999_999)
}
fn at_least_one(sequence: &mut Sequence) -> f64 {
    1.0 + sequence.magnitude(30.0)
}
fn positive(sequence: &mut Sequence) -> f64 {
    sequence.magnitude(1000.0)
}
fn above_minus_one(sequence: &mut Sequence) -> f64 {
    sequence.magnitude(30.0) - 0.999_999
}
fn signed_magnitude(sequence: &mut Sequence) -> f64 {
    let value = sequence.magnitude(300.0);
    if sequence.unit() < 0.5 { -value } else { value }
}

const UNARY: &[Unary] = &[
    Unary {
        name: "sin",
        reference: libm::sin,
        input: wide,
    },
    Unary {
        name: "cos",
        reference: libm::cos,
        input: wide,
    },
    Unary {
        name: "tan",
        reference: libm::tan,
        input: wide,
    },
    Unary {
        name: "asin",
        reference: libm::asin,
        input: open_unit,
    },
    Unary {
        name: "acos",
        reference: libm::acos,
        input: open_unit,
    },
    Unary {
        name: "atan",
        reference: libm::atan,
        input: signed_magnitude,
    },
    Unary {
        name: "sinh",
        reference: libm::sinh,
        input: exponent_range,
    },
    Unary {
        name: "cosh",
        reference: libm::cosh,
        input: exponent_range,
    },
    Unary {
        name: "tanh",
        reference: libm::tanh,
        input: small,
    },
    Unary {
        name: "asinh",
        reference: libm::asinh,
        input: signed_magnitude,
    },
    Unary {
        name: "acosh",
        reference: libm::acosh,
        input: at_least_one,
    },
    Unary {
        name: "atanh",
        reference: libm::atanh,
        input: open_unit,
    },
    Unary {
        name: "exp",
        reference: libm::exp,
        input: exponent_range,
    },
    Unary {
        name: "exp2",
        reference: libm::exp2,
        input: small,
    },
    Unary {
        name: "expm1",
        reference: libm::expm1,
        input: exponent_range,
    },
    Unary {
        name: "log2",
        reference: libm::log2,
        input: positive,
    },
    Unary {
        name: "log10",
        reference: libm::log10,
        input: positive,
    },
    Unary {
        name: "log1p",
        reference: libm::log1p,
        input: above_minus_one,
    },
    Unary {
        name: "cbrt",
        reference: libm::cbrt,
        input: signed_magnitude,
    },
];

/// Runs `code` with the given float lists bound to `xs` (and `ys`, `zs` when present) and returns
/// the floats it produced.
fn run_floats(code: &str, arguments: &[&[f64]]) -> Vec<f64> {
    let names = ["xs", "ys", "zs"];
    let input_names = names[..arguments.len()].iter().map(|name| (*name).to_owned()).collect();
    let inputs = arguments
        .iter()
        .map(|values| MontyObject::list(values.iter().copied().map(MontyObject::float)))
        .collect();
    let result = MontyRun::new(
        code.to_owned(),
        "portable_math.py",
        input_names,
        CompileOptions::default(),
    )
    .expect("the script compiles")
    .run_no_limits(inputs)
    .expect("the script runs");
    result
        .as_ref()
        .items()
        .expect("the script returns a list")
        .iter()
        .map(|item| item.as_float().expect("every item is a float"))
        .collect()
}

/// Compares two result lists bit for bit, describing the first few differences.
fn mismatches(label: &str, inputs: &[Vec<f64>], got: &[f64], expected: &[f64]) -> Option<String> {
    assert_eq!(got.len(), expected.len(), "{label}: result count");
    let differing: Vec<usize> = (0..got.len())
        .filter(|&index| got[index].to_bits() != expected[index].to_bits())
        .collect();
    if differing.is_empty() {
        return None;
    }
    let examples: Vec<String> = differing
        .iter()
        .take(3)
        .map(|&index| {
            let arguments: Vec<String> = inputs.iter().map(|column| format!("{:e}", column[index])).collect();
            format!(
                "({}) gave {:#018x} but libm gives {:#018x}",
                arguments.join(", "),
                got[index].to_bits(),
                expected[index].to_bits()
            )
        })
        .collect();
    Some(format!(
        "{label}: {} of {} results differ, e.g. {}",
        differing.len(),
        got.len(),
        examples.join("; ")
    ))
}

#[test]
fn one_argument_math_functions_equal_libm_bit_for_bit() {
    let mut failures = Vec::new();
    for function in UNARY {
        let mut sequence = Sequence(0x9E37_79B9_7F4A_7C15);
        let inputs: Vec<f64> = (0..SAMPLE_COUNT).map(|_| (function.input)(&mut sequence)).collect();
        let got = run_floats(
            &format!("import math\n[math.{}(x) for x in xs]", function.name),
            &[&inputs],
        );
        let expected: Vec<f64> = inputs.iter().map(|&x| (function.reference)(x)).collect();
        if let Some(failure) = mismatches(&format!("math.{}", function.name), &[inputs], &got, &expected) {
            failures.push(failure);
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn natural_log_equals_libm_with_and_without_a_base() {
    let mut sequence = Sequence(0xD1B5_4A32_D192_ED03);
    let values: Vec<f64> = (0..SAMPLE_COUNT).map(|_| sequence.magnitude(1000.0)).collect();
    let bases: Vec<f64> = (0..SAMPLE_COUNT).map(|_| sequence.between(1.5, 50.0)).collect();
    let natural = run_floats("import math\n[math.log(x) for x in xs]", &[&values]);
    let expected_natural: Vec<f64> = values.iter().map(|&x| libm::log(x)).collect();
    // `math.log(x, base)` is `log(x) / log(base)` in CPython, so the quotient is the contract.
    let with_base = run_floats(
        "import math\n[math.log(x, b) for x, b in zip(xs, ys)]",
        &[&values, &bases],
    );
    let expected_with_base: Vec<f64> = values
        .iter()
        .zip(&bases)
        .map(|(&x, &base)| libm::log(x) / libm::log(base))
        .collect();
    let failures: Vec<String> = [
        mismatches("math.log(x)", from_ref(&values), &natural, &expected_natural),
        mismatches(
            "math.log(x, base)",
            &[values.clone(), bases],
            &with_base,
            &expected_with_base,
        ),
    ]
    .into_iter()
    .flatten()
    .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn two_argument_math_functions_and_the_power_operator_equal_libm_bit_for_bit() {
    let mut sequence = Sequence(0xA076_1D64_78BD_642F);
    let bases: Vec<f64> = (0..SAMPLE_COUNT).map(|_| sequence.magnitude(6.0)).collect();
    let exponents: Vec<f64> = (0..SAMPLE_COUNT).map(|_| sequence.between(-10.0, 10.0)).collect();
    let xs: Vec<f64> = (0..SAMPLE_COUNT).map(|_| sequence.between(-1e3, 1e3)).collect();
    let ys: Vec<f64> = (0..SAMPLE_COUNT).map(|_| sequence.between(-1e3, 1e3)).collect();
    let zs: Vec<f64> = (0..SAMPLE_COUNT).map(|_| sequence.between(-1e3, 1e3)).collect();

    let cases: [Case<'_>; 5] = [
        (
            "math.pow",
            "import math\n[math.pow(x, y) for x, y in zip(xs, ys)]",
            vec![&bases, &exponents],
            bases.iter().zip(&exponents).map(|(&b, &e)| libm::pow(b, e)).collect(),
        ),
        (
            "float ** float",
            "[x ** y for x, y in zip(xs, ys)]",
            vec![&bases, &exponents],
            bases.iter().zip(&exponents).map(|(&b, &e)| libm::pow(b, e)).collect(),
        ),
        (
            "math.atan2",
            "import math\n[math.atan2(x, y) for x, y in zip(xs, ys)]",
            vec![&xs, &ys],
            xs.iter().zip(&ys).map(|(&y, &x)| libm::atan2(y, x)).collect(),
        ),
        (
            "math.hypot",
            "import math\n[math.hypot(x, y) for x, y in zip(xs, ys)]",
            vec![&xs, &ys],
            // `math.hypot` has its own scaled summation (see `aggregate.rs`) that CPython
            // also uses instead of the C library; its contract is correct rounding of the
            // true length, which libm's hypot meets within one unit in the last place for
            // two arguments. Compared loosely below, not here.
            xs.iter().zip(&ys).map(|(&x, &y)| libm::hypot(x, y)).collect(),
        ),
        (
            "math.fma",
            "import math\n[math.fma(x, y, z) for x, y, z in zip(xs, ys, zs)]",
            vec![&xs, &ys, &zs],
            xs.iter()
                .zip(&ys)
                .zip(&zs)
                .map(|((&x, &y), &z)| libm::fma(x, y, z))
                .collect(),
        ),
    ];

    let mut failures = Vec::new();
    for (label, code, arguments, expected) in &cases {
        let got = run_floats(code, arguments);
        let columns: Vec<Vec<f64>> = arguments.iter().map(|column| column.to_vec()).collect();
        if *label == "math.hypot" {
            // Bit equality is not promised for hypot (see above); a unit in the last place is.
            let worst = got
                .iter()
                .zip(expected)
                .map(|(a, b)| (i128::from(a.to_bits()) - i128::from(b.to_bits())).abs())
                .max()
                .unwrap_or(0);
            if worst > 1 {
                failures.push(format!("{label}: differs from libm by {worst} units in the last place"));
            }
            continue;
        }
        if let Some(failure) = mismatches(label, &columns, &got, expected) {
            failures.push(failure);
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The script behind [`seeded_random_and_complex_powers_have_the_same_bits_on_every_platform`]:
/// every random distribution that calls a transcendental function, then complex powers.
const GOLDEN_SCRIPT: &str = "
import random
random.seed(12345)
out = []
out.append(random.gauss(0.0, 1.0))
out.append(random.gauss(0.0, 1.0))
out.append(random.normalvariate(10.0, 2.0))
out.append(random.lognormvariate(0.0, 0.5))
out.append(random.expovariate(1.5))
out.append(random.vonmisesvariate(1.0, 2.0))
out.append(random.vonmisesvariate(0.5, 0.1))
out.append(random.gammavariate(0.5, 1.0))
out.append(random.gammavariate(2.5, 1.0))
out.append(random.gammavariate(1.0, 2.0))
out.append(random.betavariate(2.0, 3.0))
out.append(random.weibullvariate(1.0, 1.5))
out.append(float(random.binomialvariate(40, 0.3)))
out.append(float(random.binomialvariate(1000, 0.4)))
out.append(abs(3.0 + 4.0j))
out.append(abs(1e200 + 1e200j))
z = (1.5 + 2.0j) ** 2.5
out.append(z.real)
out.append(z.imag)
w = (2.0 + 1.0j) ** (0.5 + 0.5j)
out.append(w.real)
out.append(w.imag)
out
";

/// Bit patterns produced by [`GOLDEN_SCRIPT`]. They were recorded once and then checked on Windows
/// and on Linux (see the commit message of the change that added this test); a platform that
/// disagrees has brought its own C library back into the interpreter.
const GOLDEN_BITS: &[u64] = &[
    0xbfbf_b168_b045_dd55,
    0x3fb2_4f75_c2f9_bf44,
    0x4027_2e8b_e434_363b,
    0x3feb_d1eb_e29b_0e45,
    0x3fe1_cebc_8547_18d7,
    0x3fea_2fde_d866_1cdd,
    0x4001_5730_b2e8_6bc6,
    0x3f87_ed1e_65f1_3383,
    0x4000_bed6_24c8_d71d,
    0x3fd4_8608_2597_7e8c,
    0x3fe3_6416_195c_dec5,
    0x3fe1_cc3a_19ce_d2d6,
    0x402c_0000_0000_0000,
    0x407a_f000_0000_0000,
    0x4014_0000_0000_0000,
    0x697d_8f98_1133_5b57,
    0xc01a_debc_19b7_1700,
    0x401c_fdca_f353_049f,
    0x3fee_9227_1bba_d7b7,
    0x3fe6_7c75_d603_94d6,
];

#[test]
fn seeded_random_and_complex_powers_have_the_same_bits_on_every_platform() {
    let got: Vec<u64> = run_floats(GOLDEN_SCRIPT, &[])
        .iter()
        .map(|value| value.to_bits())
        .collect();
    let listing: Vec<String> = got.iter().map(|bits| format!("{bits:#018x}")).collect();
    assert_eq!(got, GOLDEN_BITS, "actual bits:\n{}", listing.join(",\n"));
}
