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

/// Static signature of one user-defined static-method overload: its declared
/// parameter types (for overload resolution + mangling) and the numeric category
/// of its return type (so a call participates in division typing). The raw
/// return-type name is kept for static typing of a call result.
struct MethodSig {
    param_tys: Vec<String>,
    ret: NumType,
    ret_name: String,
}

/// A resolved static-method call: the mangled target subroutine name and its
/// return type (numeric category + raw name).
struct StaticResolved {
    mangled: String,
    ret: NumType,
    ret_name: String,
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
    /// Every instance field this class has (ancestors first, then own), in
    /// initialization order — the sequence the constructor prologue emits.
    fields: Vec<FieldInit>,
    /// Declared type of every field (own + inherited), for static typing.
    field_types: HashMap<String, String>,
    /// Every method this class can dispatch, keyed by `(name, param_types)` so
    /// same-name overloads differing only in parameter type coexist. Interface
    /// abstract/`default` methods are folded in first, then the class chain
    /// (most-derived wins). An entry whose defining type is an interface abstract
    /// method has no subroutine; it is only reached through virtual dispatch to a
    /// concrete implementor.
    methods: Vec<MethodMeta>,
    /// Constructor parameter-type signatures this class declares (empty ⇒
    /// implicit default ctor). Enables constructor overload resolution by type.
    ctors: Vec<Vec<String>>,
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
}

impl MethodScope {
    /// A static-method scope: slots start at 0, no `this`.
    fn new() -> Self {
        MethodScope::with_first_slot(0)
    }

    /// An instance-method/constructor scope: slot 0 is `this`, params start at 1.
    fn for_instance() -> Self {
        MethodScope::with_first_slot(1)
    }

