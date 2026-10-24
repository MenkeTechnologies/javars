//! Definite assignment (JLS chapter 16), checked the way `javac` checks it.
//!
//! One walk per class, ported from `com.sun.tools.javac.comp.Flow`'s
//! `AssignAnalyzer` (JDK 21): every local and every blank `final` field gets
//! an *address*, and two bit sets follow control flow — `inits`, the
//! variables definitely assigned (DA) at this point, and `uninits`, the ones
//! definitely unassigned (DU). Reading a variable outside `inits` is
//! "variable x might not have been initialized"; assigning a `final` outside
//! `uninits` is "variable x might already have been assigned" (or "… might
//! be assigned in loop" on the second, speculative pass over a loop body).
//! A condition splits the state into a when-true and a when-false pair, and a
//! constant `true`/`false` condition makes the impossible branch vacuously
//! assign everything — which is how `while (true) { x = 1; break; }` leaves
//! `x` assigned.
//!
//! The same walk carries the three checks that hang off the same facts:
//!
//! * `Attr.checkAssignable` — assigning a `final` that has an initializer, a
//!   blank `final` field outside its class's constructors and initializers,
//!   or a resource ("cannot assign a value to final variable x");
//! * `Attr.checkInit`'s forward-reference rule (JLS 8.3.3) — a field read by
//!   simple name in an initializer of its own class before its declaration,
//!   lambda bodies included ("illegal forward reference", "self-reference in
//!   initializer");
//! * `Flow.CaptureAnalyzer` — a local read inside a lambda or a `when` guard
//!   must be final or effectively final, effective finality being what the
//!   assignment walk observed over the whole method.
//!
//! `javac` reports all of a top-level class's attribution errors before its
//! flow errors, and the capture errors after those, so the first diagnostic
//! is chosen in that order (classes in source order).
//!
//! What the walk cannot see, because the parser lowered it away: an `assert`
//! (dropped, so a read inside one is not checked), the bodies of anonymous
//! and local classes in their enclosing method (they are analyzed as classes
//! of their own, so a captured local is not checked there), and a lambda
//! parameter's `final`.

use std::collections::{HashMap, HashSet};

use crate::ast::*;

/// Run the analysis over every user class; the first diagnostic `javac`
/// would print, in `javac`'s words.
pub fn check(prog: &Program) -> Result<(), String> {
    // A local or anonymous class reads the enclosing locals it captured as
    // fields; which of them its bodies really name (and whether they assign
    // one) is found by analyzing the class first, and judged where the
    // enclosing method creates it.
    let base = Classes::new(prog);
    let mut inner_refs = HashMap::new();
    for cl in prog
        .classes
        .iter()
        .filter(|c| c.captures.iter().any(|p| p.name != OUTER_THIS))
    {
        let mut flow = Flow::new(&base, cl);
        flow.class(prog);
        inner_refs.insert(cl.name.as_str(), flow.free_captured);
    }
    let classes = Classes { inner_refs, ..base };
    let mut best: Option<(u32, Diag)> = None;
    for cl in &prog.classes {
        let mut flow = Flow::new(&classes, cl);
        flow.class(prog);
        let group = classes.top_line(cl);
        if let Some(d) = flow.first_diag() {
            let better = match &best {
                None => true,
                Some((g, b)) => (group, d.phase) < (*g, b.phase),
            };
            if better {
                best = Some((group, d));
            }
        }
    }
    match best {
        Some((_, d)) => Err(format!("javars: {} (line {})", d.msg, d.line)),
        None => Ok(()),
    }
}

/// The program's classes, indexed for field lookup.
struct Classes<'a> {
    by_name: HashMap<&'a str, &'a Class>,
    by_binary: HashMap<&'a str, &'a Class>,
    /// Per local or anonymous class, the captured locals its bodies name,
    /// each with whether one of them assigns it.
    inner_refs: HashMap<&'a str, Vec<(String, bool)>>,
    /// The `static` methods, by declaring class.
    statics: HashMap<&'a str, Vec<&'a Method>>,
}

impl<'a> Classes<'a> {
    fn new(prog: &'a Program) -> Self {
        Classes {
            by_name: prog.classes.iter().map(|c| (c.name.as_str(), c)).collect(),
            by_binary: prog
                .classes
                .iter()
                .map(|c| (c.binary.as_str(), c))
                .collect(),
            inner_refs: HashMap::new(),
            statics: prog.methods.iter().fold(HashMap::new(), |mut m, f| {
                m.entry(f.owner.as_str()).or_insert_with(Vec::new).push(f);
                m
            }),
        }
    }

    /// The source line of the top-level class `cl` is nested in: `javac`
    /// attributes and flow-checks one top-level class at a time, in order.
    fn top_line(&self, cl: &Class) -> u32 {
        let top = cl.binary.split('$').next().unwrap_or(&cl.binary);
        self.by_binary.get(top).map_or(cl.line, |c| c.line)
    }

    /// The class immediately enclosing `cl`, by its binary name.
    fn enclosing(&self, cl: &Class) -> Option<&'a Class> {
        let (outer, _) = cl.binary.rsplit_once('$')?;
        self.by_binary.get(outer).copied()
    }

    /// The field a simple name `name` used in `cl` denotes: `cl`'s own, then
    /// its supertypes', then an enclosing class's (JLS 6.5.6.1). The
    /// synthetic fields holding captured values are not source fields.
    fn field(&self, cl: &'a Class, name: &str) -> Option<(&'a Class, &'a FieldDecl)> {
        let mut seen = HashSet::new();
        let mut cur = Some(cl);
        while let Some(c) = cur {
            if let Some(found) = self.member_field(c, name, &mut seen) {
                return Some(found);
            }
            cur = self.enclosing(c);
        }
        None
    }

    /// The one method named `name` taking `argc` arguments that a call on
    /// `cl` reaches — declared or inherited, and for an `unqualified` call
    /// also an enclosing class's. `None` when there is none, or more than one
    /// (an overload this cannot pick between).
    fn method(
        &self,
        cl: &'a Class,
        name: &str,
        argc: usize,
        unqualified: bool,
    ) -> Option<&'a Method> {
        let mut found: Vec<&'a Method> = Vec::new();
        let mut cur = Some(cl);
        while let Some(c) = cur {
            let mut seen = HashSet::new();
            self.member_methods(c, name, argc, &mut seen, &mut found);
            if !found.is_empty() || !unqualified {
                break;
            }
            cur = self.enclosing(c);
        }
        match found.as_slice() {
            [m] => Some(m),
            _ => None,
        }
    }

    fn member_methods(
        &self,
        c: &'a Class,
        name: &str,
        argc: usize,
        seen: &mut HashSet<&'a str>,
        out: &mut Vec<&'a Method>,
    ) {
        if !seen.insert(c.name.as_str()) {
            return;
        }
        let statics = self
            .statics
            .get(c.name.as_str())
            .into_iter()
            .flatten()
            .copied();
        for m in c.methods.iter().chain(statics) {
            let overridden = out.iter().any(|o| {
                o.params.len() == m.params.len()
                    && o.params.iter().zip(&m.params).all(|(a, b)| a.ty == b.ty)
            });
            if m.name == name && m.params.len() == argc && !overridden {
                out.push(m);
            }
        }
        for s in c.superclass.iter().chain(&c.interfaces) {
            if let Some(sc) = self.by_name.get(s.as_str()) {
                self.member_methods(sc, name, argc, seen, out);
            }
        }
    }

    /// A field `c` declares or inherits.
    fn member_field(
        &self,
        c: &'a Class,
        name: &str,
        seen: &mut HashSet<&'a str>,
    ) -> Option<(&'a Class, &'a FieldDecl)> {
        if !seen.insert(c.name.as_str()) {
            return None;
        }
        let own = c
            .static_fields
            .iter()
            .chain(&c.fields)
            .find(|f| f.name == name && !c.captures.iter().any(|p| p.name == name));
        if let Some(f) = own {
            return Some((c, f));
        }
        c.superclass
            .iter()
            .chain(&c.interfaces)
            .filter_map(|s| self.by_name.get(s.as_str()).copied())
            .find_map(|s| self.member_field(s, name, seen))
    }
}

/// Whether `f` (declared in `c`) is `final`. An interface field is
/// implicitly `public static final` (JLS 9.3).
fn field_is_final(c: &Class, f: &FieldDecl) -> bool {
    f.is_final || c.is_interface
}

fn field_is_static(c: &Class, f: &FieldDecl) -> bool {
    c.is_interface || c.static_fields.iter().any(|s| std::ptr::eq(s, f))
}

/// A set of variable addresses — `javac`'s `Bits`.
#[derive(Clone, Default, Debug, PartialEq)]
struct Bits(Vec<u64>);

impl Bits {
    fn is_member(&self, a: usize) -> bool {
        self.0.get(a / 64).is_some_and(|w| w >> (a % 64) & 1 == 1)
    }

    fn incl(&mut self, a: usize) {
        if self.0.len() <= a / 64 {
            self.0.resize(a / 64 + 1, 0);
        }
        self.0[a / 64] |= 1 << (a % 64);
    }

    fn excl(&mut self, a: usize) {
        if let Some(w) = self.0.get_mut(a / 64) {
            *w &= !(1 << (a % 64));
        }
    }

    /// Include every address in `from..to`.
    fn incl_range(&mut self, from: usize, to: usize) {
        for a in from..to {
            self.incl(a);
        }
    }

    /// Exclude every address from `from` on.
    fn exclude_from(&mut self, from: usize) {
        for a in from..self.0.len() * 64 {
            self.excl(a);
        }
    }

    fn and_set(&mut self, o: &Bits) {
        for (i, w) in self.0.iter_mut().enumerate() {
            *w &= o.0.get(i).copied().unwrap_or(0);
        }
    }

    fn or_set(&mut self, o: &Bits) {
        if self.0.len() < o.0.len() {
            self.0.resize(o.0.len(), 0);
        }
        for (i, w) in o.0.iter().enumerate() {
            self.0[i] |= w;
        }
    }

    /// Whether some address from `from` on is in `self` but not in `o`.
    fn has_outside(&self, o: &Bits, from: usize) -> bool {
        (from..self.0.len() * 64).any(|a| self.is_member(a) && !o.is_member(a))
    }
}

/// Which `javac` phase reports a diagnostic, in the order a class's
/// diagnostics come out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Phase {
    Attr,
    Flow,
    Capture,
}

#[derive(Clone, Debug)]
struct Diag {
    phase: Phase,
    line: u32,
    msg: String,
}

/// What a variable is, as far as the assignment rules care.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Local,
    /// A method, constructor, lambda or `catch` parameter (javac's
    /// `PARAMETER` flag).
    Param,
    /// A multi-catch parameter: implicitly `final`.
    MultiCatch,
    /// A try-with-resources variable: implicitly `final`, with an
    /// initializer.
    Resource,
    StaticField,
    InstanceField,
}

/// One declaration. Its address can be reused once it leaves scope, but its
/// effective finality belongs to the declaration, so it lives here.
#[derive(Clone, Debug)]
struct Decl {
    name: String,
    kind: Kind,
    is_final: bool,
    has_init: bool,
    /// javac's `EFFECTIVELY_FINAL`: set on a non-`final` declaration and
    /// cleared by any assignment while it is not definitely unassigned.
    eff_final: bool,
    /// Declared type (`var` included), for a field access through it.
    ty: String,
    /// The declared type's arguments and its lower-bounded wildcard
    /// positions (see [`StmtKind::Local::super_slots`]).
    targs: Vec<String>,
    super_slots: Vec<usize>,
    /// The value, for a *constant variable* (JLS 4.12.4).
    constant: Option<Const>,
}

