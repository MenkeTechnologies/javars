//! The implicit `java.lang` / `java.util.function` prelude.
//!
//! javars has no JDK to link against, so the exception classes a `throw`/`catch`
//! program names (`RuntimeException`, `IllegalStateException`, …) do not exist
//! until javars supplies them. Rather than special-case them in the compiler,
//! this module *writes them in Java* and parses them with the ordinary parser:
//! they become ordinary [`crate::ast::Class`] nodes with real fields, real
//! constructors, and real `toString()` bodies, so every existing mechanism —
//! `extends` chains, `super(m)` chaining, virtual dispatch, `instanceof` through
//! [`crate::host`]'s supertype table — applies to them unchanged. `catch
//! (Exception e)` catching a `NumberFormatException` is then just the
//! `instanceof` walk javars already had.
//!
//! The `java.util.function` interfaces (`Runnable`, `Function`, `Predicate`, …)
//! are supplied the same way, and for the same reason: a lambda target is just
//! an interface with one abstract method, so declaring them in Java means the
//! compiler needs no table of built-in functional types — a user's own
//! `interface Calc { int of(int a); }` reaches the identical code path.
//!
//! Each half of the prelude is injected only into programs that actually use it
//! (see [`inject`]), so a program that never throws and never writes a lambda
//! carries none of these types and emits exactly the bytecode it did before.

use crate::ast::Program;

/// The modeled `java.lang` throwable hierarchy: `(class, superclass)`, ordered
/// so a superclass is always declared before its subclasses. `Throwable` has no
/// superclass and carries the `detailMessage` field the whole tree inherits.
///
/// This is the subset a hand-written program throws and catches; the JDK's full
/// hierarchy is far larger, and an unlisted name is an unknown class (a compile
/// error) rather than a silently-accepted one.
pub const THROWABLES: &[(&str, &str)] = &[
    ("Throwable", ""),
    ("Error", "Throwable"),
    ("Exception", "Throwable"),
    ("RuntimeException", "Exception"),
    ("IllegalArgumentException", "RuntimeException"),
    ("NumberFormatException", "IllegalArgumentException"),
    ("IllegalStateException", "RuntimeException"),
    ("ArithmeticException", "RuntimeException"),
    ("NullPointerException", "RuntimeException"),
    ("ClassCastException", "RuntimeException"),
    ("UnsupportedOperationException", "RuntimeException"),
    ("NegativeArraySizeException", "RuntimeException"),
    ("IndexOutOfBoundsException", "RuntimeException"),
    (
        "ArrayIndexOutOfBoundsException",
        "IndexOutOfBoundsException",
    ),
    (
        "StringIndexOutOfBoundsException",
        "IndexOutOfBoundsException",
    ),
    // The modeled throwables that do not live in `java.lang` — see
    // [`qualified_throwable`].
    ("PatternSyntaxException", "IllegalArgumentException"),
    ("ConcurrentModificationException", "RuntimeException"),
    ("NoSuchElementException", "RuntimeException"),
    ("IllegalFormatException", "IllegalArgumentException"),
    ("IllegalFormatConversionException", "IllegalFormatException"),
    // The rest of `java.util`'s format family. Each is a *catchable*
    // `IllegalFormatException` in Java, and javars reported all four as internal
    // `javars:` errors that abort the run — two of them only after a `panic!` or
    // a hang first. Superclasses read off `getSuperclass()` on openjdk 21.0.12,
    // not assumed from the names: all four sit directly under
    // `IllegalFormatException`, siblings of `IllegalFormatConversionException`.
    ("IllegalFormatWidthException", "IllegalFormatException"),
    ("IllegalFormatPrecisionException", "IllegalFormatException"),
    ("MissingFormatArgumentException", "IllegalFormatException"),
    ("UnknownFormatConversionException", "IllegalFormatException"),
    // The two the flag validator raises. A flag the conversion does not accept
    // (`%,x`) and a pair that contradict each other (`%+ d`) were both accepted
    // and ignored before it existed, so the format string said something the
    // program could not have meant and nothing reported it.
    (
        "FormatFlagsConversionMismatchException",
        "IllegalFormatException",
    ),
    ("IllegalFormatFlagsException", "IllegalFormatException"),
    ("MissingFormatWidthException", "IllegalFormatException"),
];

