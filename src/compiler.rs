//! Lower the Java AST to a `fusevm::Chunk`.
//!
//! There is no bespoke VM or JVM here: statements and expressions emit fusevm
//! ops (`LoadInt`, `Add`, `GetVar`, `JumpIfFalse`, `PrintLn`, …) into a
//! `ChunkBuilder`, and fusevm runs the chunk on its three-tier Cranelift JIT.
//! Java values ride the fusevm value model; the strict numeric hook in
//! `crate::host` supplies string `+` concatenation for the mixed operands the
//! VM's native arithmetic does not compute.
//!
//! Locals are addressed by name through `GetVar`/`SetVar` (slice 1 has a single
//! `main` frame with no lexical scopes), so this stays a direct, readable
//! lowering. `break`/`continue` are backpatched through a loop-context stack.

use crate::ast::*;
use fusevm::{Chunk, ChunkBuilder, Op, Value};
use std::collections::HashMap;

/// The desugar target an inline `rust { ... }` FFI block lowers to (see
/// [`crate::rust_ffi`]).
const RUST_COMPILE: &str = "__rust_compile";

/// The static numeric category of an expression, used to reproduce Java's
/// binary numeric promotion. Java's `/` truncates when both operands are
/// integral and divides as floating point when either is `float`/`double`; the
/// fusevm runtime is untyped, so the compiler tracks this statically and emits
/// a truncating division only for `Int` ÷ `Int`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NumType {
    /// An integral type (`int`, `long`, `short`, `byte`, `char`).
    Int,
    /// A floating-point type (`float`, `double`).
    Float,
    /// A non-numeric or statically-unknown type (`String`, `boolean`, an
    /// untyped `var`, an unresolved variable). Never truncates.
    Other,
}

/// Map a declaration-position type name to its numeric category. Returns `None`
/// for `var` and unknown types, whose category is inferred from an initializer.
fn numtype_of_ty(ty: &str) -> Option<NumType> {
    match ty {
        "int" | "long" | "short" | "byte" | "char" => Some(NumType::Int),
        "float" | "double" => Some(NumType::Float),
        "boolean" | "String" => Some(NumType::Other),
        _ => None,
    }
}

/// Static signature of a user-defined static method: its parameter arity (for
/// call-site checking) and the numeric category of its return type (so a call
/// participates in division typing).
struct MethodSig {
    arity: usize,
    ret: NumType,
}

/// The lowering scope for a user-defined method body. Locals and parameters
/// live in fusevm call-frame slots (`GetSlot`/`SetSlot`) rather than the shared
/// globals `main` uses, so recursion does not clobber a caller's variables.
struct MethodScope {
    /// Local/parameter name → frame slot index (allocated on first mention).
    slots: HashMap<String, u16>,
    /// Next free slot index.
    next_slot: u16,
    /// Declared numeric types of this method's locals/parameters.
    types: HashMap<String, NumType>,
}

impl MethodScope {
    fn new() -> Self {
        MethodScope {
            slots: HashMap::new(),
            next_slot: 0,
            types: HashMap::new(),
        }
    }

    /// Slot index for `name`, allocating a fresh one on first mention.
    fn slot(&mut self, name: &str) -> u16 {
        if let Some(&s) = self.slots.get(name) {
            return s;
        }
        let s = self.next_slot;
        self.next_slot += 1;
        self.slots.insert(name.to_string(), s);
        s
    }
}

/// One enclosing loop's backpatch targets.
struct Loop {
    /// `continue` jump op indices, patched to the loop's continue target (the
    /// step/condition entry) once it is known. Backpatched — not read eagerly —
    /// because a `for` loop's continue target (the update clause) is only known
    /// after its body is lowered.
    continue_ops: Vec<usize>,
    /// `break` jump op indices, patched to the loop exit once known.
    break_ops: Vec<usize>,
}

