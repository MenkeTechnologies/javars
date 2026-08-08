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

/// Run a Java source string with program arguments, returning (stdout, ok) —
/// the only way to observe `main`'s `String[] args` carrying real values.
fn run_args(src: &str, args: &[&str]) -> (String, bool) {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("javars_test_{}.java", fasthash(src)));
    std::fs::write(&path, src).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_java"))
        .arg(&path)
        .args(args)
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

// ── Reference arrays (host-heap objects) ────────────────────────────────────

#[test]
fn new_int_array_defaults_indexing_and_length() {
    let (out, ok) = run("public class Main { public static void main(String[] a) {\
         int[] x = new int[4]; x[0] = 7; x[3] = x[0] * 2;\
         System.out.println(x[0]); System.out.println(x[3]);\
         System.out.println(x[1]); System.out.println(x.length); } }");
    assert!(ok);
    // x[1] is the zero default; length is 4.
    assert_eq!(out, "7\n14\n0\n4\n");
}

#[test]
fn array_literal_and_length_drive_a_loop() {
    let (out, _) = run("public class Main { public static void main(String[] a) {\
         int[] y = {5, 10, 15, 20}; int s = 0;\
         for (int i = 0; i < y.length; i++) { s += y[i]; }\
         System.out.println(s); System.out.println(y.length); } }");
    assert_eq!(out, "50\n4\n");
}

#[test]
fn array_is_passed_by_reference_and_mutation_is_visible_in_caller() {
    // The aliasing case round 1 correctly refused to fake: a method mutates an
    // element through the handle and the caller observes it.
    let (out, _) = run("public class Main {\
         static void fill(int[] a, int v) { for (int i = 0; i < a.length; i++) { a[i] = v; } }\
         static void bump(int[] a) { a[0] += 100; }\
         public static void main(String[] x) {\
         int[] arr = new int[3]; fill(arr, 9); bump(arr);\
         System.out.println(arr[0]); System.out.println(arr[1]); System.out.println(arr[2]); } }");
    assert_eq!(out, "109\n9\n9\n");
}

#[test]
fn array_element_compound_assignment_and_post_increment() {
    let (out, _) = run("public class Main { public static void main(String[] a) {\
         int[] x = {12, 20, 30}; x[0]++; x[1] -= 5; x[2] /= 4;\
         System.out.println(x[0] + \" \" + x[1] + \" \" + x[2]); } }");
    // 13, 15, 7 — note integer-truncating /= on the int[] element.
    assert_eq!(out, "13 15 7\n");
}

#[test]
fn string_array_elements_are_reference_typed() {
    let (out, _) = run(
        "public class Main { public static void main(String[] a) {\
         String[] w = {\"al\", \"bob\", \"cy\"};\
         for (int i = 0; i < w.length; i++) { System.out.println(w[i] + \":\" + w[i].length()); } } }",
    );
    assert_eq!(out, "al:2\nbob:3\ncy:2\n");
}

#[test]
fn array_index_out_of_bounds_is_an_error() {
    let (_out, ok) = run("public class Main { public static void main(String[] a) {\
         int[] x = new int[2]; System.out.println(x[5]); } }");
    assert!(
        !ok,
        "an out-of-range array read must fault, not return junk"
    );
}

// ── Classes: fields, constructors, instance methods, `this` ─────────────────

#[test]
fn class_constructor_fields_and_instance_methods() {
    let (out, ok) = run("public class Main {\
         static class Point { int x; int y;\
         Point(int x, int y) { this.x = x; this.y = y; }\
         int sum() { return x + y; }\
         void shift(int d) { this.x += d; this.y += d; } }\
         public static void main(String[] a) {\
         Point p = new Point(3, 4); System.out.println(p.sum());\
         p.shift(10); System.out.println(p.x + \",\" + p.y); System.out.println(p.sum()); } }");
    assert!(ok);
    assert_eq!(out, "7\n13,14\n27\n");
}

#[test]
fn instance_field_initializers_run_before_the_constructor() {
    let (out, _) = run("public class Main {\
         static class Acc { int n = 100; int hits;\
         void add(int v) { n += v; hits++; } }\
         public static void main(String[] a) {\
         Acc c = new Acc(); c.add(5); c.add(7);\
         System.out.println(c.n); System.out.println(c.hits); } }");
    // n starts at its initializer 100 → 112; hits starts at its 0 default → 2.
    assert_eq!(out, "112\n2\n");
}

#[test]
fn objects_are_passed_and_assigned_by_reference() {
    let (out, _) = run("public class Main {\
         static class Box { int v; Box(int v) { this.v = v; } }\
         static void set(Box b, int x) { b.v = x; }\
         public static void main(String[] a) {\
         Box b = new Box(1); set(b, 42); System.out.println(b.v);\
         Box b2 = b; b2.v = 99; System.out.println(b.v); } }");
    // Mutation through a passed reference and through an aliased binding both
    // show in the original.
    assert_eq!(out, "42\n99\n");
}

#[test]
fn array_of_objects_constructed_in_a_loop() {
    let (out, _) = run("public class Main {\
         static class P { int x; P(int x) { this.x = x; } int get() { return x; } }\
         public static void main(String[] a) {\
         P[] ps = new P[3]; for (int i = 0; i < ps.length; i++) { ps[i] = new P(i * i); }\
         int s = 0; for (int i = 0; i < ps.length; i++) { s += ps[i].get(); }\
         System.out.println(s); } }");
    // 0 + 1 + 4 = 5.
    assert_eq!(out, "5\n");
}

// ── Inheritance, super(), instanceof, virtual dispatch, toString ────────────

#[test]
fn subclass_inherits_fields_and_super_constructor_runs() {
    let (out, _) = run(
        "public class Main {\
         static class Animal { String name; Animal(String n) { this.name = n; } String describe() { return \"a \" + name; } }\
         static class Dog extends Animal { Dog(String n) { super(n); } String bark() { return name + \" woofs\"; } }\
         public static void main(String[] a) {\
         Dog d = new Dog(\"Rex\");\
         System.out.println(d.name); System.out.println(d.describe()); System.out.println(d.bark()); } }",
    );
    assert_eq!(out, "Rex\na Rex\nRex woofs\n");
}

#[test]
fn overridden_method_dispatches_on_runtime_class() {
    // A supertype-typed array holding subclass instances: each element's own
    // override must run (true virtual dispatch, not static-type dispatch).
    let (out, _) = run(
        "public class Main {\
         static class Shape { double area() { return 0.0; } }\
         static class Circle extends Shape { double r; Circle(double r) { this.r = r; } double area() { return 3.0 * r * r; } }\
         static class Square extends Shape { double s; Square(double s) { this.s = s; } double area() { return s * s; } }\
         public static void main(String[] a) {\
         Shape[] shapes = { new Circle(2.0), new Square(3.0), new Shape() };\
         double t = 0.0; for (int i = 0; i < shapes.length; i++) { t += shapes[i].area(); }\
         System.out.println(t); } }",
    );
    // 12.0 + 9.0 + 0.0 = 21.0.
    assert_eq!(out, "21.0\n");
}

#[test]
fn instanceof_respects_the_subclass_chain() {
    let (out, _) = run("public class Main {\
         static class A {} static class B extends A {}\
         public static void main(String[] a) {\
         A x = new B(); A y = new A();\
         System.out.println(x instanceof A); System.out.println(x instanceof B);\
         System.out.println(y instanceof B); System.out.println(y instanceof A); } }");
    assert_eq!(out, "true\ntrue\nfalse\ntrue\n");
}

#[test]
fn tostring_override_is_used_by_println_and_explicit_call() {
    let (out, _) = run(
        "public class Main {\
         static class Point { int x; int y; Point(int x, int y) { this.x = x; this.y = y; }\
         public String toString() { return \"(\" + x + \",\" + y + \")\"; } }\
         public static void main(String[] a) {\
         Point p = new Point(3, 4);\
         System.out.println(p); System.out.println(p.toString()); System.out.println(\"P=\" + p.toString()); } }",
    );
    assert_eq!(out, "(3,4)\n(3,4)\nP=(3,4)\n");
}

#[test]
fn object_equality_is_reference_identity() {
    let (out, _) = run("public class Main {\
         static class C { int v; C(int v) { this.v = v; } }\
         public static void main(String[] a) {\
         C x = new C(1); C y = new C(1); C z = x;\
         System.out.println(x == y); System.out.println(x == z); } }");
    // Distinct instances are not ==, an aliased binding is.
    assert_eq!(out, "false\ntrue\n");
}

#[test]
fn uninitialized_reference_field_is_null() {
    let (out, _) = run("public class Main {\
         static class N { int v; N next; N(int v) { this.v = v; } }\
         public static void main(String[] a) {\
         N n = new N(5); System.out.println(n.next == null); } }");
    assert_eq!(out, "true\n");
}

// ── Interfaces (abstract + default methods, multiple, polymorphism) ──────────

#[test]
fn interface_dispatch_through_interface_typed_variable() {
    // A variable typed as the interface dispatches to the implementing class's
    // override at runtime.
    let (out, ok) = run("public class Main {\
         interface Speaker { String speak(); }\
         static class Dog implements Speaker { public String speak() { return \"woof\"; } }\
         static class Cat implements Speaker { public String speak() { return \"meow\"; } }\
         public static void main(String[] a) {\
         Speaker s = new Dog(); System.out.println(s.speak());\
         s = new Cat(); System.out.println(s.speak()); } }");
    assert!(ok);
    assert_eq!(out, "woof\nmeow\n");
}

