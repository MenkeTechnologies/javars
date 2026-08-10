//! Lower the Java AST to a `fusevm::Chunk`.
//!
//! There is no bespoke VM or JVM here: statements and expressions emit fusevm
//! ops (`LoadInt`, `Add`, `GetVar`, `JumpIfFalse`, `PrintLn`, …) into a
//! `ChunkBuilder`, and fusevm runs the chunk on its three-tier Cranelift JIT.
//! Java values ride the fusevm value model; the strict numeric hook in
//! `crate::host` supplies string `+` concatenation for the mixed operands the
//! VM's native arithmetic does not compute.
//!
//! `main`'s locals are addressed by name through `GetVar`/`SetVar` (one frame,
//! no lexical scopes); a method body's locals live in call-frame slots. Both
//! stay a direct, readable lowering. `break`/`continue` are backpatched through
//! a loop-context stack, and `throw`/`catch` through a try-context stack plus a
//! pending-exception check after every call (see the exception section below).

use crate::ast::*;
use fusevm::{Chunk, ChunkBuilder, Op, Value};
use std::collections::HashMap;

/// The desugar target an inline `rust { ... }` FFI block lowers to (see
/// [`crate::rust_ffi`]).
const RUST_COMPILE: &str = "__rust_compile";

/// `super` reaches the parser as an ordinary identifier, so both of its
/// expression forms — `super.field` and `super.method(args)` — arrive as an
/// [`Expr::Var`] receiver. It is a Java reserved word, so no user declaration
/// can ever produce this name and no shadowing check is needed. (`super(args)`
/// constructor chaining is a separate [`Expr::Call`] shape.)
const SUPER: &str = "super";

/// Java's `null` literal reaches the compiler as a bare name (the lexer has no
/// null token), and reading the never-assigned cell is exactly `Value::Undef` —
/// so it is a name that legitimately resolves to nothing.
const NULL_LITERAL: &str = "null";

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
        "int" | "long" | "short" | "byte" | "char" | "Character" => Some(NumType::Int),
        "float" | "double" => Some(NumType::Float),
        "boolean" | "String" => Some(NumType::Other),
        _ => None,
    }
}

/// Static signature of one user-defined static-method overload: its declared
/// parameter types (for overload resolution + mangling) and the numeric category
/// of its return type (so a call participates in division typing). The raw
/// return-type name is kept for static typing of a call result.
struct MethodSig {
    /// The class that declares this overload. Two classes may declare a static
    /// of the same name and signature, so the owner is part of both the
    /// resolution key (a qualified `C.m()` may only reach `C`'s chain) and the
    /// mangled subroutine name (or the two bodies would collide).
    owner: String,
    param_tys: Vec<String>,
    ret: NumType,
    ret_name: String,
    /// True when the last parameter was declared `T...` — the overload is
    /// eligible for Java's variable-arity resolution phase.
    varargs: bool,
}

/// A resolved static-method call: the mangled target subroutine name and its
/// return type (numeric category + raw name).
struct StaticResolved {
    mangled: String,
    ret: NumType,
    ret_name: String,
    /// The chosen overload's declared parameter types — the lambda target type
    /// for each argument.
    param_tys: Vec<String>,
    /// `Some(k)` when the call was resolved in the variable-arity phase: the
    /// arguments from index `k` on are packed into one array at the call site
    /// (see [`Compiler::effective_args`]). `None` is a fixed-arity call, whose
    /// arguments pass through untouched.
    vararg_from: Option<usize>,
}

/// A resolved instance-method call: the chosen overload's declared parameter
/// types (the virtual-dispatch key and the per-argument lambda target type),
/// its return type, and the variable-arity packing point.
struct InstanceResolved {
    param_tys: Vec<String>,
    ret_name: String,
    ret: NumType,
    vararg_from: Option<usize>,
}

/// Compile-time metadata for one user-defined class, resolved with inheritance
/// (fields and methods include those from ancestors). Drives object layout,
/// field-type lookup, and static-type method dispatch.
struct ClassInfo {
    /// Direct superclass name (for `super(...)` constructor chaining), if any.
    superclass: Option<String>,
    /// Direct supertypes (superclass + implemented/extended interfaces), for
    /// `instanceof` and virtual-dispatch subtype tests.
    supertypes: Vec<String>,
    /// True when this is an `interface` (not instantiable; a dispatch-only type).
    is_interface: bool,
    /// True when this is an `enum` — the flag that makes a bare `Color` a type
    /// reference rather than a variable, even for `enum Empty { }`.
    is_enum: bool,
    /// An `enum`'s constant names in declaration order. Index is the constant's
    /// `ordinal()`.
    enum_constants: Vec<String>,
    /// Every instance field this class has (ancestors first, then own), in
    /// initialization order — the sequence the constructor prologue emits.
    fields: Vec<FieldInit>,
    /// Declared type of every field (own + inherited), for static typing.
    field_types: HashMap<String, String>,
    /// Declared type of every `static` field visible on this class — its own
    /// plus every ancestor's, because Java inherits statics by name. Maps the
    /// field name to `(declaring class, declared type)`; the declaring class is
    /// what mints the global, so `Sub.count` and `Base.count` name one cell.
    static_fields: HashMap<String, (String, String)>,
    /// Every method this class can dispatch, keyed by `(name, param_types)` so
    /// same-name overloads differing only in parameter type coexist. Interface
    /// abstract/`default` methods are folded in first, then the class chain
    /// (most-derived wins). An entry whose defining type is an interface abstract
    /// method has no subroutine; it is only reached through virtual dispatch to a
    /// concrete implementor.
    methods: Vec<MethodMeta>,
    /// Constructor signatures this class declares (empty ⇒ implicit default
    /// ctor). Enables constructor overload resolution by type.
    ctors: Vec<CtorSig>,
}

/// One declared constructor's compile-time signature.
#[derive(Clone)]
struct CtorSig {
    param_tys: Vec<String>,
    /// True when the last parameter was declared `T...` (see [`MethodSig`]).
    varargs: bool,
}

/// True when a parameter list ends in a variable-arity parameter (`T... xs`).
/// Java allows it only in last position, so only the last one is consulted.
fn is_varargs(params: &[Param]) -> bool {
    params.last().is_some_and(|p| p.varargs)
}

/// The `i`-th parameter type of a variable-arity signature *expanded* to as
/// many positions as asked for: the fixed parameters as declared, then the
/// trailing array's component type repeated. `(String, int[]…)` expands to
/// `String, int, int, …`.
fn expanded_param(ptys: &[String], i: usize) -> &str {
    let last = ptys.len().saturating_sub(1);
    let p = &ptys[i.min(last)];
    if i >= last {
        p.strip_suffix("[]").unwrap_or(p)
    } else {
        p
    }
}

/// Why a fixed-arity resolution phase selected nothing.
///
/// The distinction decides whether Java's *third* (variable-arity) phase runs:
/// it is reached only when the first two find nothing **applicable**. An
/// ambiguity among applicable fixed-arity candidates is a compile error in
/// Java, not a reason to widen the search — so it must not fall through, or
/// `f(int,int)`/`f(long,long)` against `f(1,2)` could silently land on a
/// `f(int...)` that Java never considers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NoPick {
    /// No candidate's parameters accept the arguments.
    Inapplicable,
    /// Several candidates are equally specific.
    Ambiguous,
}

/// One dispatchable method's compile-time signature: its name, declared
/// parameter types (for overload resolution and mangling), the class that
/// defines the body (`this` binding), and its declared return type.
#[derive(Clone)]
struct MethodMeta {
    name: String,
    param_tys: Vec<String>,
    defining: String,
    ret: String,
    /// True for a bodyless interface/abstract method — it has no subroutine and
    /// is never itself a concrete virtual-dispatch target.
    is_abstract: bool,
    /// True when the last parameter was declared `T...` (see [`MethodSig`]).
    varargs: bool,
}

/// A field with its declared type and optional initializer, in the order it is
/// initialized (used to seed a fresh instance before the constructor runs).
struct FieldInit {
    name: String,
    ty: String,
    init: Option<Expr>,
}

/// The lowering scope for a user-defined method body. Locals and parameters
/// live in fusevm call-frame slots (`GetSlot`/`SetSlot`) rather than the shared
/// globals `main` uses, so recursion does not clobber a caller's variables.
struct MethodScope {
    /// Local/parameter name → frame slot index (allocated on first mention).
    slots: HashMap<String, u16>,
    /// Next free slot index. Starts at 1 for an instance method/constructor
    /// (slot 0 is reserved for `this`).
    next_slot: u16,
    /// Declared numeric types of this method's locals/parameters.
    types: HashMap<String, NumType>,
    /// Declared type name of each local/parameter (raw, e.g. `Point`, `int[]`).
    decl_types: HashMap<String, String>,
    /// Names actually declared as a local or parameter here — distinguishes a
    /// true local from an implicit-`this` field reference.
    declared: std::collections::HashSet<String>,
    /// The slot holding `this`, when this frame has one. Slot 0 for an instance
    /// method or constructor; a lambda body binds `this` as its last captured
    /// upvalue instead, so the slot is not fixed.
    this_slot: Option<u16>,
}

impl MethodScope {
    /// A static-method scope: slots start at 0, no `this`.
    fn new() -> Self {
        MethodScope::with_first_slot(0)
    }

    /// An instance-method/constructor scope: slot 0 is `this`, params start at 1.
    fn for_instance() -> Self {
        let mut s = MethodScope::with_first_slot(1);
        s.this_slot = Some(0);
        s
    }