/// True when `name` is one of the modeled JDK throwables.
pub fn is_throwable(name: &str) -> bool {
    THROWABLES.iter().any(|(n, _)| *n == name)
}

/// The fully-qualified name of a modeled throwable, which is what
/// `getClass().getName()`, `Throwable.toString()`, and the uncaught-exception
/// report print. Most live in `java.lang`; `PatternSyntaxException` is
/// `java.util.regex` and `ConcurrentModificationException` is `java.util`, and
/// printing either under the wrong package would be a visible difference from
/// Java for a program that ever prints a caught one.
pub fn qualified_throwable(name: &str) -> Option<String> {
    if !is_throwable(name) {
        return None;
    }
    Some(match name {
        "PatternSyntaxException" => format!("java.util.regex.{name}"),
        "ConcurrentModificationException"
        | "NoSuchElementException"
        | "IllegalFormatException"
        | "IllegalFormatConversionException"
        | "IllegalFormatWidthException"
        | "IllegalFormatPrecisionException"
        | "MissingFormatArgumentException"
        | "UnknownFormatConversionException"
        | "FormatFlagsConversionMismatchException"
        | "IllegalFormatFlagsException"
        | "MissingFormatWidthException" => format!("java.util.{name}"),
        _ => format!("java.lang.{name}"),
    })
}

/// The fully-qualified name of a modeled JDK class, or `None` for a user class
/// (whose simple name *is* its qualified one — a single-file program declares no
/// package). This is what `getClass().getName()` prints and what Java's
/// `Object.toString()` puts before the `@`.
///
/// `Object` is the root of the hierarchy rather than a modeled type: javars
/// keeps it out of the class table on purpose (it is also the erasure of every
/// type variable), so it is named here directly.
pub fn qualified_class(name: &str) -> Option<String> {
    if name == "Object" {
        return Some("java.lang.Object".to_string());
    }
    qualified_throwable(name)
}