#[test]
fn interface_default_method_calls_abstract_method() {
    // A `default` method with a body may call an abstract method; the call
    // dispatches to the implementor's concrete definition.
    let (out, ok) = run("public class Main {\
         interface Greeter { String name(); default String greet() { return \"Hi, \" + name(); } }\
         static class En implements Greeter { public String name() { return \"Sam\"; } }\
         static class Fancy implements Greeter { public String name() { return \"Sam\"; }\
         public String greet() { return \"Welcome, \" + name(); } }\
         public static void main(String[] a) {\
         Greeter g = new En(); System.out.println(g.greet());\
         Greeter f = new Fancy(); System.out.println(f.greet()); } }");
    assert!(ok);
    // En inherits the default; Fancy overrides it.
    assert_eq!(out, "Hi, Sam\nWelcome, Sam\n");
}

#[test]
fn method_taking_interface_calls_polymorphically() {
    // A method parameter typed as the interface calls `.m()` polymorphically on
    // whatever concrete instance is passed.
    let (out, _) = run(
        "public class Main {\
         interface Shape { double area(); }\
         static class Circle implements Shape { double r; Circle(double r) { this.r = r; } public double area() { return 3.0 * r * r; } }\
         static class Square implements Shape { double s; Square(double s) { this.s = s; } public double area() { return s * s; } }\
         static double total(Shape[] xs) { double t = 0.0; for (int i = 0; i < xs.length; i++) { t += xs[i].area(); } return t; }\
         public static void main(String[] a) {\
         Shape[] xs = { new Circle(2.0), new Square(3.0) };\
         System.out.println(total(xs)); } }",
    );
    // 12.0 + 9.0 = 21.0.
    assert_eq!(out, "21.0\n");
}

#[test]
fn class_implementing_multiple_interfaces_and_instanceof() {
    // A class implementing two interfaces satisfies `instanceof` for both, and
    // an instance of a class implementing only one does not satisfy the other.
    let (out, _) = run(
        "public class Main {\
         interface Named { String name(); }\
         interface Aged { int age(); }\
         static class Person implements Named, Aged { public String name() { return \"Al\"; } public int age() { return 30; } }\
         static class Robot implements Named { public String name() { return \"R2\"; } }\
         public static void main(String[] a) {\
         Person p = new Person(); Robot r = new Robot();\
         System.out.println(p.name() + \" \" + p.age());\
         System.out.println(p instanceof Named); System.out.println(p instanceof Aged);\
         System.out.println(r instanceof Named); System.out.println(r instanceof Aged); } }",
    );
    assert_eq!(out, "Al 30\ntrue\ntrue\ntrue\nfalse\n");
}

#[test]
fn interface_extends_interface_default_override() {
    // A sub-interface may override a super-interface `default`, and a class with
    // no override inherits the most-specific default.
    let (out, _) = run("public class Main {\
         interface A { default String who() { return \"A\"; } }\
         interface B extends A { default String who() { return \"B+\" + tag(); } String tag(); }\
         static class C implements B { public String tag() { return \"c\"; } }\
         static class D implements A { }\
         public static void main(String[] x) {\
         A a = new C(); System.out.println(a.who());\
         A d = new D(); System.out.println(d.who());\
         System.out.println(a instanceof B); System.out.println(d instanceof B); } }");
    assert_eq!(out, "B+c\nA\ntrue\nfalse\n");
}

#[test]
fn instantiating_an_interface_is_an_error() {
    let (_out, ok) = run("public class Main { interface I { void m(); }\
         public static void main(String[] a) { I x = new I(); } }");
    assert!(!ok, "an interface must not be instantiable");
}

// ── Method overloading by parameter type ─────────────────────────────────────

#[test]
fn static_overload_resolves_by_primitive_and_string_type() {
    // Same name + arity, different parameter type: the static argument type
    // selects the overload.
    let (out, ok) = run("public class Main {\
         static String f(int x) { return \"int:\" + x; }\
         static String f(String x) { return \"str:\" + x; }\
         static String f(double x) { return \"dbl:\" + x; }\
         public static void main(String[] a) {\
         System.out.println(f(5)); System.out.println(f(\"hi\")); System.out.println(f(3.5)); } }");
    assert!(ok);
    assert_eq!(out, "int:5\nstr:hi\ndbl:3.5\n");
}

#[test]
fn overload_by_reference_type_picks_most_specific() {
    // Dog is more specific than Animal, which is more specific than Object; a
    // Dog argument selects `g(Dog)`, and a supertype-typed argument selects the
    // overload for its static type.
    let (out, _) = run("public class Main {\
         static class Animal { String n; Animal(String n) { this.n = n; } }\
         static class Dog extends Animal { Dog(String n) { super(n); } }\
         static String g(Animal x) { return \"animal:\" + x.n; }\
         static String g(Dog x) { return \"dog:\" + x.n; }\
         static String g(Object x) { return \"obj\"; }\
         public static void main(String[] a) {\
         System.out.println(g(new Dog(\"Rex\")));\
         System.out.println(g(new Animal(\"Cat\")));\
         Animal poly = new Dog(\"Poly\"); System.out.println(g(poly));\
         System.out.println(g(\"plain\")); } }");
    // Dog -> g(Dog); Animal -> g(Animal); Animal-typed Dog -> g(Animal) (static
    // type wins for overload choice); String -> g(Object).
    assert_eq!(out, "dog:Rex\nanimal:Cat\nanimal:Poly\nobj\n");
}

#[test]
fn constructor_overloading_by_type() {
    let (out, _) = run("public class Main {\
         static class Box { String tag;\
         Box(int x) { this.tag = \"fromInt:\" + x; }\
         Box(String s) { this.tag = \"fromStr:\" + s; } }\
         public static void main(String[] a) {\
         System.out.println(new Box(7).tag); System.out.println(new Box(\"hey\").tag); } }");
    assert_eq!(out, "fromInt:7\nfromStr:hey\n");
}

#[test]
fn instance_method_overloading_by_type() {
    let (out, _) = run("public class Main {\
         static class P {\
         String d(int n) { return \"int:\" + n; }\
         String d(String s) { return \"str:\" + s; } }\
         public static void main(String[] a) {\
         P p = new P(); System.out.println(p.d(9)); System.out.println(p.d(\"x\")); } }");
    assert_eq!(out, "int:9\nstr:x\n");
}

#[test]
fn overriding_one_overload_dispatches_virtually() {
    // Sub overrides only the `int` overload; a Base-typed reference to a Sub
    // instance runs Sub's `int` override and Base's inherited `String` overload.
    let (out, _) = run(
        "public class Main {\
         static class Base { String h(int x) { return \"B-int:\" + x; } String h(String s) { return \"B-str:\" + s; } }\
         static class Sub extends Base { String h(int x) { return \"S-int:\" + (x + 1); } }\
         public static void main(String[] a) {\
         Base b = new Sub(); System.out.println(b.h(10)); System.out.println(b.h(\"hi\")); } }",
    );
    assert_eq!(out, "S-int:11\nB-str:hi\n");
}

// ── Generics (type-erased, like javac) ───────────────────────────────────────

#[test]
fn generic_class_box_erases_and_runs() {
    // A `Box<T>` with a T field, T-returning getter, and T-taking setter; the
    // diamond `new Box<>()` and explicit `new Box<String>()` both work.
    let (out, ok) = run(
        "public class Main {\
         static class Box<T> { T v; Box(T v) { this.v = v; } T get() { return v; } void set(T x) { this.v = x; } }\
         public static void main(String[] a) {\
         Box<Integer> bi = new Box<>(5); System.out.println(bi.get());\
         bi.set(42); System.out.println(bi.get());\
         Box<String> bs = new Box<String>(\"hello\"); System.out.println(bs.get()); } }",
    );
    assert!(ok);
    assert_eq!(out, "5\n42\nhello\n");
}

#[test]
fn generic_class_with_two_type_params() {
    let (out, _) = run("public class Main {\
         static class Pair<K, V> { K k; V val; Pair(K k, V val) { this.k = k; this.val = val; }\
         String show() { return k + \"=\" + val; } }\
         public static void main(String[] a) {\
         Pair<String, Integer> p = new Pair<>(\"age\", 30); System.out.println(p.show()); } }");
    assert_eq!(out, "age=30\n");
}

#[test]
fn generic_static_method_and_bounded_type_param() {
    // `<T> T id(T x)` and a bounded `<T extends Number>` both parse and run
    // (erased at runtime).
    let (out, _) = run(
        "public class Main {\
         static <T> T id(T x) { return x; }\
         static <T extends Number> String describe(T n) { return \"num:\" + n; }\
         public static void main(String[] a) {\
         System.out.println(id(99)); System.out.println(id(\"str\")); System.out.println(describe(7)); } }",
    );
    assert_eq!(out, "99\nstr\nnum:7\n");
}

// ── Multi-dimensional arrays, String.format, Arrays.toString ─────────────────

#[test]
fn multidimensional_array_allocate_index_and_length() {
    let (out, ok) = run("public class Main { public static void main(String[] a) {\
         int[][] g = new int[2][3];\
         for (int i = 0; i < 2; i++) { for (int j = 0; j < 3; j++) { g[i][j] = i * 10 + j; } }\
         System.out.println(g[0][0] + \" \" + g[1][2]);\
         System.out.println(g.length + \" \" + g[0].length); } }");
    assert!(ok);
    assert_eq!(out, "0 12\n2 3\n");
}

#[test]
fn nested_array_literal_including_jagged_rows() {
    let (out, _) = run(
        "public class Main { public static void main(String[] a) {\
         int[][] lit = {{1, 2}, {3, 4, 5}};\
         System.out.println(lit[1][2]); System.out.println(lit[0].length + \" \" + lit[1].length); } }",
    );
    assert_eq!(out, "5\n2 3\n");
}

