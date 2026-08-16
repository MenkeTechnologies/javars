//! A recursive-descent parser with precedence-climbing for expressions.
//!
//! Grammar: a compilation unit is one or more `class`/`interface` declarations;
//! javars locates `public static void main(String[] args) { <body> }` as the
//! entry and parses every other member (fields, constructors, static and
//! instance methods) into the AST. Statements cover local decls, assignments,
//! `if`/`while`/`do`/`for` (C-style and enhanced)/`switch`, labeled
//! `break`/`continue`, `return`, `try`/`catch`/`finally`, `throw`,
//! `System.out.print[ln]`, and post-inc/dec.

use crate::ast::*;
use crate::lexer::{Tok, Token};

/// Parse Java `src` into a [`Program`].
pub fn parse(src: &str) -> Result<Program, String> {
    let tokens = crate::lexer::lex(src)?;
    let mut p = Parser {
        toks: tokens,
        pos: 0,
        uses_exceptions: false,
        uses_functional: false,
        switch_expr_depth: 0,
    };
    p.program()
}

/// One entry of a try-with-resources header. `decl` is the declared type and
/// initializer (`Type r = expr`); a bare `try (r)` naming an already-declared
/// variable carries `None` and only contributes the `close()` call.
struct Resource {
    decl: Option<(String, Expr)>,
    name: String,
    line: u32,
}

/// The located entry point: the class declaring `main`, its body, and the name
/// it gives its `String[]` parameter (if it declares one).
struct Entry {
    class_name: String,
    body: Vec<Stmt>,
    param: Option<String>,
}