    fn with_first_slot(first: u16) -> Self {
        MethodScope {
            slots: HashMap::new(),
            next_slot: first,
            types: HashMap::new(),
            decl_types: HashMap::new(),
            declared: std::collections::HashSet::new(),
            this_slot: None,
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

/// What kind of construct a [`BreakScope`] wraps. A `break` may target either a
/// loop or a `switch`; a `continue` targets only a loop.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ScopeKind {
    Loop,
    Switch,
}

/// One enclosing breakable construct's backpatch targets. Loops and `switch`
/// bodies both catch `break`; only loops catch `continue`. An optional `label`
/// lets `break label;`/`continue label;` target a specific enclosing construct.
struct BreakScope {
    kind: ScopeKind,
    /// The source label naming this construct (`outer: for …`), if any.
    label: Option<String>,
    /// `continue` jump op indices, patched to the loop's continue target (the
    /// step/condition entry) once it is known. Backpatched — not read eagerly —
    /// because a `for` loop's continue target (the update clause) is only known
    /// after its body is lowered. Always empty for a `switch` scope.
    continue_ops: Vec<usize>,
    /// `break` jump op indices, patched to the construct's exit once known.
    break_ops: Vec<usize>,
}

impl BreakScope {
    fn loop_scope(label: Option<String>) -> Self {
        BreakScope {
            kind: ScopeKind::Loop,
            label,
            continue_ops: Vec::new(),
            break_ops: Vec::new(),
        }
    }
    fn switch_scope(label: Option<String>) -> Self {
        BreakScope {
            kind: ScopeKind::Switch,
            label,
            continue_ops: Vec::new(),
            break_ops: Vec::new(),
        }
    }
}

/// One enclosing `try` block's pending unwind jumps. A `throw` (or a post-call
/// check that finds an exception in flight) lowered *inside* the try body emits
/// a `Jump` and parks its index here; the jumps are patched to the handler once
/// the try body has been laid out. Popped before the `catch` bodies are lowered,
/// so a throw from a handler targets the *enclosing* try — Java's rule.
struct TryScope {
    unwind_ops: Vec<usize>,
}

/// One enclosing `finally` block of the *current frame*, innermost last.
///
/// A `return`/`break`/`continue` that leaves the guarded region has to run every
/// `finally` it jumps out of, innermost first, before taking the jump. javars
/// duplicates the block at each exit (as `javac` does since `jsr`/`ret` were
/// dropped), so the exit sites need the source back — hence the body is kept
/// here rather than a code address.
struct FinallyScope {
    body: Vec<Stmt>,
    /// `Compiler::scopes.len()` when the `try` was entered. A `break`/`continue`
    /// targeting scope index `i` must run exactly the finallys entered *inside*
    /// that construct — the ones whose depth is greater than `i`.
    scope_depth: usize,
}

struct Compiler {
    b: ChunkBuilder,
    /// The stack of enclosing breakable constructs (loops and `switch`es),
    /// innermost last. `break`/`continue` backpatch through it.
    scopes: Vec<BreakScope>,
    /// A pending source label consumed by the next loop/`switch` it prefixes
    /// (`outer: for …`). Set by [`StmtKind::Labeled`], taken when the construct
    /// pushes its [`BreakScope`].
    pending_label: Option<String>,
    /// Counter minting unique internal names for `switch` discriminant temps.
    switch_counter: u32,
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
    /// True when any user class supplies a `toString()` body. Only then does a
    /// concatenation operand the compiler could not type route through the
    /// VM-holding rendering builtin instead of fusevm's `Op::Add` — see
    /// [`Compiler::emit_host_stringified`].
    has_user_tostring: bool,
    /// Declared numeric types of `main`'s locals (the global/`main` scope,
    /// keyed by name). Method locals live in [`Compiler::scope`] instead.
    global_types: HashMap<String, NumType>,
    /// The active method scope while lowering a method body; `None` while
    /// lowering `main`. Selects slot-based vs. global variable access.
    scope: Option<MethodScope>,
    /// User-defined static-method overloads, keyed by name — populated before
    /// any body is lowered so calls (including forward and recursive ones)
    /// resolve. Multiple entries per name are overloads resolved by argument
    /// type at the call site.
    methods: HashMap<String, Vec<MethodSig>>,
    /// Declared type name of each `main`-scope local (parallel to
    /// [`Compiler::global_types`] but the raw type string, for class typing).
    global_decl_types: HashMap<String, String>,
    /// Resolved metadata (fields/methods/ctors, inheritance-flattened) for every
    /// user class. Keyed by class name; populated before any body is lowered.
    classes: HashMap<String, ClassInfo>,
    /// The class of `this` while lowering an instance method or constructor;
    /// `None` in `main` and in `static` methods.
    this_class: Option<String>,
    /// The class textually enclosing the code being lowered — the one an
    /// unqualified `static` field name resolves against. Unlike
    /// [`Compiler::this_class`] it is also set inside a `static` method (to the
    /// method's declaring class) and inside `main` (to the entry class).
    current_class: Option<String>,
    /// Counter minting unique internal temp names (`new`/compound-assign temps).
    temp_counter: u32,
    /// True when the program uses exceptions ([`Program::uses_exceptions`]).
    /// Only then does a call site carry the pending-exception check, so an
    /// exception-free program emits byte-identical bytecode to before.
    has_exceptions: bool,
    /// Enclosing `try` blocks of the *current frame*, innermost last. Empty in a
    /// frame with no active try, which makes an unwind a plain "return out of
    /// this frame and let the caller's check see it".
    tries: Vec<TryScope>,
    /// Enclosing `finally` blocks of the current frame, innermost last. A jump
    /// that leaves them (`return`/`break`/`continue`) emits their bodies first.
    finallys: Vec<FinallyScope>,
    /// Lambda bodies queued for emission as subroutines. A lambda literal emits
    /// only the closure construction at its site; the body is laid out with the
    /// other subroutines, after `main`, so control never falls into it. The queue
    /// grows while it drains (a lambda may contain a lambda).
    pending_lambdas: Vec<PendingLambda>,
    /// Counter minting unique lambda subroutine names (`#lambda#0`).
    lambda_counter: u32,
    /// The functional-interface type the expression being lowered is assigned
    /// to, when one is known — a local's declared type, a parameter's type, a
    /// method's return type. A lambda literal consumes it to type its own
    /// parameters from the interface's single abstract method, which is how
    /// `Calc c = x -> 100 / x;` gets Java's integral division inside the body.
    lambda_target: Option<String>,
    /// The declared return type of the method being lowered, so a `return
    /// <lambda>;` knows its target type.
    current_ret: Option<String>,
    /// `yield` jumps of the arrow-`switch` arm body being lowered, patched to
    /// the arm's exit once it is laid out.
    yield_ops: Vec<usize>,
    /// `finallys.len()` when the current arm body was entered — a `yield` runs
    /// exactly the cleanup blocks opened inside the arm, and no more.
    yield_finally_depth: usize,
}

/// A lambda body waiting to be emitted as a subroutine.
struct PendingLambda {
    /// Chunk name index of the body's subroutine — the value the closure stores
    /// and [`crate::host::JCLOSURE_CALL`] looks the entry address up by.
    name_idx: u16,
    /// The lambda's formal parameter names, bound to slots `0..n`.
    params: Vec<String>,
    /// The declared parameter types the target interface's single abstract
    /// method gives them, when the literal's context named a target type. Empty
    /// when it did not, in which case the parameters are statically untyped and
    /// arithmetic on them falls back to the runtime's own rules.
    param_tys: Vec<String>,
    /// The target interface's declared *return* type, when the literal's context
    /// named a target. It drives the assignment conversion on the body's result
    /// — a `double`-returning single abstract method widens an integral body.
    ret_ty: Option<String>,
    /// The enclosing locals captured by value, in push order, each with the
    /// declared type and numeric category it had in the enclosing scope (so
    /// `/`-truncation and class-typed dispatch keep working inside the body).
    /// Bound to the slots after the parameters.
    captures: Vec<Capture>,
    /// True when the enclosing frame had a `this` — captured as the last
    /// upvalue, so `this`, implicit field reads, and instance calls work inside
    /// a lambda written in an instance method or constructor.
    captures_this: bool,
    body: LambdaBody,
    /// The `this`/enclosing class the body lowers under (restored around the
    /// body, because it is emitted long after its literal site).
    this_class: Option<String>,
    current_class: Option<String>,
    line: u32,
}

/// One captured upvalue: the name it keeps inside the body, plus the static
/// typing it had outside.
struct Capture {
    name: String,
    decl_ty: Option<String>,
    num_ty: NumType,
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
    // Register every static-method signature up front so calls resolve
    // regardless of source order (forward references, recursion, mutual
    // recursion).
    let mut methods: HashMap<String, Vec<MethodSig>> = HashMap::new();
    for m in &prog.methods {
        methods.entry(m.name.clone()).or_default().push(MethodSig {
            owner: m.owner.clone(),
            param_tys: m.params.iter().map(|p| p.ty.clone()).collect(),
            ret: numtype_of_ty(&m.ret).unwrap_or(NumType::Other),
            ret_name: m.ret.clone(),
            varargs: is_varargs(&m.params),
        });
    }
    let classes = resolve_classes(prog)?;
    // One whole-program question, asked once: does any class supply a
    // `toString()` body? It gates the concatenation rerouting in
    // [`Compiler::emit_host_stringified`], so a program with none emits exactly
    // the bytecode it did before that path existed.
    let has_user_tostring = classes.values().any(|ci| {
        ci.methods
            .iter()
            .any(|m| m.name == "toString" && m.param_tys.is_empty() && !m.is_abstract)
    });
    let mut c = Compiler {
        b: ChunkBuilder::new(),
        scopes: Vec::new(),
        pending_label: None,
        switch_counter: 0,
        exit_ops: Vec::new(),
        debug,
        has_ffi,
        has_user_tostring,
        global_types: HashMap::new(),
        scope: None,
        methods,
        global_decl_types: HashMap::new(),
        classes,
        this_class: None,
        current_class: Some(prog.class_name.clone()),
        temp_counter: 0,
        has_exceptions: prog.uses_exceptions,
        tries: Vec::new(),
        finallys: Vec::new(),
        pending_lambdas: Vec::new(),
        lambda_counter: 0,
        lambda_target: None,
        current_ret: None,
        yield_ops: Vec::new(),
        yield_finally_depth: 0,
    };
    // ── main body (global scope) ──
    // Class-level state exists before any user code runs, in Java's order:
    // every `static` field takes its type's default value, enum constants are
    // constructed (they are the first statics of their type), then the static
    // initializers and `static { … }` blocks run in textual order.
    c.emit_static_defaults(prog);
    c.emit_enum_prologue(prog)?;
    c.emit_static_init(prog)?;
    // `main`'s `String[]` parameter is the real program arguments.
    c.bind_main_args(prog);
    for stmt in &prog.main {
        c.stmt(stmt)?;
    }
    // Patch any program-level `break`/`return;` to the position right after
    // main. When subroutines follow, that position holds the skip-over jump.
    // An exception that reached the top of `main` also lands here, so the
    // uncaught report is emitted at exactly that position.
    let end = c.b.current_pos();
    if c.has_exceptions {
        c.emit_uncaught_check();
    }
    let exit_ops = std::mem::take(&mut c.exit_ops);
    for op in exit_ops {
        c.b.patch_jump(op, end);
    }
    // ── subroutine bodies (static methods, then instance methods + ctors) ──
    // Emitted after `main` and jumped over so control never falls into them;
    // each is reached only via `Op::Call`.
    let has_subs = !prog.methods.is_empty()
        || !c.pending_lambdas.is_empty()
        || prog
            .classes
            .iter()
            .any(|cl| !cl.methods.is_empty() || !cl.ctors.is_empty());
    if has_subs {
        let skip = c.b.emit(Op::Jump(0), 0);
        for m in &prog.methods {
            c.compile_method(m)?;
        }
        for cl in &prog.classes {
            for m in &cl.methods {
                // Abstract methods (interface signatures, `abstract` class
                // methods) have no body — no subroutine is emitted. A concrete
                // implementor supplies the body reached by virtual dispatch.
                if m.is_abstract {
                    continue;
                }
                c.compile_instance_method(&cl.name, m)?;
            }
            for ctor in &cl.ctors {
                c.compile_ctor(cl, ctor)?;
            }
        }
        // Lambda bodies last, and by draining rather than iterating: emitting one
        // can queue another (a lambda that returns a lambda).
        while let Some(pl) = c.pending_lambdas.pop() {
            c.compile_lambda_body(pl)?;
        }
        let after = c.b.current_pos();
        c.b.patch_jump(skip, after);
    }
    Ok(c.b.build())
}

/// Resolve every class's metadata with inheritance flattened: fields listed
/// ancestors-first (Java initialization order), field types merged, and each
/// `(method, arity)` mapped to its most-derived defining class.
fn resolve_classes(prog: &Program) -> Result<HashMap<String, ClassInfo>, String> {
    let by_name: HashMap<&str, &Class> =
        prog.classes.iter().map(|c| (c.name.as_str(), c)).collect();
    // The ancestor chain of `name`, farthest ancestor first, `name` last.
    let chain = |name: &str| -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        let mut cur = Some(name.to_string());
        let mut guard = 0;
        while let Some(n) = cur {
            if let Some(cl) = by_name.get(n.as_str()) {
                out.push(n.clone());
                cur = cl.superclass.clone();
            } else {
                // An unknown superclass (e.g. a JDK class) terminates the chain.
                break;
            }
            guard += 1;
            if guard > 1000 {
                return Err(format!("javars: cyclic class hierarchy at `{name}`"));
            }
        }
        out.reverse();
        Ok(out)
    };
    // Collect a type's transitive interfaces, super-interfaces first (so a more
    // specific interface's `default` overrides a less specific one), deduped.
    fn collect_ifaces(
        name: &str,
        by_name: &HashMap<&str, &Class>,
        order: &mut Vec<String>,
        seen: &mut std::collections::HashSet<String>,
    ) {
        if !seen.insert(name.to_string()) {
            return;
        }
        if let Some(cl) = by_name.get(name) {
            for i in &cl.interfaces {
                collect_ifaces(i, by_name, order, seen);
            }
            if cl.is_interface {
                order.push(name.to_string());
            }
        }
    }
    let mut out = HashMap::new();
    for cl in &prog.classes {
        let ancestry = chain(&cl.name)?;
        // The interfaces this type (and its superclass chain) brings in, super-
        // interfaces first. `extends`-ed interfaces of an interface, and
        // `implements`-ed interfaces of every class in the ancestry.
        let mut iface_order = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for anc_name in &ancestry {
            let anc = by_name[anc_name.as_str()];
            for i in &anc.interfaces {
                collect_ifaces(i, &by_name, &mut iface_order, &mut seen);
            }
        }
        let mut fields = Vec::new();
        let mut field_types = HashMap::new();
        // `static` fields, ancestors first so a subclass re-declaration shadows
        // the inherited one (and interface constants are visible too).
        let mut static_fields = HashMap::new();
        for iface in &iface_order {
            for f in &by_name[iface.as_str()].static_fields {
                static_fields.insert(f.name.clone(), (iface.clone(), f.ty.clone()));
            }
        }
        for anc_name in &ancestry {
            for f in &by_name[anc_name.as_str()].static_fields {
                static_fields.insert(f.name.clone(), (anc_name.clone(), f.ty.clone()));
            }
        }
        // Keyed by (name, param_types) so type-overloads coexist while an
        // override (same name + same param types) replaces the inherited entry.
        let mut methods: HashMap<(String, Vec<String>), MethodMeta> = HashMap::new();
        let param_tys =
            |m: &Method| -> Vec<String> { m.params.iter().map(|p| p.ty.clone()).collect() };
        // 1. Interface methods (abstract + `default`), super-interfaces first —
        //    later (more specific) entries overwrite earlier ones.
        for iface in &iface_order {
            let icl = by_name[iface.as_str()];
            for m in &icl.methods {
                let ptys = param_tys(m);
                methods.insert(
                    (m.name.clone(), ptys.clone()),
                    MethodMeta {
                        name: m.name.clone(),
                        param_tys: ptys,
                        defining: iface.clone(),
                        ret: m.ret.clone(),
                        is_abstract: m.is_abstract,
                        varargs: is_varargs(&m.params),
                    },
                );
            }
        }
        // 2. Class-chain fields and methods (ancestors first) — a class method
        //    always overrides an inherited interface `default`.
        for anc_name in &ancestry {
            let anc = by_name[anc_name.as_str()];
            for f in &anc.fields {
                field_types.insert(f.name.clone(), f.ty.clone());
                fields.push(FieldInit {
                    name: f.name.clone(),
                    ty: f.ty.clone(),
                    init: f.init.clone(),
                });
            }
            for m in &anc.methods {
                let ptys = param_tys(m);
                methods.insert(
                    (m.name.clone(), ptys.clone()),
                    MethodMeta {
                        name: m.name.clone(),
                        param_tys: ptys,
                        defining: anc_name.clone(),
                        ret: m.ret.clone(),
                        is_abstract: m.is_abstract,
                        varargs: is_varargs(&m.params),
                    },
                );
            }
        }
        let methods: Vec<MethodMeta> = methods.into_values().collect();
        let ctors: Vec<CtorSig> = cl
            .ctors
            .iter()
            .map(|c| CtorSig {
                param_tys: c.params.iter().map(|p| p.ty.clone()).collect(),
                varargs: is_varargs(&c.params),
            })
            .collect();
        let mut supertypes = Vec::new();
        if let Some(sup) = &cl.superclass {
            supertypes.push(sup.clone());
        }
        supertypes.extend(cl.interfaces.iter().cloned());
        out.insert(
            cl.name.clone(),
            ClassInfo {
                superclass: cl.superclass.clone(),
                supertypes,
                is_interface: cl.is_interface,
                is_enum: cl.is_enum,
                enum_constants: cl.enum_constants.iter().map(|c| c.name.clone()).collect(),
                fields,
                field_types,
                static_fields,
                methods,
                ctors,
            },
        );
    }
    Ok(out)
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

    /// Emit a read of `this`. Slot 0 in an instance method or constructor; the
    /// slot holding the captured receiver inside a lambda body.
    fn emit_this(&mut self, line: u32) {
        let slot = self.scope.as_ref().and_then(|s| s.this_slot).unwrap_or(0);
        self.b.emit(Op::GetSlot(slot), line);
    }

    // ── enums ────────────────────────────────────────────────────────────
    //
    // An enum constant is a singleton created once, before any user code runs,
    // and named from every frame. fusevm's `GetVar`/`SetVar` address a flat
    // global table (frame slots are per-call), so each constant gets one global
    // — `#enum#Color#RED` — that both `main` and a method body read directly.
    // Everything after that is ordinary object machinery: the constants are
    // instances of a normal class, so `==`, `instanceof`, virtual dispatch, and
    // arrays of them all work with no further special cases.

    /// Read the global holding an enum constant, from any frame.
    fn emit_global_get(&mut self, name: &str, line: u32) {
        let idx = self.b.add_name(name);
        self.b.emit(Op::GetVar(idx), line);
    }

    /// Store the top of stack into a global, from any frame.
    fn emit_global_set(&mut self, name: &str, line: u32) {
        let idx = self.b.add_name(name);
        self.b.emit(Op::SetVar(idx), line);
    }

    /// Build every enum constant's instance and park it in its global. Emitted
    /// at the very top of `main`, before the program's first statement.
    fn emit_enum_prologue(&mut self, prog: &Program) -> Result<(), String> {
        for cl in &prog.classes {
            for (ordinal, constant) in cl.enum_constants.iter().enumerate() {
                let line = cl.line;
                // A constant with a body is an instance of its own synthetic
                // subclass, but it is the *enum's* constructor that runs — Java
                // gives an anonymous enum subclass no constructor of its own.
                let runtime_class = constant.body_class.as_deref().unwrap_or(&cl.name);
                self.new_object_as(runtime_class, &cl.name, &constant.args, line)?;
                let obj = self.temp();
                self.emit_set(&obj, line);
                // … then the identity every enum constant carries.
                for (field, value) in [
                    (ENUM_NAME, Expr::Str(constant.name.clone())),
                    (ENUM_ORDINAL, Expr::Int(ordinal as i64)),
                ] {
                    self.emit_get(&obj, line);
                    let name_c = self.b.add_constant(Value::str(field.to_string()));
                    self.b.emit(Op::LoadConst(name_c), line);
                    self.expr(&value)?;
                    self.emit_raising_builtin(crate::host::JFIELD_SET, 3, line);
                    self.b.emit(Op::Pop, line);
                }
                self.emit_get(&obj, line);
                self.emit_global_set(&enum_global(&cl.name, &constant.name), line);
            }
        }
        Ok(())
    }

    // ── `static` fields ──────────────────────────────────────────────────
    //
    // A `static` field is one cell shared by every instance and readable from
    // every frame, which is exactly what a fusevm global is (frame slots are
    // per-call). So each one gets a compiler-minted global — `#static#T#n` —
    // seeded with the field's default before any user code runs, and both the
    // qualified (`T.n`) and unqualified (`n`, inside the class) forms lower to
    // a read/write of that global.

    /// Seed every `static` field with its declared type's default value. Runs
    /// before any initializer so a field read during another class's static
    /// initialization sees `0`/`null` rather than nothing — Java's rule.
    fn emit_static_defaults(&mut self, prog: &Program) {
        for cl in &prog.classes {
            for f in &cl.static_fields {
                self.emit_type_default(&f.ty, cl.line);
                self.emit_global_set(&static_global(&cl.name, &f.name), cl.line);
            }
        }
    }

    /// Run each class's static initialization — its field initializers and
    /// `static { … }` blocks, in textual order — with the class in scope so an
    /// unqualified name resolves to its own statics.
    fn emit_static_init(&mut self, prog: &Program) -> Result<(), String> {
        for cl in &prog.classes {
            if cl.static_init.is_empty() {
                continue;
            }
            let saved = self.current_class.replace(cl.name.clone());
            let result = cl.static_init.iter().try_for_each(|s| self.stmt(s));
            self.current_class = saved;
            result?;
        }
        Ok(())
    }

    /// Bind `main`'s `String[]` parameter to the real program arguments.
    /// Nothing is emitted for a `main()` that declares no parameter.
    fn bind_main_args(&mut self, prog: &Program) {
        let Some(name) = &prog.main_param else {
            return;
        };
        let name = name.clone();
        self.declare_local(&name, "String[]", NumType::Other);
        self.b.emit(Op::CallBuiltin(crate::host::JARGV, 0), 0);
        self.emit_set(&name, 0);
    }

    /// The class declaring the `static` field an unqualified `name` refers to,
    /// with its declared type. `None` when `name` is a local/parameter (which
    /// shadows a field), or when the enclosing class has no such static.
    fn static_field_owner(&self, name: &str) -> Option<(String, String)> {
        if self.is_declared_var(name) {
            return None;
        }
        let cur = self.current_class.as_deref()?;
        self.classes.get(cur)?.static_fields.get(name).cloned()
    }

    /// When `e` is `ClassName.field` naming a `static` field, its declaring
    /// class and declared type — the `Expr::Field` shape whose receiver is a
    /// *type* rather than a value. A declared variable of the same name always
    /// wins, exactly as it does for an enum constant.
    fn static_field_ref(&self, e: &Expr) -> Option<(String, String)> {
        let Expr::Field { recv, name } = e else {
            return None;
        };
        let Expr::Var(class) = recv.as_ref() else {
            return None;
        };
        if self.is_declared_var(class) {
            return None;
        }
        self.classes.get(class)?.static_fields.get(name).cloned()
    }

    /// The declaring class + declared type of whichever `static` field an
    /// assignment target names — an unqualified `n` or a qualified `T.n`.
    fn static_target(&self, recv: &Expr, name: &str) -> Option<(String, String)> {
        match recv {
            Expr::This => self.static_field_owner(name),
            _ => self.static_field_ref(&Expr::Field {
                recv: Box::new(recv.clone()),
                name: name.to_string(),
            }),
        }
    }

    /// Lower `<static field> <op>= value` — a read/modify/write of the field's
    /// global. `=` writes straight through.
    fn static_assign(
        &mut self,
        class: &str,
        ty: &str,
        name: &str,
        op: AssignOp,
        value: &Expr,
        line: u32,
    ) -> Result<(), String> {
        let global = static_global(class, name);
        if op == AssignOp::Assign {
            let ty = ty.to_string();
            self.expr_targeted(value, Some(&ty))?;
            self.emit_global_set(&global, line);
            return Ok(());
        }
        let target = numtype_of_ty(ty).unwrap_or(NumType::Other);
        let wrap = self.compound_wraps(Some(ty), value);
        self.emit_global_get(&global, line);
        self.emit_compound(op, value, target, Some(ty), wrap, line)?;
        self.emit_narrow_to(Some(ty), line);
        self.emit_global_set(&global, line);
        Ok(())
    }

    /// When `e` is `EnumName.CONSTANT`, the enum's name — the one `Expr::Field`
    /// shape that is a *type* reference rather than an instance field read.
    /// `None` for every other expression, including a field of a variable that
    /// happens to share an enum's name (a declared variable always wins).
    fn enum_constant_ref(&self, e: &Expr) -> Option<String> {
        let Expr::Field { recv, name } = e else {
            return None;
        };
        let Expr::Var(class) = recv.as_ref() else {
            return None;
        };
        if self.is_declared_var(class) {
            return None;
        }
        let info = self.classes.get(class)?;
        info.enum_constants
            .iter()
            .any(|c| c == name)
            .then(|| class.clone())
    }

    /// A bare `MUL` written inside `enum Op`'s own body names its constant —
    /// Java allows the unqualified form only there and in a `switch` label.
    /// A local of the same name shadows it, exactly as it shadows a field.
    fn enclosing_enum_constant(&self, name: &str) -> Option<String> {
        if self.is_local(name) {
            return None;
        }
        let this = self.this_class.as_deref()?;
        let info = self.classes.get(this)?;
        info.enum_constants
            .iter()
            .any(|c| c == name)
            .then(|| this.to_string())
    }

    /// The type of `EnumName.values()` / `EnumName.valueOf(s)` — the two enum
    /// statics the compiler generates, which have no entry in any method table.
    fn enum_static_type(&self, recv: &Expr, method: &str, argc: usize) -> Option<String> {
        let class = self.enum_type_ref(recv)?;
        match (method, argc) {
            ("values", 0) => Some(format!("{class}[]")),
            ("valueOf", 1) => Some(class),
            _ => None,
        }
    }

    /// The user class named by a bare receiver (`Counter.reset()`), or `None`
    /// when the receiver is a value rather than a type name. A declared variable
    /// of the same name always wins.
    fn user_class_ref(&self, recv: &Expr) -> Option<String> {
        let Expr::Var(name) = recv else {
            return None;
        };
        if self.is_declared_var(name) || self.bare_var_type(name).is_some() {
            return None;
        }
        self.classes.contains_key(name).then(|| name.clone())
    }

    /// The enum type named by a bare receiver (`Color.values()`), or `None` when
    /// the receiver is a value rather than an enum type name.
    fn enum_type_ref(&self, recv: &Expr) -> Option<String> {
        let Expr::Var(class) = recv else {
            return None;
        };
        if self.is_declared_var(class) {
            return None;
        }
        let info = self.classes.get(class)?;
        info.is_enum.then(|| class.clone())
    }

    /// Lower `EnumName.values()` — a fresh array of the constants, in
    /// declaration order, exactly as Java hands out a fresh copy each call.
    fn emit_enum_values(&mut self, class: &str, line: u32) {
        let constants = self.classes[class].enum_constants.clone();
        for c in &constants {
            self.emit_global_get(&enum_global(class, c), line);
        }
        self.b.emit(
            Op::CallBuiltin(crate::host::JARRAY_LIT, constants.len() as u8),
            line,
        );
    }

    /// Lower `EnumName.valueOf(s)` — a name-to-constant lookup, raising Java's
    /// `IllegalArgumentException: No enum constant Color.PINK` on a miss.
    fn emit_enum_value_of(&mut self, class: &str, arg: &Expr, line: u32) -> Result<(), String> {
        let constants = self.classes[class].enum_constants.clone();
        let key = self.temp();
        self.expr(arg)?;
        self.emit_set(&key, line);
        let mut done = Vec::new();
        for c in &constants {
            self.emit_get(&key, line);
            let cc = self.b.add_constant(Value::str(c.clone()));
            self.b.emit(Op::LoadConst(cc), line);
            self.b.emit(Op::StrEq, line);
            let jf = self.b.emit(Op::JumpIfFalse(0), line);
            self.emit_global_get(&enum_global(class, c), line);
            done.push(self.b.emit(Op::Jump(0), line));
            let next = self.b.current_pos();
            self.b.patch_jump(jf, next);
        }
        // No match. The message needs the runtime argument, so it is built with
        // a concatenation rather than a constant.
        let msg = self.temp();
        let prefix = self
            .b
            .add_constant(Value::str(format!("No enum constant {class}.")));
        self.b.emit(Op::LoadConst(prefix), line);
        self.emit_get(&key, line);
        self.b.emit(Op::Add, line);
        self.emit_set(&msg, line);
        let cls = self.b.add_constant(Value::str("IllegalArgumentException"));
        self.b.emit(Op::LoadConst(cls), line);
        self.emit_get(&msg, line);
        self.b.emit(Op::CallBuiltin(crate::host::JFAULT, 2), line);
        self.b.emit(Op::Pop, line);
        self.emit_unwind(line);
        // The unwind never falls through, but the jumps from a match do.
        let end = self.b.current_pos();
        for j in done {
            self.b.patch_jump(j, end);
        }
        Ok(())
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

    /// Declare a local/parameter with its raw declared type: records the numeric
    /// category (for `/` typing), the raw type string (for class-typed dispatch),
    /// and marks the name as a true local (so it shadows any same-named field).
    fn declare_local(&mut self, name: &str, ty: &str, nt: NumType) {
        self.declare_type(name, nt);
        match &mut self.scope {
            Some(scope) => {
                scope.decl_types.insert(name.to_string(), ty.to_string());
                scope.declared.insert(name.to_string());
            }
            None => {
                self.global_decl_types
                    .insert(name.to_string(), ty.to_string());
            }
        }
    }

    /// True when `name` resolves to a real local/parameter in the active scope
    /// (rather than an implicit `this.field`). In `main`, every bare name is a
    /// global "local".
    fn is_local(&self, name: &str) -> bool {
        match &self.scope {
            Some(scope) => scope.declared.contains(name),
            None => true,
        }
    }

    /// True when `name` was actually declared as a variable in the active scope
    /// (a real local/parameter, or a `main` global). Distinguishes a variable
    /// receiver from a bare stdlib class name like `Math`.
    fn is_declared_var(&self, name: &str) -> bool {
        match &self.scope {
            Some(scope) => scope.declared.contains(name),
            None => {
                self.global_types.contains_key(name) || self.global_decl_types.contains_key(name)
            }
        }
    }

    /// If `name` is not a local but is a field of the enclosing `this` class,
    /// return that class — the receiver for an implicit `this.name` access.
    fn implicit_this_field(&self, name: &str) -> Option<String> {
        if self.is_local(name) {
            return None;
        }
        let this = self.this_class.as_deref()?;
        let info = self.classes.get(this)?;
        info.field_types
            .contains_key(name)
            .then(|| this.to_string())
    }

    /// The declared type string of a variable in the active scope, if known.
    fn var_decl_type(&self, name: &str) -> Option<&str> {
        match &self.scope {
            Some(scope) => scope.decl_types.get(name).map(|s| s.as_str()),
            None => self.global_decl_types.get(name).map(|s| s.as_str()),
        }
    }

    /// The declared array-type string of an expression (`int[]`, `Shape[]`), if
    /// statically known — a local/param variable or an instance field.
    fn expr_array_type(&self, e: &Expr) -> Option<String> {
        match e {
            Expr::Var(name) => {
                let ty = self.bare_var_type(name)?;
                ty.ends_with("[]").then_some(ty)
            }
            Expr::Field { recv, name } => {
                if let Some((_, ty)) = self.static_field_ref(e) {
                    return ty.ends_with("[]").then_some(ty);
                }
                let rc = self.expr_class(recv)?;
                let ft = self.classes.get(&rc)?.field_types.get(name)?;
                ft.ends_with("[]").then(|| ft.clone())
            }
            // `Color.values()` hands back a `Color[]`.
            Expr::MethodCall {
                recv, method, args, ..
            } if method == "values" && args.is_empty() => {
                self.enum_type_ref(recv).map(|c| format!("{c}[]"))
            }
            // A row of a multi-dimensional array: `int[][]` indexed once is an
            // `int[]`, so `g[i][j]` types its element as `int`.
            Expr::Index { array, .. } => {
                let outer = self.expr_array_type(array)?;
                let inner = outer.strip_suffix("[]")?;
                inner.ends_with("[]").then(|| inner.to_string())
            }
            _ => None,
        }
    }

    /// The user-class name an expression statically evaluates to (for instance
    /// method/field dispatch), or `None` when it is not a known class type.
    fn expr_class(&self, e: &Expr) -> Option<String> {
        match e {
            Expr::This => self.this_class.clone(),
            Expr::Var(name) => {
                let ty = self.bare_var_type(name)?;
                self.classes.contains_key(&ty).then_some(ty)
            }
            Expr::Field { recv, name } => {
                if let Some(class) = self.enum_constant_ref(e) {
                    return Some(class);
                }
                if let Some((_, ty)) = self.static_field_ref(e) {
                    return self.classes.contains_key(&ty).then_some(ty);
                }
                let rc = self.expr_class(recv)?;
                let ty = self.classes.get(&rc)?.field_types.get(name)?;
                self.classes.contains_key(ty).then(|| ty.to_string())
            }
            // Only a *user* class, on the rule every other arm here follows:
            // `new ArrayList<>()` and `new Object()` are host values rather than
            // class instances with a method table, so naming their type here
            // would route `new ArrayList<>().size()` into user-class dispatch and
            // reject it. (`Compiler::expr_java_type` still reports the type.)
            Expr::NewObject { class, .. } => {
                self.classes.contains_key(class).then(|| class.clone())
            }
            // A cast states the class outright, which is how `((Animal) x)`
            // reaches `Animal`'s methods and how `println((Dog) a)` finds the
            // `toString` override.
            Expr::Cast { ty, expr, .. } => self
                .classes
                .contains_key(ty)
                .then(|| ty.clone())
                // A cast to a type javars does not model as a class (`Object`)
                // does not erase what the operand is, so `println((Object) dog)`
                // still finds `Dog`'s `toString`.
                .or_else(|| self.expr_class(expr)),
            // An element of a class-typed array (`Shape[] → Shape`).
            Expr::Index { array, .. } => {
                let arr_ty = self.expr_array_type(array)?;
                let elem = arr_ty.strip_suffix("[]")?;
                self.classes.contains_key(elem).then(|| elem.to_string())
            }
            // A bare call to a user `static` method: its declared return type.
            Expr::Call { name, args, .. } => {
                let arg_tys: Vec<Option<String>> =
                    args.iter().map(|a| self.expr_java_type(a)).collect();
                let ret = self.resolve_static_call(name, &arg_tys)?.ret_name;
                self.classes.contains_key(&ret).then_some(ret)
            }
            Expr::MethodCall {
                recv, method, args, ..
            } => {
                if let Some(t) = self.enum_static_type(recv, method, args.len()) {
                    return self.classes.contains_key(&t).then_some(t);
                }
                if let Some(class) = self.user_class_ref(recv) {
                    let arg_tys: Vec<Option<String>> =
                        args.iter().map(|a| self.expr_java_type(a)).collect();
                    let ret = self.resolve_static_on(&class, method, &arg_tys)?.ret_name;
                    return self.classes.contains_key(&ret).then_some(ret);
                }
                let rc = self.expr_class(recv)?;
                let arg_tys: Vec<Option<String>> =
                    args.iter().map(|a| self.expr_java_type(a)).collect();
                let ret_name = self.resolve_instance_call(&rc, method, &arg_tys)?.ret_name;
                self.classes.contains_key(&ret_name).then_some(ret_name)
            }
            _ => None,
        }
    }

    /// The direct superclass of the class whose body is being compiled — the
    /// type `super` reads at. `None` outside an instance member, and for a class
    /// whose parent is the implicit `java.lang.Object` (which javars keeps out
    /// of the class table on purpose; see `new_object_as`).
    fn super_class(&self) -> Option<String> {
        let this = self.this_class.as_deref()?;
        self.classes.get(this)?.superclass.clone()
    }

    /// The declared type-name of a bare variable read `name`: a local/param/
    /// global declared type, else an instance field of the enclosing `this`.
    fn bare_var_type(&self, name: &str) -> Option<String> {
        // `super` and `this` are the same object; only the *static* type they
        // are read at differs. Typing it as the superclass is what resolves
        // `super.field` and the result type of `super.m()` one level up — and
        // `super` cannot be shadowed, because it is a Java reserved word.
        if name == SUPER {
            return self.super_class();
        }
        if let Some(t) = self.var_decl_type(name) {
            return Some(t.to_string());
        }
        if let Some(class) = self.enclosing_enum_constant(name) {
            return Some(class);
        }
        if let Some(this) = self.this_class.as_ref() {
            if let Some(t) = self
                .classes
                .get(this)
                .and_then(|ci| ci.field_types.get(name))
            {
                return Some(t.clone());
            }
        }
        // An unqualified `static` field of the enclosing class.
        self.static_field_owner(name).map(|(_, ty)| ty)
    }

    /// The static Java type-name of an expression (`int`, `double`, `boolean`,
    /// `String`, a class/interface name, an array type, `null`), or `None` when
    /// it cannot be determined statically. Drives overload resolution by argument
    /// type; an unknown type falls back to arity-only matching at the call site.
    fn expr_java_type(&self, e: &Expr) -> Option<String> {
        match e {
            Expr::Int(_) => Some("int".to_string()),
            Expr::Long(_) => Some("long".to_string()),
            Expr::Float(_) => Some("double".to_string()),
            Expr::Float32(_) => Some("float".to_string()),
            Expr::Bool(_) => Some("boolean".to_string()),
            Expr::Str(_) => Some("String".to_string()),
            Expr::Char(_) => Some("char".to_string()),
            Expr::This => self.this_class.clone(),
            Expr::Var(name) => self.bare_var_type(name),
            Expr::Unary { op, rhs } => match op {
                // Unary numeric promotion (JLS 5.6.1): `-x` and `~x` widen
                // `byte`/`short`/`char` to `int` and leave every other type
                // alone — a `double` operand stays `double`.
                UnOp::Neg | UnOp::BitNot => match self.expr_java_type(rhs).as_deref() {
                    Some("byte" | "short" | "char") => Some("int".to_string()),
                    other => other.map(|s| s.to_string()),
                },
                UnOp::Not => Some("boolean".to_string()),
            },
            Expr::Cast { ty, .. } => Some(ty.clone()),
            Expr::Binary { op, lhs, rhs } => match op {
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
                    let l = self.expr_java_type(lhs);
                    let r = self.expr_java_type(rhs);
                    // `+` with a String operand is concatenation.
                    if matches!(op, BinOp::Add)
                        && (l.as_deref() == Some("String") || r.as_deref() == Some("String"))
                    {
                        return Some("String".to_string());
                    }
                    let lr = l.as_deref().and_then(numeric_rank)?;
                    let rr = r.as_deref().and_then(numeric_rank)?;
                    Some(rank_name(lr.max(rr)).to_string())
                }
                // `&`/`|`/`^` are bitwise on integral operands and logical on
                // booleans; the operand type decides which.
                BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
                    let l = self.expr_java_type(lhs);
                    let r = self.expr_java_type(rhs);
                    if l.as_deref() == Some("boolean") || r.as_deref() == Some("boolean") {
                        return Some("boolean".to_string());
                    }
                    let lr = l.as_deref().and_then(numeric_rank)?;
                    let rr = r.as_deref().and_then(numeric_rank)?;
                    Some(rank_name(lr.max(rr)).to_string())
                }
                // A shift promotes only its *left* operand — `1L << 2` is a
                // `long`, but `1 << 2L` is still an `int`.
                BinOp::Shl | BinOp::Shr | BinOp::Ushr => {
                    match self.expr_java_type(lhs).as_deref() {
                        Some("long") => Some("long".to_string()),
                        Some(t) if numeric_rank(t).is_some() => Some("int".to_string()),
                        _ => None,
                    }
                }
                _ => Some("boolean".to_string()),
            },
            Expr::Ternary { then, els, .. } => {
                let t = self.expr_java_type(then);
                let e2 = self.expr_java_type(els);
                if t == e2 {
                    return t;
                }
                // JLS 15.25: `char` paired with an `int` *constant* that fits in
                // a `char` keeps the conditional's type at `char`, so
                // `flag ? 'a' : 98` prints `a`/`b` rather than 97/98. A
                // non-constant `int` promotes as usual.
                let fits_char = |e: &Expr| matches!(e, Expr::Int(n) if (0..=0xFFFF).contains(n));
                if (t.as_deref() == Some("char") && e2.as_deref() == Some("int") && fits_char(els))
                    || (e2.as_deref() == Some("char")
                        && t.as_deref() == Some("int")
                        && fits_char(then))
                {
                    return Some("char".to_string());
                }
                let tr = t.as_deref().and_then(numeric_rank)?;
                let er = e2.as_deref().and_then(numeric_rank)?;
                Some(rank_name(tr.max(er)).to_string())
            }
            Expr::Field { recv, name } => {
                if name == "length" {
                    return Some("int".to_string());
                }
                if let Some((_, ty)) = self.wrapper_constant_ref(e) {
                    return Some(ty.to_string());
                }
                if let Some(class) = self.enum_constant_ref(e) {
                    return Some(class);
                }
                if let Some((_, ty)) = self.static_field_ref(e) {
                    return Some(ty);
                }
                let rc = self.expr_class(recv)?;
                self.classes.get(&rc)?.field_types.get(name).cloned()
            }
            Expr::Index { array, .. } => {
                let arr_ty = self.expr_array_type(array)?;
                arr_ty.strip_suffix("[]").map(|s| s.to_string())
            }
            Expr::NewObject { class, .. } => Some(class.clone()),
            Expr::NewArray {
                elem_ty,
                sizes,
                extra_dims,
            } => Some(format!(
                "{elem_ty}{}",
                "[]".repeat(sizes.len() + extra_dims)
            )),
            // `new int[]{…}` named its element type; a bare `{…}` did not, and
            // takes its type from the declaration it initializes instead.
            Expr::ArrayLit { elem_ty, .. } => elem_ty.as_ref().map(|t| format!("{t}[]")),
            Expr::InstanceOf { .. } => Some("boolean".to_string()),
            Expr::SwitchExpr { arms, .. } => arms.iter().find_map(|a| match &a.body {
                SwitchArmBody::Expr(e) => self.expr_java_type(e),
                SwitchArmBody::Block(_) => None,
            }),
            Expr::PostIncDec { name, .. } | Expr::PreIncDec { name, .. } => {
                self.bare_var_type(name)
            }
            Expr::Call { name, args, .. } => {
                let arg_tys: Vec<Option<String>> =
                    args.iter().map(|a| self.expr_java_type(a)).collect();
                self.resolve_static_call(name, &arg_tys).map(|s| s.ret_name)
            }
            Expr::MethodCall {
                recv, method, args, ..
            } => {
                if let Some(t) = self.enum_static_type(recv, method, args.len()) {
                    return Some(t);
                }
                // `T.helper(x)` — the named class's `static` method, resolved in
                // that class's own inheritance chain.
                if let Some(class) = self.user_class_ref(recv) {
                    let arg_tys: Vec<Option<String>> =
                        args.iter().map(|a| self.expr_java_type(a)).collect();
                    return self
                        .resolve_static_on(&class, method, &arg_tys)
                        .map(|s| s.ret_name);
                }
                // A bare stdlib class receiver (`Integer.parseInt`) is a static
                // call whose declared return type is known.
                if let Expr::Var(class) = recv.as_ref() {
                    if is_static_class(class) && !self.is_declared_var(class) {
                        // `Arrays.copyOf`/`copyOfRange` return an array of the
                        // *source's* element type, which is what keeps a
                        // `char[]` copy rendering as characters.
                        if class == "Arrays"
                            && matches!(method.as_str(), "copyOf" | "copyOfRange")
                            && !args.is_empty()
                        {
                            return self.expr_java_type(&args[0]);
                        }
                        if let Some(t) = static_call_java_type(class, method) {
                            return Some(t.to_string());
                        }
                        // `Math.abs`/`max`/`min` are overloaded on every numeric
                        // type and return the promotion of their arguments —
                        // `abs(int)` is an `int` (and so wraps), `abs(long)` is
                        // not. Unknown argument types leave the result unknown.
                        if class == "Math" && matches!(method.as_str(), "abs" | "max" | "min") {
                            let ranks: Option<Vec<u32>> = args
                                .iter()
                                .map(|a| self.expr_java_type(a).as_deref().and_then(numeric_rank))
                                .collect();
                            return ranks
                                .map(|r| rank_name(r.into_iter().max().unwrap_or(3)).to_string());
                        }
                        return None;
                    }
                }
                let arg_tys: Vec<Option<String>> =
                    args.iter().map(|a| self.expr_java_type(a)).collect();
                if let Some(rc) = self.expr_class(recv) {
                    if let Some(r) = self.resolve_instance_call(&rc, method, &arg_tys) {
                        return Some(r.ret_name);
                    }
                }
                // A collection receiver's known return types.
                if let Some(kind) = self
                    .expr_java_type(recv)
                    .as_deref()
                    .and_then(collection_kind)
                {
                    if let Some(t) = collection_call_java_type(kind, method, args.len()) {
                        return Some(t.to_string());
                    }
                    return None;
                }
                // A `String` receiver's known return types.
                match (method.as_str(), args.len()) {
                    ("length", 0)
                    | ("indexOf", 1)
                    | ("compareTo", 1)
                    | ("compareToIgnoreCase", 1) => Some("int".to_string()),
                    ("isEmpty", 0)
                    | ("contains", 1)
                    | ("equals", 1)
                    | ("startsWith", 1)
                    | ("endsWith", 1)
                    | ("equalsIgnoreCase", 1) => Some("boolean".to_string()),
                    ("substring", _)
                    | ("toUpperCase", 0)
                    | ("toLowerCase", 0)
                    | ("trim", 0)
                    | ("concat", 1)
                    | ("replace", 2)
                    | ("repeat", 1) => Some("String".to_string()),
                    ("charAt", 1) => Some("char".to_string()),
                    ("toCharArray", 0) => Some("char[]".to_string()),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Look up the declared numeric type of `name`, defaulting to `Other` when
    /// unknown (an undeclared read, or a type javars does not track). A name
    /// that is not a local falls back to whatever the enclosing class makes it —
    /// an instance field of `this` or a `static` field — so a bare `int` field
    /// truncates its division the same way an `int` local does.
    fn lookup_type(&self, name: &str) -> NumType {
        let map = match &self.scope {
            Some(scope) => &scope.types,
            None => &self.global_types,
        };
        if let Some(nt) = map.get(name) {
            return *nt;
        }
        self.bare_var_type(name)
            .as_deref()
            .and_then(numtype_of_ty)
            .unwrap_or(NumType::Other)
    }

    /// Mint a unique internal temp name (`#t0`, `#t1`, …). `#` is not a legal
    /// Java identifier char, so these never collide with user variables.
    fn temp(&mut self) -> String {
        let t = format!("#t{}", self.temp_counter);
        self.temp_counter += 1;
        t
    }

    /// The conversion cost from static type `from` to a parameter type `to`, or
    /// `None` when `from` is not assignable to `to`. `0` is an identity match;
    /// numeric widening costs the rank distance; a reference upcast costs its
    /// subtype distance. Lower cost = more specific (drives overload resolution).
    fn assign_cost(&self, from: &str, to: &str) -> Option<u32> {
        if from == to {
            return Some(0);
        }
        if let (Some(f), Some(t)) = (numeric_rank(from), numeric_rank(to)) {
            return (f <= t).then(|| t - f);
        }
        if from == "null" {
            return is_reference_type(to).then_some(50);
        }
        if let Some(d) = self.subtype_distance(from, to) {
            return Some(d);
        }
        if from == "String" && matches!(to, "Object" | "CharSequence") {
            return Some(1);
        }
        if to == "Object" && is_reference_type(from) {
            return Some(40);
        }
        // Boxing — Java's *second* resolution phase. A primitive converts to its
        // wrapper and then widens like any other reference, which is what makes
        // `f(1)` reach an `f(Object)` (and `h(1, 2)` reach an `h(Object... xs)`)
        // at all. Priced above every widening conversion so a phase-1 match
        // always wins: `f(double)` still beats `f(Object)` for `f(1)`.
        const BOX: u32 = 60;
        if let Some(w) = wrapper_of(from) {
            if w == to {
                return Some(BOX);
            }
            if let Some(d) = self.subtype_distance(w, to) {
                return Some(BOX + d);
            }
            let numeric = numeric_rank(from).is_some();
            if to == "Object" || to == "Comparable" || (to == "Number" && numeric) {
                return Some(BOX + 1);
            }
        }
        None
    }

    /// The number of supertype hops from `from` up to `to` (0 when equal), or
    /// `None` when `to` is not a supertype of `from`. Walks superclass +
    /// interface edges breadth-first for the shortest path.
    fn subtype_distance(&self, from: &str, to: &str) -> Option<u32> {
        if from == to {
            return Some(0);
        }
        let mut frontier = vec![from.to_string()];
        let mut seen = std::collections::HashSet::new();
        let mut dist = 0u32;
        while !frontier.is_empty() {
            dist += 1;
            let mut next = Vec::new();
            for cur in &frontier {
                if let Some(ci) = self.classes.get(cur) {
                    for sup in &ci.supertypes {
                        if sup == to {
                            return Some(dist);
                        }
                        if seen.insert(sup.clone()) {
                            next.push(sup.clone());
                        }
                    }
                }
            }
            frontier = next;
            if dist > 10000 {
                return None;
            }
        }
        None
    }

    /// Choose the most-specific applicable overload for argument types `arg_tys`
    /// among candidates whose parameter-type lists are `cands`. Returns the index
    /// into `cands`, or an error naming the failure (none applicable / ambiguous).
    /// An argument whose static type is unknown (`None`) matches any parameter
    /// without contributing to specificity.
    fn pick_overload(
        &self,
        cands: &[(&[String], bool)],
        arg_tys: &[Option<String>],
    ) -> Result<usize, NoPick> {
        // javars does not type-check (see BUGS.md), so a lone *fixed-arity*
        // candidate of the right arity is taken whether or not its parameters
        // accept the arguments — a `javac` type error may still run. A lone
        // *variable-arity* candidate is not the same question: whether its
        // declared `T[]` accepts the argument is precisely what separates
        // `sum(intArray)` (passed straight through) from `sum(1)` (packed), so
        // there applicability has to be checked.
        if matches!(cands, [(_, false)]) {
            return Ok(0);
        }
        let scored = cands
            .iter()
            .enumerate()
            .filter_map(|(i, (ptys, _))| self.signature_cost(ptys, arg_tys).map(|c| (i, c)));
        Self::most_specific(scored)
    }

    /// Java's three resolution phases over one candidate set, in order: the
    /// fixed-arity ones first, and the variable-arity one **only** when they
    /// find nothing applicable. Returns the winning candidate's index and, for
    /// a variable-arity win, the argument index where call-site packing starts.
    ///
    /// This is the single entry point for all three kinds of user-declared
    /// callable — `static` methods, instance methods, and constructors — so a
    /// `T...` declaration means the same thing wherever it sits.
    fn resolve_overload(
        &self,
        cands: &[(&[String], bool)],
        arg_tys: &[Option<String>],
    ) -> Option<(usize, Option<usize>)> {
        let of_arity: Vec<usize> = (0..cands.len())
            .filter(|&i| cands[i].0.len() == arg_tys.len())
            .collect();
        let fixed: Vec<(&[String], bool)> = of_arity.iter().map(|&i| cands[i]).collect();
        match self.pick_overload(&fixed, arg_tys) {
            Ok(i) => Some((of_arity[i], None)),
            Err(NoPick::Ambiguous) => None,
            Err(NoPick::Inapplicable) => {
                let (i, from) = self.pick_varargs(cands, arg_tys)?;
                Some((i, Some(from)))
            }
        }
    }

    /// The total conversion cost of passing `arg_tys` to a fixed-arity
    /// parameter list, or `None` when some argument is not assignable. An
    /// argument whose static type is unknown matches any parameter without
    /// contributing to specificity.
    fn signature_cost(&self, ptys: &[String], arg_tys: &[Option<String>]) -> Option<u32> {
        let mut total = 0;
        for (p, a) in ptys.iter().zip(arg_tys) {
            if let Some(at) = a {
                total += self.assign_cost(at, p)?;
            }
        }
        Some(total)
    }

    /// The single cheapest candidate out of `(index, cost)` pairs — Java's
    /// most-specific rule, approximated by conversion cost. A tie is
    /// [`NoPick::Ambiguous`]; an empty set is [`NoPick::Inapplicable`].
    fn most_specific(scored: impl Iterator<Item = (usize, u32)>) -> Result<usize, NoPick> {
        let mut best: Option<(usize, u32)> = None;
        let mut tie = false;
        for (i, cost) in scored {
            match best {
                Some((_, bc)) if cost > bc => {}
                Some((_, bc)) if cost == bc => tie = true,
                _ => {
                    best = Some((i, cost));
                    tie = false;
                }
            }
        }
        match best {
            Some((i, _)) if !tie => Ok(i),
            Some(_) => Err(NoPick::Ambiguous),
            None => Err(NoPick::Inapplicable),
        }
    }

    /// Java's **third** resolution phase (JLS 15.12.2.4), run only after both
    /// fixed-arity phases report [`NoPick::Inapplicable`].
    ///
    /// A candidate declared `(P0 … Pk-1, T[]…)` is applicable to `n ≥ k`
    /// arguments when each of the first `k` is assignable to its own parameter
    /// and every remaining one is assignable to the *component* type `T`. The
    /// winner's trailing arguments are packed into a `T[]` at the call site.
    ///
    /// Ordering matters and is not an implementation detail: because this phase
    /// never runs while a fixed-arity candidate applies, `f(int,int)` beats
    /// `f(int...)` for `f(1,2)`, and `sum(new int[]{1,2})` against
    /// `sum(int... xs)` is a *fixed-arity* match on the declared `int[]` — so
    /// the array passes straight through instead of being wrapped, which is
    /// exactly what Java does (and why `f(null)` against `f(Object... xs)`
    /// passes `null` as the whole array rather than an array holding `null`).
    ///
    /// Among applicable variable-arity candidates the cheaper conversion wins.
    /// A tie is settled by JLS 15.12.2.5 specificity over the *expanded*
    /// parameter lists ([`Compiler::at_least_as_specific`]) — which is what
    /// picks `h(String...)` over `h(Object...)` for the zero-argument `h()`,
    /// where both convert at no cost. Mutually-specific candidates are
    /// genuinely ambiguous and select nothing, exactly as `javac` reports for
    /// `g(int, int...)` against `g(int...)`.
    ///
    /// Returns the winning index and the argument index packing starts at.
    fn pick_varargs(
        &self,
        cands: &[(&[String], bool)],
        arg_tys: &[Option<String>],
    ) -> Option<(usize, usize)> {
        let n = arg_tys.len();
        let applicable: Vec<(usize, u32)> = cands
            .iter()
            .enumerate()
            .filter_map(|(i, (ptys, varargs))| {
                if !varargs || ptys.is_empty() || n + 1 < ptys.len() {
                    return None;
                }
                let fixed = ptys.len() - 1;
                let elem = ptys[fixed].strip_suffix("[]")?;
                let mut total = self.signature_cost(&ptys[..fixed], &arg_tys[..fixed])?;
                for a in arg_tys[fixed..].iter().flatten() {
                    total += self.assign_cost(a, elem)?;
                }
                Some((i, total))
            })
            .collect();
        let best = applicable.iter().map(|&(_, c)| c).min()?;
        let tied: Vec<usize> = applicable
            .iter()
            .filter(|&&(_, c)| c == best)
            .map(|&(i, _)| i)
            .collect();
        let mut winners = tied.iter().filter(|&&i| {
            tied.iter()
                .all(|&j| self.at_least_as_specific(cands[i].0, cands[j].0, n))
        });
        let idx = *winners.next()?;
        winners
            .next()
            .is_none()
            .then(|| (idx, cands[idx].0.len() - 1))
    }

    /// True when every expanded parameter of `a` is assignable to `b`'s — JLS
    /// 15.12.2.5's "at least as specific", compared over
    /// `max(n, arity(a), arity(b))` positions. Comparing at the *declared*
    /// arity too is what separates `String...` from `Object...` when the call
    /// passes no arguments at all.
    fn at_least_as_specific(&self, a: &[String], b: &[String], n: usize) -> bool {
        let k = n.max(a.len()).max(b.len());
        (0..k).all(|i| {
            self.assign_cost(expanded_param(a, i), expanded_param(b, i))
                .is_some()
        })
    }

    /// The argument list a call actually emits. `vararg_from` is `None` for a
    /// fixed-arity call (the arguments pass through); for a variable-arity one
    /// it is the index where packing starts, and everything from there becomes
    /// a single array literal of the parameter's component type — the array the
    /// callee's body sees as its `T[]` parameter.
    fn effective_args(
        args: &[Expr],
        param_tys: &[String],
        vararg_from: Option<usize>,
    ) -> Vec<Expr> {
        let Some(from) = vararg_from else {
            return args.to_vec();
        };
        let elem_ty = param_tys
            .last()
            .and_then(|t| t.strip_suffix("[]"))
            .map(str::to_string);
        let mut out = args[..from].to_vec();
        out.push(Expr::ArrayLit {
            elems: args[from..].to_vec(),
            elem_ty,
        });
        out
    }

    /// Resolve an instance-method call on a static receiver class by argument
    /// type. `None` when no method of that name resolves for these arguments.
    fn resolve_instance_call(
        &self,
        class: &str,
        method: &str,
        arg_tys: &[Option<String>],
    ) -> Option<InstanceResolved> {
        let info = self.classes.get(class)?;
        let named: Vec<&MethodMeta> = info.methods.iter().filter(|m| m.name == method).collect();
        let cands: Vec<(&[String], bool)> = named
            .iter()
            .map(|m| (m.param_tys.as_slice(), m.varargs))
            .collect();
        let (i, vararg_from) = self.resolve_overload(&cands, arg_tys)?;
        let m = named[i];
        Some(InstanceResolved {
            param_tys: m.param_tys.clone(),
            ret_name: m.ret.clone(),
            ret: numtype_of_ty(&m.ret).unwrap_or(NumType::Other),
            vararg_from,
        })
    }

    /// Resolve the concrete implementation of an exact `(method, param_tys)`
    /// signature visible on `class`: the mangled subroutine name and its return
    /// type name. Used per-subclass for virtual dispatch (the overload is chosen
    /// once statically; each runtime class supplies its own override body).
    fn resolve_instance_sig(
        &self,
        class: &str,
        method: &str,
        param_tys: &[String],
    ) -> Option<(String, String)> {
        let info = self.classes.get(class)?;
        // Exact signature match to a concrete (bodied) method — the common case.
        if let Some(m) = info
            .methods
            .iter()
            .find(|m| !m.is_abstract && m.name == method && m.param_tys == param_tys)
        {
            return Some((mangle(&m.defining, method, &m.param_tys), m.ret.clone()));
        }
        // Erased-generic override: an interface/base method with a type-variable
        // parameter (`score(T)`) is implemented with the concrete erased type
        // (`score(String)`), so the raw strings differ. When exactly one concrete
        // method of this name+arity exists on the class, it is that override.
        let mut same_arity = info
            .methods
            .iter()
            .filter(|m| !m.is_abstract && m.name == method && m.param_tys.len() == param_tys.len());
        let only = same_arity.next()?;
        if same_arity.next().is_none() {
            return Some((
                mangle(&only.defining, method, &only.param_tys),
                only.ret.clone(),
            ));
        }
        None
    }

    /// Resolve a top-level user `static` method call by argument type: the
    /// mangled target subroutine and its return type. `None` when no method of
    /// that name+arity exists (an unresolved reference or arity mismatch).
    /// The superclass chain of `class`, most-derived first — the order Java
    /// searches for a `static` member, a subclass's declaration *hiding* the
    /// one it inherits.
    fn static_lookup_chain(&self, class: &str) -> Vec<String> {
        let mut chain = Vec::new();
        let mut cur = Some(class.to_string());
        // The class graph is acyclic, but a malformed `extends` cycle would spin
        // here, so the walk is bounded by the number of declared classes.
        while let Some(c) = cur {
            if chain.contains(&c) || chain.len() > self.classes.len() {
                break;
            }
            cur = self
                .classes
                .get(&c)
                .and_then(|ci| ci.superclass.clone())
                .filter(|s| s != &c);
            chain.push(c);
        }
        chain
    }

    /// Resolve `class.name(args)` — a **qualified** static call. Java looks the
    /// name up in `class`'s own declarations first and only then in the classes
    /// it inherits from; it never reaches an unrelated class. Restricting the
    /// candidate set to that chain is what keeps `Q.f(1)` out of `P.f(int)`
    /// when both `P` and `Q` declare an `f` — with one flat by-name pool the
    /// nearest *signature* won regardless of receiver, which silently ran the
    /// wrong method body.
    fn resolve_static_on(
        &self,
        class: &str,
        name: &str,
        arg_tys: &[Option<String>],
    ) -> Option<StaticResolved> {
        let overloads = self.methods.get(name)?;
        // Each class in the chain is a separate resolution scope: the most
        // derived one that declares the name at all wins, hiding the rest.
        self.static_lookup_chain(class).into_iter().find_map(|c| {
            let scoped: Vec<&MethodSig> = overloads.iter().filter(|s| s.owner == c).collect();
            (!scoped.is_empty())
                .then(|| self.pick_from(&scoped, name, arg_tys))
                .flatten()
        })
    }

    /// Resolve an **unqualified** `name(args)` call. The enclosing class's own
    /// chain is searched first (Java's rule); the whole pool is the fallback,
    /// which is what lets a nested class call a sibling's or the entry class's
    /// static the way javars has always allowed.
    fn resolve_static_call(
        &self,
        name: &str,
        arg_tys: &[Option<String>],
    ) -> Option<StaticResolved> {
        if let Some(cur) = self.current_class.as_deref() {
            if let Some(r) = self.resolve_static_on(cur, name, arg_tys) {
                return Some(r);
            }
        }
        let overloads = self.methods.get(name)?;
        let all: Vec<&MethodSig> = overloads.iter().collect();
        self.pick_from(&all, name, arg_tys)
    }

    /// Choose the best-matching overload out of an already-scoped candidate set.
    fn pick_from(
        &self,
        cands: &[&MethodSig],
        name: &str,
        arg_tys: &[Option<String>],
    ) -> Option<StaticResolved> {
        let sigs: Vec<(&[String], bool)> = cands
            .iter()
            .map(|s| (s.param_tys.as_slice(), s.varargs))
            .collect();
        let (i, vararg_from) = self.resolve_overload(&sigs, arg_tys)?;
        let s = cands[i];
        Some(StaticResolved {
            mangled: mangle_static(&s.owner, name, &s.param_tys),
            ret: s.ret,
            ret_name: s.ret_name.clone(),
            param_tys: s.param_tys.clone(),
            vararg_from,
        })
    }

    /// Resolve a constructor of `class` by argument type: the chosen ctor's
    /// parameter-type list (for mangling the `<init>` subroutine) and its
    /// variable-arity packing point. `None` when no constructor resolves.
    fn resolve_ctor(
        &self,
        class: &str,
        arg_tys: &[Option<String>],
    ) -> Option<(Vec<String>, Option<usize>)> {
        let info = self.classes.get(class)?;
        let cands: Vec<(&[String], bool)> = info
            .ctors
            .iter()
            .map(|c| (c.param_tys.as_slice(), c.varargs))
            .collect();
        let (i, vararg_from) = self.resolve_overload(&cands, arg_tys)?;
        Some((info.ctors[i].param_tys.clone(), vararg_from))
    }

    /// True when `class` declares or inherits any method named `method` with
    /// `argc` parameters (an existence check for dispatch decisions).
    fn has_instance_method(&self, class: &str, method: &str, argc: usize) -> bool {
        self.classes.get(class).is_some_and(|ci| {
            ci.methods.iter().any(|m| {
                m.name == method
                    && (m.param_tys.len() == argc
                        // A variable-arity method accepts any count from its
                        // fixed parameters up, so a bare `f(1, 2, 3)` inside
                        // the class still routes to `this.f(int... xs)`.
                        || (m.varargs && argc + 1 >= m.param_tys.len()))
            })
        })
    }

    /// True when `class` is `base`, a (transitive) subclass of it, or a type
    /// that implements/extends the interface `base` — walking the full supertype
    /// graph (superclass + interfaces).
    fn is_subclass(&self, class: &str, base: &str) -> bool {
        if class == base {
            return true;
        }
        let mut stack = vec![class.to_string()];
        let mut seen = std::collections::HashSet::new();
        let mut guard = 0;
        while let Some(cur) = stack.pop() {
            if cur == base {
                return true;
            }
            if !seen.insert(cur.clone()) {
                continue;
            }
            if let Some(ci) = self.classes.get(&cur) {
                stack.extend(ci.supertypes.iter().cloned());
            }
            guard += 1;
            if guard > 10000 {
                return false;
            }
        }
        false
    }

    /// The virtual-dispatch targets for `method(argc)` on a static receiver class
    /// `rc`: for every class in `rc`'s subtree, its resolved `(class, mangled)`.
    /// `None` when `rc` does not resolve the method at all. Sorted by class name
    /// for deterministic bytecode.
    fn virtual_targets(
        &self,
        rc: &str,
        method: &str,
        param_tys: &[String],
    ) -> Option<Vec<(String, String)>> {
        // The method must exist on the static type (else it is a type error).
        // An abstract-only method on an interface still gates dispatch — the
        // concrete implementors below supply the bodies.
        if !self.has_instance_method(rc, method, param_tys.len()) {
            return None;
        }
        let mut v: Vec<(String, String)> = self
            .classes
            .iter()
            // Only concrete (instantiable) classes are runtime dispatch targets;
            // an interface is never a runtime class of any object.
            .filter(|(_, ci)| !ci.is_interface)
            .map(|(k, _)| k)
            .filter(|k| self.is_subclass(k, rc))
            .filter_map(|k| {
                self.resolve_instance_sig(k, method, param_tys)
                    .map(|(m, _)| (k.clone(), m))
            })
            .collect();
        v.sort();
        Some(v)
    }

    /// Emit a call to instance `method(args)` on `recv` (whose static class is
    /// `rc`), binding `recv` as `this`. When the method is not overridden anywhere
    /// in `rc`'s subtree, a direct `Op::Call` is emitted; otherwise a runtime
    /// dispatch chain keyed on the receiver's actual class selects the override
    /// (true virtual dispatch — arguments are evaluated once into temps).
    fn dispatch_instance_method(
        &mut self,
        recv: &Expr,
        rc: &str,
        method: &str,
        args: &[Expr],
        line: u32,
    ) -> Result<(), String> {
        // Resolve which overload the static argument types select, then dispatch
        // that exact signature virtually on the receiver's runtime class.
        let arg_tys: Vec<Option<String>> = args.iter().map(|a| self.expr_java_type(a)).collect();
        let resolved = self
            .resolve_instance_call(rc, method, &arg_tys)
            .ok_or_else(|| {
                format!(
                "javars: class `{rc}` has no method `{method}` taking {} argument(s) (line {line})",
                args.len()
            )
            })?;
        let param_tys = resolved.param_tys;
        // A variable-arity call packs its trailing arguments into one array
        // before anything is emitted, so every path below — the direct call,
        // the virtual dispatch chain, and the argument temps — sees the same
        // fixed-arity argument list the callee's parameters expect.
        let packed = Self::effective_args(args, &param_tys, resolved.vararg_from);
        let args: &[Expr] = &packed;
        let targets = self
            .virtual_targets(rc, method, &param_tys)
            .ok_or_else(|| {
                format!(
                "javars: class `{rc}` has no method `{method}` taking {} argument(s) (line {line})",
                args.len()
            )
            })?;
        let distinct: std::collections::HashSet<&str> =
            targets.iter().map(|(_, m)| m.as_str()).collect();
        // Calling the single abstract method of a functional interface may land
        // on a lambda, which is a closure rather than a class instance — so that
        // receiver gets its own arm in the dispatch chain (keyed on the sentinel
        // runtime class the host reports for a closure), and the fast path is
        // given up even when only one class implements the interface.
        let lambda_arm = matches!(
            self.functional_sam(rc),
            Some((sam, arity)) if sam == method && arity == args.len()
        );
        // A functional interface's *other* methods — its `default` ones
        // (`Predicate.negate`, `Function.andThen`) — are reachable on a lambda
        // receiver too, and a closure's runtime class matches no concrete-class
        // arm. The interface's own body is what Java runs for it, so it gets its
        // own arm keyed on the same closure sentinel.
        let default_arm: Option<String> = if lambda_arm || self.functional_sam(rc).is_none() {
            None
        } else {
            self.resolve_instance_sig(rc, method, &param_tys)
                .map(|(m, _)| m)
        };
        // Fast path: exactly one body is reachable. That is a single concrete
        // implementation across the whole subtree — the concrete target's
        // mangled name, not the static type's, so an interface- or
        // abstract-typed receiver still calls a real subroutine — or, when no
        // class implements the interface at all, its own `default` body, which
        // is the only thing a lambda receiver can run.
        let only_body = match (targets.first(), &default_arm) {
            (None, Some(d)) => Some(d.clone()),
            (Some((_, m)), d) if distinct.len() == 1 && d.as_deref().unwrap_or(m) == m => {
                Some(m.clone())
            }
            _ => None,
        };
        if !lambda_arm {
            if let Some(mangled) = only_body {
                self.expr(recv)?; // this (deepest)
                self.call_args_targeted(args, &param_tys)?;
                let idx = self.b.add_name(&mangled);
                self.b.emit(Op::Call(idx, args.len() as u8 + 1), line);
                self.emit_exc_check(line);
                return Ok(());
            }
            if targets.is_empty() {
                return Err(format!(
                    "javars: no concrete implementation of `{method}` for `{rc}` (line {line})"
                ));
            }
        }
        // Virtual path: stash receiver + args in temps (single evaluation), read
        // the runtime class, then dispatch.
        let recv_t = self.temp();
        self.expr(recv)?;
        self.emit_set(&recv_t, line);
        let arg_ts: Vec<String> = args
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let t = self.temp();
                let want = param_tys.get(i).cloned();
                self.expr_targeted(a, want.as_deref())?;
                self.emit_set(&t, line);
                Ok(t)
            })
            .collect::<Result<_, String>>()?;
        let class_t = self.temp();
        self.emit_get(&recv_t, line);
        self.b.emit(Op::CallBuiltin(crate::host::JCLASSOF, 1), line);
        self.emit_set(&class_t, line);

        let argc = args.len() as u8 + 1;
        let mut end_jumps = Vec::new();
        if lambda_arm {
            // if classof == "#lambda" { closure + args; JCLOSURE_CALL }
            self.emit_get(&class_t, line);
            let cc = self
                .b
                .add_constant(Value::str(crate::host::LAMBDA_CLASS.to_string()));
            self.b.emit(Op::LoadConst(cc), line);
            self.b.emit(Op::StrEq, line);
            let skip = self.b.emit(Op::JumpIfFalse(0), line);
            self.emit_get(&recv_t, line);
            for t in &arg_ts {
                self.emit_get(t, line);
            }
            self.emit_raising_builtin(crate::host::JCLOSURE_CALL, argc, line);
            end_jumps.push(self.b.emit(Op::Jump(0), line));
            let next = self.b.current_pos();
            self.b.patch_jump(skip, next);
        }
        for (class, mangled) in &targets {
            // if classof == "class" { this + args; Call(mangled) }
            self.emit_get(&class_t, line);
            let cc = self.b.add_constant(Value::str(class.clone()));
            self.b.emit(Op::LoadConst(cc), line);
            self.b.emit(Op::StrEq, line);
            let skip = self.b.emit(Op::JumpIfFalse(0), line);
            self.emit_get(&recv_t, line);
            for t in &arg_ts {
                self.emit_get(t, line);
            }
            let idx = self.b.add_name(mangled);
            self.b.emit(Op::Call(idx, argc), line);
            self.emit_exc_check(line);
            end_jumps.push(self.b.emit(Op::Jump(0), line));
            let next = self.b.current_pos();
            self.b.patch_jump(skip, next);
        }
        // Fallback (unreachable at runtime — every concrete class is a target):
        // call an arbitrary concrete target so the stack stays balanced. Using a
        // real target (not the static type) keeps this valid when `rc` is an
        // interface whose own method has no subroutine. A functional interface
        // that no class implements has no such target, so its fallback is the
        // closure call — which is also the arm that reports the Java
        // `NullPointerException` when the target is null.
        self.emit_get(&recv_t, line);
        for t in &arg_ts {
            self.emit_get(t, line);
        }
        match targets.first() {
            Some((_, base)) => {
                let idx = self.b.add_name(base);
                self.b.emit(Op::Call(idx, argc), line);
                self.emit_exc_check(line);
            }
            None => self.emit_raising_builtin(crate::host::JCLOSURE_CALL, argc, line),
        }
        let end = self.b.current_pos();
        for j in end_jumps {
            self.b.patch_jump(j, end);
        }
        Ok(())
    }

    /// Emit the Java default value for a declared type: `0` for integral, `0.0`
    /// for floating point, `false` for `boolean`, `null` (`Undef`) for arrays,
    /// `String`, and class references.
    fn emit_type_default(&mut self, ty: &str, line: u32) {
        match ty {
            "int" | "long" | "short" | "byte" | "char" => {
                self.b.emit(Op::LoadInt(0), line);
            }
            "double" | "float" => {
                self.b.emit(Op::LoadFloat(0.0), line);
            }
            "boolean" => {
                self.b.emit(Op::LoadFalse, line);
            }
            _ => {
                self.b.emit(Op::LoadUndef, line);
            }
        }
    }

    // ── 32-bit `int` overflow wrapping ────────────────────────────────────
    //
    // fusevm's integers are 64-bit; Java's `int` is 32-bit and wraps. The two
    // only disagree once a value leaves `int` range, and the compiler is the
    // only place that knows which operations are `int` operations — the runtime
    // value model has one integer type. So an operation whose *static* Java type
    // is `int` gets a sign-extend of its low 32 bits appended, and every other
    // operation is left alone.
    //
    // The sign-extend is `Shl 32; Shr 32` rather than a builtin because fusevm's
    // `Shr` is an arithmetic shift both in the interpreter and in the Cranelift
    // backend (`sshr`), so wrapping stays two native, JIT-traceable ops — a
    // `CallBuiltin` would abort trace recording and cost hot loops their JIT.

    /// True when `ty` is a static type whose arithmetic Java performs at 32-bit
    /// `int` width. `byte`/`short`/`char` operands promote to `int` before any
    /// binary operation, so they qualify; `long` (64-bit) and an unknown type do
    /// not.
    fn is_int_width(ty: Option<&str>) -> bool {
        matches!(ty, Some("int" | "short" | "byte" | "char" | "Character"))
    }

    /// True when `e`'s static Java type is `char`. A `char` runs as its code
    /// point, so this is the flag that says "convert to a one-character String
    /// before this value crosses into a String or a `Character` box".
    fn is_char_expr(&self, e: &Expr) -> bool {
        matches!(
            self.expr_java_type(e).as_deref(),
            Some("char" | "Character")
        )
    }

    /// True when `e`'s static Java type is `char[]` — the array `toCharArray`
    /// returns, whose elements are code points.
    fn is_char_array_expr(&self, e: &Expr) -> bool {
        self.expr_java_type(e).as_deref() == Some("char[]")
    }

    /// True when `e`'s static Java type is `float` (or `float[]`). fusevm has one
    /// floating representation, so `float` is a `double` *kept* at 32-bit
    /// precision — this is the flag that says where to narrow it and where to
    /// print it as a `float` rather than as a `double`.
    fn is_float32_expr(&self, e: &Expr) -> bool {
        matches!(self.expr_java_type(e).as_deref(), Some("float" | "float[]"))
    }

    /// Emit one arithmetic operation at 32-bit `float` width, with both operands
    /// already on the stack. Java rounds a `float` operation once, at 32 bits;
    /// computing it in `f64` and rounding afterwards rounds twice and can land a
    /// ulp away (`16777217.0f * 0.2f`), so the operation itself moves to the
    /// host rather than the narrowing being appended to a native op.
    fn emit_f32_arith(&mut self, op: BinOp, line: u32) {
        let code = match op {
            BinOp::Sub => crate::host::f32_op::SUB,
            BinOp::Mul => crate::host::f32_op::MUL,
            BinOp::Div => crate::host::f32_op::DIV,
            BinOp::Mod => crate::host::f32_op::REM,
            _ => crate::host::f32_op::ADD,
        };
        self.b.emit(Op::LoadInt(code), line);
        self.b
            .emit(Op::CallBuiltin(crate::host::JF32_ARITH, 3), line);
    }

    /// The Java type of `lhs <op> rhs` under binary numeric promotion, or `None`
    /// when either operand's type is unknown. Only the promoted *width* matters
    /// to the caller, so the operator itself does not enter into it.
    fn arith_result_type(&self, lhs: &Expr, rhs: &Expr) -> Option<&'static str> {
        let l = self.expr_java_type(lhs)?;
        let r = self.expr_java_type(rhs)?;
        Some(rank_name(numeric_rank(&l)?.max(numeric_rank(&r)?)))
    }

    /// Evaluate `e` and, when its static type is `char` (or `char[]`), apply
    /// Java's string conversion — the code point becomes the one-character
    /// String. Every other expression is emitted unchanged, so this is safe to
    /// use anywhere a value flows into a String or an erased (`Object`)
    /// position.
    fn emit_char_string(&mut self, e: &Expr) -> Result<(), String> {
        self.emit_converted_arg(e, true)
    }

    /// The same conversion, with the `float` half switchable.
    ///
    /// `String.format`'s numeric conversions (`%f`, `%e`, `%.9f`) must receive
    /// the *number* — Java widens the `float` to a `double` for them, so
    /// `%.9f` of `1.0f/3.0f` is `0.333333343`, which the shortest decimal
    /// `0.33333334` cannot reproduce. Only its `%s`-family conversions want
    /// `Float.toString`. A `char` has no such split: its one-character String
    /// serves `%c` and `%s` alike.
    fn emit_converted_arg(&mut self, e: &Expr, floats: bool) -> Result<(), String> {
        self.expr(e)?;
        if self.is_char_expr(e) || self.is_char_array_expr(e) {
            self.b.emit(Op::CallBuiltin(crate::host::JCHR_STR, 1), 0);
        } else if floats && self.is_float32_expr(e) {
            // A `float` and a `double` holding the same bits print differently,
            // and only the static type says which this is.
            self.b.emit(Op::CallBuiltin(crate::host::JF32_STR, 1), 0);
        }
        Ok(())
    }

    /// True when `lhs + rhs` is Java's *string* concatenation rather than
    /// arithmetic: one operand is a `String`, or one is a class instance whose
    /// `toString()` the concatenation would call.
    fn is_string_concat(&self, lhs: &Expr, rhs: &Expr) -> bool {
        let is_str = |e: &Expr| {
            self.expr_java_type(e).as_deref() == Some("String") || self.expr_class(e).is_some()
        };
        is_str(lhs) || is_str(rhs)
    }

    /// True when both operands of a binary arithmetic operation are statically
    /// `int`-width, which makes the result an `int` that wraps at 32 bits.
    fn operands_are_int(&self, lhs: &Expr, rhs: &Expr) -> bool {
        Self::is_int_width(self.expr_java_type(lhs).as_deref())
            && Self::is_int_width(self.expr_java_type(rhs).as_deref())
    }

    /// True when a compound assignment (`x op= e`, `a[i] op= e`, `f op= e`,
    /// `x++`) narrows back to a 32-bit `int`: the target is declared `int` and
    /// the operand is statically `int`-width. A `long` target, or an operand
    /// whose type is unknown, keeps the 64-bit result.
    fn compound_wraps(&self, target_ty: Option<&str>, value: &Expr) -> bool {
        target_ty == Some("int") && Self::is_int_width(self.expr_java_type(value).as_deref())
    }

    /// Sign-extend the low 32 bits of the value on top of the stack — Java's
    /// `int` overflow wrap.
    fn emit_wrap32(&mut self, line: u32) {
        self.b.emit(Op::LoadInt(32), line);
        self.b.emit(Op::Shl, line);
        self.b.emit(Op::LoadInt(32), line);
        self.b.emit(Op::Shr, line);
    }

    /// Narrow the value on top of the stack to a sub-`int` declared type. A
    /// compound assignment and `++`/`--` carry an *implicit narrowing cast* back
    /// to the target's type (JLS 15.26.2), so `byte b = 100; b += 100;` is -56
    /// and `char c = 65535; c++;` is 0. `byte`/`short` sign-extend with the same
    /// native shift pair `emit_wrap32` uses; `char` is unsigned, so it masks.
    /// Every other type (including `int`, whose wrap is `emit_wrap32`'s job) is
    /// left alone.
    fn emit_narrow_to(&mut self, ty: Option<&str>, line: u32) {
        match ty {
            Some("byte") => {
                self.b.emit(Op::LoadInt(56), line);
                self.b.emit(Op::Shl, line);
                self.b.emit(Op::LoadInt(56), line);
                self.b.emit(Op::Shr, line);
            }
            Some("short") => {
                self.b.emit(Op::LoadInt(48), line);
                self.b.emit(Op::Shl, line);
                self.b.emit(Op::LoadInt(48), line);
                self.b.emit(Op::Shr, line);
            }
            Some("char" | "Character") => {
                self.b.emit(Op::LoadInt(0xFFFF), line);
                self.b.emit(Op::BitAnd, line);
            }
            _ => {}
        }
    }

    /// The floating type an assignment of `e` into a `target`-typed slot widens
    /// to, or `None` when no widening primitive conversion applies.
    ///
    /// Java's assignment and method-invocation conversions (JLS 5.2 / 5.3)
    /// change the *value*, not only its static type: `double d = 7;` stores 7.0
    /// and prints `7.0`. javars's runtime is dynamically typed on the fusevm
    /// value model, so the conversion has to be emitted at each site the
    /// language performs one — every initializer, assignment, array-literal
    /// element, argument, and `return` whose target is `float`/`double` and
    /// whose source type is integral.
    fn widen_target(&self, target: Option<&str>, e: &Expr) -> Option<&'static str> {
        let t = match target? {
            "double" => "double",
            "float" => "float",
            _ => return None,
        };
        let src = self.expr_java_type(e)?;
        matches!(src.as_str(), "int" | "long" | "short" | "byte" | "char").then_some(t)
    }