#[test]
fn arrays_to_string_of_int_and_string_arrays() {
    let (out, _) = run("import java.util.Arrays;\
         public class Main { public static void main(String[] a) {\
         int[] xs = {5, 10, 15}; String[] ws = {\"a\", \"b\"};\
         System.out.println(Arrays.toString(xs));\
         System.out.println(Arrays.toString(ws)); } }");
    assert_eq!(out, "[5, 10, 15]\n[a, b]\n");
}

#[test]
fn string_format_conversions_flags_and_width() {
    let (out, _) = run("public class Main { public static void main(String[] a) {\
         System.out.println(String.format(\"%d-%s-%.2f\", 7, \"hi\", 3.14159));\
         System.out.println(String.format(\"[%5d][%-5d][%05d]\", 42, 42, 42));\
         System.out.println(String.format(\"%x %b %+d %%\", 255, true, 8)); } }");
    assert_eq!(out, "7-hi-3.14\n[   42][42   ][00042]\nff true +8 %\n");
}

#[test]
fn generic_interface_method_dispatches_to_erased_override() {
    // A generic interface method `apply(T)` is implemented with the concrete
    // erased type `apply(String)`; dispatch through the interface-typed variable
    // must reach that override (erasure links the differing raw signatures). The
    // `default tag()` calls the abstract method on `this`.
    let (out, ok) = run(
        "public class Main {\
         interface Transform<T> { String apply(T x); default String tag() { return \"t:\" + apply(null); } }\
         static class Upper implements Transform<String> { public String apply(String s) { return s == null ? \"<null>\" : s.toUpperCase(); } }\
         static class Wrap implements Transform<Integer> { public String apply(Integer n) { return \"[\" + n + \"]\"; } }\
         public static void main(String[] a) {\
         Transform<String> t = new Upper(); System.out.println(t.apply(\"hi\")); System.out.println(t.tag());\
         Transform<Integer> w = new Wrap(); System.out.println(w.apply(5));\
         System.out.println(t instanceof Transform); } }",
    );
    assert!(ok);
    assert_eq!(out, "HI\nt:<null>\n[5]\ntrue\n");
}

#[test]
fn erased_library_generic_type_args_parse() {
    // `List<String>` / `Map<K,V>` type arguments (and a nested generic bound)
    // parse and erase — the declared references default to null.
    let (out, _) = run(
        "import java.util.List;\
         import java.util.Map;\
         public class Main {\
         static class Holder<T extends Comparable<T>> { T val; Holder(T v) { this.val = v; } T value() { return val; } }\
         public static void main(String[] a) {\
         List<String> xs = null; Map<String, Integer> m = null;\
         System.out.println(xs == null); System.out.println(m == null);\
         Holder<Integer> h = new Holder<>(8); System.out.println(h.value()); } }",
    );
    assert_eq!(out, "true\ntrue\n8\n");
}

#[test]
fn enhanced_for_iterates_arrays_once() {
    // The array expression must be evaluated exactly once (the `made()` probe
    // prints on each call), the loop variable is rebound per element, and the
    // element's declared type still drives `/` typing (`d / 2` is floating).
    // Verified against OpenJDK 26.
    let (out, ok) = run("public class Main {\
         static int[] made() { System.out.println(\"made\"); return new int[]{5, 6}; }\
         static int sum(int[] a) { int t = 0; for (int v : a) { t += v; } return t; }\
         public static void main(String[] x) {\
         String[] names = {\"ann\", \"bo\"};\
         for (String s : names) { System.out.println(s.toUpperCase()); }\
         for (int v : made()) { System.out.println(v); }\
         System.out.println(sum(new int[]{1, 2, 3}));\
         double[] ds = {1.0, 5.0}; for (double d : ds) { System.out.println(d / 2); }\
         for (var s : names) { System.out.println(s.length()); }\
         for (String s : new String[0]) { System.out.println(\"never\"); } } }");
    assert!(ok);
    assert_eq!(out, "ANN\nBO\nmade\n5\n6\n6\n0.5\n2.5\n3\n2\n");
}

#[test]
fn enhanced_for_honours_labeled_break_and_continue() {
    // A labeled `continue`/`break` must target the named enhanced-`for` exactly
    // as it targets a C-style one. Verified against OpenJDK 26.
    let (out, ok) = run("public class Main { public static void main(String[] a) {\
         int[][] grid = {{1, 2}, {3, 4}, {5, 6}};\
         int total = 0;\
         outer: for (int[] row : grid) { for (int v : row) {\
         if (v == 4) { continue outer; } if (v == 5) { break outer; } total += v; } }\
         System.out.println(total); } }");
    assert!(ok);
    assert_eq!(out, "6\n");
}

#[test]
fn exceptions_propagate_across_call_frames_to_a_handler() {
    // The crux of the lowering: a `throw` several frames deep must unwind every
    // frame and land in the caller's `catch`, and a `catch` arm matches by the
    // thrown object's class hierarchy. Verified against OpenJDK 26.
    let (out, ok) = run("public class Main {\
         static void a() { b(); }\
         static void b() { throw new IllegalArgumentException(\"deep\"); }\
         static int guarded() { try { a(); return 1; } catch (RuntimeException e) { return 2; } }\
         public static void main(String[] x) {\
         System.out.println(guarded());\
         try { a(); } catch (Exception e) { System.out.println(\"caught \" + e.getMessage()); }\
         try { a(); } catch (Exception e) { System.out.println(e); } } }");
    assert!(ok);
    assert_eq!(
        out,
        "2\ncaught deep\njava.lang.IllegalArgumentException: deep\n"
    );
}

#[test]
fn catch_arms_match_in_source_order_and_finally_always_runs() {
    // The first arm whose type matches wins (a `NumberFormatException` is an
    // `IllegalArgumentException`), `finally` runs on both the normal and the
    // exceptional path, and an unmatched exception continues outward *after*
    // the inner `finally`. Verified against OpenJDK 26.
    let (out, ok) = run(
        "public class Main { public static void main(String[] a) {\
         try { throw new NumberFormatException(\"nfe\"); }\
         catch (IllegalStateException e) { System.out.println(\"ise\"); }\
         catch (IllegalArgumentException e) { System.out.println(\"iae \" + e.getMessage()); }\
         catch (RuntimeException e) { System.out.println(\"rte\"); }\
         finally { System.out.println(\"fin1\"); }\
         try { System.out.println(\"ok\"); } finally { System.out.println(\"fin2\"); }\
         try { try { throw new IllegalStateException(\"inner\"); } finally { System.out.println(\"fin3\"); } }\
         catch (RuntimeException e) { System.out.println(\"outer \" + e.getMessage()); }\
         System.out.println(new RuntimeException().getMessage()); } }",
    );
    assert!(ok);
    assert_eq!(out, "iae nfe\nfin1\nok\nfin2\nfin3\nouter inner\nnull\n");
}

#[test]
fn user_exception_subclass_carries_its_own_state() {
    // A user class extending a modeled `java.lang` throwable chains through
    // `super(m)`, keeps its own fields, and satisfies `instanceof` on both its
    // own name and its inherited ones. Verified against OpenJDK 26.
    let (out, ok) = run(
        "public class Main {\
         static class MyError extends RuntimeException { int code; MyError(String m, int c) { super(m); code = c; } }\
         static void boom() { throw new MyError(\"bad\", 7); }\
         public static void main(String[] a) {\
         try { boom(); } catch (RuntimeException e) {\
         System.out.println(e.getMessage());\
         System.out.println(e instanceof MyError);\
         System.out.println(e instanceof Exception); } } }",
    );
    assert!(ok);
    assert_eq!(out, "bad\ntrue\ntrue\n");
}

#[test]
fn uncaught_exception_reports_and_exits_nonzero() {
    // An exception no handler claims prints Java's `Exception in thread "main"`
    // line (on stderr, with javars's error prefix) and fails the run — output
    // written before the throw is still flushed.
    let (out, err, ok) = run_streams(
        "public class Main {\
         static int f(int n) { if (n > 2) { throw new IllegalStateException(\"big\"); } return n; }\
         public static void main(String[] a) {\
         System.out.println(f(1)); System.out.println(f(9)); System.out.println(\"never\"); } }",
    );
    assert!(!ok);
    assert_eq!(out, "1\n");
    assert!(
        err.contains("Exception in thread \"main\" java.lang.IllegalStateException: big"),
        "stderr was {err:?}"
    );
}

#[test]
fn throwing_in_a_loop_does_not_grow_the_value_stack() {
    // Each throw abandons a half-evaluated expression; the handler must discard
    // those operands (JEXC_CUT) or a loop would leak one value per iteration.
    // 20_000 iterations would be visible as unbounded growth.
    let (out, ok) = run(
        "public class Main {\
         static int boom(int i) { throw new RuntimeException(\"x\"); }\
         public static void main(String[] a) {\
         int n = 0;\
         for (int i = 0; i < 20000; i++) { try { n += boom(i) + boom(i); } catch (RuntimeException e) { n++; } }\
         System.out.println(n); } }",
    );
    assert!(ok);
    assert_eq!(out, "20000\n");
}

