//! Integration tests: run `.java` programs through the built `java` binary and
//! assert their stdout. Frozen expected outputs (verified byte-for-byte against
//! OpenJDK) so the suite is self-contained — CI needs no JDK installed.

use std::process::Command;

/// Run a Java source string through the `java` binary and return (stdout, ok).
fn run(src: &str) -> (String, bool) {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("javars_test_{}.java", fasthash(src)));
    std::fs::write(&path, src).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_java"))
        .arg(&path)
        .output()
        .expect("spawn java");
    let _ = std::fs::remove_file(&path);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.success(),
    )
}

/// Run a Java source string and return (stdout, stderr, ok) — for exercising
/// `System.err`, which the stdout-only [`run`] helper cannot observe.
fn run_streams(src: &str) -> (String, String, bool) {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("javars_test_{}.java", fasthash(src)));
    std::fs::write(&path, src).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_java"))
        .arg(&path)
        .output()
        .expect("spawn java");
    let _ = std::fs::remove_file(&path);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

fn fasthash(s: &str) -> u64 {
    // A tiny FNV-1a so concurrent tests use distinct temp files.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn wrap(body: &str) -> String {
    format!("public class T {{ public static void main(String[] args) {{ {body} }} }}")
}

#[test]
fn prints_a_string_literal() {
    let (out, ok) = run(&wrap(r#"System.out.println("hello");"#));
    assert!(ok);
    assert_eq!(out, "hello\n");
}

#[test]
fn integer_arithmetic_and_precedence() {
    let (out, _) = run(&wrap("System.out.println(2 + 3 * 4 - 1);"));
    assert_eq!(out, "13\n");
}

#[test]
fn integer_division_truncates_and_modulo() {
    let (out, _) = run(&wrap(
        "System.out.println(7 / 2); System.out.println(7 % 3);",
    ));
    assert_eq!(out, "3\n1\n");
}

#[test]
fn string_plus_int_concatenation() {
    let (out, _) = run(&wrap(r#"int x = 21; System.out.println("x=" + x * 2);"#));
    assert_eq!(out, "x=42\n");
}

#[test]
fn boolean_prints_java_style() {
    let (out, _) = run(&wrap(
        "System.out.println(3 > 2); System.out.println(1 == 2);",
    ));
    assert_eq!(out, "true\nfalse\n");
}

#[test]
fn double_prints_with_trailing_point_zero() {
    let (out, _) = run(&wrap("double d = 3.0; System.out.println(d);"));
    assert_eq!(out, "3.0\n");
}

#[test]
fn if_else_chain() {
    let (out, _) = run(&wrap(
        "int n = 5; if (n < 0) { System.out.println(\"neg\"); } else if (n == 0) { System.out.println(\"zero\"); } else { System.out.println(\"pos\"); }",
    ));
    assert_eq!(out, "pos\n");
}

#[test]
fn while_loop_accumulates() {
    let (out, _) = run(&wrap(
        "int sum = 0; int i = 1; while (i <= 5) { sum += i; i++; } System.out.println(sum);",
    ));
    assert_eq!(out, "15\n");
}

#[test]
fn for_loop_counts() {
    let (out, _) = run(&wrap(
        "for (int i = 0; i < 3; i++) { System.out.println(i); }",
    ));
    assert_eq!(out, "0\n1\n2\n");
}

#[test]
fn for_loop_with_break_and_continue() {
    let (out, _) = run(&wrap(
        "for (int i = 0; i < 10; i++) { if (i == 2) { continue; } if (i == 4) { break; } System.out.println(i); }",
    ));
    assert_eq!(out, "0\n1\n3\n");
}

#[test]
fn short_circuit_and_or() {
    let (out, _) = run(&wrap(
        "int x = 5; System.out.println(x > 0 && x < 10); System.out.println(x < 0 || x == 5);",
    ));
    assert_eq!(out, "true\ntrue\n");
}

#[test]
fn unary_negation_and_not() {
    let (out, _) = run(&wrap(
        "int x = 3; System.out.println(-x); System.out.println(!(x > 5));",
    ));
    assert_eq!(out, "-3\ntrue\n");
}

#[test]
fn fizzbuzz_first_five() {
    let (out, _) = run(&wrap(
        "for (int i = 1; i <= 5; i++) { if (i % 15 == 0) { System.out.println(\"FizzBuzz\"); } else if (i % 3 == 0) { System.out.println(\"Fizz\"); } else if (i % 5 == 0) { System.out.println(\"Buzz\"); } else { System.out.println(i); } }",
    ));
    assert_eq!(out, "1\n2\nFizz\n4\nBuzz\n");
}

#[test]
fn utf8_string_literal_survives() {
    let (out, _) = run(&wrap(r#"System.out.println("café — ☕");"#));
    assert_eq!(out, "café — ☕\n");
}

#[test]
fn missing_main_is_an_error() {
    let (_out, ok) = run("public class NoMain { int x; }");
    assert!(!ok, "a class with no main should fail to run");
}

// ── integer vs. floating division (Java binary numeric promotion) ──

#[test]
fn division_truncates_toward_zero_and_stays_float_when_typed() {
    // `int/int` truncates toward zero (so a negative quotient rounds up);
    // a `double` operand keeps the fractional result.
    let (out, _) = run(&wrap(
        "System.out.println(-7 / 2); System.out.println(7.0 / 2); System.out.println(9 / 4);",
    ));
    assert_eq!(out, "-3\n3.5\n2\n");
}

#[test]
fn division_uses_declared_variable_types() {
    // `x` is a tracked `int`, so `x / 2` truncates; `d` is a `double`, so it
    // does not. A truncating result must print as an int (`3`, not `3.0`).
    let (out, _) = run(&wrap(
        "int x = 7; double d = 7; System.out.println(x / 2); System.out.println(d / 2);",
    ));
    assert_eq!(out, "3\n3.5\n");
}

// ── user-defined static methods (Op::Call frame ABI) ──

#[test]
fn static_method_with_parameters() {
    let (out, ok) = run(
        "public class M { static int add(int a, int b) { return a + b; } \
         public static void main(String[] args) { System.out.println(add(20, 22)); } }",
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn recursion_does_not_clobber_frame_locals() {
    // Recursive factorial: each call must keep its own `n` in a fresh frame
    // slot. A shared-global lowering would return the wrong product.
    let (out, ok) = run(
        "public class F { static int fact(int n) { if (n <= 1) return 1; return n * fact(n - 1); } \
         public static void main(String[] args) { System.out.println(fact(6)); } }",
    );
    assert!(ok);
    assert_eq!(out, "720\n");
}

#[test]
fn mutual_recursion_via_forward_reference() {
    // `isEven` calls `isOdd`, declared after it — signatures are registered
    // before any body is lowered, so forward references resolve.
    let (out, ok) = run(
        "public class P { \
         static boolean isEven(int n) { if (n == 0) return true; return isOdd(n - 1); } \
         static boolean isOdd(int n) { if (n == 0) return false; return isEven(n - 1); } \
         public static void main(String[] args) { System.out.println(isEven(10)); System.out.println(isOdd(10)); } }",
    );
    assert!(ok);
    assert_eq!(out, "true\nfalse\n");
}

#[test]
fn void_method_with_early_return() {
    let (out, ok) = run(
        "public class V { \
         static void sign(int n) { if (n > 0) { System.out.println(\"pos\"); return; } System.out.println(\"nonpos\"); } \
         public static void main(String[] args) { sign(3); sign(-2); } }",
    );
    assert!(ok);
    assert_eq!(out, "pos\nnonpos\n");
}

#[test]
fn method_call_result_feeds_arithmetic() {
    // A method's return value participates in an expression (and its `int`
    // return type drives the division truncation).
    let (out, ok) = run("public class A { static int half(int x) { return x / 2; } \
         public static void main(String[] args) { System.out.println(half(9) + 1); } }");
    assert!(ok);
    assert_eq!(out, "5\n");
}

#[test]
fn wrong_argument_count_is_a_compile_error() {
    let (_out, ok) = run("public class E { static int f(int a) { return a; } \
         public static void main(String[] args) { System.out.println(f(1, 2)); } }");
    assert!(!ok, "calling a method with the wrong arity must fail");
}

// ── String instance methods (postfix `.` dispatch) ──

#[test]
fn string_length_case_and_substring() {
    let (out, _) = run(&wrap(
        "String s = \"Hello\"; System.out.println(s.length()); \
         System.out.println(s.toUpperCase()); System.out.println(s.substring(1, 4));",
    ));
    assert_eq!(out, "5\nHELLO\nell\n");
}

#[test]
fn string_search_and_predicates() {
    let (out, _) = run(&wrap(
        "String s = \"banana\"; System.out.println(s.indexOf(\"na\")); \
         System.out.println(s.contains(\"nan\")); System.out.println(s.startsWith(\"ba\")); \
         System.out.println(s.charAt(0));",
    ));
    assert_eq!(out, "2\ntrue\ntrue\nb\n");
}

#[test]
fn string_equals_and_transform() {
    let (out, _) = run(&wrap(
        "String s = \"abc\"; System.out.println(s.equals(\"abc\")); \
         System.out.println(s.equalsIgnoreCase(\"ABC\")); System.out.println(s.replace(\"b\", \"X\")); \
         System.out.println(\"ab\".repeat(3));",
    ));
    assert_eq!(out, "true\ntrue\naXc\nababab\n");
}

#[test]
fn chained_string_calls() {
    // Postfix dispatch chains: substring's result receives another call.
    let (out, _) = run(&wrap(
        "System.out.println(\"  Hello World  \".trim().substring(0, 5).toUpperCase());",
    ));
    assert_eq!(out, "HELLO\n");
}

#[test]
fn string_index_out_of_range_is_an_error() {
    let (_out, ok) = run(&wrap("System.out.println(\"hi\".charAt(9));"));
    assert!(
        !ok,
        "an out-of-range charAt must surface an error, not a wrong value"
    );
}

// ── ternary conditional operator ──

#[test]
fn ternary_selects_branch_by_condition() {
    let (out, _) = run(&wrap(
        "int x = 5; System.out.println(x > 0 ? \"pos\" : \"nonpos\"); \
         System.out.println(x < 0 ? \"neg\" : \"nonneg\");",
    ));
    assert_eq!(out, "pos\nnonneg\n");
}

#[test]
fn ternary_is_right_associative() {
    // `a ? b : c ? d : e` parses as `a ? b : (c ? d : e)`. With x=5:
    // x<0 is false, so evaluate x>3 ? 9 : 2 → 9.
    let (out, _) = run(&wrap(
        "int x = 5; System.out.println(x < 0 ? 1 : x > 3 ? 9 : 2);",
    ));
    assert_eq!(out, "9\n");
}

#[test]
fn ternary_result_type_drives_division() {
    // A conditional with two `int` branches truncates the following `/`; a
    // branch typed `double` promotes the whole expression to floating point.
    let (out, _) = run(&wrap(
        "boolean f = true; System.out.println((f ? 7 : 8) / 2); \
         System.out.println((f ? 7.0 : 8) / 2);",
    ));
    assert_eq!(out, "3\n3.5\n");
}

// ── switch statement (classic, with fall-through) ──

#[test]
fn switch_int_fallthrough_and_grouped_labels() {
    // case 0 breaks; cases 1 and 2 share a body then break; anything else hits
    // default. Verifies grouped labels (`case 1: case 2:`) and break/default.
    let (out, _) = run(&wrap(
        "for (int i = 0; i < 5; i++) { switch (i) { \
         case 0: System.out.println(\"zero\"); break; \
         case 1: case 2: System.out.println(\"one-two\"); break; \
         default: System.out.println(\"big\"); } }",
    ));
    assert_eq!(out, "zero\none-two\none-two\nbig\nbig\n");
}

#[test]
fn switch_string_falls_through_without_break() {
    // A matched `case` with no `break` falls into the following group's body —
    // here into `default`. Switch on `String` uses value equality.
    let (out, _) = run(&wrap(
        "String s = \"b\"; switch (s) { \
         case \"a\": System.out.println(\"A\"); break; \
         case \"b\": System.out.println(\"B\"); \
         default: System.out.println(\"fell\"); }",
    ));
    assert_eq!(out, "B\nfell\n");
}

#[test]
fn switch_default_reached_when_no_case_matches() {
    // `default` need not be last, and an unmatched discriminant jumps to it.
    let (out, _) = run(&wrap(
        "int x = 9; switch (x) { \
         case 1: System.out.println(\"1\"); break; \
         default: System.out.println(\"def\"); break; \
         case 2: System.out.println(\"2\"); } System.out.println(\"end\");",
    ));
    assert_eq!(out, "def\nend\n");
}

#[test]
fn switch_break_exits_switch_not_enclosing_loop() {
    // Inside a loop: `break` leaves the switch (so `after` still prints), while
    // `continue` skips to the next loop iteration (so `after` is skipped).
    let (out, _) = run(&wrap(
        "for (int i = 0; i < 4; i++) { switch (i) { \
         case 1: continue; \
         case 2: break; \
         default: System.out.println(\"d\" + i); } System.out.println(\"after\" + i); }",
    ));
    assert_eq!(out, "d0\nafter0\nafter2\nd3\nafter3\n");
}

// ── do/while and labeled break/continue ──

#[test]
fn do_while_runs_body_before_testing() {
    // The body runs once even when the condition is false on entry.
    let (out, _) = run(&wrap(
        "int i = 0; do { System.out.println(i); i++; } while (i < 3); \
         int j = 10; do { System.out.println(\"once\"); } while (j < 5);",
    ));
    assert_eq!(out, "0\n1\n2\nonce\n");
}

#[test]
fn labeled_break_exits_outer_loop() {
    let (out, _) = run(&wrap(
        "outer: for (int i = 0; i < 3; i++) { for (int j = 0; j < 3; j++) { \
         if (i + j == 3) { break outer; } System.out.println(i + \",\" + j); } }",
    ));
    assert_eq!(out, "0,0\n0,1\n0,2\n1,0\n1,1\n");
}

#[test]
fn labeled_continue_advances_outer_loop() {
    // `continue outer` abandons the inner loop and steps the outer one, so only
    // the `j == 0` row of each `i` prints.
    let (out, _) = run(&wrap(
        "outer: for (int i = 0; i < 3; i++) { for (int j = 0; j < 3; j++) { \
         if (j == 1) { continue outer; } System.out.println(i + \",\" + j); } }",
    ));
    assert_eq!(out, "0,0\n1,0\n2,0\n");
}

// ── stdlib essentials (Math, Integer, String.valueOf, Boolean, System.err) ──

#[test]
fn math_functions_match_java_overload_typing() {
    // abs/max/min keep an int result for int operands and a double result when
    // any operand is floating point; pow/sqrt/floor/ceil are always double;
    // round returns an integer (ties toward positive infinity).
    let (out, _) = run(&wrap(
        "System.out.println(Math.abs(-5)); System.out.println(Math.abs(-5.5)); \
         System.out.println(Math.max(3, 7)); System.out.println(Math.min(3.0, 7)); \
         System.out.println(Math.pow(2, 10)); System.out.println(Math.sqrt(16)); \
         System.out.println(Math.floor(2.7)); System.out.println(Math.ceil(2.1)); \
         System.out.println(Math.round(2.5)); System.out.println(Math.round(-2.5));",
    ));
    assert_eq!(out, "5\n5.5\n7\n3.0\n1024.0\n4.0\n2.0\n3.0\n3\n-2\n");
}

#[test]
fn integer_parse_value_and_to_string_with_radix() {
    let (out, _) = run(&wrap(
        "System.out.println(Integer.parseInt(\"42\")); \
         System.out.println(Integer.parseInt(\"ff\", 16)); \
         System.out.println(Integer.valueOf(\"7\") + 1); \
         System.out.println(Integer.toString(255)); \
         System.out.println(Integer.toString(255, 16));",
    ));
    assert_eq!(out, "42\n255\n8\n255\nff\n");
}

#[test]
fn integer_parse_int_rejects_non_numeric() {
    let (_out, ok) = run(&wrap("System.out.println(Integer.parseInt(\"notnum\"));"));
    assert!(
        !ok,
        "parseInt of a non-numeric string must fault, not return a wrong value"
    );
}

#[test]
fn string_value_of_and_boolean_parse() {
    let (out, _) = run(&wrap(
        "System.out.println(String.valueOf(42)); System.out.println(String.valueOf(true)); \
         System.out.println(String.valueOf(3.0)); System.out.println(Boolean.parseBoolean(\"TRUE\"));",
    ));
    assert_eq!(out, "42\ntrue\n3.0\ntrue\n");
}

#[test]
fn system_err_writes_to_stderr_not_stdout() {
    let (out, err, ok) = run_streams(&wrap(
        "System.out.println(\"to-out\"); System.err.println(\"to-err\"); System.err.print(\"tail\");",
    ));
    assert!(ok);
    assert_eq!(out, "to-out\n");
    assert_eq!(err, "to-err\ntail");
}

#[test]
fn unknown_static_method_is_an_error() {
    let (_out, ok) = run(&wrap("System.out.println(Math.tan(1.0));"));
    assert!(
        !ok,
        "an unregistered static method must error rather than run"
    );
}