    /// Emit the widening primitive conversion for the value already on top of
    /// the stack.
    ///
    /// `double` is the native `Op::TruncFloat`: it converts its operand to `f64`
    /// and truncates, and an integral operand is already whole, so the result is
    /// the same value as a `double` — Java's widening exactly, with no builtin
    /// call to break the JIT trace. `float` additionally rounds to 32-bit
    /// precision, which is a real value change (`float f = 16777217;` is
    /// 1.6777216E7), so it routes through the same host cast `(float) x` uses.
    fn emit_widen(&mut self, ty: &str, line: u32) {
        if ty == "float" {
            let c = self.b.add_constant(Value::str("float".to_string()));
            self.b.emit(Op::LoadConst(c), line);
            self.b.emit(Op::CallBuiltin(crate::host::JCAST, 2), line);
        } else {
            self.b.emit(Op::TruncFloat, line);
        }
    }

    /// The type a conditional's two branches are promoted to when it is a
    /// *floating* one — `flag ? 1 : 2.0` is a `double` conditional, so the `int`
    /// branch widens to 1.0 (JLS 15.25 binary numeric promotion). `None` for
    /// every other conditional, including the integral ones, whose branches are
    /// already represented identically.
    fn ternary_promotion(&self, then: &Expr, els: &Expr) -> Option<&'static str> {
        let t = numeric_rank(self.expr_java_type(then)?.as_str())?;
        let e = numeric_rank(self.expr_java_type(els)?.as_str())?;
        match rank_name(t.max(e)) {
            "double" => Some("double"),
            "float" => Some("float"),
            _ => None,
        }
    }

    /// The declared type name of the field `name` on `recv`'s static class, when
    /// both are statically known.
    fn field_type_name(&self, recv: &Expr, name: &str) -> Option<String> {
        let rc = self.expr_class(recv)?;
        self.classes.get(&rc)?.field_types.get(name).cloned()
    }

    /// The static numeric category of `e` under Java's binary numeric promotion.
    /// Drives the truncating-vs-floating choice for `/`.
    fn expr_type(&self, e: &Expr) -> NumType {
        match e {
            Expr::Int(_) | Expr::Long(_) => NumType::Int,
            Expr::Float(_) | Expr::Float32(_) => NumType::Float,
            Expr::Str(_) | Expr::Bool(_) => NumType::Other,
            // `char` is integral, so `'a' / 2` truncates like any other `int`
            // division.
            Expr::Char(_) => NumType::Int,
            Expr::Var(name) => self.lookup_type(name),
            Expr::Unary { op, rhs } => match op {
                // `-x` keeps the operand's numeric type; `~x` is always
                // integral; `!b` is boolean.
                UnOp::Neg => self.expr_type(rhs),
                UnOp::BitNot => NumType::Int,
                UnOp::Not => NumType::Other,
            },
            // A cast states the type outright.
            Expr::Cast { ty, .. } => numtype_of_ty(ty).unwrap_or(NumType::Other),
            Expr::Binary { op, lhs, rhs } => match op {
                // Shifts and bitwise ops on integral operands stay integral;
                // `&`/`|`/`^` on booleans do not, which `expr_java_type`
                // distinguishes.
                BinOp::Shl | BinOp::Shr | BinOp::Ushr => NumType::Int,
                BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
                    if self.expr_type(lhs) == NumType::Int && self.expr_type(rhs) == NumType::Int {
                        NumType::Int
                    } else {
                        NumType::Other
                    }
                }
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
            Expr::PostIncDec { name, .. } | Expr::PreIncDec { name, .. } => self.lookup_type(name),
            Expr::Println { .. } => NumType::Other,
            // A conditional expression's numeric category is the promotion of
            // its two result branches (Java's conditional-expression typing).
            Expr::Ternary { then, els, .. } => {
                let t = self.expr_type(then);
                let e = self.expr_type(els);
                if t == NumType::Other || e == NumType::Other {
                    NumType::Other
                } else if t == NumType::Float || e == NumType::Float {
                    NumType::Float
                } else {
                    NumType::Int
                }
            }
            Expr::Call { name, args, .. } => {
                let arg_tys: Vec<Option<String>> =
                    args.iter().map(|a| self.expr_java_type(a)).collect();
                self.resolve_static_call(name, &arg_tys)
                    .map(|s| s.ret)
                    .unwrap_or(NumType::Other)
            }
            Expr::MethodCall {
                recv, method, args, ..
            } => {
                // `T.helper(x)` — the named class's declared return type.
                if let Some(class) = self.user_class_ref(recv) {
                    let arg_tys: Vec<Option<String>> =
                        args.iter().map(|a| self.expr_java_type(a)).collect();
                    return self
                        .resolve_static_on(&class, method, &arg_tys)
                        .map(|s| s.ret)
                        .unwrap_or(NumType::Other);
                }
                // Static stdlib calls that yield an `int` participate in `/`
                // truncation typing.
                if let Expr::Var(class) = recv.as_ref() {
                    if let Some(nt) = static_call_numtype(class, method) {
                        return nt;
                    }
                }
                // A user-class instance method's declared return type.
                if let Some(rc) = self.expr_class(recv) {
                    let arg_tys: Vec<Option<String>> =
                        args.iter().map(|a| self.expr_java_type(a)).collect();
                    if let Some(r) = self.resolve_instance_call(&rc, method, &arg_tys) {
                        return r.ret;
                    }
                }
                // The `String` instance methods that return `int`.
                match method.as_str() {
                    "length" | "indexOf" => NumType::Int,
                    _ => NumType::Other,
                }
            }
            // An array element's category comes from the array's declared type
            // (`int[]` → int, `double[]` → double); `.length` is always `int`.
            Expr::Index { array, .. } => match array.as_ref() {
                Expr::Var(name) => self
                    .var_decl_type(name)
                    .map(array_elem_numtype)
                    .unwrap_or(NumType::Other),
                _ => NumType::Other,
            },
            Expr::Field { recv, name } => {
                if name == "length" {
                    return NumType::Int;
                }
                if let Some((_, ty)) = self.static_field_ref(e) {
                    return numtype_of_ty(&ty).unwrap_or(NumType::Other);
                }
                self.expr_class(recv)
                    .and_then(|rc| {
                        self.classes
                            .get(&rc)
                            .and_then(|ci| ci.field_types.get(name))
                            .and_then(|ty| numtype_of_ty(ty))
                    })
                    .unwrap_or(NumType::Other)
            }
            // Reference- and boolean-valued expressions are never a numeric
            // category.
            Expr::NewArray { .. }
            | Expr::ArrayLit { .. }
            | Expr::NewObject { .. }
            | Expr::InstanceOf { .. }
            | Expr::Lambda { .. }
            | Expr::MethodRef { .. }
            | Expr::This => NumType::Other,
            // An arrow `switch`'s type is its arms' common type; taking the
            // first expression arm's is enough, because `javac` has already
            // checked they agree.
            Expr::SwitchExpr { arms, .. } => arms
                .iter()
                .find_map(|a| match &a.body {
                    SwitchArmBody::Expr(e) => Some(self.expr_type(e)),
                    SwitchArmBody::Block(_) => None,
                })
                .unwrap_or(NumType::Other),
        }
    }

    /// Lower one user-defined static method to a call-frame subroutine. Args
    /// arrive on the value stack (`arg0` deepest); the prologue binds them into
    /// frame slots `0..arity`, the body runs in slot scope, and every exit
    /// leaves exactly one value on the stack (`Undef` for `void`) so a call is
    /// always stack-balanced.
    fn compile_method(&mut self, m: &Method) -> Result<(), String> {
        let entry = self.b.current_pos();
        let param_tys: Vec<String> = m.params.iter().map(|p| p.ty.clone()).collect();
        let name_idx = self
            .b
            .add_name(&mangle_static(&m.owner, &m.name, &param_tys));
        self.b.add_sub_entry(name_idx, entry);

        let mut scope = MethodScope::new();
        register_params(&mut scope, &m.params);
        self.scope = Some(scope);
        // A `static` method sees its own class's statics unqualified.
        let saved_class = self.current_class.replace(m.owner.clone());
        let saved_ret = self.current_ret.replace(m.ret.clone());

        // Prologue: pop args into their slots. The last parameter is on top of
        // the stack, so bind slots high-to-low.
        for i in (0..m.params.len()).rev() {
            self.b.emit(Op::SetSlot(i as u16), m.line);
        }

        let result = m.body.iter().try_for_each(|s| self.stmt(s));
        // Implicit `return;` on fall-off — `void` methods yield `null`.
        self.b.emit(Op::LoadUndef, m.line);
        self.b.emit(Op::ReturnValue, m.line);

        self.scope = None;
        self.current_class = saved_class;
        self.current_ret = saved_ret;
        result
    }

    // ── lambdas ──────────────────────────────────────────────────────────
    //
    // A lambda is a value that outlives the frame it was written in, but a
    // javars local lives in a fusevm call-frame slot that does not. So the
    // literal site emits a heap closure carrying a *snapshot* of the enclosing
    // locals, and the body becomes an ordinary subroutine whose slots hold the
    // parameters first and those captured values after. Java only lets a lambda
    // read effectively-final locals, so the snapshot is observationally exact —
    // and it is what gives the enhanced `for` Java's per-iteration capture,
    // which a by-name read of javars's shared loop variable would not.

    /// Build a closure value at a lambda literal: push the captured upvalues,
    /// then the body's name index, parameter count, and capture count, and call
    /// [`crate::host::JMAKE_CLOSURE`] (which returns the handle). The body itself
    /// is queued for emission with the other subroutines.
    fn compile_lambda(
        &mut self,
        params: &[String],
        body: &LambdaBody,
        line: u32,
    ) -> Result<(), String> {
        // The interface this lambda implements (when the assignment context
        // named one) types its parameters, so `Calc c = x -> 100 / x;` divides
        // integrally exactly where `int of(int)` says it should.
        // The interface's declared *return* type comes along for the same
        // reason: a `double of(int)` lambda widens its body's `int` result, so
        // `F f = a -> a; f.of(4)` is 4.0 exactly as Java's assignment
        // conversion on the `return` makes it.
        let target = self.lambda_target.take();
        let sam = target.as_deref().and_then(|t| self.functional_sam_meta(t));
        let param_tys: Vec<String> = sam.map(|s| s.param_tys.clone()).unwrap_or_default();
        let ret_ty: Option<String> = sam.map(|s| s.ret.clone());
        // Capture every name declared in the enclosing scope that the lambda's
        // own parameters do not shadow. Compiler temps are deliberately not in
        // `declared`, so none are captured.
        let mut visible: Vec<String> = match &self.scope {
            Some(sc) => sc.declared.iter().cloned().collect(),
            None => self
                .global_decl_types
                .keys()
                .chain(self.global_types.keys())
                .cloned()
                .collect(),
        };
        visible.sort();
        visible.dedup();
        let captures: Vec<Capture> = visible
            .into_iter()
            .filter(|n| !params.contains(n))
            .map(|n| Capture {
                decl_ty: self.var_decl_type(&n).map(str::to_string),
                num_ty: self.lookup_type(&n),
                name: n,
            })
            .collect();
        for c in &captures {
            let name = c.name.clone();
            self.emit_get(&name, line);
        }
        let captures_this = self.this_class.is_some();
        if captures_this {
            self.emit_this(line);
        }

        let name_idx = self.b.add_name(&format!("#lambda#{}", self.lambda_counter));
        self.lambda_counter += 1;
        let ncap = captures.len() + usize::from(captures_this);
        self.pending_lambdas.push(PendingLambda {
            name_idx,
            params: params.to_vec(),
            param_tys,
            ret_ty,
            captures,
            captures_this,
            body: body.clone(),
            this_class: self.this_class.clone(),
            current_class: self.current_class.clone(),
            line,
        });
        self.b.emit(Op::LoadInt(name_idx as i64), line);
        self.b.emit(Op::LoadInt(params.len() as i64), line);
        self.b.emit(Op::LoadInt(ncap as i64), line);
        self.b.emit(
            Op::CallBuiltin(crate::host::JMAKE_CLOSURE, ncap as u8 + 3),
            line,
        );
        Ok(())
    }

    /// Emit a queued lambda body as a subroutine. Slots hold the parameters
    /// (`0..n`), then the captured upvalues, then `this` when one was captured;
    /// the prologue binds all of them high-to-low, exactly like a method's.
    ///
    /// The body is its own frame for `break`/`continue`/`return`/`try`, so those
    /// stacks are swapped out around it — a `return` inside a lambda returns from
    /// the *lambda*, not from the method that wrote it.
    fn compile_lambda_body(&mut self, pl: PendingLambda) -> Result<(), String> {
        let entry = self.b.current_pos();
        self.b.add_sub_entry(pl.name_idx, entry);
        let line = pl.line;

        let mut scope = MethodScope::new();
        for (i, p) in pl.params.iter().enumerate() {
            scope.slot(p);
            scope.declared.insert(p.clone());
            if let Some(ty) = pl.param_tys.get(i) {
                scope.decl_types.insert(p.clone(), ty.clone());
                scope
                    .types
                    .insert(p.clone(), numtype_of_ty(ty).unwrap_or(NumType::Other));
            }
        }
        for c in &pl.captures {
            scope.slot(&c.name);
            scope.declared.insert(c.name.clone());
            scope.types.insert(c.name.clone(), c.num_ty);
            if let Some(t) = &c.decl_ty {
                scope.decl_types.insert(c.name.clone(), t.clone());
            }
        }
        let total = pl.params.len() + pl.captures.len() + usize::from(pl.captures_this);
        if pl.captures_this {
            // `this` rides in the slot after the last named capture; nothing
            // reads it by name, so it needs no `declared` entry.
            scope.this_slot = Some((total - 1) as u16);
            scope.next_slot = total as u16;
        }

        let saved_scope = self.scope.replace(scope);
        let saved_this = std::mem::replace(&mut self.this_class, pl.this_class.clone());
        let saved_current = std::mem::replace(&mut self.current_class, pl.current_class.clone());
        let saved_scopes = std::mem::take(&mut self.scopes);
        let saved_tries = std::mem::take(&mut self.tries);
        let saved_finallys = std::mem::take(&mut self.finallys);
        let saved_exits = std::mem::take(&mut self.exit_ops);
        let saved_ret = std::mem::replace(&mut self.current_ret, pl.ret_ty.clone());

        for i in (0..total).rev() {
            self.b.emit(Op::SetSlot(i as u16), line);
        }
        let result = match &pl.body {
            // An expression body's value is the lambda's result, converted to
            // the single abstract method's declared return type.
            LambdaBody::Expr(e) => self.expr_targeted(e, pl.ret_ty.as_deref()).map(|()| {
                self.b.emit(Op::ReturnValue, line);
            }),
            LambdaBody::Block(stmts) => stmts.iter().try_for_each(|s| self.stmt(s)),
        };
        // Fall-off (and a bare `return;`) yields `null`, which is what a `void`
        // functional interface wants and what a value-returning one never reaches.
        let end = self.b.current_pos();
        let exits = std::mem::replace(&mut self.exit_ops, saved_exits);
        for op in exits {
            self.b.patch_jump(op, end);
        }
        self.b.emit(Op::LoadUndef, line);
        self.b.emit(Op::ReturnValue, line);

        self.scope = saved_scope;
        self.this_class = saved_this;
        self.current_class = saved_current;
        self.scopes = saved_scopes;
        self.tries = saved_tries;
        self.finallys = saved_finallys;
        self.current_ret = saved_ret;
        result
    }

    /// The single abstract method of a functional interface: `(name, arity)`.
    ///
    /// Any interface with exactly one abstract method is a lambda target — Java's
    /// own rule, and the reason a user-declared `interface Calc { int of(int a); }`
    /// works with no registration. The `java.util.function` interfaces reach here
    /// the same way, because [`crate::prelude`] declares them as ordinary
    /// one-method interfaces.
    fn functional_sam(&self, ty: &str) -> Option<(String, usize)> {
        let sam = self.functional_sam_meta(ty)?;
        Some((sam.name.clone(), sam.param_tys.len()))
    }

    /// The full signature of a functional interface's single abstract method.
    fn functional_sam_meta(&self, ty: &str) -> Option<&MethodMeta> {
        let ci = self.classes.get(ty)?;
        if !ci.is_interface {
            return None;
        }
        let mut abstracts = ci.methods.iter().filter(|m| m.is_abstract);
        let sam = abstracts.next()?;
        abstracts.next().is_none().then_some(sam)
    }

    /// Lower `e` knowing the type it is being assigned to — the site of Java's
    /// *assignment conversion*. A lambda (or the method reference that desugars
    /// to one) reads the target as its functional interface; an integral
    /// expression assigned into a `float`/`double` slot is widened; a `char`
    /// entering a reference slot is boxed; and a bare `{…}` array literal takes
    /// its element type from the declaration it initializes.
    fn expr_targeted(&mut self, e: &Expr, target: Option<&str>) -> Result<(), String> {
        if !matches!(e, Expr::Lambda { .. } | Expr::MethodRef { .. }) {
            // A `char` bound to a reference-typed slot (`Object o = 'x';`, an
            // `Object` parameter, an `Object`-returning method) is *boxed* in
            // Java, so it renders as a character from then on — javars models
            // the box as the one-character String. `Character` is excluded
            // because javars keeps it as the primitive, which is what makes
            // `Character c = 'x'; c + 1` the 121 Java's unboxing gives.
            if target.is_some_and(|t| is_reference_type(t) && t != "Character")
                && self.is_char_expr(e)
            {
                return self.emit_char_string(e);
            }
            // `double[] a = {1, 2};` — the untyped literal takes its element
            // type from the declaration, and every element is assigned into a
            // slot of that type, so the conversion applies element-wise.
            if let Expr::ArrayLit {
                elems,
                elem_ty: None,
            } = e
            {
                if let Some(el) = target.and_then(|t| t.strip_suffix("[]")) {
                    let el = el.to_string();
                    return self.array_lit(elems, Some(&el));
                }
            }
            let widen = self.widen_target(target, e);
            self.expr(e)?;
            if let Some(w) = widen {
                self.emit_widen(w, 0);
            }
            return Ok(());
        }
        let saved = std::mem::replace(&mut self.lambda_target, target.map(str::to_string));
        let result = self.expr(e);
        self.lambda_target = saved;
        result
    }

    /// Lower a call's arguments, giving each the declared parameter type as its
    /// lambda target. `param_tys` may be shorter than `args` (an unresolved
    /// signature), in which case the extras lower untargeted.
    fn call_args_targeted(&mut self, args: &[Expr], param_tys: &[String]) -> Result<(), String> {
        for (i, a) in args.iter().enumerate() {
            self.expr_targeted(a, param_tys.get(i).map(String::as_str))?;
        }
        Ok(())
    }

    /// Desugar a method reference into the lambda it abbreviates.
    ///
    /// Java infers the reference's arity from its *target* type; javars has no
    /// target-typing pass, so the arity is taken from the referenced member's own
    /// declaration instead. That resolves every unambiguous form and rejects the
    /// rest with a diagnostic rather than guessing.
    fn desugar_method_ref(&self, recv: &Expr, method: &str, line: u32) -> Result<Expr, String> {
        // Synthesized parameter names. `#` is not a legal Java identifier char,
        // so they cannot collide with (or be shadowed by) a user variable.
        let mk = |arity: usize| -> Vec<String> { (0..arity).map(|i| format!("#p{i}")).collect() };
        let vars =
            |ps: &[String]| -> Vec<Expr> { ps.iter().map(|p| Expr::Var(p.clone())).collect() };
        let lambda = |params: Vec<String>, body: Expr| Expr::Lambda {
            body: LambdaBody::Expr(Box::new(body)),
            params,
            line,
        };

        // `System.out::println` — the print statement, not a dispatchable method.
        if let Expr::Field { recv: sys, name } = recv {
            if matches!(&**sys, Expr::Var(v) if v == "System") {
                let err = name == "err";
                let newline = match method {
                    "println" => true,
                    "print" => false,
                    _ => {
                        return Err(format!(
                            "javars: `System.{name}::{method}` is not a supported method reference (line {line})"
                        ))
                    }
                };
                let ps = mk(1);
                let arg = Expr::Var(ps[0].clone());
                return Ok(lambda(
                    ps,
                    Expr::Println {
                        newline,
                        err,
                        arg: Some(Box::new(arg)),
                    },
                ));
            }
        }

        // A bare type name on the left: `Point::new`, `Point::area`,
        // `Integer::parseInt`, `String::length`.
        if let Expr::Var(name) = recv {
            if !self.is_declared_var(name) {
                if let Some(e) = self.type_method_ref(name, method, line, &mk, &vars, &lambda)? {
                    return Ok(e);
                }
            }
        }

        // Otherwise the left side is a value: `obj::method`, `this::method`. The
        // receiver must be a plain name so the synthesized lambda captures it —
        // Java evaluates the receiver once, at the reference, and a captured
        // local is exactly that.
        if !matches!(recv, Expr::Var(_) | Expr::This) {
            return Err(format!(
                "javars: a bound method reference needs a variable or `this` receiver (line {line})"
            ));
        }
        let rc = self.expr_class(recv).ok_or_else(|| {
            format!("javars: cannot resolve the receiver of `::{method}` (line {line})")
        })?;
        let arity = self.unique_method_arity(&rc, method).ok_or_else(|| {
            format!(
                "javars: `{rc}::{method}` has no single method reference can name (line {line})"
            )
        })?;
        let ps = mk(arity);
        Ok(lambda(
            ps.clone(),
            Expr::MethodCall {
                recv: Box::new(recv.clone()),
                method: method.to_string(),
                args: vars(&ps),
                line,
            },
        ))
    }

    /// The type-name form of [`Compiler::desugar_method_ref`]. Returns `None`
    /// when `name` is not a type javars knows, so the caller can fall through to
    /// the bound (value receiver) form.
    #[allow(clippy::type_complexity)]
    fn type_method_ref(
        &self,
        name: &str,
        method: &str,
        line: u32,
        mk: &dyn Fn(usize) -> Vec<String>,
        vars: &dyn Fn(&[String]) -> Vec<Expr>,
        lambda: &dyn Fn(Vec<String>, Expr) -> Expr,
    ) -> Result<Option<Expr>, String> {
        if let Some(ci) = self.classes.get(name) {
            // `Point::new` — a constructor reference.
            if method == "new" {
                let arity = match ci.ctors.len() {
                    0 => 0,
                    1 => ci.ctors[0].param_tys.len(),
                    _ => {
                        return Err(format!(
                            "javars: `{name}::new` is ambiguous — {} constructors (line {line})",
                            ci.ctors.len()
                        ))
                    }
                };
                let ps = mk(arity);
                return Ok(Some(lambda(
                    ps.clone(),
                    Expr::NewObject {
                        class: name.to_string(),
                        args: vars(&ps),
                        line,
                    },
                )));
            }
            // `Point::area` — an unbound instance reference takes the receiver as
            // its first parameter.
            if let Some(arity) = self.unique_method_arity(name, method) {
                let ps = mk(arity + 1);
                return Ok(Some(lambda(
                    ps.clone(),
                    Expr::MethodCall {
                        recv: Box::new(Expr::Var(ps[0].clone())),
                        method: method.to_string(),
                        args: vars(&ps[1..]),
                        line,
                    },
                )));
            }
            // `Helper::twice` — a `static` method of a user class.
            if let Some(arity) = self.unique_static_arity(method) {
                let ps = mk(arity);
                return Ok(Some(lambda(
                    ps.clone(),
                    Expr::Call {
                        name: method.to_string(),
                        args: vars(&ps),
                        line,
                    },
                )));
            }
            return Err(format!(
                "javars: class `{name}` has no member `{method}` a method reference can name (line {line})"
            ));
        }
        if !is_static_class(name) {
            return Ok(None);
        }
        // `Integer::parseInt` — a modeled stdlib static.
        if let Some(arity) = stdlib_static_ref_arity(name, method) {
            let ps = mk(arity);
            return Ok(Some(lambda(
                ps.clone(),
                Expr::MethodCall {
                    recv: Box::new(Expr::Var(name.to_string())),
                    method: method.to_string(),
                    args: vars(&ps),
                    line,
                },
            )));
        }
        // `String::length` — an unbound `String` instance method.
        if name == "String" {
            if let Some(arity) = string_instance_ref_arity(method) {
                let ps = mk(arity + 1);
                return Ok(Some(lambda(
                    ps.clone(),
                    Expr::MethodCall {
                        recv: Box::new(Expr::Var(ps[0].clone())),
                        method: method.to_string(),
                        args: vars(&ps[1..]),
                        line,
                    },
                )));
            }
        }
        Err(format!(
            "javars: `{name}::{method}` is not a method reference javars models (line {line})"
        ))
    }

    /// The parameter count of `class.method` when the class declares exactly one
    /// method of that name; `None` when there is none or several (an overloaded
    /// name gives a method reference no unambiguous arity).
    fn unique_method_arity(&self, class: &str, method: &str) -> Option<usize> {
        let ci = self.classes.get(class)?;
        let mut it = ci.methods.iter().filter(|m| m.name == method);
        let first = it.next()?;
        it.next().is_none().then_some(first.param_tys.len())
    }

    /// The parameter count of a user `static` method with exactly one overload.
    fn unique_static_arity(&self, method: &str) -> Option<usize> {
        let sigs = self.methods.get(method)?;
        (sigs.len() == 1).then(|| sigs[0].param_tys.len())
    }

    /// Lower one instance method to a subroutine named `Class#method#argc`. Slot
    /// 0 holds `this`; parameters take slots `1..=argc`. The prologue binds all
    /// `argc + 1` incoming values (this deepest).
    fn compile_instance_method(&mut self, class: &str, m: &Method) -> Result<(), String> {
        let entry = self.b.current_pos();
        let param_tys: Vec<String> = m.params.iter().map(|p| p.ty.clone()).collect();
        let mangled = mangle(class, &m.name, &param_tys);
        let name_idx = self.b.add_name(&mangled);
        self.b.add_sub_entry(name_idx, entry);

        let mut scope = MethodScope::for_instance();
        register_params(&mut scope, &m.params);
        self.scope = Some(scope);
        self.this_class = Some(class.to_string());
        let saved_class = self.current_class.replace(class.to_string());
        let saved_ret = self.current_ret.replace(m.ret.clone());

        // Prologue: bind `this` (slot 0) + params (slots 1..=n), high-to-low.
        for i in (0..=m.params.len()).rev() {
            self.b.emit(Op::SetSlot(i as u16), m.line);
        }
        let result = m.body.iter().try_for_each(|s| self.stmt(s));
        self.b.emit(Op::LoadUndef, m.line);
        self.b.emit(Op::ReturnValue, m.line);

        self.scope = None;
        self.this_class = None;
        self.current_class = saved_class;
        self.current_ret = saved_ret;
        result
    }

    /// Lower a constructor to a subroutine named `Class#<init>#argc`. Field
    /// defaults/initializers are emitted by [`Compiler::new_object`] before the
    /// call, so the body only runs the programmer's constructor statements.
    /// `this` is slot 0; the ctor returns `null` (its result is discarded).
    fn compile_ctor(&mut self, cl: &Class, ctor: &Ctor) -> Result<(), String> {
        let entry = self.b.current_pos();
        let param_tys: Vec<String> = ctor.params.iter().map(|p| p.ty.clone()).collect();
        let mangled = mangle(&cl.name, "<init>", &param_tys);
        let name_idx = self.b.add_name(&mangled);
        self.b.add_sub_entry(name_idx, entry);

        let mut scope = MethodScope::for_instance();
        register_params(&mut scope, &ctor.params);
        self.scope = Some(scope);
        self.this_class = Some(cl.name.clone());
        let saved_class = self.current_class.replace(cl.name.clone());

        for i in (0..=ctor.params.len()).rev() {
            self.b.emit(Op::SetSlot(i as u16), ctor.line);
        }
        let result = ctor.body.iter().try_for_each(|s| self.stmt(s));
        self.b.emit(Op::LoadUndef, ctor.line);
        self.b.emit(Op::ReturnValue, ctor.line);

        self.scope = None;
        self.this_class = None;
        self.current_class = saved_class;
        result
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
            // `int a = 1, b = 2;` — one declaration statement, several
            // declarators, lowered left to right so a later initializer sees the
            // earlier names (Java's evaluation order).
            StmtKind::Locals(decls) => {
                for d in decls {
                    self.stmt(d)?;
                }
                Ok(())
            }
            StmtKind::Local { ty, name, init } => {
                // Record the declared numeric type (for `/` truncation); `var`
                // and untracked types are inferred from the initializer. The raw
                // type string powers class-typed dispatch — for `var`, infer the
                // class from the initializer.
                let nt = numtype_of_ty(ty)
                    .or_else(|| init.as_ref().map(|e| self.expr_type(e)))
                    .unwrap_or(NumType::Other);
                let raw = if ty == "var" {
                    // Infer `var`'s type from the initializer: a user class when
                    // there is one, else the initializer's static Java type
                    // (`int`, `String`, `int[]`, …). Keeping the inferred type —
                    // rather than the literal string `var` — is what lets a
                    // `var` loop counter participate in `int` wrapping and in
                    // array-element typing.
                    init.as_ref()
                        .and_then(|e| self.expr_class(e).or_else(|| self.expr_java_type(e)))
                        .unwrap_or_else(|| ty.clone())
                } else {
                    ty.clone()
                };
                self.declare_local(name, &raw, nt);
                if let Some(e) = init {
                    self.expr_targeted(e, Some(&raw))?;
                    self.emit_set(name, line);
                }
                // An uninitialized local is simply unbound until first assigned
                // (Java's definite-assignment check is not enforced yet).
                Ok(())
            }
            StmtKind::Assign { name, op, value } => {
                // A bare name that is a field of `this` (not a local) is an
                // implicit `this.name = …` field assignment.
                if self.implicit_this_field(name).is_some() {
                    let recv = Expr::This;
                    return self.field_assign(&recv, name, *op, value, line);
                }
                // A bare name that is a `static` field of the enclosing class
                // writes that class's shared cell.
                if let Some((class, ty)) = self.static_field_owner(name) {
                    return self.static_assign(&class, &ty, name, *op, value, line);
                }
                let l = self.lookup_type(name);
                // A compound assignment back into an `int` variable wraps.
                let wrap =
                    *op != AssignOp::Assign && self.compound_wraps(self.var_decl_type(name), value);
                match op {
                    AssignOp::Assign => {
                        let target = self.var_decl_type(name).map(str::to_string);
                        self.expr_targeted(value, target.as_deref())?;
                    }
                    // `x <op>= e` → `x = x <op> e`, through the one shared
                    // lowering that also handles `/=`'s int truncation, `%=`'s
                    // zero check, the shifts' width masking, and the logical
                    // `&=`/`|=`/`^=` on booleans.
                    _ => {
                        self.emit_get(name, line);
                        let decl = self.var_decl_type(name).map(str::to_string);
                        self.emit_compound(*op, value, l, decl.as_deref(), wrap, line)?;
                        self.emit_narrow_to(decl.as_deref(), line);
                    }
                }
                self.emit_set(name, line);
                Ok(())
            }
            StmtKind::IndexAssign {
                array,
                index,
                op,
                value,
            } => self.index_assign(array, index, *op, value, line),
            StmtKind::FieldAssign {
                recv,
                name,
                op,
                value,
            } => self.field_assign(recv, name, *op, value, line),
            StmtKind::Expr(Expr::Println { newline, err, arg }) => {
                // The print builtin returns `null`; discard it in statement
                // position.
                self.println(*newline, *err, arg.as_deref())?;
                self.b.emit(Op::Pop, line);
                Ok(())
            }
            StmtKind::Expr(Expr::PostIncDec { name, inc }) => self.post_inc_dec(name, *inc),
            StmtKind::Expr(e) => {
                self.expr(e)?;
                self.b.emit(Op::Pop, line);
                Ok(())
            }
            StmtKind::If { cond, then, els } => self.if_stmt(cond, then, els),
            StmtKind::While { cond, body } => self.while_stmt(cond, body),
            StmtKind::DoWhile { body, cond } => self.do_while_stmt(body, cond),
            StmtKind::For {
                init,
                cond,
                update,
                body,
            } => self.for_stmt(init, cond, update, body),
            StmtKind::ForEach {
                ty,
                name,
                iter,
                body,
            } => self.foreach_stmt(ty, name, iter, body, line),
            StmtKind::Switch { disc, groups } => self.switch_stmt(disc, groups),
            StmtKind::Labeled { label, body } => {
                // A label prefixing a loop/`switch` is consumed by that
                // construct's own `BreakScope` (via `pending_label`). A label on
                // any other statement (a block, an `if`) gets a break-only scope
                // so `break label;` can still exit it.
                if is_breakable(body) {
                    self.pending_label = Some(label.clone());
                    self.stmt(body)?;
                    // If the construct did not consume the label (should not
                    // happen for breakables), clear it so it cannot leak.
                    self.pending_label = None;
                } else {
                    self.scopes
                        .push(BreakScope::switch_scope(Some(label.clone())));
                    self.stmt(body)?;
                    let scope = self.scopes.pop().unwrap();
                    let end = self.b.current_pos();
                    for op in scope.break_ops {
                        self.b.patch_jump(op, end);
                    }
                }
                Ok(())
            }
            StmtKind::Try {
                body,
                catches,
                finally_body,
            } => self.try_stmt(body, catches, finally_body, line),
            StmtKind::Throw(e) => self.throw_stmt(e, line),
            // `yield v;` — leave the value on the stack (running any cleanup
            // block opened inside this arm first, exactly as `return` does),
            // then jump to the arm's exit.
            StmtKind::Yield(e) => {
                self.expr(e)?;
                let keep = self.yield_finally_depth;
                self.emit_finallys_down_to(keep)?;
                let j = self.b.emit(Op::Jump(0), line);
                self.yield_ops.push(j);
                Ok(())
            }
            StmtKind::Break(label) => self.break_stmt(label.as_deref(), line),
            StmtKind::Continue(label) => self.continue_stmt(label.as_deref(), line),
            StmtKind::Return(val) => {
                if self.scope.is_some() {
                    // In a method: return a value (or `null` for `void`).
                    // Java evaluates the returned expression *before* running
                    // the cleanup blocks, so a `finally` that reassigns the
                    // variable cannot change the value already computed.
                    match val {
                        Some(e) => {
                            let want = self.current_ret.clone();
                            self.expr_targeted(e, want.as_deref())?;
                        }
                        None => {
                            self.b.emit(Op::LoadUndef, line);
                        }
                    }
                    self.emit_finallys_down_to(0)?;
                    self.b.emit(Op::ReturnValue, line);
                } else {
                    // In `main` (void): a bare `return;` ends the program; a
                    // value return is a type error javars does not accept.
                    if val.is_some() {
                        return Err(format!(
                            "javars: `return <value>` from void main is not supported (line {line})"
                        ));
                    }
                    self.emit_finallys_down_to(0)?;
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
        let label = self.pending_label.take();
        let top = self.b.current_pos();
        self.expr(cond)?;
        let jf = self.b.emit(Op::JumpIfFalse(0), 0);
        self.scopes.push(BreakScope::loop_scope(label));
        for s in body {
            self.stmt(s)?;
        }
        // `continue` re-tests the condition — target it at the loop top.
        let l = self.scopes.pop().unwrap();
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

    /// `do { body } while (cond);` — the body runs once unconditionally, then
    /// the condition gates a backward jump. `continue` targets the condition
    /// test; `break` targets the exit.
    fn do_while_stmt(&mut self, body: &[Stmt], cond: &Expr) -> Result<(), String> {
        let label = self.pending_label.take();
        let top = self.b.current_pos();
        self.scopes.push(BreakScope::loop_scope(label));
        for s in body {
            self.stmt(s)?;
        }
        // `continue` re-tests the condition — target it at the test emitted next.
        let test = self.b.current_pos();
        let l = self.scopes.pop().unwrap();
        for op in &l.continue_ops {
            self.b.patch_jump(*op, test);
        }
        self.expr(cond)?;
        self.b.emit(Op::JumpIfTrue(top), 0);
        let end = self.b.current_pos();
        for op in l.break_ops {
            self.b.patch_jump(op, end);
        }
        Ok(())
    }

    fn for_stmt(
        &mut self,
        init: &[Stmt],
        cond: &Option<Expr>,
        update: &[Stmt],
        body: &[Stmt],
    ) -> Result<(), String> {
        let label = self.pending_label.take();
        for s in init {
            self.stmt(s)?;
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
        self.scopes.push(BreakScope::loop_scope(label));
        for s in body {
            self.stmt(s)?;
        }
        // step label: the continue target is the update clause (or the loop-top
        // re-test when there is no update).
        let step = self.b.current_pos();
        for s in update {
            self.stmt(s)?;
        }
        self.b.emit(Op::Jump(top), 0);
        let end = self.b.current_pos();
        if let Some(jf) = jf {
            self.b.patch_jump(jf, end);
        }
        let l = self.scopes.pop().unwrap();
        for op in l.continue_ops {
            self.b.patch_jump(op, step);
        }
        for op in l.break_ops {
            self.b.patch_jump(op, end);
        }
        Ok(())
    }

    // ── exceptions (`throw` / `try` / `catch` / `finally`) ────────────────
    //
    // fusevm has no unwind opcode. javars models the in-flight exception as a
    // host-side pending value ([`crate::host::JTHROW`]) plus two compiler-side
    // pieces:
    //
    //   * inside a frame, an unwind is a `Jump` to the innermost enclosing
    //     handler — backpatched through [`Compiler::tries`];
    //   * across frames, an unwind is `LoadUndef; ReturnValue`, and every call
    //     site is followed by a pending-exception check that repeats the unwind
    //     in the caller. `Op::ReturnValue` truncates the value stack to the
    //     frame base, so the abandoned operands of the callee cost nothing.
    //
    // Only the second piece has a runtime cost, and only in a program that uses
    // exceptions at all ([`Compiler::has_exceptions`]).

    /// Abandon the current computation: jump to the innermost handler in this
    /// frame, or leave the frame so the caller's post-call check picks the
    /// exception up. In `main` (no frame to leave) it jumps to the program exit,
    /// where the uncaught report runs.
    fn emit_unwind(&mut self, line: u32) {
        if !self.tries.is_empty() {
            let op = self.b.emit(Op::Jump(0), line);
            self.tries.last_mut().unwrap().unwind_ops.push(op);
        } else if self.scope.is_some() {
            // A method/constructor frame: return a placeholder so the frame is
            // popped and the stack rebalanced. The caller discards it — the
            // pending exception, not the value, is what it acts on.
            self.b.emit(Op::LoadUndef, line);
            self.b.emit(Op::ReturnValue, line);
        } else {
            let op = self.b.emit(Op::Jump(0), line);
            self.exit_ops.push(op);
        }
    }

    /// Emit the post-call check: if the call left an exception in flight, unwind.
    /// A no-op for a program with no exceptions, which is why those programs'
    /// bytecode is unchanged. Must follow *every* `Op::Call` — a call path that
    /// skips it swallows the exception and resumes with a placeholder value.
    fn emit_exc_check(&mut self, line: u32) {
        if !self.has_exceptions {
            return;
        }
        self.b
            .emit(Op::CallBuiltin(crate::host::JEXC_PENDING, 0), line);
        let jf = self.b.emit(Op::JumpIfFalse(0), line);
        self.emit_unwind(line);
        let after = self.b.current_pos();
        self.b.patch_jump(jf, after);
    }

    /// Emit a call to a builtin that can raise a Java exception (an array index,
    /// a null receiver, `Integer.parseInt`, …), followed by the pending check.
    ///
    /// Every raising builtin is emitted through here rather than by a bare
    /// `Op::CallBuiltin`, because a fault site that skips the check would leave
    /// the throwable parked forever: the `Undef` the builtin returned would flow
    /// on as a value and the program would keep running past the exception.
    fn emit_raising_builtin(&mut self, id: u16, argc: u8, line: u32) {
        self.b.emit(Op::CallBuiltin(id, argc), line);
        self.emit_exc_check(line);
    }

    /// Raise a `java.lang` throwable from compiler-emitted code and unwind — the
    /// integer division-by-zero path, where the fault is visible statically but
    /// only the runtime knows the divisor.
    fn emit_fault(&mut self, class: &str, msg: &str, line: u32) {
        let c = self.b.add_constant(Value::str(class.to_string()));
        self.b.emit(Op::LoadConst(c), line);
        let m = self.b.add_constant(Value::str(msg.to_string()));
        self.b.emit(Op::LoadConst(m), line);
        self.b.emit(Op::CallBuiltin(crate::host::JFAULT, 2), line);
        self.b.emit(Op::Pop, line);
        self.emit_unwind(line);
    }

    /// Emit the end-of-`main` uncaught-exception report: an exception that no
    /// handler claimed prints Java's `Exception in thread "main" …` line and
    /// exits non-zero.
    fn emit_uncaught_check(&mut self) {
        self.b
            .emit(Op::CallBuiltin(crate::host::JEXC_PENDING, 0), 0);
        let jf = self.b.emit(Op::JumpIfFalse(0), 0);
        self.b.emit(Op::CallBuiltin(crate::host::JEXC_ABORT, 0), 0);
        self.b.emit(Op::Pop, 0);
        let after = self.b.current_pos();
        self.b.patch_jump(jf, after);
    }

    /// Lower `throw <expr>;`: evaluate the throwable, park it as the pending
    /// exception, then unwind.
    fn throw_stmt(&mut self, e: &Expr, line: u32) -> Result<(), String> {
        self.expr(e)?;
        self.b.emit(Op::CallBuiltin(crate::host::JTHROW, 1), line);
        self.b.emit(Op::Pop, line);
        self.emit_unwind(line);
        Ok(())
    }

    /// Lower `try { … } catch (E e) { … }* [finally { … }]`.
    ///
    /// Layout:
    ///
    /// ```text
    ///   depth = JEXC_DEPTH          ; so the handler can drop abandoned operands
    ///   <try body>                  ; unwinds inside it jump to `handler`
    ///   Jump normal
    /// handler:
    ///   JEXC_CUT(depth); exc = JEXC_TAKE
    ///   if (exc instanceof E1) { e1 = exc; <catch 1>; Jump normal }
    ///   …
    ///   Jump rethrow                ; no arm matched — `exc` continues outward
    /// pad:                          ; a catch arm threw: same treatment, but the
    ///   JEXC_CUT(depth); exc = JEXC_TAKE   ; NEW exception replaces `exc`
    /// rethrow:
    ///   <finally>                   ; the cleanup still runs …
    ///   JTHROW(exc); <unwind>       ; … then the exception continues outward
    /// normal:
    ///   <finally>
    /// ```
    ///
    /// The `finally` body is emitted twice — once per path — rather than shared
    /// through a subroutine, because a shared copy would need a return address
    /// and fusevm's frames are for calls, not for local jumps. Duplication is
    /// what `javac` itself did before `jsr`/`ret` were dropped, and it is why a
    /// `return`/`break`/`continue` leaving the block emits its own copy (see
    /// [`Compiler::emit_finallys_down_to`]).
    ///
    /// The exception is parked back into the pending slot only *after* the
    /// `finally` has run: leaving it in flight would make the cleanup block's
    /// own post-call checks fire immediately and skip it.
    fn try_stmt(
        &mut self,
        body: &[Stmt],
        catches: &[CatchArm],
        finally_body: &[Stmt],
        line: u32,
    ) -> Result<(), String> {
        // Record the stack depth so the handler can discard whatever the
        // abandoned expression had already pushed.
        let depth_t = self.temp();
        self.b
            .emit(Op::CallBuiltin(crate::host::JEXC_DEPTH, 0), line);
        self.emit_set(&depth_t, line);

        // The cleanup block is in scope for the try body AND for every catch
        // arm — a `return` from either runs it.
        let has_finally = !finally_body.is_empty();
        if has_finally {
            self.finallys.push(FinallyScope {
                body: finally_body.to_vec(),
                scope_depth: self.scopes.len(),
            });
        }

        self.tries.push(TryScope {
            unwind_ops: Vec::new(),
        });
        for s in body {
            self.stmt(s)?;
        }
        let scope = self.tries.pop().unwrap();
        let to_normal = self.b.emit(Op::Jump(0), line);

        // ── handler ──
        let handler = self.b.current_pos();
        for op in scope.unwind_ops {
            self.b.patch_jump(op, handler);
        }
        let exc_t = self.temp();
        self.emit_claim_exception(&depth_t, &exc_t, line);

        // `catch` arms, in source order — the first type match wins. An unwind
        // raised *inside* an arm belongs to the enclosing try, but must still
        // run this `finally`, so the arms get their own landing pad.
        self.tries.push(TryScope {
            unwind_ops: Vec::new(),
        });
        let mut matched_jumps = Vec::new();
        for arm in catches {
            // A multi-catch's alternatives are tested in order: any hit jumps
            // straight to the shared body, and only the last miss skips the arm.
            let mut hits = Vec::new();
            let mut jf = None;
            let last = arm.types.len().saturating_sub(1);
            for (i, ty) in arm.types.iter().enumerate() {
                self.emit_get(&exc_t, line);
                let c = self.b.add_constant(Value::str(ty.clone()));
                self.b.emit(Op::LoadConst(c), line);
                self.b
                    .emit(Op::CallBuiltin(crate::host::JINSTANCEOF, 2), line);
                if i == last {
                    jf = Some(self.b.emit(Op::JumpIfFalse(0), line));
                } else {
                    hits.push(self.b.emit(Op::JumpIfTrue(0), line));
                }
            }
            let jf = jf.expect("a catch arm always names at least one type");
            let body_start = self.b.current_pos();
            for h in hits {
                self.b.patch_jump(h, body_start);
            }
            // The bound variable's static type is the first alternative; Java
            // types a multi-catch parameter as the alternatives' least upper
            // bound, which javars does not compute.
            self.declare_local(&arm.name, &arm.types[0], NumType::Other);
            self.emit_get(&exc_t, line);
            self.emit_set(&arm.name, line);
            for s in &arm.body {
                self.stmt(s)?;
            }
            matched_jumps.push(self.b.emit(Op::Jump(0), line));
            let next = self.b.current_pos();
            self.b.patch_jump(jf, next);
        }
        let catch_scope = self.tries.pop().unwrap();

        // Falling out of the arm chain means no arm matched: `exc_t` already
        // holds the exception, so skip the pad that re-reads a new one.
        let skip_pad = (!catch_scope.unwind_ops.is_empty()).then(|| self.b.emit(Op::Jump(0), line));
        if !catch_scope.unwind_ops.is_empty() {
            let pad = self.b.current_pos();
            for op in catch_scope.unwind_ops {
                self.b.patch_jump(op, pad);
            }
            self.emit_claim_exception(&depth_t, &exc_t, line);
        }

        // Either way the cleanup runs, then the exception continues outward.
        let rethrow = self.b.current_pos();
        if let Some(j) = skip_pad {
            self.b.patch_jump(j, rethrow);
        }
        if has_finally {
            self.finallys.pop();
        }
        for s in finally_body {
            self.stmt(s)?;
        }
        self.emit_get(&exc_t, line);
        self.b.emit(Op::CallBuiltin(crate::host::JTHROW, 1), line);
        self.b.emit(Op::Pop, line);
        self.emit_unwind(line);

        // ── normal completion (try fell through, or a catch arm finished) ──
        let normal = self.b.current_pos();
        self.b.patch_jump(to_normal, normal);
        for op in matched_jumps {
            self.b.patch_jump(op, normal);
        }
        for s in finally_body {
            self.stmt(s)?;
        }
        Ok(())
    }

    /// Take the in-flight exception into `exc_t` and drop the operands the
    /// abandoned expression left behind (back to the depth recorded in
    /// `depth_t`). Both handler entry points start with exactly this.
    fn emit_claim_exception(&mut self, depth_t: &str, exc_t: &str, line: u32) {
        self.emit_get(depth_t, line);
        self.b.emit(Op::CallBuiltin(crate::host::JEXC_CUT, 1), line);
        self.b.emit(Op::Pop, line);
        self.b
            .emit(Op::CallBuiltin(crate::host::JEXC_TAKE, 0), line);
        self.emit_set(exc_t, line);
    }

    /// Emit the `finally` blocks a jump is about to leave, innermost first,
    /// down to `keep` remaining. The stack is temporarily shortened while a body
    /// is lowered so a `return` *inside* a cleanup block does not re-emit that
    /// same block, then restored — the statements after the jump are still
    /// inside the same `try`.
    fn emit_finallys_down_to(&mut self, keep: usize) -> Result<(), String> {
        if self.finallys.len() <= keep {
            return Ok(());
        }
        let pending = self.finallys.split_off(keep);
        let mut result = Ok(());
        'outer: for f in pending.iter().rev() {
            for s in &f.body {
                if let Err(e) = self.stmt(s) {
                    result = Err(e);
                    break 'outer;
                }
            }
        }
        self.finallys.extend(pending);
        result
    }

    /// The number of enclosing `finally` blocks that stay in scope when a jump
    /// targets breakable scope `idx` — the ones entered *outside* it. A jump
    /// leaving the frame entirely (`return`) keeps none.
    fn finallys_outside(&self, idx: usize) -> usize {
        self.finallys
            .iter()
            .take_while(|f| f.scope_depth <= idx)
            .count()
    }

    /// Lower the enhanced `for (T x : arr) { … }` over an array.
    ///
    /// Java specifies the array form as exactly this index loop, with the array
    /// and the cursor held in variables the program cannot name — so both live in
    /// compiler-minted `#t` temps here. Evaluating the iterable once matters:
    /// `for (int v : make())` must call `make()` a single time, and re-reading
    /// `.length` from the temp (rather than from the source expression) keeps
    /// that true.
    ///
    /// `continue` targets the increment, `break` the exit — the same contract
    /// the C-style loop uses, so labeled `break`/`continue` work unchanged.
    fn foreach_stmt(
        &mut self,
        ty: &str,
        name: &str,
        iter: &Expr,
        body: &[Stmt],
        line: u32,
    ) -> Result<(), String> {
        let label = self.pending_label.take();
        // `var` takes its type from the array's element type when that is
        // statically known (`String[]` → `String`), so the loop variable still
        // participates in `/` typing and class-typed dispatch.
        let elem_ty = if ty == "var" {
            self.expr_array_type(iter)
                .and_then(|t| t.strip_suffix("[]").map(str::to_string))
                .unwrap_or_else(|| ty.to_string())
        } else {
            ty.to_string()
        };
        let nt = numtype_of_ty(&elem_ty).unwrap_or(NumType::Other);
        self.declare_local(name, &elem_ty, nt);

        // The array and the index cursor, evaluated/initialised once. When the
        // iterable is not *statically* an array it goes through `JITER_ARRAY`,
        // which returns an array handle unchanged and snapshots a collection
        // into a fresh one — so `for (String s : list)` works and an array loop
        // emits exactly the ops it did before.
        let arr_t = self.temp();
        self.expr(iter)?;
        if self.expr_array_type(iter).is_none() {
            self.emit_raising_builtin(crate::host::JITER_ARRAY, 1, line);
        }
        self.emit_set(&arr_t, line);
        let idx_t = self.temp();
        self.b.emit(Op::LoadInt(0), line);
        self.emit_set(&idx_t, line);

        // `i < arr.length`
        let top = self.b.current_pos();
        self.emit_get(&idx_t, line);
        self.emit_get(&arr_t, line);
        self.emit_field_get("length", line);
        self.b.emit(Op::NumLt, line);
        let jf = self.b.emit(Op::JumpIfFalse(0), line);

        // The loop variable is rebound from `arr[i]` at the top of every
        // iteration, before the body runs.
        self.emit_get(&arr_t, line);
        self.emit_get(&idx_t, line);
        self.emit_raising_builtin(crate::host::JARRAY_GET, 2, line);
        self.emit_set(name, line);

        self.scopes.push(BreakScope::loop_scope(label));
        for s in body {
            self.stmt(s)?;
        }
        // step label: `continue` lands on the increment.
        let step = self.b.current_pos();
        self.emit_get(&idx_t, line);
        self.b.emit(Op::LoadInt(1), line);
        self.b.emit(Op::Add, line);
        self.emit_set(&idx_t, line);
        self.b.emit(Op::Jump(top), line);

        let end = self.b.current_pos();
        self.b.patch_jump(jf, end);
        let l = self.scopes.pop().unwrap();
        for op in l.continue_ops {
            self.b.patch_jump(op, step);
        }
        for op in l.break_ops {
            self.b.patch_jump(op, end);
        }
        Ok(())
    }

    /// Lower an arrow `switch` expression:
    /// `switch (d) { case A, B -> e; default -> { … yield v; } }`.
    ///
    /// Arms do not fall through, so this is a `?:` chain rather than the classic
    /// `switch`'s laid-out group bodies: the discriminant is evaluated once into
    /// a temp, each arm's labels are compared against it, and the matching arm's
    /// value is left on the stack before a jump to the end. Exactly one arm runs,
    /// which is what makes the construct an expression at all.
    ///
    /// A block arm's value comes from its `yield`, which is compiled as "leave
    /// the value on the stack, then jump to the end" — the same shape a matching
    /// expression arm produces, so the two are indistinguishable downstream.
    fn switch_expr(&mut self, disc: &Expr, arms: &[SwitchArm], line: u32) -> Result<(), String> {
        // Java writes `case RED ->` unqualified when switching on an enum; the
        // label's meaning comes from the discriminant's static type.
        let enum_disc = self
            .expr_java_type(disc)
            .filter(|t| self.classes.get(t).is_some_and(|ci| ci.is_enum));
        let disc_t = self.temp();
        self.expr(disc)?;
        self.emit_set(&disc_t, line);

        let mut end_jumps: Vec<usize> = Vec::new();
        let mut default_arm: Option<&SwitchArm> = None;
        for arm in arms {
            if arm.is_default {
                // `default` is laid out last whatever its source position, so an
                // arm written before it still gets to match.
                default_arm = Some(arm);
                continue;
            }
            // if (disc == L1 || disc == L2 …) { <arm>; jump end }
            let mut hits: Vec<usize> = Vec::new();
            let mut miss: Vec<usize> = Vec::new();
            for (i, label) in arm.labels.iter().enumerate() {
                self.emit_get(&disc_t, line);
                match (&enum_disc, label) {
                    (Some(class), Expr::Var(c)) => {
                        let g = enum_global(class, c);
                        self.emit_global_get(&g, line);
                    }
                    _ => self.expr(label)?,
                }
                // The same `NumEq` the classic `switch` uses: value equality for
                // `int`/`String` and handle identity for an enum singleton.
                self.b.emit(Op::NumEq, line);
                if i + 1 == arm.labels.len() {
                    miss.push(self.b.emit(Op::JumpIfFalse(0), line));
                } else {
                    hits.push(self.b.emit(Op::JumpIfTrue(0), line));
                }
            }
            let body_at = self.b.current_pos();
            for j in hits {
                self.b.patch_jump(j, body_at);
            }
            self.emit_switch_arm_body(&arm.body, line)?;
            end_jumps.push(self.b.emit(Op::Jump(0), line));
            let next = self.b.current_pos();
            for j in miss {
                self.b.patch_jump(j, next);
            }
        }
        match default_arm {
            Some(arm) => self.emit_switch_arm_body(&arm.body, line)?,
            // No `default`: `javac` only accepts that for an exhaustive `enum`
            // switch, so no value can be missing in a program it compiled. The
            // placeholder keeps the stack balanced if one somehow is.
            None => {
                self.b.emit(Op::LoadUndef, line);
            }
        }
        let end = self.b.current_pos();
        for j in end_jumps {
            self.b.patch_jump(j, end);
        }
        Ok(())
    }

    /// Emit one arrow arm's body so that it leaves exactly one value on the
    /// stack. An expression arm is that expression; a block arm runs its
    /// statements and takes its value from `yield` (or `null` on fall-off, which
    /// is what an arrow-`switch` *statement*'s arms all do).
    fn emit_switch_arm_body(&mut self, body: &SwitchArmBody, line: u32) -> Result<(), String> {
        match body {
            SwitchArmBody::Expr(e) => self.expr(e),
            SwitchArmBody::Block(stmts) => {
                let outer = std::mem::take(&mut self.yield_ops);
                let outer_depth =
                    std::mem::replace(&mut self.yield_finally_depth, self.finallys.len());
                for s in stmts {
                    self.stmt(s)?;
                }
                // Falling off the end of the block pushes `null` — which is what
                // an arrow-`switch` *statement*'s arms all do, and what a
                // value-producing arm never reaches because its `yield` jumped
                // straight past this instruction.
                self.b.emit(Op::LoadUndef, line);
                self.yield_finally_depth = outer_depth;
                let yields = std::mem::replace(&mut self.yield_ops, outer);
                let end = self.b.current_pos();
                for j in yields {
                    self.b.patch_jump(j, end);
                }
                Ok(())
            }
        }
    }

    /// Lower a classic `switch`. The discriminant is evaluated once into an
    /// internal temp, then a dispatch chain compares it (via `==`, matching
    /// javars's value-equality model for both `int` and `String`) against each
    /// group's labels and jumps to the first match. Group bodies are laid out
    /// consecutively so control falls through into the next group unless a
    /// `break` intervenes; an unmatched discriminant jumps to `default` (or the
    /// switch exit when there is no default).
    fn switch_stmt(&mut self, disc: &Expr, groups: &[SwitchGroup]) -> Result<(), String> {
        let label = self.pending_label.take();
        // Evaluate the discriminant once and stash it in an internal temp. The
        // name uses `#` (not a legal Java identifier char) so it never collides
        // with a user variable.
        let temp = format!("#switch{}", self.switch_counter);
        self.switch_counter += 1;
        // Java writes `case RED:` unqualified when switching on an enum; the
        // label's meaning comes from the discriminant's static type.
        let enum_disc = self
            .expr_java_type(disc)
            .filter(|t| self.classes.get(t).is_some_and(|ci| ci.is_enum));
        self.expr(disc)?;
        self.emit_set(&temp, 0);

        // Dispatch: for each group's labels, compare and jump-if-equal to that
        // group's body. Body positions are not known yet, so collect the jumps
        // and backpatch once the bodies are emitted.
        let mut group_jumps: Vec<Vec<usize>> = Vec::with_capacity(groups.len());
        let mut default_group: Option<usize> = None;
        for (gi, g) in groups.iter().enumerate() {
            let mut jumps = Vec::new();
            for lab in &g.labels {
                self.emit_get(&temp, 0);
                match (&enum_disc, lab) {
                    (Some(class), Expr::Var(c)) => {
                        self.emit_global_get(&enum_global(class, c), 0);
                    }
                    _ => self.expr(lab)?,
                }
                // Enum constants are singletons, so identity is equality — the
                // same `NumEq` handle comparison `==` on objects already uses.
                self.b.emit(Op::NumEq, 0);
                jumps.push(self.b.emit(Op::JumpIfTrue(0), 0));
            }
            if g.is_default {
                default_group = Some(gi);
            }
            group_jumps.push(jumps);
        }
        // No label matched: jump to the default body (patched later) or the exit.
        let to_default = self.b.emit(Op::Jump(0), 0);

        // Bodies, laid out in source order for fall-through.
        self.scopes.push(BreakScope::switch_scope(label));
        let mut group_starts = Vec::with_capacity(groups.len());
        for g in groups {
            group_starts.push(self.b.current_pos());
            for s in &g.body {
                self.stmt(s)?;
            }
        }
        let end = self.b.current_pos();
        let scope = self.scopes.pop().unwrap();

        // Patch each label's jump to its group body.
        for (gi, jumps) in group_jumps.into_iter().enumerate() {
            for op in jumps {
                self.b.patch_jump(op, group_starts[gi]);
            }
        }
        // Patch the no-match jump to the default body, or the exit.
        match default_group {
            Some(gi) => self.b.patch_jump(to_default, group_starts[gi]),
            None => self.b.patch_jump(to_default, end),
        }
        for op in scope.break_ops {
            self.b.patch_jump(op, end);
        }
        Ok(())
    }

    /// Lower `break;` / `break label;`. An unlabeled break targets the innermost
    /// loop or `switch`; a labeled break targets the matching named construct.
    fn break_stmt(&mut self, label: Option<&str>, line: u32) -> Result<(), String> {
        // The target is resolved first: the `finally` blocks the jump leaves are
        // exactly those entered inside the targeted construct, and their bodies
        // have to be emitted *before* the jump itself.
        match self.find_scope(label, |_| true) {
            Some(idx) => {
                let keep = self.finallys_outside(idx);
                self.emit_finallys_down_to(keep)?;
                let op = self.b.emit(Op::Jump(0), line);
                self.scopes[idx].break_ops.push(op);
                Ok(())
            }
            None if label.is_none() => {
                // A top-level `break` (no enclosing construct) ends the program,
                // preserving javars's existing behavior.
                self.emit_finallys_down_to(0)?;
                let op = self.b.emit(Op::Jump(0), line);
                self.exit_ops.push(op);
                Ok(())
            }
            None => Err(format!(
                "javars: undefined label `{}` for break (line {line})",
                label.unwrap()
            )),
        }
    }

    /// Lower `continue;` / `continue label;`. Targets the innermost enclosing
    /// loop (skipping `switch` scopes), or the named loop for a labeled form.
    fn continue_stmt(&mut self, label: Option<&str>, line: u32) -> Result<(), String> {
        match self.find_scope(label, |s| s.kind == ScopeKind::Loop) {
            Some(idx) => {
                let keep = self.finallys_outside(idx);
                self.emit_finallys_down_to(keep)?;
                let op = self.b.emit(Op::Jump(0), line);
                self.scopes[idx].continue_ops.push(op);
                Ok(())
            }
            None => Err(match label {
                Some(l) => format!("javars: undefined label `{l}` for continue (line {line})"),
                None => "javars: `continue` outside a loop".to_string(),
            }),
        }
    }

    /// Find the index of the innermost enclosing scope satisfying `pred`. With a
    /// label, the scope must also carry that label; without one, the innermost
    /// matching scope is used.
    fn find_scope(&self, label: Option<&str>, pred: impl Fn(&BreakScope) -> bool) -> Option<usize> {
        self.scopes.iter().enumerate().rev().find_map(|(i, s)| {
            let label_ok = match label {
                Some(l) => s.label.as_deref() == Some(l),
                None => true,
            };
            if label_ok && pred(s) {
                Some(i)
            } else {
                None
            }
        })
    }

    /// Lower `name++` / `name--` as a statement (result discarded), mutating a
    /// local/global or — when `name` is an implicit `this` field — that field.
    fn post_inc_dec(&mut self, name: &str, inc: bool) -> Result<(), String> {
        let op = if inc { AssignOp::Add } else { AssignOp::Sub };
        if self.implicit_this_field(name).is_some() {
            return self.field_assign(&Expr::This, name, op, &Expr::Int(1), 0);
        }
        if let Some((class, ty)) = self.static_field_owner(name) {
            return self.static_assign(&class, &ty, name, op, &Expr::Int(1), 0);
        }
        let decl = self.var_decl_type(name).map(str::to_string);
        let wrap = decl.as_deref() == Some("int");
        self.emit_get(name, 0);
        self.b.emit(Op::LoadInt(1), 0);
        // `float f; f++;` is a 32-bit addition like every other `float`
        // operation, not a 64-bit one narrowed afterwards.
        if decl.as_deref() == Some("float") {
            self.emit_f32_arith(if inc { BinOp::Add } else { BinOp::Sub }, 0);
            self.emit_set(name, 0);
            return Ok(());
        }
        self.b.emit(if inc { Op::Add } else { Op::Sub }, 0);
        if wrap {
            self.emit_wrap32(0);
        }
        // `++` carries the same implicit narrowing cast a compound assignment
        // does, so `char c = 65535; c++;` is 0 rather than 65536.
        self.emit_narrow_to(decl.as_deref(), 0);
        self.emit_set(name, 0);
        Ok(())
    }

    /// Lower `System.out.print[ln](arg)` (or the `System.err` variant when
    /// `err`) to the Java-formatting print builtin. Leaves the builtin's `null`
    /// return value on the stack.
    fn println(&mut self, newline: bool, err: bool, arg: Option<&Expr>) -> Result<(), String> {
        let n = match arg {
            Some(e) => {
                self.emit_stringified(e)?;
                1
            }
            None => 0,
        };
        let id = match (err, newline) {
            (false, true) => crate::host::JPRINTLN,
            (false, false) => crate::host::JPRINT,
            (true, true) => crate::host::JEPRINTLN,
            (true, false) => crate::host::JEPRINT,
        };
        self.b.emit(Op::CallBuiltin(id, n), 0);
        Ok(())
    }

    /// Evaluate `e`, dispatching a user-defined `toString()` when `e` is an
    /// instance of a class whose (subtree) declares one — so `println(obj)` and
    /// string concatenation (`"x " + obj`) honour the override. Otherwise
    /// evaluate normally (the host's `java_str` renders the default `Class@hash`
    /// form, which is what `String.valueOf(obj)` still gets — see BUGS.md).
    ///
    /// The dispatch is keyed on the receiver's *runtime* class even when the
    /// static type already declares `toString`, because that is also the test
    /// that catches a `null`: Java's string conversion of a null reference is
    /// the text "null", not a call on nothing.
    fn emit_stringified(&mut self, e: &Expr) -> Result<(), String> {
        if self.is_char_expr(e) || self.is_float32_expr(e) {
            return self.emit_char_string(e);
        }
        // A `List` operand's string conversion is its `toString()`, and for a
        // `subList` view that call is what reports a backing list which moved
        // underneath it. Routing a statically-known list through the collection
        // dispatch (instead of the host's infallible rendering) is what lets
        // the `ConcurrentModificationException` surface from `"x " + view`; it
        // produces the identical text for every other list.
        if self.expr_java_type(e).as_deref().and_then(collection_kind) == Some("list") {
            self.expr(e)?;
            let name_c = self.b.add_constant(Value::str("toString".to_string()));
            self.b.emit(Op::LoadConst(name_c), 0);
            self.emit_raising_builtin(crate::host::JCOLL_DISPATCH, 2, 0);
            return Ok(());
        }
        let Some(rc) = self.expr_class(e) else {
            return self.emit_host_stringified(e);
        };
        let overriders = self.to_string_overriders(&rc);
        if overriders.is_empty() {
            return self.emit_host_stringified(e);
        }
        self.emit_virtual_to_string(e, &overriders)
    }

    /// The fall-through of [`Compiler::emit_stringified`]: an operand whose
    /// static type does not name a user class — an `Object`, a `Map`, an erased
    /// `get()` — so the compiler cannot build a dispatch chain for it.
    ///
    /// When the program declares a `toString()` anywhere, the value goes through
    /// the [`JSTRINGIFY`](crate::host::JSTRINGIFY) builtin, which holds a `&mut
    /// VM` and can therefore run the override the runtime class resolves, at
    /// every depth of a nested collection. Left to fusevm's `Op::Add` instead,
    /// the operand would be stringified by the numeric hook — three values and
    /// no VM — so `"" + o` would print `Pt@1` while `println(o)`, which is a
    /// builtin, printed `Pt<1>`. One rendering that is wrong is a documented
    /// model; two renderings of one object in one program is a trap.
    ///
    /// A program with no override emits exactly the bytecode it always did: the
    /// `Op::Add` lowering is the JIT-visible one, and this must not cost
    /// anything when there is nothing to dispatch to.
    fn emit_host_stringified(&mut self, e: &Expr) -> Result<(), String> {
        self.expr(e)?;
        if self.has_user_tostring && !self.renders_without_a_class(e) {
            self.emit_raising_builtin(crate::host::JSTRINGIFY, 1, 0);
        }
        Ok(())
    }

    /// Whether `e` is statically known to be something no `toString()` override
    /// can answer for — a literal, or a value whose declared type is a primitive,
    /// a wrapper, or `String`. javars models every one of those as a scalar
    /// `Value`, never a heap handle, so routing it through the rendering builtin
    /// would only add a call to every `"x " + i` in a program that happens to
    /// declare one override somewhere.
    fn renders_without_a_class(&self, e: &Expr) -> bool {
        if matches!(
            e,
            Expr::Int(_)
                | Expr::Long(_)
                | Expr::Float(_)
                | Expr::Float32(_)
                | Expr::Str(_)
                | Expr::Char(_)
                | Expr::Bool(_)
        ) {
            return true;
        }
        matches!(
            self.expr_java_type(e).as_deref(),
            Some(
                "int"
                    | "long"
                    | "short"
                    | "byte"
                    | "char"
                    | "boolean"
                    | "double"
                    | "float"
                    | "Integer"
                    | "Long"
                    | "Short"
                    | "Byte"
                    | "Character"
                    | "Boolean"
                    | "Double"
                    | "Float"
                    | "String"
            )
        )
    }

    /// Every concrete subtype of `rc` that resolves a no-arg `toString`, as
    /// `(class, mangled subroutine)`, sorted for deterministic bytecode.
    fn to_string_overriders(&self, rc: &str) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = self
            .classes
            .iter()
            .filter(|(_, ci)| !ci.is_interface)
            .map(|(k, _)| k)
            .filter(|k| self.is_subclass(k, rc))
            .filter_map(|k| {
                self.resolve_instance_sig(k, "toString", &[])
                    .map(|(m, _)| (k.clone(), m))
            })
            .collect();
        v.sort();
        v
    }

    /// Render `e` through whichever `toString` its *runtime* class supplies,
    /// falling through to the host's default `Class@hash` when it has none. The
    /// receiver is evaluated exactly once, into a temp.
    fn emit_virtual_to_string(
        &mut self,
        e: &Expr,
        overriders: &[(String, String)],
    ) -> Result<(), String> {
        let recv_t = self.temp();
        self.expr(e)?;
        self.emit_set(&recv_t, 0);
        let class_t = self.temp();
        self.emit_get(&recv_t, 0);
        self.b.emit(Op::CallBuiltin(crate::host::JCLASSOF, 1), 0);
        self.emit_set(&class_t, 0);

        let mut end_jumps = Vec::new();
        for (class, mangled) in overriders {
            self.emit_get(&class_t, 0);
            let cc = self.b.add_constant(Value::str(class.clone()));
            self.b.emit(Op::LoadConst(cc), 0);
            self.b.emit(Op::StrEq, 0);
            let skip = self.b.emit(Op::JumpIfFalse(0), 0);
            self.emit_get(&recv_t, 0);
            let idx = self.b.add_name(mangled);
            self.b.emit(Op::Call(idx, 1), 0);
            self.emit_exc_check(0);
            end_jumps.push(self.b.emit(Op::Jump(0), 0));
            let next = self.b.current_pos();
            self.b.patch_jump(skip, next);
        }
        // No override for this runtime class: the value renders itself.
        self.emit_get(&recv_t, 0);
        let end = self.b.current_pos();
        for j in end_jumps {
            self.b.patch_jump(j, end);
        }
        Ok(())
    }

    fn expr(&mut self, e: &Expr) -> Result<(), String> {
        match e {
            Expr::Int(n) | Expr::Long(n) => {
                self.b.emit(Op::LoadInt(*n), 0);
            }
            Expr::Float(f) => {
                let c = self.b.add_constant(Value::float(*f));
                self.b.emit(Op::LoadConst(c), 0);
            }
            // A `float` literal is stored already rounded to 32-bit precision,
            // so `0.1f` is the `f64` nearest `0.1f32` from the start.
            Expr::Float32(f) => {
                let c = self.b.add_constant(Value::float(*f as f32 as f64));
                self.b.emit(Op::LoadConst(c), 0);
            }
            Expr::Str(s) => {
                let c = self.b.add_constant(Value::str(s.clone()));
                self.b.emit(Op::LoadConst(c), 0);
            }
            // A `char` runs as its code point; the static type is what turns it
            // back into a one-character String at a string conversion.
            Expr::Char(c) => {
                self.b.emit(Op::LoadInt(*c as i64), 0);
            }
            Expr::Bool(b) => {
                self.b
                    .emit(if *b { Op::LoadTrue } else { Op::LoadFalse }, 0);
            }
            Expr::Var(name) => {
                // `super` denotes the same object `this` does, so it evaluates
                // to the receiver — the superclass view is entirely a
                // compile-time matter (`bare_var_type` supplies the type,
                // `super_call` the non-virtual dispatch). Emitting the receiver
                // is what makes `super.f` read and write `this`'s field cell.
                if name == SUPER {
                    if self.this_class.is_none() {
                        return Err("javars: `super` used outside an instance method".to_string());
                    }
                    self.emit_this(0);
                }
                // A bare name that is a field of `this` (not a local) reads
                // `this.name`; otherwise it is a plain local/global.
                else if let Some(class) = self.enclosing_enum_constant(name) {
                    self.emit_global_get(&enum_global(&class, name), 0);
                } else if self.implicit_this_field(name).is_some() {
                    self.emit_this(0); // this
                    self.emit_field_get(name, 0);
                } else if let Some((class, _)) = self.static_field_owner(name) {
                    self.emit_global_get(&static_global(&class, name), 0);
                } else if !self.is_declared_var(name) && name != NULL_LITERAL {
                    // Nothing declares this name. Java's answer is "cannot find
                    // symbol"; javars read the unset cell instead and got
                    // `null`, so `undefinedVar` printed `null` and
                    // `undefinedVar + 1` printed `null1` — a typo becoming
                    // output rather than a diagnostic. It is also what made
                    // naming an unmodeled class a *runtime* failure:
                    // `IntStream.range(0, 3)` reported
                    // `NullPointerException: Cannot invoke "String.range()"`,
                    // because `IntStream` is, to javars, just an undeclared
                    // name.
                    return Err(format!("javars: cannot find symbol: `{name}`"));
                } else {
                    self.emit_get(name, 0);
                }
            }
            Expr::This => {
                if self.this_class.is_none() {
                    return Err("javars: `this` used outside an instance method".to_string());
                }
                self.emit_this(0);
            }
            Expr::NewArray {
                elem_ty,
                sizes,
                extra_dims,
            } => {
                if sizes.len() == 1 && *extra_dims == 0 {
                    // Single dimension — the direct allocate builtin.
                    self.expr(&sizes[0])?;
                    self.emit_type_default(elem_ty, 0);
                    self.emit_raising_builtin(crate::host::JARRAY_NEW, 2, 0);
                } else {
                    // Multi-dimensional: push each sized dimension, then the leaf
                    // default (the element-type default when fully sized, else
                    // `null` for the inner unsized levels).
                    for s in sizes {
                        self.expr(s)?;
                    }
                    if *extra_dims > 0 {
                        self.b.emit(Op::LoadUndef, 0);
                    } else {
                        self.emit_type_default(elem_ty, 0);
                    }
                    self.emit_raising_builtin(
                        crate::host::JARRAY_NEW_MULTI,
                        sizes.len() as u8 + 1,
                        0,
                    );
                }
            }
            Expr::ArrayLit { elems, elem_ty } => {
                let elem_ty = elem_ty.clone();
                self.array_lit(elems, elem_ty.as_deref())?;
            }
            Expr::Index { array, index } => {
                self.expr(array)?;
                self.expr(index)?;
                self.emit_raising_builtin(crate::host::JARRAY_GET, 2, 0);
            }
            Expr::Field { recv, name } => {
                // `Integer.MAX_VALUE` / `Math.PI` / … — a `static final` of a
                // `java.lang` type javars does not model as a class, folded to
                // its literal value.
                if let Some((v, _)) = self.wrapper_constant_ref(e) {
                    let c = self.b.add_constant(v);
                    self.b.emit(Op::LoadConst(c), 0);
                }
                // `Color.RED` names an enum constant's singleton, not a field of
                // a value — there is no receiver to evaluate.
                else if let Some(class) = self.enum_constant_ref(e) {
                    self.emit_global_get(&enum_global(&class, name), 0);
                } else if let Some((class, _)) = self.static_field_ref(e) {
                    // `T.n` names a `static` field's shared cell — the receiver
                    // is a type, so there is nothing to evaluate.
                    self.emit_global_get(&static_global(&class, name), 0);
                } else {
                    self.expr(recv)?;
                    self.emit_field_get(name, 0);
                }
            }
            Expr::NewObject { class, args, line } => self.new_object(class, args, *line)?,
            Expr::InstanceOf { expr, class } => {
                self.expr(expr)?;
                let class_c = self.b.add_constant(Value::str(class.clone()));
                self.b.emit(Op::LoadConst(class_c), 0);
                self.b.emit(Op::CallBuiltin(crate::host::JINSTANCEOF, 2), 0);
            }
            Expr::Unary { op, rhs } => {
                self.expr(rhs)?;
                match op {
                    UnOp::Neg => {
                        self.b.emit(Op::Negate, 0);
                        // `-Integer.MIN_VALUE` is `Integer.MIN_VALUE`.
                        if Self::is_int_width(self.expr_java_type(rhs).as_deref()) {
                            self.emit_wrap32(0);
                        }
                    }
                    UnOp::Not => {
                        self.b.emit(Op::LogNot, 0);
                    }
                    UnOp::BitNot => {
                        self.b.emit(Op::BitNot, 0);
                    }
                }
            }
            Expr::Cast { ty, expr, line } => self.cast(ty, expr, *line)?,
            Expr::Binary { op, lhs, rhs } => self.binary(*op, lhs, rhs)?,
            Expr::Ternary { cond, then, els } => self.ternary(cond, then, els)?,
            // Println/PostIncDec in value position are handled as statements;
            // if one reaches here (nested), the print builtin already leaves its
            // `null` return value on the stack.
            Expr::Println { newline, err, arg } => {
                self.println(*newline, *err, arg.as_deref())?;
            }
            Expr::PostIncDec { name, inc } => {
                // Value position: push the old value, then apply the mutation.
                if self.implicit_this_field(name).is_some() {
                    self.emit_this(0);
                    self.emit_field_get(name, 0);
                } else if let Some((class, _)) = self.static_field_owner(name) {
                    self.emit_global_get(&static_global(&class, name), 0);
                } else {
                    self.emit_get(name, 0);
                }
                self.post_inc_dec(name, *inc)?;
            }
            Expr::PreIncDec { name, inc } => {
                // Value position: apply the mutation first, then read back the
                // new value — the only difference from `PostIncDec`.
                self.post_inc_dec(name, *inc)?;
                if self.implicit_this_field(name).is_some() {
                    self.emit_this(0);
                    self.emit_field_get(name, 0);
                } else if let Some((class, _)) = self.static_field_owner(name) {
                    self.emit_global_get(&static_global(&class, name), 0);
                } else {
                    self.emit_get(name, 0);
                }
            }
            Expr::Call { name, args, line } => self.call(name, args, *line)?,
            Expr::MethodCall {
                recv,
                method,
                args,
                line,
            } => self.method_call(recv, method, args, *line)?,
            Expr::SwitchExpr { disc, arms, line } => self.switch_expr(disc, arms, *line)?,
            Expr::Lambda { params, body, line } => self.compile_lambda(params, body, *line)?,
            Expr::MethodRef { recv, method, line } => {
                let lambda = self.desugar_method_ref(recv, method, *line)?;
                self.expr(&lambda)?;
            }
        }
        Ok(())
    }

    /// Lower an instance method call `recv.method(args...)`. Dispatch order:
    /// a bare stdlib class receiver (`Math.abs`) → the static-dispatch builtin;
    /// a user-class receiver (resolved by the receiver's static type) → a direct
    /// `Op::Call` to the class's mangled instance-method subroutine, with the
    /// receiver bound as `this` (frame slot 0); anything else → the `String`
    /// method-dispatch builtin.
    fn method_call(
        &mut self,
        recv: &Expr,
        method: &str,
        args: &[Expr],
        line: u32,
    ) -> Result<(), String> {
        // A call whose receiver is a bare capitalized class name (`Math.abs`,
        // `Integer.parseInt`, `String.valueOf`) is a static stdlib call: the
        // receiver is not a value, so it is not evaluated. Args, then the class
        // and method names, are handed to the static-dispatch builtin.
        // `x.getClass()` — javars has no `Class` object, so the call evaluates
        // to the receiver's runtime class *name*, and `Class`'s two accessors
        // (`getName`, `getSimpleName`) are String methods over it. A user class
        // that declares its own `getClass` is not shadowed, because Java forbids
        // overriding it.
        if method == "getClass" && args.is_empty() {
            self.expr(recv)?;
            self.b.emit(Op::CallBuiltin(crate::host::JCLASSOF, 1), line);
            return Ok(());
        }
        // A sort with no comparator orders by the elements' own `compareTo`,
        // which only a Java-level call can reach — the host's `natural_cmp`
        // knows numbers and strings and answers "equal" for everything else, so
        // a `List` of a user `Comparable` came back in its original order. The
        // missing comparator is supplied here instead, as the same
        // `(a, b) -> a.compareTo(b)` lambda `Comparator.naturalOrder()` is.
        if method == "sort" {
            let collections_sort = matches!(recv, Expr::Var(c) if c == "Collections")
                && !self.is_declared_var("Collections");
            let explicit_null = |e: &Expr| matches!(e, Expr::Var(n) if n == NULL_LITERAL);
            let natural = if collections_sort {
                match args {
                    [l] => Some(vec![l.clone(), natural_order_comparator(line)]),
                    [l, c] if explicit_null(c) => {
                        Some(vec![l.clone(), natural_order_comparator(line)])
                    }
                    _ => None,
                }
            } else {
                match args {
                    [c] if explicit_null(c) => Some(vec![natural_order_comparator(line)]),
                    _ => None,
                }
            };
            if let Some(args) = natural {
                return self.method_call(recv, method, &args, line);
            }
        }
        // `super.method(args)` is the one call that must NOT go through the
        // virtual dispatch chain: it names the superclass's implementation of a
        // method the receiver's own class overrides. It is intercepted here,
        // ahead of every receiver-typed branch below, because `expr_class` types
        // a `super` receiver as the superclass — which would otherwise dispatch
        // it virtually and call the override back on itself. (`getClass` is
        // above this on purpose: it is `final` in Java, so `super.getClass()` is
        // `this.getClass()`.)
        if matches!(recv, Expr::Var(n) if n == SUPER) {
            return self.super_call(method, args, line);
        }
        // A fully-qualified stdlib receiver (`java.util.Arrays.sort(x)`) names
        // exactly the class its simple name does — javars keys every type on the
        // simple name, so the package qualifier is dropped and the call re-enters
        // through the ordinary static path.
        if let Some(simple) = qualified_static_class(recv) {
            return self.method_call(&Expr::Var(simple), method, args, line);
        }
        // `Color.values()` / `Color.valueOf(s)` — the two statics every enum has.
        // They are compiler-generated because javars keeps no per-class static
        // method table.
        if let Some(class) = self.enum_type_ref(recv) {
            match (method, args.len()) {
                ("values", 0) => {
                    self.emit_enum_values(&class, line);
                    return Ok(());
                }
                ("valueOf", 1) => return self.emit_enum_value_of(&class, &args[0], line),
                _ => {
                    return Err(format!(
                        "javars: enum `{class}` has no static method `{method}` taking {} argument(s) (line {line})",
                        args.len()
                    ))
                }
            }
        }
        // `T.helper(x)` on a user class is a call to that class's `static`
        // method. It resolves in `T`'s own inheritance chain — NOT through the
        // flat by-name pool a bare `helper(x)` falls back to — so a same-named
        // static on an unrelated class can never be selected.
        if let Some(class) = self.user_class_ref(recv) {
            if self.methods.contains_key(method) {
                let arg_tys: Vec<Option<String>> =
                    args.iter().map(|a| self.expr_java_type(a)).collect();
                if let Some(resolved) = self.resolve_static_on(&class, method, &arg_tys) {
                    let args =
                        Self::effective_args(args, &resolved.param_tys, resolved.vararg_from);
                    self.call_args_targeted(&args, &resolved.param_tys)?;
                    let name_idx = self.b.add_name(&resolved.mangled);
                    self.b.emit(Op::Call(name_idx, args.len() as u8), line);
                    self.emit_exc_check(line);
                    return Ok(());
                }
                return Err(format!(
                    "javars: class `{class}` has no static method `{method}` taking {} argument(s) (line {line})",
                    args.len()
                ));
            }
            return Err(format!(
                "javars: class `{class}` has no static method `{method}` (line {line})"
            ));
        }
        if let Expr::Var(class) = recv {
            if is_static_class(class) && !self.is_declared_var(class) {
                // A static whose parameter renders the value as text takes the
                // `char`'s one-character String; `Math.max(c, 5)` and the
                // `Character` predicates take the code point unchanged.
                let stringify = takes_char_as_string(class, method);
                // `String.format` is the one static that treats its arguments
                // differently from each other: the slots a `%s` consumes want
                // `Float.toString`, the slots a `%f` consumes want the number.
                let text_slots = (class == "String" && method == "format")
                    .then(|| match args.first() {
                        Some(Expr::Str(fmt)) => Some(text_conversion_slots(fmt)),
                        // A non-literal format string cannot be scanned, so the
                        // numeric conversions are kept exact and `%s` of a
                        // `float` prints its `double` form (see BUGS.md).
                        _ => Some(Vec::new()),
                    })
                    .flatten();
                for (i, a) in args.iter().enumerate() {
                    let floats = match &text_slots {
                        Some(slots) => i > 0 && slots.contains(&(i - 1)),
                        None => stringify,
                    };
                    if stringify {
                        self.emit_converted_arg(a, floats)?;
                    } else {
                        self.expr(a)?;
                    }
                }
                // `java.util.Formatter` rejects a conversion whose argument is
                // the wrong boxed type, and only the compiler knows which box
                // each argument is — `Value::Int` is `Integer`, `Long`,
                // `Short`, `Byte` and `char` all at once. So `String.format`
                // gets its own builtin, taking the static types alongside the
                // values. An argument whose type javars could not infer sends
                // an empty tag and is classified from its runtime value.
                if text_slots.is_some() {
                    let tags: Vec<String> = args
                        .iter()
                        .skip(1)
                        .map(|a| self.expr_java_type(a).unwrap_or_default())
                        .collect();
                    let tags_c = self.b.add_constant(Value::str(tags.join("\x1f")));
                    self.b.emit(Op::LoadConst(tags_c), line);
                    self.emit_raising_builtin(crate::host::JFORMAT, args.len() as u8 + 1, line);
                    return Ok(());
                }
                let class_c = self.b.add_constant(Value::str(class.clone()));
                self.b.emit(Op::LoadConst(class_c), line);
                let method_c = self.b.add_constant(Value::str(method.to_string()));
                self.b.emit(Op::LoadConst(method_c), line);
                // argc counts the args plus the class-name and method-name strings.
                self.emit_raising_builtin(
                    crate::host::JSTATIC_DISPATCH,
                    args.len() as u8 + 2,
                    line,
                );
                // `Math.abs(int)` is the one `Math` overload that overflows:
                // `Math.abs(Integer.MIN_VALUE)` is `Integer.MIN_VALUE`, because
                // negating it does not fit an `int`. The host has no argument
                // width to work from, so the narrowing is emitted here.
                if class == "Math"
                    && method == "abs"
                    && args.len() == 1
                    && Self::is_int_width(self.expr_java_type(&args[0]).as_deref())
                {
                    self.emit_wrap32(line);
                }
                return Ok(());
            }
        }
        // A user-class receiver: dispatch on the receiver's runtime class
        // (virtual dispatch), collapsing to a direct call when not overridden.
        //
        // `equals`/`toString` a class does not declare are the ones it inherits
        // from `Object`, which has no Java-level body here — so they fall
        // through to the host, where they are reference identity and
        // `getClass().getName() + "@" + hex`. A class that declares either wins,
        // which is what keeps a `record`'s and an `enum`'s own versions in play.
        // `hashCode` is deliberately *not* in that set: a `record`'s is derived
        // from its components, and answering an identity hash there would be a
        // silently wrong number instead of the compile error it is today.
        if let Some(rc) = self.expr_class(recv) {
            let arg_tys: Vec<Option<String>> =
                args.iter().map(|a| self.expr_java_type(a)).collect();
            let inherited = matches!((method, args.len()), ("equals", 1) | ("toString", 0))
                && self.resolve_instance_call(&rc, method, &arg_tys).is_none();
            if !inherited {
                return self.dispatch_instance_method(recv, &rc, method, args, line);
            }
        }
        // A `java.util` collection receiver. The runtime path in `b_str_dispatch`
        // catches a collection whose static type javars could not determine
        // (an erased `Map.get` result), so this is a diagnostics-and-clarity
        // shortcut rather than the only route.
        if let Some(kind) = self
            .expr_java_type(recv)
            .as_deref()
            .and_then(collection_kind)
        {
            // `List` declares BOTH `remove(int)` and `remove(Object)`, and Java
            // picks between them from the argument's *static* type: an integral
            // primitive removes by index, anything else (a boxed `Integer`, a
            // `String`, an `Object`) removes the first equal element. Selecting
            // by index unconditionally silently removed the wrong element
            // (`l.remove(Integer.valueOf(20))` on `[10,20,30]` gave `[10,20]`
            // instead of `[10,30]`) or threw a bogus `IndexOutOfBoundsException`
            // when the value exceeded the size. The choice is a compile-time one
            // because it is a static-type question, so it is made here and the
            // by-value overload reaches the host under its own name.
            // An argument whose static type javars could not infer keeps the
            // by-index reading it has always had, so only a *known* reference
            // type switches overload. A boxed `Character` is a reference type in
            // Java and selects `remove(Object)` there too.
            let by_value = kind == "list"
                && method == "remove"
                && args.len() == 1
                && (is_boxing_call(&args[0])
                    || self
                        .expr_java_type(&args[0])
                        .is_some_and(|t| !matches!(t.as_str(), "int" | "short" | "byte" | "char")));
            let method = if by_value { "removeObject" } else { method };
            self.expr(recv)?;
            // A collection element is a *boxed* `Character`, which javars models
            // as the one-character String — so it prints and compares like Java's.
            for a in args {
                self.emit_char_string(a)?;
            }
            let name_c = self.b.add_constant(Value::str(method.to_string()));
            self.b.emit(Op::LoadConst(name_c), line);
            self.emit_raising_builtin(crate::host::JCOLL_DISPATCH, args.len() as u8 + 2, line);
            return Ok(());
        }
        self.emit_erased_call(recv, method, args, line)
    }

    /// Lower `recv.method(args)` where the receiver's static class is **not** a
    /// user class — a boxed primitive, a `String`, an erased `List.get` result,
    /// a lambda parameter, an `Object` upcast.
    ///
    /// Two things can be true of such a receiver at runtime that the static type
    /// does not say, and both used to be answered wrongly:
    ///
    ///   * It may be an instance of a user class. `Comparator.comparing(P::n)`
    ///     calls `n()` on a lambda parameter; `list.get(0).compareTo(x)` calls a
    ///     user `Comparable`'s own method. Both reached [`crate::host::JSTR_DISPATCH`]
    ///     and were answered by the `String` method of that name — a wrong
    ///     answer for `compareTo` (two `toString`s compared as text) and an
    ///     "unsupported String method" for everything else. So the receiver's
    ///     *runtime* class is read and matched against every concrete user class
    ///     declaring that name and arity, exactly as virtual dispatch does when
    ///     the static class is known.
    ///   * For `compareTo` specifically, the boxed type decides the answer, so
    ///     the receiver's static Java type rides along to
    ///     [`crate::host::JCOMPARE_TO`] as a tag.
    ///
    /// The `String` method remains the last arm, which is what keeps a genuine
    /// `String` receiver — and the collection and closure receivers
    /// `b_str_dispatch` resolves at runtime — behaving as before.
    fn emit_erased_call(
        &mut self,
        recv: &Expr,
        method: &str,
        args: &[Expr],
        line: u32,
    ) -> Result<(), String> {
        let tag = self.expr_java_type(recv).unwrap_or_default();
        let is_compare = method == "compareTo" && args.len() == 1;
        // A `char`/`Character` receiver compares as a code point, so `compareTo`
        // keeps its argument a number; every other call keeps the
        // one-character-String spelling the rest of the frontend uses.
        let raw_args = is_compare && matches!(tag.as_str(), "char" | "Character");
        // The concrete user classes declaring this name and arity, as
        // (runtime class, subroutine). A receiver the compiler already typed as
        // a boxed primitive or a `String` can never be one of them.
        let param_tys: Vec<String> = vec!["Object".to_string(); args.len()];
        // An erasure (`Object`, or no inferred type at all) is what leaves the
        // door open; a receiver already typed as a boxed primitive, a `String`
        // or a collection is never a user instance and needs no chain.
        let erased = !matches!(
            tag.as_str(),
            "int"
                | "long"
                | "short"
                | "byte"
                | "char"
                | "boolean"
                | "double"
                | "float"
                | "Integer"
                | "Long"
                | "Short"
                | "Byte"
                | "Character"
                | "Boolean"
                | "Double"
                | "Float"
                | "String"
                | "CharSequence"
        ) && collection_kind(&tag).is_none();
        let mut targets: Vec<(String, String)> = if erased {
            self.classes
                .iter()
                .filter(|(_, ci)| !ci.is_interface)
                .map(|(k, _)| k.clone())
                .filter_map(|k| {
                    self.resolve_instance_sig(&k, method, &param_tys)
                        .map(|(m, _)| (k, m))
                })
                .collect()
        } else {
            Vec::new()
        };
        targets.sort();
        // The last arm: `compareTo`'s typed comparison, or the `String` method.
        let fallback = |c: &mut Self| {
            if is_compare {
                let tag_c = c.b.add_constant(Value::str(tag.clone()));
                c.b.emit(Op::LoadConst(tag_c), line);
                c.emit_raising_builtin(crate::host::JCOMPARE_TO, 3, line);
            } else {
                let name_c = c.b.add_constant(Value::str(method.to_string()));
                c.b.emit(Op::LoadConst(name_c), line);
                // argc counts the receiver, the arguments, and the method name.
                c.emit_raising_builtin(crate::host::JSTR_DISPATCH, args.len() as u8 + 2, line);
            }
        };
        if targets.is_empty() {
            self.expr(recv)?;
            for a in args {
                if raw_args {
                    self.expr(a)?;
                } else {
                    self.emit_char_string(a)?;
                }
            }
            fallback(self);
            return Ok(());
        }
        // Evaluate receiver and arguments once, then branch on the runtime class.
        let recv_t = self.temp();
        self.expr(recv)?;
        self.emit_set(&recv_t, line);
        let arg_ts: Vec<String> = args
            .iter()
            .map(|a| {
                let t = self.temp();
                if raw_args {
                    self.expr(a)?;
                } else {
                    self.emit_char_string(a)?;
                }
                self.emit_set(&t, line);
                Ok(t)
            })
            .collect::<Result<_, String>>()?;
        let class_t = self.temp();
        self.emit_get(&recv_t, line);
        self.b.emit(Op::CallBuiltin(crate::host::JCLASSOF, 1), line);
        self.emit_set(&class_t, line);
        let argc = args.len() as u8 + 1;
        let mut end_jumps = Vec::new();
        for (class, mangled) in &targets {
            self.emit_get(&class_t, line);
            let cc = self.b.add_constant(Value::str(class.clone()));
            self.b.emit(Op::LoadConst(cc), line);
            self.b.emit(Op::StrEq, line);
            let skip = self.b.emit(Op::JumpIfFalse(0), line);
            self.emit_get(&recv_t, line);
            for t in &arg_ts {
                self.emit_get(t, line);
            }
            let idx = self.b.add_name(mangled);
            self.b.emit(Op::Call(idx, argc), line);
            self.emit_exc_check(line);
            end_jumps.push(self.b.emit(Op::Jump(0), line));
            let next = self.b.current_pos();
            self.b.patch_jump(skip, next);
        }
        // Not one of them: a `String`, a boxed primitive, a collection, or a
        // closure — all of which the fallback resolves.
        self.emit_get(&recv_t, line);
        for t in &arg_ts {
            self.emit_get(t, line);
        }
        fallback(self);
        let end = self.b.current_pos();
        for j in end_jumps {
            self.b.patch_jump(j, end);
        }
        Ok(())
    }

    /// Lower `super.method(args)` — a **non-virtual** call to the
    /// implementation the enclosing class inherits.
    ///
    /// Resolution starts at the *declaring* class's superclass, never at the
    /// receiver's runtime class, which is exactly what lets an override call the
    /// version it overrides (`String toString() { return "D[" +
    /// super.toString() + "]"; }` terminates instead of recursing). The body it
    /// selects is the one visible from the superclass, so a grandparent's method
    /// is reached when the parent does not declare it.
    ///
    /// Everything *inside* that body still dispatches virtually: a `super.m()`
    /// whose body calls an unqualified `n()` reaches the subclass's `n`, because
    /// only this one call site is de-virtualized — which is Java's rule.
    fn super_call(&mut self, method: &str, args: &[Expr], line: u32) -> Result<(), String> {
        let this_class = self.this_class.clone().ok_or_else(|| {
            format!("javars: `super` used outside an instance method (line {line})")
        })?;
        let arg_tys: Vec<Option<String>> = args.iter().map(|a| self.expr_java_type(a)).collect();
        if let Some(sup) = self.super_class() {
            if let Some(resolved) = self.resolve_instance_call(&sup, method, &arg_tys) {
                let param_tys = resolved.param_tys;
                let packed = Self::effective_args(args, &param_tys, resolved.vararg_from);
                let (mangled, _) = self
                    .resolve_instance_sig(&sup, method, &param_tys)
                    .ok_or_else(|| {
                        format!(
                            "javars: `{sup}` declares `{method}` but no superclass of \
                             `{this_class}` supplies a body for it (line {line})"
                        )
                    })?;
                self.emit_this(line); // this (deepest)
                self.call_args_targeted(&packed, &param_tys)?;
                let idx = self.b.add_name(&mangled);
                self.b.emit(Op::Call(idx, packed.len() as u8 + 1), line);
                self.emit_exc_check(line);
                return Ok(());
            }
        }
        // No user class up the chain declares it, so Java resolves the call to
        // `java.lang.Object`'s own member. `Object` has no Java-level body here
        // (see `new_object_as`), and the host supplies exactly the three
        // observable ones — reference-identity `equals`, the `Class@hash`
        // `toString`, and the identity `hashCode`.
        if matches!(
            (method, args.len()),
            ("toString", 0) | ("hashCode", 0) | ("equals", 1)
        ) {
            self.emit_this(line);
            for a in args {
                self.emit_char_string(a)?;
            }
            let name_c = self.b.add_constant(Value::str(method.to_string()));
            self.b.emit(Op::LoadConst(name_c), line);
            self.emit_raising_builtin(crate::host::JSTR_DISPATCH, args.len() as u8 + 2, line);
            return Ok(());
        }
        Err(format!(
            "javars: no superclass of `{this_class}` has a method `{method}` taking {} argument(s) (line {line})",
            args.len()
        ))
    }

    /// Emit `recv.field` read given the receiver value is already on the stack:
    /// push the field name and call the field-get builtin.
    fn emit_field_get(&mut self, name: &str, line: u32) {
        let name_c = self.b.add_constant(Value::str(name.to_string()));
        self.b.emit(Op::LoadConst(name_c), line);
        self.emit_raising_builtin(crate::host::JFIELD_GET, 2, line);
    }

    /// Lower `new ClassName(args...)`: allocate the instance, seed its fields
    /// (defaults then declared initializers, ancestors first), run the matching
    /// constructor, and leave the instance handle on the stack.
    fn new_object(&mut self, class: &str, args: &[Expr], line: u32) -> Result<(), String> {
        self.new_object_as(class, class, args, line)
    }

    /// Lower a construction whose *runtime class* and *constructor* differ.
    ///
    /// They coincide for `new C(…)`; an `enum` constant with a body is the one
    /// case they do not — the instance is of the constant's synthetic subclass
    /// (so its overrides dispatch) while the constructor that runs is the
    /// enum's, because Java gives an anonymous enum subclass none of its own.
    fn new_object_as(
        &mut self,
        class: &str,
        ctor_class: &str,
        args: &[Expr],
        line: u32,
    ) -> Result<(), String> {
        // `new String(cs)` / `new String(s)` / `new String()` — javars models a
        // `String` as a primitive value rather than an instance, so constructing
        // one is exactly the conversion `String.valueOf` performs.
        if !self.classes.contains_key(class) && class == "String" {
            match args.first() {
                // A `char[]` argument is a code-point array; `String.valueOf`
                // concatenates its *characters*, so convert element-wise first.
                Some(a) => self.emit_char_string(a)?,
                None => {
                    let empty = self.b.add_constant(Value::str(String::new()));
                    self.b.emit(Op::LoadConst(empty), line);
                }
            }
            let class_c = self.b.add_constant(Value::str("String".to_string()));
            self.b.emit(Op::LoadConst(class_c), line);
            let method_c = self.b.add_constant(Value::str("valueOf".to_string()));
            self.b.emit(Op::LoadConst(method_c), line);
            self.emit_raising_builtin(crate::host::JSTATIC_DISPATCH, 3, line);
            return Ok(());
        }
        // `new Object()` — the fieldless root instance programs use as a lock or
        // a sentinel. `Object` is deliberately *not* in the class table (it is
        // also the erasure of every type variable, and a receiver statically
        // typed `Object` has to keep dispatching dynamically), so the allocation
        // is emitted here directly: a `HostObj::Instance` with no fields and no
        // constructor, which the heap already gives a distinct identity.
        if !self.classes.contains_key(class) && class == "Object" {
            if !args.is_empty() {
                return Err(format!(
                    "javars: `Object` has only a no-argument constructor (line {line})"
                ));
            }
            let class_c = self.b.add_constant(Value::str("Object".to_string()));
            self.b.emit(Op::LoadConst(class_c), line);
            self.b.emit(Op::CallBuiltin(crate::host::JNEW, 1), line);
            return Ok(());
        }
        // `new ArrayList<>()` / `new HashMap<>(other)` — a `java.util`
        // collection, allocated by the host rather than laid out as an instance.
        // A user class of the same name wins, because `self.classes` is checked
        // by the branch below only after this one declines.
        if !self.classes.contains_key(class) && is_concrete_collection(class) {
            if args.len() > 1 {
                return Err(format!(
                    "javars: `new {class}(…)` takes no argument or one collection (line {line})"
                ));
            }
            let kind_c = self.b.add_constant(Value::str(class.to_string()));
            self.b.emit(Op::LoadConst(kind_c), line);
            match args.first() {
                // `new ArrayList<>(other)` copies; `new ArrayList<>(16)` is a
                // capacity hint with no observable effect, so an integral
                // argument seeds nothing.
                Some(a) if self.expr_java_type(a).as_deref() != Some("int") => self.expr(a)?,
                _ => {
                    self.b.emit(Op::LoadUndef, line);
                }
            }
            self.emit_raising_builtin(crate::host::JCOLL_NEW, 2, line);
            return Ok(());
        }
        let info = self
            .classes
            .get(class)
            .ok_or_else(|| format!("javars: unknown class `{class}` (line {line})"))?;
        if info.is_interface {
            return Err(format!(
                "javars: `{class}` is an interface and cannot be instantiated (line {line})"
            ));
        }
        // Constructor resolution up front (immutable borrow released before we
        // emit anything mutating `self`). The chosen overload's parameter types
        // mangle the `<init>` target.
        let ctor_info = self
            .classes
            .get(ctor_class)
            .ok_or_else(|| format!("javars: unknown class `{ctor_class}` (line {line})"))?;
        let has_any_ctor = !ctor_info.ctors.is_empty();
        let ctor_arities: Vec<usize> = ctor_info.ctors.iter().map(|c| c.param_tys.len()).collect();
        let info = &self.classes[class];
        // Field-init plan (name, type, optional init expr), cloned so the
        // ChunkBuilder borrow does not alias `self.classes`.
        let field_plan: Vec<(String, String, Option<Expr>)> = info
            .fields
            .iter()
            .map(|f| (f.name.clone(), f.ty.clone(), f.init.clone()))
            .collect();

        // Allocate: push class name, call JNEW → instance handle.
        let class_c = self.b.add_constant(Value::str(class.to_string()));
        self.b.emit(Op::LoadConst(class_c), line);
        self.b.emit(Op::CallBuiltin(crate::host::JNEW, 1), line);
        let obj = self.temp();
        self.emit_set(&obj, line);

        // Seed each field: default value, then its declared initializer if any.
        for (fname, fty, finit) in &field_plan {
            self.emit_get(&obj, line);
            let name_c = self.b.add_constant(Value::str(fname.clone()));
            self.b.emit(Op::LoadConst(name_c), line);
            match finit {
                Some(e) => self.expr_targeted(e, Some(fty))?,
                None => self.emit_type_default(fty, line),
            }
            self.emit_raising_builtin(crate::host::JFIELD_SET, 3, line);
            self.b.emit(Op::Pop, line);
        }

        // Run the constructor (resolving the overload by argument type). A class
        // with no declared ctor accepts only `new C()`.
        let arg_tys: Vec<Option<String>> = args.iter().map(|a| self.expr_java_type(a)).collect();
        let ctor_sig = self.resolve_ctor(ctor_class, &arg_tys);
        if let Some((param_tys, vararg_from)) = ctor_sig {
            let args = Self::effective_args(args, &param_tys, vararg_from);
            self.emit_get(&obj, line); // this
            self.call_args_targeted(&args, &param_tys)?;
            let mangled = mangle(ctor_class, "<init>", &param_tys);
            let name_idx = self.b.add_name(&mangled);
            self.b.emit(Op::Call(name_idx, args.len() as u8 + 1), line);
            self.emit_exc_check(line);
            self.b.emit(Op::Pop, line); // discard the ctor's (Undef) result
        } else if has_any_ctor || !args.is_empty() {
            return Err(format!(
                "javars: class `{ctor_class}` has no constructor taking {} argument(s) (declared arities: {:?}) (line {line})",
                args.len(),
                ctor_arities
            ));
        }

        // The expression value is the new instance.
        self.emit_get(&obj, line);
        Ok(())
    }

    /// Lower `a[i] <op>= v`. A plain assignment writes directly; a compound one
    /// reads the element, applies the operator, and writes it back — evaluating
    /// the array and index once (into temps) so their side effects don't repeat.
    fn index_assign(
        &mut self,
        array: &Expr,
        index: &Expr,
        op: AssignOp,
        value: &Expr,
        line: u32,
    ) -> Result<(), String> {
        // The element's declared type decides the assignment conversion, the
        // compound-`/` truncation, and the 32-bit wrap.
        let elem_ty = self
            .expr_array_type(array)
            .and_then(|t| t.strip_suffix("[]").map(str::to_string));
        if op == AssignOp::Assign {
            self.expr(array)?;
            self.expr(index)?;
            self.expr_targeted(value, elem_ty.as_deref())?;
            self.emit_raising_builtin(crate::host::JARRAY_SET, 3, line);
            self.b.emit(Op::Pop, line);
            return Ok(());
        }
        // Compound: stash array + index in temps, read old, combine, write back.
        let arr_t = self.temp();
        let idx_t = self.temp();
        self.expr(array)?;
        self.emit_set(&arr_t, line);
        self.expr(index)?;
        self.emit_set(&idx_t, line);
        // old element
        self.emit_get(&arr_t, line);
        self.emit_get(&idx_t, line);
        self.emit_raising_builtin(crate::host::JARRAY_GET, 2, line);
        let elem_t = elem_ty
            .as_deref()
            .map(|t| numtype_of_ty(t).unwrap_or(NumType::Other))
            .unwrap_or(NumType::Other);
        let wrap = self.compound_wraps(elem_ty.as_deref(), value);
        self.emit_compound(op, value, elem_t, elem_ty.as_deref(), wrap, line)?;
        self.emit_narrow_to(elem_ty.as_deref(), line);
        let new_t = self.temp();
        self.emit_set(&new_t, line);
        // write back
        self.emit_get(&arr_t, line);
        self.emit_get(&idx_t, line);
        self.emit_get(&new_t, line);
        self.emit_raising_builtin(crate::host::JARRAY_SET, 3, line);
        self.b.emit(Op::Pop, line);
        Ok(())
    }

    /// Lower `recv.field <op>= v` (and implicit `this.field`). Plain assignment
    /// writes directly; a compound one reads the field, applies the operator, and
    /// writes it back — evaluating the receiver once (into a temp).
    fn field_assign(
        &mut self,
        recv: &Expr,
        name: &str,
        op: AssignOp,
        value: &Expr,
        line: u32,
    ) -> Result<(), String> {
        // `T.n = …` (or a bare `n` naming a static) writes the class's shared
        // cell, not a field of an object.
        if let Some((class, ty)) = self.static_target(recv, name) {
            return self.static_assign(&class, &ty, name, op, value, line);
        }
        // The field's declared type drives both the compound-`/` truncation and
        // the 32-bit wrap, so it is resolved here rather than at each call site.
        let field_ty_name = self.field_type_name(recv, name);
        let field_ty = field_ty_name
            .as_deref()
            .and_then(numtype_of_ty)
            .unwrap_or(NumType::Other);
        let wrap = self.compound_wraps(field_ty_name.as_deref(), value);
        if op == AssignOp::Assign {
            self.expr(recv)?;
            let name_c = self.b.add_constant(Value::str(name.to_string()));
            self.b.emit(Op::LoadConst(name_c), line);
            self.expr_targeted(value, field_ty_name.as_deref())?;
            self.emit_raising_builtin(crate::host::JFIELD_SET, 3, line);
            self.b.emit(Op::Pop, line);
            return Ok(());
        }
        let obj_t = self.temp();
        self.expr(recv)?;
        self.emit_set(&obj_t, line);
        // old field value
        self.emit_get(&obj_t, line);
        self.emit_field_get(name, line);
        self.emit_compound(op, value, field_ty, field_ty_name.as_deref(), wrap, line)?;
        self.emit_narrow_to(field_ty_name.as_deref(), line);
        let new_t = self.temp();
        self.emit_set(&new_t, line);
        // write back
        self.emit_get(&obj_t, line);
        let name_c = self.b.add_constant(Value::str(name.to_string()));
        self.b.emit(Op::LoadConst(name_c), line);
        self.emit_get(&new_t, line);
        self.emit_raising_builtin(crate::host::JFIELD_SET, 3, line);
        self.b.emit(Op::Pop, line);
        Ok(())
    }

    /// Combine an already-pushed left operand with `value` under a compound
    /// operator, leaving the result on the stack. `/=` truncates when both the
    /// target and the value are statically integral (Java `int /= int`).
    fn emit_compound(
        &mut self,
        op: AssignOp,
        value: &Expr,
        target: NumType,
        target_ty: Option<&str>,
        wrap32: bool,
        line: u32,
    ) -> Result<(), String> {
        // `float f; f *= x;` is one 32-bit operation, not a 64-bit one narrowed
        // afterwards — the same reason the binary path routes through the host.
        if target_ty == Some("float") {
            if let Some(bop) = compound_binop(op) {
                self.expr(value)?;
                self.emit_f32_arith(bop, line);
                return Ok(());
            }
        }
        if matches!(op, AssignOp::Shl | AssignOp::Shr | AssignOp::Ushr) {
            // Same width rule as the binary shifts: the distance is masked to
            // the *target's* width, and `>>>` zero-fills at it. `wrap32` is
            // exactly "the target is `int`-wide", and the shared narrowing
            // below finishes the job.
            self.expr(value)?;
            self.b.emit(Op::LoadInt(if wrap32 { 31 } else { 63 }), line);
            self.b.emit(Op::BitAnd, line);
            match op {
                AssignOp::Shl => self.b.emit(Op::Shl, line),
                AssignOp::Shr => self.b.emit(Op::Shr, line),
                _ => {
                    self.b.emit(Op::LoadInt(if wrap32 { 32 } else { 64 }), line);
                    self.b.emit(Op::CallBuiltin(crate::host::JUSHR, 3), line)
                }
            };
        } else if matches!(op, AssignOp::BitAnd | AssignOp::BitOr | AssignOp::BitXor)
            && self.expr_type(value) != NumType::Int
        {
            // `b &= c` on booleans is the logical form, and its result must
            // stay a boolean rather than the 0/1 an integer op would leave.
            self.expr(value)?;
            let vop = match op {
                AssignOp::BitAnd => Op::LogAnd,
                AssignOp::BitOr => Op::LogOr,
                _ => Op::NumNe,
            };
            self.b.emit(vop, line);
        } else if op == AssignOp::Div {
            let r = self.expr_type(value);
            self.expr(value)?;
            self.emit_div(target, r, value, line);
        } else {
            // `s += x` on a String target is concatenation, so the operand takes
            // Java's string conversion; a numeric target makes it arithmetic,
            // which is what keeps `int n = 0; n += c;` adding code points.
            if op == AssignOp::Add && target == NumType::Other {
                self.emit_stringified(value)?;
            } else {
                self.expr(value)?;
            }
            // `x %= 0` throws the same `ArithmeticException` as `x / 0`.
            if op == AssignOp::Mod
                && target == NumType::Int
                && self.expr_type(value) == NumType::Int
            {
                self.emit_zero_divisor_check(value, line);
            }
            self.b.emit(compound_op(op), line);
        }
        if wrap32 {
            self.emit_wrap32(line);
        }
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
        // `super(args)` — chain to the superclass constructor. Instance fields
        // (including inherited ones) are already seeded by `new_object`, so this
        // just runs the parent constructor body on the same `this`.
        if name == "super" {
            if let Some(this_class) = self.this_class.clone() {
                if let Some(sup) = self
                    .classes
                    .get(&this_class)
                    .and_then(|ci| ci.superclass.clone())
                {
                    let arg_tys: Vec<Option<String>> =
                        args.iter().map(|a| self.expr_java_type(a)).collect();
                    if let Some((param_tys, vararg_from)) = self.resolve_ctor(&sup, &arg_tys) {
                        let args = Self::effective_args(args, &param_tys, vararg_from);
                        self.emit_this(line); // this
                        self.call_args_targeted(&args, &param_tys)?;
                        let mangled = mangle(&sup, "<init>", &param_tys);
                        let idx = self.b.add_name(&mangled);
                        self.b.emit(Op::Call(idx, args.len() as u8 + 1), line);
                        self.emit_exc_check(line);
                        return Ok(());
                    }
                }
            }
            // No user-class superclass constructor for this arity — a no-op that
            // still leaves a (discarded) value for statement position.
            self.b.emit(Op::LoadUndef, line);
            return Ok(());
        }
        // A user-defined static method resolves to the native call-frame ABI,
        // choosing the overload that matches the argument types.
        if self.methods.contains_key(name) {
            let arg_tys: Vec<Option<String>> =
                args.iter().map(|a| self.expr_java_type(a)).collect();
            let resolved = self.resolve_static_call(name, &arg_tys).ok_or_else(|| {
                format!(
                    "javars: no `{name}` overload matches {} argument(s) (line {line})",
                    args.len()
                )
            })?;
            let args = Self::effective_args(args, &resolved.param_tys, resolved.vararg_from);
            self.call_args_targeted(&args, &resolved.param_tys)?;
            let name_idx = self.b.add_name(&resolved.mangled);
            self.b.emit(Op::Call(name_idx, args.len() as u8), line);
            self.emit_exc_check(line);
            return Ok(());
        }
        // A bare call inside an instance method/ctor that names an instance
        // method of `this` is an implicit `this.name(args)` — dispatched
        // virtually on `this`'s runtime class.
        if let Some(this_class) = self.this_class.clone() {
            if self.has_instance_method(&this_class, name, args.len()) {
                return self.dispatch_instance_method(&Expr::This, &this_class, name, args, line);
            }
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

    /// Lower `cond ? then : els`. Evaluates `cond`, jumps to the `els` branch
    /// when false, and leaves exactly one branch's value on the stack.
    fn ternary(&mut self, cond: &Expr, then: &Expr, els: &Expr) -> Result<(), String> {
        // A floating conditional widens whichever branch is integral, so
        // `flag ? 1 : 2.0` yields 1.0 rather than 1 (JLS 15.25).
        let promote = self.ternary_promotion(then, els);
        self.expr(cond)?;
        let jf = self.b.emit(Op::JumpIfFalse(0), 0);
        self.expr_targeted(then, promote)?;
        let jend = self.b.emit(Op::Jump(0), 0);
        let else_start = self.b.current_pos();
        self.b.patch_jump(jf, else_start);
        self.expr_targeted(els, promote)?;
        let end = self.b.current_pos();
        self.b.patch_jump(jend, end);
        Ok(())
    }

    /// Lower an array literal, each element lowered as an assignment into a slot
    /// of the array's element type — which is what widens a `double[]`
    /// literal's integral elements and boxes a `char` into an `Object[]`.
    /// `elem_ty` is `None` for a bare `{…}` in a position javars cannot type,
    /// in which case the elements lower untargeted.
    fn array_lit(&mut self, elems: &[Expr], elem_ty: Option<&str>) -> Result<(), String> {
        for el in elems {
            self.expr_targeted(el, elem_ty)?;
        }
        self.b.emit(
            Op::CallBuiltin(crate::host::JARRAY_LIT, elems.len() as u8),
            0,
        );
        Ok(())
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
        // Shifts and the bitwise trio need Java's operand widths, which the
        // generic path below has no way to express.
        match op {
            BinOp::Shl | BinOp::Shr | BinOp::Ushr => return self.shift(op, lhs, rhs),
            BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => return self.bitwise(op, lhs, rhs),
            _ => {}
        }
        // A `float`-typed arithmetic operation runs at 32-bit width throughout.
        if matches!(
            op,
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod
        ) && self.arith_result_type(lhs, rhs) == Some("float")
            && !(op == BinOp::Add && self.is_string_concat(lhs, rhs))
        {
            self.expr(lhs)?;
            self.expr(rhs)?;
            self.emit_f32_arith(op, 0);
            return Ok(());
        }
        // `/` truncation is decided from the operands' static types.
        if let BinOp::Div = op {
            let l = self.expr_type(lhs);
            let r = self.expr_type(rhs);
            let wrap = self.operands_are_int(lhs, rhs);
            self.expr(lhs)?;
            self.expr(rhs)?;
            self.emit_div(l, r, rhs, 0);
            // `Integer.MIN_VALUE / -1` is the one division that overflows.
            if wrap {
                self.emit_wrap32(0);
            }
            return Ok(());
        }
        // `+` with a String or class-typed operand is string concatenation, and
        // Java concatenation applies its string conversion to the other operand
        // — the operand's `toString()` for an object, the one-character String
        // for a `char`. Arithmetic `+` (which is what `'a' + 1` is) must not.
        if op == BinOp::Add && self.is_string_concat(lhs, rhs) {
            self.emit_stringified(lhs)?;
            self.emit_stringified(rhs)?;
        } else {
            self.expr(lhs)?;
            self.expr(rhs)?;
        }
        // Integral `%` by zero throws `ArithmeticException` in Java, exactly as
        // `/` does; the floating `%` yields NaN and needs no check.
        if op == BinOp::Mod
            && self.expr_type(lhs) == NumType::Int
            && self.expr_type(rhs) == NumType::Int
        {
            self.emit_zero_divisor_check(rhs, 0);
        }
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
            BinOp::BitAnd
            | BinOp::BitOr
            | BinOp::BitXor
            | BinOp::Shl
            | BinOp::Shr
            | BinOp::Ushr => unreachable!("handled above"),
        };
        self.b.emit(vop, 0);
        // An `int` arithmetic result wraps at 32 bits. Comparisons yield a
        // boolean and `+` on a String concatenates, so neither wraps.
        if matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Mod)
            && self.operands_are_int(lhs, rhs)
        {
            self.emit_wrap32(0);
        }
        Ok(())
    }

    /// `Integer.MAX_VALUE` / `Double.NaN` / `Math.PI` / … — the value and Java
    /// type of a `static final` constant on a `java.lang` type, when `e` names
    /// one. A user class of the same name shadows it.
    fn wrapper_constant_ref(&self, e: &Expr) -> Option<(Value, &'static str)> {
        let Expr::Field { recv, name } = e else {
            return None;
        };
        let Expr::Var(class) = &**recv else {
            return None;
        };
        if self.classes.contains_key(class) {
            return None;
        }
        wrapper_constant(class, name)
    }

    /// Lower `&`, `|`, `^`.
    ///
    /// On integral operands these are the bitwise operators and fusevm's native
    /// ops match exactly (both sides are already inside their Java width, and
    /// `&`/`|`/`^` cannot widen a value). On `boolean` operands they are Java's
    /// *non-short-circuiting* logical operators, and the result has to stay a
    /// boolean rather than the 0/1 an integer op would leave.
    fn bitwise(&mut self, op: BinOp, lhs: &Expr, rhs: &Expr) -> Result<(), String> {
        let boolean = self.expr_java_type(lhs).as_deref() == Some("boolean")
            || self.expr_java_type(rhs).as_deref() == Some("boolean");
        self.expr(lhs)?;
        self.expr(rhs)?;
        let vop = match (op, boolean) {
            (BinOp::BitAnd, true) => Op::LogAnd,
            (BinOp::BitOr, true) => Op::LogOr,
            // `^` on two booleans is exactly "they differ".
            (_, true) => Op::NumNe,
            (BinOp::BitAnd, false) => Op::BitAnd,
            (BinOp::BitOr, false) => Op::BitOr,
            (_, false) => Op::BitXor,
        };
        self.b.emit(vop, 0);
        Ok(())
    }

    /// Lower `<<`, `>>`, `>>>`.
    ///
    /// Java masks the shift distance to the width of the *left* operand — 5 bits
    /// for `int` (so `1 << 33` is `1 << 1`), 6 for `long` — and promotes only
    /// that operand, so `1 << 2L` is still an `int`. fusevm's `Shl`/`Shr` always
    /// mask to 6 bits and work on 64 bits, so the mask is emitted explicitly and
    /// an `int` result is narrowed afterwards. `>>>` zero-fills at the operand's
    /// width, which no fusevm op carries, so it routes through the host.
    fn shift(&mut self, op: BinOp, lhs: &Expr, rhs: &Expr) -> Result<(), String> {
        let long = self.expr_java_type(lhs).as_deref() == Some("long");
        self.expr(lhs)?;
        self.expr(rhs)?;
        self.b.emit(Op::LoadInt(if long { 63 } else { 31 }), 0);
        self.b.emit(Op::BitAnd, 0);
        if op == BinOp::Ushr {
            self.b.emit(Op::LoadInt(if long { 64 } else { 32 }), 0);
            self.b.emit(Op::CallBuiltin(crate::host::JUSHR, 3), 0);
            return Ok(());
        }
        self.b
            .emit(if op == BinOp::Shl { Op::Shl } else { Op::Shr }, 0);
        if !long {
            self.emit_wrap32(0);
        }
        Ok(())
    }

    /// Lower `(ty) expr`.
    ///
    /// Java's *narrowing* primitive conversions are real value changes —
    /// `(int) 3.9` is 3, `(byte) 200` is -56, `(int) 1e18` saturates to
    /// `Integer.MAX_VALUE` — so those route through the host, which applies the
    /// conversion at the right width. A widening or identity cast between types
    /// javars already represents identically is emitted as the operand alone, so
    /// the common `(int) i` stays native. A *reference* cast has no runtime
    /// effect here: the host heap already carries each object's class and
    /// javars does not box primitives, so it changes no representation — but it
    /// is still *checked*, and a cast the runtime class does not satisfy throws
    /// `ClassCastException` the way Java's does.
    fn cast(&mut self, ty: &str, e: &Expr, line: u32) -> Result<(), String> {
        let src = self.expr_java_type(e);
        let identity = matches!(
            (ty, src.as_deref()),
            ("int", Some("int" | "short" | "byte"))
                | ("long", Some("int" | "long" | "short" | "byte"))
                | ("double" | "float", Some("double" | "float"))
                | ("boolean", Some("boolean"))
        );
        let primitive = matches!(
            ty,
            "int" | "long" | "short" | "byte" | "char" | "float" | "double" | "boolean"
        );
        if !primitive {
            return self.reference_cast(ty, e, line);
        }
        if identity {
            return self.expr(e);
        }
        self.expr(e)?;
        let c = self.b.add_constant(Value::str(ty.to_string()));
        self.b.emit(Op::LoadConst(c), line);
        self.b.emit(Op::CallBuiltin(crate::host::JCAST, 2), line);
        Ok(())
    }

    /// Lower `(RefType) expr` — a *checked* reference cast.
    ///
    /// The cast changes no representation (the host heap already carries each
    /// object's class), so all it can do is verify one. It is emitted when the
    /// target is a user class or interface, or a JDK type javars models — which
    /// is [`crate::host::is_checkable_cast_target`], the same list the host
    /// decides the check against, rather than a second and narrower copy kept
    /// here. `Object` is always satisfied, and an unknown name — a type
    /// *variable* after erasure, an array type — is passed through rather than
    /// checked, because a check javars cannot decide must not invent a failure.
    fn reference_cast(&mut self, ty: &str, e: &Expr, line: u32) -> Result<(), String> {
        self.expr(e)?;
        let checkable = self.classes.contains_key(ty) || crate::host::is_checkable_cast_target(ty);
        if !checkable {
            return Ok(());
        }
        let c = self.b.add_constant(Value::str(ty.to_string()));
        self.b.emit(Op::LoadConst(c), line);
        self.emit_raising_builtin(crate::host::JCHECKCAST, 2, line);
        Ok(())
    }

    /// Emit a division of two already-pushed operands. Java `/` divides as
    /// floating point (fusevm's `Op::Div`) and truncates toward zero to an
    /// integer only when both operands are statically integral — reproduced
    /// with a trailing `Op::TruncInt`.
    /// Lower `/`.
    ///
    /// Statically-integral division keeps the native op pair so the JIT can
    /// trace it. Anything else — a floating operand, or an operand whose type is
    /// not statically known — routes through the `JDIV` builtin, because Java
    /// floating division is IEEE-754: `x / 0.0` is a signed infinity and
    /// `0.0 / 0.0` is NaN, where the native op yields `Undef`.
    fn emit_div(&mut self, l: NumType, r: NumType, divisor: &Expr, line: u32) {
        if l == NumType::Int && r == NumType::Int {
            self.emit_zero_divisor_check(divisor, line);
            self.b.emit(Op::Div, line);
            self.b.emit(Op::TruncInt, line);
        } else {
            self.b.emit(Op::CallBuiltin(crate::host::JDIV, 2), line);
        }
    }

    /// Emit Java's integral division-by-zero check for the divisor already on
    /// top of the stack: `int / 0` and `int % 0` throw `ArithmeticException`,
    /// where fusevm's native `Div`/`Mod` (shell/awk flavoured, with no
    /// infinities) yield `Undef`. The floating path needs no check — IEEE-754
    /// division by zero is an infinity, which `JDIV` already produces.
    ///
    /// A literal non-zero divisor is checked at compile time and emits nothing,
    /// so `x / 2` and every constant-divisor loop keep the bare native op pair
    /// and stay JIT-traceable.
    fn emit_zero_divisor_check(&mut self, divisor: &Expr, line: u32) {
        if let Expr::Int(n) = divisor {
            if *n != 0 {
                return;
            }
        }
        self.b.emit(Op::Dup, line);
        self.b.emit(Op::LoadInt(0), line);
        self.b.emit(Op::NumEq, line);
        let jf = self.b.emit(Op::JumpIfFalse(0), line);
        // The unwind abandons both operands; the handler's `JEXC_CUT` (or the
        // frame's `ReturnValue`) drops them.
        self.emit_fault("ArithmeticException", "/ by zero", line);
        let after = self.b.current_pos();
        self.b.patch_jump(jf, after);
    }
}

// ── FFI detection (does the program contain a `rust { ... }` block?) ────────

/// True if any statement in `body` (recursively) evaluates a `__rust_compile`
/// call — the desugar target of an inline `rust { ... }` block.
fn body_has_ffi(body: &[Stmt]) -> bool {
    body.iter().any(|s| match &s.kind {
        StmtKind::Local { init, .. } => init.as_ref().is_some_and(expr_has_ffi),
        StmtKind::Locals(decls) => body_has_ffi(decls),
        StmtKind::Assign { value, .. } => expr_has_ffi(value),
        StmtKind::IndexAssign {
            array,
            index,
            value,
            ..
        } => expr_has_ffi(array) || expr_has_ffi(index) || expr_has_ffi(value),
        StmtKind::FieldAssign { recv, value, .. } => expr_has_ffi(recv) || expr_has_ffi(value),
        StmtKind::Expr(e) => expr_has_ffi(e),
        StmtKind::If { cond, then, els } => {
            expr_has_ffi(cond) || body_has_ffi(then) || body_has_ffi(els)
        }
        StmtKind::While { cond, body } => expr_has_ffi(cond) || body_has_ffi(body),
        StmtKind::DoWhile { body, cond } => body_has_ffi(body) || expr_has_ffi(cond),
        StmtKind::For {
            init,
            cond,
            update,
            body,
        } => {
            body_has_ffi(init)
                || cond.as_ref().is_some_and(expr_has_ffi)
                || body_has_ffi(update)
                || body_has_ffi(body)
        }
        StmtKind::ForEach { iter, body, .. } => expr_has_ffi(iter) || body_has_ffi(body),
        StmtKind::Switch { disc, groups } => {
            expr_has_ffi(disc)
                || groups
                    .iter()
                    .any(|g| g.labels.iter().any(expr_has_ffi) || body_has_ffi(&g.body))
        }
        StmtKind::Labeled { body, .. } => body_has_ffi(std::slice::from_ref(body)),
        StmtKind::Return(val) => val.as_ref().is_some_and(expr_has_ffi),
        StmtKind::Try {
            body,
            catches,
            finally_body,
        } => {
            body_has_ffi(body)
                || catches.iter().any(|c| body_has_ffi(&c.body))
                || body_has_ffi(finally_body)
        }
        StmtKind::Throw(e) | StmtKind::Yield(e) => expr_has_ffi(e),
        StmtKind::Break(_) | StmtKind::Continue(_) => false,
    })
}

fn expr_has_ffi(e: &Expr) -> bool {
    match e {
        Expr::Call { name, args, .. } => name == RUST_COMPILE || args.iter().any(expr_has_ffi),
        Expr::Unary { rhs, .. } => expr_has_ffi(rhs),
        Expr::Cast { expr, .. } => expr_has_ffi(expr),
        Expr::PreIncDec { .. } => false,
        Expr::Binary { lhs, rhs, .. } => expr_has_ffi(lhs) || expr_has_ffi(rhs),
        Expr::Ternary { cond, then, els } => {
            expr_has_ffi(cond) || expr_has_ffi(then) || expr_has_ffi(els)
        }
        Expr::Println { arg, .. } => arg.as_deref().is_some_and(expr_has_ffi),
        Expr::MethodCall { recv, args, .. } => expr_has_ffi(recv) || args.iter().any(expr_has_ffi),
        Expr::NewArray { sizes, .. } => sizes.iter().any(expr_has_ffi),
        Expr::ArrayLit { elems, .. } => elems.iter().any(expr_has_ffi),
        Expr::Index { array, index } => expr_has_ffi(array) || expr_has_ffi(index),
        Expr::Field { recv, .. } => expr_has_ffi(recv),
        Expr::NewObject { args, .. } => args.iter().any(expr_has_ffi),
        Expr::InstanceOf { expr, .. } => expr_has_ffi(expr),
        Expr::Lambda { body, .. } => match body {
            LambdaBody::Expr(e) => expr_has_ffi(e),
            LambdaBody::Block(b) => body_has_ffi(b),
        },
        Expr::MethodRef { recv, .. } => expr_has_ffi(recv),
        Expr::SwitchExpr { disc, arms, .. } => {
            expr_has_ffi(disc)
                || arms.iter().any(|a| {
                    a.labels.iter().any(expr_has_ffi)
                        || match &a.body {
                            SwitchArmBody::Expr(e) => expr_has_ffi(e),
                            SwitchArmBody::Block(b) => body_has_ffi(b),
                        }
                })
        }
        Expr::Int(_)
        | Expr::Long(_)
        | Expr::Float(_)
        | Expr::Float32(_)
        | Expr::Str(_)
        | Expr::Char(_)
        | Expr::Bool(_)
        | Expr::Var(_)
        | Expr::This
        | Expr::PostIncDec { .. } => false,
    }
}

/// The mangled subroutine name for a class member: `Class#member#ty1,ty2`. `#`
/// is not a legal Java identifier char, so mangled names never collide with a
/// user-declared static method. Including the parameter types disambiguates
/// same-name/same-arity overloads. Constructors use the member name `<init>`.
fn mangle(class: &str, member: &str, param_tys: &[String]) -> String {
    format!("{class}#{member}#{}", param_tys.join(","))
}

/// The mangled subroutine name for a top-level user `static` method, including
/// its parameter types so overloads get distinct subroutines. Prefixed with `#`
/// so it never collides with an instance mangle or a user identifier.
fn mangle_static(owner: &str, name: &str, param_tys: &[String]) -> String {
    format!("#s#{owner}#{name}#{}", param_tys.join(","))
}

/// The numeric widening rank of a primitive type (`byte` < `short`/`char` <
/// `int` < `long` < `float` < `double`); `None` for non-numeric types. Used by
/// overload resolution to score widening conversions.
fn numeric_rank(ty: &str) -> Option<u32> {
    Some(match ty {
        "byte" => 1,
        "short" | "char" | "Character" => 2,
        "int" => 3,
        "long" => 4,
        "float" => 5,
        "double" => 6,
        _ => return None,
    })
}

/// The type name for a numeric rank, promoted to at least `int` (Java's binary
/// numeric promotion never yields a sub-`int` result).
fn rank_name(rank: u32) -> &'static str {
    match rank.max(3) {
        3 => "int",
        4 => "long",
        5 => "float",
        _ => "double",
    }
}

