//! javars — Java as a fusevm frontend.
//!
//! Pipeline: `lexer` → `parser` builds a Java AST → `compiler` lowers it to a
//! `fusevm::Chunk` → fusevm executes it on the shared three-tier Cranelift JIT,
//! calling back into `host` (the strict numeric hook) for Java's `String` `+`
//! overload. There is no bespoke VM or JVM here — execution and codegen live in
//! fusevm, the same engine behind zshrs, stryke, awkrs, elisp, and ruby.

pub mod ast;
pub mod banner;
pub mod cli;
pub mod compiler;
pub mod dap;
pub mod host;
pub mod lexer;
pub mod lsp;
pub mod parser;
pub mod prelude;
pub mod reference;
pub mod regex;
pub mod rust_ffi;
pub mod tiers;

pub use banner::version_banner;
use fusevm::{VMResult, Value, VM};

/// Parse Java `src` to an AST. Inline `rust { ... }` FFI blocks are desugared to
/// `__rust_compile(...)` statements first (see [`rust_ffi`]), so every parse
/// path — run, `--dump-ast`, `--disasm`, `--dap` — sees the same rewritten
/// source. No-op when the source has no `rust` block.
pub fn parse(src: &str) -> Result<ast::Program, String> {
    let src = rust_ffi::desugar(src);
    let mut prog = parser::parse(&src)?;
    // A program that throws or catches also declares, implicitly, the
    // `java.lang` throwable classes it names — there is no JDK to import them
    // from. No-op for every other program (see [`prelude::inject`]).
    prelude::inject(&mut prog)?;
    Ok(prog)
}

/// Parse and lower Java `src` to a runnable fusevm chunk.
pub fn compile(src: &str) -> Result<fusevm::Chunk, String> {
    let prog = parse(src)?;
    compiler::compile(&prog)
}

/// Register the javars builtins + strict numeric hook on a fresh VM, enable the
/// tracing JIT, and run the chunk. `supers` is the class → direct-superclass
/// map (for `instanceof`/`toString`); `exceptions` says whether the chunk was
/// compiled with the exception machinery (so a runtime fault can become a
/// catchable throwable rather than an abort). Returns the last top-of-stack
/// value.
fn run_chunk(
    chunk: fusevm::Chunk,
    supers: std::collections::HashMap<String, Vec<String>>,
    exceptions: bool,
    argv: &[String],
) -> Result<Value, String> {
    // A fresh object heap per run — no handle leaks across programs.
    host::heap_reset();
    host::set_supertypes(supers);
    host::set_exceptions_enabled(exceptions);
    host::set_argv(argv.to_vec());
    let mut vm = VM::new(chunk);
    host::install(&mut vm);
    vm.set_numeric_hook(std::sync::Arc::new(host::numeric_hook));
    vm.enable_tracing_jit();
    let result = vm.run();
    // A host builtin fault (inline-Rust FFI compile/call error, or a `String`
    // method fault such as an out-of-range index) stashes its message and halts
    // the VM. The VM reports `Ok(top-of-stack)` when a value remains on the
    // stack and `Halted` only when it is empty, so the pending message must be
    // checked on **both** paths — a fault mid-expression (e.g. inside a
    // `println(...)` argument) leaves its `Undef` return on the stack and would
    // otherwise be silently swallowed.
    if let Some(e) = host::take_ffi_error() {
        return Err(e);
    }
    match result {
        VMResult::Ok(v) => Ok(v),
        VMResult::Halted => Ok(vm.stack.last().cloned().unwrap_or(Value::Undef)),
        VMResult::Error(e) => Err(e),
    }
}

/// Compile and run a Java source string with no program arguments (`args` is a
/// zero-length `String[]`); return the last VM value.
pub fn run_str(src: &str) -> Result<Value, String> {
    run_str_args(src, &[])
}

/// Compile and run a Java source string, binding `argv` to `main`'s `String[]`
/// parameter.
pub fn run_str_args(src: &str, argv: &[String]) -> Result<Value, String> {
    let prog = parse(src)?;
    // The type → direct-supertypes map (superclass + interfaces) drives runtime
    // `instanceof`/`toString`.
    let supers = prog
        .classes
        .iter()
        .map(|c| {
            let mut sups: Vec<String> = c.superclass.clone().into_iter().collect();
            sups.extend(c.interfaces.iter().cloned());
            (c.name.clone(), sups)
        })
        .collect();
    let chunk = compiler::compile(&prog)?;
    run_chunk(chunk, supers, prog.uses_exceptions, argv)
}

/// Read and run a `.java` file.
pub fn run_file(path: &str) -> Result<Value, String> {
    let src =
        std::fs::read_to_string(path).map_err(|e| format!("javars: cannot read {path}: {e}"))?;
    run_str(&src)
}

/// Compile `src` and return a human-readable disassembly of the fusevm chunk
/// (for `java --disasm`).
pub fn disassemble(src: &str) -> Result<String, String> {
    Ok(compile(src)?.disassemble())
}
