//! The Java AST javars parses and lowers to fusevm bytecode.
//!
//! Slice 1 is a single-class, single-`main` subset: the parser accepts a
//! `public class Name { ... static void main(String[] args) { ... } }` shell
//! and models the statements and expressions inside `main`. User-defined
//! methods, fields, and multiple classes are parsed no further than the entry
//! point today (see `BUGS.md`); the AST is shaped to grow into them.

/// A parsed compilation unit: the entry class name, the body of its `main`, and
/// any user-defined static helper methods.
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    /// The name of the class that declares `main` (informational; used by the
    /// `--dump-ast` output and error messages).
    pub class_name: String,
    /// The statements of `public static void main(String[] args)`.
    pub main: Vec<Stmt>,
    /// User-defined `static` methods declared in any class (a flat pool — javars
    /// resolves a bare `name(...)` call to one of these by name). `main` is
    /// excluded (it is the entry [`Program::main`]).
    pub methods: Vec<Method>,
    /// Every user-defined class in the compilation unit (top-level siblings and
    /// nested `static` classes, flattened into one namespace). The entry class
    /// (whichever declares `main`) is also present here so `new EntryClass()` and
    /// its instance members resolve.
    pub classes: Vec<Class>,
}

/// A user-defined class: its instance fields, constructors, and instance
/// methods. `static` methods are hoisted into [`Program::methods`]; only the
/// non-static members live here. Modeled as heap objects ([`crate::host`]
/// `HostObj::Instance`) at runtime.
#[derive(Debug, Clone, PartialEq)]
pub struct Class {
    pub name: String,
    /// The direct superclass name (`class C extends B`), if any. For an
    /// `interface` this is always `None` (extended interfaces go in
    /// [`Class::interfaces`]).
    pub superclass: Option<String>,
    /// Directly implemented interfaces (`class C implements I, J`). For an
    /// `interface`, the interfaces it `extends` (`interface I extends A, B`).
    pub interfaces: Vec<String>,
    /// True when this declaration is an `interface` rather than a `class`. An
    /// interface is not instantiable; its abstract methods are dispatch-only and
    /// its `default` methods supply an inherited body to implementors.
    pub is_interface: bool,
    /// Instance fields in declaration order (name, type, optional initializer).
    pub fields: Vec<FieldDecl>,
    /// Declared constructors. Empty means the implicit no-arg default ctor.
    pub ctors: Vec<Ctor>,
    /// Instance (non-static) methods.
    pub methods: Vec<Method>,
    /// 1-based source line the class starts on (diagnostics).
    pub line: u32,
}

/// An instance field declaration: `int x;` or `String name = "?";`.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldDecl {
    pub ty: String,
    pub name: String,
    /// Field initializer expression, run (with default values first) before the
    /// constructor body — Java's instance-initialization order.
    pub init: Option<Expr>,
}

/// A constructor: `ClassName(<params>) { <body> }`.
#[derive(Debug, Clone, PartialEq)]
pub struct Ctor {
    pub params: Vec<Param>,
    pub body: Vec<Stmt>,
    pub line: u32,
}

/// A user-defined `static` method: `static <ret> name(<params>) { <body> }`.
/// Lowered to a fusevm call-frame subroutine (`Op::Call`); parameters and locals
/// live in frame slots so recursion is well-behaved.
#[derive(Debug, Clone, PartialEq)]
pub struct Method {
    pub name: String,
    /// Formal parameters in declaration order (bound to slots `0..n`).
    pub params: Vec<Param>,
    /// The declared return type (retained for diagnostics and numeric-division
    /// typing; not otherwise checked).
    pub ret: String,
    pub body: Vec<Stmt>,
    /// True for a bodyless method — an interface abstract method (`void m();`)
    /// or an `abstract` class method. No subroutine is emitted; the method is a
    /// virtual-dispatch target resolved to a concrete override at runtime.
    pub is_abstract: bool,
    /// 1-based source line the method starts on (for the debug line marker and
    /// prologue ops).
    pub line: u32,
}

