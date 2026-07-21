//! A recursive-descent parser with precedence-climbing for expressions.
//!
//! Grammar (slice 1): a compilation unit is `public class Name { ... }`; inside
//! it, javars locates `public static void main(String[] args) { <body> }` and
//! parses `<body>` into `ast::Stmt`s. Other members are skipped by brace
//! matching so a class that also declares helper methods still parses its
//! `main`. Statements cover local decls, assignments, `if`/`while`/`for`,
//! `break`/`continue`, `System.out.print[ln]`, and post-inc/dec.

use crate::ast::*;
use crate::lexer::{Tok, Token};

/// Parse Java `src` into a [`Program`].
pub fn parse(src: &str) -> Result<Program, String> {
    let tokens = crate::lexer::lex(src)?;
    let mut p = Parser {
        toks: tokens,
        pos: 0,
    };
    p.program()
}

struct Parser {
    toks: Vec<Token>,
    pos: usize,
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
        let mut entry: Option<(String, Vec<Stmt>)> = None;
        let mut methods = Vec::new();
        let mut classes = Vec::new();
        // Top-level type declarations, in sequence.
        while !self.is(&Tok::Eof) {
            self.parse_class(&mut entry, &mut methods, &mut classes)?;
        }
        match entry {
            Some((class_name, main)) => Ok(Program {
                class_name,
                main,
                methods,
                classes,
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
        entry: &mut Option<(String, Vec<Stmt>)>,
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
        // `class Name` or `interface Name`. `interface` is an ordinary ident
        // (not a reserved token), matched here.
        let is_interface = matches!(self.peek(), Tok::Ident(w) if w == "interface");
        if is_interface {
            self.advance();
        } else {
            self.eat(&Tok::Class)?;
        }
        let name = self.ident()?;
        // Optional generic type-parameter declaration `<T>`, `<T extends X>`,
        // `<K, V>` — erased (parsed and discarded).
        self.skip_generics();
        // `extends`: a class has a single superclass; an interface may extend
        // several interfaces (collected as `interfaces`).
        let mut superclass = None;
        let mut interfaces = Vec::new();
        if matches!(self.peek(), Tok::Ident(w) if w == "extends") {
            self.advance();
            loop {
                let sup = self.type_name()?;
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
        let mut ctors = Vec::new();
        let mut inst_methods = Vec::new();
        while !self.is(&Tok::RBrace) && !self.is(&Tok::Eof) {
            if let Some(body) = self.try_main()? {
                *entry = Some((name.clone(), body));
            } else if self.at_nested_class() {
                self.parse_class(entry, methods, classes)?;
            } else if let Some(c) = self.try_ctor(&name)? {
                ctors.push(c);
            } else if let Some((m, is_static)) = self.try_any_method()? {
                if is_static {
                    methods.push(m);
                } else {
                    inst_methods.push(m);
                }
            } else if let Some(fs) = self.try_fields()? {
                fields.extend(fs);
            } else {
                self.skip_member()?;
            }
        }
        self.eat(&Tok::RBrace)?;
        classes.push(Class {
            name,
            superclass,
            interfaces,
            is_interface,
            fields,
            ctors,
            methods: inst_methods,
            line,
        });
        Ok(())
    }

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
                Tok::Gt => {
                    depth -= 1;
                    if depth == 0 {
                        self.advance();
                        return;
                    }
                }
                // `>=`/`>>`-style tokens never appear inside a type-arg list from
                // this lexer (it emits single `>`); stop defensively at EOF.
                Tok::Eof => return,
                _ => {}
            }
            self.advance();
        }
    }

    /// True when the member at the cursor is a (possibly modifier-prefixed)
    /// nested `class` declaration.
    fn at_nested_class(&self) -> bool {
        let mut j = self.pos;
        while matches!(self.toks[j].kind, Tok::Public | Tok::Static)
            || matches!(&self.toks[j].kind, Tok::Ident(w) if w == "final" || w == "abstract")
        {
            j += 1;
        }
        matches!(self.toks[j].kind, Tok::Class)
            || matches!(&self.toks[j].kind, Tok::Ident(w) if w == "interface")
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
        self.eat(&Tok::LBrace)?;
        let body = self.block()?;
        Ok(Some(Ctor { params, body, line }))
    }

    /// Parse one or more instance-field declarations sharing a type
    /// (`int x, y = 3;`). Restores and returns `None` if the member is not a
    /// field (e.g. a method the earlier probes already rejected — a `type name (`
    /// shape). `static` fields are accepted but treated as instance fields
    /// (javars has no per-class statics yet).
    fn try_fields(&mut self) -> Result<Option<Vec<FieldDecl>>, String> {
        let save = self.pos;
        while matches!(self.peek(), Tok::Public | Tok::Static)
            || matches!(self.peek(), Tok::Ident(w) if w == "final" || w == "private" || w == "protected" || w == "volatile" || w == "transient")
        {
            self.advance();
        }
        if !self.at_type() {
            self.pos = save;
            return Ok(None);
        }
        let ty = self.type_name()?;
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
            let name = self.ident()?;
            let init = if self.is(&Tok::Assign) {
                self.advance();
                Some(self.var_init()?)
            } else {
                None
            };
            out.push(FieldDecl {
                ty: ty.clone(),
                name,
                init,
            });
            if self.is(&Tok::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        self.eat(&Tok::Semi)?;
        Ok(Some(out))
    }

    /// If the cursor is at a method (`[modifiers] <ret> name(<params>) { ... }`),
    /// parse it and report whether it was `static`. Otherwise restore the cursor
    /// and return `None` (fields and constructors are handled by their own
    /// probes). `main` is matched earlier by [`Parser::try_main`], so it never
    /// reaches here.
    fn try_any_method(&mut self) -> Result<Option<(Method, bool)>, String> {
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
                params,
                ret,
                body,
                is_abstract,
                line,
            },
            saw_static,
        )))
    }

    /// Parse a comma-separated formal parameter list `<type> <name>, ...`, the
    /// cursor sitting just past the opening `(`. Stops at the closing `)`.
    fn params(&mut self) -> Result<Vec<Param>, String> {
        let mut out = Vec::new();
        if self.is(&Tok::RParen) {
            return Ok(out);
        }
        loop {
            let ty = self.type_name()?;
            let name = self.ident()?;
            out.push(Param { ty, name });
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

    /// Parse a declaration-position type: `void`, `int`, `String`, `int[]`, ….
    /// Trailing `[]` pairs are folded into the returned name (e.g. `int[]`).
    fn type_name(&mut self) -> Result<String, String> {
        let mut ty = self.ident()?;
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
    /// body and return it. Otherwise leave the cursor untouched and return None.
    fn try_main(&mut self) -> Result<Option<Vec<Stmt>>, String> {
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
        // skip the parameter list — slice 1 ignores argv
        let mut depth = 1;
        while depth > 0 && !self.is(&Tok::Eof) {
            match self.advance() {
                Tok::LParen => depth += 1,
                Tok::RParen => depth -= 1,
                _ => {}
            }
        }
        self.eat(&Tok::LBrace)?;
        let body = self.block()?;
        Ok(Some(body))
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
            let ty = self.type_name()?;
            let name = self.ident()?;
            let init = if self.is(&Tok::Assign) {
                self.advance();
                Some(self.var_init()?)
            } else {
                None
            };
            if expect_semi {
                self.eat(&Tok::Semi)?;
            }
            return Ok(StmtKind::Local { ty, name, init });
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
            Ok(Expr::ArrayLit { elems })
        } else {
            self.expression()
        }
    }

    /// Heuristic: two identifiers in a row (`Type name`) with the type not being
    /// a value keyword — a local declaration. Array types (`int[] a`) are
    /// handled by skipping the `[]` before the name.
    fn looks_like_decl(&self) -> bool {
        let t0 = &self.toks[self.pos].kind;
        let is_type = matches!(t0, Tok::Ident(_));
        if !is_type {
            return false;
        }
        let mut j = self.pos + 1;
        // optional generic type arguments on the type (`List<String> xs`)
        if matches!(self.toks[j].kind, Tok::Lt) {
            let mut depth = 0;
            while j < self.toks.len() {
                match self.toks[j].kind {
                    Tok::Lt => depth += 1,
                    Tok::Gt => {
                        depth -= 1;
                        if depth == 0 {
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

    fn for_stmt(&mut self) -> Result<StmtKind, String> {
        self.eat(&Tok::For)?;
        self.eat(&Tok::LParen)?;
        let init = if self.is(&Tok::Semi) {
            None
        } else {
            let line = self.line();
            Some(Box::new(Stmt::new(line, self.simple_statement(false)?)))
        };
        self.eat(&Tok::Semi)?;
        let cond = if self.is(&Tok::Semi) {
            None
        } else {
            Some(self.expression()?)
        };
        self.eat(&Tok::Semi)?;
        let update = if self.is(&Tok::RParen) {
            None
        } else {
            let line = self.line();
            Some(Box::new(Stmt::new(line, self.simple_statement(false)?)))
        };
        self.eat(&Tok::RParen)?;
        let body = self.braced_or_single()?;
        Ok(StmtKind::For {
            init,
            cond,
            update,
            body,
        })
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

    // ── expressions (precedence climbing) ─────────────────────────────────

    /// A full expression: a binary/precedence-climbed operand optionally
    /// followed by the ternary `? then : els`. The ternary binds looser than
    /// every binary operator and is right-associative — `a ? b : c ? d : e`
    /// parses as `a ? b : (c ? d : e)` because the `els` branch recurses through
    /// `expression`.
    fn expression(&mut self) -> Result<Expr, String> {
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
            if matches!(self.peek(), Tok::Ident(w) if w == "instanceof") && 4 >= min_bp {
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

    fn unary(&mut self) -> Result<Expr, String> {
        match self.peek() {
            Tok::Minus => {
                self.advance();
                Ok(Expr::Unary {
                    op: UnOp::Neg,
                    rhs: Box::new(self.unary()?),
                })
            }
            Tok::Not => {
                self.advance();
                Ok(Expr::Unary {
                    op: UnOp::Not,
                    rhs: Box::new(self.unary()?),
                })
            }
            _ => self.postfix(),
        }
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
            Tok::Float(f) => {
                self.advance();
                Ok(Expr::Float(f))
            }
            Tok::Str(s) => {
                self.advance();
                Ok(Expr::Str(s))
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
            Tok::Ident(name) if name == "this" => {
                self.advance();
                Ok(Expr::This)
            }
            Tok::Ident(name) => {
                // `System.out.println(...)` / `.print(...)`, or a var read,
                // a bare-identifier call `name(args)`, or unsupported field
                // access.
                if name == "System" {
                    return self.system_out();
                }
                let line = self.line();
                self.advance();
                // post-inc/dec as an expression value is not modeled; only as a
                // statement (handled in simple_statement). A trailing ++/-- here
                // is treated as the variable's value with the mutation deferred
                // — reject to avoid silently wrong semantics.
                if matches!(self.peek(), Tok::PlusPlus | Tok::MinusMinus) {
                    return Err(format!(
                        "javars: `{name}++`/`--` is only supported as a statement yet (line {})",
                        self.line()
                    ));
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
        let ty = self.ident()?;
        // Diamond / explicit type arguments (`new Box<>()`, `new Box<Integer>()`)
        // — erased.
        self.skip_generics();
        if self.is(&Tok::LBracket) {
            self.advance();
            if self.is(&Tok::RBracket) {
                // `new T[]{...}` — an array literal with an explicit element type.
                self.advance();
                self.eat(&Tok::LBrace)?;
                let elems = self.array_lit_elems()?;
                return Ok(Expr::ArrayLit { elems });
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
        // `new Class(args)` — object construction.
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
                elems.push(Expr::ArrayLit { elems: inner });
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
        let method = self.ident()?;
        let newline = match method.as_str() {
            "println" => true,
            "print" => false,
            _ => {
                return Err(format!(
                "javars: only `System.{stream}.println`/`print` are supported, not `{method}` (line {})",
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
        _ => return None,
    })
}

/// Binary operator + its binding power (higher binds tighter).
fn binop(t: &Tok) -> Option<(BinOp, u8)> {
    Some(match t {
        Tok::OrOr => (BinOp::Or, 1),
        Tok::AndAnd => (BinOp::And, 2),
        Tok::EqEq => (BinOp::Eq, 3),
        Tok::NotEq => (BinOp::Ne, 3),
        Tok::Lt => (BinOp::Lt, 4),
        Tok::Gt => (BinOp::Gt, 4),
        Tok::Le => (BinOp::Le, 4),
        Tok::Ge => (BinOp::Ge, 4),
        Tok::Plus => (BinOp::Add, 5),
        Tok::Minus => (BinOp::Sub, 5),
        Tok::Star => (BinOp::Mul, 6),
        Tok::Slash => (BinOp::Div, 6),
        Tok::Percent => (BinOp::Mod, 6),
        _ => return None,
    })
}