#[test]
fn enum_constants_are_singletons_with_name_and_ordinal() {
    // Constants are one instance each, created before `main`, so `==` is
    // identity and `values()` hands them back in declaration order. `name()`,
    // `ordinal()`, `toString()`, `valueOf`, an unqualified `switch` label, an
    // enum-typed parameter, and a body of the enum's own all work off that.
    // Verified against OpenJDK 26.
    let (out, ok) = run("enum Op {\
         ADD, SUB, MUL;\
         int apply(int a, int b) { switch (this) { case ADD: return a + b; case SUB: return a - b; default: return a * b; } }\
         boolean isMul() { return this == MUL; } }\
         public class Main {\
         static String describe(Op o) { return o.name() + \"/\" + o.ordinal(); }\
         public static void main(String[] a) {\
         for (Op o : Op.values()) { System.out.println(describe(o) + \" \" + o.apply(6, 3) + \" \" + o.isMul()); }\
         System.out.println(Op.values().length + \" \" + (Op.ADD == Op.ADD) + \" \" + Op.ADD.equals(Op.SUB));\
         System.out.println(Op.valueOf(\"MUL\").apply(2, 3));\
         Op v = Op.SUB;\
         switch (v) { case ADD: System.out.println(\"a\"); break; case SUB: System.out.println(\"s\"); break; default: System.out.println(\"m\"); }\
         try { Op.valueOf(\"nope\"); } catch (IllegalArgumentException e) { System.out.println(e.getMessage()); } } }");
    assert!(ok);
    assert_eq!(
        out,
        "ADD/0 9 false\nSUB/1 3 false\nMUL/2 18 true\n\
         3 true false\n6\ns\nNo enum constant Op.nope\n"
    );
}

#[test]
fn an_enum_constant_with_arguments_is_rejected() {
    // javars has no per-constant state, so running `EARTH(5.97)` as a bare
    // `EARTH` would silently drop the mass. The program is refused instead.
    let (_, ok) = run("enum Planet { EARTH(5.97), MARS(0.64); double mass; }\
         public class Main { public static void main(String[] a) { System.out.println(Planet.EARTH); } }");
    assert!(!ok);
}

#[test]
fn a_runtime_fault_is_a_catchable_exception() {
    // javars's own faults — array index, `Integer.parseInt`, integral `/ 0`,
    // a negative array size, a `String` index — raise the throwable Java raises,
    // with Java's detail message, catchable by any supertype and observable
    // through `getMessage()`/`toString()`. Outputs verified against OpenJDK 26.
    let (out, ok) = run("public class Main {\
         static int deep(int[] q, int i) { return q[i] * 2; }\
         public static void main(String[] a) {\
         int[] q = {1, 2, 3}; int[] empty = {};\
         try { System.out.println(q[5]); } catch (ArrayIndexOutOfBoundsException e) { System.out.println(e.getMessage()); }\
         try { Integer.parseInt(\"abc\"); } catch (NumberFormatException e) { System.out.println(e.getMessage()); }\
         try { System.out.println(5 / empty.length); } catch (ArithmeticException e) { System.out.println(e); }\
         try { System.out.println(5 % empty.length); } catch (ArithmeticException e) { System.out.println(e.getMessage()); }\
         try { int[] neg = new int[empty.length - 2]; } catch (NegativeArraySizeException e) { System.out.println(e.getMessage()); }\
         try { \"abcd\".charAt(9); } catch (StringIndexOutOfBoundsException e) { System.out.println(e.getMessage()); }\
         try { deep(q, 9); } catch (RuntimeException e) { System.out.println(\"deep \" + e); } } }");
    assert!(ok);
    assert_eq!(
        out,
        "Index 5 out of bounds for length 3\n\
         For input string: \"abc\"\n\
         java.lang.ArithmeticException: / by zero\n\
         / by zero\n\
         -2\n\
         Index 9 out of bounds for length 4\n\
         deep java.lang.ArrayIndexOutOfBoundsException: Index 9 out of bounds for length 3\n"
    );
}

#[test]
fn an_uncaught_runtime_fault_exits_non_zero() {
    // With no handler anywhere the fault still ends the program the way `java`
    // does: the output written before it survives, the rest never runs, and the
    // exit status is non-zero.
    let (out, ok) = run("public class Main {\
         public static void main(String[] a) {\
         int[] q = {1, 2, 3};\
         System.out.println(\"before\");\
         System.out.println(q[9]);\
         System.out.println(\"after\"); } }");
    assert!(!ok);
    assert_eq!(out, "before\n");
}

#[test]
fn try_with_resources_closes_in_reverse_order() {
    // The resources close in reverse declaration order, before the outer
    // `catch`/`finally` runs, on both the normal and the exceptional path — and
    // a `return` out of the block closes them too. Verified against OpenJDK 26.
    let (out, ok) = run("public class Main {\
         static int f() { try (Res r = new Res(\"r\")) { return 7; } }\
         public static void main(String[] a) {\
         try (Res x = new Res(\"a\"); Res y = new Res(\"b\")) { System.out.println(\"body\"); }\
         try (Res x = new Res(\"c\")) { throw new IllegalStateException(\"boom\"); }\
         catch (IllegalStateException e) { System.out.println(\"caught \" + e.getMessage()); }\
         System.out.println(f()); } }\
         class Res implements AutoCloseable {\
         String n;\
         Res(String n) { this.n = n; System.out.println(\"open \" + n); }\
         public void close() { System.out.println(\"close \" + n); } }");
    assert!(ok);
    assert_eq!(
        out,
        "open a\nopen b\nbody\nclose b\nclose a\n\
         open c\nclose c\ncaught boom\n\
         open r\nclose r\n7\n"
    );
}

#[test]
fn a_jump_out_of_a_try_runs_its_finally_first() {
    // `return`/`break`/`continue` leaving a guarded region each emit the
    // cleanup block before taking the jump — innermost first, and only for the
    // blocks actually being left (`brk`'s `finally` is inside the loop, so it
    // runs on every iteration up to the `break`). The returned value is fixed
    // before the cleanup runs, so `keep`'s reassignment cannot change it.
    // Outputs verified against OpenJDK 26.
    let (out, ok) = run("public class Main {\
         static int ret() { try { return 1; } finally { System.out.println(\"fin\"); } }\
         static int keep() { int x = 5; try { return x; } finally { x = 99; } }\
         static int nest() { try { try { return 3; } finally { System.out.println(\"in\"); } } finally { System.out.println(\"out\"); } }\
         static int brk() { int t = 0; for (int i = 0; i < 4; i++) { try { if (i == 2) { break; } t += i; } finally { System.out.println(\"b\" + i); } } return t; }\
         static int over() { try { return 6; } finally { return 66; } }\
         public static void main(String[] a) {\
         System.out.println(ret()); System.out.println(keep());\
         System.out.println(nest()); System.out.println(brk());\
         System.out.println(over()); } }");
    assert!(ok);
    assert_eq!(out, "fin\n1\n5\nin\nout\n3\nb0\nb1\nb2\n1\n66\n");
}

#[test]
fn a_throw_from_a_catch_arm_still_runs_the_finally() {
    // The cleanup block guards the catch arms too: an exception raised inside a
    // handler runs the `finally` on its way out, and reaches the *enclosing*
    // handler — not this try's own arms. Verified against OpenJDK 26.
    let (out, ok) = run("public class Main {\
         public static void main(String[] a) {\
         try {\
           try { throw new IllegalStateException(\"one\"); }\
           catch (IllegalStateException e) { throw new RuntimeException(\"two\"); }\
           finally { System.out.println(\"fin\"); }\
         } catch (RuntimeException e) { System.out.println(\"outer \" + e.getMessage()); } } }");
    assert!(ok);
    assert_eq!(out, "fin\nouter two\n");
}

#[test]
fn int_arithmetic_wraps_at_32_bits_and_long_does_not() {
    // Java's `int` is 32-bit and wraps; `long` is 64-bit. The distinction is
    // static, so it has to survive locals, parameters, compound assignment,
    // `++`, fields, and array elements. Verified against OpenJDK 26.
    let (out, ok) = run("public class Main {\
         static int mul(int a, int b) { return a * b; }\
         static class Acc { int v; }\
         public static void main(String[] a) {\
         int max = 2147483647; int min = -2147483648;\
         System.out.println(max + 1); System.out.println(min - 1);\
         System.out.println(max * max); System.out.println(-min); System.out.println(min / -1);\
         System.out.println(mul(100000, 100000));\
         long lmax = 2147483647; System.out.println(lmax + 1); System.out.println(lmax * lmax);\
         int x = max; x += 1; System.out.println(x);\
         int c = max; c++; System.out.println(c);\
         Acc acc = new Acc(); acc.v = max; acc.v += 5; System.out.println(acc.v);\
         int[] arr = {max}; arr[0] *= 3; System.out.println(arr[0]);\
         System.out.println(Integer.parseInt(\"2147483647\") + 1);\
         System.out.println(Long.parseLong(\"2147483647\") + 1);\
         System.out.println(Math.abs(max) * 2); } }");
    assert!(ok);
    assert_eq!(
        out,
        "-2147483648\n2147483647\n1\n-2147483648\n-2147483648\n1410065408\n2147483648\n4611686014132420609\n-2147483648\n-2147483648\n-2147483644\n2147483645\n-2147483648\n2147483648\n-2\n"
    );
}

#[test]
fn int_hash_loop_matches_the_jdk() {
    // The classic overflow-sensitive shape: a multiply-accumulate that only
    // agrees with Java once every step wraps. Verified against OpenJDK 26.
    let (out, ok) = run("public class Main { public static void main(String[] a) {\
         int seed = 12345;\
         for (int i = 0; i < 20; i++) { seed = seed * 1103515245 + 12345; }\
         System.out.println(seed);\
         int h = 0; String s = \"hello world\";\
         for (int i = 0; i < s.length(); i++) { h = h * 31 + s.indexOf(s.substring(i, i + 1)); }\
         System.out.println(h); } }");
    assert!(ok);
    assert_eq!(out, "-804247707\n-921940528\n");
}