/// A method formal parameter: its declared type and name.
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub ty: String,
    pub name: String,
}

/// A Java statement with its 1-based source line (threaded through the parser so
/// the debug-compile path can emit a per-statement line marker for `--dap`).
#[derive(Debug, Clone, PartialEq)]
pub struct Stmt {
    pub line: u32,
    pub kind: StmtKind,
}

impl Stmt {
    /// Build a statement carrying its source line.
    pub fn new(line: u32, kind: StmtKind) -> Self {
        Stmt { line, kind }
    }
}

/// The kind of a Java statement (the payload of [`Stmt`]).
#[derive(Debug, Clone, PartialEq)]
pub enum StmtKind {
    /// A local variable declaration: `int x = expr;`, `var s = expr;`. The
    /// declared type is retained for diagnostics; the runtime is dynamically
    /// typed on the fusevm value model, so it does not gate execution yet.
    Local {
        ty: String,
        name: String,
        init: Option<Expr>,
    },
    /// An assignment to an existing variable: `x = expr;`, `x += expr;`.
    Assign {
        name: String,
        op: AssignOp,
        value: Expr,
    },
    /// An assignment to an array element: `a[i] = expr;`, `a[i] += expr;`.
    IndexAssign {
        array: Expr,
        index: Expr,
        op: AssignOp,
        value: Expr,
    },
    /// An assignment to an instance field: `obj.f = expr;`, `this.f += expr;`.
    FieldAssign {
        recv: Expr,
        name: String,
        op: AssignOp,
        value: Expr,
    },
    /// An expression evaluated for its side effects: `System.out.println(x);`.
    Expr(Expr),
    /// `if (cond) { .. } else { .. }`.
    If {
        cond: Expr,
        then: Vec<Stmt>,
        els: Vec<Stmt>,
    },
    /// `while (cond) { .. }`.
    While { cond: Expr, body: Vec<Stmt> },
    /// `do { .. } while (cond);` — the body runs once before the condition is
    /// first tested.
    DoWhile { body: Vec<Stmt>, cond: Expr },
    /// `for (init; cond; update) { .. }` — the C-style loop.
    For {
        init: Option<Box<Stmt>>,
        cond: Option<Expr>,
        update: Option<Box<Stmt>>,
        body: Vec<Stmt>,
    },
    /// `switch (disc) { case L: .. break; default: .. }` — the classic
    /// statement form with fall-through between groups.
    Switch {
        disc: Expr,
        groups: Vec<SwitchGroup>,
    },
    /// A labeled statement `label: <stmt>`. The label names the enclosing loop
    /// or switch so `break label;`/`continue label;` can target it.
    Labeled { label: String, body: Box<Stmt> },
    /// `break;` or `break label;`.
    Break(Option<String>),
    /// `continue;` or `continue label;`.
    Continue(Option<String>),
    /// `return;` or `return <expr>;`. In `main` (which is `void`) a bare return
    /// ends the program and a value return is rejected; in a method it lowers to
    /// `Op::ReturnValue`.
    Return(Option<Expr>),
}

/// One group of a `switch`: its `case` label expressions (and/or `default`),
/// followed by the statements that run when a label matches. Fall-through is
/// modeled by laying the groups' bodies out consecutively — control drops into
/// the next group's body unless a `break` intervenes. `case 1: case 2: body`
/// is a single group with two labels.
#[derive(Debug, Clone, PartialEq)]
pub struct SwitchGroup {
    /// The `case` label constant expressions selecting this group.
    pub labels: Vec<Expr>,
    /// True when this group carries the `default:` label.
    pub is_default: bool,
    /// The statements of this group.
    pub body: Vec<Stmt>,
}

/// Compound-assignment operator. `Assign` is a plain `=`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignOp {
    Assign,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

