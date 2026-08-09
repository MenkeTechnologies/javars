//! The strict numeric hook, driven directly.
//!
//! fusevm delegates to `javars::host::numeric_hook` for every operation it
//! cannot answer exactly under the strict policy. Three cases arrive: a
//! non-numeric operand (Java's `String` `+`), an all-integer operation that
//! overflows `i64`, and — since fusevm's strict-exactness fix — a mixed
//! `Int`/`Float` pair whose integer is past 2^53, where converting it to `f64`
//! would round.
//!
//! Every expectation below is the verbatim answer of `java` 26.0.2 /
//! `javac` 26.0.2 (`openjdk version "26.0.2" 2026-07-21`, Temurin-26.0.2+10)
//! for the same operands, not an inferred one. Java's binary numeric promotion
//! (JLS 5.6.2) converts the `long` to `double`, so the rounded result *is*
//! Java's answer — `L == R` below is `true` in real `java` even though `L` and
//! `R` are different mathematical values.
//!
//! Calling the hook directly rather than through a program is deliberate: the
//! delegation that reaches these arms only happens under a fusevm carrying the
//! fix, which is committed but unpublished. The hook is a pure function, so its
//! contract is testable ahead of the release.

use fusevm::{NumOp, Value};
use javars::host::numeric_hook;

/// `3^34` — the smallest exponent of three past 2^53, and the value fusevm's
/// fix is written around. Its `f64` image is [`L_AS_DOUBLE`], a *neighbouring*
/// integer: `(double) L` prints `1.6677181699666568E16` under `java`.
const L: i64 = 16_677_181_699_666_569;
const L_AS_DOUBLE: f64 = 1.6677181699666568e16;

fn num(op: NumOp, a: Value, b: Value) -> Value {
    numeric_hook(op, &a, &b).unwrap_or_else(|e| panic!("{op:?}({a:?}, {b:?}) errored: {e}"))
}

fn f(op: NumOp, a: Value, b: Value) -> f64 {
    match num(op, a.clone(), b.clone()) {
        Value::Float(v) => v,
        other => panic!("{op:?}({a:?}, {b:?}) is {other:?}, want a Float"),
    }
}

fn t(op: NumOp, a: Value, b: Value) -> bool {
    match num(op, a.clone(), b.clone()) {
        Value::Bool(v) => v,
        other => panic!("{op:?}({a:?}, {b:?}) is {other:?}, want a Bool"),
    }
}

/// A mixed pair must never reach the concatenating `+` arm.
///
/// This is the regression that motivated the file. With the numeric pair
/// falling through to the `String` arms, `Add(Int(L), Float(2.0))` returned
/// `Str("166771816996665692.0")` — not an error, not obviously wrong, and
/// number-shaped enough to survive a glance. `java` says `1.667718169966657E16`.
#[test]
fn mixed_add_is_a_number_not_a_concatenation() {
    let got = num(NumOp::Add, Value::Int(L), Value::Float(2.0));
    assert!(
        matches!(got, Value::Float(_)),
        "mixed `+` produced {got:?}; a non-Float here is the string-concat \
         fallthrough returning a number-shaped String"
    );
    assert_eq!(
        f(NumOp::Add, Value::Int(L), Value::Float(2.0)),
        1.667718169966657e16
    );
    assert_eq!(
        f(NumOp::Add, Value::Float(2.0), Value::Int(L)),
        1.667718169966657e16
    );
}

/// `java`: `L - 2.0 = 1.6677181699666566E16`, `2.0 - L = -1.6677181699666566E16`,
/// `L * 2.0 = 3.3354363399333136E16`, `L % 2.0 = 0.0`, `2.0 % L = 2.0`.
#[test]
fn mixed_arithmetic_matches_java_in_both_orders() {
    assert_eq!(
        f(NumOp::Sub, Value::Int(L), Value::Float(2.0)),
        1.6677181699666566e16
    );
    assert_eq!(
        f(NumOp::Sub, Value::Float(2.0), Value::Int(L)),
        -1.6677181699666566e16
    );
    assert_eq!(
        f(NumOp::Mul, Value::Int(L), Value::Float(2.0)),
        3.3354363399333136e16
    );
    assert_eq!(
        f(NumOp::Mul, Value::Float(2.0), Value::Int(L)),
        3.3354363399333136e16
    );
    assert_eq!(f(NumOp::Mod, Value::Int(L), Value::Float(2.0)), 0.0);
    assert_eq!(f(NumOp::Mod, Value::Float(2.0), Value::Int(L)), 2.0);
}

