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

    /// `[modifiers] class Name { members... }` — find the entry class and its
    /// `main`. Leading `import`/`package` lines are tolerated (skipped to `;`).
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
        // class modifiers
        while matches!(self.peek(), Tok::Public | Tok::Static) {
            self.advance();
        }
        // `final`/`abstract` come through as idents; skip until `class`.
        while !self.is(&Tok::Class) && !self.is(&Tok::Eof) {
            self.advance();
        }
        self.eat(&Tok::Class)?;
        let class_name = self.ident()?;
        self.eat(&Tok::LBrace)?;

        let mut main = None;
        while !self.is(&Tok::RBrace) && !self.is(&Tok::Eof) {
            if let Some(body) = self.try_main()? {
                main = Some(body);
            } else {
                self.skip_member()?;
            }
        }

        match main {
            Some(main) => Ok(Program { class_name, main }),
            None => Err(format!(
                "javars: class `{class_name}` has no `public static void main(String[] args)`"
            )),
        }
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
        match self.peek() {
            Tok::If => self.if_stmt(),
            Tok::While => self.while_stmt(),
            Tok::For => self.for_stmt(),
            Tok::Return => {
                // slice 1: `main` is `void`; a bare `return;` ends it, and a
                // value return is rejected rather than silently dropped.
                self.advance();
                if self.is(&Tok::Semi) {
                    self.advance();
                    // model as a no-op break out of nothing — end of main only
                    Ok(StmtKind::Break)
                } else {
                    Err(format!(
                        "javars: `return <value>` from void main is not supported yet (line {})",
                        self.line()
                    ))
                }
            }
            Tok::Break => {
                self.advance();
                self.eat(&Tok::Semi)?;
                Ok(StmtKind::Break)
            }
            Tok::Continue => {
                self.advance();
                self.eat(&Tok::Semi)?;
                Ok(StmtKind::Continue)
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
        // identifier: `int x`, `String s`, `var v`, `long n`, `double d`.
        if self.looks_like_decl() {
            let ty = self.ident()?;
            let name = self.ident()?;
            let init = if self.is(&Tok::Assign) {
                self.advance();
                Some(self.expression()?)
            } else {
                None
            };
            if expect_semi {
                self.eat(&Tok::Semi)?;
            }
            return Ok(StmtKind::Local { ty, name, init });
        }

        // Assignment or expression statement.
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
            // post-inc/dec statement: `i++;`
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

        // Fallback: an expression statement (e.g. System.out.println(...)).
        let e = self.expression()?;
        if expect_semi {
            self.eat(&Tok::Semi)?;
        }
        Ok(StmtKind::Expr(e))
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

    // ── expressions (precedence climbing) ─────────────────────────────────

    fn expression(&mut self) -> Result<Expr, String> {
        self.binary(0)
    }

    fn binary(&mut self, min_bp: u8) -> Result<Expr, String> {
        let mut lhs = self.unary()?;
        while let Some((op, bp)) = binop(self.peek()) {
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
            _ => self.primary(),
        }
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
                if self.is(&Tok::Dot) {
                    return Err(format!(
                        "javars: method/field access on `{name}` is not supported yet (line {})",
                        self.line()
                    ));
                }
                Ok(Expr::Var(name))
            }
            other => Err(format!(
                "javars: unexpected token {other} in expression on line {}",
                self.line()
            )),
        }
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

    /// Parse `System.out.println(arg)` / `System.out.print(arg)`.
    fn system_out(&mut self) -> Result<Expr, String> {
        self.ident()?; // System
        self.eat(&Tok::Dot)?;
        let out = self.ident()?;
        if out != "out" {
            return Err(format!(
                "javars: only `System.out` is supported, not `System.{out}` (line {})",
                self.line()
            ));
        }
        self.eat(&Tok::Dot)?;
        let method = self.ident()?;
        let newline = match method.as_str() {
            "println" => true,
            "print" => false,
            _ => {
                return Err(format!(
                "javars: only `System.out.println`/`print` are supported, not `{method}` (line {})",
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
        Ok(Expr::Println { newline, arg })
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