/// True when `ty` is a reference type (a class, interface, array, or `String`)
/// rather than a primitive or `void`.
/// A primitive type's wrapper class, or `None` when `ty` is not a primitive.
/// `void` has no boxing conversion, so it is deliberately absent.
fn wrapper_of(ty: &str) -> Option<&'static str> {
    Some(match ty {
        "int" => "Integer",
        "long" => "Long",
        "double" => "Double",
        "float" => "Float",
        "short" => "Short",
        "byte" => "Byte",
        "char" => "Character",
        "boolean" => "Boolean",
        _ => return None,
    })
}

fn is_reference_type(ty: &str) -> bool {
    !matches!(
        ty,
        "int" | "long" | "short" | "byte" | "char" | "float" | "double" | "boolean" | "void"
    )
}

/// Register a subroutine's formal parameters into its scope: allocate slots in
/// declaration order and record each parameter's numeric type, raw declared
/// type, and its status as a true local (so a same-named field is shadowed).
fn register_params(scope: &mut MethodScope, params: &[Param]) {
    for p in params {
        scope.slot(&p.name);
        scope.types.insert(
            p.name.clone(),
            numtype_of_ty(&p.ty).unwrap_or(NumType::Other),
        );
        scope.decl_types.insert(p.name.clone(), p.ty.clone());
        scope.declared.insert(p.name.clone());
    }
}