struct Compiler {
    b: ChunkBuilder,
    loops: Vec<Loop>,
    /// A top-level `break`/`return;` (no enclosing loop) jumps to program end.
    exit_ops: Vec<usize>,
    /// When true, emit a per-statement `CallBuiltin(DBG_LINE)` line marker so the
    /// `--dap` debugger can stop on statement lines. Normal runs leave this off
    /// and carry zero extra ops.
    debug: bool,
    /// True when the program contains an inline `rust { ... }` FFI block (a
    /// `__rust_compile` call). Only then does an unresolved call name lower to a
    /// runtime FFI dispatch instead of a compile error — so non-FFI programs keep
    /// their exact "unresolved reference" compile-time diagnostic.
    has_ffi: bool,
    /// Declared numeric types of `main`'s locals (the global/`main` scope,
    /// keyed by name). Method locals live in [`Compiler::scope`] instead.
    global_types: HashMap<String, NumType>,
    /// The active method scope while lowering a method body; `None` while
    /// lowering `main`. Selects slot-based vs. global variable access.
    scope: Option<MethodScope>,
    /// User-defined static method signatures, keyed by name — populated before
    /// any body is lowered so calls (including forward and recursive ones)
    /// resolve.
    methods: HashMap<String, MethodSig>,
}

/// Compile a parsed [`Program`]'s `main` body to a runnable fusevm chunk.
pub fn compile(prog: &Program) -> Result<Chunk, String> {
    compile_with(prog, false)
}

/// Compile with per-statement `DBG_LINE` line markers for the `--dap` debugger.
/// Identical to [`compile`] except each statement is preceded by a marker
/// carrying its source line (see [`crate::host::DBG_LINE`]).
pub fn compile_debug(prog: &Program) -> Result<Chunk, String> {
    compile_with(prog, true)
}

fn compile_with(prog: &Program, debug: bool) -> Result<Chunk, String> {
    let has_ffi = body_has_ffi(&prog.main);
    // Register every method signature up front so calls resolve regardless of
    // source order (forward references, recursion, mutual recursion).
    let mut methods = HashMap::new();
    for m in &prog.methods {
        methods.insert(
            m.name.clone(),
            MethodSig {
                arity: m.params.len(),
                ret: numtype_of_ty(&m.ret).unwrap_or(NumType::Other),
            },
        );
    }
    let mut c = Compiler {
        b: ChunkBuilder::new(),
        loops: Vec::new(),
        exit_ops: Vec::new(),
        debug,
        has_ffi,
        global_types: HashMap::new(),
        scope: None,
        methods,
    };
    // ── main body (global scope) ──
    for stmt in &prog.main {
        c.stmt(stmt)?;
    }
    // Patch any program-level `break`/`return;` to the position right after
    // main. When methods follow, that position holds the skip-over jump.
    let end = c.b.current_pos();
    let exit_ops = std::mem::take(&mut c.exit_ops);
    for op in exit_ops {
        c.b.patch_jump(op, end);
    }
    // ── method bodies ──
    // Emitted after `main` and jumped over so control never falls into them;
    // each is reached only via `Op::Call`.
    if !prog.methods.is_empty() {
        let skip = c.b.emit(Op::Jump(0), 0);
        for m in &prog.methods {
            c.compile_method(m)?;
        }
        let after = c.b.current_pos();
        c.b.patch_jump(skip, after);
    }
    Ok(c.b.build())
}

impl Compiler {
    // ── variable access (global `main` scope vs. slot-based method scope) ──

    /// Emit a read of local/parameter `name`: a frame slot in a method scope,
    /// or a global name-pool variable in `main`.
    fn emit_get(&mut self, name: &str, line: u32) {
        match &mut self.scope {
            Some(scope) => {
                let slot = scope.slot(name);
                self.b.emit(Op::GetSlot(slot), line);
            }
            None => {
                let idx = self.b.add_name(name);
                self.b.emit(Op::GetVar(idx), line);
            }
        }
    }

    /// Emit a store of the top-of-stack into local/parameter `name`.
    fn emit_set(&mut self, name: &str, line: u32) {
        match &mut self.scope {
            Some(scope) => {
                let slot = scope.slot(name);
                self.b.emit(Op::SetSlot(slot), line);
            }
            None => {
                let idx = self.b.add_name(name);
                self.b.emit(Op::SetVar(idx), line);
            }
        }
    }

    /// Record the declared numeric type of a local/parameter in the active scope.
    fn declare_type(&mut self, name: &str, nt: NumType) {
        match &mut self.scope {
            Some(scope) => {
                scope.types.insert(name.to_string(), nt);
            }
            None => {
                self.global_types.insert(name.to_string(), nt);
            }
        }
    }