    fn with_first_slot(first: u16) -> Self {
        MethodScope {
            slots: HashMap::new(),
            next_slot: first,
            types: HashMap::new(),
            decl_types: HashMap::new(),
            declared: std::collections::HashSet::new(),
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
    /// Counter minting unique internal temp names (`new`/compound-assign temps).
    temp_counter: u32,
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
            param_tys: m.params.iter().map(|p| p.ty.clone()).collect(),
            ret: numtype_of_ty(&m.ret).unwrap_or(NumType::Other),
            ret_name: m.ret.clone(),
        });
    }
    let classes = resolve_classes(prog)?;
    let mut c = Compiler {
        b: ChunkBuilder::new(),
        scopes: Vec::new(),
        pending_label: None,
        switch_counter: 0,
        exit_ops: Vec::new(),
        debug,
        has_ffi,
        global_types: HashMap::new(),
        scope: None,
        methods,
        global_decl_types: HashMap::new(),
        classes,
        this_class: None,
        temp_counter: 0,
    };
    // ── main body (global scope) ──
    for stmt in &prog.main {
        c.stmt(stmt)?;
    }
    // Patch any program-level `break`/`return;` to the position right after
    // main. When subroutines follow, that position holds the skip-over jump.
    let end = c.b.current_pos();
    let exit_ops = std::mem::take(&mut c.exit_ops);
    for op in exit_ops {
        c.b.patch_jump(op, end);
    }
    // ── subroutine bodies (static methods, then instance methods + ctors) ──
    // Emitted after `main` and jumped over so control never falls into them;
    // each is reached only via `Op::Call`.
    let has_subs = !prog.methods.is_empty()
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
                    },
                );
            }
        }
        let methods: Vec<MethodMeta> = methods.into_values().collect();
        let ctors: Vec<Vec<String>> = cl
            .ctors
            .iter()
            .map(|c| c.params.iter().map(|p| p.ty.clone()).collect())
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
                fields,
                field_types,
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
                let ty = self.var_decl_type(name)?;
                if ty.ends_with("[]") {
                    return Some(ty.to_string());
                }
                // A bare field of `this`.
                let this = self.this_class.as_ref()?;
                let ft = self.classes.get(this)?.field_types.get(name)?;
                ft.ends_with("[]").then(|| ft.clone())
            }
            Expr::Field { recv, name } => {
                let rc = self.expr_class(recv)?;
                let ft = self.classes.get(&rc)?.field_types.get(name)?;
                ft.ends_with("[]").then(|| ft.clone())
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
                if let Some(ty) = self.var_decl_type(name) {
                    if self.classes.contains_key(ty) {
                        return Some(ty.to_string());
                    }
                }
                // A bare field reference of `this`.
                let this = self.this_class.as_ref()?;
                let ty = self.classes.get(this)?.field_types.get(name)?;
                self.classes.contains_key(ty).then(|| ty.to_string())
            }
            Expr::Field { recv, name } => {
                let rc = self.expr_class(recv)?;
                let ty = self.classes.get(&rc)?.field_types.get(name)?;
                self.classes.contains_key(ty).then(|| ty.to_string())
            }
            Expr::NewObject { class, .. } => Some(class.clone()),
            // An element of a class-typed array (`Shape[] → Shape`).
            Expr::Index { array, .. } => {
                let arr_ty = self.expr_array_type(array)?;
                let elem = arr_ty.strip_suffix("[]")?;
                self.classes.contains_key(elem).then(|| elem.to_string())
            }
            Expr::MethodCall {
                recv, method, args, ..
            } => {
                let rc = self.expr_class(recv)?;
                let arg_tys: Vec<Option<String>> =
                    args.iter().map(|a| self.expr_java_type(a)).collect();
                let (_, ret_name, _) = self.resolve_instance_call(&rc, method, &arg_tys)?;
                self.classes.contains_key(&ret_name).then_some(ret_name)
            }
            _ => None,
        }
    }

    /// The declared type-name of a bare variable read `name`: a local/param/
    /// global declared type, else an instance field of the enclosing `this`.
    fn bare_var_type(&self, name: &str) -> Option<String> {
        if let Some(t) = self.var_decl_type(name) {
            return Some(t.to_string());
        }
        let this = self.this_class.as_ref()?;
        self.classes.get(this)?.field_types.get(name).cloned()
    }

    /// The static Java type-name of an expression (`int`, `double`, `boolean`,
    /// `String`, a class/interface name, an array type, `null`), or `None` when
    /// it cannot be determined statically. Drives overload resolution by argument
    /// type; an unknown type falls back to arity-only matching at the call site.
    fn expr_java_type(&self, e: &Expr) -> Option<String> {
        match e {
            Expr::Int(_) => Some("int".to_string()),
            Expr::Float(_) => Some("double".to_string()),
            Expr::Bool(_) => Some("boolean".to_string()),
            Expr::Str(_) => Some("String".to_string()),
            Expr::This => self.this_class.clone(),
            Expr::Var(name) => self.bare_var_type(name),
            Expr::Unary { op, rhs } => match op {
                UnOp::Neg => self.expr_java_type(rhs),
                UnOp::Not => Some("boolean".to_string()),
            },
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
                _ => Some("boolean".to_string()),
            },
            Expr::Ternary { then, els, .. } => {
                let t = self.expr_java_type(then);
                let e2 = self.expr_java_type(els);
                if t == e2 {
                    return t;
                }
                let tr = t.as_deref().and_then(numeric_rank)?;
                let er = e2.as_deref().and_then(numeric_rank)?;
                Some(rank_name(tr.max(er)).to_string())
            }
            Expr::Field { recv, name } => {
                if name == "length" {
                    return Some("int".to_string());
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
            Expr::InstanceOf { .. } => Some("boolean".to_string()),
            Expr::PostIncDec { name, .. } => self.bare_var_type(name),
            Expr::Call { name, args, .. } => {
                let arg_tys: Vec<Option<String>> =
                    args.iter().map(|a| self.expr_java_type(a)).collect();
                self.resolve_static_call(name, &arg_tys).map(|s| s.ret_name)
            }
            Expr::MethodCall {
                recv, method, args, ..
            } => {
                let arg_tys: Vec<Option<String>> =
                    args.iter().map(|a| self.expr_java_type(a)).collect();
                if let Some(rc) = self.expr_class(recv) {
                    if let Some((_, ret_name, _)) =
                        self.resolve_instance_call(&rc, method, &arg_tys)
                    {
                        return Some(ret_name);
                    }
                }
                // A `String` receiver's known return types.
                match (method.as_str(), args.len()) {
                    ("length", 0) | ("indexOf", 1) => Some("int".to_string()),
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
                    | ("repeat", 1)
                    | ("charAt", 1) => Some("String".to_string()),
                    _ => None,
                }
            }
            _ => None,
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
        cands: &[&[String]],
        arg_tys: &[Option<String>],
        who: &str,
    ) -> Result<usize, String> {
        if cands.len() == 1 {
            return Ok(0);
        }
        let mut best: Option<(usize, u32)> = None;
        let mut tie = false;
        for (i, ptys) in cands.iter().enumerate() {
            let mut total = 0u32;
            let mut ok = true;
            for (p, a) in ptys.iter().zip(arg_tys) {
                if let Some(at) = a {
                    match self.assign_cost(at, p) {
                        Some(c) => total += c,
                        None => {
                            ok = false;
                            break;
                        }
                    }
                }
            }
            if !ok {
                continue;
            }
            match best {
                None => {
                    best = Some((i, total));
                    tie = false;
                }
                Some((_, bc)) => {
                    if total < bc {
                        best = Some((i, total));
                        tie = false;
                    } else if total == bc {
                        tie = true;
                    }
                }
            }
        }
        match best {
            Some((i, _)) if !tie => Ok(i),
            Some(_) => Err(format!("javars: ambiguous overload for `{who}`")),
            None => Err(format!("javars: no applicable overload for `{who}`")),
        }
    }

    /// Resolve an instance-method call on a static receiver class by argument
    /// type: the chosen overload's parameter types, its return type name, and its
    /// return numeric category. `None` when no method of that name+arity exists.
    fn resolve_instance_call(
        &self,
        class: &str,
        method: &str,
        arg_tys: &[Option<String>],
    ) -> Option<(Vec<String>, String, NumType)> {
        let info = self.classes.get(class)?;
        let cands: Vec<&MethodMeta> = info
            .methods
            .iter()
            .filter(|m| m.name == method && m.param_tys.len() == arg_tys.len())
            .collect();
        if cands.is_empty() {
            return None;
        }
        let cand_tys: Vec<&[String]> = cands.iter().map(|m| m.param_tys.as_slice()).collect();
        let idx = self.pick_overload(&cand_tys, arg_tys, method).ok()?;
        let m = cands[idx];
        Some((
            m.param_tys.clone(),
            m.ret.clone(),
            numtype_of_ty(&m.ret).unwrap_or(NumType::Other),
        ))
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
    fn resolve_static_call(
        &self,
        name: &str,
        arg_tys: &[Option<String>],
    ) -> Option<StaticResolved> {
        let overloads = self.methods.get(name)?;
        let filtered: Vec<&MethodSig> = overloads
            .iter()
            .filter(|s| s.param_tys.len() == arg_tys.len())
            .collect();
        if filtered.is_empty() {
            return None;
        }
        let cand_tys: Vec<&[String]> = filtered.iter().map(|s| s.param_tys.as_slice()).collect();
        let idx = self.pick_overload(&cand_tys, arg_tys, name).ok()?;
        let s = filtered[idx];
        Some(StaticResolved {
            mangled: mangle_static(name, &s.param_tys),
            ret: s.ret,
            ret_name: s.ret_name.clone(),
        })
    }

    /// Resolve a constructor of `class` by argument type: the chosen ctor's
    /// parameter-type list (for mangling the `<init>` subroutine). `None` when
    /// no constructor of that arity is declared.
    fn resolve_ctor(&self, class: &str, arg_tys: &[Option<String>]) -> Option<Vec<String>> {
        let info = self.classes.get(class)?;
        let filtered: Vec<&Vec<String>> = info
            .ctors
            .iter()
            .filter(|c| c.len() == arg_tys.len())
            .collect();
        if filtered.is_empty() {
            return None;
        }
        let cand_tys: Vec<&[String]> = filtered.iter().map(|c| c.as_slice()).collect();
        let idx = self.pick_overload(&cand_tys, arg_tys, "<init>").ok()?;
        Some(filtered[idx].clone())
    }

    /// True when `class` declares or inherits any method named `method` with
    /// `argc` parameters (an existence check for dispatch decisions).
    fn has_instance_method(&self, class: &str, method: &str, argc: usize) -> bool {
        self.classes.get(class).is_some_and(|ci| {
            ci.methods
                .iter()
                .any(|m| m.name == method && m.param_tys.len() == argc)
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
        let (param_tys, _, _) = self
            .resolve_instance_call(rc, method, &arg_tys)
            .ok_or_else(|| {
                format!(
                "javars: class `{rc}` has no method `{method}` taking {} argument(s) (line {line})",
                args.len()
            )
            })?;
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
        // Fast path: a single concrete implementation across the whole subtree.
        // Use the concrete target's mangled name (not the static type's), so an
        // interface- or abstract-typed receiver still calls a real subroutine.
        if distinct.len() <= 1 {
            let mangled = targets.first().map(|(_, m)| m.clone()).ok_or_else(|| {
                format!("javars: no concrete implementation of `{method}` for `{rc}` (line {line})")
            })?;
            self.expr(recv)?; // this (deepest)
            for a in args {
                self.expr(a)?;
            }
            let idx = self.b.add_name(&mangled);
            self.b.emit(Op::Call(idx, args.len() as u8 + 1), line);
            return Ok(());
        }
        // Virtual path: stash receiver + args in temps (single evaluation), read
        // the runtime class, then dispatch.
        let recv_t = self.temp();
        self.expr(recv)?;
        self.emit_set(&recv_t, line);
        let arg_ts: Vec<String> = args
            .iter()
            .map(|a| {
                let t = self.temp();
                self.expr(a)?;
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
            end_jumps.push(self.b.emit(Op::Jump(0), line));
            let next = self.b.current_pos();
            self.b.patch_jump(skip, next);
        }
        // Fallback (unreachable at runtime — every concrete class is a target):
        // call an arbitrary concrete target so the stack stays balanced. Using a
        // real target (not the static type) keeps this valid when `rc` is an
        // interface whose own method has no subroutine.
        let base = targets[0].1.clone();
        self.emit_get(&recv_t, line);
        for t in &arg_ts {
            self.emit_get(t, line);
        }
        let idx = self.b.add_name(&base);
        self.b.emit(Op::Call(idx, argc), line);
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
                    if let Some((_, _, ret)) = self.resolve_instance_call(&rc, method, &arg_tys) {
                        return ret;
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
            | Expr::This => NumType::Other,
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
        let name_idx = self.b.add_name(&mangle_static(&m.name, &param_tys));
        self.b.add_sub_entry(name_idx, entry);

        let mut scope = MethodScope::new();
        register_params(&mut scope, &m.params);
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

        // Prologue: bind `this` (slot 0) + params (slots 1..=n), high-to-low.
        for i in (0..=m.params.len()).rev() {
            self.b.emit(Op::SetSlot(i as u16), m.line);
        }
        for s in &m.body {
            self.stmt(s)?;
        }
        self.b.emit(Op::LoadUndef, m.line);
        self.b.emit(Op::ReturnValue, m.line);

        self.scope = None;
        self.this_class = None;
        Ok(())
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

        for i in (0..=ctor.params.len()).rev() {
            self.b.emit(Op::SetSlot(i as u16), ctor.line);
        }
        for s in &ctor.body {
            self.stmt(s)?;
        }
        self.b.emit(Op::LoadUndef, ctor.line);
        self.b.emit(Op::ReturnValue, ctor.line);

        self.scope = None;
        self.this_class = None;
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
                // and untracked types are inferred from the initializer. The raw
                // type string powers class-typed dispatch — for `var`, infer the
                // class from the initializer.
                let nt = numtype_of_ty(ty)
                    .or_else(|| init.as_ref().map(|e| self.expr_type(e)))
                    .unwrap_or(NumType::Other);
                let raw = if ty == "var" {
                    init.as_ref()
                        .and_then(|e| self.expr_class(e))
                        .unwrap_or_else(|| ty.clone())
                } else {
                    ty.clone()
                };
                self.declare_local(name, &raw, nt);
                if let Some(e) = init {
                    self.expr(e)?;
                    self.emit_set(name, line);
                }
                // An uninitialized local is simply unbound until first assigned
                // (Java's definite-assignment check is not enforced yet).
                Ok(())
            }
            StmtKind::Assign { name, op, value } => {
                // A bare name that is a field of `this` (not a local) is an
                // implicit `this.name = …` field assignment.
                if let Some(class) = self.implicit_this_field(name) {
                    let recv = Expr::This;
                    let ft = self
                        .classes
                        .get(&class)
                        .and_then(|ci| ci.field_types.get(name))
                        .and_then(|ty| numtype_of_ty(ty))
                        .unwrap_or(NumType::Other);
                    return self.field_assign(&recv, name, *op, value, ft, line);
                }
                let l = self.lookup_type(name);
                match op {
                    AssignOp::Assign => {
                        self.expr(value)?;
                    }
                    AssignOp::Div => {
                        // `x /= e` — integer division truncates when both `x`
                        // and `e` are statically integral (Java `int /= int`).
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
            } => {
                let ft = self
                    .expr_class(recv)
                    .and_then(|rc| {
                        self.classes
                            .get(&rc)
                            .and_then(|ci| ci.field_types.get(name))
                            .and_then(|ty| numtype_of_ty(ty))
                    })
                    .unwrap_or(NumType::Other);
                self.field_assign(recv, name, *op, value, ft, line)
            }
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
            StmtKind::Break(label) => self.break_stmt(label.as_deref(), line),
            StmtKind::Continue(label) => self.continue_stmt(label.as_deref(), line),
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
        init: &Option<Box<Stmt>>,
        cond: &Option<Expr>,
        update: &Option<Box<Stmt>>,
        body: &[Stmt],
    ) -> Result<(), String> {
        let label = self.pending_label.take();
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
        self.scopes.push(BreakScope::loop_scope(label));
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
        let l = self.scopes.pop().unwrap();
        for op in l.continue_ops {
            self.b.patch_jump(op, step);
        }
        for op in l.break_ops {
            self.b.patch_jump(op, end);
        }
        Ok(())
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
                self.expr(lab)?;
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
        let op = self.b.emit(Op::Jump(0), line);
        match self.find_scope(label, |_| true) {
            Some(idx) => {
                self.scopes[idx].break_ops.push(op);
                Ok(())
            }
            None if label.is_none() => {
                // A top-level `break` (no enclosing construct) ends the program,
                // preserving javars's existing behavior.
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
        let op = self.b.emit(Op::Jump(0), line);
        match self.find_scope(label, |s| s.kind == ScopeKind::Loop) {
            Some(idx) => {
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
        if let Some(class) = self.implicit_this_field(name) {
            let ft = self.field_numtype(&class, name);
            return self.field_assign(&Expr::This, name, op, &Expr::Int(1), ft, 0);
        }
        self.emit_get(name, 0);
        self.b.emit(Op::LoadInt(1), 0);
        self.b.emit(if inc { Op::Add } else { Op::Sub }, 0);
        self.emit_set(name, 0);
        Ok(())
    }

    /// The numeric category of a field, for compound-assignment `/` typing.
    fn field_numtype(&self, class: &str, name: &str) -> NumType {
        self.classes
            .get(class)
            .and_then(|ci| ci.field_types.get(name))
            .and_then(|ty| numtype_of_ty(ty))
            .unwrap_or(NumType::Other)
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
    /// instance of a class whose (subtree) declares one — so `println(obj)`
    /// honours the override. Otherwise evaluate normally (the host's `java_str`
    /// renders the default `Class@hash` form). String concatenation with a plain
    /// object and `String.valueOf(obj)` still use the default form (see BUGS.md).
    fn emit_stringified(&mut self, e: &Expr) -> Result<(), String> {
        if let Some(rc) = self.expr_class(e) {
            if self.has_instance_method(&rc, "toString", 0) {
                return self.dispatch_instance_method(e, &rc, "toString", &[], 0);
            }
        }
        self.expr(e)
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
                // A bare name that is a field of `this` (not a local) reads
                // `this.name`; otherwise it is a plain local/global.
                if self.implicit_this_field(name).is_some() {
                    self.b.emit(Op::GetSlot(0), 0); // this
                    self.emit_field_get(name, 0);
                } else {
                    self.emit_get(name, 0);
                }
            }
            Expr::This => {
                if self.this_class.is_none() {
                    return Err("javars: `this` used outside an instance method".to_string());
                }
                self.b.emit(Op::GetSlot(0), 0);
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
                    self.b.emit(Op::CallBuiltin(crate::host::JARRAY_NEW, 2), 0);
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
                    self.b.emit(
                        Op::CallBuiltin(crate::host::JARRAY_NEW_MULTI, sizes.len() as u8 + 1),
                        0,
                    );
                }
            }
            Expr::ArrayLit { elems } => {
                for el in elems {
                    self.expr(el)?;
                }
                self.b.emit(
                    Op::CallBuiltin(crate::host::JARRAY_LIT, elems.len() as u8),
                    0,
                );
            }
            Expr::Index { array, index } => {
                self.expr(array)?;
                self.expr(index)?;
                self.b.emit(Op::CallBuiltin(crate::host::JARRAY_GET, 2), 0);
            }
            Expr::Field { recv, name } => {
                self.expr(recv)?;
                self.emit_field_get(name, 0);
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
                    }
                    UnOp::Not => {
                        self.b.emit(Op::LogNot, 0);
                    }
                }
            }
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
                    self.b.emit(Op::GetSlot(0), 0);
                    self.emit_field_get(name, 0);
                } else {
                    self.emit_get(name, 0);
                }
                self.post_inc_dec(name, *inc)?;
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
        if let Expr::Var(class) = recv {
            if is_static_class(class) && !self.is_declared_var(class) {
                for a in args {
                    self.expr(a)?;
                }
                let class_c = self.b.add_constant(Value::str(class.clone()));
                self.b.emit(Op::LoadConst(class_c), line);
                let method_c = self.b.add_constant(Value::str(method.to_string()));
                self.b.emit(Op::LoadConst(method_c), line);
                // argc counts the args plus the class-name and method-name strings.
                self.b.emit(
                    Op::CallBuiltin(crate::host::JSTATIC_DISPATCH, args.len() as u8 + 2),
                    line,
                );
                return Ok(());
            }
        }
        // A user-class receiver: dispatch on the receiver's runtime class
        // (virtual dispatch), collapsing to a direct call when not overridden.
        if let Some(rc) = self.expr_class(recv) {
            return self.dispatch_instance_method(recv, &rc, method, args, line);
        }
        // Otherwise a `String` method.
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

    /// Emit `recv.field` read given the receiver value is already on the stack:
    /// push the field name and call the field-get builtin.
    fn emit_field_get(&mut self, name: &str, line: u32) {
        let name_c = self.b.add_constant(Value::str(name.to_string()));
        self.b.emit(Op::LoadConst(name_c), line);
        self.b
            .emit(Op::CallBuiltin(crate::host::JFIELD_GET, 2), line);
    }

    /// Lower `new ClassName(args...)`: allocate the instance, seed its fields
    /// (defaults then declared initializers, ancestors first), run the matching
    /// constructor, and leave the instance handle on the stack.
    fn new_object(&mut self, class: &str, args: &[Expr], line: u32) -> Result<(), String> {
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
        let has_any_ctor = !info.ctors.is_empty();
        let ctor_arities: Vec<usize> = info.ctors.iter().map(|c| c.len()).collect();
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
                Some(e) => self.expr(e)?,
                None => self.emit_type_default(fty, line),
            }
            self.b
                .emit(Op::CallBuiltin(crate::host::JFIELD_SET, 3), line);
            self.b.emit(Op::Pop, line);
        }

        // Run the constructor (resolving the overload by argument type). A class
        // with no declared ctor accepts only `new C()`.
        let arg_tys: Vec<Option<String>> = args.iter().map(|a| self.expr_java_type(a)).collect();
        let ctor_sig = self.resolve_ctor(class, &arg_tys);
        if let Some(param_tys) = ctor_sig {
            self.emit_get(&obj, line); // this
            for a in args {
                self.expr(a)?;
            }
            let mangled = mangle(class, "<init>", &param_tys);
            let name_idx = self.b.add_name(&mangled);
            self.b.emit(Op::Call(name_idx, args.len() as u8 + 1), line);
            self.b.emit(Op::Pop, line); // discard the ctor's (Undef) result
        } else if has_any_ctor || !args.is_empty() {
            return Err(format!(
                "javars: class `{class}` has no constructor taking {} argument(s) (declared arities: {:?}) (line {line})",
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
        if op == AssignOp::Assign {
            self.expr(array)?;
            self.expr(index)?;
            self.expr(value)?;
            self.b
                .emit(Op::CallBuiltin(crate::host::JARRAY_SET, 3), line);
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
        self.b
            .emit(Op::CallBuiltin(crate::host::JARRAY_GET, 2), line);
        // combine with value
        let elem_t = match array {
            Expr::Var(n) => self
                .var_decl_type(n)
                .map(array_elem_numtype)
                .unwrap_or(NumType::Other),
            _ => NumType::Other,
        };
        self.emit_compound(op, value, elem_t, line)?;
        let new_t = self.temp();
        self.emit_set(&new_t, line);
        // write back
        self.emit_get(&arr_t, line);
        self.emit_get(&idx_t, line);
        self.emit_get(&new_t, line);
        self.b
            .emit(Op::CallBuiltin(crate::host::JARRAY_SET, 3), line);
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
        field_ty: NumType,
        line: u32,
    ) -> Result<(), String> {
        if op == AssignOp::Assign {
            self.expr(recv)?;
            let name_c = self.b.add_constant(Value::str(name.to_string()));
            self.b.emit(Op::LoadConst(name_c), line);
            self.expr(value)?;
            self.b
                .emit(Op::CallBuiltin(crate::host::JFIELD_SET, 3), line);
            self.b.emit(Op::Pop, line);
            return Ok(());
        }
        let obj_t = self.temp();
        self.expr(recv)?;
        self.emit_set(&obj_t, line);
        // old field value
        self.emit_get(&obj_t, line);
        self.emit_field_get(name, line);
        self.emit_compound(op, value, field_ty, line)?;
        let new_t = self.temp();
        self.emit_set(&new_t, line);
        // write back
        self.emit_get(&obj_t, line);
        let name_c = self.b.add_constant(Value::str(name.to_string()));
        self.b.emit(Op::LoadConst(name_c), line);
        self.emit_get(&new_t, line);
        self.b
            .emit(Op::CallBuiltin(crate::host::JFIELD_SET, 3), line);
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
        line: u32,
    ) -> Result<(), String> {
        if op == AssignOp::Div {
            let r = self.expr_type(value);
            self.expr(value)?;
            self.emit_div(target, r, line);
        } else {
            self.expr(value)?;
            self.b.emit(compound_op(op), line);
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
                    if let Some(param_tys) = self.resolve_ctor(&sup, &arg_tys) {
                        self.b.emit(Op::GetSlot(0), line); // this
                        for a in args {
                            self.expr(a)?;
                        }
                        let mangled = mangle(&sup, "<init>", &param_tys);
                        let idx = self.b.add_name(&mangled);
                        self.b.emit(Op::Call(idx, args.len() as u8 + 1), line);
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
            for a in args {
                self.expr(a)?;
            }
            let name_idx = self.b.add_name(&resolved.mangled);
            self.b.emit(Op::Call(name_idx, args.len() as u8), line);
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
        self.expr(cond)?;
        let jf = self.b.emit(Op::JumpIfFalse(0), 0);
        self.expr(then)?;
        let jend = self.b.emit(Op::Jump(0), 0);
        let else_start = self.b.current_pos();
        self.b.patch_jump(jf, else_start);
        self.expr(els)?;
        let end = self.b.current_pos();
        self.b.patch_jump(jend, end);
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
            init.as_deref()
                .is_some_and(|s| body_has_ffi(std::slice::from_ref(s)))
                || cond.as_ref().is_some_and(expr_has_ffi)
                || update
                    .as_deref()
                    .is_some_and(|s| body_has_ffi(std::slice::from_ref(s)))
                || body_has_ffi(body)
        }
        StmtKind::Switch { disc, groups } => {
            expr_has_ffi(disc)
                || groups
                    .iter()
                    .any(|g| g.labels.iter().any(expr_has_ffi) || body_has_ffi(&g.body))
        }
        StmtKind::Labeled { body, .. } => body_has_ffi(std::slice::from_ref(body)),
        StmtKind::Return(val) => val.as_ref().is_some_and(expr_has_ffi),
        StmtKind::Break(_) | StmtKind::Continue(_) => false,
    })
}

fn expr_has_ffi(e: &Expr) -> bool {
    match e {
        Expr::Call { name, args, .. } => name == RUST_COMPILE || args.iter().any(expr_has_ffi),
        Expr::Unary { rhs, .. } => expr_has_ffi(rhs),
        Expr::Binary { lhs, rhs, .. } => expr_has_ffi(lhs) || expr_has_ffi(rhs),
        Expr::Ternary { cond, then, els } => {
            expr_has_ffi(cond) || expr_has_ffi(then) || expr_has_ffi(els)
        }
        Expr::Println { arg, .. } => arg.as_deref().is_some_and(expr_has_ffi),
        Expr::MethodCall { recv, args, .. } => expr_has_ffi(recv) || args.iter().any(expr_has_ffi),
        Expr::NewArray { sizes, .. } => sizes.iter().any(expr_has_ffi),
        Expr::ArrayLit { elems } => elems.iter().any(expr_has_ffi),
        Expr::Index { array, index } => expr_has_ffi(array) || expr_has_ffi(index),
        Expr::Field { recv, .. } => expr_has_ffi(recv),
        Expr::NewObject { args, .. } => args.iter().any(expr_has_ffi),
        Expr::InstanceOf { expr, .. } => expr_has_ffi(expr),
        Expr::Int(_)
        | Expr::Float(_)
        | Expr::Str(_)
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
fn mangle_static(name: &str, param_tys: &[String]) -> String {
    format!("#s#{name}#{}", param_tys.join(","))
}

/// The numeric widening rank of a primitive type (`byte` < `short`/`char` <
/// `int` < `long` < `float` < `double`); `None` for non-numeric types. Used by
/// overload resolution to score widening conversions.
fn numeric_rank(ty: &str) -> Option<u32> {
    Some(match ty {
        "byte" => 1,
        "short" | "char" => 2,
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
fn is_static_class(name: &str) -> bool {
    matches!(
        name,
        "Math" | "Integer" | "Long" | "Double" | "Boolean" | "String" | "Character" | "Arrays"
    )
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

/// True when a statement is a loop or `switch` — a construct that owns its own
/// [`BreakScope`] and therefore consumes a prefixing label directly.
fn is_breakable(s: &Stmt) -> bool {
    matches!(
        s.kind,
        StmtKind::While { .. }
            | StmtKind::DoWhile { .. }
            | StmtKind::For { .. }
            | StmtKind::Switch { .. }
    )
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