/// The numeric category of an array's elements from its declared type string:
/// `int[]`/`long[]`/… → int, `double[]`/`float[]` → float, else non-numeric.
fn array_elem_numtype(array_ty: &str) -> NumType {
    match array_ty.strip_suffix("[]") {
        Some("int" | "long" | "short" | "byte" | "char") => NumType::Int,
        Some("double" | "float") => NumType::Float,
        _ => NumType::Other,
    }
}

/// True when `name` is a stdlib class whose methods javars dispatches
/// statically (rather than treating `name` as a value/receiver).
/// The simple name of a fully-qualified stdlib class reference
/// (`java.util.Arrays` → `Arrays`), when `e` is a dotted chain rooted at a
/// package name.
///
/// Only `java`/`javax` roots qualify, so an ordinary field access on a user
/// expression can never be mistaken for a package path.
fn qualified_static_class(e: &Expr) -> Option<String> {
    fn is_package(e: &Expr) -> bool {
        match e {
            Expr::Var(v) => v == "java" || v == "javax",
            Expr::Field { recv, .. } => is_package(recv),
            _ => false,
        }
    }
    let Expr::Field { recv, name } = e else {
        return None;
    };
    (is_package(recv) && is_static_class(name)).then(|| name.clone())
}

fn is_static_class(name: &str) -> bool {
    matches!(
        name,
        "Math"
            | "Integer"
            | "Long"
            | "Double"
            | "Float"
            | "Boolean"
            | "String"
            | "Character"
            | "Arrays"
            | "Collections"
            | "List"
            | "Set"
    )
}