#[test]
fn static_fields_are_one_cell_per_class() {
    // A `static` field is class-level state: it exists before `main`, survives
    // every instance, and the qualified and unqualified forms name one cell.
    // Verified against OpenJDK 26.
    let (out, ok) = run("public class Main {\
         static int n = 5;\
         static String label = \"c\";\
         static int[] arr = {1, 2, 3};\
         static int derived;\
         static { derived = n * 4; }\
         static int bump() { n++; return n; }\
         public static void main(String[] a) {\
         System.out.println(n + \",\" + Main.n + \",\" + label + \",\" + derived);\
         n += 2; Main.n *= 3; n--;\
         System.out.println(n + \",\" + bump() + \",\" + Main.bump());\
         arr[1] = 9;\
         System.out.println(arr[0] + \",\" + arr[1] + \",\" + arr.length);\
         System.out.println(n / 4 + \",\" + n % 4); } }");
    assert!(ok);
    assert_eq!(out, "5,5,c,20\n20,21,22\n1,9,3\n5,2\n");
}

#[test]
fn static_fields_are_shared_across_instances_and_subclasses() {
    // The counter every instance increments is the same cell an inheriting
    // class reads. Verified against OpenJDK 26.
    let (out, ok) = run(
        "public class Main { public static void main(String[] a) {\
         System.out.println(C.count);\
         C first = new C(); new C(); new C();\
         System.out.println(C.count + \",\" + D.count + \",\" + first.id + \",\" + C.describe()); } }\
         class C { static int count; int id; C() { count++; id = count; }\
         static String describe() { return \"n=\" + count; } }\
         class D extends C { }",
    );
    assert!(ok);
    assert_eq!(out, "0\n3,3,1,n=3\n");
}

#[test]
fn main_args_carries_the_program_arguments() {
    // `main`'s `String[]` parameter is the real argv — indexable, iterable, and
    // zero-length (never null) when none are passed. Verified against OpenJDK 26.
    let src = "public class Main { public static void main(String[] args) {\
         System.out.println(args.length);\
         for (int i = 0; i < args.length; i++) { System.out.println(i + \"=\" + args[i]); }\
         for (String s : args) { System.out.println(s.toUpperCase()); } } }";
    let (out, ok) = run_args(src, &["alpha", "b c"]);
    assert!(ok);
    assert_eq!(out, "2\n0=alpha\n1=b c\nALPHA\nB C\n");
    let (out, ok) = run_args(src, &[]);
    assert!(ok);
    assert_eq!(out, "0\n");
}

#[test]
fn enum_constants_carry_per_constant_state() {
    // A constant with constructor arguments runs the enum's own constructor, so
    // each singleton keeps its own field values. Verified against OpenJDK 26.
    let (out, ok) = run(
        "public class Main { public static void main(String[] a) {\
         for (P p : P.values()) { System.out.println(p + \" \" + p.ordinal() + \" \" + p.mass() + \" \" + p.heavy()); }\
         System.out.println(P.valueOf(\"EARTH\").mass()); } }\
         enum P { MERCURY(3.3), EARTH(5.97), JUPITER(1898.0);\
         private final double mass;\
         P(double m) { this.mass = m; }\
         double mass() { return mass; }\
         boolean heavy() { return mass > 100.0; } }",
    );
    assert!(ok);
    assert_eq!(
        out,
        "MERCURY 0 3.3 false\nEARTH 1 5.97 false\nJUPITER 2 1898.0 true\n5.97\n"
    );
}

#[test]
fn enum_constants_with_bodies_override_per_constant() {
    // A constant body is an anonymous subclass: its override is what the enum's
    // abstract method dispatches to, and a constant that declares no override
    // inherits the enum's own body. Verified against OpenJDK 26.
    let (out, ok) = run(
        "public class Main { public static void main(String[] a) {\
         for (O o : O.values()) { System.out.println(o + \" \" + o.apply(6, 3) + \" \" + o.label()); }\
         O o = O.TIMES;\
         switch (o) { case TIMES: System.out.println(\"times\"); break; default: System.out.println(\"other\"); }\
         System.out.println((o == O.valueOf(\"TIMES\")) + \" \" + (o instanceof O)); } }\
         enum O { PLUS { int apply(int x, int y) { return x + y; } },\
         MINUS { int apply(int x, int y) { return x - y; } },\
         TIMES { int apply(int x, int y) { return x * y; } String label() { return \"x\"; } };\
         abstract int apply(int x, int y);\
         String label() { return name().toLowerCase(); } }",
    );
    assert!(ok);
    assert_eq!(
        out,
        "PLUS 9 plus\nMINUS 3 minus\nTIMES 18 x\ntimes\ntrue true\n"
    );
}

#[test]
fn records_derive_accessors_tostring_and_equals() {
    // A record's components become final fields, the canonical constructor, one
    // accessor each, `toString` in `Name[c=v, …]` form, and a component-wise
    // `equals`. A compact constructor validates before the fields are assigned.
    // Verified against OpenJDK 26.
    let (out, ok) = run(
        "public class Main {\
         record Pt(int x, int y) { int sum() { return x + y; } }\
         record Tag(String s, double d, boolean b) { }\
         record Ord(int lo, int hi) { Ord { if (lo > hi) { throw new IllegalArgumentException(lo + \">\" + hi); } } }\
         public static void main(String[] a) {\
         Pt p = new Pt(1, 2);\
         System.out.println(p + \" \" + p.x() + \" \" + p.y() + \" \" + p.sum());\
         System.out.println(p.equals(new Pt(1, 2)) + \" \" + p.equals(new Pt(2, 1)));\
         System.out.println(new Tag(\"hi\", 2.5, true));\
         System.out.println(new Ord(2, 9));\
         try { new Ord(9, 2); } catch (IllegalArgumentException e) { System.out.println(e.getMessage()); } } }",
    );
    assert!(ok);
    assert_eq!(
        out,
        "Pt[x=1, y=2] 1 2 3\ntrue false\nTag[s=hi, d=2.5, b=true]\nOrd[lo=2, hi=9]\n9>2\n"
    );
}

#[test]
fn tostring_dispatches_through_an_interface_typed_receiver() {
    // The static type declares no `toString`, but the runtime class does — so
    // the override still has to be the one that renders. Verified against
    // OpenJDK 26.
    let (out, ok) = run("public class Main { public static void main(String[] a) {\
         S[] xs = { new Cir(2.0), new Sq(3) };\
         for (S s : xs) { System.out.println(s + \" \" + s.area()); } } }\
         interface S { double area(); }\
         record Cir(double r) implements S { public double area() { return 3.0 * r * r; } }\
         record Sq(int side) implements S { public double area() { return side * 1.0 * side; } }");
    assert!(ok);
    assert_eq!(out, "Cir[r=2.0] 12.0\nSq[side=3] 9.0\n");
}

#[test]
fn lambdas_implement_functional_interfaces() {
    // A lambda assigned to a single-abstract-method interface is invoked through
    // that method — expression and block bodies, a captured effectively-final
    // local, an interface with both a lambda and a class implementor (so the
    // dispatch chain has to pick the right arm), and the target interface's
    // declared `int` parameters driving both `/` truncation and the 32-bit wrap
    // inside the body. Verified against OpenJDK 26.
    let (out, ok) = run(
        "public class Main {\
         interface Calc { int of(int a, int b); }\
         interface Named { String label(); default String shout() { return label() + \"!\"; } }\
         static class Impl implements Named { public String label() { return \"impl\"; } }\
         static int fold(Calc c, int[] xs) { int acc = xs[0]; for (int i = 1; i < xs.length; i++) { acc = c.of(acc, xs[i]); } return acc; }\
         static String use(Named n) { return n.shout(); }\
         public static void main(String[] a) {\
         Calc add = (x, y) -> x + y;\
         Calc mul = (x, y) -> { return x * y; };\
         System.out.println(add.of(3, 4) + \" \" + mul.of(3, 4));\
         int base = 10;\
         Calc off = (x, y) -> x + y + base;\
         System.out.println(off.of(1, 2));\
         System.out.println(fold(add, new int[]{1,2,3,4}) + \" \" + fold((x, y) -> x / y, new int[]{100, 3, 2}));\
         System.out.println(use(new Impl()) + \" \" + use(() -> \"lam\"));\
         Calc big = (x, y) -> x * y;\
         System.out.println(big.of(100000, 100000)); } }",
    );
    assert!(ok);
    assert_eq!(out, "7 12\n13\n10 16\nimpl! lam!\n1410065408\n");
}

#[test]
fn lambdas_capture_by_value_and_method_references_desugar() {
    // The capture is a by-value snapshot taken where the literal runs, which is
    // the only model that gives the enhanced `for` Java's per-iteration capture
    // (`a!b!c!`, not `c!c!c!`) and lets a lambda outlive the frame that built it
    // (`adder(5)`). Also covers capturing an instance field and a `static`
    // through `this`, the three method-reference shapes, a lambda returning a
    // lambda, `finally` inside a lambda body, and a throw out of one. Verified
    // against OpenJDK 26.
    let (out, ok) = run(
        "import java.util.function.*;\
         public class Main {\
         interface Calc { int of(int a); }\
         static int k = 5;\
         int f = 3;\
         String viaField() { Supplier<String> s = () -> \"f=\" + f + \",k=\" + k; return s.get(); }\
         static Calc adder(int n) { return x -> x + n; }\
         public static void main(String[] a) {\
         String[] names = {\"a\", \"b\", \"c\"};\
         Supplier<String>[] subs = new Supplier[3];\
         int i = 0;\
         for (String s : names) { subs[i] = () -> s + \"!\"; i++; }\
         for (Supplier<String> s : subs) { System.out.print(s.get()); }\
         System.out.println();\
         System.out.println(adder(5).of(10));\
         System.out.println(new Main().viaField());\
         Function<String, Integer> len = String::length;\
         Function<String, Integer> pi = Integer::parseInt;\
         Consumer<String> pr = System.out::println;\
         System.out.println(len.apply(\"hello\") + \" \" + pi.apply(\"123\"));\
         pr.accept(\"ref\");\
         Function<Integer, Function<Integer, Integer>> add = p -> q -> p + q;\
         System.out.println(add.apply(3).apply(4));\
         Calc fin = x -> { try { return x * 2; } finally { System.out.println(\"fin \" + x); } };\
         System.out.println(fin.of(4));\
         Calc boom = x -> { if (x == 0) { throw new IllegalStateException(\"zero\"); } return 100 / x; };\
         try { System.out.println(boom.of(0)); } catch (IllegalStateException e) { System.out.println(\"caught \" + e.getMessage()); }\
         System.out.println(boom.of(4)); } }",
    );
    assert!(ok);
    assert_eq!(
        out,
        "a!b!c!\n15\nf=3,k=5\n5 123\nref\n7\nfin 4\n8\ncaught zero\n25\n"
    );
}