struct Parser {
    toks: Vec<Token>,
    pos: usize,
    /// Set when a `try`, a `throw`, or a `new` of a modeled `java.lang`
    /// throwable is parsed — the signal that this unit needs the exception
    /// prelude and the compiler's exception lowering (see
    /// [`crate::ast::Program::uses_exceptions`]).
    uses_exceptions: bool,
    /// Set when a lambda, a method reference, or a declared
    /// `java.util.function` type is parsed — the signal that this unit needs the
    /// functional-interface prelude (see
    /// [`crate::ast::Program::uses_functional`]).
    uses_functional: bool,
    /// How many arrow-`switch` arm bodies enclose the cursor. `yield` is a
    /// keyword only inside one; everywhere else it is an ordinary identifier,
    /// which is Java's contextual rule (a variable or method named `yield`
    /// predates the keyword and must keep working).
    switch_expr_depth: usize,
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos].kind
    }

    fn line(&self) -> u32 {
        self.toks[self.pos].line
    }

    fn advance(&mut self) -> Tok {
        let t = self.toks[self.pos].kind.clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn eat(&mut self, want: &Tok) -> Result<(), String> {
        if std::mem::discriminant(self.peek()) == std::mem::discriminant(want) {
            self.advance();
            Ok(())
        } else {
            Err(format!(
                "javars: expected {want} but found {} on line {}",
                self.peek(),
                self.line()
            ))
        }
    }

    fn is(&self, t: &Tok) -> bool {
        std::mem::discriminant(self.peek()) == std::mem::discriminant(t)
    }

    /// A compilation unit: one or more top-level classes. Locates the class
    /// declaring `public static void main(String[] args)` (the entry) and
    /// flattens every class (top-level siblings and nested `static` classes)
    /// into [`Program::classes`]. Leading `import`/`package` lines are tolerated.
    fn program(&mut self) -> Result<Program, String> {
        // Skip package/import prologue lines.
        loop {
            match self.peek() {
                Tok::Ident(w) if w == "package" || w == "import" => {
                    while !self.is(&Tok::Semi) && !self.is(&Tok::Eof) {
                        self.advance();
                    }
                    self.eat(&Tok::Semi)?;
                }
                _ => break,
            }
        }
        let mut entry: Option<Entry> = None;
        let mut methods = Vec::new();
        let mut classes = Vec::new();
        // Top-level type declarations, in sequence.
        while !self.is(&Tok::Eof) {
            self.parse_class(None, &mut entry, &mut methods, &mut classes)?;
        }
        match entry {
            Some(entry) => Ok(Program {
                class_name: entry.class_name,
                main: entry.body,
                main_param: entry.param,
                methods,
                classes,
                uses_exceptions: self.uses_exceptions,
                uses_functional: self.uses_functional,
            }),
            None => Err(
                "javars: no class declares `public static void main(String[] args)`".to_string(),
            ),
        }
    }

    /// Parse one `[modifiers] class Name [extends Super] { members }`. The entry
    /// `main` body (when present) is written to `entry`; `static` methods go to
    /// the flat `methods` pool; the class itself (with its fields, constructors,
    /// and instance methods) is pushed to `classes`. Nested classes recurse into
    /// the same three sinks (flattened namespace).
    fn parse_class(
        &mut self,
        enclosing: Option<&str>,
        entry: &mut Option<Entry>,
        methods: &mut Vec<Method>,
        classes: &mut Vec<Class>,
    ) -> Result<(), String> {
        // modifiers (`public`, `static`, and ident-form `final`/`abstract`)
        while matches!(self.peek(), Tok::Public | Tok::Static)
            || matches!(self.peek(), Tok::Ident(w) if w == "final" || w == "abstract")
        {
            self.advance();
        }
        let line = self.line();
        // `class Name`, `interface Name`, or `enum Name`. Neither `interface`
        // nor `enum` is a reserved token in this lexer, so both are matched by
        // name here.
        let is_interface = matches!(self.peek(), Tok::Ident(w) if w == "interface");
        let is_enum = matches!(self.peek(), Tok::Ident(w) if w == "enum");
        let is_record = self.at_record();
        if is_interface || is_enum || is_record {
            self.advance();
        } else {
            self.eat(&Tok::Class)?;
        }
        let name = self.ident()?;
        // Java's binary name nests with `$`; the enclosing declaration passes
        // down its own binary name, so a doubly-nested type is `A$B$C`.
        let binary = match enclosing {
            Some(outer) => format!("{outer}${name}"),
            None => name.clone(),
        };
        // Optional generic type-parameter declaration `<T>`, `<T extends X>`,
        // `<K, V>` — erased (parsed and discarded).
        self.skip_generics();
        // A record's header carries its components: `record Point(int x, int y)`.
        let components = if is_record {
            self.eat(&Tok::LParen)?;
            let ps = self.params()?;
            self.eat(&Tok::RParen)?;
            ps
        } else {
            Vec::new()
        };
        // `extends`: a class has a single superclass; an interface may extend
        // several interfaces (collected as `interfaces`).
        let mut superclass = None;
        let mut interfaces = Vec::new();
        if matches!(self.peek(), Tok::Ident(w) if w == "extends") {
            self.advance();
            loop {
                let sup = self.type_name()?;
                // `class MyEx extends RuntimeException { … }` needs the modeled
                // throwable hierarchy just as much as a `throw` does — the
                // superclass is where `detailMessage`, `getMessage()` and
                // `toString()` come from. Without this, the prelude was skipped
                // for a program that only *subclasses* an exception, the
                // `extends` dangled, and `e.getMessage()` was "class `MyEx` has
                // no method `getMessage`" while `println(e)` rendered the
                // `Object` default `T$MyEx@1` instead of `T$MyEx: b`.
                if crate::prelude::is_throwable(&sup) {
                    self.uses_exceptions = true;
                }
                if is_interface {
                    interfaces.push(sup);
                } else {
                    superclass = Some(sup);
                }
                if self.is(&Tok::Comma) {
                    self.advance();
                } else {
                    break;
                }
            }
        }
        // `implements I, J` (classes only).
        if matches!(self.peek(), Tok::Ident(w) if w == "implements") {
            self.advance();
            loop {
                interfaces.push(self.type_name()?);
                if self.is(&Tok::Comma) {
                    self.advance();
                } else {
                    break;
                }
            }
        }
        self.eat(&Tok::LBrace)?;

        let mut fields = Vec::new();
        let mut static_fields = Vec::new();
        let mut static_init = Vec::new();
        let mut inst_init = Vec::new();
        let mut ctors = Vec::new();
        let mut inst_methods = Vec::new();
        // An `enum` body opens with its constant list; the ordinary members (if
        // any) follow the `;` that terminates it.
        let enum_constants = if is_enum {
            self.enum_constants(&name, classes)?
        } else {
            Vec::new()
        };
        while !self.is(&Tok::RBrace) && !self.is(&Tok::Eof) {
            if let Some(body) = self.try_static_block()? {
                static_init.extend(body);
            } else if let Some(found) = self.try_main()? {
                *entry = Some(Entry {
                    class_name: name.clone(),
                    ..found
                });
            } else if self.at_nested_class() {
                self.parse_class(Some(&binary), entry, methods, classes)?;
            } else if let Some(c) = self.try_compact_ctor(&name, is_record, &components)? {
                ctors.push(c);
            } else if let Some(c) = self.try_ctor(&name)? {
                ctors.push(c);
            } else if let Some((m, is_static)) = self.try_any_method(&name)? {
                if is_static {
                    methods.push(m);
                } else {
                    inst_methods.push(m);
                }
            } else if let Some((fs, is_static)) = self.try_fields()? {
                if is_static {
                    static_declaration(line, fs, &mut static_fields, &mut static_init);
                } else {
                    instance_declaration(line, fs, &mut fields, &mut inst_init);
                }
            } else if self.is(&Tok::LBrace) {
                // A bare `{ … }` at member position is an instance initializer
                // (JLS 8.6). It runs on every new instance in textual order with
                // the field initializers; `skip_member` used to drop it, which
                // silently lost everything it did.
                self.advance();
                inst_init.extend(self.block()?);
            } else {
                self.skip_member()?;
            }
        }
        self.eat(&Tok::RBrace)?;
        if is_enum {
            enum_members(line, &name, &mut fields, &mut inst_methods);
        }
        if is_record {
            record_members(
                line,
                &name,
                &components,
                &mut fields,
                &mut ctors,
                &mut inst_methods,
            );
        }
        classes.push(Class {
            name,
            binary,
            superclass,
            interfaces,
            is_interface,
            is_enum,
            is_record,
            enum_constants,
            fields,
            static_fields,
            static_init,
            inst_init,
            ctors,
            methods: inst_methods,
            line,
        });
        Ok(())
    }

    /// Parse a `static { … }` initializer block, returning its statements. The
    /// cursor is restored and `None` returned when the member is something else.
    fn try_static_block(&mut self) -> Result<Option<Vec<Stmt>>, String> {
        if !(matches!(self.peek(), Tok::Static)
            && matches!(self.toks[self.pos + 1].kind, Tok::LBrace))
        {
            return Ok(None);
        }
        self.advance(); // static
        self.advance(); // {
        Ok(Some(self.block()?))
    }

    /// Parse an `enum` body's constant list (the cursor just past `{`), and give
    /// the type the state and accessors every enum has.
    ///
    /// javars models an enum as an ordinary class with two synthesized fields —
    /// `#name` and `#ordinal`, whose `#` is not a legal Java identifier char, so
    /// they cannot collide with a user field — plus the `name()`, `ordinal()`,
    /// and `toString()` bodies that read them. The compiler builds one instance
    /// per constant before `main` runs (see `Compiler::emit_enum_prologue`), so
    /// everything else (virtual dispatch, `instanceof`, reference `==`, arrays)
    /// applies unchanged.
    ///
    /// A constant may carry constructor arguments (`EARTH(5.97e24)`), a class
    /// body (`PLUS { int apply(…) { … } }`), or both. Java specifies a constant
    /// with a body as an anonymous *subclass* of the enum, so the body is
    /// parsed into a real synthetic class pushed onto `classes`; its overrides
    /// are then reached by the same virtual dispatch every other subclass uses.
    fn enum_constants(
        &mut self,
        name: &str,
        classes: &mut Vec<Class>,
    ) -> Result<Vec<EnumConstant>, String> {
        let mut constants = Vec::new();
        while matches!(self.peek(), Tok::Ident(_)) {
            let line = self.line();
            let c = self.ident()?;
            let args = if self.is(&Tok::LParen) {
                self.call_args()?
            } else {
                Vec::new()
            };
            let body_class = if self.is(&Tok::LBrace) {
                self.advance();
                let cl = self.enum_constant_body(name, &c, line)?;
                let cname = cl.name.clone();
                classes.push(cl);
                Some(cname)
            } else {
                None
            };
            constants.push(EnumConstant {
                name: c,
                line,
                args,
                body_class,
            });
            if self.is(&Tok::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        // The `;` is optional when the body holds nothing but constants.
        if self.is(&Tok::Semi) {
            self.advance();
        }
        Ok(constants)
    }

    /// Parse an enum constant's `{ … }` body (the cursor just past the `{`) into
    /// the synthetic subclass that models it. A constant body may declare fields
    /// and methods but no constructor — the enum's own constructor runs against
    /// the arguments the constant supplied.
    fn enum_constant_body(
        &mut self,
        enum_name: &str,
        constant: &str,
        line: u32,
    ) -> Result<Class, String> {
        let name = enum_body_class(enum_name, constant);
        let mut fields = Vec::new();
        let mut static_fields = Vec::new();
        let mut static_init = Vec::new();
        let mut inst_init = Vec::new();
        let mut methods = Vec::new();
        while !self.is(&Tok::RBrace) && !self.is(&Tok::Eof) {
            if let Some((m, _)) = self.try_any_method(&name)? {
                methods.push(m);
            } else if let Some((fs, is_static)) = self.try_fields()? {
                if is_static {
                    static_declaration(line, fs, &mut static_fields, &mut static_init);
                } else {
                    instance_declaration(line, fs, &mut fields, &mut inst_init);
                }
            } else {
                return Err(format!(
                    "javars: unsupported member in the body of enum constant `{enum_name}.{constant}` (line {})",
                    self.line()
                ));
            }
        }
        self.eat(&Tok::RBrace)?;
        Ok(Class {
            // An enum constant body is a synthetic subclass javars mints; Java
            // names its own `Outer$Enum$1`, which is a counter this frontend does
            // not model, so the synthetic name stands in.
            binary: name.clone(),
            name,
            superclass: Some(enum_name.to_string()),
            interfaces: Vec::new(),
            is_interface: false,
            is_enum: false,
            // The body of an enum constant is an anonymous subclass of the enum,
            // never a record of its own.
            is_record: false,
            enum_constants: Vec::new(),
            fields,
            static_fields,
            static_init,
            inst_init,
            ctors: Vec::new(),
            methods,
            line,
        })
    }
}

/// True when `toks[j]` starts a `record Name(` header.
fn record_at(toks: &[Token], j: usize) -> bool {
    matches!(&toks[j].kind, Tok::Ident(w) if w == "record")
        && matches!(toks.get(j + 1).map(|t| &t.kind), Some(Tok::Ident(_)))
        && matches!(
            toks.get(j + 2).map(|t| &t.kind),
            Some(Tok::LParen | Tok::Lt)
        )
}

/// Give a `record` the members Java derives from its component list: one final
/// field per component, the canonical constructor, the component accessors, and
/// `toString`/`equals`. Anything the body declares itself wins — a record that
/// writes its own `toString()` keeps it.
///
/// Called once per record declaration, after its own members are parsed.
/// How a `record`'s derived `equals` compares one component, which is not `==`
/// for two of the three kinds.
///
/// JLS 8.10.3: the derived `equals` compares a `float` component with
/// `Float.compare(…) == 0`, a `double` with `Double.compare(…) == 0`, any other
/// primitive with `==`, and a reference with `Objects.equals`. The two special
/// cases are not pedantry — `Double.compare` is a *total* order, so
/// `new D(Double.NaN).equals(new D(Double.NaN))` is `true` and
/// `new D(0.0).equals(new D(-0.0))` is `false`, both the opposite of what `==`
/// answers. `Objects.equals` reaches a component class's own `equals`, where
/// `==` would compare handles; for a `String` component the two agree already
/// (javars's `==` on strings is by value) and for an array component they agree
/// too (arrays inherit identity `equals`), but routing every reference through
/// one rule is the rule Java states.
fn component_eq(ty: &str, mine: Expr, theirs: Expr, line: u32) -> Expr {
    match ty {
        "float" | "double" => {
            let owner = if ty == "float" { "Float" } else { "Double" };
            Expr::Binary {
                op: BinOp::Eq,
                lhs: Box::new(Expr::MethodCall {
                    recv: Box::new(Expr::Var(owner.to_string())),
                    method: "compare".to_string(),
                    args: vec![mine, theirs],
                    line,
                }),
                rhs: Box::new(Expr::Int(0)),
            }
        }
        "int" | "long" | "short" | "byte" | "char" | "boolean" => Expr::Binary {
            op: BinOp::Eq,
            lhs: Box::new(mine),
            rhs: Box::new(theirs),
        },
        _ => Expr::MethodCall {
            recv: Box::new(Expr::Var("Objects".to_string())),
            method: "equals".to_string(),
            args: vec![mine, theirs],
            line,
        },
    }
}

fn record_members(
    line: u32,
    name: &str,
    components: &[Param],
    fields: &mut Vec<FieldDecl>,
    ctors: &mut Vec<Ctor>,
    methods: &mut Vec<Method>,
) {
    for c in components {
        fields.push(FieldDecl {
            ty: c.ty.clone(),
            name: c.name.clone(),
            init: None,
            line,
        });
    }
    // The canonical constructor assigns each component to its field. A compact
    // constructor already contributed its validation body under the same
    // parameter list, so the assignments are appended to it — Java's order.
    let assigns: Vec<Stmt> = components
        .iter()
        .map(|c| {
            Stmt::new(
                line,
                StmtKind::FieldAssign {
                    recv: Expr::This,
                    name: c.name.clone(),
                    op: AssignOp::Assign,
                    value: Expr::Var(c.name.clone()),
                },
            )
        })
        .collect();
    let param_tys: Vec<&str> = components.iter().map(|c| c.ty.as_str()).collect();
    match ctors.iter_mut().find(|c| {
        c.params
            .iter()
            .map(|p| p.ty.as_str())
            .eq(param_tys.iter().copied())
    }) {
        Some(canonical) => canonical.body.extend(assigns),
        None => ctors.push(Ctor {
            params: components.to_vec(),
            body: assigns,
            line,
        }),
    }
    // One accessor per component.
    for c in components {
        if methods
            .iter()
            .any(|m| m.name == c.name && m.params.is_empty())
        {
            continue;
        }
        methods.push(Method {
            name: c.name.clone(),
            owner: name.to_string(),
            params: Vec::new(),
            ret: c.ty.clone(),
            body: vec![Stmt::new(
                line,
                StmtKind::Return(Some(Expr::Field {
                    recv: Box::new(Expr::This),
                    name: c.name.clone(),
                })),
            )],
            is_abstract: false,
            line,
        });
    }
    // `toString()` → `Point[x=1, y=2]`.
    if !methods
        .iter()
        .any(|m| m.name == "toString" && m.params.is_empty())
    {
        let mut text = Expr::Str(format!("{name}["));
        for (i, c) in components.iter().enumerate() {
            let sep = if i == 0 { "" } else { ", " };
            text = concat(text, Expr::Str(format!("{sep}{}=", c.name)));
            text = concat(
                text,
                Expr::Field {
                    recv: Box::new(Expr::This),
                    name: c.name.clone(),
                },
            );
        }
        text = concat(text, Expr::Str("]".to_string()));
        methods.push(Method {
            name: "toString".to_string(),
            owner: name.to_string(),
            params: Vec::new(),
            ret: "String".to_string(),
            body: vec![Stmt::new(line, StmtKind::Return(Some(text)))],
            is_abstract: false,
            line,
        });
    }
    // `equals(Object)` — component-wise, guarded by the type test.
    if !methods
        .iter()
        .any(|m| m.name == "equals" && m.params.len() == 1)
    {
        let other = "#other";
        let mut cond = Expr::Bool(true);
        for c in components {
            let mine = Expr::Field {
                recv: Box::new(Expr::This),
                name: c.name.clone(),
            };
            let theirs = Expr::Field {
                recv: Box::new(Expr::Var(other.to_string())),
                name: c.name.clone(),
            };
            cond = Expr::Binary {
                op: BinOp::And,
                lhs: Box::new(cond),
                rhs: Box::new(component_eq(&c.ty, mine, theirs, line)),
            };
        }
        methods.push(Method {
            name: "equals".to_string(),
            owner: name.to_string(),
            params: vec![Param {
                ty: "Object".to_string(),
                name: other.to_string(),
                varargs: false,
            }],
            ret: "boolean".to_string(),
            body: vec![
                Stmt::new(
                    line,
                    StmtKind::If {
                        cond: Expr::Unary {
                            op: UnOp::Not,
                            rhs: Box::new(Expr::InstanceOf {
                                expr: Box::new(Expr::Var(other.to_string())),
                                class: name.to_string(),
                            }),
                        },
                        then: vec![Stmt::new(line, StmtKind::Return(Some(Expr::Bool(false))))],
                        els: Vec::new(),
                    },
                ),
                Stmt::new(line, StmtKind::Return(Some(cond))),
            ],
            is_abstract: false,
            line,
        });
    }
}

/// `lhs + rhs` — the string-concatenation node the synthesized `toString`
/// bodies are built from.
fn concat(lhs: Expr, rhs: Expr) -> Expr {
    Expr::Binary {
        op: BinOp::Add,
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
    }
}

/// Split a `static` field declaration into its storage (one cell per field,
/// seeded with the type's default) and its initialization (an assignment run at
/// class-init time, in textual order with the `static { … }` blocks).
fn static_declaration(
    line: u32,
    decls: Vec<FieldDecl>,
    fields: &mut Vec<FieldDecl>,
    init: &mut Vec<Stmt>,
) {
    for f in decls {
        if let Some(value) = f.init.clone() {
            init.push(Stmt::new(
                line,
                StmtKind::Assign {
                    name: f.name.clone(),
                    op: AssignOp::Assign,
                    value,
                },
            ));
        }
        fields.push(FieldDecl { init: None, ..f });
    }
}

/// Split an instance field declaration into its storage (the field, seeded with
/// the type's default) and its initialization (a `this.f = …` assignment run in
/// textual order with the bare `{ … }` instance-initializer blocks). The mirror
/// of [`static_declaration`], one level down: an instance initializer needs a
/// bound `this`, which only the per-instance initialization subroutine has.
fn instance_declaration(
    line: u32,
    decls: Vec<FieldDecl>,
    fields: &mut Vec<FieldDecl>,
    init: &mut Vec<Stmt>,
) {
    for f in decls {
        if let Some(value) = f.init.clone() {
            init.push(Stmt::new(
                line,
                StmtKind::FieldAssign {
                    recv: Expr::This,
                    name: f.name.clone(),
                    op: AssignOp::Assign,
                    value,
                },
            ));
        }
        fields.push(FieldDecl { init: None, ..f });
    }
}

/// Give an `enum` its synthesized state and accessors, after its own members
/// have been parsed so a user-declared `toString()`/`name()`/`ordinal()` is left
/// alone. Called once per enum declaration.
fn enum_members(line: u32, owner: &str, fields: &mut Vec<FieldDecl>, methods: &mut Vec<Method>) {
    fields.push(FieldDecl {
        ty: "String".to_string(),
        name: ENUM_NAME.to_string(),
        init: None,
        line,
    });
    fields.push(FieldDecl {
        ty: "int".to_string(),
        name: ENUM_ORDINAL.to_string(),
        init: None,
        line,
    });
    let reader = |method: &str, field: &str, ret: &str| Method {
        name: method.to_string(),
        owner: owner.to_string(),
        params: Vec::new(),
        ret: ret.to_string(),
        body: vec![Stmt::new(
            line,
            StmtKind::Return(Some(Expr::Field {
                recv: Box::new(Expr::This),
                name: field.to_string(),
            })),
        )],
        is_abstract: false,
        line,
    };
    // `Enum.toString()` returns `name()`. A body that declares any of these
    // itself keeps its own version — the synthesized one is only a default.
    for (method, field, ret) in [
        ("name", ENUM_NAME, "String"),
        ("ordinal", ENUM_ORDINAL, "int"),
        ("toString", ENUM_NAME, "String"),
    ] {
        if !methods
            .iter()
            .any(|m| m.name == method && m.params.is_empty())
        {
            methods.push(reader(method, field, ret));
        }
    }
    // `Enum.compareTo` is the difference of the two ordinals — declaration
    // order — and is `final` in Java, so an enum never supplies its own. That
    // makes every enum a `Comparable`, which is what lets `Collections.sort` of
    // an enum list and a bare `a.compareTo(b)` between constants both work.
    if !methods
        .iter()
        .any(|m| m.name == "compareTo" && m.params.len() == 1)
    {
        methods.push(Method {
            name: "compareTo".to_string(),
            owner: owner.to_string(),
            params: vec![Param {
                ty: owner.to_string(),
                name: "other".to_string(),
                varargs: false,
            }],
            ret: "int".to_string(),
            body: vec![Stmt::new(
                line,
                StmtKind::Return(Some(Expr::Binary {
                    op: BinOp::Sub,
                    lhs: Box::new(Expr::Field {
                        recv: Box::new(Expr::This),
                        name: ENUM_ORDINAL.to_string(),
                    }),
                    rhs: Box::new(Expr::Field {
                        recv: Box::new(Expr::Var("other".to_string())),
                        name: ENUM_ORDINAL.to_string(),
                    }),
                })),
            )],
            is_abstract: false,
            line,
        });
    }
    // `Enum.equals` is identity — constants are singletons, so `==` is exactly
    // it, and `final` in Java (never overridden).
    if !methods
        .iter()
        .any(|m| m.name == "equals" && m.params.len() == 1)
    {
        methods.push(Method {
            name: "equals".to_string(),
            owner: owner.to_string(),
            params: vec![Param {
                ty: "Object".to_string(),
                name: "other".to_string(),
                varargs: false,
            }],
            ret: "boolean".to_string(),
            body: vec![Stmt::new(
                line,
                StmtKind::Return(Some(Expr::Binary {
                    op: BinOp::Eq,
                    lhs: Box::new(Expr::This),
                    rhs: Box::new(Expr::Var("other".to_string())),
                })),
            )],
            is_abstract: false,
            line,
        });
    }
}

impl Parser {
    /// Skip a generic type-parameter/argument group `< ... >` when the cursor is
    /// on `<`, matching nested `<`/`>` (Java erases these at runtime, so javars
    /// parses and discards them). A no-op when the cursor is not on `<`.
    fn skip_generics(&mut self) {
        if !self.is(&Tok::Lt) {
            return;
        }
        let mut depth = 0;
        loop {
            match self.peek() {
                Tok::Lt => depth += 1,
                // A nested type argument closes with `>>`/`>>>`, which the lexer
                // takes as one shift token — so a closer is worth however many
                // `>` it spells.
                t if t.generic_closers() > 0 => {
                    depth -= t.generic_closers();
                    if depth <= 0 {
                        self.advance();
                        return;
                    }
                }
                Tok::Eof => return,
                _ => {}
            }
            self.advance();
        }
    }

    /// True when the member at the cursor is a (possibly modifier-prefixed)
    /// nested `class`, `interface`, `enum`, or `record` declaration.
    fn at_nested_class(&self) -> bool {
        let mut j = self.pos;
        while matches!(self.toks[j].kind, Tok::Public | Tok::Static)
            || matches!(&self.toks[j].kind, Tok::Ident(w) if w == "final" || w == "abstract")
        {
            j += 1;
        }
        matches!(self.toks[j].kind, Tok::Class)
            || matches!(&self.toks[j].kind, Tok::Ident(w) if w == "interface" || w == "enum")
            || record_at(&self.toks, j)
    }

    /// True when the cursor is on a `record Name(` header. `record` is a
    /// contextual keyword in Java, so the shape — not the word alone — is what
    /// makes it one; a variable or method named `record` still parses.
    fn at_record(&self) -> bool {
        record_at(&self.toks, self.pos)
    }

    /// A record's *compact* constructor — `Point { … }`, no parameter list. Its
    /// body validates/normalizes the components; the field assignments Java
    /// appends afterwards are supplied by [`record_members`], so the canonical
    /// constructor synthesized here already carries the component parameters.
    fn try_compact_ctor(
        &mut self,
        class_name: &str,
        is_record: bool,
        components: &[Param],
    ) -> Result<Option<Ctor>, String> {
        if !is_record
            || !(matches!(self.peek(), Tok::Ident(n) if n == class_name)
                && matches!(self.toks[self.pos + 1].kind, Tok::LBrace))
        {
            return Ok(None);
        }
        let line = self.line();
        self.advance(); // class name
        self.advance(); // {
        let body = self.block()?;
        Ok(Some(Ctor {
            params: components.to_vec(),
            body,
            line,
        }))
    }

    /// If the cursor is at a constructor (`[public] Name(<params>) { ... }` where
    /// `Name` is the enclosing class), parse it; otherwise restore and return
    /// `None`. A constructor has no return type — the class name sits directly
    /// before the `(`.
    fn try_ctor(&mut self, class_name: &str) -> Result<Option<Ctor>, String> {
        let save = self.pos;
        while matches!(self.peek(), Tok::Public | Tok::Static)
            || matches!(self.peek(), Tok::Ident(w) if w == "final" || w == "abstract" || w == "private" || w == "protected")
        {
            self.advance();
        }
        let line = self.line();
        let is_ctor = matches!(self.peek(), Tok::Ident(n) if n == class_name)
            && matches!(&self.toks[self.pos + 1].kind, Tok::LParen);
        if !is_ctor {
            self.pos = save;
            return Ok(None);
        }
        self.advance(); // class name
        self.eat(&Tok::LParen)?;
        let params = self.params()?;
        self.eat(&Tok::RParen)?;
        self.skip_throws()?;
        self.eat(&Tok::LBrace)?;
        let body = self.block()?;
        Ok(Some(Ctor { params, body, line }))
    }

    /// Parse one or more field declarations sharing a type (`int x, y = 3;`).
    /// Restores and returns `None` if the member is not a field (e.g. a method
    /// the earlier probes already rejected — a `type name (` shape). The `bool`
    /// of the result reports whether the declaration carried `static`: a
    /// `static` field is one shared cell per class and an instance field is one
    /// per object, so the caller routes them to different sinks.
    fn try_fields(&mut self) -> Result<Option<(Vec<FieldDecl>, bool)>, String> {
        let save = self.pos;
        let mut saw_static = false;
        while matches!(self.peek(), Tok::Public | Tok::Static)
            || matches!(self.peek(), Tok::Ident(w) if w == "final" || w == "private" || w == "protected" || w == "volatile" || w == "transient")
        {
            if matches!(self.peek(), Tok::Static) {
                saw_static = true;
            }
            self.advance();
        }
        if !self.at_type() {
            self.pos = save;
            return Ok(None);
        }
        let base_ty = self.type_name()?;
        // A field is `type name` where name is not followed by `(` (that would be
        // a method the method-probe should have taken).
        let first_is_field = matches!(self.peek(), Tok::Ident(_))
            && !matches!(&self.toks[self.pos + 1].kind, Tok::LParen);
        if !first_is_field {
            self.pos = save;
            return Ok(None);
        }
        let mut out = Vec::new();
        loop {
            // Each declarator in `int a = 1, b = 2;` reports its own line, so a
            // duplicate is pointed at where it was written.
            let line = self.line();
            let (name, ty) = self.declarator(&base_ty)?;
            let init = if self.is(&Tok::Assign) {
                self.advance();
                Some(self.var_init()?)
            } else {
                None
            };
            out.push(FieldDecl {
                ty,
                name,
                init,
                line,
            });
            if self.is(&Tok::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        self.eat(&Tok::Semi)?;
        Ok(Some((out, saw_static)))
    }

    /// If the cursor is at a method (`[modifiers] <ret> name(<params>) { ... }`),
    /// parse it and report whether it was `static`. Otherwise restore the cursor
    /// and return `None` (fields and constructors are handled by their own
    /// probes). `main` is matched earlier by [`Parser::try_main`], so it never
    /// reaches here.
    fn try_any_method(&mut self, owner: &str) -> Result<Option<(Method, bool)>, String> {
        let save = self.pos;
        let mut saw_static = false;
        // Modifiers, including interface method modifiers (`default`, `abstract`)
        // and access/other qualifiers, in any order.
        while matches!(self.peek(), Tok::Public | Tok::Static | Tok::Default)
            || matches!(self.peek(), Tok::Ident(w) if w == "final" || w == "abstract" || w == "private" || w == "protected" || w == "synchronized" || w == "native")
        {
            if matches!(self.peek(), Tok::Static) {
                saw_static = true;
            }
            self.advance();
        }
        // Optional generic method type parameters `<T> T id(T x)` — erased.
        self.skip_generics();
        // A return type is required, so peeking a non-type here means this is not
        // a method (it is a field or constructor).
        if !self.at_type() {
            self.pos = save;
            return Ok(None);
        }
        let line = self.line();
        let ret = self.type_name()?;
        // `<ret> name (` — anything else (e.g. `int COUNT =`) is a field.
        let name = match self.peek().clone() {
            Tok::Ident(n) if matches!(&self.toks[self.pos + 1].kind, Tok::LParen) => {
                self.advance();
                n
            }
            _ => {
                self.pos = save;
                return Ok(None);
            }
        };
        self.eat(&Tok::LParen)?;
        let params = self.params()?;
        self.eat(&Tok::RParen)?;
        self.skip_throws()?;
        // An interface abstract method (or `abstract` class method) ends in `;`
        // with no body; a concrete/`default` method has a `{ ... }` block.
        let (body, is_abstract) = if self.is(&Tok::Semi) {
            self.advance();
            (Vec::new(), true)
        } else {
            self.eat(&Tok::LBrace)?;
            (self.block()?, false)
        };
        Ok(Some((
            Method {
                name,
                owner: owner.to_string(),
                params,
                ret,
                body,
                is_abstract,
                line,
            },
            saw_static,
        )))
    }

    /// Skip a `throws A, B` clause following a method/constructor parameter
    /// list. javars has no checked-exception analysis — an exception either
    /// reaches a handler or terminates the program — so the declared list is
    /// parsed and discarded, exactly as its runtime effect is nil.
    fn skip_throws(&mut self) -> Result<(), String> {
        if !matches!(self.peek(), Tok::Ident(w) if w == "throws") {
            return Ok(());
        }
        self.advance();
        loop {
            self.type_name()?;
            if self.is(&Tok::Comma) {
                self.advance();
            } else {
                return Ok(());
            }
        }
    }

    /// Parse a comma-separated formal parameter list `<type> <name>, ...`, the
    /// cursor sitting just past the opening `(`. Stops at the closing `)`.
    fn params(&mut self) -> Result<Vec<Param>, String> {
        let mut out = Vec::new();
        if self.is(&Tok::RParen) {
            return Ok(out);
        }
        loop {
            let mut ty = self.type_name()?;
            // A varargs parameter (`String... args`) *is* an array parameter —
            // inside the body it is a `String[]`. The dots additionally admit
            // Java's third resolution phase, so the flag rides along on the
            // parameter for the compiler's call-site packing.
            let mut varargs = false;
            if self.is(&Tok::Dot) && matches!(self.toks[self.pos + 1].kind, Tok::Dot) {
                self.advance();
                self.advance();
                self.eat(&Tok::Dot)?;
                ty.push_str("[]");
                varargs = true;
            }
            let (name, ty) = self.declarator(&ty)?;
            out.push(Param { ty, name, varargs });
            if self.is(&Tok::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        Ok(out)
    }

    /// True when the cursor sits on a type name (`void` or an identifier,
    /// possibly followed by `[]`).
    fn at_type(&self) -> bool {
        matches!(self.peek(), Tok::Void | Tok::Ident(_))
    }

    /// Read a type's name, dropping any package qualifier
    /// (`java.util.List` → `List`). javars keys every type on its simple name —
    /// there is no JDK to import from and no package graph — so the prefix
    /// carries no meaning, exactly as it does not for the qualified *static*
    /// call `java.util.Arrays.sort(x)`.
    ///
    /// A segment counts as a package only when it starts with a lowercase letter
    /// *and* is followed by another name. That is what keeps `Map.Entry` (a
    /// nested type, not a package) and the expression `obj.field` out of the
    /// loop, and what leaves `int... xs` alone (the dot is not followed by a
    /// name).
    fn simple_type_name(&mut self) -> Result<String, String> {
        let mut ty = self.ident()?;
        while ty.starts_with(|c: char| c.is_lowercase())
            && self.is(&Tok::Dot)
            && matches!(&self.toks[self.pos + 1].kind, Tok::Ident(_))
        {
            self.advance();
            ty = self.ident()?;
        }
        Ok(ty)
    }

    /// Parse one declarator — its name plus any C-style array suffix — and
    /// return `(name, type)`. Java lets the brackets sit after the *name*
    /// (`int a[]`) as well as after the type (`int[] a`), and the two spellings
    /// declare the same thing; the difference only shows in a multi-declarator
    /// statement, where a suffix binds to its own declarator alone, so
    /// `int a[], b;` declares an `int[]` and an `int`.
    fn declarator(&mut self, base_ty: &str) -> Result<(String, String), String> {
        let name = self.ident()?;
        let mut ty = base_ty.to_string();
        while self.is(&Tok::LBracket) && matches!(self.toks[self.pos + 1].kind, Tok::RBracket) {
            self.advance();
            self.advance();
            ty.push_str("[]");
        }
        Ok((name, ty))
    }

    /// Parse a declaration-position type: `void`, `int`, `String`, `int[]`, ….
    /// Trailing `[]` pairs are folded into the returned name (e.g. `int[]`).
    fn type_name(&mut self) -> Result<String, String> {
        let mut ty = self.simple_type_name()?;
        // Naming a functional interface pulls in the prelude that declares it,
        // even when the program never writes a lambda literal (a method
        // parameter of type `Runnable` is enough).
        if crate::prelude::is_functional(&ty) {
            self.uses_functional = true;
        }
        // Generic type arguments (`List<String>`, `Map<K, V>`) are erased — the
        // erased type is just the raw name.
        self.skip_generics();
        while self.is(&Tok::LBracket) {
            self.advance();
            self.eat(&Tok::RBracket)?;
            ty.push_str("[]");
        }
        Ok(ty)
    }

    /// If the cursor is at `public static void main(String[] args)`, parse its
    /// body and the name it gives its parameter. Otherwise leave the cursor
    /// untouched and return None. The enclosing class name is the caller's, so
    /// the returned [`Entry`] carries an empty one for it to fill in.
    fn try_main(&mut self) -> Result<Option<Entry>, String> {
        let save = self.pos;
        // modifiers in any order
        let mut saw_static = false;
        while matches!(self.peek(), Tok::Public | Tok::Static) {
            if matches!(self.peek(), Tok::Static) {
                saw_static = true;
            }
            self.advance();
        }
        let is_void = self.is(&Tok::Void);
        let named_main = matches!(self.peek(), Tok::Void)
            && matches!(&self.toks[self.pos + 1].kind, Tok::Ident(n) if n == "main");
        if !(saw_static && is_void && named_main) {
            self.pos = save;
            return Ok(None);
        }
        self.eat(&Tok::Void)?; // void
        self.ident()?; // main
        self.eat(&Tok::LParen)?;
        // `main` takes `String[] args` (or `String... args`, or nothing). The
        // name is what the compiler binds the real program arguments to.
        let params = self.params()?;
        self.eat(&Tok::RParen)?;
        let param = params.into_iter().next().map(|p| p.name);
        self.skip_throws()?;
        self.eat(&Tok::LBrace)?;
        let body = self.block()?;
        Ok(Some(Entry {
            class_name: String::new(),
            body,
            param,
        }))
    }

    /// Skip a non-`main` member by matching its braces (or a field `;`).
    fn skip_member(&mut self) -> Result<(), String> {
        while !self.is(&Tok::Eof) {
            match self.peek() {
                Tok::LBrace => {
                    let mut depth = 0;
                    loop {
                        match self.advance() {
                            Tok::LBrace => depth += 1,
                            Tok::RBrace => {
                                depth -= 1;
                                if depth == 0 {
                                    return Ok(());
                                }
                            }
                            Tok::Eof => return Ok(()),
                            _ => {}
                        }
                    }
                }
                Tok::Semi => {
                    self.advance();
                    return Ok(());
                }
                Tok::RBrace => return Ok(()),
                _ => {
                    self.advance();
                }
            }
        }
        Ok(())
    }

    /// Parse a `{ ... }` body already past the opening brace; consumes the `}`.
    fn block(&mut self) -> Result<Vec<Stmt>, String> {
        let mut out = Vec::new();
        while !self.is(&Tok::RBrace) && !self.is(&Tok::Eof) {
            out.push(self.statement()?);
        }
        self.eat(&Tok::RBrace)?;
        Ok(out)
    }

    /// Parse a `{ ... }` or a single statement into a statement list.
    fn braced_or_single(&mut self) -> Result<Vec<Stmt>, String> {
        if self.is(&Tok::LBrace) {
            self.advance();
            self.block()
        } else {
            Ok(vec![self.statement()?])
        }
    }

    /// Parse one statement, tagging it with the source line it starts on.
    fn statement(&mut self) -> Result<Stmt, String> {
        let line = self.line();
        Ok(Stmt::new(line, self.statement_kind()?))
    }

    fn statement_kind(&mut self) -> Result<StmtKind, String> {
        // A labeled statement `label: <stmt>` — an identifier immediately
        // followed by `:`. (A ternary is never a valid statement expression, so
        // a leading `Ident :` is unambiguously a label.)
        if let Tok::Ident(label) = self.peek().clone() {
            if matches!(self.toks[self.pos + 1].kind, Tok::Colon) {
                self.advance(); // label
                self.advance(); // :
                let body = self.statement()?;
                return Ok(StmtKind::Labeled {
                    label,
                    body: Box::new(body),
                });
            }
        }
        // `try` and `throw` are not reserved tokens in this lexer (they lex as
        // identifiers, like `instanceof` and `interface`), so they are matched
        // here by name before the general statement forms.
        if let Tok::Ident(w) = self.peek() {
            match w.as_str() {
                "try" => return self.try_stmt(),
                // `yield <expr>;` is a keyword only inside an arrow-`switch`
                // arm body — Java's contextual rule, which keeps a variable or
                // method named `yield` working everywhere else.
                "yield" if self.switch_expr_depth > 0 => {
                    self.advance();
                    let e = self.expression()?;
                    self.eat(&Tok::Semi)?;
                    return Ok(StmtKind::Yield(e));
                }
                "throw" => {
                    self.uses_exceptions = true;
                    self.advance();
                    let e = self.expression()?;
                    self.eat(&Tok::Semi)?;
                    return Ok(StmtKind::Throw(e));
                }
                // `synchronized (m) { … }` — a statement here, but also a method
                // modifier, so the `(` is what tells the two apart.
                "synchronized" if matches!(self.toks[self.pos + 1].kind, Tok::LParen) => {
                    return self.synchronized_stmt()
                }
                _ => {}
            }
        }
        match self.peek() {
            Tok::If => self.if_stmt(),
            Tok::While => self.while_stmt(),
            Tok::Do => self.do_stmt(),
            Tok::For => self.for_stmt(),
            Tok::Switch => self.switch_stmt(),
            Tok::Return => {
                // `return;` or `return <expr>;`. The compiler resolves the
                // context: a value return is valid in a method but rejected in
                // `void main`.
                self.advance();
                if self.is(&Tok::Semi) {
                    self.advance();
                    Ok(StmtKind::Return(None))
                } else {
                    let e = self.expression()?;
                    self.eat(&Tok::Semi)?;
                    Ok(StmtKind::Return(Some(e)))
                }
            }
            Tok::Break => {
                self.advance();
                let label = self.optional_label();
                self.eat(&Tok::Semi)?;
                Ok(StmtKind::Break(label))
            }
            Tok::Continue => {
                self.advance();
                let label = self.optional_label();
                self.eat(&Tok::Semi)?;
                Ok(StmtKind::Continue(label))
            }
            Tok::LBrace => {
                self.advance();
                // a bare block: flatten into a single synthetic if-true. Slice 1
                // has no lexical scopes, so inlining is behavior-preserving.
                let body = self.block()?;
                Ok(StmtKind::If {
                    cond: Expr::Bool(true),
                    then: body,
                    els: vec![],
                })
            }
            _ => self.simple_statement(true),
        }
    }

    /// Local decl, assignment, or expression statement. `expect_semi` consumes
    /// the trailing `;` (false for the `for` init/update clauses).
    fn simple_statement(&mut self, expect_semi: bool) -> Result<StmtKind, String> {
        // A local declaration starts with a type keyword/ident followed by an
        // identifier: `int x`, `String s`, `var v`, `int[] a`, `Point p`.
        if self.looks_like_decl() {
            if matches!(self.peek(), Tok::Ident(w) if w == "final") {
                self.advance();
            }
            let base_ty = self.type_name()?;
            // Every declarator of the statement, in source order: `int a = 1,
            // b = 2;` is two, and `int a[], b;` gives them different types.
            let mut decls = Vec::new();
            loop {
                let line = self.line();
                let (name, ty) = self.declarator(&base_ty)?;
                let init = if self.is(&Tok::Assign) {
                    self.advance();
                    Some(self.var_init()?)
                } else {
                    None
                };
                decls.push(Stmt::new(line, StmtKind::Local { ty, name, init }));
                if !self.is(&Tok::Comma) {
                    break;
                }
                self.advance();
            }
            // `var a = 1, b = 2;` is not Java: each `var` declarator would need
            // its own inferred type, so the language forbids the compound form.
            if base_ty == "var" && decls.len() > 1 {
                return Err(format!(
                    "javars: 'var' is not allowed in a compound declaration on line {}",
                    self.line()
                ));
            }
            if expect_semi {
                self.eat(&Tok::Semi)?;
            }
            // A one-declarator statement stays the plain `Local` node, so the
            // overwhelmingly common shape emits exactly the bytecode it did.
            return Ok(if decls.len() == 1 {
                decls.pop().expect("length checked").kind
            } else {
                StmtKind::Locals(decls)
            });
        }

        // Bare-variable fast paths: `x <op>= e` and `x++`/`x--`. Handled before
        // the general expression parse because `primary` rejects a bare `x++` in
        // value position (post-inc/dec is a statement, not an expression here).
        if let Tok::Ident(name) = self.peek().clone() {
            let next = &self.toks[self.pos + 1].kind;
            if let Some(op) = assign_op(next) {
                self.advance(); // name
                self.advance(); // op
                let value = self.expression()?;
                if expect_semi {
                    self.eat(&Tok::Semi)?;
                }
                return Ok(StmtKind::Assign { name, op, value });
            }
            if matches!(next, Tok::PlusPlus | Tok::MinusMinus) {
                let inc = matches!(next, Tok::PlusPlus);
                self.advance(); // name
                self.advance(); // ++/--
                if expect_semi {
                    self.eat(&Tok::Semi)?;
                }
                return Ok(StmtKind::Expr(Expr::PostIncDec { name, inc }));
            }
        }

        // Otherwise parse an lvalue/expression, then decide by what follows: an
        // `a[i] = …` / `obj.f = …` assignment, an `a[i]++` post-inc, or a plain
        // expression statement (`System.out.println(...)`, a call, `new C(...)`).
        let lhs = self.expression()?;
        if let Some(op) = assign_op(self.peek()) {
            self.advance();
            let value = self.expression()?;
            if expect_semi {
                self.eat(&Tok::Semi)?;
            }
            return self.make_assign(lhs, op, value);
        }
        if matches!(self.peek(), Tok::PlusPlus | Tok::MinusMinus) {
            let inc = matches!(self.peek(), Tok::PlusPlus);
            self.advance();
            if expect_semi {
                self.eat(&Tok::Semi)?;
            }
            // The discarded statement result makes `a[i]++` == `a[i] += 1`.
            let op = if inc { AssignOp::Add } else { AssignOp::Sub };
            return self.make_assign(lhs, op, Expr::Int(1));
        }
        if expect_semi {
            self.eat(&Tok::Semi)?;
        }
        Ok(StmtKind::Expr(lhs))
    }

    /// Build an assignment statement from a parsed lvalue expression, rejecting
    /// non-assignable left-hand sides.
    fn make_assign(&self, lhs: Expr, op: AssignOp, value: Expr) -> Result<StmtKind, String> {
        match lhs {
            Expr::Var(name) => Ok(StmtKind::Assign { name, op, value }),
            Expr::Index { array, index } => Ok(StmtKind::IndexAssign {
                array: *array,
                index: *index,
                op,
                value,
            }),
            Expr::Field { recv, name } => Ok(StmtKind::FieldAssign {
                recv: *recv,
                name,
                op,
                value,
            }),
            other => Err(format!(
                "javars: `{other:?}` is not an assignable target on line {}",
                self.line()
            )),
        }
    }

    /// Parse a variable/field initializer: an array literal `{...}` when the
    /// cursor is on `{`, otherwise an ordinary expression.
    fn var_init(&mut self) -> Result<Expr, String> {
        if self.is(&Tok::LBrace) {
            self.advance();
            let elems = self.array_lit_elems()?;
            Ok(Expr::ArrayLit {
                elems,
                elem_ty: None,
            })
        } else {
            self.expression()
        }
    }

    /// Heuristic: two identifiers in a row (`Type name`) with the type not being
    /// a value keyword — a local declaration. Array types (`int[] a`) are
    /// handled by skipping the `[]` before the name.
    fn looks_like_decl(&self) -> bool {
        // `final` is a legal modifier on a local declaration (and the classic
        // spelling of a lambda-capturable local). It constrains reassignment,
        // which `javac` has already checked, so javars parses and drops it.
        let mut start = self.pos;
        if matches!(&self.toks[start].kind, Tok::Ident(w) if w == "final")
            && matches!(self.toks[start + 1].kind, Tok::Ident(_))
        {
            start += 1;
        }
        let t0 = &self.toks[start].kind;
        let is_type = matches!(t0, Tok::Ident(_));
        if !is_type {
            return false;
        }
        let mut j = start + 1;
        // Step over a package qualifier (`java.util.List xs`), on the same
        // lowercase-segment rule [`Parser::simple_type_name`] uses. An
        // expression like `obj.field = 1` walks the same path and then fails the
        // trailing-identifier test below, so it is still not a declaration.
        while matches!(&self.toks[j - 1].kind, Tok::Ident(w) if w.starts_with(|c: char| c.is_lowercase()))
            && matches!(self.toks[j].kind, Tok::Dot)
            && matches!(self.toks.get(j + 1).map(|t| &t.kind), Some(Tok::Ident(_)))
        {
            j += 2;
        }
        // optional generic type arguments on the type (`List<String> xs`)
        if matches!(self.toks[j].kind, Tok::Lt) {
            let mut depth = 0;
            while j < self.toks.len() {
                match &self.toks[j].kind {
                    Tok::Lt => depth += 1,
                    t if t.generic_closers() > 0 => {
                        depth -= t.generic_closers();
                        if depth <= 0 {
                            j += 1;
                            break;
                        }
                    }
                    Tok::Eof => return false,
                    _ => {}
                }
                j += 1;
            }
        }
        // optional array brackets on the type
        while matches!(self.toks[j].kind, Tok::LBracket)
            && matches!(self.toks.get(j + 1).map(|t| &t.kind), Some(Tok::RBracket))
        {
            j += 2;
        }
        matches!(self.toks[j].kind, Tok::Ident(_))
    }

    fn if_stmt(&mut self) -> Result<StmtKind, String> {
        self.eat(&Tok::If)?;
        self.eat(&Tok::LParen)?;
        let cond = self.expression()?;
        self.eat(&Tok::RParen)?;
        let then = self.braced_or_single()?;
        let els = if self.is(&Tok::Else) {
            self.advance();
            self.braced_or_single()?
        } else {
            vec![]
        };
        Ok(StmtKind::If { cond, then, els })
    }

    fn while_stmt(&mut self) -> Result<StmtKind, String> {
        self.eat(&Tok::While)?;
        self.eat(&Tok::LParen)?;
        let cond = self.expression()?;
        self.eat(&Tok::RParen)?;
        let body = self.braced_or_single()?;
        Ok(StmtKind::While { cond, body })
    }

    /// Both `for` forms: the enhanced `for (T x : arr)` and the C-style
    /// `for (init; cond; update)`. The two are told apart by a probe for the
    /// `<type> <name> :` header — a C-style init clause always has `=` or `;`
    /// where the enhanced form has `:`, so the probe is unambiguous.
    fn for_stmt(&mut self) -> Result<StmtKind, String> {
        self.eat(&Tok::For)?;
        self.eat(&Tok::LParen)?;
        if let Some((ty, name)) = self.try_foreach_header()? {
            let iter = self.expression()?;
            self.eat(&Tok::RParen)?;
            let body = self.braced_or_single()?;
            return Ok(StmtKind::ForEach {
                ty,
                name,
                iter,
                body,
            });
        }
        let init = self.for_clause(&Tok::Semi)?;
        self.eat(&Tok::Semi)?;
        let cond = if self.is(&Tok::Semi) {
            None
        } else {
            Some(self.expression()?)
        };
        self.eat(&Tok::Semi)?;
        let update = self.for_clause(&Tok::RParen)?;
        self.eat(&Tok::RParen)?;
        let body = self.braced_or_single()?;
        Ok(StmtKind::For {
            init,
            cond,
            update,
            body,
        })
    }

    /// Parse a `for` header's init or update clause — a comma-separated list of
    /// simple statements, terminated by `end` (`;` for init, `)` for update).
    ///
    /// Java lets the init clause declare several variables of one type
    /// (`for (int i = 0, j = n; …)`), where only the first declarator names the
    /// type; the rest reuse it, so the type is carried across the commas.
    fn for_clause(&mut self, end: &Tok) -> Result<Vec<Stmt>, String> {
        let mut out = Vec::new();
        if self.is(end) {
            return Ok(out);
        }
        let line = self.line();
        out.push(Stmt::new(line, self.simple_statement(false)?));
        // A declaration clause has already swallowed its own comma-separated
        // declarators (`for (int i = 0, n = 3; …)` is one `Locals` statement),
        // so a comma left here separates statement *expressions*: `i++, j--`.
        while self.is(&Tok::Comma) {
            self.advance();
            let line = self.line();
            out.push(Stmt::new(line, self.simple_statement(false)?));
        }
        Ok(out)
    }

    /// Parse `synchronized (monitor) { body }`.
    ///
    /// javars runs one thread, so holding the monitor is unobservable and the
    /// lock itself is not modeled. What *is* observable is the rest of the
    /// statement's semantics, and those are kept: the monitor expression is
    /// evaluated exactly once (so `synchronized (next())` still calls `next`),
    /// and a `null` monitor throws `NullPointerException` before the body runs.
    /// Both fall out of desugaring to `if (m != null) { body } else { throw }` —
    /// the same shape try-with-resources uses, so no new node reaches the
    /// compiler.
    ///
    /// Java's own message names where the null came from
    /// (`… because the return value of "C.give()" is null`), which is the
    /// bytecode-slot provenance javars cannot reproduce; it keeps the operation
    /// half, exactly as every other javars NPE message does.
    fn synchronized_stmt(&mut self) -> Result<StmtKind, String> {
        self.uses_exceptions = true;
        let line = self.line();
        self.advance(); // `synchronized`
        self.eat(&Tok::LParen)?;
        let monitor = self.expression()?;
        self.eat(&Tok::RParen)?;
        self.eat(&Tok::LBrace)?;
        let body = self.block()?;
        let npe = Stmt::new(
            line,
            StmtKind::Throw(Expr::NewObject {
                class: "NullPointerException".to_string(),
                args: vec![Expr::Str(
                    "Cannot enter synchronized block because the monitor is null".to_string(),
                )],
                line,
            }),
        );
        Ok(StmtKind::If {
            cond: Expr::Binary {
                op: BinOp::Ne,
                lhs: Box::new(monitor),
                rhs: Box::new(Expr::Var("null".to_string())),
            },
            then: body,
            els: vec![npe],
        })
    }

    /// Parse `try [(resources)] { .. } catch (Type name) { .. }* [finally { .. }]`.
    ///
    /// Try-with-resources is desugared here (see [`Parser::with_resources`]) so
    /// the compiler only ever sees the plain `try`/`catch`/`finally` node.
    /// Multi-catch (`catch (A | B e)`) does not reach this point: the lexer has
    /// no single `|` token, so it fails earlier with a lexical error.
    fn try_stmt(&mut self) -> Result<StmtKind, String> {
        self.uses_exceptions = true;
        self.advance(); // `try`
        let resources = if self.is(&Tok::LParen) {
            self.resource_spec()?
        } else {
            Vec::new()
        };
        self.eat(&Tok::LBrace)?;
        let body = self.block()?;
        let mut catches = Vec::new();
        while matches!(self.peek(), Tok::Ident(w) if w == "catch") {
            self.advance();
            self.eat(&Tok::LParen)?;
            // `final` is legal on a catch parameter and carries no meaning here.
            if matches!(self.peek(), Tok::Ident(w) if w == "final") {
                self.advance();
            }
            // `catch (A | B e)` — a multi-catch lists alternative types before
            // the one bound variable.
            let mut types = vec![self.type_name()?];
            while self.is(&Tok::Pipe) {
                self.advance();
                types.push(self.type_name()?);
            }
            let name = self.ident()?;
            self.eat(&Tok::RParen)?;
            self.eat(&Tok::LBrace)?;
            let arm_body = self.block()?;
            catches.push(CatchArm {
                types,
                name,
                body: arm_body,
            });
        }
        let finally_body = if matches!(self.peek(), Tok::Ident(w) if w == "finally") {
            self.advance();
            self.eat(&Tok::LBrace)?;
            self.block()?
        } else {
            Vec::new()
        };
        if catches.is_empty() && finally_body.is_empty() && resources.is_empty() {
            return Err(format!(
                "javars: `try` needs a `catch` or a `finally` (line {})",
                self.line()
            ));
        }
        if resources.is_empty() {
            return Ok(StmtKind::Try {
                body,
                catches,
                finally_body,
            });
        }
        Ok(Parser::with_resources(
            resources,
            body,
            catches,
            finally_body,
        ))
    }

    /// Parse a try-with-resources header `( <resource> ; … [;] )`, the cursor on
    /// the `(`. A resource is `Type name = expr`, `var name = expr`, or (Java 9)
    /// a bare name already holding an effectively-final reference.
    fn resource_spec(&mut self) -> Result<Vec<Resource>, String> {
        self.eat(&Tok::LParen)?;
        let mut out = Vec::new();
        loop {
            if self.is(&Tok::RParen) {
                self.advance();
                break;
            }
            // `final` on a resource declaration carries no meaning here.
            if matches!(self.peek(), Tok::Ident(w) if w == "final") {
                self.advance();
            }
            let line = self.line();
            // `try (existing)` — a bare name, no declaration.
            if matches!(self.peek(), Tok::Ident(_))
                && matches!(self.toks[self.pos + 1].kind, Tok::RParen | Tok::Semi)
            {
                let name = self.ident()?;
                out.push(Resource {
                    decl: None,
                    name,
                    line,
                });
            } else {
                let ty = self.type_name()?;
                let name = self.ident()?;
                self.eat(&Tok::Assign)?;
                let init = self.expression()?;
                out.push(Resource {
                    decl: Some((ty, init)),
                    name,
                    line,
                });
            }
            if self.is(&Tok::Semi) {
                self.advance();
            }
        }
        if out.is_empty() {
            return Err(format!(
                "javars: try-with-resources needs at least one resource (line {})",
                self.line()
            ));
        }
        Ok(out)
    }

    /// Desugar try-with-resources into the plain `try` nodes Java itself
    /// specifies it as: each resource declaration is followed by a
    /// `try { … } finally { if (r != null) r.close(); }` wrapping everything
    /// inside it, so resources close in reverse declaration order and close
    /// before any `catch`/`finally` of the outer statement runs.
    ///
    /// Java additionally records a close-thrown exception as *suppressed* on the
    /// body's exception; javars has no `addSuppressed`, so a throwing `close`
    /// replaces it (documented in BUGS.md).
    fn with_resources(
        resources: Vec<Resource>,
        body: Vec<Stmt>,
        catches: Vec<CatchArm>,
        finally_body: Vec<Stmt>,
    ) -> StmtKind {
        let mut inner = body;
        for r in resources.into_iter().rev() {
            let close = Stmt::new(
                r.line,
                StmtKind::If {
                    cond: Expr::Binary {
                        op: BinOp::Ne,
                        lhs: Box::new(Expr::Var(r.name.clone())),
                        rhs: Box::new(Expr::Var("null".to_string())),
                    },
                    then: vec![Stmt::new(
                        r.line,
                        StmtKind::Expr(Expr::MethodCall {
                            recv: Box::new(Expr::Var(r.name.clone())),
                            method: "close".to_string(),
                            args: Vec::new(),
                            line: r.line,
                        }),
                    )],
                    els: Vec::new(),
                },
            );
            let guarded = Stmt::new(
                r.line,
                StmtKind::Try {
                    body: inner,
                    catches: Vec::new(),
                    finally_body: vec![close],
                },
            );
            inner = match r.decl {
                Some((ty, init)) => vec![
                    Stmt::new(
                        r.line,
                        StmtKind::Local {
                            ty,
                            name: r.name,
                            init: Some(init),
                        },
                    ),
                    guarded,
                ],
                None => vec![guarded],
            };
        }
        if catches.is_empty() && finally_body.is_empty() {
            // No outer handler: the declarations + guarded body are just a
            // block, which this parser models as an always-taken `if`.
            return StmtKind::If {
                cond: Expr::Bool(true),
                then: inner,
                els: Vec::new(),
            };
        }
        StmtKind::Try {
            body: inner,
            catches,
            finally_body,
        }
    }

    /// Probe for an enhanced-`for` header `<type> <name> :`, the cursor sitting
    /// just past the `(`. On a match the cursor is left on the iterable
    /// expression and the declared type + variable name are returned; otherwise
    /// the cursor is restored untouched so the C-style clauses can parse.
    fn try_foreach_header(&mut self) -> Result<Option<(String, String)>, String> {
        if !self.looks_like_decl() {
            return Ok(None);
        }
        let save = self.pos;
        // `for (final String s : xs)` — the modifier is parsed and dropped.
        if matches!(self.peek(), Tok::Ident(w) if w == "final") {
            self.advance();
        }
        let ty = self.type_name()?;
        let name = self.ident()?;
        if self.is(&Tok::Colon) {
            self.advance();
            return Ok(Some((ty, name)));
        }
        self.pos = save;
        Ok(None)
    }

    /// Consume an optional label identifier following `break`/`continue`
    /// (`break outer;`). Returns `None` when the next token is the `;`.
    fn optional_label(&mut self) -> Option<String> {
        if let Tok::Ident(name) = self.peek().clone() {
            self.advance();
            Some(name)
        } else {
            None
        }
    }

    fn do_stmt(&mut self) -> Result<StmtKind, String> {
        self.eat(&Tok::Do)?;
        let body = self.braced_or_single()?;
        self.eat(&Tok::While)?;
        self.eat(&Tok::LParen)?;
        let cond = self.expression()?;
        self.eat(&Tok::RParen)?;
        self.eat(&Tok::Semi)?;
        Ok(StmtKind::DoWhile { body, cond })
    }

    /// Parse `switch (disc) { (case E:|default:)+ stmts ... }`. Consecutive
    /// `case`/`default` labels with no statements between them share the body
    /// that follows (`case 1: case 2: body`), forming one [`SwitchGroup`].
    fn switch_stmt(&mut self) -> Result<StmtKind, String> {
        // An arrow `switch` used as a *statement* is the expression form with
        // its value discarded, and that is exactly how it is lowered: one shared
        // parser, one shared lowering, no fall-through logic to keep in step.
        if self.at_arrow_switch() {
            return Ok(StmtKind::Expr(self.switch_expr()?));
        }
        self.eat(&Tok::Switch)?;
        self.eat(&Tok::LParen)?;
        let disc = self.expression()?;
        self.eat(&Tok::RParen)?;
        self.eat(&Tok::LBrace)?;
        let mut groups = Vec::new();
        while !self.is(&Tok::RBrace) && !self.is(&Tok::Eof) {
            // Collect the run of labels that introduce this group.
            let mut labels = Vec::new();
            let mut is_default = false;
            loop {
                if self.is(&Tok::Case) {
                    self.advance();
                    // Case labels are constant expressions; parse below the
                    // ternary so the group-terminating `:` is not swallowed.
                    labels.push(self.binary(0)?);
                    self.eat(&Tok::Colon)?;
                } else if self.is(&Tok::Default) {
                    self.advance();
                    self.eat(&Tok::Colon)?;
                    is_default = true;
                } else {
                    break;
                }
            }
            if labels.is_empty() && !is_default {
                return Err(format!(
                    "javars: expected `case` or `default` in switch body (line {})",
                    self.line()
                ));
            }
            // Statements up to the next label or the closing brace.
            let mut body = Vec::new();
            while !self.is(&Tok::Case)
                && !self.is(&Tok::Default)
                && !self.is(&Tok::RBrace)
                && !self.is(&Tok::Eof)
            {
                body.push(self.statement()?);
            }
            groups.push(SwitchGroup {
                labels,
                is_default,
                body,
            });
        }
        self.eat(&Tok::RBrace)?;
        Ok(StmtKind::Switch { disc, groups })
    }

    /// True when the `switch` at the cursor uses the arrow form. The two forms
    /// differ only at the first label's terminator (`->` vs `:`), so the decision
    /// needs a scan past the discriminant's parentheses to that token.
    fn at_arrow_switch(&self) -> bool {
        if !self.is(&Tok::Switch) {
            return false;
        }
        let mut j = self.pos + 1;
        let mut depth = 0usize;
        // Skip `( disc )`.
        loop {
            match self.toks.get(j).map(|t| &t.kind) {
                Some(Tok::LParen) => depth += 1,
                Some(Tok::RParen) => {
                    depth -= 1;
                    if depth == 0 {
                        j += 1;
                        break;
                    }
                }
                Some(Tok::Eof) | None => return false,
                _ => {}
            }
            j += 1;
        }
        // `{ case <labels> ->` / `{ default ->`.
        if !matches!(self.toks.get(j).map(|t| &t.kind), Some(Tok::LBrace)) {
            return false;
        }
        j += 1;
        let mut depth = 0usize;
        loop {
            match self.toks.get(j).map(|t| &t.kind) {
                Some(Tok::Arrow) if depth == 0 => return true,
                Some(Tok::Colon) if depth == 0 => return false,
                // A label may itself contain parentheses; only a top-level
                // terminator decides the form.
                Some(Tok::LParen) => depth += 1,
                Some(Tok::RParen) => depth = depth.saturating_sub(1),
                Some(Tok::RBrace) | Some(Tok::Eof) | None => return false,
                _ => {}
            }
            j += 1;
        }
    }

    /// Parse an arrow `switch` expression, the cursor on `switch`.
    fn switch_expr(&mut self) -> Result<Expr, String> {
        let line = self.line();
        self.eat(&Tok::Switch)?;
        self.eat(&Tok::LParen)?;
        let disc = self.expression()?;
        self.eat(&Tok::RParen)?;
        self.eat(&Tok::LBrace)?;
        let mut arms = Vec::new();
        // Labels of colon-form arms with an empty body, waiting to be folded
        // into the next arm that has one (`case 1: case 2: yield x;`).
        let mut grouped: Vec<Expr> = Vec::new();
        while !self.is(&Tok::RBrace) && !self.is(&Tok::Eof) {
            let mut labels = Vec::new();
            let mut is_default = false;
            if self.is(&Tok::Default) {
                self.advance();
                is_default = true;
            } else {
                self.eat(&Tok::Case)?;
                loop {
                    // Below the ternary, so the arm's `->` is not swallowed.
                    labels.push(self.binary(0)?);
                    if self.is(&Tok::Comma) {
                        self.advance();
                    } else {
                        break;
                    }
                }
            }
            // Java allows both arm forms in a switch *expression*: `case X ->`
            // and the classic `case X:`. In the colon form every arm must
            // complete with `yield` or `throw`, so there is no fall-through
            // value to model — only an empty arm falls through, and that just
            // groups its labels onto the next one.
            if self.is(&Tok::Colon) {
                self.advance();
                self.switch_expr_depth += 1;
                let mut stmts = Vec::new();
                while !self.is(&Tok::Case)
                    && !self.is(&Tok::Default)
                    && !self.is(&Tok::RBrace)
                    && !self.is(&Tok::Eof)
                {
                    stmts.push(self.statement()?);
                }
                self.switch_expr_depth -= 1;
                if stmts.is_empty() && !is_default {
                    grouped.extend(labels);
                    continue;
                }
                labels.splice(0..0, grouped.drain(..));
                arms.push(SwitchArm {
                    labels,
                    is_default,
                    body: SwitchArmBody::Block(stmts),
                });
                continue;
            }
            self.eat(&Tok::Arrow)?;
            // Inside an arm body `yield` is a keyword; outside it stays an
            // ordinary identifier, which is what Java's contextual rule says.
            self.switch_expr_depth += 1;
            let body = if self.is(&Tok::LBrace) {
                self.advance();
                SwitchArmBody::Block(self.block()?)
            } else if matches!(self.peek(), Tok::Ident(w) if w == "throw") {
                // `case X -> throw new Foo();` — the one statement form an
                // unbraced arm may take. It never produces a value.
                SwitchArmBody::Block(vec![self.statement()?])
            } else {
                let e = self.expression()?;
                self.eat(&Tok::Semi)?;
                SwitchArmBody::Expr(Box::new(e))
            };
            self.switch_expr_depth -= 1;
            arms.push(SwitchArm {
                labels,
                is_default,
                body,
            });
        }
        self.eat(&Tok::RBrace)?;
        Ok(Expr::SwitchExpr {
            disc: Box::new(disc),
            arms,
            line,
        })
    }

    // ── expressions (precedence climbing) ─────────────────────────────────

    /// A full expression: a binary/precedence-climbed operand optionally
    /// followed by the ternary `? then : els`. The ternary binds looser than
    /// every binary operator and is right-associative — `a ? b : c ? d : e`
    /// parses as `a ? b : (c ? d : e)` because the `els` branch recurses through
    /// `expression`.
    fn expression(&mut self) -> Result<Expr, String> {
        // A lambda is recognised before any operator parsing, because its
        // parameter list is not an expression: `(a, b) -> …` would otherwise be
        // parsed as a parenthesized `a` followed by a stray comma.
        if let Some(lambda) = self.try_lambda()? {
            return Ok(lambda);
        }
        let cond = self.binary(0)?;
        if self.is(&Tok::Question) {
            self.advance();
            let then = self.expression()?;
            self.eat(&Tok::Colon)?;
            let els = self.expression()?;
            return Ok(Expr::Ternary {
                cond: Box::new(cond),
                then: Box::new(then),
                els: Box::new(els),
            });
        }
        Ok(cond)
    }

    fn binary(&mut self, min_bp: u8) -> Result<Expr, String> {
        let mut lhs = self.unary()?;
        loop {
            // `instanceof` is a relational operator (binding power 4) whose
            // right-hand side is a type name, not an expression.
            if matches!(self.peek(), Tok::Ident(w) if w == "instanceof") && BP_RELATIONAL >= min_bp
            {
                self.advance();
                let class = self.ident()?;
                lhs = Expr::InstanceOf {
                    expr: Box::new(lhs),
                    class,
                };
                continue;
            }
            let Some((op, bp)) = binop(self.peek()) else {
                break;
            };
            if bp < min_bp {
                break;
            }
            self.advance();
            let rhs = self.binary(bp + 1)?;
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    /// Parse a lambda if the cursor is at one, else leave the cursor untouched.
    ///
    /// Two spellings start a lambda: a bare `x ->`, and a parenthesized
    /// parameter list `(…) ->`. Only the arrow distinguishes the second from a
    /// parenthesized expression, so the decision needs a scan to the matching
    /// `)` before any token is consumed.
    fn try_lambda(&mut self) -> Result<Option<Expr>, String> {
        let line = self.line();
        // `x -> …`
        if matches!(self.peek(), Tok::Ident(_)) && self.toks[self.pos + 1].kind == Tok::Arrow {
            let name = self.ident()?;
            self.eat(&Tok::Arrow)?;
            let body = self.lambda_body()?;
            self.uses_functional = true;
            return Ok(Some(Expr::Lambda {
                params: vec![name],
                body,
                line,
            }));
        }
        if !self.is(&Tok::LParen) {
            return Ok(None);
        }
        // `(…) -> …` — scan to the matching `)` and require an arrow after it.
        let mut depth = 0usize;
        let mut j = self.pos;
        loop {
            match &self.toks[j].kind {
                Tok::LParen => depth += 1,
                Tok::RParen => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                Tok::Eof => return Ok(None),
                _ => {}
            }
            j += 1;
        }
        if self.toks[j + 1].kind != Tok::Arrow {
            return Ok(None);
        }
        self.eat(&Tok::LParen)?;
        let mut params = Vec::new();
        while !self.is(&Tok::RParen) {
            // `final` is a legal modifier on an explicitly-typed parameter.
            if matches!(self.peek(), Tok::Ident(w) if w == "final") {
                self.advance();
            }
            // An explicitly-typed parameter (`(int a, String b) -> …`) is two
            // identifiers; an inferred one (`(a, b) -> …`) is one. The declared
            // type is erased, as every other javars declared type is.
            if matches!(self.peek(), Tok::Ident(_) | Tok::Void)
                && matches!(
                    self.toks[self.pos + 1].kind,
                    Tok::Ident(_) | Tok::Lt | Tok::LBracket
                )
            {
                self.type_name()?;
            }
            params.push(self.ident()?);
            if self.is(&Tok::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        self.eat(&Tok::RParen)?;
        self.eat(&Tok::Arrow)?;
        let body = self.lambda_body()?;
        self.uses_functional = true;
        Ok(Some(Expr::Lambda { params, body, line }))
    }

    /// A lambda's body: `{ statements }` or a single expression.
    fn lambda_body(&mut self) -> Result<LambdaBody, String> {
        if self.is(&Tok::LBrace) {
            self.advance();
            return Ok(LambdaBody::Block(self.block()?));
        }
        Ok(LambdaBody::Expr(Box::new(self.expression()?)))
    }

    fn unary(&mut self) -> Result<Expr, String> {
        // A cast shares its opening `(` with a parenthesized expression, so it
        // is probed (without consuming anything on a miss) before the prefix
        // operators — `(int) -x` is a cast of a negation, not a negation.
        if let Some(cast) = self.try_cast()? {
            return Ok(cast);
        }
        match self.peek() {
            Tok::Minus => {
                self.advance();
                Ok(Expr::Unary {
                    op: UnOp::Neg,
                    rhs: Box::new(self.unary()?),
                })
            }
            // Java's unary `+` is a no-op on an already-promoted operand.
            Tok::Plus => {
                self.advance();
                self.unary()
            }
            Tok::Not => {
                self.advance();
                Ok(Expr::Unary {
                    op: UnOp::Not,
                    rhs: Box::new(self.unary()?),
                })
            }
            Tok::Tilde => {
                self.advance();
                Ok(Expr::Unary {
                    op: UnOp::BitNot,
                    rhs: Box::new(self.unary()?),
                })
            }
            // `++i` / `--i`: update first, and the expression's value is the
            // *new* one.
            Tok::PlusPlus | Tok::MinusMinus => {
                let inc = self.is(&Tok::PlusPlus);
                self.advance();
                let name = self.ident()?;
                Ok(Expr::PreIncDec { name, inc })
            }
            _ => self.postfix(),
        }
    }

    /// Parse `(Type) operand` if the cursor is at one, else leave the cursor
    /// untouched and return `None`.
    ///
    /// `(x)` opens both a cast and a parenthesized expression, and only what
    /// follows the `)` tells them apart. Java's rule, reproduced here: a
    /// *primitive* type name is always a cast (`(int) -x`), while a reference
    /// type name is a cast only when the next token can start an operand —
    /// `(a) - b` is a subtraction, `(a) b` and `(Integer) o` are casts.
    fn try_cast(&mut self) -> Result<Option<Expr>, String> {
        if !self.is(&Tok::LParen) {
            return Ok(None);
        }
        let at = |j: usize| self.toks.get(j).map(|t| &t.kind).unwrap_or(&Tok::Eof);
        let mut j = self.pos + 1;
        let Tok::Ident(head) = at(j) else {
            return Ok(None);
        };
        // A qualified name (`java.util.List`) keys on its simple name, which is
        // what every other javars type lookup uses.
        let mut name = head.clone();
        j += 1;
        while matches!(at(j), Tok::Dot) {
            let Tok::Ident(seg) = at(j + 1) else {
                return Ok(None);
            };
            name = seg.clone();
            j += 2;
        }
        // Type arguments are erased, so they only have to be stepped over.
        if matches!(at(j), Tok::Lt) {
            let mut depth = 0;
            loop {
                match at(j) {
                    Tok::Lt => depth += 1,
                    Tok::Eof => return Ok(None),
                    t if t.generic_closers() > 0 => {
                        depth -= t.generic_closers();
                        if depth <= 0 {
                            j += 1;
                            break;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
        }
        while matches!(at(j), Tok::LBracket) && matches!(at(j + 1), Tok::RBracket) {
            name.push_str("[]");
            j += 2;
        }
        if !matches!(at(j), Tok::RParen) {
            return Ok(None);
        }
        let primitive = matches!(
            name.as_str(),
            "int" | "long" | "short" | "byte" | "char" | "float" | "double" | "boolean"
        );
        // `instanceof` reads as an identifier but continues the *enclosing*
        // expression, so it must not be mistaken for a cast operand.
        let starts_operand = match at(j + 1) {
            Tok::Ident(w) => w != "instanceof",
            Tok::Str(_)
            | Tok::Char(_)
            | Tok::Int(_)
            | Tok::Long(_)
            | Tok::Float(_)
            | Tok::Float32(_)
            | Tok::True
            | Tok::False
            | Tok::LParen
            | Tok::Not
            | Tok::Tilde
            | Tok::New => true,
            _ => false,
        };
        if !primitive && !starts_operand {
            return Ok(None);
        }
        let line = self.toks[j].line;
        self.pos = j + 1;
        let expr = self.unary()?;
        Ok(Some(Expr::Cast {
            ty: name,
            expr: Box::new(expr),
            line,
        }))
    }

    /// Parse a primary followed by any postfix chain: `.method(args)` instance
    /// method calls, `.field` field access (an array's `.length` or an instance
    /// field), and `[index]` array indexing. Chains compose left-to-right
    /// (`grid[i][j]`, `s.substring(1).length()`, `p.next.value`).
    fn postfix(&mut self) -> Result<Expr, String> {
        let mut e = self.primary()?;
        loop {
            if self.is(&Tok::Dot) {
                let line = self.line();
                self.advance();
                let member = self.ident()?;
                if self.is(&Tok::LParen) {
                    let args = self.call_args()?;
                    e = Expr::MethodCall {
                        recv: Box::new(e),
                        method: member,
                        args,
                        line,
                    };
                } else {
                    e = Expr::Field {
                        recv: Box::new(e),
                        name: member,
                    };
                }
            } else if self.is(&Tok::ColonColon) {
                let line = self.line();
                self.advance();
                // `Type::new` is a constructor reference; everything else names
                // a method.
                let method = if self.is(&Tok::New) {
                    self.advance();
                    "new".to_string()
                } else {
                    self.ident()?
                };
                self.uses_functional = true;
                e = Expr::MethodRef {
                    recv: Box::new(e),
                    method,
                    line,
                };
            } else if self.is(&Tok::LBracket) {
                self.advance();
                let index = self.expression()?;
                self.eat(&Tok::RBracket)?;
                e = Expr::Index {
                    array: Box::new(e),
                    index: Box::new(index),
                };
            } else {
                break;
            }
        }
        Ok(e)
    }

    fn primary(&mut self) -> Result<Expr, String> {
        match self.peek().clone() {
            Tok::Int(n) => {
                self.advance();
                Ok(Expr::Int(n))
            }
            Tok::Long(n) => {
                self.advance();
                Ok(Expr::Long(n))
            }
            Tok::Float(f) => {
                self.advance();
                Ok(Expr::Float(f))
            }
            Tok::Float32(f) => {
                self.advance();
                Ok(Expr::Float32(f))
            }
            Tok::Str(s) => {
                self.advance();
                Ok(Expr::Str(s))
            }
            Tok::Char(c) => {
                self.advance();
                Ok(Expr::Char(c))
            }
            Tok::True => {
                self.advance();
                Ok(Expr::Bool(true))
            }
            Tok::False => {
                self.advance();
                Ok(Expr::Bool(false))
            }
            Tok::LParen => {
                self.advance();
                let e = self.expression()?;
                self.eat(&Tok::RParen)?;
                Ok(e)
            }
            Tok::New => self.new_expr(),
            // A `switch` *expression* — only the arrow form has one.
            Tok::Switch => self.switch_expr(),
            Tok::Ident(name) if name == "this" => {
                let line = self.line();
                self.advance();
                // `this(args)` is an explicit constructor invocation (JLS 8.8.7.1)
                // — a call, not a read of `this`. It is spelled as a `Call` named
                // `this`, the same shape `super(args)` already uses.
                if self.is(&Tok::LParen) {
                    let args = self.call_args()?;
                    return Ok(Expr::Call {
                        name: "this".to_string(),
                        args,
                        line,
                    });
                }
                Ok(Expr::This)
            }
            Tok::Ident(name) => {
                // `System.out.println(...)` / `.print(...)`, or a var read,
                // a bare-identifier call `name(args)`, or unsupported field
                // access.
                // `java.lang.System.out.println(...)` names the same stream its
                // simple form does. The package prefix is stepped over here
                // rather than in [`Parser::simple_type_name`] because
                // `System.out` is parsed as a dedicated node, not as a type.
                if name == "java"
                    && matches!(&self.toks[self.pos + 2].kind, Tok::Ident(w) if w == "lang")
                    && matches!(&self.toks[self.pos + 4].kind, Tok::Ident(w) if w == "System")
                {
                    self.advance(); // java
                    self.eat(&Tok::Dot)?;
                    self.advance(); // lang
                    self.eat(&Tok::Dot)?;
                    return self.system_out();
                }
                if name == "System" {
                    // `System.out::println` is a method *reference*, not a call:
                    // hand the stream back as a field access and let the postfix
                    // layer take the `::`.
                    if self.toks[self.pos + 3].kind == Tok::ColonColon {
                        self.advance(); // System
                        self.eat(&Tok::Dot)?;
                        let stream = self.ident()?;
                        return Ok(Expr::Field {
                            recv: Box::new(Expr::Var("System".to_string())),
                            name: stream,
                        });
                    }
                    return self.system_out();
                }
                let line = self.line();
                self.advance();
                // `i++` / `i--` in value position: the expression's value is the
                // variable's *old* value, with the update applied after (the
                // compiler's `Expr::PostIncDec` arm emits exactly that order).
                if matches!(self.peek(), Tok::PlusPlus | Tok::MinusMinus) {
                    let inc = self.is(&Tok::PlusPlus);
                    self.advance();
                    return Ok(Expr::PostIncDec { name, inc });
                }
                // `name(args...)` — a call. The compiler resolves it: the FFI
                // desugar target `__rust_compile(...)` and `rust { ... }`-exported
                // barewords lower to `fusevm::ffi`; any other name with no FFI
                // block present is an unresolved reference.
                if self.is(&Tok::LParen) {
                    let args = self.call_args()?;
                    return Ok(Expr::Call { name, args, line });
                }
                // A trailing `.` (method/field access) is consumed by the
                // postfix layer above; a bare identifier is a variable read.
                Ok(Expr::Var(name))
            }
            other => Err(format!(
                "javars: unexpected token {other} in expression on line {}",
                self.line()
            )),
        }
    }

    /// Parse a `new` expression, the cursor sitting just past `new`:
    /// `new Type[size]` (default-valued array), `new Type[]{elems}` /
    /// `new Type[]{}` (array literal), or `new Class(args)` (object).
    fn new_expr(&mut self) -> Result<Expr, String> {
        let line = self.line();
        self.eat(&Tok::New)?;
        let ty = self.simple_type_name()?;
        // Diamond / explicit type arguments (`new Box<>()`, `new Box<Integer>()`)
        // — erased.
        self.skip_generics();
        if self.is(&Tok::LBracket) {
            self.advance();
            if self.is(&Tok::RBracket) {
                // `new T[]{…}` / `new T[][]{{…},{…}}` — an array literal with an
                // explicit element type. Each extra `[]` pair deepens the
                // element type by one dimension (`new int[][]` has `int[]`
                // elements), which is what a `var` binding needs to infer from.
                self.advance();
                let mut elem_ty = ty;
                while self.is(&Tok::LBracket) {
                    self.advance();
                    self.eat(&Tok::RBracket)?;
                    elem_ty.push_str("[]");
                }
                self.eat(&Tok::LBrace)?;
                let elems = self.array_lit_elems()?;
                return Ok(Expr::ArrayLit {
                    elems,
                    elem_ty: Some(elem_ty),
                });
            }
            // `new T[s0][s1]…[sK][]…` — collect the sized dimensions, then any
            // trailing unsized `[]` (whose elements default to null).
            let mut sizes = vec![self.expression()?];
            self.eat(&Tok::RBracket)?;
            let mut extra_dims = 0;
            while self.is(&Tok::LBracket) {
                self.advance();
                if self.is(&Tok::RBracket) {
                    self.advance();
                    extra_dims += 1;
                } else {
                    if extra_dims > 0 {
                        return Err(format!(
                            "javars: a sized array dimension cannot follow an empty one (line {line})"
                        ));
                    }
                    sizes.push(self.expression()?);
                    self.eat(&Tok::RBracket)?;
                }
            }
            return Ok(Expr::NewArray {
                elem_ty: ty,
                sizes,
                extra_dims,
            });
        }
        // `new Class(args)` — object construction. Constructing a modeled
        // `java.lang` throwable pulls in the exception prelude even when the
        // program never throws it (`Exception e = new Exception("x");`).
        if crate::prelude::is_throwable(&ty) {
            self.uses_exceptions = true;
        }
        let args = self.call_args()?;
        Ok(Expr::NewObject {
            class: ty,
            args,
            line,
        })
    }

    /// Parse the elements of an array literal `{e, e, ...}` (a trailing comma is
    /// allowed), the cursor sitting just past the opening `{`; consumes the `}`.
    fn array_lit_elems(&mut self) -> Result<Vec<Expr>, String> {
        let mut elems = Vec::new();
        while !self.is(&Tok::RBrace) && !self.is(&Tok::Eof) {
            // A nested `{…}` element is a sub-array literal (`{{1,2},{3,4}}`).
            if self.is(&Tok::LBrace) {
                self.advance();
                let inner = self.array_lit_elems()?;
                elems.push(Expr::ArrayLit {
                    elems: inner,
                    elem_ty: None,
                });
            } else {
                elems.push(self.expression()?);
            }
            if self.is(&Tok::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        self.eat(&Tok::RBrace)?;
        Ok(elems)
    }

    /// Parse a parenthesized, comma-separated argument list `( e, e, ... )`,
    /// the cursor sitting on the opening `(`.
    fn call_args(&mut self) -> Result<Vec<Expr>, String> {
        self.eat(&Tok::LParen)?;
        let mut args = Vec::new();
        if !self.is(&Tok::RParen) {
            loop {
                args.push(self.expression()?);
                if self.is(&Tok::Comma) {
                    self.advance();
                } else {
                    break;
                }
            }
        }
        self.eat(&Tok::RParen)?;
        Ok(args)
    }

    /// Parse `System.out.println(arg)` / `System.out.print(arg)` and the
    /// `System.err` variants (which print to stderr).
    fn system_out(&mut self) -> Result<Expr, String> {
        self.ident()?; // System
        self.eat(&Tok::Dot)?;
        let stream = self.ident()?;
        let err = match stream.as_str() {
            "out" => false,
            "err" => true,
            _ => {
                return Err(format!(
                    "javars: only `System.out`/`System.err` are supported, not `System.{stream}` (line {})",
                    self.line()
                ))
            }
        };
        self.eat(&Tok::Dot)?;
        let line = self.line();
        let method = self.ident()?;
        // `printf(fmt, args…)` is `print(String.format(fmt, args…))` — the same
        // formatter, no trailing newline. Desugaring here keeps one formatting
        // implementation instead of two.
        if method == "printf" || method == "format" {
            let args = self.call_args()?;
            return Ok(Expr::Println {
                newline: false,
                err,
                arg: Some(Box::new(Expr::MethodCall {
                    recv: Box::new(Expr::Var("String".to_string())),
                    method: "format".to_string(),
                    args,
                    line,
                })),
            });
        }
        let newline = match method.as_str() {
            "println" => true,
            "print" => false,
            _ => {
                return Err(format!(
                "javars: only `System.{stream}.println`/`print`/`printf` are supported, not `{method}` (line {})",
                self.line()
            ))
            }
        };
        self.eat(&Tok::LParen)?;
        let arg = if self.is(&Tok::RParen) {
            None
        } else {
            Some(Box::new(self.expression()?))
        };
        self.eat(&Tok::RParen)?;
        Ok(Expr::Println { newline, err, arg })
    }

    fn ident(&mut self) -> Result<String, String> {
        match self.advance() {
            Tok::Ident(s) => Ok(s),
            // type keywords double as idents in declaration position
            Tok::Void => Ok("void".into()),
            other => Err(format!(
                "javars: expected an identifier but found {other} on line {}",
                self.line()
            )),
        }
    }
}

/// Map a token to a compound-assignment operator, if it is one.
fn assign_op(t: &Tok) -> Option<AssignOp> {
    Some(match t {
        Tok::Assign => AssignOp::Assign,
        Tok::PlusAssign => AssignOp::Add,
        Tok::MinusAssign => AssignOp::Sub,
        Tok::StarAssign => AssignOp::Mul,
        Tok::SlashAssign => AssignOp::Div,
        Tok::PercentAssign => AssignOp::Mod,
        Tok::AmpAssign => AssignOp::BitAnd,
        Tok::PipeAssign => AssignOp::BitOr,
        Tok::CaretAssign => AssignOp::BitXor,
        Tok::ShlAssign => AssignOp::Shl,
        Tok::ShrAssign => AssignOp::Shr,
        Tok::UshrAssign => AssignOp::Ushr,
        _ => return None,
    })
}

/// Binding power of `instanceof`, which [`Parser::binary`] handles inline
/// because its right-hand side is a type name rather than an expression. Java
/// puts it at the relational level, alongside `<` and `>`.
const BP_RELATIONAL: u8 = 7;

/// Binary operator + its binding power (higher binds tighter).
///
/// The ladder is Java's, loosest first: `||`, `&&`, `|`, `^`, `&`, equality,
/// relational (+ `instanceof`), shift, additive, multiplicative.
fn binop(t: &Tok) -> Option<(BinOp, u8)> {
    Some(match t {
        Tok::OrOr => (BinOp::Or, 1),
        Tok::AndAnd => (BinOp::And, 2),
        Tok::Pipe => (BinOp::BitOr, 3),
        Tok::Caret => (BinOp::BitXor, 4),
        Tok::Amp => (BinOp::BitAnd, 5),
        Tok::EqEq => (BinOp::Eq, 6),
        Tok::NotEq => (BinOp::Ne, 6),
        Tok::Lt => (BinOp::Lt, BP_RELATIONAL),
        Tok::Gt => (BinOp::Gt, BP_RELATIONAL),
        Tok::Le => (BinOp::Le, BP_RELATIONAL),
        Tok::Ge => (BinOp::Ge, BP_RELATIONAL),
        Tok::Shl => (BinOp::Shl, 8),
        Tok::Shr => (BinOp::Shr, 8),
        Tok::Ushr => (BinOp::Ushr, 8),
        Tok::Plus => (BinOp::Add, 9),
        Tok::Minus => (BinOp::Sub, 9),
        Tok::Star => (BinOp::Mul, 10),
        Tok::Slash => (BinOp::Div, 10),
        Tok::Percent => (BinOp::Mod, 10),
        _ => return None,
    })
}
