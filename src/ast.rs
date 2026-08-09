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
    /// The name `main` gives its `String[]` parameter (`args`), when it declares
    /// one. Bound to the real program arguments by the compiler's prologue;
    /// `None` for the (legal) `main()` that declares no parameter.
    pub main_param: Option<String>,
    /// User-defined `static` methods declared in any class, in one pool keyed by
    /// name. Each entry carries its declaring class in [`Method::owner`], which
    /// is what a *qualified* `C.m(...)` call resolves against (`C`'s own
    /// declarations, then its superclass chain); an *unqualified* `m(...)`
    /// prefers the enclosing class's chain and falls back to the whole pool.
    /// `main` is excluded (it is the entry [`Program::main`]).
    pub methods: Vec<Method>,
    /// Every user-defined class in the compilation unit (top-level siblings and
    /// nested `static` classes, flattened into one namespace). The entry class
    /// (whichever declares `main`) is also present here so `new EntryClass()` and
    /// its instance members resolve.
    pub classes: Vec<Class>,
    /// True when the unit mentions exceptions at all — a `try`, a `throw`, or a
    /// `new` of a modeled `java.lang` throwable. Set by the parser and read by
    /// [`crate::prelude::inject`] (which supplies the throwable classes) and by
    /// the compiler (which only emits the post-call exception checks when this is
    /// set, so an exception-free program's bytecode is unchanged).
    pub uses_exceptions: bool,
    /// True when the unit mentions a lambda, a method reference, or one of the
    /// `java.util.function` interface names. Set by the parser and read by
    /// [`crate::prelude::inject`], which supplies the functional interfaces —
    /// a program with no lambda carries none of them.
    pub uses_functional: bool,
}

/// Field holding an enum constant's `name()`. `#` is not a legal Java identifier
/// character, so a synthesized field can never collide with a user-declared one.
pub const ENUM_NAME: &str = "#name";
/// Field holding an enum constant's `ordinal()`.
pub const ENUM_ORDINAL: &str = "#ordinal";

/// The global holding the singleton instance of `class.constant` — the storage
/// `Color.RED` reads. Minted by the compiler's enum prologue.
pub fn enum_global(class: &str, constant: &str) -> String {
    format!("#enum#{class}#{constant}")
}

/// The global holding `class.field`, a `static` field's one shared cell.
///
/// fusevm's `GetVar`/`SetVar` address a flat global table while frame slots are
/// per-call, so a `static` field — which every frame sees, and which outlives
/// any one call — is stored as a global rather than a slot. `#` is not a legal
/// Java identifier character, so the minted name can never collide with a
/// user-declared variable.
pub fn static_global(class: &str, field: &str) -> String {
    format!("#static#{class}#{field}")
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
    /// True when this declaration is an `enum`. Distinct from a non-empty
    /// [`Class::enum_constants`] because `enum Empty { }` is legal Java.
    pub is_enum: bool,
    /// True when this declaration is a `record`. The parser lowers a record to
    /// an ordinary class (fields, canonical constructor, accessors), so this is
    /// the only thing left that says it was one — and `r instanceof Record` is
    /// the observable that needs it, since Java makes every record a subclass of
    /// `java.lang.Record`.
    pub is_record: bool,
    /// The constants of an `enum`, in declaration order — the index is the
    /// value of `ordinal()`. Empty for a `class`/`interface`. An enum is
    /// otherwise an ordinary class: the parser gives it the `#name`/`#ordinal`
    /// fields and the `name()`/`ordinal()`/`toString()` bodies that read them,
    /// and the compiler builds one instance per constant before `main` runs.
    pub enum_constants: Vec<EnumConstant>,
    /// Instance fields in declaration order (name, type, optional initializer).
    pub fields: Vec<FieldDecl>,
    /// `static` fields in declaration order. Their storage is one global per
    /// field ([`static_global`]), seeded with the type's default value before
    /// any user code runs.
    pub static_fields: Vec<FieldDecl>,
    /// The class's static initialization, in textual order: each `static` field
    /// initializer lowered to an assignment, interleaved with the bodies of the
    /// `static { … }` blocks. Run once, before `main`.
    pub static_init: Vec<Stmt>,
    /// Declared constructors. Empty means the implicit no-arg default ctor.
    pub ctors: Vec<Ctor>,
    /// Instance (non-static) methods.
    pub methods: Vec<Method>,
    /// 1-based source line the class starts on (diagnostics).
    pub line: u32,
}

/// One constant of an `enum`: `RED`, `EARTH(5.97e24)`, or `PLUS { … }`.
#[derive(Debug, Clone, PartialEq)]
pub struct EnumConstant {
    pub name: String,
    /// The constructor arguments the constant supplies (`EARTH(5.97e24)`), run
    /// against the enum's own constructor when its singleton is built.
    pub args: Vec<Expr>,
    /// The synthetic subclass a constant *body* (`PLUS { int apply(…) { … } }`)
    /// is compiled to — Java specifies such a constant as an anonymous subclass
    /// of the enum, and modeling it as a real class is what makes its overrides
    /// reachable through ordinary virtual dispatch. `None` for a bare constant.
    pub body_class: Option<String>,
}