#[test]
fn an_uncaught_throw_from_a_lambda_exits_non_zero() {
    // A lambda body is a real fusevm call frame, so a throw with no handler in
    // it unwinds to the caller's — and, with no handler anywhere, reports Java's
    // uncaught line and exits non-zero rather than resuming with a placeholder.
    let (out, err, ok) = run_streams(
        "public class Main {\
         interface Calc { int of(int a); }\
         public static void main(String[] a) {\
         Calc boom = x -> { throw new IllegalArgumentException(\"bad \" + x); };\
         System.out.println(\"before\");\
         System.out.println(boom.of(7));\
         System.out.println(\"unreached\"); } }",
    );
    assert!(!ok);
    assert_eq!(out, "before\n");
    assert!(
        err.contains("Exception in thread \"main\" java.lang.IllegalArgumentException: bad 7"),
        "{err}"
    );
}

#[test]
fn an_ambiguous_method_reference_is_a_compile_error() {
    // A method reference's arity comes from the referenced member, because
    // javars does not target-type. An overloaded name therefore has no single
    // arity and is rejected, rather than one overload being guessed.
    let (_, ok) = run("import java.util.function.*;\
         public class Main {\
         static int f(int a) { return a; }\
         static int f(int a, int b) { return a + b; }\
         public static void main(String[] x) {\
         Function<Integer, Integer> g = Main::f;\
         System.out.println(g.apply(1)); } }");
    assert!(!ok);
}

#[test]
fn hash_map_and_hash_set_iterate_in_javas_bucket_order() {
    // The point of the whole collections wave. Java lays entries out in a
    // power-of-two table indexed by `(capacity - 1) & (h ^ (h >>> 16))`,
    // appending within a bucket and preserving relative order across a resize —
    // so iteration is a *stable* sort of the insertion sequence by bucket index.
    // These three cases pin that: `String` keys, `Integer` keys, and 20 entries
    // (which crosses the resize at 13). Verified against OpenJDK 26 — a
    // LinkedHashMap of the same keys prints in a visibly different order.
    let (out, ok) = run(
        "import java.util.*;\
         public class Main { public static void main(String[] a) {\
         String[] ks = {\"banana\", \"apple\", \"cherry\", \"date\", \"elderberry\", \"fig\", \"grape\"};\
         Map<String, Integer> m = new HashMap<>();\
         for (int i = 0; i < ks.length; i++) { m.put(ks[i], i); }\
         System.out.println(m);\
         System.out.println(m.keySet());\
         System.out.println(m.values());\
         Map<String, Integer> lm = new LinkedHashMap<>();\
         for (int i = 0; i < ks.length; i++) { lm.put(ks[i], i); }\
         System.out.println(lm);\
         Set<String> hs = new HashSet<>(Arrays.asList(ks));\
         System.out.println(hs);\
         Map<Integer, String> mi = new HashMap<>();\
         int[] is = {5, 3, 17, 1, 33, 2, 16};\
         for (int i : is) { mi.put(i, \"v\" + i); }\
         System.out.println(mi);\
         Map<String, Integer> big = new HashMap<>();\
         for (int i = 0; i < 20; i++) { big.put(\"k\" + i, i); }\
         System.out.println(big); } }",
    );
    assert!(ok);
    assert_eq!(
        out,
        "{banana=0, date=3, apple=1, cherry=2, fig=5, grape=6, elderberry=4}\n\
         [banana, date, apple, cherry, fig, grape, elderberry]\n\
         [0, 3, 1, 2, 5, 6, 4]\n\
         {banana=0, apple=1, cherry=2, date=3, elderberry=4, fig=5, grape=6}\n\
         [banana, date, apple, cherry, fig, grape, elderberry]\n\
         {16=v16, 17=v17, 1=v1, 33=v33, 2=v2, 3=v3, 5=v5}\n\
         {k0=0, k1=1, k2=2, k3=3, k4=4, k5=5, k11=11, k6=6, k10=10, k7=7, \
         k13=13, k8=8, k12=12, k9=9, k15=15, k14=14, k17=17, k16=16, k19=19, k18=18}\n"
    );
}

#[test]
fn list_map_and_set_methods_match_java() {
    // The mutating and querying surface, the sorted implementations, the
    // enhanced `for` over a collection, and reference semantics (a `List` handed
    // to a method is the same object). Verified against OpenJDK 26.
    let (out, ok) = run(
        "import java.util.*;\
         public class Main {\
         static void fill(List<Integer> out) { out.add(7); out.add(8); }\
         public static void main(String[] a) {\
         List<String> xs = new ArrayList<>();\
         xs.add(\"b\"); xs.add(\"a\"); xs.add(\"c\");\
         System.out.println(xs + \" \" + xs.size() + \" \" + xs.get(1) + \" \" + xs.contains(\"a\") + \" \" + xs.indexOf(\"c\"));\
         xs.set(0, \"z\"); xs.remove(1);\
         System.out.println(xs + \" \" + xs.isEmpty());\
         for (String s : xs) { System.out.print(s + \";\"); }\
         System.out.println();\
         Collections.sort(xs); System.out.println(xs);\
         Map<String, Integer> tm = new TreeMap<>();\
         tm.put(\"z\", 1); tm.put(\"a\", 2); tm.put(\"m\", 3);\
         System.out.println(tm + \" \" + tm.get(\"m\") + \" \" + tm.getOrDefault(\"q\", -1) + \" \" + tm.containsKey(\"a\"));\
         System.out.println(new TreeSet<>(Arrays.asList(5, 1, 9, 3)));\
         Set<String> ls = new LinkedHashSet<>(Arrays.asList(\"b\", \"a\", \"b\"));\
         System.out.println(ls + \" \" + ls.add(\"a\") + \" \" + ls.add(\"c\") + \" \" + ls);\
         List<Integer> shared = new ArrayList<>(); fill(shared);\
         System.out.println(shared);\
         Map<String, List<Integer>> nested = new LinkedHashMap<>();\
         nested.put(\"odd\", new ArrayList<>()); nested.get(\"odd\").add(1); nested.get(\"odd\").add(3);\
         System.out.println(nested + \" \" + nested.get(\"odd\").size());\
         System.out.println(new ArrayList<Integer>() + \" \" + new HashMap<String,Integer>() + \" \" + new HashSet<String>()); } }",
    );
    assert!(ok);
    assert_eq!(
        out,
        "[b, a, c] 3 a true 2\n\
         [z, c] false\n\
         z;c;\n\
         [c, z]\n\
         {a=2, m=3, z=1} 3 -1 true\n\
         [1, 3, 5, 9]\n\
         [b, a] false true [b, a, c]\n\
         [7, 8]\n\
         {odd=[1, 3]} 2\n\
         [] {} []\n"
    );
}

#[test]
fn collections_take_lambdas_and_raise_javas_faults() {
    // A comparator lambda drives a stable sort, `forEach` runs one per element
    // (and per entry, for a `Map`), and the two faults Java raises here — an
    // out-of-range `get` and a structural write to `List.of` — arrive as
    // catchable Java exceptions with Java's detail message. Verified against
    // OpenJDK 26.
    let (out, ok) = run(
        "import java.util.*;\
         public class Main { public static void main(String[] a) {\
         List<String> xs = new ArrayList<>(Arrays.asList(\"pear\", \"fig\", \"banana\", \"kiwi\"));\
         xs.sort((p, q) -> p.length() - q.length());\
         System.out.println(xs);\
         Collections.sort(xs, (p, q) -> q.compareTo(p));\
         System.out.println(xs);\
         Collections.reverse(xs);\
         System.out.println(xs + \" \" + Collections.max(Arrays.asList(3, 9, 2)) + \" \" + Collections.min(Arrays.asList(3, 9, 2)));\
         xs.forEach(s -> System.out.print(s + \"|\"));\
         System.out.println();\
         Map<String, Integer> m = new LinkedHashMap<>();\
         m.put(\"a\", 1); m.put(\"b\", 2);\
         m.forEach((k, v) -> System.out.print(k + \"=\" + v + \";\"));\
         System.out.println();\
         try { xs.get(99); } catch (IndexOutOfBoundsException e) { System.out.println(\"oob \" + e.getMessage()); }\
         try { List.of(\"a\").add(\"b\"); } catch (UnsupportedOperationException e) { System.out.println(\"uoe\"); }\
         try { Arrays.asList(1, 2).add(3); } catch (UnsupportedOperationException e) { System.out.println(\"uoe2\"); } } }",
    );
    assert!(ok);
    assert_eq!(
        out,
        "[fig, pear, kiwi, banana]\n\
         [pear, kiwi, fig, banana]\n\
         [banana, fig, kiwi, pear] 9 2\n\
         banana|fig|kiwi|pear|\n\
         a=1;b=2;\n\
         oob Index 99 out of bounds for length 4\n\
         uoe\n\
         uoe2\n"
    );
}