impl Decl {
    fn is_field(&self) -> bool {
        matches!(self.kind, Kind::StaticField | Kind::InstanceField)
    }

    /// Final for the capture rule: declared so, or implicitly.
    fn final_or_implicit(&self) -> bool {
        self.is_final || matches!(self.kind, Kind::MultiCatch | Kind::Resource)
    }
}

/// The second, speculative pass over a loop body reports a reassigned final
/// differently (javac's `FlowKind`).
#[derive(Clone, Copy, PartialEq)]
enum FlowKind {
    Normal,
    SpeculativeLoop,
}

/// A statement a `break`, `continue` or `yield` may name.
#[derive(Clone, Copy, PartialEq)]
enum JumpKind<'a> {
    Loop,
    Switch,
    SwitchExpr,
    Label(&'a str),
}

#[derive(Clone, Copy, PartialEq)]
enum ExitKind {
    Break,
    Continue,
    Yield,
    Return,
}

/// An outward transfer of control waiting for its target to resolve it,
/// with the state it carries there (javac's `AssignPendingExit`).
struct Exit {
    kind: ExitKind,
    target: usize,
    line: u32,
    inits: Bits,
    uninits: Bits,
}

/// The body being analyzed, for `Attr`'s blank-final assignability.
#[derive(Clone, Copy, PartialEq)]
enum Ctx {
    StaticInit,
    InstanceInit,
    Ctor,
    Method,
}

#[derive(Clone, Copy, PartialEq)]
enum CaptureKind {
    Lambda,
    Guard,
    InnerClass,
}

/// A local referenced from inside a lambda or guard that it was declared
/// outside of; judged once the whole class's effective finality is known.
struct CaptureRef {
    decl: usize,
    line: u32,
    write: bool,
    kind: CaptureKind,
}

/// The when-true and when-false states after a condition.
struct Split {
    t_inits: Bits,
    t_uninits: Bits,
    f_inits: Bits,
    f_uninits: Bits,
}