/// Java's `%` is the truncated remainder and takes the *dividend's* sign, which
/// is also Rust's `f64` `%`. `java`: `7L % -2.5 = 2.0`, `-7L % 2.5 = -2.0`,
/// `2.5 % 7L = 2.5`.
#[test]
fn mixed_remainder_takes_the_dividend_sign() {
    assert_eq!(f(NumOp::Mod, Value::Int(7), Value::Float(-2.5)), 2.0);
    assert_eq!(f(NumOp::Mod, Value::Int(-7), Value::Float(2.5)), -2.0);
    assert_eq!(f(NumOp::Mod, Value::Float(2.5), Value::Int(7)), 2.5);
}

/// All six comparisons, both orders, against the `double` that `L` rounds to.
///
/// Java promotes `L` to `double` first, so it compares *equal* to its own
/// rounded image. The pre-fix hook answered these by lexicographic `String`
/// order, which got `Eq`/`Le` (and their mirrors) backwards.
#[test]
fn mixed_comparisons_use_javas_promotion_not_string_order() {
    let (l, r) = (Value::Int(L), Value::Float(L_AS_DOUBLE));
    assert!(t(NumOp::Eq, l.clone(), r.clone()), "java: L == R is true");
    assert!(t(NumOp::Eq, r.clone(), l.clone()), "java: R == L is true");
    assert!(!t(NumOp::Ne, l.clone(), r.clone()), "java: L != R is false");
    assert!(!t(NumOp::Ne, r.clone(), l.clone()), "java: R != L is false");
    assert!(!t(NumOp::Lt, l.clone(), r.clone()), "java: L < R is false");
    assert!(!t(NumOp::Lt, r.clone(), l.clone()), "java: R < L is false");
    assert!(!t(NumOp::Gt, l.clone(), r.clone()), "java: L > R is false");
    assert!(!t(NumOp::Gt, r.clone(), l.clone()), "java: R > L is false");
    assert!(t(NumOp::Le, l.clone(), r.clone()), "java: L <= R is true");
    assert!(t(NumOp::Le, r.clone(), l.clone()), "java: R <= L is true");
    assert!(t(NumOp::Ge, l.clone(), r.clone()), "java: L >= R is true");
    assert!(t(NumOp::Ge, r.clone(), l.clone()), "java: R >= L is true");
}

/// A mixed pair that really does order strictly, so the equality test above
/// cannot pass by a comparison arm that answers `true` unconditionally.
#[test]
fn mixed_comparisons_still_order() {
    assert!(t(NumOp::Lt, Value::Int(7), Value::Float(7.5)));
    assert!(t(NumOp::Gt, Value::Float(7.5), Value::Int(7)));
    assert!(!t(NumOp::Ge, Value::Int(7), Value::Float(7.5)));
    assert!(!t(NumOp::Le, Value::Float(7.5), Value::Int(7)));
    assert!(t(NumOp::Ne, Value::Int(7), Value::Float(7.5)));
    assert!(!t(NumOp::Eq, Value::Int(7), Value::Float(7.5)));
}

/// `java`: every comparison with `NaN` is `false` except `!=`, and any
/// arithmetic with it is `NaN`.
#[test]
fn nan_operand_follows_java() {
    let (l, n) = (Value::Int(L), Value::Float(f64::NAN));
    assert!(f(NumOp::Add, l.clone(), n.clone()).is_nan());
    assert!(!t(NumOp::Eq, l.clone(), n.clone()));
    assert!(t(NumOp::Ne, l.clone(), n.clone()));
    assert!(!t(NumOp::Lt, l.clone(), n.clone()));
    assert!(!t(NumOp::Gt, l.clone(), n.clone()));
    assert!(!t(NumOp::Le, l.clone(), n.clone()));
    assert!(!t(NumOp::Ge, l.clone(), n.clone()));
    assert!(!t(NumOp::Ge, n, l));
}

/// `java`: `1L / 0.0` is `Infinity` and `1L % 0.0` is `NaN` — the floating
/// divisor never raises `ArithmeticException`.
#[test]
fn mixed_zero_divisor_is_ieee_not_a_fault() {
    assert_eq!(
        f(NumOp::Div, Value::Int(1), Value::Float(0.0)),
        f64::INFINITY
    );
    assert!(f(NumOp::Mod, Value::Int(1), Value::Float(0.0)).is_nan());
}

