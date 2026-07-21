// Inline Rust FFI: a `rust { ... }` block inside `main` compiles to a cdylib
// whose `pub extern "C"` exports are callable by name from Java. javars
// desugars the block to `__rust_compile("<base64>", line)` before lexing, and
// routes the bareword calls (`j_triple`, `j_add`) through `fusevm::ffi`.
//
//   java examples/Ffi.java
//   => 42
//   => 50
public class Ffi {
    public static void main(String[] args) {
        rust {
            pub extern "C" fn j_triple(x: i64) -> i64 { x * 3 }
            pub extern "C" fn j_add(a: i64, b: i64) -> i64 { a + b }
        }
        System.out.println(j_triple(14));
        System.out.println(j_add(20, 30));
    }
}