/// The binary operator a compound assignment applies, or `None` for the ones
/// whose lowering is not a plain binary op (the shifts, and `=` itself).
fn compound_binop(op: AssignOp) -> Option<BinOp> {
    Some(match op {
        AssignOp::Add => BinOp::Add,
        AssignOp::Sub => BinOp::Sub,
        AssignOp::Mul => BinOp::Mul,
        AssignOp::Div => BinOp::Div,
        AssignOp::Mod => BinOp::Mod,
        _ => return None,
    })
}

/// The argument positions (0-based, counted after the format string) a format
/// string consumes with a *text* conversion — `%s`, `%S`, `%h`, `%H`, `%b`,
/// `%B`. Those are the ones a `float` reaches as `Float.toString`; every other
/// conversion widens it to a `double` the way Java does.
///
/// Explicit argument indexes (`%2$s`) select a slot without advancing the
/// implicit cursor, and `%%`/`%n` consume no argument at all.
fn text_conversion_slots(fmt: &str) -> Vec<usize> {
    let cs: Vec<char> = fmt.chars().collect();
    let mut out = Vec::new();
    let mut next = 0usize;
    let mut i = 0usize;
    while i < cs.len() {
        if cs[i] != '%' {
            i += 1;
            continue;
        }
        i += 1;
        // `%<n>$` — an explicit, 1-based argument index.
        let start = i;
        while i < cs.len() && cs[i].is_ascii_digit() {
            i += 1;
        }
        let explicit = if i < cs.len() && cs[i] == '$' && i > start {
            let n: usize = cs[start..i].iter().collect::<String>().parse().unwrap_or(1);
            i += 1;
            Some(n.saturating_sub(1))
        } else {
            i = start;
            None
        };
        // flags, width, `.precision`
        while i < cs.len() && "-+ 0,(#".contains(cs[i]) {
            i += 1;
        }
        while i < cs.len() && (cs[i].is_ascii_digit() || cs[i] == '.') {
            i += 1;
        }
        let Some(&conv) = cs.get(i) else { break };
        i += 1;
        if conv == '%' || conv == 'n' {
            continue;
        }
        let slot = explicit.unwrap_or_else(|| {
            let s = next;
            next += 1;
            s
        });
        if matches!(conv, 's' | 'S' | 'h' | 'H' | 'b' | 'B') {
            out.push(slot);
        }
    }
    out
}