    /// Look up the declared numeric type of `name`, defaulting to `Other` when
    /// unknown (an undeclared read, or a type javars does not track).
    fn lookup_type(&self, name: &str) -> NumType {
        let map = match &self.scope {
            Some(scope) => &scope.types,
            None => &self.global_types,
        };
        map.get(name).copied().unwrap_or(NumType::Other)
    }

    /// The static numeric category of `e` under Java's binary numeric promotion.
    /// Drives the truncating-vs-floating choice for `/`.
    fn expr_type(&self, e: &Expr) -> NumType {
        match e {
            Expr::Int(_) => NumType::Int,
            Expr::Float(_) => NumType::Float,
            Expr::Str(_) | Expr::Bool(_) => NumType::Other,
            Expr::Var(name) => self.lookup_type(name),
            Expr::Unary { op, rhs } => match op {
                // `-x` keeps the operand's numeric type; `!b` is boolean.
                UnOp::Neg => self.expr_type(rhs),
                UnOp::Not => NumType::Other,
            },
            Expr::Binary { op, lhs, rhs } => match op {
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
                    let l = self.expr_type(lhs);
                    let r = self.expr_type(rhs);
                    // A non-numeric operand (String `+` concat, or unknown) is
                    // not integral; otherwise a float operand promotes to float.
                    if l == NumType::Other || r == NumType::Other {
                        NumType::Other
                    } else if l == NumType::Float || r == NumType::Float {
                        NumType::Float
                    } else {
                        NumType::Int
                    }
                }
                // Comparisons and logical ops yield `boolean`.
                _ => NumType::Other,
            },
            Expr::PostIncDec { name, .. } => self.lookup_type(name),
            Expr::Println { .. } => NumType::Other,
            Expr::Call { name, .. } => self
                .methods
                .get(name)
                .map(|s| s.ret)
                .unwrap_or(NumType::Other),
            // The `String` methods that return `int` participate in `/` typing.
            Expr::MethodCall { method, .. } => match method.as_str() {
                "length" | "indexOf" => NumType::Int,
                _ => NumType::Other,
            },
        }
    }

    /// Lower one user-defined static method to a call-frame subroutine. Args
    /// arrive on the value stack (`arg0` deepest); the prologue binds them into
    /// frame slots `0..arity`, the body runs in slot scope, and every exit
    /// leaves exactly one value on the stack (`Undef` for `void`) so a call is
    /// always stack-balanced.
    fn compile_method(&mut self, m: &Method) -> Result<(), String> {
        let entry = self.b.current_pos();
        let name_idx = self.b.add_name(&m.name);
        self.b.add_sub_entry(name_idx, entry);

        let mut scope = MethodScope::new();
        // Pre-allocate parameter slots 0..n in declaration order and record
        // their declared types.
        for p in &m.params {
            let slot = scope.slot(&p.name);
            scope.types.insert(
                p.name.clone(),
                numtype_of_ty(&p.ty).unwrap_or(NumType::Other),
            );
            debug_assert_eq!(slot as usize, scope.slots.len() - 1);
        }
        self.scope = Some(scope);

        // Prologue: pop args into their slots. The last parameter is on top of
        // the stack, so bind slots high-to-low.
        for i in (0..m.params.len()).rev() {
            self.b.emit(Op::SetSlot(i as u16), m.line);
        }

        for s in &m.body {
            self.stmt(s)?;
        }
        // Implicit `return;` on fall-off — `void` methods yield `null`.
        self.b.emit(Op::LoadUndef, m.line);
        self.b.emit(Op::ReturnValue, m.line);

        self.scope = None;
        Ok(())
    }

    fn stmt(&mut self, s: &Stmt) -> Result<(), String> {
        // In debug mode, emit a line marker before the statement so `--dap` can
        // stop on it. `CallBuiltin` always pushes its return value, so pop it.
        if self.debug && s.line != 0 {
            self.b
                .emit(Op::CallBuiltin(crate::host::DBG_LINE, 0), s.line);
            self.b.emit(Op::Pop, s.line);
        }
        let line = s.line;
        match &s.kind {
            StmtKind::Local { ty, name, init } => {
                // Record the declared numeric type (for `/` truncation); `var`
                // and untracked types are inferred from the initializer.
                let nt = numtype_of_ty(ty)
                    .or_else(|| init.as_ref().map(|e| self.expr_type(e)))
                    .unwrap_or(NumType::Other);
                self.declare_type(name, nt);
                if let Some(e) = init {
                    self.expr(e)?;
                    self.emit_set(name, line);
                }
                // An uninitialized local is simply unbound until first assigned
                // (Java's definite-assignment check is not enforced in slice 1).
                Ok(())
            }
            StmtKind::Assign { name, op, value } => {
                match op {
                    AssignOp::Assign => {
                        self.expr(value)?;
                    }
                    AssignOp::Div => {
                        // `x /= e` — integer division truncates when both `x`
                        // and `e` are statically integral (Java `int /= int`).
                        let l = self.lookup_type(name);
                        self.emit_get(name, line);
                        let r = self.expr_type(value);
                        self.expr(value)?;
                        self.emit_div(l, r, line);
                    }
                    _ => {
                        // `x <op>= e` → x = x <op> e
                        self.emit_get(name, line);
                        self.expr(value)?;
                        self.b.emit(compound_op(*op), line);
                    }
                }
                self.emit_set(name, line);
                Ok(())
            }
            StmtKind::Expr(Expr::Println { newline, arg }) => {
                // The print builtin returns `null`; discard it in statement
                // position.
                self.println(*newline, arg.as_deref())?;
                self.b.emit(Op::Pop, line);
                Ok(())
            }
            StmtKind::Expr(Expr::PostIncDec { name, inc }) => {
                self.post_inc_dec(name, *inc);
                Ok(())
            }
            StmtKind::Expr(e) => {
                self.expr(e)?;
                self.b.emit(Op::Pop, line);
                Ok(())
            }
            StmtKind::If { cond, then, els } => self.if_stmt(cond, then, els),
            StmtKind::While { cond, body } => self.while_stmt(cond, body),
            StmtKind::For {
                init,
                cond,
                update,
                body,
            } => self.for_stmt(init, cond, update, body),
            StmtKind::Break => {
                let op = self.b.emit(Op::Jump(0), line);
                match self.loops.last_mut() {
                    Some(l) => l.break_ops.push(op),
                    None => self.exit_ops.push(op),
                }
                Ok(())
            }
            StmtKind::Continue => {
                let op = self.b.emit(Op::Jump(0), line);
                match self.loops.last_mut() {
                    Some(l) => l.continue_ops.push(op),
                    None => return Err("javars: `continue` outside a loop".to_string()),
                }
                Ok(())
            }
            StmtKind::Return(val) => {
                if self.scope.is_some() {
                    // In a method: return a value (or `null` for `void`).
                    match val {
                        Some(e) => self.expr(e)?,
                        None => {
                            self.b.emit(Op::LoadUndef, line);
                        }
                    }
                    self.b.emit(Op::ReturnValue, line);
                } else {
                    // In `main` (void): a bare `return;` ends the program; a
                    // value return is a type error javars does not accept.
                    if val.is_some() {
                        return Err(format!(
                            "javars: `return <value>` from void main is not supported (line {line})"
                        ));
                    }
                    let op = self.b.emit(Op::Jump(0), line);
                    self.exit_ops.push(op);
                }
                Ok(())
            }
        }
    }

    fn if_stmt(&mut self, cond: &Expr, then: &[Stmt], els: &[Stmt]) -> Result<(), String> {
        self.expr(cond)?;
        let jf = self.b.emit(Op::JumpIfFalse(0), 0);
        for s in then {
            self.stmt(s)?;
        }
        if els.is_empty() {
            let end = self.b.current_pos();
            self.b.patch_jump(jf, end);
        } else {
            let jend = self.b.emit(Op::Jump(0), 0);
            let else_start = self.b.current_pos();
            self.b.patch_jump(jf, else_start);
            for s in els {
                self.stmt(s)?;
            }
            let end = self.b.current_pos();
            self.b.patch_jump(jend, end);
        }
        Ok(())
    }

    fn while_stmt(&mut self, cond: &Expr, body: &[Stmt]) -> Result<(), String> {
        let top = self.b.current_pos();
        self.expr(cond)?;
        let jf = self.b.emit(Op::JumpIfFalse(0), 0);
        self.loops.push(Loop {
            continue_ops: Vec::new(),
            break_ops: Vec::new(),
        });
        for s in body {
            self.stmt(s)?;
        }
        // `continue` re-tests the condition — target it at the loop top.
        let l = self.loops.pop().unwrap();
        for op in &l.continue_ops {
            self.b.patch_jump(*op, top);
        }
        self.b.emit(Op::Jump(top), 0);
        let end = self.b.current_pos();
        self.b.patch_jump(jf, end);
        for op in l.break_ops {
            self.b.patch_jump(op, end);
        }
        Ok(())
    }

    fn for_stmt(
        &mut self,
        init: &Option<Box<Stmt>>,
        cond: &Option<Expr>,
        update: &Option<Box<Stmt>>,
        body: &[Stmt],
    ) -> Result<(), String> {
        if let Some(init) = init {
            self.stmt(init)?;
        }
        let top = self.b.current_pos();
        let jf = match cond {
            Some(c) => {
                self.expr(c)?;
                Some(self.b.emit(Op::JumpIfFalse(0), 0))
            }
            None => None,
        };
        // `continue` runs the update clause, then re-tests — target it at the
        // step label emitted after the body.
        self.loops.push(Loop {
            continue_ops: Vec::new(),
            break_ops: Vec::new(),
        });
        for s in body {
            self.stmt(s)?;
        }
        // step label: the continue target is the update clause (or the loop-top
        // re-test when there is no update).
        let step = self.b.current_pos();
        if let Some(update) = update {
            self.stmt(update)?;
        }
        self.b.emit(Op::Jump(top), 0);
        let end = self.b.current_pos();
        if let Some(jf) = jf {
            self.b.patch_jump(jf, end);
        }
        let l = self.loops.pop().unwrap();
        for op in l.continue_ops {
            self.b.patch_jump(op, step);
        }
        for op in l.break_ops {
            self.b.patch_jump(op, end);
        }
        Ok(())
    }

    fn post_inc_dec(&mut self, name: &str, inc: bool) {
        self.emit_get(name, 0);
        self.b.emit(Op::LoadInt(1), 0);
        self.b.emit(if inc { Op::Add } else { Op::Sub }, 0);
        self.emit_set(name, 0);
    }

    /// Lower `System.out.print[ln](arg)` to the Java-formatting print builtin.
    /// Leaves the builtin's `null` return value on the stack.
    fn println(&mut self, newline: bool, arg: Option<&Expr>) -> Result<(), String> {
        let n = match arg {
            Some(e) => {
                self.expr(e)?;
                1
            }
            None => 0,
        };
        let id = if newline {
            crate::host::JPRINTLN
        } else {
            crate::host::JPRINT
        };
        self.b.emit(Op::CallBuiltin(id, n), 0);
        Ok(())
    }

    fn expr(&mut self, e: &Expr) -> Result<(), String> {
        match e {
            Expr::Int(n) => {
                self.b.emit(Op::LoadInt(*n), 0);
            }
            Expr::Float(f) => {
                let c = self.b.add_constant(Value::float(*f));
                self.b.emit(Op::LoadConst(c), 0);
            }
            Expr::Str(s) => {
                let c = self.b.add_constant(Value::str(s.clone()));
                self.b.emit(Op::LoadConst(c), 0);
            }
            Expr::Bool(b) => {
                self.b
                    .emit(if *b { Op::LoadTrue } else { Op::LoadFalse }, 0);
            }
            Expr::Var(name) => {
                self.emit_get(name, 0);
            }
            Expr::Unary { op, rhs } => {
                self.expr(rhs)?;
                match op {
                    UnOp::Neg => {
                        self.b.emit(Op::Negate, 0);
                    }
                    UnOp::Not => {
                        self.b.emit(Op::LogNot, 0);
                    }
                }
            }
            Expr::Binary { op, lhs, rhs } => self.binary(*op, lhs, rhs)?,
            // Println/PostIncDec in value position are handled as statements;
            // if one reaches here (nested), the print builtin already leaves its
            // `null` return value on the stack.
            Expr::Println { newline, arg } => {
                self.println(*newline, arg.as_deref())?;
            }
            Expr::PostIncDec { name, inc } => {
                self.emit_get(name, 0);
                self.post_inc_dec(name, *inc);
            }
            Expr::Call { name, args, line } => self.call(name, args, *line)?,
            Expr::MethodCall {
                recv,
                method,
                args,
                line,
            } => self.method_call(recv, method, args, *line)?,
        }
        Ok(())
    }

    /// Lower an instance method call `recv.method(args...)`. Slice 1 dispatches
    /// on `String` receivers through the [`crate::host::JSTR_DISPATCH`] builtin:
    /// the receiver is pushed first, then the arguments, then the method name,
    /// matching the builtin's `[recv, args…, name]` stack contract.
    fn method_call(
        &mut self,
        recv: &Expr,
        method: &str,
        args: &[Expr],
        line: u32,
    ) -> Result<(), String> {
        self.expr(recv)?;
        for a in args {
            self.expr(a)?;
        }
        let name_c = self.b.add_constant(Value::str(method.to_string()));
        self.b.emit(Op::LoadConst(name_c), line);
        // argc counts the receiver, the arguments, and the method-name string.
        self.b.emit(
            Op::CallBuiltin(crate::host::JSTR_DISPATCH, args.len() as u8 + 2),
            line,
        );
        Ok(())
    }

    /// Lower a bare-identifier call `name(args...)`. Slice 1 declares no user
    /// methods, so the only calls that resolve are the inline-Rust FFI ones:
    ///
    /// - `__rust_compile("<base64>", line)` — the desugar target of a
    ///   `rust { ... }` block. Compile the base64 body and register its exports
    ///   via the `JFFI_COMPILE` builtin; the call evaluates to `null`.
    /// - any other name, **when a `rust { ... }` block is present** — an export
    ///   registered at runtime, dispatched by name through the `JFFI_CALL`
    ///   builtin (args pushed deepest-first, then the name on top).
    ///
    /// With no FFI block in the program, an unknown name stays a compile-time
    /// "unresolved reference" error — javars's existing diagnostic is preserved.
    /// Every branch leaves exactly one value on the stack (the builtin's return,
    /// or the error path never emits).
    fn call(&mut self, name: &str, args: &[Expr], line: u32) -> Result<(), String> {
        if name == RUST_COMPILE {
            // Compile only the base64 body (first arg); the line arg is metadata
            // the builtin does not need.
            if let Some(body) = args.first() {
                self.expr(body)?;
                self.b
                    .emit(Op::CallBuiltin(crate::host::JFFI_COMPILE, 1), line);
            } else {
                self.b.emit(Op::LoadUndef, line);
            }
            return Ok(());
        }
        // A user-defined static method resolves to the native call-frame ABI.
        if let Some(sig) = self.methods.get(name) {
            if args.len() != sig.arity {
                return Err(format!(
                    "javars: method `{name}` expects {} argument(s) but got {} (line {line})",
                    sig.arity,
                    args.len()
                ));
            }
            for a in args {
                self.expr(a)?;
            }
            let name_idx = self.b.add_name(name);
            self.b.emit(Op::Call(name_idx, args.len() as u8), line);
            return Ok(());
        }
        if self.has_ffi {
            for a in args {
                self.expr(a)?;
            }
            let c = self.b.add_constant(Value::str(name.to_string()));
            self.b.emit(Op::LoadConst(c), line);
            self.b.emit(
                Op::CallBuiltin(crate::host::JFFI_CALL, args.len() as u8 + 1),
                line,
            );
            return Ok(());
        }
        Err(format!(
            "javars: unresolved reference: {name} (line {line})"
        ))
    }

    fn binary(&mut self, op: BinOp, lhs: &Expr, rhs: &Expr) -> Result<(), String> {
        // `&&` / `||` short-circuit: keep the deciding operand as the result.
        match op {
            BinOp::And => {
                self.expr(lhs)?;
                let jf = self.b.emit(Op::JumpIfFalseKeep(0), 0);
                self.b.emit(Op::Pop, 0);
                self.expr(rhs)?;
                let end = self.b.current_pos();
                self.b.patch_jump(jf, end);
                return Ok(());
            }
            BinOp::Or => {
                self.expr(lhs)?;
                let jt = self.b.emit(Op::JumpIfTrueKeep(0), 0);
                self.b.emit(Op::Pop, 0);
                self.expr(rhs)?;
                let end = self.b.current_pos();
                self.b.patch_jump(jt, end);
                return Ok(());
            }
            _ => {}
        }
        // `/` truncation is decided from the operands' static types.
        if let BinOp::Div = op {
            let l = self.expr_type(lhs);
            let r = self.expr_type(rhs);
            self.expr(lhs)?;
            self.expr(rhs)?;
            self.emit_div(l, r, 0);
            return Ok(());
        }
        self.expr(lhs)?;
        self.expr(rhs)?;
        let vop = match op {
            BinOp::Add => Op::Add,
            BinOp::Sub => Op::Sub,
            BinOp::Mul => Op::Mul,
            BinOp::Mod => Op::Mod,
            BinOp::Eq => Op::NumEq,
            BinOp::Ne => Op::NumNe,
            BinOp::Lt => Op::NumLt,
            BinOp::Gt => Op::NumGt,
            BinOp::Le => Op::NumLe,
            BinOp::Ge => Op::NumGe,
            BinOp::Div => unreachable!("handled above"),
            BinOp::And | BinOp::Or => unreachable!("handled above"),
        };
        self.b.emit(vop, 0);
        Ok(())
    }

    /// Emit a division of two already-pushed operands. Java `/` divides as
    /// floating point (fusevm's `Op::Div`) and truncates toward zero to an
    /// integer only when both operands are statically integral — reproduced
    /// with a trailing `Op::TruncInt`.
    fn emit_div(&mut self, l: NumType, r: NumType, line: u32) {
        self.b.emit(Op::Div, line);
        if l == NumType::Int && r == NumType::Int {
            self.b.emit(Op::TruncInt, line);
        }
    }
}