#[test]
fn string_compare_to_returns_javas_difference_not_just_its_sign() {
    // `String.compareTo` is specified as the difference of the first differing
    // character (else the length difference), and programs print it — so
    // returning only -1/0/1 would be wrong. Verified against OpenJDK 26.
    let (out, ok) = run("public class Main { public static void main(String[] a) {\
         System.out.println(\"abc\".compareTo(\"abd\") + \" \" + \"b\".compareTo(\"a\") + \" \"\
         + \"ab\".compareTo(\"ab\") + \" \" + \"abc\".compareTo(\"ab\")); } }");
    assert!(ok);
    assert_eq!(out, "-1 1 0 1\n");
}

#[test]
fn arrow_switch_expressions_yield_a_value() {
    // The arrow form has no fall-through and exactly one arm runs, so it is
    // lowered as a `?:` chain rather than as laid-out group bodies. Covers
    // multi-label arms, a block arm's `yield`, an `enum` discriminant with
    // unqualified labels, a `String` discriminant, a `default` written *before*
    // a matching arm (which must not shadow it), and the result feeding `/`
    // typing — which only truncates if the arm's static type survives the
    // switch. Verified against OpenJDK 26.
    let (out, ok) = run(
        "public class Main {\
         enum Day { MON, TUE, SAT, SUN }\
         static String kind(Day d) { return switch (d) { case SAT, SUN -> \"weekend\"; case MON -> \"start\"; default -> \"mid\"; }; }\
         static int score(int n) { return switch (n) { case 1 -> 10; case 2, 3 -> 20; default -> { int t = n * 2; yield t + 1; } }; }\
         public static void main(String[] a) {\
         for (Day d : Day.values()) { System.out.print(kind(d) + \",\"); }\
         System.out.println();\
         for (int i = 0; i < 5; i++) { System.out.print(score(i) + \",\"); }\
         System.out.println();\
         String s = \"b\";\
         System.out.println(switch (s) { case \"a\" -> 1; case \"b\" -> 2; default -> 3; });\
         int k = 2;\
         System.out.println(switch (k) { case 2 -> 7; default -> 9; } / 2);\
         System.out.println(1 + switch (k) { case 2 -> 20; default -> 0; } + 300);\
         System.out.println(switch (k) { default -> \"d\"; case 2 -> \"two\"; });\
         System.out.println(switch (k) { case 2 -> { if (k > 1) { yield \"big\"; } yield \"small\"; } default -> \"n\"; }); } }",
    );
    assert!(ok);
    assert_eq!(
        out,
        "start,mid,weekend,weekend,\n1,10,20,20,9,\n2\n3\n321\ntwo\nbig\n"
    );
}

#[test]
fn arrow_switch_statements_and_arms_that_leave_the_switch() {
    // An arrow `switch` used as a statement is the expression form with its
    // value discarded — no `break` and no fall-through. Also pins the two arms
    // that do not produce a value normally: one that throws, and one whose
    // `yield` has to run a `finally` it is leaving. And `yield` stays an
    // ordinary identifier outside an arm, which is Java's contextual rule.
    // Verified against OpenJDK 26.
    let (out, ok) = run(
        "public class Main { public static void main(String[] a) {\
         int x = 2;\
         switch (x) { case 1 -> System.out.println(\"one\"); case 2 -> System.out.println(\"two\"); default -> System.out.println(\"other\"); }\
         switch (x) { case 1: case 2: System.out.println(\"1or2\"); default: System.out.println(\"fell\"); }\
         System.out.println(switch (x) { case 2 -> { try { yield \"y\"; } finally { System.out.println(\"fin\"); } } default -> \"n\"; });\
         try { int bad = switch (x) { case 2 -> throw new IllegalStateException(\"nope\"); default -> 0; }; System.out.println(bad); }\
         catch (IllegalStateException e) { System.out.println(\"caught \" + e.getMessage()); }\
         int yield = 5;\
         System.out.println(yield + 1); } }",
    );
    assert!(ok);
    assert_eq!(out, "two\n1or2\nfell\nfin\ny\ncaught nope\n6\n");
}

#[test]
fn var_carries_its_initializers_type_not_just_its_value() {
    // `var` has to record the *type* it infers, or the binding stops truncating
    // its division and stops wrapping at 32 bits. Each line reads the binding
    // back through an operation only a correct static type gets right —
    // including a `var` enhanced-`for` variable over `new int[]{…}`, whose
    // element type the array literal has to have carried along. Verified against
    // OpenJDK 26.
    let (out, ok) = run("public class Main {\
         record Pt(int x, int y) { int sum() { return x + y; } }\
         public static void main(String[] a) {\
         var i = 7; var d = 7.0;\
         System.out.println(i / 2 + \" \" + d / 2);\
         var arr = new int[]{1, 5, 9};\
         for (var v : arr) { System.out.print(v / 2); }\
         System.out.println();\
         var g = new int[][]{{1, 2}, {3, 4}};\
         for (var row : g) { for (var c : row) { System.out.print(c / 2); } }\
         System.out.println();\
         var s = new String[]{\"aa\", \"b\"};\
         for (var e : s) { System.out.print(e.length()); }\
         System.out.println();\
         var p = new Pt(9, 4);\
         System.out.println(p + \" \" + p.x() / 2 + \" \" + p.sum());\
         var t = 0;\
         for (var k = 0; k < 5; k++) { t += k; }\
         System.out.println(t / 2);\
         var big = 100000;\
         System.out.println(big * big); } }");
    assert!(ok);
    assert_eq!(
        out,
        "3 3.5\n024\n0112\n21\nPt[x=9, y=4] 4 13\n5\n1410065408\n"
    );
}

#[test]
fn long_literals_are_not_wrapped_at_32_bits() {
    // An `L`-suffixed literal is a `long` in Java, so it is exempt from the
    // 32-bit `int` wrap — and dropping the suffix at lex time (as javars used
    // to) made `3000000000L + 3000000000L` print `1705032704`. Only visible
    // past `Integer.MAX_VALUE`, which is why it needs its own case. Verified
    // against OpenJDK 26.
    let (out, ok) = run("public class Main { public static void main(String[] a) {\
         var big = 3000000000L;\
         System.out.println(big + big);\
         System.out.println(3000000000L + 3000000000L);\
         long x = 3000000000L;\
         System.out.println(x * 3);\
         System.out.println(2147483647L + 1L); } }");
    assert!(ok);
    assert_eq!(out, "6000000000\n6000000000\n9000000000\n2147483648\n");
}

#[test]
fn bitwise_operators_follow_java_widths() {
    // `&`/`|`/`^`/`~` on integers are bitwise; on booleans they are the
    // non-short-circuiting logical operators, whose result must print
    // `true`/`false` rather than the 0/1 an integer op would leave. Verified
    // against OpenJDK 26.
    let (out, ok) = run(&wrap(
        "System.out.println(5 & 3);\
         System.out.println(5 | 3);\
         System.out.println(5 ^ 3);\
         System.out.println(~5);\
         System.out.println(-1 & 0xFF);\
         System.out.println(true & false);\
         System.out.println(true | false);\
         System.out.println(true ^ true);",
    ));
    assert!(ok);
    assert_eq!(out, "1\n7\n6\n-6\n255\nfalse\ntrue\nfalse\n");
}