/// True when a stdlib static renders a `char` argument as text, so the code
/// point has to be converted to its one-character String first. `String`'s
/// statics all do (`valueOf`, `format`, `join`); `Arrays`'s rendering pair does,
/// which is what makes `Arrays.toString(s.toCharArray())` print `[a, b, c]`;
/// `List.of`/`Set.of` box their elements. `Math` and the `Character` predicates
/// take the code point itself and are deliberately absent.
fn takes_char_as_string(class: &str, method: &str) -> bool {
    match class {
        "String" | "List" | "Set" | "Collections" => true,
        "Arrays" => matches!(method, "toString" | "deepToString" | "asList"),
        _ => false,
    }
}

/// The declared Java return type of a known stdlib static call, or `None` when
/// javars does not model it statically. `Math.abs`/`max`/`min` are deliberately
/// absent: their result is `int` or `double` depending on the argument, and
/// claiming either would mis-type the other. Feeds overload resolution and the
/// 32-bit `int` wrap decision.
fn static_call_java_type(class: &str, method: &str) -> Option<&'static str> {
    Some(match (class, method) {
        ("Integer", "parseInt") | ("Integer", "valueOf") => "int",
        // `Long.parseLong` and `Math.round` are 64-bit results in Java, so they
        // must NOT be treated as `int` — that is exactly the case where the
        // wrap would be wrong.
        ("Long", "parseLong") | ("Math", "round") => "long",
        ("Math", "pow") | ("Math", "sqrt") | ("Math", "floor") | ("Math", "ceil") => "double",
        ("Float", "parseFloat") | ("Float", "valueOf") => "float",
        ("Float", "toString") => "String",
        ("Float", "compare") => "int",
        ("Float", "isNaN") | ("Float", "isInfinite") => "boolean",
        ("Integer", "toString") | ("String", "valueOf") | ("String", "format") => "String",
        ("Arrays", "toString") => "String",
        ("Boolean", "parseBoolean") => "boolean",
        // `Character.toUpperCase(char)` returns a `char`, so its result keeps
        // rendering as a character rather than as a code point.
        ("Character", "toUpperCase") | ("Character", "toLowerCase") => "char",
        ("Character", "toString") => "String",
        ("Character", "getNumericValue") => "int",
        (
            "Character",
            "isDigit" | "isLetter" | "isLetterOrDigit" | "isWhitespace" | "isUpperCase"
            | "isLowerCase",
        ) => "boolean",
        _ => return None,
    })
}

