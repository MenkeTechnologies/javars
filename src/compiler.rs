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

/// One enclosing loop's backpatch targets.
struct Loop {
    /// `continue` jumps here (the loop's step/condition entry).
    continue_target: usize,
    /// `break` jump op indices, patched to the loop exit once known.
    break_ops: Vec<usize>,
}

struct Compiler {
    b: ChunkBuilder,
    loops: Vec<Loop>,
    /// A top-level `break`/`return;` (no enclosing loop) jumps to program end.
    exit_ops: Vec<usize>,
}

/// Compile a parsed [`Program`]'s `main` body to a runnable fusevm chunk.
pub fn compile(prog: &Program) -> Result<Chunk, String> {
    let mut c = Compiler {
        b: ChunkBuilder::new(),
        loops: Vec::new(),
        exit_ops: Vec::new(),
    };
    for stmt in &prog.main {
        c.stmt(stmt)?;
    }
    // Patch any program-level `break`/`return;` to the final position.
    let end = c.b.current_pos();
    let exit_ops = std::mem::take(&mut c.exit_ops);
    for op in exit_ops {
        c.b.patch_jump(op, end);
    }
    Ok(c.b.build())
}

impl Compiler {
    fn stmt(&mut self, s: &Stmt) -> Result<(), String> {
        match s {
            Stmt::Local { name, init, .. } => {
                if let Some(e) = init {
                    self.expr(e)?;
                    let idx = self.b.add_name(name);
                    self.b.emit(Op::SetVar(idx), 0);
                }
                // An uninitialized local is simply unbound until first assigned
                // (Java's definite-assignment check is not enforced in slice 1).
                Ok(())
            }
            Stmt::Assign { name, op, value } => {
                let idx = self.b.add_name(name);
                match op {
                    AssignOp::Assign => {
                        self.expr(value)?;
                    }
                    _ => {
                        // `x <op>= e` → x = x <op> e
                        self.b.emit(Op::GetVar(idx), 0);
                        self.expr(value)?;
                        self.b.emit(compound_op(*op), 0);
                    }
                }
                self.b.emit(Op::SetVar(idx), 0);
                Ok(())
            }
            Stmt::Expr(Expr::Println { newline, arg }) => {
                // The print builtin returns `null`; discard it in statement
                // position.
                self.println(*newline, arg.as_deref())?;
                self.b.emit(Op::Pop, 0);
                Ok(())
            }
            Stmt::Expr(Expr::PostIncDec { name, inc }) => {
                self.post_inc_dec(name, *inc);
                Ok(())
            }
            Stmt::Expr(e) => {
                self.expr(e)?;
                self.b.emit(Op::Pop, 0);
                Ok(())
            }
            Stmt::If { cond, then, els } => self.if_stmt(cond, then, els),
            Stmt::While { cond, body } => self.while_stmt(cond, body),
            Stmt::For {
                init,
                cond,
                update,
                body,
            } => self.for_stmt(init, cond, update, body),
            Stmt::Break => {
                let op = self.b.emit(Op::Jump(0), 0);
                match self.loops.last_mut() {
                    Some(l) => l.break_ops.push(op),
                    None => self.exit_ops.push(op),
                }
                Ok(())
            }
            Stmt::Continue => {
                let target = self
                    .loops
                    .last()
                    .map(|l| l.continue_target)
                    .ok_or_else(|| "javars: `continue` outside a loop".to_string())?;
                self.b.emit(Op::Jump(target), 0);
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
            continue_target: top,
            break_ops: Vec::new(),
        });
        for s in body {
            self.stmt(s)?;
        }
        self.b.emit(Op::Jump(top), 0);
        let end = self.b.current_pos();
        self.b.patch_jump(jf, end);
        let l = self.loops.pop().unwrap();
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
        // `continue` runs the update clause, then re-tests — target it at a
        // dedicated step label emitted after the body.
        self.loops.push(Loop {
            continue_target: 0,
            break_ops: Vec::new(),
        });
        for s in body {
            self.stmt(s)?;
        }
        // step label: patch this loop's continue target to here.
        let step = self.b.current_pos();
        if let Some(l) = self.loops.last_mut() {
            l.continue_target = step;
        }
        if let Some(update) = update {
            self.stmt(update)?;
        }
        self.b.emit(Op::Jump(top), 0);
        let end = self.b.current_pos();
        if let Some(jf) = jf {
            self.b.patch_jump(jf, end);
        }
        let l = self.loops.pop().unwrap();
        // A `continue` emitted before the step label was patched still points at
        // `step` because we set `continue_target` in place above.
        for op in l.break_ops {
            self.b.patch_jump(op, end);
        }
        Ok(())
    }

    fn post_inc_dec(&mut self, name: &str, inc: bool) {
        let idx = self.b.add_name(name);
        self.b.emit(Op::GetVar(idx), 0);
        self.b.emit(Op::LoadInt(1), 0);
        self.b.emit(if inc { Op::Add } else { Op::Sub }, 0);
        self.b.emit(Op::SetVar(idx), 0);
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
                let idx = self.b.add_name(name);
                self.b.emit(Op::GetVar(idx), 0);
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
                let idx = self.b.add_name(name);
                self.b.emit(Op::GetVar(idx), 0);
                self.post_inc_dec(name, *inc);
            }
        }
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
        self.expr(lhs)?;
        self.expr(rhs)?;
        let vop = match op {
            BinOp::Add => Op::Add,
            BinOp::Sub => Op::Sub,
            BinOp::Mul => Op::Mul,
            BinOp::Div => Op::Div,
            BinOp::Mod => Op::Mod,
            BinOp::Eq => Op::NumEq,
            BinOp::Ne => Op::NumNe,
            BinOp::Lt => Op::NumLt,
            BinOp::Gt => Op::NumGt,
            BinOp::Le => Op::NumLe,
            BinOp::Ge => Op::NumGe,
            BinOp::And | BinOp::Or => unreachable!("handled above"),
        };
        self.b.emit(vop, 0);
        Ok(())
    }
}

fn compound_op(op: AssignOp) -> Op {
    match op {
        AssignOp::Add => Op::Add,
        AssignOp::Sub => Op::Sub,
        AssignOp::Mul => Op::Mul,
        AssignOp::Div => Op::Div,
        AssignOp::Mod => Op::Mod,
        AssignOp::Assign => unreachable!("plain assign never lowers through compound_op"),
    }
}