#[test]
fn shift_distance_is_masked_to_the_left_operands_width() {
    // Java masks the shift distance to 5 bits for `int` and 6 for `long`, so
    // `1 << 33` is `1 << 1`; `>>>` zero-fills at the operand's width, which is
    // why `-8 >>> 1` and `-8L >>> 1` differ. Only the left operand is promoted,
    // so `1 << 2L` stays an `int`. Verified against OpenJDK 26.
    let (out, ok) = run(&wrap(
        "System.out.println(1 << 31);\
         System.out.println(1 << 33);\
         System.out.println(1L << 40);\
         System.out.println(-8 >> 1);\
         System.out.println(-8 >>> 1);\
         System.out.println(-8L >>> 1);\
         int v = 1; v <<= 3; v |= 1; v &= 14; v ^= 3;\
         System.out.println(v);\
         long w = -8L; w >>>= 1;\
         System.out.println(w);",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "-2147483648\n2\n1099511627776\n-4\n2147483644\n9223372036854775804\n11\n9223372036854775804\n"
    );
}

#[test]
fn nested_generic_type_arguments_still_parse_after_shift_lexing() {
    // `>>` is one token now, so `List<List<String>>` closes its two type
    // argument lists with a single `Tok::Shr`. Every generic-skipping depth
    // counter has to weigh a closer by how many `>` it spells, or this fails to
    // parse. Verified against OpenJDK 26.
    let (out, ok) = run("import java.util.List; import java.util.ArrayList;\
         public class T { public static void main(String[] a) {\
         List<List<String>> m = new ArrayList<>();\
         m.add(new ArrayList<>()); m.get(0).add(\"q\");\
         System.out.println(m); } }");
    assert!(ok);
    assert_eq!(out, "[[q]]\n");
}

#[test]
fn radix_and_underscore_literals() {
    // Hex and binary literals are read as a *bit pattern* at the literal's
    // width, so `0xFFFFFFFF` is the `int` -1 while the `L`-suffixed form is the
    // `long` -1; a leading `0` is octal; `_` separates digits. Verified against
    // OpenJDK 26.
    let (out, ok) = run(&wrap(
        "System.out.println(0x1F);\
         System.out.println(0xFFFFFFFF);\
         System.out.println(0xFFFFFFFFFFFFFFFFL);\
         System.out.println(0b1111_0000);\
         System.out.println(017);\
         System.out.println(1_000_000);",
    ));
    assert!(ok);
    assert_eq!(out, "31\n-1\n-1\n240\n15\n1000000\n");
}

#[test]
fn narrowing_casts_saturate_and_wrap_like_java() {
    // `double` → integral saturates (`(int) 1e18` is `Integer.MAX_VALUE`) and
    // truncates toward zero; the integral narrowings are two's-complement; and
    // `(double) 7 / 2` is a floating division because the cast retypes the left
    // operand. Verified against OpenJDK 26.
    let (out, ok) = run(&wrap(
        "System.out.println((int) 3.99);\
         System.out.println((int) -3.99);\
         System.out.println((int) 1e18);\
         System.out.println((long) 1e18);\
         System.out.println((byte) 200);\
         System.out.println((short) 70000);\
         System.out.println((char) 65);\
         System.out.println((double) 7 / 2);",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "3\n-3\n2147483647\n1000000000000000000\n-56\n4464\nA\n3.5\n"
    );
}

#[test]
fn pre_and_post_increment_differ_in_value_position() {
    // The post-form evaluates to the value the variable *held*, the pre-form to
    // the value it *takes* — the whole reason both spellings exist. Verified
    // against OpenJDK 26.
    let (out, ok) = run(&wrap(
        "int a = 5; System.out.println(a++ + \",\" + a);\
         int b = 5; System.out.println(++b + \",\" + b);\
         int c = 5; System.out.println(c-- + \",\" + c);\
         int d = 5; System.out.println(--d + \",\" + d);",
    ));
    assert!(ok);
    assert_eq!(out, "5,6\n6,6\n5,4\n4,4\n");
}

#[test]
fn for_header_takes_comma_separated_clauses() {
    // Both the init and the update clause are lists; the init's later
    // declarators reuse the first one's type. Verified against OpenJDK 26.
    let (out, ok) = run(&wrap(
        "int t = 0; for (int i = 0, j = 10; i < j; i++, j--) { t += i + j; }\
         System.out.println(t);",
    ));
    assert!(ok);
    assert_eq!(out, "50\n");
}

#[test]
fn multi_catch_matches_any_listed_type() {
    // `catch (A | B e)` runs one body for either type, and the alternatives are
    // tested in order against the throwable's class. Verified against
    // OpenJDK 26.
    let (out, ok) = run(&wrap(
        "try { throw new IllegalArgumentException(\"m\"); }\
         catch (IllegalStateException | IllegalArgumentException e) {\
             System.out.println(\"first \" + e.getMessage()); }\
         try { throw new IllegalStateException(\"n\"); }\
         catch (IllegalStateException | IllegalArgumentException e) {\
             System.out.println(\"second \" + e.getMessage()); }",
    ));
    assert!(ok);
    assert_eq!(out, "first m\nsecond n\n");
}

#[test]
fn labeled_jumps_out_of_a_try_run_the_finally() {
    // A labeled `break`/`continue` that leaves a `try` has to run the cleanup
    // on the way out *and* land on the outer loop — the shape that broke the
    // sibling frontends. `return` from inside a `try` in a loop must exit the
    // method rather than spin. Verified against OpenJDK 26.
    let (out, ok) = run("public class T {\
         static int f() { for (int i = 0; i < 10; i++) {\
             try { if (i == 3) return i; } finally { System.out.print(\"f\" + i); } }\
             return -1; }\
         public static void main(String[] a) {\
         outer: for (int i = 0; i < 2; i++) { for (int j = 0; j < 3; j++) {\
             try { if (j == 1) continue outer; System.out.print(\"b\" + i + j); }\
             finally { System.out.print(\"c\" + i + j); } } }\
         System.out.println();\
         int k = 0; ex: while (true) { k++;\
             try { if (k > 2) break ex; } finally { System.out.print(\"w\" + k); } }\
         System.out.println(k);\
         System.out.println(f()); } }");
    assert!(ok);
    assert_eq!(out, "b00c00c01b10c10c11\nw1w2w33\nf0f1f2f33\n");
}

#[test]
fn format_flags_indexes_and_scientific_conversions() {
    // The grouping and parenthesis flags, explicit argument indexes (which do
    // not advance the implicit cursor), `%e`'s two-digit signed exponent, and
    // `%f`'s HALF_UP rounding — where Rust's own formatter rounds half-to-even
    // and would print `2` for `%.0f` of 2.5. Verified against OpenJDK 26.
    let (out, ok) = run(&wrap(
        "System.out.println(String.format(\"%,d\", 1234567));\
         System.out.println(String.format(\"%(d\", -5));\
         System.out.println(String.format(\"%2$s %1$s\", \"a\", \"b\"));\
         System.out.println(String.format(\"%e|%E\", 1234.5678, -1.5));\
         System.out.println(String.format(\"%.0f,%.0f,%.3f\", 2.5, 3.5, -1.5));\
         System.out.println(String.format(\"%08.2f\", 3.5));\
         System.out.printf(\"pf %d %s%n\", 7, \"x\");",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "1,234,567\n(5)\nb a\n1.234568e+03|-1.500000E+00\n3,4,-1.500\n00003.50\npf 7 x\n"
    );
}

#[test]
fn wrapper_class_constants_and_long_wraparound() {
    // The `java.lang` `static final`s are folded to their literal value, and
    // `long` arithmetic that overflows `i64` wraps two's-complement instead of
    // escaping to a wider representation. `Math.abs(int)` overflows the same
    // way. Verified against OpenJDK 26.
    let (out, ok) = run(&wrap(
        "System.out.println(Integer.MAX_VALUE + \",\" + Integer.MIN_VALUE);\
         System.out.println(Long.MAX_VALUE + 1);\
         System.out.println(Long.MIN_VALUE - 1);\
         System.out.println(Double.MAX_VALUE);\
         System.out.println(Math.abs(Integer.MIN_VALUE));\
         System.out.println(Math.floorDiv(-7, 2) + \",\" + Math.floorMod(-7, 2));",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "2147483647,-2147483648\n-9223372036854775808\n9223372036854775807\n1.7976931348623157E308\n-2147483648\n-4,1\n"
    );
}

#[test]
fn array_statics_sort_copy_and_render_nested() {
    // `Arrays.sort`/`fill` mutate the caller's array in place; `copyOf` pads
    // with the element type's default; `deepToString` recurses where
    // `toString` does not. Verified against OpenJDK 26.
    let (out, ok) = run("import java.util.Arrays;\
         public class T { public static void main(String[] a) {\
         int[] x = {5, 3, 1, 4}; Arrays.sort(x);\
         System.out.println(Arrays.toString(x));\
         System.out.println(Arrays.toString(Arrays.copyOf(x, 6)));\
         System.out.println(Arrays.toString(Arrays.copyOfRange(x, 1, 3)));\
         int[] f = new int[3]; Arrays.fill(f, 7);\
         System.out.println(Arrays.toString(f));\
         System.out.println(Arrays.binarySearch(x, 4));\
         int[][] m = new int[2][3]; m[1][2] = 9;\
         System.out.println(Arrays.deepToString(m)); } }");
    assert!(ok);
    assert_eq!(
        out,
        "[1, 3, 4, 5]\n[1, 3, 4, 5, 0, 0]\n[3, 4]\n[7, 7, 7]\n2\n[[0, 0, 0], [0, 0, 9]]\n"
    );
}

#[test]
fn string_search_split_and_regexless_replace() {
    // The offset searches, the char-array round trip, and the literal-pattern
    // subset of `split` (which drops trailing empty fields but keeps interior
    // ones). Verified against OpenJDK 26.
    let (out, ok) = run(&wrap(
        "System.out.println(\"Hello\".indexOf(\"l\", 3) + \",\" + \"Hello\".lastIndexOf(\"l\"));\
         System.out.println(\"  pad  \".strip() + \"|\" + \"  \".isBlank());\
         System.out.println(\"abc\".hashCode());\
         System.out.println(new String(\"ab\".toCharArray()));\
         System.out.println(\"a,b,,c\".split(\",\").length + \",\" + \"a,b,,c\".split(\",\")[2] + \"|\");\
         System.out.println(\"a,b,,\".split(\",\").length);\
         System.out.println(String.join(\"-\", \"a\", \"b\", \"c\"));",
    ));
    assert!(ok);
    assert_eq!(out, "3,3\npad|true\n96354\nab\n4,|\n2\na-b-c\n");
}

#[test]
fn get_class_reports_the_runtime_class_name() {
    // `getClass()` has no `Class` object behind it here: it evaluates to the
    // runtime class *name*, over which `getName`/`getSimpleName` are String
    // methods. Verified against OpenJDK 26.
    let (out, ok) = run(&wrap(
        "Exception e = new IllegalStateException(\"x\");\
         System.out.println(e.getClass().getName());\
         System.out.println(e.getClass().getSimpleName());",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "java.lang.IllegalStateException\nIllegalStateException\n"
    );
}

#[test]
fn colon_form_switch_expression_yields() {
    // A switch *expression* accepts the classic `case X:` arm as well as the
    // arrow form; an empty colon arm groups its labels onto the next one.
    // Verified against OpenJDK 26.
    let (out, ok) = run(&wrap(
        "int v = switch (2) { case 2: yield 22; default: yield 0; };\
         System.out.println(v);\
         int w = switch (3) { case 1: case 3: yield 13; default: yield 0; };\
         System.out.println(w);",
    ));
    assert!(ok);
    assert_eq!(out, "22\n13\n");
}
