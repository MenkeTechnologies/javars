```
     ██╗ █████╗ ██╗   ██╗ █████╗ ██████╗ ███████╗
     ██║██╔══██╗██║   ██║██╔══██╗██╔══██╗██╔════╝
     ██║███████║██║   ██║███████║██████╔╝███████╗
██   ██║██╔══██║╚██╗ ██╔╝██╔══██║██╔══██╗╚════██║
╚█████╔╝██║  ██║ ╚████╔╝ ██║  ██║██║  ██║███████║
 ╚════╝ ╚═╝  ╚═╝  ╚═══╝  ╚═╝  ╚═╝╚═╝  ╚═╝╚══════╝
```

[![CI](https://github.com/MenkeTechnologies/javars/actions/workflows/ci.yml/badge.svg)](https://github.com/MenkeTechnologies/javars/actions/workflows/ci.yml)
![Rust](https://img.shields.io/badge/Rust-2021-05d9e8?style=flat-square)
![license](https://img.shields.io/badge/license-MIT-ff2a6d?style=flat-square)
![status](https://img.shields.io/badge/status-active%20%C2%B7%20in%20development-9b5de5?style=flat-square)

### `[JAVA, COMPILED TO BYTECODE — JIT-COMPILED, NOT WALKED — NO JVM]`

> *"The JVM runs Java on the JVM. javars runs Java on fusevm."*

**Java in Rust** — a Java frontend that lexes and parses Java source, lowers it
to [`fusevm`](https://github.com/MenkeTechnologies/fusevm) bytecode, and runs it
on the shared three-tier Cranelift JIT — the same engine behind `zshrs`,
`stryke`, `awkrs`, `elisp`, and `ruby`. No bespoke VM. No JVM. No `.class`
files.

---

## Table of Contents

- [\[0x00\] Overview](#0x00-overview)
- [\[0x01\] Install](#0x01-install)
- [\[0x02\] Usage](#0x02-usage)
- [\[0x03\] Language Features](#0x03-language-features)
- [\[0x04\] Command-Line Flags](#0x04-command-line-flags)
- [\[0x05\] Architecture](#0x05-architecture)
- [\[0x06\] Status & Roadmap](#0x06-status--roadmap)
- [\[0xFF\] License](#0xff-license)

---

## [0x00] OVERVIEW

Every Java runtime in existence targets the JVM: `javac` emits `.class`
bytecode, and a JVM (HotSpot, OpenJ9, GraalVM) interprets and JIT-compiles it.
`javars` takes a different path — it lexes and parses Java to an AST, lowers
that AST **directly to fusevm bytecode**, and runs it on fusevm's compiled VM
with a Cranelift tracing JIT. javars carries no VM or JIT of its own; it is a
pure frontend over the shared engine. Highlights:

- **Compiled, not tree-walked** — arithmetic, comparisons, and control flow
  lower to native fusevm ops (`LoadInt`, `Add`, `NumLt`, `JumpIfFalse`, …), so
  the tracing JIT compiles hot loops to native code.
- **fusevm-hosted, no JVM** — no local `vm.rs` / `jit.rs`, no `.class` files, no
  `libjvm`. The same three-tier Cranelift engine that hosts zshrs, stryke,
  awkrs, elisp, and ruby runs Java too. `jit-disk-cache` persists native code
  across runs.
- **Java print semantics** — `System.out.print[ln]` lowers to a formatting
  builtin so `boolean` prints `true`/`false`, `double` prints `3.0`, and `null`
  prints `null` — matching `java`, not the VM's shell-flavoured default.
- **Java `+` overloading** — a strict numeric hook supplies string
  concatenation (`"x=" + x`) for the mixed operands the VM's native arithmetic
  does not compute, while all-numeric arithmetic stays on the JIT fast path.
- **Verified against OpenJDK** — the example programs and the test corpus are
  diffed byte-for-byte against a reference `java`; the tests freeze that output
  so CI needs no JDK installed.

This is an early slice: single-class programs whose `main` uses locals,
arithmetic, the C-style control statements, and `System.out.print[ln]`. User
methods, classes, and the standard library are the next waves (see
[`BUGS.md`](BUGS.md)). Nothing is faked — an unsupported construct is a parse
error, not a silent mis-run.

---

## [0x01] INSTALL

```sh
git clone https://github.com/MenkeTechnologies/javars
cd javars
cargo build

# run a .java file
./target/debug/java examples/FizzBuzz.java
```

`javars` is a standalone Rust crate (an explicit empty `[workspace]` keeps it
independent of the meta repo). `fusevm` is pulled from crates.io with the `jit`,
`jit-disk-cache`, and `aot` features. Run the tests with `cargo test` (no JDK
required).

#### Zsh tab completion

```sh
cp completions/_java /usr/local/share/zsh/site-functions/_java
# or: fpath=(/path/to/javars/completions $fpath) in .zshrc
autoload -Uz compinit && compinit
```

---

## [0x02] USAGE

```java
public class FizzBuzz {
    public static void main(String[] args) {
        for (int i = 1; i <= 15; i++) {
            if (i % 15 == 0) {
                System.out.println("FizzBuzz");
            } else if (i % 3 == 0) {
                System.out.println("Fizz");
            } else if (i % 5 == 0) {
                System.out.println("Buzz");
            } else {
                System.out.println(i);
            }
        }
    }
}
```

```sh
$ java FizzBuzz.java
1
2
Fizz
4
Buzz
...
```

---

## [0x03] LANGUAGE FEATURES

Implemented and checked against the reference `java`:

- **Entry point** — `public class Name { public static void main(String[] args) { … } }`.
  A class that also declares other members still parses and runs its `main`
  (non-`main` members are skipped in slice 1).
- **Locals** — `int` / `long` / `double` / `boolean` / `String` / `var`
  declarations with optional initializers; plain and compound assignment
  (`=`, `+=`, `-=`, `*=`, `/=`, `%=`); post-increment / post-decrement
  (`i++`, `i--`).
- **Expressions** — integer / floating / string / char / boolean literals; the
  binary operators `+ - * / %`, `== != < > <= >=`, `&& ||` (short-circuiting);
  unary `-` and `!`; parenthesised grouping; Java's `+` string concatenation.
- **Control flow** — `if` / `else if` / `else`, `while`, the C-style
  `for (init; cond; update)`, `break`, `continue`, and a bare `return;`.
- **Output** — `System.out.println(x)` / `System.out.print(x)` with Java value
  formatting.
- **Comments** — `//` line, `/* … */` block.

---

## [0x04] COMMAND-LINE FLAGS

| Flag | Effect |
| --- | --- |
| `FILE [args…]` | Run a `.java` file. |
| `-version` / `--version` | Print the version banner and exit. |
| `-h` / `--help` | Print usage and exit. |
| `--dump-tokens FILE` | Print the lexer token stream and exit. |
| `--dump-ast FILE` | Print the parsed AST and exit. |
| `--disasm FILE` | Print the lowered fusevm bytecode and exit. |

`java --version` reports the targeted language level (`java 21`) followed by the
real engine (`javars <crate-version>`) and the host triple, so nothing is
misrepresented as the JDK.

---

## [0x05] ARCHITECTURE

javars contains no virtual machine or JIT of its own. The execution path
mirrors how `zshrs` hosts zsh and `ruby` hosts Ruby:

```
Java source → lexer → parser (AST) → lower to fusevm bytecode → fusevm VM + Cranelift JIT
                                            │
                              strict numeric hook (Java `+` concat)
                              print builtins (Java value formatting)
```

| Piece | How |
| --- | --- |
| **fusevm-hosted** | No local `vm.rs` / `jit.rs`, no JVM. Java lowers to fusevm bytecode and runs on the shared three-tier Cranelift JIT; `jit-disk-cache` persists native code across runs. |
| **Native arithmetic** | Operators lower to native fusevm ops; the JIT traces hot integer loops. A strict numeric hook supplies Java's `+` string concatenation only for non-numeric operands. |
| **Java print semantics** | `System.out.print[ln]` lowers to a registered builtin that formats values Java-style (`true`/`false`, `3.0`, `null`), rather than the VM's shell-flavoured `PrintLn`. |

---

## [0x06] STATUS & ROADMAP

Slice 1 (this release): single-class programs, `main`, locals, arithmetic /
comparison / logic, `if` / `while` / `for` / `break` / `continue`,
`System.out.print[ln]`, string concatenation — all verified byte-for-byte
against OpenJDK.

Next waves, in priority order:

1. **User-defined static methods** (recursion, parameters, returns) over
   fusevm's native `Op::Call` frame ABI.
2. **Reference types** — real `String` methods, arrays, and a class/instance
   object model on a host heap.
3. **Standard library surface** — `Math`, `String`/`Integer` statics, common
   `java.util` collections.
4. **A differential parity harness** — a snippet corpus diffed live against a
   reference `java`, frozen and replayed in CI (the pattern `ruby`/`node`/
   `python` frontends use).

See [`BUGS.md`](BUGS.md) for the honest known-gaps list.

---

## [0xFF] LICENSE

MIT — free and open source. See [`LICENSE`](LICENSE).
