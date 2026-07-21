//! Inline Rust FFI integration tests: run `.java` programs that carry a
//! `rust { ... }` block through the built `java` binary and assert the numbers
//! printed come from a real compiled cdylib (not a stub). Compiling a cdylib
//! needs a Rust toolchain, so these are `#[ignore]`d by default and run
//! explicitly with `cargo test --test ffi -- --ignored`.

use std::process::Command;

/// Run a Java source string through the `java` binary and return (stdout, ok).
fn run(src: &str) -> (String, String, bool) {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("javars_ffi_{}.java", fasthash(src)));
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
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[test]
#[ignore = "compiles a cdylib; needs a Rust toolchain — run with --ignored"]
fn rust_block_export_returns_real_value() {
    let src = r#"
public class T {
    public static void main(String[] args) {
        rust { pub extern "C" fn j_triple(x: i64) -> i64 { x * 3 } }
        System.out.println(j_triple(14));
    }
}
"#;
    let (out, err, ok) = run(src);
    assert!(ok, "run failed: stderr={err}");
    assert_eq!(out, "42\n", "stderr={err}");
}

#[test]
#[ignore = "compiles a cdylib; needs a Rust toolchain — run with --ignored"]
fn rust_block_multiple_exports() {
    let src = r#"
public class T {
    public static void main(String[] args) {
        rust {
            pub extern "C" fn j_triple(x: i64) -> i64 { x * 3 }
            pub extern "C" fn j_add(a: i64, b: i64) -> i64 { a + b }
        }
        System.out.println(j_triple(14));
        System.out.println(j_add(20, 30));
    }
}
"#;
    let (out, err, ok) = run(src);
    assert!(ok, "run failed: stderr={err}");
    assert_eq!(out, "42\n50\n", "stderr={err}");
}

/// Without a `rust { ... }` block, an unknown call name is still a compile-time
/// unresolved-reference error — the FFI dispatch is gated on `has_ffi`.
#[test]
fn unknown_call_without_ffi_is_unresolved() {
    let src = r#"
public class T {
    public static void main(String[] args) {
        System.out.println(nope(1));
    }
}
"#;
    let (_out, err, ok) = run(src);
    assert!(!ok, "unknown call with no rust block should fail");
    assert!(
        err.contains("unresolved reference: nope"),
        "unexpected error: {err}"
    );
}