/// The modeled functional interfaces: `(name, declared single abstract method,
/// the interface's other members)`.
///
/// Each is declared to javars as an ordinary `interface`, whose one *abstract*
/// method is all a lambda target needs — the compiler recognises *any*
/// single-abstract-method interface (user-declared ones included) as a lambda
/// target, so these entries buy no special case, only the declarations the JDK
/// would have supplied. Type parameters are erased, so the parameter and return
/// types below are the erasure Java itself uses (`Object`), except where the
/// JDK's own signature is primitive (`Predicate.test` → `boolean`,
/// `Comparator.compare` → `int`), which javars keeps so a call participates in
/// numeric typing.
///
/// The third column is the JDK's `default` and `static` members, transliterated
/// from the bodies `java.util.function` declares. They are ordinary Java, so
/// they compile through the same paths a user's own `default` method does, and a
/// *lambda* receiver reaches them because a closure gets its own arm in the
/// dispatch chain. The JDK's leading `Objects.requireNonNull(after)` is the one
/// thing dropped: it would pull the whole throwable prelude into every program
/// that writes a lambda, and composing with `null` still raises the same
/// `NullPointerException` — at the composed function's first call rather than at
/// composition (named in `BUGS.md`).
pub const FUNCTIONAL: &[(&str, &str, &str)] = &[
    ("Runnable", "void run()", ""),
    ("Callable", "Object call()", ""),
    ("Supplier", "Object get()", ""),
    (
        "Consumer",
        "void accept(Object t)",
        "default Consumer andThen(Consumer after) { return t -> { this.accept(t); after.accept(t); }; }",
    ),
    (
        "BiConsumer",
        "void accept(Object t, Object u)",
        "default BiConsumer andThen(BiConsumer after) { return (t, u) -> { this.accept(t, u); after.accept(t, u); }; }",
    ),
    (
        "Function",
        "Object apply(Object t)",
        "default Function andThen(Function after) { return t -> after.apply(this.apply(t)); } \
         default Function compose(Function before) { return v -> this.apply(before.apply(v)); } \
         static Function identity() { return t -> t; }",
    ),
    (
        "BiFunction",
        "Object apply(Object t, Object u)",
        "default BiFunction andThen(Function after) { return (t, u) -> after.apply(this.apply(t, u)); }",
    ),
    (
        "UnaryOperator",
        "Object apply(Object t)",
        "static UnaryOperator identity() { return t -> t; }",
    ),
    (
        "BinaryOperator",
        "Object apply(Object t, Object u)",
        "static BinaryOperator minBy(Comparator comparator) { return (a, b) -> comparator.compare(a, b) <= 0 ? a : b; } \
         static BinaryOperator maxBy(Comparator comparator) { return (a, b) -> comparator.compare(a, b) >= 0 ? a : b; }",
    ),
    (
        "Predicate",
        "boolean test(Object t)",
        "default Predicate and(Predicate other) { return t -> this.test(t) && other.test(t); } \
         default Predicate negate() { return t -> !this.test(t); } \
         default Predicate or(Predicate other) { return t -> this.test(t) || other.test(t); } \
         static Predicate not(Predicate target) { return target.negate(); }",
    ),
    (
        "BiPredicate",
        "boolean test(Object t, Object u)",
        "default BiPredicate and(BiPredicate other) { return (t, u) -> this.test(t, u) && other.test(t, u); } \
         default BiPredicate negate() { return (t, u) -> !this.test(t, u); } \
         default BiPredicate or(BiPredicate other) { return (t, u) -> this.test(t, u) || other.test(t, u); }",
    ),
    (
        "Comparator",
        "int compare(Object a, Object b)",
        "default Comparator reversed() { return (a, b) -> this.compare(b, a); } \
         default Comparator thenComparing(Comparator other) { return (a, b) -> { int r = this.compare(a, b); if (r != 0) { return r; } return other.compare(a, b); }; } \
         static Comparator naturalOrder() { return (a, b) -> a.compareTo(b); } \
         static Comparator reverseOrder() { return (a, b) -> b.compareTo(a); } \
         static Comparator comparing(Function key) { return (a, b) -> key.apply(a).compareTo(key.apply(b)); }",
    ),
    ("IntSupplier", "int getAsInt()", ""),
    (
        "IntPredicate",
        "boolean test(int value)",
        "default IntPredicate and(IntPredicate other) { return value -> this.test(value) && other.test(value); } \
         default IntPredicate negate() { return value -> !this.test(value); } \
         default IntPredicate or(IntPredicate other) { return value -> this.test(value) || other.test(value); }",
    ),
    (
        "IntConsumer",
        "void accept(int value)",
        "default IntConsumer andThen(IntConsumer after) { return value -> { this.accept(value); after.accept(value); }; }",
    ),
    (
        "IntUnaryOperator",
        "int applyAsInt(int operand)",
        "default IntUnaryOperator compose(IntUnaryOperator before) { return v -> this.applyAsInt(before.applyAsInt(v)); } \
         default IntUnaryOperator andThen(IntUnaryOperator after) { return v -> after.applyAsInt(this.applyAsInt(v)); } \
         static IntUnaryOperator identity() { return v -> v; }",
    ),
    ("IntBinaryOperator", "int applyAsInt(int left, int right)", ""),
    ("ToIntFunction", "int applyAsInt(Object value)", ""),
    ("ToDoubleFunction", "double applyAsDouble(Object value)", ""),
];

/// True when `name` is one of the modeled functional interfaces.
pub fn is_functional(name: &str) -> bool {
    FUNCTIONAL.iter().any(|(n, _, _)| *n == name)
}