/// A Java expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    /// A bare identifier — a local variable read.
    Var(String),
    /// A unary operator applied to one operand (`-x`, `!b`).
    Unary {
        op: UnOp,
        rhs: Box<Expr>,
    },
    /// A binary operator applied to two operands.
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// `cond ? then : els` — the conditional (ternary) operator. Right-
    /// associative; the result takes the value of the selected branch.
    Ternary {
        cond: Box<Expr>,
        then: Box<Expr>,
        els: Box<Expr>,
    },
    /// `System.out.println(arg)` / `System.out.print(arg)`, or the `System.err`
    /// variants when `err` is set. Modeled directly (rather than as a general
    /// method call) because `System.out`/`err` are streams, not values.
    Println {
        newline: bool,
        err: bool,
        arg: Option<Box<Expr>>,
    },
    /// Post-increment / post-decrement of a variable (`i++`, `i--`), evaluated
    /// as a statement today. The bool is `true` for `++`.
    PostIncDec {
        name: String,
        inc: bool,
    },
    /// A bare-identifier function call `name(args...)`. Slice 1 declares no
    /// user methods, so the only calls that reach the compiler are the inline
    /// Rust FFI desugar target `__rust_compile("<base64>", line)` and the
    /// barewords a `rust { ... }` block exports (routed to `fusevm::ffi` by
    /// name — see [`crate::rust_ffi`] and [`crate::host`]). A call name that is
    /// neither, with no FFI block present, is an unresolved reference.
    Call {
        name: String,
        args: Vec<Expr>,
        line: u32,
    },
    /// An instance method call `recv.method(args...)`. Dispatches on the
    /// receiver's static type: a `String` receiver runs the [`crate::host`]
    /// string builtins (`length`, `substring`, …); a user-class receiver calls
    /// the class's mangled instance-method subroutine (static-type dispatch —
    /// exact when there is no overriding). `System.out.print[ln]` keeps its
    /// dedicated [`Expr::Println`] form.
    MethodCall {
        recv: Box<Expr>,
        method: String,
        args: Vec<Expr>,
        line: u32,
    },
    /// `new int[n]` / `new int[m][n]` — a fresh (possibly multi-dimensional)
    /// array of default-valued elements. The element type sets the leaf default
    /// (`int`→0, `double`→0.0, `boolean`→false, a class/`String`→`null`).
    /// `sizes` holds the explicitly-sized dimensions (outer first); `extra_dims`
    /// counts trailing unsized `[]` dimensions (`new int[2][]` → `sizes=[2]`,
    /// `extra_dims=1`), whose innermost elements default to `null`. Lives on the
    /// host heap as `Obj` handles, so aliasing is by reference.
    NewArray {
        elem_ty: String,
        sizes: Vec<Expr>,
        extra_dims: usize,
    },
    /// An array literal `{a, b, c}` (from `int[] a = {1,2,3}` or
    /// `new int[]{1,2,3}`). Builds a heap array from the element values.
    ArrayLit {
        elems: Vec<Expr>,
    },
    /// `array[index]` — element read. Bounds-checked at runtime.
    Index {
        array: Box<Expr>,
        index: Box<Expr>,
    },
    /// `recv.field` field access with no call parens: an array's `.length` or an
    /// instance field. (String's `.length()` keeps its method-call form.)
    Field {
        recv: Box<Expr>,
        name: String,
    },
    /// `new ClassName(args...)` — construct a class instance on the host heap.
    NewObject {
        class: String,
        args: Vec<Expr>,
        line: u32,
    },
    /// `this` — the receiver of the enclosing instance method or constructor
    /// (frame slot 0).
    This,
    /// `expr instanceof ClassName` — true when `expr` is a non-null instance of
    /// `ClassName` or a subclass.
    InstanceOf {
        expr: Box<Expr>,
        class: String,
    },
}

/// Unary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
}

/// Binary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    And,
    Or,
}