/// The static numeric category of a known stdlib static call, when it is
/// statically `int`-typed (so it participates in `/` truncation). Methods whose
/// result is `int`-or-`double` at runtime (`Math.abs`/`max`/`min`) stay `None`
/// (treated as `Other`) rather than mis-typing a `double` result as `int`.
fn static_call_numtype(class: &str, method: &str) -> Option<NumType> {
    match (class, method) {
        ("Integer", "parseInt") | ("Integer", "valueOf") => Some(NumType::Int),
        ("Long", "parseLong") => Some(NumType::Int),
        ("Math", "round") => Some(NumType::Int),
        _ => None,
    }
}

/// The `java.util` collection types javars models, mapped to the collection
/// *shape* the host allocates. A user class of the same name wins (the compiler
/// checks `self.classes` first), so declaring your own `List` is still legal.
/// True when `e` is an explicit *boxing* call — `Integer.valueOf(x)` and the
/// other wrapper factories. Its static Java type in Java is the wrapper class,
/// but javars types it as the primitive it wraps (so `Integer.valueOf(7) / 2`
/// still truncates), which leaves the reference-ness invisible to
/// `expr_java_type`. The one place that distinction changes an *answer* is
/// `List.remove`, where the wrapper picks `remove(Object)` and the primitive
/// picks `remove(int)`, so it is recognised syntactically here.
/// The comparator `Comparator.naturalOrder()` denotes, as a lambda expression:
/// `(a, b) -> a.compareTo(b)`. Synthesized for the sorts that name no
/// comparator, so they order by the element's own `compareTo` through the same
/// erased-receiver dispatch every other `compareTo` call uses. The `#` in the
/// parameter names is not a legal Java identifier character, so they cannot
/// collide with (or be shadowed by) a user variable.
fn natural_order_comparator(line: u32) -> Expr {
    let (a, b) = ("#cmp0".to_string(), "#cmp1".to_string());
    Expr::Lambda {
        params: vec![a.clone(), b.clone()],
        body: LambdaBody::Expr(Box::new(Expr::MethodCall {
            recv: Box::new(Expr::Var(a)),
            method: "compareTo".to_string(),
            args: vec![Expr::Var(b)],
            line,
        })),
        line,
    }
}

fn is_boxing_call(e: &Expr) -> bool {
    matches!(
        e,
        Expr::MethodCall { recv, method, args, .. }
            if method == "valueOf"
                && args.len() == 1
                && matches!(
                    recv.as_ref(),
                    Expr::Var(c) if matches!(
                        c.as_str(),
                        "Integer" | "Long" | "Short" | "Byte" | "Character"
                            | "Double" | "Float" | "Boolean"
                    )
                )
    )
}

fn collection_kind(ty: &str) -> Option<&'static str> {
    Some(match ty {
        "ArrayList" | "LinkedList" => "list",
        "List" | "Collection" | "Iterable" => "list",
        "HashMap" | "LinkedHashMap" | "TreeMap" | "Map" => "map",
        "HashSet" | "LinkedHashSet" | "TreeSet" | "Set" => "set",
        _ => return None,
    })
}

/// True when `ty` names a collection *implementation* — the types `new` can
/// construct. The interfaces (`List`, `Map`, `Set`) are declaration-only.
fn is_concrete_collection(ty: &str) -> bool {
    matches!(
        ty,
        "ArrayList"
            | "LinkedList"
            | "HashMap"
            | "LinkedHashMap"
            | "TreeMap"
            | "HashSet"
            | "LinkedHashSet"
            | "TreeSet"
    )
}

/// The declared Java return type of a collection method, for the ones whose
/// result javars can type statically. `get`/`put`/`remove` return the erased
/// element type, which javars does not track, so they stay unknown.
fn collection_call_java_type(kind: &str, method: &str, argc: usize) -> Option<&'static str> {
    Some(match (method, argc) {
        ("size", 0) | ("indexOf", 1) | ("lastIndexOf", 1) => "int",
        ("isEmpty", 0) | ("contains", 1) | ("containsKey", 1) | ("containsValue", 1) => "boolean",
        ("add", 1) | ("addAll", 1) | ("equals", 1) => "boolean",
        ("remove", 1) if kind == "set" => "boolean",
        // The compiler-selected `List.remove(Object)` overload.
        ("removeObject", 1) => "boolean",
        ("toString", 0) => "String",
        ("keySet", 0) => "Set",
        ("values", 0) => "List",
        // A `subList` view is a `List`, so methods chain off it and a nested
        // `subList` resolves through the collection path rather than falling
        // through to the `String` methods.
        ("subList", 2) if kind == "list" => "List",
        _ => return None,
    })
}

/// The parameter count of a modeled stdlib static a method reference can name.
///
/// Only the single-arity entries are listed: `Integer.toString` and
/// `String.valueOf` take one *or* two arguments in Java, and a method reference
/// to an overloaded name has no arity until it is target-typed — which javars
/// does not do — so naming one is an error rather than a guess.
fn stdlib_static_ref_arity(class: &str, method: &str) -> Option<usize> {
    Some(match (class, method) {
        ("Integer", "parseInt")
        | ("Long", "parseLong")
        | ("Boolean", "parseBoolean")
        | ("Math", "sqrt")
        | ("Math", "floor")
        | ("Math", "ceil")
        | ("Math", "round")
        | ("Arrays", "toString") => 1,
        ("Math", "max") | ("Math", "min") | ("Math", "pow") => 2,
        _ => return None,
    })
}

/// The parameter count of a `String` instance method a method reference can name
/// (`String::length` → 0, so the synthesized lambda takes 1: the receiver).
/// `substring` and `indexOf` are overloaded on arity in Java and so are absent.
fn string_instance_ref_arity(method: &str) -> Option<usize> {
    Some(match method {
        "length" | "isEmpty" | "toUpperCase" | "toLowerCase" | "trim" => 0,
        "charAt"
        | "contains"
        | "startsWith"
        | "endsWith"
        | "equals"
        | "equalsIgnoreCase"
        | "concat"
        | "repeat"
        | "compareTo"
        | "compareToIgnoreCase" => 1,
        "replace" => 2,
        _ => return None,
    })
}

/// True when a statement is a loop or `switch` — a construct that owns its own
/// [`BreakScope`] and therefore consumes a prefixing label directly.
fn is_breakable(s: &Stmt) -> bool {
    matches!(
        s.kind,
        StmtKind::While { .. }
            | StmtKind::DoWhile { .. }
            | StmtKind::For { .. }
            | StmtKind::ForEach { .. }
            | StmtKind::Switch { .. }
    )
}

/// The value and Java type of a `static final` constant on a `java.lang` type
/// (`Integer.MAX_VALUE`, `Double.NaN`, `Math.PI`, …).
///
/// javars does not model the wrapper classes as classes, so these are folded to
/// their literal value at compile time rather than read from a field.
fn wrapper_constant(class: &str, name: &str) -> Option<(Value, &'static str)> {
    Some(match (class, name) {
        ("Integer", "MAX_VALUE") => (Value::Int(i32::MAX as i64), "int"),
        ("Integer", "MIN_VALUE") => (Value::Int(i32::MIN as i64), "int"),
        ("Long", "MAX_VALUE") => (Value::Int(i64::MAX), "long"),
        ("Long", "MIN_VALUE") => (Value::Int(i64::MIN), "long"),
        ("Short", "MAX_VALUE") => (Value::Int(i16::MAX as i64), "short"),
        ("Short", "MIN_VALUE") => (Value::Int(i16::MIN as i64), "short"),
        ("Byte", "MAX_VALUE") => (Value::Int(i8::MAX as i64), "byte"),
        ("Byte", "MIN_VALUE") => (Value::Int(i8::MIN as i64), "byte"),
        ("Float", "MAX_VALUE") => (Value::float(f32::MAX as f64), "float"),
        // `Float.MIN_VALUE` is the smallest positive *subnormal*, not `f32::MIN`.
        ("Float", "MIN_VALUE") => (Value::float(f32::from_bits(1) as f64), "float"),
        // `MIN_NORMAL` is the smallest positive value with a full mantissa: the
        // bottom of the *normal* range (1.1754944E-38), well above the subnormal
        // floor `MIN_VALUE` names. Rust spells it `MIN_POSITIVE`.
        ("Float", "MIN_NORMAL") => (Value::float(f32::MIN_POSITIVE as f64), "float"),
        ("Float", "POSITIVE_INFINITY") => (Value::float(f64::INFINITY), "float"),
        ("Float", "NEGATIVE_INFINITY") => (Value::float(f64::NEG_INFINITY), "float"),
        ("Float", "NaN") => (Value::float(f64::NAN), "float"),
        ("Double", "MAX_VALUE") => (Value::float(f64::MAX), "double"),
        // The smallest positive *subnormal* double, 4.9E-324 — Java's
        // `Double.MIN_VALUE` is not `f64::MIN`.
        ("Double", "MIN_VALUE") => (Value::float(f64::from_bits(1)), "double"),
        ("Double", "MIN_NORMAL") => (Value::float(f64::MIN_POSITIVE), "double"),
        ("Double", "POSITIVE_INFINITY") => (Value::float(f64::INFINITY), "double"),
        ("Double", "NEGATIVE_INFINITY") => (Value::float(f64::NEG_INFINITY), "double"),
        ("Double", "NaN") => (Value::float(f64::NAN), "double"),
        ("Math", "PI") => (Value::float(std::f64::consts::PI), "double"),
        ("Math", "E") => (Value::float(std::f64::consts::E), "double"),
        _ => return None,
    })
}

fn compound_op(op: AssignOp) -> Op {
    match op {
        AssignOp::Add => Op::Add,
        AssignOp::Sub => Op::Sub,
        AssignOp::Mul => Op::Mul,
        AssignOp::Mod => Op::Mod,
        AssignOp::BitAnd => Op::BitAnd,
        AssignOp::BitOr => Op::BitOr,
        AssignOp::BitXor => Op::BitXor,
        // `/=` is lowered separately so it can truncate int division, and the
        // shifts so they can mask the distance to the target's width.
        AssignOp::Div => unreachable!("`/=` lowers through the div-typing path"),
        AssignOp::Shl | AssignOp::Shr | AssignOp::Ushr => {
            unreachable!("shift assignments lower through the width-masking path")
        }
        AssignOp::Assign => unreachable!("plain assign never lowers through compound_op"),
    }
}

// ── duplicate declarations ──────────────────────────────────────────────────
//
// Every table below is keyed by a *name*, and every one of them was
// last-write-wins or first-write-wins before this check existed: the class
// table (`resolve_classes`' `out.insert`), the supertype map
// (`crate::supertype_map`'s `collect`), a class's field list, the static-method
// pool, and fusevm's `sub_entries` (whose lookup returns the first match — see
// its own note that the builder does not prevent duplicates). A compilation
// unit that declares one name twice therefore ran, silently, against whichever
// declaration the table happened to keep:
//
//   class Pt { int v() { return 1; } }
//   class Pt { int v() { return 2; } }   // javars printed 1
//   class C { int x = 1; int x = 2; }    // javars printed 2
//
// `javac` rejects all of them, so no working Java program is affected; what the
// silence cost was a *wrong answer* for a program `javac` never would have let
// run. The two that did fail failed misleadingly — a duplicate `static` method
// was reported as "no `f` overload matches 1 argument(s)" at the call site, and
// a duplicate constructor as "no constructor taking 1 argument(s) (declared
// arities: [1, 1])", both naming the caller rather than the duplicate.
//
// Java's own wording is reproduced, because it is what a reader will search
// for. Local variables are deliberately NOT checked here: Java forbids
// redeclaring a local anywhere inside an enclosing local's block but allows two
// sibling blocks to reuse a name, and javars's `MethodScope` is flat per method
// (sibling blocks share one slot), so the check needs block-scope tracking that
// does not exist yet. See BUGS.md.

/// The parameter-type list Java prints inside a member's diagnostic:
/// `f(int, String)`.
fn signature(name: &str, param_tys: &[String]) -> String {
    format!("{name}({})", param_tys.join(", "))
}

/// Reject a compilation unit that declares any name twice, with `javac`'s
/// diagnostic.
///
/// Runs on the *parsed* unit, before [`crate::prelude::inject`] adds the
/// modeled JDK types — a prelude type is skipped when the user declares one of
/// the same name, so checking afterwards would be checking javars's own tables
/// rather than the program's. (Those tables have their own guard:
/// `tests/registry_names.rs`.)
pub fn check_duplicate_declarations(prog: &Program) -> Result<(), String> {
    let mut seen_class: HashMap<&str, u32> = HashMap::new();
    for cl in &prog.classes {
        if seen_class.insert(&cl.name, cl.line).is_some() {
            return Err(format!(
                "javars: duplicate class: `{}` (line {})",
                cl.name, cl.line
            ));
        }
    }
    for cl in &prog.classes {
        let what = if cl.is_enum {
            "enum"
        } else if cl.is_interface {
            "interface"
        } else {
            "class"
        };
        // Instance and `static` fields share one namespace in Java, and so do
        // an enum's constants — `enum C { RED }` with a field `RED` collides.
        let mut seen: HashMap<&str, ()> = HashMap::new();
        let fields = cl.fields.iter().chain(&cl.static_fields);
        for f in fields {
            // The parser synthesizes an enum's `#name`/`#ordinal` and a
            // record's component fields; `#` is not a legal Java identifier
            // char, and a record component is already checked as a
            // constructor parameter, so neither can collide with user text.
            if f.name.starts_with('#') {
                continue;
            }
            if seen.insert(&f.name, ()).is_some() {
                return Err(format!(
                    "javars: variable `{}` is already defined in {what} `{}` (line {})",
                    f.name, cl.name, f.line
                ));
            }
        }
        for c in &cl.enum_constants {
            if seen.insert(&c.name, ()).is_some() {
                return Err(format!(
                    "javars: variable `{}` is already defined in {what} `{}` (line {})",
                    c.name, cl.name, c.line
                ));
            }
        }
        // A class's instance methods and its `static` methods share one
        // namespace: `int m()` and `static int m()` in one class is
        // "method m() is already defined" in Java too.
        let statics = prog.methods.iter().filter(|m| m.owner == cl.name);
        let mut seen: HashMap<(String, Vec<String>), ()> = HashMap::new();
        for m in cl.methods.iter().chain(statics) {
            let tys: Vec<String> = m.params.iter().map(|p| p.ty.clone()).collect();
            if seen.insert((m.name.clone(), tys.clone()), ()).is_some() {
                return Err(format!(
                    "javars: method `{}` is already defined in {what} `{}` (line {})",
                    signature(&m.name, &tys),
                    cl.name,
                    m.line
                ));
            }
            check_duplicate_params(&m.params, &m.name, m.line)?;
        }
        let mut seen: HashMap<Vec<String>, ()> = HashMap::new();
        for c in &cl.ctors {
            let tys: Vec<String> = c.params.iter().map(|p| p.ty.clone()).collect();
            if seen.insert(tys.clone(), ()).is_some() {
                return Err(format!(
                    "javars: constructor `{}` is already defined in {what} `{}` (line {})",
                    signature(&cl.name, &tys),
                    cl.name,
                    c.line
                ));
            }
            check_duplicate_params(&c.params, &cl.name, c.line)?;
        }
    }
    // `static` methods whose owner declares no class node of its own still
    // share the by-name pool, so they are checked as a whole too — a duplicate
    // there is the one that used to surface as a bogus "no overload matches".
    let mut seen: HashMap<(&str, &str, Vec<String>), ()> = HashMap::new();
    for m in &prog.methods {
        let tys: Vec<String> = m.params.iter().map(|p| p.ty.clone()).collect();
        if seen.insert((&m.owner, &m.name, tys.clone()), ()).is_some() {
            return Err(format!(
                "javars: method `{}` is already defined in class `{}` (line {})",
                signature(&m.name, &tys),
                m.owner,
                m.line
            ));
        }
    }
    Ok(())
}

/// Reject `f(int a, int a)`, which `javac` reports as a variable redeclaration
/// rather than as a signature problem.
fn check_duplicate_params(params: &[Param], owner: &str, line: u32) -> Result<(), String> {
    let mut seen: HashMap<&str, ()> = HashMap::new();
    for p in params {
        if seen.insert(&p.name, ()).is_some() {
            return Err(format!(
                "javars: variable `{}` is already defined in method `{owner}` (line {line})",
                p.name
            ));
        }
    }
    Ok(())
}