/// The Java source of the prelude classes the program does not already declare.
///
/// `Throwable` holds the message, the accessors, and the single `toString()` the
/// whole tree inherits; every subclass adds only its two constructors.
///
/// That one `toString()` asks `this.getClass().getName()` for the name, which is
/// what Java's own `Throwable.toString()` does. The earlier shape gave *each*
/// prelude class its own override with the qualified name written into the
/// string literal, so the name came from the class the compiler resolved the
/// override on rather than from the object. That is indistinguishable for a
/// prelude class thrown directly, and wrong for every user-defined one:
/// `class MyEx extends RuntimeException` inherited `RuntimeException`'s
/// override and rendered `java.lang.RuntimeException: b` where Java prints
/// `T$MyEx: b`. Subclassing an exception is the ordinary way to define one, so
/// that was the common case, not the corner.
fn prelude_source(declared: &[String]) -> String {
    let mut src =
        String::from("public class __JavaLang { public static void main(String[] a) { } }\n");
    for (name, sup) in THROWABLES {
        if declared.iter().any(|d| d == name) {
            continue;
        }
        let extends = if sup.is_empty() {
            String::new()
        } else {
            format!(" extends {sup}")
        };
        src.push_str(&format!("class {name}{extends} {{\n"));
        if sup.is_empty() {
            // The root carries the state and the accessors. An absent message is
            // the field's default (`null`), which is exactly what Java's
            // `getMessage()` returns for the no-arg constructor.
            src.push_str("  String detailMessage;\n");
            src.push_str(&format!("  {name}() {{ }}\n"));
            src.push_str(&format!("  {name}(String m) {{ detailMessage = m; }}\n"));
            src.push_str("  String getMessage() { return detailMessage; }\n");
            // `getLocalizedMessage()` is `Throwable`'s own one-liner
            // (`return getMessage();`), overridable but never overridden here.
            src.push_str("  String getLocalizedMessage() { return getMessage(); }\n");
            // The name comes from the *object*, not from the class this method
            // was resolved on — `getName()` already answers `java.lang.Foo` for
            // a modeled throwable (including the two that are not in
            // `java.lang`) and `T$MyEx` for a user-defined one.
            src.push_str(
                "  String toString() { String n = this.getClass().getName(); \
                 if (detailMessage == null) { return n; } return n + \": \" + detailMessage; }\n",
            );
        } else {
            src.push_str(&format!("  {name}() {{ }}\n"));
            src.push_str(&format!("  {name}(String m) {{ super(m); }}\n"));
        }
        src.push_str("}\n");
    }
    src
}

/// Add the modeled `java.lang` throwables to `prog` when it uses exceptions.
///
/// A no-op for a program that never throws, catches, or constructs a throwable —
/// which keeps every existing program's AST, bytecode, and class table exactly
/// as they were. A class the user declares themselves (`class Exception { … }`)
/// wins: its name is skipped, so the user's definition is the only one.
pub fn inject(prog: &mut Program) -> Result<(), String> {
    if prog.uses_exceptions {
        let src = prelude_source(&declared_names(prog));
        merge(prog, &src)?;
    }
    if prog.uses_functional {
        let src = functional_source(&declared_names(prog));
        merge(prog, &src)?;
    }
    Ok(())
}

/// The names of every type the unit already declares (a user declaration always
/// wins over a prelude one of the same name).
fn declared_names(prog: &Program) -> Vec<String> {
    prog.classes.iter().map(|c| c.name.clone()).collect()
}

/// Parse a prelude compilation unit and fold its types into `prog`.
fn merge(prog: &mut Program, src: &str) -> Result<(), String> {
    let parsed = crate::parser::parse(src)?;
    // Everything except the synthetic entry class (which only exists because a
    // compilation unit must declare a `main`). `__` is not a name any Java
    // program uses for a type, and both prelude units spell theirs that way.
    prog.classes.extend(
        parsed
            .classes
            .into_iter()
            .filter(|c| !c.name.starts_with("__")),
    );
    // A prelude type's `static` members (`Function.identity`, `Predicate.not`,
    // `BinaryOperator.minBy`) live in the unit-wide pool a qualified `C.m(…)`
    // resolves against, not on the class node, so the pool has to be merged too.
    prog.methods.extend(
        parsed
            .methods
            .into_iter()
            .filter(|m| !m.owner.starts_with("__")),
    );
    Ok(())
}

/// The Java source of the functional interfaces the program does not already
/// declare. Each is a plain `interface` with one abstract method, so a lambda
/// assigned to it reaches the compiler's ordinary single-abstract-method lambda
/// target path, plus the `default`/`static` members the JDK declares alongside.
fn functional_source(declared: &[String]) -> String {
    let mut src = String::from(
        "public class __JavaUtilFunction { public static void main(String[] a) { } }\n",
    );
    for (name, sam, members) in FUNCTIONAL {
        if declared.iter().any(|d| d == name) {
            continue;
        }
        src.push_str(&format!("interface {name} {{ {sam}; {members} }}\n"));
    }
    src
}
