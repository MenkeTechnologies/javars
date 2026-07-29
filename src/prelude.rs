//! The implicit `java.lang` exception prelude.
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
//! The prelude is injected only into programs that actually use exceptions (see
//! [`inject`]), so a program that never throws carries none of these classes and
//! emits exactly the bytecode it did before.

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
];

/// True when `name` is one of the modeled `java.lang` throwables.
pub fn is_throwable(name: &str) -> bool {
    THROWABLES.iter().any(|(n, _)| *n == name)
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
        src.push_str(&format!(
            "  String toString() {{ if (detailMessage == null) {{ return \"java.lang.{name}\"; }} return \"java.lang.{name}: \" + detailMessage; }}\n"
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
    if !prog.uses_exceptions {
        return Ok(());
    }
    let declared: Vec<String> = prog.classes.iter().map(|c| c.name.clone()).collect();
    let src = prelude_source(&declared);
    let parsed = crate::parser::parse(&src)?;
    // Everything except the synthetic entry class (which only exists because a
    // compilation unit must declare a `main`).
    prog.classes.extend(
        parsed
            .classes
            .into_iter()
            .filter(|c| c.name != "__JavaLang"),
    );
    Ok(())
}