/// The pre-existing delegation case: `long` arithmetic that overflows `i64`
/// wraps two's-complement. `java`: `Long.MAX_VALUE + 1L == Long.MIN_VALUE`,
/// `Long.MIN_VALUE + -1L == Long.MAX_VALUE`, `-Long.MIN_VALUE == Long.MIN_VALUE`.
#[test]
fn integer_overflow_still_wraps() {
    let w = |op, x: i64, y: i64| num(op, Value::Int(x), Value::Int(y));
    assert_eq!(w(NumOp::Add, i64::MAX, 1), Value::Int(i64::MIN));
    assert_eq!(w(NumOp::Add, i64::MIN, -1), Value::Int(i64::MAX));
    assert_eq!(w(NumOp::Sub, i64::MIN, 1), Value::Int(i64::MAX));
    assert_eq!(w(NumOp::Mul, i64::MIN, -1), Value::Int(i64::MIN));
    assert_eq!(
        numeric_hook(NumOp::Neg, &Value::Int(i64::MIN), &Value::Undef),
        Ok(Value::Int(i64::MIN))
    );
    assert_eq!(
        numeric_hook(NumOp::Neg, &Value::Float(2.5), &Value::Undef),
        Ok(Value::Float(-2.5))
    );
}

/// `Long.MIN_VALUE % -1L` overflows `checked_rem`, so fusevm delegates it.
/// `java` answers `0`; `java`: `Long.MIN_VALUE / -1L == Long.MIN_VALUE`.
/// Before the fix both fell into the `String` arm and returned a type error.
#[test]
fn integer_division_edges_match_java() {
    assert_eq!(
        num(NumOp::Mod, Value::Int(i64::MIN), Value::Int(-1)),
        Value::Int(0)
    );
    assert_eq!(
        num(NumOp::Div, Value::Int(i64::MIN), Value::Int(-1)),
        Value::Int(i64::MIN)
    );
    assert_eq!(
        num(NumOp::Mod, Value::Int(-7), Value::Int(2)),
        Value::Int(-1)
    );
    assert_eq!(
        num(NumOp::Div, Value::Int(-7), Value::Int(2)),
        Value::Int(-3)
    );
    // Integral `/` and `%` by zero are Java's `ArithmeticException`, not a
    // wrap and not a panic.
    for op in [NumOp::Div, NumOp::Mod] {
        let e = numeric_hook(op, &Value::Int(1), &Value::Int(0)).unwrap_err();
        assert!(e.contains("ArithmeticException"), "{op:?} gave `{e}`");
    }
}

/// The `String` `+` overload the hook was originally written for is unchanged —
/// the numeric gate must not have captured it. `java` prints `x1`, `1x`,
/// `nulltrue` for these.
#[test]
fn string_concatenation_is_untouched() {
    let cat = |a: Value, b: Value| match numeric_hook(NumOp::Add, &a, &b) {
        Ok(Value::Str(s)) => s.to_string(),
        other => panic!("concat of {a:?} + {b:?} gave {other:?}"),
    };
    assert_eq!(cat(Value::str("x"), Value::Int(1)), "x1");
    assert_eq!(cat(Value::Int(1), Value::str("x")), "1x");
    assert_eq!(cat(Value::Undef, Value::Bool(true)), "nulltrue");
    assert_eq!(cat(Value::str("v="), Value::Float(3.0)), "v=3.0");
    // Arithmetic on a `String` operand stays a type error — `"a" - 1` does not
    // compile in Java, and must not start returning a number now that a numeric
    // arm exists.
    assert!(numeric_hook(NumOp::Sub, &Value::str("a"), &Value::Int(1)).is_err());
    assert!(numeric_hook(NumOp::Mul, &Value::Int(1), &Value::str("a")).is_err());
}

/// Java has no `**`, so the compiler never emits `Pow`; if one ever arrives it
/// must say so rather than inventing an exponentiation.
#[test]
fn pow_is_rejected_for_numeric_operands_too() {
    for (a, b) in [
        (Value::Int(2), Value::Int(3)),
        (Value::Int(2), Value::Float(3.0)),
        (Value::Float(2.0), Value::Float(3.0)),
    ] {
        let e = numeric_hook(NumOp::Pow, &a, &b).unwrap_err();
        assert!(e.contains("no `**` operator"), "{a:?} ** {b:?} gave `{e}`");
    }
}
