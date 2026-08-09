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
        "ConcurrentModificationException" => format!("java.util.{name}"),
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

/// The modeled functional interfaces: `(name, declared single abstract method)`.
///
/// Each is declared to javars as an ordinary `interface` with exactly one
/// abstract method, which is all a lambda target needs — the compiler recognises
/// *any* single-abstract-method interface (user-declared ones included) as a
/// lambda target, so these entries buy no special case, only the declarations
/// the JDK would have supplied. Type parameters are erased, so the parameter and
/// return types below are the erasure Java itself uses (`Object`), except where
/// the JDK's own signature is primitive (`Predicate.test` → `boolean`,
/// `Comparator.compare` → `int`), which javars keeps so a call participates in
/// numeric typing.
pub const FUNCTIONAL: &[(&str, &str)] = &[
    ("Runnable", "void run()"),
    ("Callable", "Object call()"),
    ("Supplier", "Object get()"),
    ("Consumer", "void accept(Object t)"),
    ("BiConsumer", "void accept(Object t, Object u)"),
    ("Function", "Object apply(Object t)"),
    ("BiFunction", "Object apply(Object t, Object u)"),
    ("UnaryOperator", "Object apply(Object t)"),
    ("BinaryOperator", "Object apply(Object t, Object u)"),
    ("Predicate", "boolean test(Object t)"),
    ("BiPredicate", "boolean test(Object t, Object u)"),
    ("Comparator", "int compare(Object a, Object b)"),
    ("IntSupplier", "int getAsInt()"),
    ("IntPredicate", "boolean test(int value)"),
    ("IntConsumer", "void accept(int value)"),
    ("IntUnaryOperator", "int applyAsInt(int operand)"),
    ("IntBinaryOperator", "int applyAsInt(int left, int right)"),
    ("ToIntFunction", "int applyAsInt(Object value)"),
    ("ToDoubleFunction", "double applyAsDouble(Object value)"),
];

/// True when `name` is one of the modeled functional interfaces.
pub fn is_functional(name: &str) -> bool {
    FUNCTIONAL.iter().any(|(n, _)| *n == name)
}

/// The Java source of the prelude classes the program does not already declare.
///
/// `Throwable` holds the message and the accessors; every subclass adds only its
/// two constructors and a `toString()` that names itself, because Java's
/// `Throwable.toString()` prints the *runtime* class name and javars resolves
/// that by virtual dispatch on the override rather than by a runtime class query.
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
        } else {
            src.push_str(&format!("  {name}() {{ }}\n"));
            src.push_str(&format!("  {name}(String m) {{ super(m); }}\n"));
        }
        // The qualified name has to come from [`qualified_throwable`], not a
        // hardcoded `java.lang.` — `PatternSyntaxException` is `java.util.regex`
        // and `ConcurrentModificationException` is `java.util`, and a caught one
        // printed under the wrong package is a silently wrong answer.
        let qualified = qualified_throwable(name).unwrap_or_else(|| (*name).to_string());
        src.push_str(&format!(
            "  String toString() {{ if (detailMessage == null) {{ return \"{qualified}\"; }} return \"{qualified}: \" + detailMessage; }}\n"
        ));
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
    Ok(())
}

/// The Java source of the functional interfaces the program does not already
/// declare. Each is a plain one-method `interface`, so a lambda assigned to it
/// reaches the compiler's ordinary single-abstract-method lambda target path.
fn functional_source(declared: &[String]) -> String {
    let mut src = String::from(
        "public class __JavaUtilFunction { public static void main(String[] a) { } }\n",
    );
    for (name, sam) in FUNCTIONAL {
        if declared.iter().any(|d| d == name) {
            continue;
        }
        src.push_str(&format!("interface {name} {{ {sam}; }}\n"));
    }
    src
}
