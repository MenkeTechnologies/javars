//! The Java AST javars parses and lowers to fusevm bytecode.
//!
//! Slice 1 is a single-class, single-`main` subset: the parser accepts a
//! `public class Name { ... static void main(String[] args) { ... } }` shell
//! and models the statements and expressions inside `main`. User-defined
//! methods, fields, and multiple classes are parsed no further than the entry
//! point today (see `BUGS.md`); the AST is shaped to grow into them.

/// A parsed compilation unit: the entry class name and the body of its `main`.
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    /// The name of the class that declares `main` (informational; used by the
    /// `--dump-ast` output and error messages).
    pub class_name: String,
    /// The statements of `public static void main(String[] args)`.
    pub main: Vec<Stmt>,
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
    /// `for (init; cond; update) { .. }` — the C-style loop.
    For {
        init: Option<Box<Stmt>>,
        cond: Option<Expr>,
        update: Option<Box<Stmt>>,
        body: Vec<Stmt>,
    },
    /// `break;`
    Break,
    /// `continue;`
    Continue,
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
    /// `System.out.println(arg)` / `System.out.print(arg)`. Modeled directly
    /// (rather than as a general method call) until user methods land.
    Println {
        newline: bool,
        arg: Option<Box<Expr>>,
    },
    /// Post-increment / post-decrement of a variable (`i++`, `i--`), evaluated
    /// as a statement today. The bool is `true` for `++`.
    PostIncDec {
        name: String,
        inc: bool,
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