// ── FFI detection (does the program contain a `rust { ... }` block?) ────────

/// True if any statement in `body` (recursively) evaluates a `__rust_compile`
/// call — the desugar target of an inline `rust { ... }` block.
fn body_has_ffi(body: &[Stmt]) -> bool {
    body.iter().any(|s| match &s.kind {
        StmtKind::Local { init, .. } => init.as_ref().is_some_and(expr_has_ffi),
        StmtKind::Assign { value, .. } => expr_has_ffi(value),
        StmtKind::Expr(e) => expr_has_ffi(e),
        StmtKind::If { cond, then, els } => {
            expr_has_ffi(cond) || body_has_ffi(then) || body_has_ffi(els)
        }
        StmtKind::While { cond, body } => expr_has_ffi(cond) || body_has_ffi(body),
        StmtKind::For {
            init,
            cond,
            update,
            body,
        } => {
            init.as_deref()
                .is_some_and(|s| body_has_ffi(std::slice::from_ref(s)))
                || cond.as_ref().is_some_and(expr_has_ffi)
                || update
                    .as_deref()
                    .is_some_and(|s| body_has_ffi(std::slice::from_ref(s)))
                || body_has_ffi(body)
        }
        StmtKind::Return(val) => val.as_ref().is_some_and(expr_has_ffi),
        StmtKind::Break | StmtKind::Continue => false,
    })
}