struct Flow<'a> {
    classes: &'a Classes<'a>,
    class: &'a Class,
    inits: Bits,
    uninits: Bits,
    /// The variables DU everywhere in the current `try` block (maintained
    /// lazily, as in javac: intersect with `uninits` to read it).
    uninits_try: Bits,
    /// Address → declaration.
    vars: Vec<usize>,
    decls: Vec<Decl>,
    /// Declaration site (the AST node's address) → declaration, so the second
    /// pass over a loop body finds the declarations of the first.
    sites: HashMap<usize, usize>,
    /// Names in scope, innermost last.
    scope: Vec<(&'a str, usize)>,
    /// The class's blank `final` fields, by name.
    fields: HashMap<&'a str, usize>,
    firstadr: usize,
    nextadr: usize,
    returnadr: usize,
    pending: Vec<Exit>,
    jumps: Vec<(usize, JumpKind<'a>)>,
    next_jump: usize,
    flow_kind: FlowKind,
    initial_ctor: bool,
    ctx: Ctx,
    /// Inside a field initializer or initializer block: whether static, and
    /// the position of the statement among the class's initialization
    /// statements (see [`FieldDecl::init_pos`]).
    init_env: Option<(bool, usize)>,
    /// The declared return type of the method being analyzed, with its type
    /// arguments; `None` in a lambda body, an initializer or a `void` method.
    ret: Option<(&'a str, &'a [String])>,
    captures: Vec<(CaptureKind, usize)>,
    capture_refs: Vec<CaptureRef>,
    /// The captured locals this (local or anonymous) class's bodies name.
    free_captured: Vec<(String, bool)>,
    diags: Vec<Diag>,
    flow_errors: usize,
}

impl<'a> Flow<'a> {
    fn new(classes: &'a Classes<'a>, class: &'a Class) -> Self {
        Flow {
            classes,
            class,
            inits: Bits::default(),
            uninits: Bits::default(),
            uninits_try: Bits::default(),
            vars: Vec::new(),
            decls: Vec::new(),
            sites: HashMap::new(),
            scope: Vec::new(),
            fields: HashMap::new(),
            firstadr: 0,
            nextadr: 0,
            returnadr: 0,
            pending: Vec::new(),
            jumps: Vec::new(),
            next_jump: 0,
            flow_kind: FlowKind::Normal,
            initial_ctor: false,
            ctx: Ctx::Method,
            init_env: None,
            ret: None,
            captures: Vec::new(),
            capture_refs: Vec::new(),
            free_captured: Vec::new(),
            diags: Vec::new(),
            flow_errors: 0,
        }
    }

    // ----- diagnostics -------------------------------------------------

    fn error(&mut self, phase: Phase, line: u32, msg: String) {
        // javac reports one error per position.
        if self
            .diags
            .iter()
            .any(|d| d.phase == phase && d.line == line && d.msg == msg)
        {
            return;
        }
        if phase == Phase::Flow {
            self.flow_errors += 1;
        }
        self.diags.push(Diag { phase, line, msg });
    }

    /// The first diagnostic in `javac`'s order: attribution errors by
    /// position, then flow errors as found, then capture errors.
    fn first_diag(&self) -> Option<Diag> {
        let attr = self
            .diags
            .iter()
            .filter(|d| d.phase == Phase::Attr)
            .min_by_key(|d| d.line);
        attr.or_else(|| self.diags.iter().find(|d| d.phase == Phase::Flow))
            .or_else(|| self.diags.iter().find(|d| d.phase == Phase::Capture))
            .cloned()
    }

    // ----- variables ---------------------------------------------------

    fn decl(&self, adr: usize) -> &Decl {
        &self.decls[self.vars[adr]]
    }

    /// Give a declaration the next address, DU and not DA (javac's
    /// `newVar`). `site` identifies the declaring node.
    fn new_var(&mut self, site: usize, mut decl: Decl) -> usize {
        decl.eff_final = !decl.final_or_implicit();
        let d = match self.sites.get(&site) {
            Some(&d) => {
                self.decls[d].eff_final = decl.eff_final;
                d
            }
            None => {
                self.decls.push(decl);
                self.sites.insert(site, self.decls.len() - 1);
                self.decls.len() - 1
            }
        };
        let adr = self.nextadr;
        if self.vars.len() <= adr {
            self.vars.resize(adr + 1, 0);
        }
        self.vars[adr] = d;
        self.inits.excl(adr);
        self.uninits.incl(adr);
        self.nextadr += 1;
        adr
    }

    /// Declare a local-like variable and bring its name into scope.
    fn declare(
        &mut self,
        site: usize,
        name: &'a str,
        kind: Kind,
        is_final: bool,
        ty: &str,
    ) -> usize {
        let adr = self.new_var(
            site,
            Decl {
                name: name.to_string(),
                kind,
                is_final,
                has_init: false,
                eff_final: false,
                ty: ty.to_string(),
                targs: Vec::new(),
                super_slots: Vec::new(),
                constant: None,
            },
        );
        self.scope.push((name, adr));
        adr
    }

    /// Record a declaration's type arguments (see [`Decl::targs`]).
    fn set_type_args(&mut self, adr: usize, targs: &[String], super_slots: &[usize]) {
        let d = self.vars[adr];
        self.decls[d].targs = targs.to_vec();
        self.decls[d].super_slots = super_slots.to_vec();
    }

    /// A parameter is assigned on entry (javac's `initParam`).
    fn init_param(&mut self, adr: usize) {
        self.inits.incl(adr);
        self.uninits.excl(adr);
    }

    fn lookup(&self, name: &str) -> Option<usize> {
        self.scope
            .iter()
            .rev()
            .find(|(n, _)| *n == name)
            .map(|&(_, a)| a)
            .or_else(|| self.fields.get(name).copied())
    }

    /// The address of a local (or parameter) named `name`, ignoring fields.
    fn local(&self, name: &str) -> Option<usize> {
        self.scope
            .iter()
            .rev()
            .find(|(n, _)| *n == name)
            .map(|&(_, a)| a)
    }

    /// Record an assignment (javac's `letInit`).
    fn let_init(&mut self, adr: usize, line: u32) {
        let d = self.vars[adr];
        if adr >= self.firstadr {
            let (eff_final, is_final) =
                (self.decls[d].eff_final, self.decls[d].final_or_implicit());
            if eff_final {
                if !self.uninits.is_member(adr) {
                    self.decls[d].eff_final = false;
                } else {
                    self.uninit(adr);
                }
            } else if is_final {
                let (name, kind) = (self.decls[d].name.clone(), self.decls[d].kind);
                match kind {
                    Kind::Param => self.error(
                        Phase::Flow,
                        line,
                        format!("final parameter {name} may not be assigned"),
                    ),
                    Kind::MultiCatch => self.error(
                        Phase::Flow,
                        line,
                        format!("multi-catch parameter {name} may not be assigned"),
                    ),
                    _ if !self.uninits.is_member(adr) => {
                        let msg = match self.flow_kind {
                            FlowKind::Normal => "might already have been assigned",
                            FlowKind::SpeculativeLoop => "might be assigned in loop",
                        };
                        self.error(Phase::Flow, line, format!("variable {name} {msg}"));
                    }
                    _ => self.uninit(adr),
                }
            }
            self.inits.incl(adr);
        } else if self.decls[d].final_or_implicit() {
            let name = self.decls[d].name.clone();
            self.error(
                Phase::Flow,
                line,
                format!("variable {name} might already have been assigned"),
            );
        }
    }

    fn uninit(&mut self, adr: usize) {
        if !self.inits.is_member(adr) {
            // A reachable assignment.
            self.uninits.excl(adr);
            self.uninits_try.excl(adr);
        } else {
            self.uninits.excl(adr);
        }
    }

    /// A read must find the variable DA (javac's `checkInit`).
    fn check_init(&mut self, adr: usize, line: u32) {
        let msg = format!(
            "variable {} might not have been initialized",
            self.decl(adr).name
        );
        self.check_init_msg(adr, line, msg);
    }

    fn check_init_msg(&mut self, adr: usize, line: u32, msg: String) {
        if (adr >= self.firstadr || !self.decl(adr).is_field()) && !self.inits.is_member(adr) {
            self.error(Phase::Flow, line, msg);
            self.inits.incl(adr);
        }
    }

    /// Everything after an abrupt completion is vacuously assigned and
    /// unassigned (javac's `markDead`).
    fn mark_dead(&mut self) {
        for a in self.returnadr..self.nextadr {
            let static_blank = self.initial_ctor && self.decl(a).kind == Kind::StaticField;
            if !static_blank {
                self.inits.incl(a);
            }
        }
        self.uninits.incl_range(self.returnadr, self.nextadr);
    }

    /// Whether the state is the vacuous one [`Flow::mark_dead`] leaves.
    fn is_dead(&self) -> bool {
        self.returnadr < self.nextadr
            && (self.returnadr..self.nextadr)
                .all(|a| self.inits.is_member(a) && self.uninits.is_member(a))
    }

    fn record_exit(&mut self, kind: ExitKind, target: usize, line: u32) {
        self.pending.push(Exit {
            kind,
            target,
            line,
            inits: self.inits.clone(),
            uninits: self.uninits.clone(),
        });
        self.mark_dead();
    }

    /// Resolve the pending exits of `kind` aimed at `target`, joining their
    /// states into the current one; the others stay pending, after `prev`.
    fn resolve(&mut self, kind: ExitKind, target: usize, prev: Vec<Exit>) {
        let exits = std::mem::replace(&mut self.pending, prev);
        for e in exits {
            if e.kind == kind && e.target == target {
                self.inits.and_set(&e.inits);
                self.uninits.and_set(&e.uninits);
            } else {
                self.pending.push(e);
            }
        }
    }

    fn resolve_continues(&mut self, target: usize) {
        self.resolve(ExitKind::Continue, target, Vec::new());
    }

    fn push_jump(&mut self, kind: JumpKind<'a>) -> usize {
        self.next_jump += 1;
        self.jumps.push((self.next_jump, kind));
        self.next_jump
    }

    /// The statement `break label` / `break` leaves.
    fn break_target(&self, label: Option<&str>) -> Option<usize> {
        self.jumps
            .iter()
            .rev()
            .find_map(|&(id, k)| match (label, k) {
                (Some(l), JumpKind::Label(n)) if l == n => Some(id),
                (None, JumpKind::Loop | JumpKind::Switch) => Some(id),
                _ => None,
            })
    }

    /// The loop `continue label` / `continue` continues: a labeled continue
    /// names the loop its label (or a chain of labels) prefixes.
    fn continue_target(&self, label: Option<&str>) -> Option<usize> {
        match label {
            None => self
                .jumps
                .iter()
                .rev()
                .find(|(_, k)| *k == JumpKind::Loop)
                .map(|&(id, _)| id),
            Some(l) => {
                let at = self
                    .jumps
                    .iter()
                    .rposition(|(_, k)| *k == JumpKind::Label(l))?;
                self.jumps[at + 1..]
                    .iter()
                    .find(|(_, k)| *k == JumpKind::Loop)
                    .map(|&(id, _)| id)
            }
        }
    }

    // ----- classes -----------------------------------------------------

    /// Analyze one class: its static initialization, its instance
    /// initialization, then its constructors and methods (javac's
    /// `visitClassDef`).
    fn class(&mut self, prog: &'a Program) {
        let cl = self.class;
        let captured = |f: &FieldDecl| cl.captures.iter().any(|c| c.name == f.name);
        let blank = |f: &FieldDecl| f.is_final && !f.has_init && !captured(f) && !cl.is_record;

        for f in cl.static_fields.iter().filter(|f| blank(f)) {
            self.declare_field(f, Kind::StaticField);
        }
        self.ctx = Ctx::StaticInit;
        self.initializers(&cl.static_init, &cl.static_fields, true);
        for a in self.firstadr..self.nextadr {
            if self.decl(a).kind == Kind::StaticField {
                let line = field_line(cl, &self.decl(a).name);
                self.check_init(a, line);
            }
        }

        for f in cl.fields.iter().filter(|f| blank(f)) {
            self.declare_field(f, Kind::InstanceField);
        }
        self.ctx = Ctx::InstanceInit;
        self.initializers(&cl.inst_init, &cl.fields, false);

        // Constructors and methods, in source order; `main` has no recorded
        // line and goes last.
        enum Member<'a> {
            Ctor(&'a Ctor),
            Method(&'a Method),
            Main,
        }
        let mut members: Vec<(u32, Member<'a>)> = Vec::new();
        members.extend(cl.ctors.iter().map(|c| (c.line, Member::Ctor(c))));
        members.extend(cl.methods.iter().map(|m| (m.line, Member::Method(m))));
        members.extend(
            prog.methods
                .iter()
                .filter(|m| m.owner == cl.name)
                .map(|m| (m.line, Member::Method(m))),
        );
        if prog.class_name == cl.name {
            members.push((u32::MAX, Member::Main));
        }
        members.sort_by_key(|(line, _)| *line);
        if cl.ctors.is_empty() && !cl.is_interface {
            self.default_ctor();
        }
        for (_, m) in members {
            match m {
                Member::Ctor(c) => self.ctor(c),
                Member::Method(m) if !m.is_abstract => {
                    self.ret =
                        (m.ret != "void").then_some((m.ret.as_str(), m.ret_targs.as_slice()));
                    self.method(m.params.iter().map(Some).collect(), &m.body);
                    self.ret = None;
                }
                Member::Method(_) => {}
                Member::Main => {
                    let site = &prog.main_param as *const _ as usize;
                    self.ctx = Ctx::Method;
                    let state = self.enter_method(false);
                    if let Some(p) = &prog.main_param {
                        let a = self.declare(site, p, Kind::Param, false, "String[]");
                        self.init_param(a);
                    }
                    self.stmts(&prog.main);
                    self.exit_method(state);
                }
            }
        }
        self.report_captures();
    }

    fn declare_field(&mut self, f: &'a FieldDecl, kind: Kind) {
        let adr = self.new_var(
            f as *const _ as usize,
            Decl {
                name: f.name.clone(),
                kind,
                is_final: true,
                has_init: false,
                eff_final: false,
                ty: f.ty.clone(),
                targs: f.targs.clone(),
                super_slots: f.super_slots.clone(),
                constant: None,
            },
        );
        self.fields.insert(&f.name, adr);
    }

    /// The field initializers and initializer blocks of one kind, in
    /// textual order. A field's initializer is its declaration's, not an
    /// assignment.
    fn initializers(&mut self, init: &'a [Stmt], fields: &'a [FieldDecl], is_static: bool) {
        for (i, st) in init.iter().enumerate() {
            self.init_env = Some((is_static, i));
            let own = fields.iter().find(|f| f.has_init && f.init_pos == i);
            match (own, &st.kind) {
                (Some(_), StmtKind::Assign { value, .. } | StmtKind::FieldAssign { value, .. }) => {
                    self.expr(value, st.line)
                }
                _ => self.stmt(st),
            }
            // An initializer's own exits are errors javac reports elsewhere.
            self.pending.clear();
        }
        self.init_env = None;
    }

    /// Saved per-method state (javac's `visitMethodDef` prologue).
    fn enter_method(&mut self, initial_ctor: bool) -> MethodState {
        let saved = MethodState {
            inits: self.inits.clone(),
            uninits: self.uninits.clone(),
            nextadr: self.nextadr,
            firstadr: self.firstadr,
            returnadr: self.returnadr,
            initial_ctor: self.initial_ctor,
            scope: self.scope.len(),
        };
        self.initial_ctor = initial_ctor;
        if !initial_ctor {
            self.firstadr = self.nextadr;
        }
        saved
    }

    fn exit_method(&mut self, s: MethodState) {
        self.pending.clear();
        self.inits = s.inits;
        self.uninits = s.uninits;
        self.nextadr = s.nextadr;
        self.firstadr = s.firstadr;
        self.returnadr = s.returnadr;
        self.initial_ctor = s.initial_ctor;
        self.scope.truncate(s.scope);
    }

    fn params(&mut self, params: &[Option<&'a Param>]) {
        for p in params.iter().flatten() {
            let a = self.declare(
                *p as *const _ as usize,
                &p.name,
                Kind::Param,
                p.is_final,
                &p.ty,
            );
            self.init_param(a);
            self.set_type_args(a, &p.targs, &p.super_slots);
        }
    }

    fn method(&mut self, params: Vec<Option<&'a Param>>, body: &'a [Stmt]) {
        self.ctx = Ctx::Method;
        let state = self.enter_method(false);
        self.params(&params);
        self.stmts(body);
        self.exit_method(state);
    }

    /// A constructor: one that does not begin `this(…)` is *initial*, and must
    /// leave every blank `final` instance field assigned — at its end and at
    /// every `return`.
    fn ctor(&mut self, c: &'a Ctor) {
        let delegates = matches!(
            c.body.first().map(|s| &s.kind),
            Some(StmtKind::Expr(Expr::Call { name, .. })) if name == "this"
        );
        self.ctx = Ctx::Ctor;
        let state = self.enter_method(!delegates);
        let captured: Vec<&str> = self
            .class
            .captures
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        let params: Vec<Option<&'a Param>> = c
            .params
            .iter()
            .map(|p| (!captured.contains(&p.name.as_str())).then_some(p))
            .collect();
        self.params(&params);
        self.stmts(&c.body);
        if !delegates {
            for a in self.firstadr..self.nextadr {
                if self.decl(a).kind != Kind::InstanceField {
                    continue;
                }
                let name = self.decl(a).name.clone();
                if c.implicit {
                    let line = field_line(self.class, &name);
                    let msg = format!("variable {name} not initialized in the default constructor");
                    self.check_init_msg(a, line, msg);
                } else {
                    self.check_init(a, c.end_line);
                }
            }
            for e in std::mem::take(&mut self.pending) {
                if e.kind == ExitKind::Return {
                    self.inits = e.inits;
                    for a in self.firstadr..self.nextadr {
                        self.check_init(a, e.line);
                    }
                }
            }
        }
        self.exit_method(state);
    }

    /// The constructor `javac` supplies when a class declares none.
    fn default_ctor(&mut self) {
        self.ctx = Ctx::Ctor;
        let state = self.enter_method(true);
        for a in self.firstadr..self.nextadr {
            if self.decl(a).kind == Kind::InstanceField {
                let name = self.decl(a).name.clone();
                let line = field_line(self.class, &name);
                let msg = format!("variable {name} not initialized in the default constructor");
                self.check_init_msg(a, line, msg);
            }
        }
        self.exit_method(state);
    }

    /// Capture errors, now that every declaration's effective finality is
    /// final (javac's `CaptureAnalyzer`, which runs after the assignment
    /// analysis).
    fn report_captures(&mut self) {
        let refs = std::mem::take(&mut self.capture_refs);
        for r in refs {
            let d = &self.decls[r.decl];
            let what = match r.kind {
                CaptureKind::Lambda => "a lambda expression",
                CaptureKind::Guard => "a guard",
                CaptureKind::InnerClass => "an inner class",
            };
            let msg = if r.write && r.kind == CaptureKind::Guard {
                format!(
                    "cannot assign to {}, as it was not declared inside the guard",
                    d.name
                )
            } else if r.write || !(d.final_or_implicit() || d.eff_final) {
                format!("local variables referenced from {what} must be final or effectively final")
            } else {
                continue;
            };
            self.error(Phase::Capture, r.line, msg);
        }
    }

    // ----- statements --------------------------------------------------

    /// A block: its declarations end with it (javac's `visitBlock`).
    fn stmts(&mut self, body: &'a [Stmt]) {
        let (nextadr, scope) = (self.nextadr, self.scope.len());
        for (i, s) in body.iter().enumerate() {
            match &s.kind {
                StmtKind::Local { .. } => self.local_decl(s, body.get(i + 1)),
                _ => self.stmt(s),
            }
        }
        self.nextadr = nextadr;
        self.scope.truncate(scope);
    }

    fn stmt(&mut self, s: &'a Stmt) {
        let line = s.line;
        match &s.kind {
            StmtKind::Local { .. } => self.local_decl(s, None),
            StmtKind::Locals(decls) => {
                for d in decls {
                    self.stmt(d);
                }
            }
            StmtKind::Assign { name, op, value } => {
                self.check_assignable_name(name, line);
                if *op == AssignOp::Assign {
                    self.expr(value, line);
                    self.check_assigned_capture(name, value, line);
                } else {
                    self.name_read(name, line);
                    self.expr(value, line);
                }
                self.name_write(name, line);
            }
            StmtKind::IndexAssign {
                array,
                index,
                value,
                ..
            } => {
                self.expr(array, line);
                self.expr(index, line);
                self.expr(value, line);
            }
            StmtKind::FieldAssign {
                recv,
                name,
                op,
                value,
            } => self.field_assign(recv, name, *op, value, line),
            StmtKind::Expr(Expr::SwitchExpr { disc, arms, .. }) => {
                self.switch_arms(disc, arms, true, line)
            }
            StmtKind::Expr(e) => self.expr(e, line),
            StmtKind::If { cond, then, els } => {
                let s = self.cond(cond, line);
                self.inits = s.t_inits;
                self.uninits = s.t_uninits;
                self.stmts(then);
                let (then_inits, then_uninits) = (self.inits.clone(), self.uninits.clone());
                self.inits = s.f_inits;
                self.uninits = s.f_uninits;
                self.stmts(els);
                self.inits.and_set(&then_inits);
                self.uninits.and_set(&then_uninits);
            }
            StmtKind::While { cond, body } => self.while_loop(cond, body, line),
            StmtKind::DoWhile { body, cond } => self.do_loop(body, cond, line),
            StmtKind::For {
                init,
                cond,
                update,
                body,
            } => self.for_loop(init, cond.as_ref(), update, body, line),
            StmtKind::ForEach {
                ty,
                targs,
                super_slots,
                name,
                is_final,
                iter,
                body,
            } => self.foreach_loop(s, (ty, targs, super_slots), name, *is_final, iter, body),
            StmtKind::Switch { disc, groups } => self.switch_groups(disc, groups, line),
            StmtKind::Labeled { label, body } => {
                let prev = std::mem::take(&mut self.pending);
                let id = self.push_jump(JumpKind::Label(label));
                self.stmt(body);
                self.jumps.pop();
                self.resolve(ExitKind::Break, id, prev);
            }
            StmtKind::Try {
                body,
                catches,
                finally_body,
            } => self.try_stmt(body, catches, finally_body),
            StmtKind::Throw(e) => {
                self.expr(e, line);
                self.mark_dead();
            }
            StmtKind::Yield(e) => {
                self.expr(e, line);
                let target = self
                    .jumps
                    .iter()
                    .rev()
                    .find(|(_, k)| *k == JumpKind::SwitchExpr)
                    .map_or(0, |&(id, _)| id);
                self.record_exit(ExitKind::Yield, target, line);
            }
            StmtKind::Break(label) => {
                let target = self.break_target(label.as_deref()).unwrap_or(0);
                self.record_exit(ExitKind::Break, target, line);
            }
            StmtKind::Continue(label) => {
                let target = self.continue_target(label.as_deref()).unwrap_or(0);
                self.record_exit(ExitKind::Continue, target, line);
            }
            StmtKind::Return(e) => {
                if let Some(e) = e {
                    self.expr(e, line);
                    if let Some((ty, targs)) = self.ret {
                        self.check_capture_conversion(e, ty, targs, line);
                    }
                }
                self.record_exit(ExitKind::Return, 0, line);
            }
        }
    }

    /// A local declaration (javac's `visitVarDef`). `next` is the statement
    /// after it, which tells a try-with-resources variable apart: the parser
    /// follows each resource with the `#twr_primary#` local of its desugaring.
    fn local_decl(&mut self, s: &'a Stmt, next: Option<&'a Stmt>) {
        let StmtKind::Local {
            ty,
            name,
            init,
            is_final,
            targs,
            super_slots,
            ..
        } = &s.kind
        else {
            return;
        };
        // The parser's try-with-resources desugaring (`Parser::with_resources`)
        // declares a `#twr_primary#name` local right after each resource.
        let resource = matches!(
            next.map(|n| &n.kind),
            Some(StmtKind::Local { name: n, .. }) if *n == format!("#twr_primary#{name}")
        );
        let kind = if resource {
            Kind::Resource
        } else {
            Kind::Local
        };
        let adr = self.new_var(
            s as *const _ as usize,
            Decl {
                name: name.clone(),
                kind,
                is_final: *is_final,
                has_init: init.is_some(),
                eff_final: false,
                ty: ty.clone(),
                targs: targs.clone(),
                super_slots: super_slots.clone(),
                constant: None,
            },
        );
        // A local's scope starts with its own initializer (JLS 6.3).
        self.scope.push((name, adr));
        if let Some(init) = init {
            self.expr(init, s.line);
            self.check_capture_conversion(init, ty, targs, s.line);
            if *is_final && is_constant_type(ty) {
                let value = self.const_value(init, 0).and_then(|v| v.convert(ty));
                let d = self.vars[adr];
                self.decls[d].constant = value;
            }
            self.let_init(adr, s.line);
        }
    }

    fn while_loop(&mut self, cond: &'a Expr, body: &'a [Stmt], line: u32) {
        let prev = std::mem::take(&mut self.pending);
        let prev_kind = std::mem::replace(&mut self.flow_kind, FlowKind::Normal);
        let mut skip = (Bits::default(), Bits::default());
        let errors = self.flow_errors;
        let mut entry = self.uninits.clone();
        entry.exclude_from(self.nextadr);
        let id = self.push_jump(JumpKind::Loop);
        loop {
            let s = self.cond(cond, line);
            if self.flow_kind == FlowKind::Normal {
                skip = (s.f_inits, s.f_uninits);
            }
            self.inits = s.t_inits;
            self.uninits = s.t_uninits;
            self.stmts(body);
            self.resolve_continues(id);
            if self.flow_errors != errors
                || self.flow_kind == FlowKind::SpeculativeLoop
                || !entry.has_outside(&self.uninits, self.firstadr)
            {
                break;
            }
            entry.and_set(&self.uninits);
            self.uninits = entry.clone();
            self.flow_kind = FlowKind::SpeculativeLoop;
        }
        self.jumps.pop();
        self.flow_kind = prev_kind;
        self.inits = skip.0;
        self.uninits = skip.1;
        self.resolve(ExitKind::Break, id, prev);
    }

    fn do_loop(&mut self, body: &'a [Stmt], cond: &'a Expr, line: u32) {
        let prev = std::mem::take(&mut self.pending);
        let prev_kind = std::mem::replace(&mut self.flow_kind, FlowKind::Normal);
        let mut skip = (Bits::default(), Bits::default());
        let errors = self.flow_errors;
        let id = self.push_jump(JumpKind::Loop);
        loop {
            let mut entry = self.uninits.clone();
            entry.exclude_from(self.nextadr);
            self.stmts(body);
            self.resolve_continues(id);
            let s = self.cond(cond, line);
            if self.flow_kind == FlowKind::Normal {
                skip = (s.f_inits.clone(), s.f_uninits.clone());
            }
            if self.flow_errors != errors
                || self.flow_kind == FlowKind::SpeculativeLoop
                || !entry.has_outside(&s.t_uninits, self.firstadr)
            {
                break;
            }
            self.inits = s.t_inits;
            entry.and_set(&s.t_uninits);
            self.uninits = entry;
            self.flow_kind = FlowKind::SpeculativeLoop;
        }
        self.jumps.pop();
        self.flow_kind = prev_kind;
        self.inits = skip.0;
        self.uninits = skip.1;
        self.resolve(ExitKind::Break, id, prev);
    }

    fn for_loop(
        &mut self,
        init: &'a [Stmt],
        cond: Option<&'a Expr>,
        update: &'a [Stmt],
        body: &'a [Stmt],
        line: u32,
    ) {
        let prev = std::mem::take(&mut self.pending);
        let prev_kind = std::mem::replace(&mut self.flow_kind, FlowKind::Normal);
        let (nextadr, scope) = (self.nextadr, self.scope.len());
        for s in init {
            self.stmt(s);
        }
        let mut skip = (Bits::default(), Bits::default());
        let errors = self.flow_errors;
        let id = self.push_jump(JumpKind::Loop);
        loop {
            let mut entry = self.uninits.clone();
            entry.exclude_from(self.nextadr);
            match cond {
                Some(c) => {
                    let s = self.cond(c, line);
                    if self.flow_kind == FlowKind::Normal {
                        skip = (s.f_inits, s.f_uninits);
                    }
                    self.inits = s.t_inits;
                    self.uninits = s.t_uninits;
                }
                None if self.flow_kind == FlowKind::Normal => {
                    let mut i = self.inits.clone();
                    i.incl_range(self.firstadr, self.nextadr);
                    let mut u = self.uninits.clone();
                    u.incl_range(self.firstadr, self.nextadr);
                    skip = (i, u);
                }
                None => {}
            }
            self.stmts(body);
            self.resolve_continues(id);
            for s in update {
                self.stmt(s);
            }
            if self.flow_errors != errors
                || self.flow_kind == FlowKind::SpeculativeLoop
                || !entry.has_outside(&self.uninits, self.firstadr)
            {
                break;
            }
            entry.and_set(&self.uninits);
            self.uninits = entry;
            self.flow_kind = FlowKind::SpeculativeLoop;
        }
        self.jumps.pop();
        self.flow_kind = prev_kind;
        self.inits = skip.0;
        self.uninits = skip.1;
        self.resolve(ExitKind::Break, id, prev);
        self.nextadr = nextadr;
        self.scope.truncate(scope);
    }

    fn foreach_loop(
        &mut self,
        s: &'a Stmt,
        (ty, targs, super_slots): (&'a str, &'a [String], &'a [usize]),
        name: &'a str,
        is_final: bool,
        iter: &'a Expr,
        body: &'a [Stmt],
    ) {
        let (nextadr, scope) = (self.nextadr, self.scope.len());
        // The iterable is resolved without the loop variable in scope.
        self.expr(iter, s.line);
        let var = self.declare(s as *const _ as usize, name, Kind::Local, is_final, ty);
        self.set_type_args(var, targs, super_slots);
        let prev = std::mem::take(&mut self.pending);
        let prev_kind = std::mem::replace(&mut self.flow_kind, FlowKind::Normal);
        let start = (self.inits.clone(), self.uninits.clone());
        self.let_init(var, s.line);
        let errors = self.flow_errors;
        let id = self.push_jump(JumpKind::Loop);
        loop {
            let mut entry = self.uninits.clone();
            entry.exclude_from(self.nextadr);
            self.stmts(body);
            self.resolve_continues(id);
            if self.flow_errors != errors
                || self.flow_kind == FlowKind::SpeculativeLoop
                || !entry.has_outside(&self.uninits, self.firstadr)
            {
                break;
            }
            entry.and_set(&self.uninits);
            self.uninits = entry;
            self.flow_kind = FlowKind::SpeculativeLoop;
        }
        self.jumps.pop();
        self.flow_kind = prev_kind;
        self.inits = start.0;
        let mut u = start.1;
        u.and_set(&self.uninits);
        self.uninits = u;
        self.resolve(ExitKind::Break, id, prev);
        self.nextadr = nextadr;
        self.scope.truncate(scope);
    }

    /// A classic `switch` statement: groups fall through, share one scope,
    /// and with no `default` (and no pattern or `null` label, which would
    /// make it exhaustive by obligation) the switch may match nothing.
    fn switch_groups(&mut self, disc: &'a Expr, groups: &'a [SwitchGroup], line: u32) {
        let cases: Vec<Case<'a>> = groups
            .iter()
            .map(|g| Case {
                labels: &g.labels,
                guard: g.guard.as_ref(),
                body: CaseBody::Stmts(&g.body),
                is_default: g.is_default,
            })
            .collect();
        self.switch(disc, &cases, SwitchForm::Statement, line);
    }

    /// An arrow `switch`: a statement when `statement`, else an expression.
    fn switch_arms(&mut self, disc: &'a Expr, arms: &'a [SwitchArm], statement: bool, line: u32) {
        let cases: Vec<Case<'a>> = arms
            .iter()
            .map(|a| Case {
                labels: &a.labels,
                guard: a.guard.as_ref(),
                body: match &a.body {
                    SwitchArmBody::Expr(e) => CaseBody::RuleExpr(e),
                    SwitchArmBody::Block(b) => CaseBody::RuleBlock(b),
                },
                is_default: a.is_default,
            })
            .collect();
        let form = if statement {
            SwitchForm::Statement
        } else {
            SwitchForm::Expression
        };
        self.switch(disc, &cases, form, line);
    }

    /// javac's `handleSwitch`.
    fn switch(&mut self, disc: &'a Expr, cases: &[Case<'a>], form: SwitchForm, line: u32) {
        let prev = std::mem::take(&mut self.pending);
        let (nextadr, scope) = (self.nextadr, self.scope.len());
        self.expr(disc, line);
        let mut switch_inits = self.inits.clone();
        let mut switch_uninits = self.uninits.clone();
        let kind = match form {
            SwitchForm::Statement => JumpKind::Switch,
            SwitchForm::Expression => JumpKind::SwitchExpr,
        };
        let id = self.push_jump(kind);
        for c in cases {
            self.inits = switch_inits.clone();
            self.uninits.and_set(&switch_uninits);
            for l in c.labels {
                self.pattern(l);
            }
            if let Some(g) = c.guard {
                self.captures.push((CaptureKind::Guard, self.nextadr));
                if let Some(s) = self.split_expr(g, line) {
                    self.inits = s.t_inits;
                    self.uninits = s.t_uninits;
                }
                self.captures.pop();
            }
            match c.body {
                CaseBody::Stmts(body) => {
                    let before = self.scope.len();
                    for (i, s) in body.iter().enumerate() {
                        match &s.kind {
                            StmtKind::Local { .. } => self.local_decl(s, body.get(i + 1)),
                            _ => self.stmt(s),
                        }
                    }
                    // A declaration in one group is in scope, unassigned, in
                    // the later ones (javac's `addVars`).
                    let top: HashSet<usize> = body
                        .iter()
                        .flat_map(|s| match &s.kind {
                            StmtKind::Locals(ds) => ds.iter().collect(),
                            StmtKind::Local { .. } => vec![s],
                            _ => Vec::new(),
                        })
                        .filter_map(|d| self.sites.get(&(d as *const Stmt as usize)).copied())
                        .collect();
                    for &(_, a) in &self.scope[before..] {
                        if top.contains(&self.vars[a]) {
                            switch_inits.excl(a);
                            switch_uninits.incl(a);
                        }
                    }
                }
                CaseBody::RuleExpr(e) => {
                    self.expr(e, line);
                    let exit = match form {
                        SwitchForm::Statement => ExitKind::Break,
                        SwitchForm::Expression => ExitKind::Yield,
                    };
                    self.record_exit(exit, id, line);
                }
                CaseBody::RuleBlock(body) => {
                    self.stmts(body);
                    if form == SwitchForm::Statement {
                        // The synthetic `break` of a rule that completes
                        // normally; recorded from a dead state it joins
                        // nothing away.
                        self.record_exit(ExitKind::Break, id, line);
                    }
                }
            }
        }
        let exhaustive = cases.iter().any(|c| {
            c.is_default
                || c.labels
                    .iter()
                    .any(|l| matches!(l, Expr::TypePattern { .. }) || is_null(l))
        });
        if !exhaustive {
            match form {
                SwitchForm::Expression => self.mark_dead(),
                SwitchForm::Statement => {
                    self.inits = switch_inits;
                    self.uninits.and_set(&switch_uninits);
                }
            }
        }
        self.jumps.pop();
        let exit = match form {
            SwitchForm::Statement => ExitKind::Break,
            SwitchForm::Expression => ExitKind::Yield,
        };
        self.resolve(exit, id, prev);
        self.nextadr = nextadr;
        self.scope.truncate(scope);
    }

    /// A `case` label: a pattern's bindings are parameters, assigned on match.
    fn pattern(&mut self, l: &'a Expr) {
        if let Expr::TypePattern {
            class,
            binding,
            components,
        } = l
        {
            if let Some(b) = binding {
                let a = self.declare(b as *const _ as usize, b, Kind::Local, false, class);
                self.init_param(a);
            }
            for c in components {
                self.pattern(c);
            }
        }
    }

    fn try_stmt(&mut self, body: &'a [Stmt], catches: &'a [CatchArm], finally_body: &'a [Stmt]) {
        let uninits_try_prev = self.uninits_try.clone();
        let prev = std::mem::take(&mut self.pending);
        let inits_try = self.inits.clone();
        self.uninits_try = self.uninits.clone();
        self.stmts(body);
        self.uninits_try.and_set(&self.uninits);
        let mut inits_end = self.inits.clone();
        let mut uninits_end = self.uninits.clone();
        let catch_inits = inits_try.clone();
        let catch_uninits = self.uninits_try.clone();
        for c in catches {
            self.inits = catch_inits.clone();
            self.uninits = catch_uninits.clone();
            let (nextadr, scope) = (self.nextadr, self.scope.len());
            let multi = c.types.len() > 1;
            let kind = if multi { Kind::MultiCatch } else { Kind::Param };
            let ty = c.types.first().map_or("Throwable", String::as_str);
            let a = self.declare(
                c as *const _ as usize,
                &c.name,
                kind,
                c.is_final || multi,
                ty,
            );
            self.init_param(a);
            self.stmts(&c.body);
            inits_end.and_set(&self.inits);
            uninits_end.and_set(&self.uninits);
            self.nextadr = nextadr;
            self.scope.truncate(scope);
        }
        if !finally_body.is_empty() {
            self.inits = inits_try;
            self.uninits = self.uninits_try.clone();
            let exits = std::mem::replace(&mut self.pending, prev);
            self.stmts(finally_body);
            if !self.is_dead() {
                self.uninits.and_set(&uninits_end);
                for mut e in exits {
                    e.inits.or_set(&self.inits);
                    e.uninits.and_set(&self.uninits);
                    self.pending.push(e);
                }
                self.inits.or_set(&inits_end);
            }
        } else {
            self.inits = inits_end;
            self.uninits = uninits_end;
            let exits = std::mem::replace(&mut self.pending, prev);
            self.pending.extend(exits);
        }
        self.uninits_try.and_set(&uninits_try_prev);
        self.uninits_try.and_set(&self.uninits.clone());
    }

    // ----- expressions -------------------------------------------------

    /// An expression whose value is not a condition (javac's `scanExpr`).
    fn expr(&mut self, e: &'a Expr, line: u32) {
        if let Some(s) = self.split_expr(e, line) {
            self.merge(s);
        }
    }

    fn merge(&mut self, mut s: Split) {
        s.t_inits.and_set(&s.f_inits);
        s.t_uninits.and_set(&s.f_uninits);
        self.inits = s.t_inits;
        self.uninits = s.t_uninits;
    }

    /// A condition (javac's `scanCond`): the when-true and when-false
    /// states. A constant condition leaves the branch it rules out
    /// vacuously assigned.
    fn cond(&mut self, e: &'a Expr, line: u32) -> Split {
        if let Some(Const::Bool(b)) = self.const_value(e, 0) {
            let mut all_inits = self.inits.clone();
            all_inits.incl_range(self.firstadr, self.nextadr);
            let mut all_uninits = self.uninits.clone();
            all_uninits.incl_range(self.firstadr, self.nextadr);
            let (live_i, live_u) = (self.inits.clone(), self.uninits.clone());
            return if b {
                Split {
                    t_inits: live_i,
                    t_uninits: live_u,
                    f_inits: all_inits,
                    f_uninits: all_uninits,
                }
            } else {
                Split {
                    t_inits: all_inits,
                    t_uninits: all_uninits,
                    f_inits: live_i,
                    f_uninits: live_u,
                }
            };
        }
        match self.split_expr(e, line) {
            Some(s) => s,
            None => Split {
                t_inits: self.inits.clone(),
                t_uninits: self.uninits.clone(),
                f_inits: self.inits.clone(),
                f_uninits: self.uninits.clone(),
            },
        }
    }

    /// Scan `e`; the split states when it is a boolean operator javac
    /// analyzes as a condition (`&&`, `||`, `!`, a boolean `?:`), else
    /// `None` with the state updated in place.
    fn split_expr(&mut self, e: &'a Expr, line: u32) -> Option<Split> {
        match e {
            Expr::Binary {
                op: BinOp::And,
                lhs,
                rhs,
            } => {
                let l = self.cond(lhs, line);
                self.inits = l.t_inits;
                self.uninits = l.t_uninits;
                let mut r = self.cond(rhs, line);
                r.f_inits.and_set(&l.f_inits);
                r.f_uninits.and_set(&l.f_uninits);
                Some(r)
            }
            Expr::Binary {
                op: BinOp::Or,
                lhs,
                rhs,
            } => {
                let l = self.cond(lhs, line);
                self.inits = l.f_inits;
                self.uninits = l.f_uninits;
                let mut r = self.cond(rhs, line);
                r.t_inits.and_set(&l.t_inits);
                r.t_uninits.and_set(&l.t_uninits);
                Some(r)
            }
            Expr::Unary { op: UnOp::Not, rhs } => {
                let s = self.cond(rhs, line);
                Some(Split {
                    t_inits: s.f_inits,
                    t_uninits: s.f_uninits,
                    f_inits: s.t_inits,
                    f_uninits: s.t_uninits,
                })
            }
            Expr::Ternary { cond, then, els } => {
                let c = self.cond(cond, line);
                self.inits = c.t_inits;
                self.uninits = c.t_uninits;
                if self.is_boolean(then) && self.is_boolean(els) {
                    let t = self.cond(then, line);
                    self.inits = c.f_inits;
                    self.uninits = c.f_uninits;
                    let mut f = self.cond(els, line);
                    f.t_inits.and_set(&t.t_inits);
                    f.f_inits.and_set(&t.f_inits);
                    f.t_uninits.and_set(&t.t_uninits);
                    f.f_uninits.and_set(&t.f_uninits);
                    return Some(f);
                }
                self.expr(then, line);
                let (ti, tu) = (self.inits.clone(), self.uninits.clone());
                self.inits = c.f_inits;
                self.uninits = c.f_uninits;
                self.expr(els, line);
                self.inits.and_set(&ti);
                self.uninits.and_set(&tu);
                None
            }
            _ => {
                self.plain_expr(e, line);
                None
            }
        }
    }

    /// Every other expression form.
    fn plain_expr(&mut self, e: &'a Expr, line: u32) {
        match e {
            Expr::Var(n) => self.name_read(n, line),
            Expr::PostIncDec { name, .. } | Expr::PreIncDec { name, .. } => {
                self.check_assignable_name(name, line);
                self.name_read(name, line);
                self.name_write(name, line);
            }
            Expr::Unary { rhs, .. } => self.expr(rhs, line),
            Expr::Binary { lhs, rhs, .. } => {
                self.expr(lhs, line);
                self.expr(rhs, line);
            }
            Expr::Println { arg, .. } => {
                if let Some(a) = arg {
                    self.expr(a, line);
                }
            }
            Expr::IncDec { target, line, .. } => {
                self.check_assignable_target(target, *line);
                self.expr(target, *line);
                self.target_write(target, *line);
            }
            Expr::Assign {
                target,
                op,
                value,
                line,
            } => match target.as_ref() {
                Expr::Var(name) => {
                    self.check_assignable_name(name, *line);
                    if *op != AssignOp::Assign {
                        self.name_read(name, *line);
                    }
                    self.expr(value, *line);
                    self.name_write(name, *line);
                }
                Expr::Field { recv, name } => self.field_assign(recv, name, *op, value, *line),
                other => {
                    self.expr(other, *line);
                    self.expr(value, *line);
                }
            },
            Expr::Cast { expr, line, .. } => self.expr(expr, *line),
            Expr::Call { name, args, line } if name == crate::parser::ANON_CLASS_CALL => {
                match args.first().and_then(anonymous_body) {
                    Some((params, body)) => self.inner_class_body(params, body, *line),
                    None => {
                        for a in args {
                            self.expr(a, *line);
                        }
                    }
                }
            }
            Expr::Call { args, line, .. } => {
                for a in args {
                    self.expr(a, *line);
                }
            }
            Expr::MethodCall {
                recv, args, line, ..
            } => {
                self.expr(recv, *line);
                for a in args {
                    self.expr(a, *line);
                }
            }
            Expr::NewArray { sizes, .. } => {
                for s in sizes {
                    self.expr(s, line);
                }
            }
            Expr::ArrayLit { elems, .. } => {
                for x in elems {
                    self.expr(x, line);
                }
            }
            Expr::Index { array, index } => {
                self.expr(array, line);
                self.expr(index, line);
            }
            Expr::Field { recv, name } => {
                if matches!(recv.as_ref(), Expr::This) {
                    // `this.x` is checked like a simple name.
                    if let Some(&a) = self.fields.get(name.as_str()) {
                        self.check_init(a, line);
                    }
                } else {
                    self.expr(recv, line);
                }
            }
            Expr::NewObject {
                class,
                args,
                outer,
                line,
                ..
            } => {
                if let Some(o) = outer {
                    self.expr(o, *line);
                }
                for a in args {
                    self.expr(a, *line);
                }
                self.inner_class_creation(class, *line);
            }
            Expr::InstanceOf {
                expr,
                class,
                binding,
                components,
            } => {
                self.expr(expr, line);
                if let Some(b) = binding {
                    let a = self.declare(b as *const _ as usize, b, Kind::Local, false, class);
                    self.init_param(a);
                }
                for c in components {
                    self.pattern(c);
                }
            }
            Expr::Lambda { params, body, line } => self.lambda(params, body, *line),
            Expr::MethodRef { recv, line, .. } => self.expr(recv, *line),
            Expr::SwitchExpr { disc, arms, line } => self.switch_arms(disc, arms, false, *line),
            Expr::ClassLit(_)
            | Expr::Int(_)
            | Expr::Long(_)
            | Expr::Float(_)
            | Expr::Float32(_)
            | Expr::Str(_)
            | Expr::Char(_)
            | Expr::Bool(_)
            | Expr::This
            | Expr::TypePattern { .. }
            | Expr::Ternary { .. } => {}
        }
    }

    /// javac's `visitLambda`: nothing is DU inside a body that may run
    /// later, and its `return`s are its own.
    fn lambda(&mut self, params: &'a [String], body: &'a LambdaBody, line: u32) {
        let prev_uninits = self.uninits.clone();
        let prev_uninits_try = self.uninits_try.clone();
        let prev_inits = self.inits.clone();
        let returnadr = self.returnadr;
        let (nextadr, scope) = (self.nextadr, self.scope.len());
        let prev_pending = std::mem::take(&mut self.pending);
        let prev_jumps = std::mem::take(&mut self.jumps);
        self.uninits.exclude_from(self.firstadr);
        let prev_ret = self.ret.take();
        self.returnadr = self.nextadr;
        self.captures.push((CaptureKind::Lambda, self.nextadr));
        for p in params {
            let a = self.declare(p as *const _ as usize, p, Kind::Param, false, "var");
            self.init_param(a);
        }
        match body {
            LambdaBody::Expr(e) => self.expr(e, line),
            LambdaBody::Block(b) => self.stmts(b),
        }
        self.captures.pop();
        self.returnadr = returnadr;
        self.uninits = prev_uninits;
        self.uninits_try = prev_uninits_try;
        self.inits = prev_inits;
        self.pending = prev_pending;
        self.jumps = prev_jumps;
        self.ret = prev_ret;
        self.nextadr = nextadr;
        self.scope.truncate(scope);
    }

    /// The body of an anonymous class the parser lowered to a lambda (its one
    /// method, or a double-brace initializer). javac analyzes it as a method
    /// of a class nested in this one (`visitClassDef` then `visitMethodDef`):
    /// the enclosing method's variables are below `firstadr` there, so a
    /// `final` one cannot be assigned, and a captured local is judged as one
    /// referenced from an inner class.
    fn inner_class_body(&mut self, params: &'a [String], body: &'a LambdaBody, line: u32) {
        let state = self.enter_method(false);
        let prev_pending = std::mem::take(&mut self.pending);
        let prev_jumps = std::mem::take(&mut self.jumps);
        let prev_ctx = std::mem::replace(&mut self.ctx, Ctx::Method);
        let prev_ret = self.ret.take();
        let prev_init_env = self.init_env.take();
        self.captures.push((CaptureKind::InnerClass, self.nextadr));
        for p in params {
            let a = self.declare(p as *const _ as usize, p, Kind::Param, false, "var");
            self.init_param(a);
        }
        match body {
            LambdaBody::Expr(e) => self.expr(e, line),
            LambdaBody::Block(b) => self.stmts(b),
        }
        self.captures.pop();
        self.exit_method(state);
        self.pending = prev_pending;
        self.jumps = prev_jumps;
        self.ret = prev_ret;
        self.ctx = prev_ctx;
        self.init_env = prev_init_env;
    }

    /// A read of the simple name `name`.
    fn name_read(&mut self, name: &str, line: u32) {
        match self.lookup(name) {
            Some(a) => {
                self.check_init(a, line);
                self.note_capture(a, line, false);
                if self.local(name).is_none() {
                    self.forward_reference(name, line);
                }
            }
            None => {
                self.note_free_capture(name, false);
                self.forward_reference(name, line);
            }
        }
    }

    /// An assignment to the simple name `name`, after its right-hand side.
    fn name_write(&mut self, name: &str, line: u32) {
        match self.lookup(name) {
            Some(a) => {
                self.let_init(a, line);
                self.note_capture(a, line, true);
            }
            None => self.note_free_capture(name, true),
        }
    }

    /// A name that is none of this class's own: when the class captured an
    /// enclosing local of that name, the body refers to that local.
    fn note_free_capture(&mut self, name: &str, write: bool) {
        if name == OUTER_THIS || !self.class.captures.iter().any(|c| c.name == name) {
            return;
        }
        match self.free_captured.iter_mut().find(|(n, _)| n == name) {
            Some(e) => e.1 |= write,
            None => self.free_captured.push((name.to_string(), write)),
        }
    }

    /// `new C(…)` of a local or anonymous class: the enclosing locals its
    /// body names must be definitely assigned here and effectively final.
    fn inner_class_creation(&mut self, class: &str, line: u32) {
        let Some(refs) = self.classes.inner_refs.get(class) else {
            return;
        };
        for (name, write) in refs {
            if let Some(a) = self.local(name) {
                self.check_init(a, line);
                self.capture_refs.push(CaptureRef {
                    decl: self.vars[a],
                    line,
                    write: *write,
                    kind: CaptureKind::InnerClass,
                });
            }
        }
    }

    /// `recv.name op= value`.
    fn field_assign(
        &mut self,
        recv: &'a Expr,
        name: &'a str,
        op: AssignOp,
        value: &'a Expr,
        line: u32,
    ) {
        let target = FieldTarget { recv, name };
        self.check_assignable_field(&target, line);
        let this = matches!(recv, Expr::This);
        if !this {
            self.expr(recv, line);
        }
        if op != AssignOp::Assign && this {
            if let Some(&a) = self.fields.get(name) {
                self.check_init(a, line);
            }
        }
        self.expr(value, line);
        if this {
            if let Some(&a) = self.fields.get(name) {
                self.let_init(a, line);
            }
        }
    }

    /// The store half of `target++` on a non-name lvalue.
    fn target_write(&mut self, target: &'a Expr, line: u32) {
        if let Expr::Field { recv, name } = target {
            if matches!(recv.as_ref(), Expr::This) {
                if let Some(&a) = self.fields.get(name.as_str()) {
                    self.let_init(a, line);
                }
            }
        }
    }

    /// Remember a reference to a local declared outside the innermost
    /// lambda or guard.
    fn note_capture(&mut self, adr: usize, line: u32, write: bool) {
        let Some(&(kind, start)) = self.captures.last() else {
            return;
        };
        let decl = self.vars[adr];
        if adr < start && !self.decls[decl].is_field() {
            self.capture_refs.push(CaptureRef {
                decl,
                line,
                write,
                kind,
            });
        }
    }

    /// JLS 8.3.3: in an initializer, a field of this class read by simple
    /// name before its declaration (`Attr.checkInit`).
    fn forward_reference(&mut self, name: &str, line: u32) {
        let Some((is_static, pos)) = self.init_env else {
            return;
        };
        let cl = self.class;
        let fields = if is_static {
            &cl.static_fields
        } else {
            &cl.fields
        };
        let Some(f) = fields.iter().find(|f| f.name == name) else {
            return;
        };
        if f.init_pos == pos && f.has_init {
            self.error(
                Phase::Attr,
                line,
                "self-reference in initializer".to_string(),
            );
        } else if f.init_pos > pos {
            self.error(Phase::Attr, line, "illegal forward reference".to_string());
        }
    }

    // ----- Attr: assignability of finals ---------------------------------

    fn check_assignable_name(&mut self, name: &str, line: u32) {
        if let Some(a) = self.local(name) {
            let d = self.decl(a);
            if d.has_init && d.final_or_implicit() {
                let msg = if d.kind == Kind::Resource {
                    format!("auto-closeable resource {name} may not be assigned")
                } else {
                    format!("cannot assign a value to final variable {name}")
                };
                self.error(Phase::Attr, line, msg);
            }
            return;
        }
        if let Some((owner, f)) = self.classes.field(self.class, name) {
            self.check_final_field(owner, f, true, line);
        }
    }

    fn check_assignable_target(&mut self, target: &'a Expr, line: u32) {
        match target {
            Expr::Var(n) => self.check_assignable_name(n, line),
            Expr::Field { recv, name } => {
                self.check_assignable_field(&FieldTarget { recv, name }, line)
            }
            _ => {}
        }
    }

    /// `recv.name = …`: through `this` a blank final may be assignable; through
    /// a class name or another reference a final never is.
    fn check_assignable_field(&mut self, t: &FieldTarget<'a>, line: u32) {
        let found = match t.recv {
            Expr::This => self
                .classes
                .field(self.class, t.name)
                .map(|(c, f)| (c, f, true)),
            Expr::Var(v) => {
                let class = match self.local(v) {
                    Some(a) => {
                        let ty = &self.decl(a).ty;
                        self.classes.by_name.get(ty.as_str()).copied()
                    }
                    None => self
                        .classes
                        .field(self.class, v)
                        .and_then(|(_, f)| self.classes.by_name.get(f.ty.as_str()).copied())
                        .or_else(|| self.classes.by_name.get(v.as_str()).copied()),
                };
                class.and_then(|c| {
                    let mut seen = HashSet::new();
                    self.classes
                        .member_field(c, t.name, &mut seen)
                        .map(|(c, f)| (c, f, false))
                })
            }
            _ => None,
        };
        if let Some((owner, f, through_this)) = found {
            self.check_final_field(owner, f, through_this, line);
        }
    }

    /// `Attr.checkAssignable` for a field.
    fn check_final_field(&mut self, owner: &Class, f: &FieldDecl, through_this: bool, line: u32) {
        if !field_is_final(owner, f) {
            return;
        }
        let is_static = field_is_static(owner, f);
        let in_init = match self.ctx {
            Ctx::Ctor | Ctx::InstanceInit => !is_static,
            Ctx::StaticInit => is_static,
            Ctx::Method => false,
        };
        let assignable = !f.has_init && through_this && std::ptr::eq(owner, self.class) && in_init;
        if !assignable {
            let what = if is_static { "static final" } else { "final" };
            self.error(
                Phase::Attr,
                line,
                format!("cannot assign a value to {what} variable {}", f.name),
            );
        }
    }

    // ----- Attr: capture conversion of `? super` element reads ----------

    /// `T x = e;` (or `x = e;`, `return e;`): an element read through a
    /// lower-bounded wildcard (`List<? super Integer> l; l.get(0)`) has a
    /// capture type whose only upper bound is `Object`, which converts to no
    /// other type.
    fn check_capture_conversion(&mut self, value: &Expr, ty: &str, targs: &[String], line: u32) {
        if matches!(ty, "var" | "Object") || !self.reads_captured_element(value) {
            return;
        }
        let target = if targs.is_empty() {
            ty.to_string()
        } else {
            format!("{ty}<{}>", targs.join(","))
        };
        self.error(
            Phase::Attr,
            line,
            format!("incompatible types: CAP#1 cannot be converted to {target}"),
        );
    }

    /// `name = value;` where `name` is a typed local or field.
    fn check_assigned_capture(&mut self, name: &str, value: &Expr, line: u32) {
        let (ty, targs) = match self.local(name) {
            Some(a) => (self.decl(a).ty.clone(), self.decl(a).targs.clone()),
            None => match self.classes.field(self.class, name) {
                Some((_, f)) => (f.ty.clone(), f.targs.clone()),
                None => return,
            },
        };
        self.check_capture_conversion(value, &ty, &targs, line);
    }

    /// Whether `e` is `recv.m(…)` answering the element a `? super` argument
    /// of `recv`'s static type stands for.
    fn reads_captured_element(&self, e: &Expr) -> bool {
        let Expr::MethodCall {
            recv, method, args, ..
        } = e
        else {
            return false;
        };
        let Some((base, slots)) = self.receiver_type(recv) else {
            return false;
        };
        let int_arg = args.first().is_some_and(|a| self.is_int(a));
        element_slot(&base, method, args.len(), int_arg).is_some_and(|s| slots.contains(&s))
    }

    /// The erased declared type of a receiver and its `? super` positions:
    /// a local, parameter or `for` variable, a field (simple, `this.`,
    /// `obj.` or `C.`), or the result of a user method.
    fn receiver_type(&self, e: &Expr) -> Option<(String, Vec<usize>)> {
        let field = |(_, f): (&Class, &FieldDecl)| (f.ty.clone(), f.super_slots.clone());
        match e {
            Expr::Var(n) => match self.local(n) {
                Some(a) => Some((self.decl(a).ty.clone(), self.decl(a).super_slots.clone())),
                None => self.classes.field(self.class, n).map(field),
            },
            Expr::Field { recv, name } => {
                let cl = self.class_of(recv)?;
                let mut seen = HashSet::new();
                self.classes.member_field(cl, name, &mut seen).map(field)
            }
            Expr::MethodCall {
                recv, method, args, ..
            } => {
                let cl = self.class_of(recv)?;
                let m = self.classes.method(cl, method, args.len(), false)?;
                Some((m.ret.clone(), m.ret_super_slots.clone()))
            }
            Expr::Call { name, args, .. } => {
                let m = self.classes.method(self.class, name, args.len(), true)?;
                Some((m.ret.clone(), m.ret_super_slots.clone()))
            }
            _ => None,
        }
    }

    /// The user class an expression's static type names: `this`, a local or
    /// field of a class type, or a class name (for a static member).
    fn class_of(&self, e: &Expr) -> Option<&'a Class> {
        let by_type = |ty: &str| self.classes.by_name.get(ty).copied();
        match e {
            Expr::This => Some(self.class),
            Expr::Var(v) => match self.local(v) {
                Some(a) => by_type(&self.decl(a).ty),
                None => match self.classes.field(self.class, v) {
                    Some((_, f)) => by_type(&f.ty),
                    None => by_type(v),
                },
            },
            _ => None,
        }
    }

    /// Whether `e` is statically of an integral type that `List.remove`
    /// takes as an index (`int` and what promotes to it).
    fn is_int(&self, e: &Expr) -> bool {
        let int_ty = |t: &str| matches!(t, "int" | "short" | "byte" | "char");
        match e {
            Expr::Int(_) | Expr::Char(_) => true,
            Expr::Unary { op, rhs } => *op != UnOp::Not && self.is_int(rhs),
            Expr::Binary { op, lhs, rhs } => {
                matches!(
                    op,
                    BinOp::Add
                        | BinOp::Sub
                        | BinOp::Mul
                        | BinOp::Div
                        | BinOp::Mod
                        | BinOp::BitAnd
                        | BinOp::BitOr
                        | BinOp::BitXor
                        | BinOp::Shl
                        | BinOp::Shr
                        | BinOp::Ushr
                ) && self.is_int(lhs)
                    && self.is_int(rhs)
            }
            Expr::Cast { ty, .. } => int_ty(ty),
            Expr::Var(n) => match self.local(n) {
                Some(a) => int_ty(&self.decl(a).ty),
                None => self
                    .classes
                    .field(self.class, n)
                    .is_some_and(|(_, f)| int_ty(&f.ty)),
            },
            _ => false,
        }
    }

    // ----- constants and types ----------------------------------------

    /// Whether `e` is of primitive type `boolean`, as far as the syntax
    /// says (javac's test on a conditional's branches).
    fn is_boolean(&self, e: &Expr) -> bool {
        match e {
            Expr::Bool(_) | Expr::InstanceOf { .. } => true,
            Expr::Unary { op: UnOp::Not, .. } => true,
            Expr::Binary { op, lhs, rhs } => match op {
                BinOp::Eq
                | BinOp::Ne
                | BinOp::Lt
                | BinOp::Gt
                | BinOp::Le
                | BinOp::Ge
                | BinOp::And
                | BinOp::Or => true,
                BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
                    self.is_boolean(lhs) && self.is_boolean(rhs)
                }
                _ => false,
            },
            Expr::Ternary { then, els, .. } => self.is_boolean(then) && self.is_boolean(els),
            Expr::Var(n) => self.local(n).is_some_and(|a| self.decl(a).ty == "boolean"),
            _ => false,
        }
    }

    /// The value of `e` when it is a constant expression (JLS 15.29). Names
    /// of constant variables count: a `final` local, or a `final` field of
    /// this class, its supertypes or enclosing classes, or of a named class,
    /// initialized with a constant.
    fn const_value(&self, e: &Expr, depth: u32) -> Option<Const> {
        match e {
            Expr::Var(n) => match self.local(n) {
                Some(a) => self.decl(a).constant.clone(),
                None => {
                    let (c, f) = self.classes.field(self.class, n)?;
                    self.field_const(c, f, depth)
                }
            },
            Expr::Field { recv, name } => match recv.as_ref() {
                Expr::Var(c)
                    if self.local(c).is_none() && self.classes.field(self.class, c).is_none() =>
                {
                    let cl = self.classes.by_name.get(c.as_str())?;
                    let mut seen = HashSet::new();
                    let (owner, f) = self.classes.member_field(cl, name, &mut seen)?;
                    if !field_is_static(owner, f) {
                        return None;
                    }
                    self.field_const(owner, f, depth)
                }
                _ => None,
            },
            _ => const_fold(e, &|n| self.const_value(n, depth)),
        }
    }

    fn field_const(&self, c: &Class, f: &FieldDecl, depth: u32) -> Option<Const> {
        if depth > 16 || !field_is_final(c, f) || !f.has_init || !is_constant_type(&f.ty) {
            return None;
        }
        let init = if field_is_static(c, f) {
            &c.static_init
        } else {
            &c.inst_init
        };
        let value = match &init.get(f.init_pos)?.kind {
            StmtKind::Assign { value, .. } | StmtKind::FieldAssign { value, .. } => value,
            _ => return None,
        };
        // The initializer is evaluated in its own class, with no locals.
        let inner = Flow::new(self.classes, self.classes.by_name.get(c.name.as_str())?);
        inner.const_value(value, depth + 1)?.convert(&f.ty)
    }
}

/// Which type argument the result of `recv.method(…)` (with `argc`
/// arguments) is typed by, for the JDK collection, map, iterator and
/// `Optional` types: the element (`0`), or a map's value (`1`) or key (`0`).
/// `None` for a method answering anything else — `List.remove(Object)` and
/// `Set.remove` answer a `boolean`, `Map.replace(k, old, new)` too.
fn element_slot(base: &str, method: &str, argc: usize, int_arg: bool) -> Option<usize> {
    const LIST: &[&str] = &["List", "ArrayList", "LinkedList", "Vector", "Stack"];
    const QUEUE: &[&str] = &[
        "Queue",
        "Deque",
        "ArrayDeque",
        "LinkedList",
        "PriorityQueue",
    ];
    const SEQUENCED: &[&str] = &[
        "SequencedCollection",
        "SequencedSet",
        "LinkedHashSet",
        "Deque",
        "ArrayDeque",
        "LinkedList",
        "List",
        "ArrayList",
        "Vector",
        "Stack",
    ];
    const SORTED_SET: &[&str] = &["SortedSet", "NavigableSet", "TreeSet"];
    const MAP: &[&str] = &[
        "Map",
        "HashMap",
        "LinkedHashMap",
        "TreeMap",
        "SortedMap",
        "NavigableMap",
        "SequencedMap",
        "Hashtable",
    ];
    const SORTED_MAP: &[&str] = &["SortedMap", "NavigableMap", "TreeMap"];
    let is = |family: &[&str]| family.contains(&base);
    let element = match (method, argc) {
        ("get", 1) | ("set", 2) if is(LIST) => true,
        ("remove", 1) if is(LIST) && int_arg => true,
        ("getFirst" | "getLast" | "removeFirst" | "removeLast", 0) if is(SEQUENCED) => true,
        ("peek" | "poll" | "element" | "remove" | "pop", 0) if is(QUEUE) => true,
        ("peekFirst" | "peekLast" | "pollFirst" | "pollLast", 0) if is(QUEUE) => true,
        ("peek" | "pop", 0) | ("push", 1) if base == "Stack" => true,
        ("first" | "last" | "pollFirst" | "pollLast", 0) if is(SORTED_SET) => true,
        ("floor" | "ceiling" | "higher" | "lower", 1) if is(SORTED_SET) => true,
        ("next", 0) if matches!(base, "Iterator" | "ListIterator") => true,
        ("previous", 0) if base == "ListIterator" => true,
        ("get" | "orElseThrow", 0) | ("orElse", 1) if base == "Optional" => true,
        _ => false,
    };
    if element {
        return Some(0);
    }
    match (method, argc) {
        ("get" | "remove", 1) | ("put" | "putIfAbsent" | "replace" | "getOrDefault", 2)
            if is(MAP) =>
        {
            Some(1)
        }
        ("firstKey" | "lastKey", 0) | ("floorKey" | "ceilingKey" | "higherKey" | "lowerKey", 1)
            if is(SORTED_MAP) =>
        {
            Some(0)
        }
        _ => None,
    }
}

/// The source line of the field `name` in `cl`.
fn field_line(cl: &Class, name: &str) -> u32 {
    cl.static_fields
        .iter()
        .chain(&cl.fields)
        .find(|f| f.name == name)
        .map_or(cl.line, |f| f.line)
}

/// The method body an anonymous class was lowered to: the lambda itself,
/// or the `(Supplier) () -> { … }` a double-brace initializer becomes.
fn anonymous_body(e: &Expr) -> Option<(&[String], &LambdaBody)> {
    let lambda = match e {
        Expr::MethodCall { recv, .. } => match recv.as_ref() {
            Expr::Cast { expr, .. } => expr.as_ref(),
            _ => return None,
        },
        other => other,
    };
    match lambda {
        Expr::Lambda { params, body, .. } => Some((params, body)),
        _ => None,
    }
}

fn is_null(e: &Expr) -> bool {
    matches!(e, Expr::Var(n) if n == "null")
}

/// Whether a variable of type `ty` can be a constant variable: a primitive,
/// `String`, or a `var` (whose type is its constant initializer's).
fn is_constant_type(ty: &str) -> bool {
    matches!(
        ty,
        "int"
            | "long"
            | "short"
            | "byte"
            | "char"
            | "float"
            | "double"
            | "boolean"
            | "String"
            | "var"
    )
}

struct MethodState {
    inits: Bits,
    uninits: Bits,
    nextadr: usize,
    firstadr: usize,
    returnadr: usize,
    initial_ctor: bool,
    scope: usize,
}

struct FieldTarget<'a> {
    recv: &'a Expr,
    name: &'a str,
}

#[derive(Clone, Copy, PartialEq)]
enum SwitchForm {
    Statement,
    Expression,
}

enum CaseBody<'a> {
    /// A classic group's statements, falling through.
    Stmts(&'a [Stmt]),
    /// `case … -> expr;`
    RuleExpr(&'a Expr),
    /// `case … -> { … }` (or a colon-form arm of a switch expression).
    RuleBlock(&'a [Stmt]),
}

struct Case<'a> {
    labels: &'a [Expr],
    guard: Option<&'a Expr>,
    body: CaseBody<'a>,
    is_default: bool,
}

/// A compile-time constant value (JLS 15.29), typed as Java types it.
#[derive(Clone, Debug, PartialEq)]
enum Const {
    Bool(bool),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    Char(u16),
    Str(String),
}

impl Const {
    /// Assignment conversion of a constant to a variable of type `ty`
    /// (`final long L = 1;` holds a `long`).
    fn convert(self, ty: &str) -> Option<Const> {
        Some(match (ty, self) {
            ("var", c) => c,
            ("boolean", c @ Const::Bool(_)) | ("String", c @ Const::Str(_)) => c,
            ("int", c) => Const::Int(c.as_i64()? as i32),
            ("short", c) => Const::Int(c.as_i64()? as i16 as i32),
            ("byte", c) => Const::Int(c.as_i64()? as i8 as i32),
            ("char", c) => Const::Char(c.as_i64()? as u16),
            ("long", c) => Const::Long(c.as_i64()?),
            ("float", c) => Const::Float(c.as_f64()? as f32),
            ("double", c) => Const::Double(c.as_f64()?),
            _ => return None,
        })
    }

    fn as_i64(&self) -> Option<i64> {
        match *self {
            Const::Int(v) => Some(v.into()),
            Const::Long(v) => Some(v),
            Const::Char(v) => Some(v.into()),
            _ => None,
        }
    }

    fn as_f64(&self) -> Option<f64> {
        match *self {
            Const::Float(v) => Some(v.into()),
            Const::Double(v) => Some(v),
            _ => self.as_i64().map(|v| v as f64),
        }
    }

    /// The text string concatenation gives the constant, where javars can
    /// produce it; a floating constant's is left out.
    fn text(&self) -> Option<String> {
        Some(match self {
            Const::Bool(b) => b.to_string(),
            Const::Int(v) => v.to_string(),
            Const::Long(v) => v.to_string(),
            Const::Char(c) => String::from_utf16_lossy(&[*c]),
            Const::Str(s) => s.clone(),
            Const::Float(_) | Const::Double(_) => return None,
        })
    }
}

/// Binary numeric promotion's result type (JLS 5.6).
#[derive(Clone, Copy, PartialEq, PartialOrd)]
enum Num {
    Int,
    Long,
    Float,
    Double,
}

fn num_kind(c: &Const) -> Option<Num> {
    Some(match c {
        Const::Int(_) | Const::Char(_) => Num::Int,
        Const::Long(_) => Num::Long,
        Const::Float(_) => Num::Float,
        Const::Double(_) => Num::Double,
        _ => return None,
    })
}

/// Fold the operator forms of a constant expression; `name` evaluates the
/// names and qualified names in it.
fn const_fold(e: &Expr, name: &dyn Fn(&Expr) -> Option<Const>) -> Option<Const> {
    let fold = |x: &Expr| match x {
        Expr::Var(_) | Expr::Field { .. } => name(x),
        _ => const_fold(x, name),
    };
    Some(match e {
        Expr::Bool(b) => Const::Bool(*b),
        Expr::Int(v) => Const::Int(*v as i32),
        Expr::Long(v) => Const::Long(*v),
        Expr::Float(v) => Const::Double(*v),
        Expr::Float32(v) => Const::Float(*v as f32),
        Expr::Char(c) => Const::Char(crate::jstr::char_value(*c) as u16),
        Expr::Str(s) => Const::Str(s.clone()),
        Expr::Var(_) | Expr::Field { .. } => return name(e),
        Expr::Unary { op, rhs } => {
            let v = fold(rhs)?;
            match (op, v) {
                (UnOp::Not, Const::Bool(b)) => Const::Bool(!b),
                (UnOp::Plus, v) => promote_unary(v)?,
                (UnOp::Neg, v) => match promote_unary(v)? {
                    Const::Int(i) => Const::Int(i.wrapping_neg()),
                    Const::Long(i) => Const::Long(i.wrapping_neg()),
                    Const::Float(f) => Const::Float(-f),
                    Const::Double(f) => Const::Double(-f),
                    _ => return None,
                },
                (UnOp::BitNot, v) => match promote_unary(v)? {
                    Const::Int(i) => Const::Int(!i),
                    Const::Long(i) => Const::Long(!i),
                    _ => return None,
                },
                _ => return None,
            }
        }
        Expr::Binary { op, lhs, rhs } => binary_const(*op, fold(lhs)?, fold(rhs)?)?,
        Expr::Ternary { cond, then, els } => match fold(cond)? {
            Const::Bool(b) => {
                let (t, f) = (fold(then)?, fold(els)?);
                let pick = if b { t.clone() } else { f.clone() };
                match (num_kind(&t), num_kind(&f)) {
                    (Some(a), Some(b2)) if a != b2 => promote(pick, if a > b2 { a } else { b2 })?,
                    _ => pick,
                }
            }
            _ => return None,
        },
        Expr::Cast { ty, expr, .. } => fold(expr)?.convert(ty)?,
        _ => return None,
    })
}

fn promote_unary(c: Const) -> Option<Const> {
    let k = num_kind(&c)?;
    promote(c, k)
}

fn promote(c: Const, to: Num) -> Option<Const> {
    Some(match to {
        Num::Int => Const::Int(c.as_i64()? as i32),
        Num::Long => Const::Long(c.as_i64()?),
        Num::Float => Const::Float(c.as_f64()? as f32),
        Num::Double => Const::Double(c.as_f64()?),
    })
}

fn binary_const(op: BinOp, l: Const, r: Const) -> Option<Const> {
    use BinOp::*;
    if op == Add && (matches!(l, Const::Str(_)) || matches!(r, Const::Str(_))) {
        return Some(Const::Str(l.text()? + &r.text()?));
    }
    if let (Const::Bool(a), Const::Bool(b)) = (&l, &r) {
        let (a, b) = (*a, *b);
        return Some(Const::Bool(match op {
            And | BitAnd => a && b,
            Or | BitOr => a || b,
            BitXor | Ne => a != b,
            Eq => a == b,
            _ => return None,
        }));
    }
    if let (Const::Str(a), Const::Str(b)) = (&l, &r) {
        return match op {
            Eq => Some(Const::Bool(a == b)),
            Ne => Some(Const::Bool(a != b)),
            _ => None,
        };
    }
    if matches!(op, Shl | Shr | Ushr) {
        let n = r.as_i64()?;
        return Some(match promote_unary(l)? {
            Const::Int(v) => {
                let s = (n & 31) as u32;
                Const::Int(match op {
                    Shl => v.wrapping_shl(s),
                    Shr => v >> s,
                    _ => ((v as u32) >> s) as i32,
                })
            }
            Const::Long(v) => {
                let s = (n & 63) as u32;
                Const::Long(match op {
                    Shl => v.wrapping_shl(s),
                    Shr => v >> s,
                    _ => ((v as u64) >> s) as i64,
                })
            }
            _ => return None,
        });
    }
    let k = num_kind(&l)?.max_kind(num_kind(&r)?);
    let (l, r) = (promote(l, k)?, promote(r, k)?);
    let cmp = |o: std::cmp::Ordering| -> Option<Const> {
        use std::cmp::Ordering::*;
        Some(Const::Bool(match op {
            Eq => o == Equal,
            Ne => o != Equal,
            Lt => o == Less,
            Gt => o == Greater,
            Le => o != Greater,
            Ge => o != Less,
            _ => return None,
        }))
    };
    match (l, r) {
        (Const::Int(a), Const::Int(b)) => Some(match op {
            Add => Const::Int(a.wrapping_add(b)),
            Sub => Const::Int(a.wrapping_sub(b)),
            Mul => Const::Int(a.wrapping_mul(b)),
            Div if b != 0 => Const::Int(a.wrapping_div(b)),
            Mod if b != 0 => Const::Int(a.wrapping_rem(b)),
            BitAnd => Const::Int(a & b),
            BitOr => Const::Int(a | b),
            BitXor => Const::Int(a ^ b),
            Div | Mod => return None,
            _ => return cmp(a.cmp(&b)),
        }),
        (Const::Long(a), Const::Long(b)) => Some(match op {
            Add => Const::Long(a.wrapping_add(b)),
            Sub => Const::Long(a.wrapping_sub(b)),
            Mul => Const::Long(a.wrapping_mul(b)),
            Div if b != 0 => Const::Long(a.wrapping_div(b)),
            Mod if b != 0 => Const::Long(a.wrapping_rem(b)),
            BitAnd => Const::Long(a & b),
            BitOr => Const::Long(a | b),
            BitXor => Const::Long(a ^ b),
            Div | Mod => return None,
            _ => return cmp(a.cmp(&b)),
        }),
        (Const::Float(a), Const::Float(b)) => float_op(op, a.into(), b.into()).map(|c| match c {
            Const::Double(v) => Const::Float(v as f32),
            other => other,
        }),
        (Const::Double(a), Const::Double(b)) => float_op(op, a, b),
        _ => None,
    }
}

/// A floating operation; a comparison with NaN is false except `!=`.
fn float_op(op: BinOp, a: f64, b: f64) -> Option<Const> {
    use BinOp::*;
    Some(match op {
        Add => Const::Double(a + b),
        Sub => Const::Double(a - b),
        Mul => Const::Double(a * b),
        Div => Const::Double(a / b),
        Mod => Const::Double(a % b),
        Eq => Const::Bool(a == b),
        Ne => Const::Bool(a != b),
        Lt => Const::Bool(a < b),
        Gt => Const::Bool(a > b),
        Le => Const::Bool(a <= b),
        Ge => Const::Bool(a >= b),
        _ => return None,
    })
}

impl Num {
    fn max_kind(self, o: Num) -> Num {
        if self > o {
            self
        } else {
            o
        }
    }
}