/// The synthetic subclass name for an `enum` constant that declares a body.
/// `#` is not a legal Java identifier character, so it cannot collide with a
/// user-declared class.
pub fn enum_body_class(enum_name: &str, constant: &str) -> String {
    format!("{enum_name}#{constant}")
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
    /// The class that declares this method. For a `static` method it is both the
    /// class whose `static` fields an unqualified name inside the body resolves
    /// against, and the scope a qualified `C.m(...)` call must match — statics
    /// share one by-name pool, so the owner has to travel with the method or two
    /// classes spelling the same method name become indistinguishable.
    pub owner: String,
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
    /// True for the variable-arity parameter (`int... xs`). `ty` is already the
    /// array type (`int[]`) — inside the body the parameter *is* that array —
    /// so this flag only says the declaration also admits Java's third
    /// (variable-arity) resolution phase, where a call passing loose trailing
    /// arguments is packed into the array at the call site. Only the last
    /// parameter may carry it.
    pub varargs: bool,
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
    /// One declaration statement that declares more than one variable:
    /// `int a = 1, b = 2;`. Each element is a [`StmtKind::Local`], in source
    /// order, so every declarator keeps its own type (a C-style declarator
    /// suffix binds to the *name*, so `int a[], b;` declares an `int[]` and an
    /// `int`) and its own initializer. Java evaluates them left to right, and a
    /// later declarator may read an earlier one (`int a = 1, b = a + 1;`).
    Locals(Vec<Stmt>),
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
        /// The init clause, which Java lets be a comma-separated list — either
        /// several declarators of one type (`int i = 0, j = n`) or several
        /// expression statements. Empty when the clause is.
        init: Vec<Stmt>,
        cond: Option<Expr>,
        /// The update clause, likewise a comma-separated list (`i++, j--`).
        update: Vec<Stmt>,
        body: Vec<Stmt>,
    },
    /// `for (T x : arr) { .. }` — the enhanced (`for-each`) loop. Java defines
    /// it over arrays and `Iterable`; javars has no collections, so the source
    /// of elements is always an array. Kept as its own node rather than
    /// desugared in the parser because the index cursor and the array temp must
    /// be compiler-minted internal names (`#t0`), which are not expressible in
    /// the `Stmt` surface the parser produces.
    ForEach {
        /// The declared element type (`String`, `int`, `var`, …) — recorded for
        /// `/`-truncation typing and class-typed dispatch of the loop variable.
        ty: String,
        name: String,
        /// The array expression, evaluated exactly once before the loop.
        iter: Expr,
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
    /// `try { .. } catch (E e) { .. } … finally { .. }`. `catches` may be empty
    /// (a `try`/`finally`), and `finally_body` may be empty (a `try`/`catch`).
    Try {
        body: Vec<Stmt>,
        catches: Vec<CatchArm>,
        finally_body: Vec<Stmt>,
    },
    /// `throw <expr>;` — raise the (already-constructed) throwable.
    Throw(Expr),
    /// `yield <expr>;` — supply the value of the enclosing arrow-`switch`
    /// *expression* from inside a block arm. Parsed as a keyword only inside
    /// such a block, because `yield` is a contextual keyword everywhere else.
    Yield(Expr),
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

/// One `catch (Type name) { body }` clause. Arms are tested in source order and
/// the first whose type the thrown object `instanceof`-matches wins — Java's
/// rule, and the reason a `catch (Exception e)` written before a
/// `catch (RuntimeException e)` shadows it.
#[derive(Debug, Clone, PartialEq)]
pub struct CatchArm {
    /// The caught type names (`RuntimeException`, a user class, …). A
    /// multi-catch (`catch (A | B e)`) lists more than one; the arm runs when
    /// the throwable matches any of them.
    pub types: Vec<String>,
    /// The bound exception variable.
    pub name: String,
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
    /// `&=`
    BitAnd,
    /// `|=`
    BitOr,
    /// `^=`
    BitXor,
    /// `<<=`
    Shl,
    /// `>>=`
    Shr,
    /// `>>>=`
    Ushr,
}

/// A Java expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Int(i64),
    /// An `L`-suffixed integer literal. Runs on the same 64-bit value as
    /// [`Expr::Int`]; the distinct node exists only so the static type is
    /// `long`, which keeps the operation out of the 32-bit `int` wrap.
    Long(i64),
    Float(f64),
    /// An `f`-suffixed floating literal. Java types it `float`, a *32-bit*
    /// value: every operation on one rounds to `f32`, and its string form is the
    /// shortest decimal that round-trips through `f32` — `1.0f / 3.0f` is
    /// `0.33333334`, not the `double` `0.3333333333333333`.
    Float32(f64),
    Str(String),
    /// A `char` literal. Runs as its code point (an integer), because Java's
    /// `char` is an integral type that participates in numeric promotion; the
    /// distinct node exists so the static type is `char`, which is what turns
    /// the value back into a one-character String at a string conversion.
    Char(char),
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
    /// Post-increment / post-decrement of a variable (`i++`, `i--`). As an
    /// expression it yields the value the variable held *before* the update; the
    /// bool is `true` for `++`.
    PostIncDec {
        name: String,
        inc: bool,
    },
    /// Pre-increment / pre-decrement of a variable (`++i`, `--i`) — updates
    /// first and yields the *new* value. The bool is `true` for `++`.
    PreIncDec {
        name: String,
        inc: bool,
    },
    /// A cast, `(ty) expr`. Java's narrowing primitive conversions are real
    /// value changes (`(int) 3.9` is 3, `(byte) 200` is -56), so the target type
    /// is kept rather than erased; a reference cast is a no-op at runtime here,
    /// because the host heap already carries each object's class.
    Cast {
        ty: String,
        expr: Box<Expr>,
        line: u32,
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
        /// The element type when the literal named one (`new int[]{…}`), so
        /// `var a = new int[]{1, 2};` still infers `int[]` and its elements
        /// still truncate their division. `None` for the bare `{…}` form, whose
        /// type comes from the declaration it initializes.
        elem_ty: Option<String>,
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
    /// A lambda expression: `() -> e`, `x -> e`, `(a, b) -> { … }`. Compiled to
    /// a heap closure that captures the enclosing scope **by value**, because a
    /// lambda outlives the fusevm call frame whose slots hold those locals.
    /// Java only lets a lambda capture effectively-final locals, so a by-value
    /// snapshot is observationally exact.
    Lambda {
        /// Formal parameter names, in order. Declared types (`(int a) -> …`) are
        /// erased by the parser, exactly as they are for every other javars
        /// declaration.
        params: Vec<String>,
        body: LambdaBody,
        line: u32,
    },
    /// A method reference: `String::length`, `Integer::parseInt`, `obj::run`,
    /// `Point::new`. Desugared by the compiler into the equivalent
    /// [`Expr::Lambda`] once the referenced member's arity is known.
    MethodRef {
        /// The receiver expression — a value (`obj::run`, captured by the
        /// synthesized lambda) or a bare type name (`String::length`, which
        /// takes the receiver as its first parameter instead).
        recv: Box<Expr>,
        /// The referenced member, or `new` for a constructor reference.
        method: String,
        line: u32,
    },
    /// A `switch` *expression* in the arrow form:
    /// `int r = switch (x) { case 1, 2 -> 10; default -> 0; };`. Arms do not
    /// fall through, exactly one runs, and its value is the expression's value.
    SwitchExpr {
        disc: Box<Expr>,
        arms: Vec<SwitchArm>,
        line: u32,
    },
}

/// One arm of an arrow `switch`: `case A, B -> body` or `default -> body`.
#[derive(Debug, Clone, PartialEq)]
pub struct SwitchArm {
    /// The `case` label constant expressions selecting this arm.
    pub labels: Vec<Expr>,
    /// True when this arm carries `default`.
    pub is_default: bool,
    pub body: SwitchArmBody,
}

/// An arrow arm's right-hand side.
#[derive(Debug, Clone, PartialEq)]
pub enum SwitchArmBody {
    /// `case 1 -> expr;` — the expression is the arm's value.
    Expr(Box<Expr>),
    /// `case 1 -> { … yield v; }` — a block whose `yield` supplies the value.
    Block(Vec<Stmt>),
}

/// A lambda's body: a single expression (whose value is the result) or a block.
#[derive(Debug, Clone, PartialEq)]
pub enum LambdaBody {
    /// `x -> x + 1` — the expression is the returned value.
    Expr(Box<Expr>),
    /// `x -> { … }` — statements; a `return e;` inside supplies the value and a
    /// fall-off returns `null` (which is what a `void` functional interface
    /// wants).
    Block(Vec<Stmt>),
}

/// Unary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
    /// `~x` — bitwise complement of an integral operand.
    BitNot,
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
    /// `&` — bitwise AND on integers, non-short-circuit AND on booleans.
    BitAnd,
    /// `|` — bitwise OR on integers, non-short-circuit OR on booleans.
    BitOr,
    /// `^` — bitwise XOR on integers, "not equal" on booleans.
    BitXor,
    /// `<<` — left shift.
    Shl,
    /// `>>` — arithmetic (sign-propagating) right shift.
    Shr,
    /// `>>>` — logical (zero-fill) right shift.
    Ushr,
}