fn expr_has_ffi(e: &Expr) -> bool {
    match e {
        Expr::Call { name, args, .. } => name == RUST_COMPILE || args.iter().any(expr_has_ffi),
        Expr::Unary { rhs, .. } => expr_has_ffi(rhs),
        Expr::Binary { lhs, rhs, .. } => expr_has_ffi(lhs) || expr_has_ffi(rhs),
        Expr::Println { arg, .. } => arg.as_deref().is_some_and(expr_has_ffi),
        Expr::MethodCall { recv, args, .. } => expr_has_ffi(recv) || args.iter().any(expr_has_ffi),
        Expr::Int(_)
        | Expr::Float(_)
        | Expr::Str(_)
        | Expr::Bool(_)
        | Expr::Var(_)
        | Expr::PostIncDec { .. } => false,
    }
}

fn compound_op(op: AssignOp) -> Op {
    match op {
        AssignOp::Add => Op::Add,
        AssignOp::Sub => Op::Sub,
        AssignOp::Mul => Op::Mul,
        AssignOp::Mod => Op::Mod,
        // `/=` is lowered separately so it can truncate int division.
        AssignOp::Div => unreachable!("`/=` lowers through the div-typing path"),
        AssignOp::Assign => unreachable!("plain assign never lowers through compound_op"),
    }
}
