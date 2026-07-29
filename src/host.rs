//! The javars host: builtin registration, Java value formatting, and the strict
//! numeric hook.
//!
//! javars keeps no object heap of its own yet (slice 1 runs on the fusevm value
//! model directly). Two places need Java semantics that fusevm's default
//! awk/shell flavour does not provide:
//!
//! 1. **Printing.** fusevm's native `PrintLn` renders values shell-style
//!    (`true`→`1`, `3.0`→`3`). `System.out.print[ln]` instead lowers to a
//!    registered builtin ([`JPRINTLN`]/[`JPRINT`]) that formats through
//!    [`java_str`] — `true`/`false`, `3.0`, `null` — matching `java`.
//! 2. **`+` overloading.** Java's `+` is string concatenation when either
//!    operand is a `String`. fusevm runs *strict* once a numeric hook is
//!    installed, delegating any operation with a non-numeric operand to
//!    [`numeric_hook`], where `+` concatenates via the same [`java_str`].

use fusevm::{NumOp, Value, VM};
use std::cell::RefCell;
use std::collections::HashMap;

/// Builtin id for `System.out.println` (one Java-formatted arg + newline).
pub const JPRINTLN: u16 = 700;
/// Builtin id for `System.out.print` (one Java-formatted arg, no newline).
pub const JPRINT: u16 = 701;
/// Builtin id for the `--dap` per-statement line marker. Emitted only by
/// [`crate::compiler::compile_debug`]; a normal run never carries it.
pub const DBG_LINE: u16 = 702;
/// Builtin id for compiling + registering an inline `rust { ... }` FFI block.
/// The desugar target `__rust_compile("<base64>", line)` lowers to a call of
/// this builtin: it pops the base64 block body and hands it to
/// `fusevm::ffi::compile_and_register`. Returns `null` (Java `Unit`).
pub const JFFI_COMPILE: u16 = 703;
/// Builtin id for calling an FFI-exported function by name. The stack holds the
/// arguments (deepest first) with the function name (a `Str`) on top; `argc` is
/// the total stack items (`args + 1`). Dispatches through `fusevm::ffi::try_call`
/// and returns the result.
pub const JFFI_CALL: u16 = 704;
/// Builtin id for an instance method call on a `String` receiver. The stack
/// holds `[recv, arg0, …, argN, methodName]` (the method name — a `Str` — on
/// top); `argc` counts every one of those items (`recv + args + name`).
/// Dispatches through `b_str_dispatch` to the `java.lang.String` method of
/// that name, returning its result.
pub const JSTR_DISPATCH: u16 = 705;
/// Builtin id for `System.err.println` (one Java-formatted arg + newline, on
/// stderr).
pub const JEPRINTLN: u16 = 706;
/// Builtin id for `System.err.print` (one Java-formatted arg, no newline, on
/// stderr).
pub const JEPRINT: u16 = 707;
/// Builtin id for a static stdlib method call (`Math.*`, `Integer.*`,
/// `String.valueOf`, …). The stack holds `[arg0, …, argN, className,
/// methodName]` (the method name — a `Str` — on top, the class name below it);
/// `argc` counts the args plus those two names. Dispatches through
/// `b_static_dispatch` to `static_method`, returning its result.
pub const JSTATIC_DISPATCH: u16 = 708;

// ── Host object heap builtins (reference arrays + class instances) ──
// `Value::Obj(u32)` is an opaque handle into [`HEAP`]; these builtins are the
// only code that dereferences it. Aliasing is by reference: passing an `Obj`
// copies the u32 handle, and a mutating builtin edits the shared heap object.

/// `new T[n]` — allocate a default-valued array. Stack `[size, default]`
/// (`default` on top); `argc == 2`. Pushes the array `Obj` handle.
pub const JARRAY_NEW: u16 = 709;
/// `{a, b, …}` array literal — pop `argc` element values (deepest first) and
/// push a fresh array `Obj`.
pub const JARRAY_LIT: u16 = 710;
/// `a[i]` element read. Stack `[array, index]` (`index` on top); `argc == 2`.
/// Pushes the element; an out-of-range index faults (`ArrayIndexOutOfBounds`).
pub const JARRAY_GET: u16 = 711;
/// `a[i] = v` element write. Stack `[array, index, value]` (`value` on top);
/// `argc == 3`. Mutates the heap array; returns `value`.
pub const JARRAY_SET: u16 = 712;
/// `new C(...)` instance allocation. Stack `[className]`; `argc == 1`. Pushes a
/// fresh instance `Obj` with an empty field map (the compiler emits field-init
/// and constructor calls after).
pub const JNEW: u16 = 713;
/// `recv.field` read — an array's `.length` or an instance field. Stack
/// `[recv, name]` (`name` on top); `argc == 2`. Pushes the value (`null`/`Undef`
/// for an absent instance field).
pub const JFIELD_GET: u16 = 714;
/// `recv.field = v` write. Stack `[recv, name, value]` (`value` on top);
/// `argc == 3`. Mutates the heap instance; returns `value`.
pub const JFIELD_SET: u16 = 715;
/// `x instanceof C` — stack `[obj, className]` (`className` on top);
/// `argc == 2`. Pushes a `Bool`: true when `obj` is a non-null instance whose
/// class is `C` or a subclass. Subclass links are resolved through `SUPERS`.
pub const JINSTANCEOF: u16 = 716;
/// Runtime class name of an instance. Stack `[obj]`; `argc == 1`. Pushes the
/// instance's class name as a `Str` (empty for a non-instance). Drives the
/// compiler's virtual method-dispatch chain.
pub const JCLASSOF: u16 = 717;
/// `new T[s0][s1]…` — allocate a rectangular multi-dimensional array. Stack
/// `[s0, s1, …, sK, leafDefault]` (`leafDefault` on top); `argc == K + 2`.
/// Builds `K+1` nested levels of default-valued arrays and pushes the outer
/// handle. Aliasing is by reference like any other array.
pub const JARRAY_NEW_MULTI: u16 = 718;

/// Builtin id for Java floating-point division. Java `/` on a floating operand
/// follows IEEE-754 — `x / 0.0` is a signed infinity and `0.0 / 0.0` is NaN,
/// never a fault — whereas fusevm's native `Op::Div` yields `Undef` for a zero
/// divisor (its shell/awk flavour has no infinities). Statically-integral
/// division keeps the native op so the JIT can still trace it; only the
/// floating path routes through here.
pub const JDIV: u16 = 719;

// ── Exception builtins (`throw` / `try` / `catch` / `finally`) ──
// fusevm has no unwind opcode, so javars models the in-flight exception as a
// host-side pending value plus a compiler-emitted check after every `Op::Call`.
// A `throw` parks the throwable in [`PENDING`] and the compiler jumps to the
// innermost handler in the current frame — or, when there is none, returns out
// of the frame so the caller's post-call check sees the pending value and
// repeats. That is the same "bubble the flag at every call site" contract the
// sibling frontends use; only the unwind step differs, because javars's calls
// are real fusevm call frames rather than nested VMs.

/// `throw e` — stack `[throwable]`; `argc == 1`. Parks the value as the pending
/// exception and returns `null`. The compiler emits the jump to the handler (or
/// the frame exit) immediately after.
pub const JTHROW: u16 = 720;
/// Is an exception in flight? `argc == 0`; pushes a `Bool`. Emitted after every
/// `Op::Call` in a program that uses exceptions.
pub const JEXC_PENDING: u16 = 721;
/// Take the pending exception (clearing it). `argc == 0`; pushes the throwable
/// (or `null` when none). Emitted at the top of a handler.
pub const JEXC_TAKE: u16 = 722;
/// The current value-stack depth. `argc == 0`; pushes an `Int`. Recorded on
/// entry to a `try` so the handler can discard the operands of the expression
/// the throw abandoned.
pub const JEXC_DEPTH: u16 = 723;
/// Truncate the value stack to a depth recorded by [`JEXC_DEPTH`]. Stack
/// `[depth]`; `argc == 1`.
pub const JEXC_CUT: u16 = 724;
/// Report an uncaught exception and halt. `argc == 0`. Formats Java's
/// `Exception in thread "main" <qualified class>: <message>` line and faults, so
/// the process exits non-zero the way `java` does.
pub const JEXC_ABORT: u16 = 725;
/// Raise a runtime fault the compiler detected inline (`int / 0`). Stack
/// `[className, message]`; `argc == 2`. Goes through the same [`raise`] path a
/// host-detected fault does, so the throwable is catchable.
pub const JFAULT: u16 = 726;

/// One object on the host-owned Java heap. `Value::Obj(id)` indexes [`HEAP`].
enum HostObj {
    /// A Java reference array (`int[]`, `String[]`, `Point[]`, …). Element type
    /// is erased at runtime — the compiler sets each slot's default on creation.
    Array(Vec<Value>),
    /// A class instance: its runtime class name and its instance fields.
    Instance {
        class: String,
        fields: HashMap<String, Value>,
    },
}

thread_local! {
    /// The host-owned Java object heap. `Value::Obj(id)` is an index into this
    /// slab; the frontend owns the objects, fusevm just carries the handle. Grows
    /// per run and is cleared by [`heap_reset`] at the start of every program so
    /// handles never leak across runs.
    static HEAP: RefCell<Vec<HostObj>> = const { RefCell::new(Vec::new()) };
    /// Type → its direct supertypes (superclass + implemented/extended
    /// interfaces), populated by [`set_supertypes`] before a run. Used by
    /// `instanceof` and default `toString` to walk the supertype graph.
    static SUPERS: RefCell<HashMap<String, Vec<String>>> = RefCell::new(HashMap::new());
    /// The exception in flight, if any. Set by [`JTHROW`], cleared by
    /// [`JEXC_TAKE`] when a handler claims it. Lives here rather than on the
    /// value stack because it has to survive the `Op::ReturnValue` that unwinds
    /// each frame between the `throw` and its handler.
    static PENDING: RefCell<Option<Value>> = const { RefCell::new(None) };
    /// True when the running program was compiled with the exception machinery
    /// (`Program::uses_exceptions`) — i.e. its call and fault sites carry the
    /// pending-exception check, so a raised throwable will actually be seen.
    /// When false there is no handler anywhere and no check to observe the
    /// pending value, so a runtime fault aborts instead (see [`raise`]).
    static EXC_ENABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Clear the object heap (and superclass table stays until reset). Called at the
/// start of each program run so a fresh program never sees a prior run's handles.
pub fn heap_reset() {
    HEAP.with(|h| h.borrow_mut().clear());
    SUPERS.with(|s| s.borrow_mut().clear());
    PENDING.with(|p| *p.borrow_mut() = None);
    EXC_ENABLED.with(|e| e.set(false));
}

/// Tell the host whether the compiled program carries the exception machinery.
/// Call before running the chunk; drives whether a runtime fault becomes a
/// catchable throwable or an immediate abort.
pub fn set_exceptions_enabled(on: bool) {
    EXC_ENABLED.with(|e| e.set(on));
}

/// A Java-level fault a host builtin detected: the `java.lang` throwable class
/// to raise and its `detailMessage`. An empty `class` marks a javars *internal*
/// error (an unimplemented method, a malformed format string) — those are not
/// Java exceptions and are never catchable.
struct Fault {
    class: &'static str,
    msg: String,
}

impl Fault {
    /// A catchable Java throwable of class `class` carrying `msg`.
    fn java(class: &'static str, msg: impl Into<String>) -> Self {
        Fault {
            class,
            msg: msg.into(),
        }
    }

    /// A javars internal error — reported as `javars: <msg>`, never catchable.
    fn internal(msg: impl Into<String>) -> Self {
        Fault {
            class: "",
            msg: msg.into(),
        }
    }
}

/// Raise `f` from a builtin.
///
/// With the exception machinery compiled in, a Java fault becomes the pending
/// exception — indistinguishable from a `throw`, so `catch`/`finally` see it and
/// `getMessage()` works. Without it (a program that never mentions `try` or
/// `throw`, whose call sites carry no check) nothing would ever observe the
/// pending value, so the fault aborts the run with the same
/// `Exception in thread "main" …` line the uncaught path prints. An internal
/// error always aborts.
fn raise(vm: &mut VM, f: Fault) -> Value {
    if f.class.is_empty() {
        ffi_fault(vm, f.msg);
        return Value::Undef;
    }
    if !EXC_ENABLED.with(|e| e.get()) {
        ffi_fault(
            vm,
            format!(
                "Exception in thread \"main\" java.lang.{}: {}",
                f.class, f.msg
            ),
        );
        return Value::Undef;
    }
    let mut fields = HashMap::new();
    fields.insert("detailMessage".to_string(), Value::str(f.msg));
    let id = heap_alloc(HostObj::Instance {
        class: f.class.to_string(),
        fields,
    });
    PENDING.with(|p| *p.borrow_mut() = Some(Value::Obj(id)));
    Value::Undef
}

/// Install the type → direct-supertypes map for the current program (used by
/// `instanceof` and default `toString`). Call before running the chunk.
pub fn set_supertypes(map: HashMap<String, Vec<String>>) {
    SUPERS.with(|s| *s.borrow_mut() = map);
}

/// Allocate `obj` on the heap and return its handle.
fn heap_alloc(obj: HostObj) -> u32 {
    HEAP.with(|h| {
        let mut h = h.borrow_mut();
        let id = h.len() as u32;
        h.push(obj);
        id
    })
}

/// True when `class` is `target`, a (transitive) subclass of it, or a type that
/// implements/extends the interface `target` — walking the supertype graph
/// (superclass + interfaces).
fn is_subclass_of(class: &str, target: &str) -> bool {
    if class == target {
        return true;
    }
    SUPERS.with(|s| {
        let s = s.borrow();
        let mut stack = vec![class.to_string()];
        let mut seen = std::collections::HashSet::new();
        while let Some(cur) = stack.pop() {
            if cur == target {
                return true;
            }
            if !seen.insert(cur.clone()) {
                continue;
            }
            if let Some(sups) = s.get(&cur) {
                stack.extend(sups.iter().cloned());
            }
        }
        false
    })
}

thread_local! {
    /// Set by an inline-Rust FFI fault (compile error, call error, or an
    /// unresolved export). A builtin cannot return a `Result`, so it stashes the
    /// message here and halts the VM; [`crate::run_chunk`] reads it after
    /// `VM::run` returns and surfaces it as a `javars:` error.
    static FFI_ERROR: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Take and clear any pending FFI-fault message.
pub fn take_ffi_error() -> Option<String> {
    FFI_ERROR.with(|e| e.borrow_mut().take())
}

/// Record an FFI fault and halt the VM; the message surfaces after the run.
fn ffi_fault(vm: &mut VM, msg: impl Into<String>) {
    FFI_ERROR.with(|e| *e.borrow_mut() = Some(msg.into()));
    vm.request_halt();
}

/// Install javars builtins on a VM: the Java-formatting print builtins and the
/// inline-Rust FFI compile/call builtins. This is the single install choke point
/// later waves (methods, `String`/array objects) grow into.
pub fn install(vm: &mut VM) {
    vm.register_builtin(JPRINTLN, b_println);
    vm.register_builtin(JPRINT, b_print);
    vm.register_builtin(JEPRINTLN, b_eprintln);
    vm.register_builtin(JEPRINT, b_eprint);
    vm.register_builtin(JFFI_COMPILE, b_ffi_compile);
    vm.register_builtin(JFFI_CALL, b_ffi_call);
    vm.register_builtin(JSTR_DISPATCH, b_str_dispatch);
    vm.register_builtin(JSTATIC_DISPATCH, b_static_dispatch);
    vm.register_builtin(JARRAY_NEW, b_array_new);
    vm.register_builtin(JARRAY_NEW_MULTI, b_array_new_multi);
    vm.register_builtin(JARRAY_LIT, b_array_lit);
    vm.register_builtin(JARRAY_GET, b_array_get);
    vm.register_builtin(JARRAY_SET, b_array_set);
    vm.register_builtin(JNEW, b_new);
    vm.register_builtin(JFIELD_GET, b_field_get);
    vm.register_builtin(JFIELD_SET, b_field_set);
    vm.register_builtin(JINSTANCEOF, b_instanceof);
    vm.register_builtin(JCLASSOF, b_classof);
    vm.register_builtin(JDIV, b_div);
    vm.register_builtin(JTHROW, b_throw);
    vm.register_builtin(JEXC_PENDING, b_exc_pending);
    vm.register_builtin(JEXC_TAKE, b_exc_take);
    vm.register_builtin(JEXC_DEPTH, b_exc_depth);
    vm.register_builtin(JEXC_CUT, b_exc_cut);
    vm.register_builtin(JEXC_ABORT, b_exc_abort);
    vm.register_builtin(JFAULT, b_fault);
}

/// Raise a compiler-detected runtime fault (stack `[className, message]`) — the
/// integer division-by-zero check emits this.
fn b_fault(vm: &mut VM, argc: u8) -> Value {
    let args = pop_args(vm, argc);
    let class = args
        .first()
        .map(|v| v.as_str_cow().into_owned())
        .unwrap_or_default();
    let msg = args
        .get(1)
        .map(|v| v.as_str_cow().into_owned())
        .unwrap_or_default();
    // Only the modeled `java.lang` throwables are raisable this way, and the
    // compiler only ever emits one of them; look the name up so `class` can stay
    // a `&'static str` in [`Fault`].
    match crate::prelude::THROWABLES.iter().find(|(n, _)| *n == class) {
        Some((n, _)) => raise(vm, Fault::java(n, msg)),
        None => raise(
            vm,
            Fault::internal(format!("javars: unknown fault `{class}`")),
        ),
    }
}

/// `throw e` — park the throwable as the pending exception.
fn b_throw(vm: &mut VM, argc: u8) -> Value {
    let args = pop_args(vm, argc);
    let exc = args.into_iter().next().unwrap_or(Value::Undef);
    PENDING.with(|p| *p.borrow_mut() = Some(exc));
    Value::Undef
}

/// True while an exception is in flight (the post-call check).
fn b_exc_pending(vm: &mut VM, argc: u8) -> Value {
    pop_args(vm, argc);
    Value::bool(PENDING.with(|p| p.borrow().is_some()))
}

/// Claim the pending exception for a handler, clearing it.
fn b_exc_take(vm: &mut VM, argc: u8) -> Value {
    pop_args(vm, argc);
    PENDING
        .with(|p| p.borrow_mut().take())
        .unwrap_or(Value::Undef)
}

/// The value-stack depth at `try` entry.
fn b_exc_depth(vm: &mut VM, argc: u8) -> Value {
    pop_args(vm, argc);
    Value::Int(vm.stack.len() as i64)
}

/// Discard everything the abandoned expression left on the value stack, back to
/// the depth [`JEXC_DEPTH`] recorded at `try` entry. Frames between the `throw`
/// and the handler clean themselves (`Op::ReturnValue` truncates to the frame
/// base), but the operands of the half-evaluated expression *inside* the
/// handler's own frame would otherwise pile up — once per throw, forever, in a
/// loop.
fn b_exc_cut(vm: &mut VM, argc: u8) -> Value {
    let args = pop_args(vm, argc);
    let depth = args.first().map(|v| v.to_int()).unwrap_or(0).max(0) as usize;
    if depth <= vm.stack.len() {
        vm.stack.truncate(depth);
    }
    Value::Undef
}

/// An exception that reached the top of `main`. Reports it the way `java` does
/// — `Exception in thread "main" java.lang.Foo: message` — and faults, so the
/// process exits non-zero.
fn b_exc_abort(vm: &mut VM, argc: u8) -> Value {
    pop_args(vm, argc);
    let exc = PENDING
        .with(|p| p.borrow_mut().take())
        .unwrap_or(Value::Undef);
    let msg = format!("Exception in thread \"main\" {}", throwable_str(&exc));
    ffi_fault(vm, msg);
    Value::Undef
}

/// Render a throwable the way `Throwable.toString()` does, from the heap object
/// directly. The Java-level `toString()` override cannot be called from here (a
/// builtin cannot re-enter the VM), and this path only serves the uncaught
/// report, so it reproduces the same text: the class name — qualified with
/// `java.lang.` for the modeled JDK throwables, bare for a user class — plus
/// `": " + detailMessage` when a message was supplied.
fn throwable_str(v: &Value) -> String {
    let Value::Obj(id) = v else {
        return java_str(v);
    };
    HEAP.with(|h| {
        let h = h.borrow();
        match h.get(*id as usize) {
            Some(HostObj::Instance { class, fields }) => {
                let name = if crate::prelude::is_throwable(class) {
                    format!("java.lang.{class}")
                } else {
                    class.clone()
                };
                match fields.get("detailMessage") {
                    Some(m) if !matches!(m, Value::Undef) => format!("{name}: {}", java_str(m)),
                    _ => name,
                }
            }
            _ => java_str(v),
        }
    })
}

/// `classof(obj)` — the runtime class name of an instance (stack `[obj]`), or
/// the empty string for a non-instance value.
fn b_classof(vm: &mut VM, argc: u8) -> Value {
    let args = pop_args(vm, argc);
    match args.first() {
        Some(Value::Obj(id)) => HEAP.with(|h| {
            let h = h.borrow();
            match h.get(*id as usize) {
                Some(HostObj::Instance { class, .. }) => Value::str(class.clone()),
                _ => Value::str(""),
            }
        }),
        _ => Value::str(""),
    }
}

/// Pop `argc` values off the VM stack, restoring source (deepest-first) order.
fn pop_args(vm: &mut VM, argc: u8) -> Vec<Value> {
    let mut v = Vec::with_capacity(argc as usize);
    for _ in 0..argc {
        v.push(vm.stack.pop().unwrap_or(Value::Undef));
    }
    v.reverse();
    v
}

/// `new T[n]` — build an `n`-element array filled with the element default
/// (stack `[size, default]`).
fn b_array_new(vm: &mut VM, argc: u8) -> Value {
    let args = pop_args(vm, argc);
    let size = args.first().map(|v| v.to_int()).unwrap_or(0);
    let default = args.get(1).cloned().unwrap_or(Value::Undef);
    if size < 0 {
        return raise(
            vm,
            Fault::java("NegativeArraySizeException", size.to_string()),
        );
    }
    let arr = vec![default; size as usize];
    Value::Obj(heap_alloc(HostObj::Array(arr)))
}

/// `new T[s0][s1]…` — build a rectangular nested array (stack
/// `[s0, …, sK, leafDefault]`). The innermost level is filled with `leafDefault`
/// (the element type default for a fully-sized `new int[2][3]`, or `null` when
/// trailing dimensions are unsized as in `new int[2][]`).
fn b_array_new_multi(vm: &mut VM, argc: u8) -> Value {
    let mut args = pop_args(vm, argc);
    // Last arg is the leaf default; the rest are the dimension sizes.
    let default = args.pop().unwrap_or(Value::Undef);
    let sizes: Vec<i64> = args.iter().map(|v| v.to_int()).collect();
    if let Some(&n) = sizes.iter().find(|&&s| s < 0) {
        return raise(vm, Fault::java("NegativeArraySizeException", n.to_string()));
    }
    match build_nested(&sizes, &default) {
        Some(v) => v,
        None => raise(
            vm,
            Fault::internal("javars: multi-dimensional array needs a size"),
        ),
    }
}

/// Recursively allocate `sizes.len()` nested array levels; the innermost holds
/// clones of `default`. `None` when `sizes` is empty (no dimension).
fn build_nested(sizes: &[i64], default: &Value) -> Option<Value> {
    let (&head, rest) = sizes.split_first()?;
    let n = head.max(0) as usize;
    let elems: Vec<Value> = if rest.is_empty() {
        vec![default.clone(); n]
    } else {
        (0..n)
            .map(|_| build_nested(rest, default).unwrap_or(Value::Undef))
            .collect()
    };
    Some(Value::Obj(heap_alloc(HostObj::Array(elems))))
}

/// `{a, b, …}` — build an array from the popped element values.
fn b_array_lit(vm: &mut VM, argc: u8) -> Value {
    let elems = pop_args(vm, argc);
    Value::Obj(heap_alloc(HostObj::Array(elems)))
}

/// `a[i]` read (stack `[array, index]`), bounds-checked.
///
/// The lookup runs inside the `HEAP` borrow and any fault is raised *after* it
/// ends — [`raise`] allocates the throwable on that same heap, so raising while
/// the borrow is live would panic.
fn b_array_get(vm: &mut VM, argc: u8) -> Value {
    let args = pop_args(vm, argc);
    let arr = args.first().cloned().unwrap_or(Value::Undef);
    let idx = args.get(1).map(|v| v.to_int()).unwrap_or(0);
    let id = match arr {
        Value::Obj(id) => id,
        _ => return raise(vm, Fault::java("NullPointerException", NULL_ARRAY_LOAD)),
    };
    let got = HEAP.with(|h| {
        let h = h.borrow();
        match h.get(id as usize) {
            Some(HostObj::Array(a)) => match usize::try_from(idx).ok().and_then(|i| a.get(i)) {
                Some(v) => Ok(v.clone()),
                None => Err(index_fault(idx, a.len())),
            },
            _ => Err(Fault::internal("javars: not an array")),
        }
    });
    match got {
        Ok(v) => v,
        Err(f) => raise(vm, f),
    }
}

/// Java's `ArrayIndexOutOfBoundsException` detail message.
fn index_fault(idx: i64, len: usize) -> Fault {
    Fault::java(
        "ArrayIndexOutOfBoundsException",
        format!("Index {idx} out of bounds for length {len}"),
    )
}

// Java's "helpful NullPointerException" messages name the *bytecode local slot*
// of the null reference (`because "<local3>" is null`), which javars cannot
// reproduce — it has no javac slot numbering. These keep the operation half of
// Java's wording and drop the provenance clause (see BUGS.md).
const NULL_ARRAY_LOAD: &str = "Cannot load from array because the array is null";
const NULL_ARRAY_STORE: &str = "Cannot store to array because the array is null";
const NULL_ARRAY_LENGTH: &str = "Cannot read the array length because the array is null";

/// `a[i] = v` write (stack `[array, index, value]`), bounds-checked. Returns `v`.
fn b_array_set(vm: &mut VM, argc: u8) -> Value {
    let args = pop_args(vm, argc);
    let arr = args.first().cloned().unwrap_or(Value::Undef);
    let idx = args.get(1).map(|v| v.to_int()).unwrap_or(0);
    let val = args.get(2).cloned().unwrap_or(Value::Undef);
    let id = match arr {
        Value::Obj(id) => id,
        _ => return raise(vm, Fault::java("NullPointerException", NULL_ARRAY_STORE)),
    };
    let stored = HEAP.with(|h| {
        let mut h = h.borrow_mut();
        match h.get_mut(id as usize) {
            Some(HostObj::Array(a)) => match usize::try_from(idx).ok().filter(|&i| i < a.len()) {
                Some(i) => {
                    a[i] = val.clone();
                    Ok(())
                }
                None => Err(index_fault(idx, a.len())),
            },
            _ => Err(Fault::internal("javars: not an array")),
        }
    });
    match stored {
        Ok(()) => val,
        Err(f) => raise(vm, f),
    }
}

/// `new C(...)` — allocate an instance with an empty field map (stack
/// `[className]`). The compiler emits field defaults/initializers and the
/// constructor call after this.
fn b_new(vm: &mut VM, argc: u8) -> Value {
    let args = pop_args(vm, argc);
    let class = args
        .first()
        .map(|v| v.as_str_cow().into_owned())
        .unwrap_or_default();
    Value::Obj(heap_alloc(HostObj::Instance {
        class,
        fields: HashMap::new(),
    }))
}

/// `recv.field` read (stack `[recv, name]`): an array's `.length` or an instance
/// field.
fn b_field_get(vm: &mut VM, argc: u8) -> Value {
    let args = pop_args(vm, argc);
    let recv = args.first().cloned().unwrap_or(Value::Undef);
    let name = args
        .get(1)
        .map(|v| v.as_str_cow().into_owned())
        .unwrap_or_default();
    let id = match recv {
        Value::Obj(id) => id,
        _ => {
            // `null.length` is Java's array-length NPE; any other name is a
            // field read.
            let msg = if name == "length" {
                NULL_ARRAY_LENGTH.to_string()
            } else {
                format!("Cannot read field \"{name}\" because the receiver is null")
            };
            return raise(vm, Fault::java("NullPointerException", msg));
        }
    };
    let got = HEAP.with(|h| {
        let h = h.borrow();
        match h.get(id as usize) {
            Some(HostObj::Array(a)) if name == "length" => Ok(Value::Int(a.len() as i64)),
            Some(HostObj::Instance { fields, .. }) => {
                Ok(fields.get(&name).cloned().unwrap_or(Value::Undef))
            }
            _ => Err(Fault::internal(format!("javars: no field `{name}`"))),
        }
    });
    match got {
        Ok(v) => v,
        Err(f) => raise(vm, f),
    }
}

/// `recv.field = v` write (stack `[recv, name, value]`). Returns `v`.
fn b_field_set(vm: &mut VM, argc: u8) -> Value {
    let args = pop_args(vm, argc);
    let recv = args.first().cloned().unwrap_or(Value::Undef);
    let name = args
        .get(1)
        .map(|v| v.as_str_cow().into_owned())
        .unwrap_or_default();
    let val = args.get(2).cloned().unwrap_or(Value::Undef);
    let id = match recv {
        Value::Obj(id) => id,
        _ => {
            return raise(
                vm,
                Fault::java(
                    "NullPointerException",
                    format!("Cannot assign field \"{name}\" because the receiver is null"),
                ),
            )
        }
    };
    let ok = HEAP.with(|h| {
        let mut h = h.borrow_mut();
        match h.get_mut(id as usize) {
            Some(HostObj::Instance { fields, .. }) => {
                fields.insert(name.clone(), val.clone());
                true
            }
            _ => false,
        }
    });
    if ok {
        val
    } else {
        raise(
            vm,
            Fault::internal(format!("javars: cannot assign field `{name}`")),
        )
    }
}

/// `x instanceof C` (stack `[obj, className]`). Null is never an instance.
fn b_instanceof(vm: &mut VM, argc: u8) -> Value {
    let args = pop_args(vm, argc);
    let obj = args.first().cloned().unwrap_or(Value::Undef);
    let target = args
        .get(1)
        .map(|v| v.as_str_cow().into_owned())
        .unwrap_or_default();
    match obj {
        Value::Obj(id) => HEAP.with(|h| {
            let h = h.borrow();
            match h.get(id as usize) {
                Some(HostObj::Instance { class, .. }) => {
                    Value::bool(is_subclass_of(class, &target))
                }
                // A String value satisfies `instanceof String`.
                _ => Value::bool(false),
            }
        }),
        Value::Str(_) => {
            Value::bool(target == "String" || target == "Object" || target == "CharSequence")
        }
        _ => Value::bool(false),
    }
}

/// `__rust_compile("<base64>")` builtin: pop the base64-encoded `rust { ... }`
/// block body, compile it to a cdylib, and register its exports. Returns `null`.
fn b_ffi_compile(vm: &mut VM, argc: u8) -> Value {
    // The compiler emits exactly one argument (the base64 body); pop `argc`
    // defensively and keep the deepest.
    let mut body = Value::Undef;
    for _ in 0..argc {
        body = vm.stack.pop().unwrap_or(Value::Undef);
    }
    let b64 = body.as_str_cow().into_owned();
    if let Err(e) = fusevm::ffi::compile_and_register(&b64) {
        ffi_fault(vm, format!("javars: rust {{}} block: {e}"));
    }
    Value::Undef
}

/// `name(args...)` FFI dispatch builtin: pop the function name (top of stack)
/// and its `argc - 1` arguments, call the exported symbol via `fusevm::ffi`, and
/// return its result.
fn b_ffi_call(vm: &mut VM, argc: u8) -> Value {
    let name = vm
        .stack
        .pop()
        .map(|v| v.as_str_cow().into_owned())
        .unwrap_or_default();
    let n = argc.saturating_sub(1) as usize;
    let mut args = Vec::with_capacity(n);
    for _ in 0..n {
        args.push(vm.stack.pop().unwrap_or(Value::Undef));
    }
    args.reverse();
    match fusevm::ffi::try_call(&name, &args) {
        Some(Ok(v)) => v,
        Some(Err(e)) => {
            ffi_fault(vm, format!("javars: rust FFI call {name}: {e}"));
            Value::Undef
        }
        None => {
            ffi_fault(vm, format!("javars: unresolved reference: {name}"));
            Value::Undef
        }
    }
}

/// `recv.method(args...)` dispatch builtin for `String` receivers. Pops the
/// method name (top of stack), its `argc - 2` arguments, and the receiver, then
/// runs the corresponding `java.lang.String` method. A faulting method (bad
/// arity, out-of-range index, unknown method) surfaces as a `javars:` error
/// rather than silently returning a wrong value.
fn b_str_dispatch(vm: &mut VM, argc: u8) -> Value {
    let method = vm
        .stack
        .pop()
        .map(|v| v.as_str_cow().into_owned())
        .unwrap_or_default();
    let n = argc.saturating_sub(2) as usize; // minus receiver and method name
    let mut args = Vec::with_capacity(n);
    for _ in 0..n {
        args.push(vm.stack.pop().unwrap_or(Value::Undef));
    }
    args.reverse();
    let recv = vm.stack.pop().unwrap_or(Value::Undef);
    // A method call on a `null` reference is Java's NPE, not an empty string.
    if matches!(recv, Value::Undef) {
        return raise(
            vm,
            Fault::java(
                "NullPointerException",
                format!("Cannot invoke \"String.{method}()\" because the receiver is null"),
            ),
        );
    }
    let s = recv.as_str_cow().into_owned();
    match string_method(&s, &method, &args) {
        Ok(v) => v,
        Err(f) => raise(vm, f),
    }
}

/// Evaluate a `java.lang.String` method on `s`. Index/length semantics use
/// Unicode scalar (`char`) positions — exact for the ASCII/BMP common case and
/// consistent with javars's existing "a `char` literal is a one-character
/// string" model (astral characters, which Java counts as two UTF-16 units,
/// count as one here — the same documented simplification). An out-of-range
/// index raises Java's `StringIndexOutOfBoundsException` with its exact detail
/// message; an unknown method is a javars internal error.
fn string_method(s: &str, method: &str, args: &[Value]) -> Result<Value, Fault> {
    let char_len = || s.chars().count() as i64;
    match (method, args.len()) {
        ("length", 0) => Ok(Value::Int(char_len())),
        ("isEmpty", 0) => Ok(Value::bool(s.is_empty())),
        ("charAt", 1) => {
            let i = args[0].to_int();
            match usize::try_from(i).ok().and_then(|i| s.chars().nth(i)) {
                Some(c) => Ok(Value::str(c.to_string())),
                None => Err(Fault::java(
                    "StringIndexOutOfBoundsException",
                    format!("Index {i} out of bounds for length {}", char_len()),
                )),
            }
        }
        ("substring", 1) => substring(s, args[0].to_int(), char_len()),
        ("substring", 2) => substring(s, args[0].to_int(), args[1].to_int()),
        ("indexOf", 1) => Ok(Value::Int(char_index_of(s, &args[0].as_str_cow()))),
        ("contains", 1) => Ok(Value::bool(s.contains(args[0].as_str_cow().as_ref()))),
        ("equals", 1) => Ok(Value::bool(s == args[0].as_str_cow().as_ref())),
        ("equalsIgnoreCase", 1) => {
            let o = args[0].as_str_cow();
            Ok(Value::bool(s.to_lowercase() == o.to_lowercase()))
        }
        ("toUpperCase", 0) => Ok(Value::str(s.to_uppercase())),
        ("toLowerCase", 0) => Ok(Value::str(s.to_lowercase())),
        // Java `trim()` removes leading/trailing chars ≤ U+0020.
        ("trim", 0) => Ok(Value::str(s.trim_matches(|c: char| c <= ' ').to_string())),
        ("startsWith", 1) => Ok(Value::bool(s.starts_with(args[0].as_str_cow().as_ref()))),
        ("endsWith", 1) => Ok(Value::bool(s.ends_with(args[0].as_str_cow().as_ref()))),
        ("concat", 1) => Ok(Value::str(format!("{s}{}", args[0].as_str_cow()))),
        ("replace", 2) => Ok(Value::str(
            s.replace(args[0].as_str_cow().as_ref(), &args[1].as_str_cow()),
        )),
        ("repeat", 1) => {
            let n = args[0].to_int();
            if n < 0 {
                Err(Fault::java(
                    "IllegalArgumentException",
                    format!("count is negative: {n}"),
                ))
            } else {
                Ok(Value::str(s.repeat(n as usize)))
            }
        }
        _ => Err(Fault::internal(format!(
            "javars: unsupported String method `{method}` with {} argument(s)",
            args.len()
        ))),
    }
}

/// `String.substring(begin, end)` on `char` indices — `[begin, end)`, with
/// Java's bounds rules (`0 ≤ begin ≤ end ≤ length`) and its exact
/// `StringIndexOutOfBoundsException` message.
fn substring(s: &str, begin: i64, end: i64) -> Result<Value, Fault> {
    let len = s.chars().count() as i64;
    if begin < 0 || end > len || begin > end {
        return Err(Fault::java(
            "StringIndexOutOfBoundsException",
            format!("Range [{begin}, {end}) out of bounds for length {len}"),
        ));
    }
    let sub: String = s
        .chars()
        .skip(begin as usize)
        .take((end - begin) as usize)
        .collect();
    Ok(Value::str(sub))
}

/// `String.indexOf(sub)` returning a `char` index (not a byte offset), or `-1`.
fn char_index_of(s: &str, needle: &str) -> i64 {
    match s.find(needle) {
        Some(byte_pos) => s[..byte_pos].chars().count() as i64,
        None => -1,
    }
}

/// Static stdlib dispatch builtin (`Math.*`, `Integer.*`, `String.valueOf`, …).
/// Pops the method name (top of stack), the class name, and the `argc - 2`
/// arguments, then evaluates the corresponding static method. A faulting call
/// (bad arity, `NumberFormatException`, unknown method) surfaces as a `javars:`
/// error rather than a wrong value.
fn b_static_dispatch(vm: &mut VM, argc: u8) -> Value {
    let method = vm
        .stack
        .pop()
        .map(|v| v.as_str_cow().into_owned())
        .unwrap_or_default();
    let class = vm
        .stack
        .pop()
        .map(|v| v.as_str_cow().into_owned())
        .unwrap_or_default();
    let n = argc.saturating_sub(2) as usize; // minus class name and method name
    let mut args = Vec::with_capacity(n);
    for _ in 0..n {
        args.push(vm.stack.pop().unwrap_or(Value::Undef));
    }
    args.reverse();
    match static_method(&class, &method, &args) {
        Ok(v) => v,
        Err(f) => raise(vm, f),
    }
}

/// Evaluate a static stdlib method `Class.method(args)`.
///
/// Numeric overloads follow Java at the value level: `Math.abs`/`max`/`min`
/// keep an `int` result for integral operands and a `double` result when any
/// operand is floating point; `Math.pow`/`sqrt`/`floor`/`ceil` always return a
/// `double`; `Math.round` returns an integer (`floor(x + 0.5)`, ties toward
/// positive infinity). `Integer.parseInt`/`Long.parseLong` reject malformed
/// input the way `javac`-compiled code would throw `NumberFormatException`.
fn static_method(class: &str, method: &str, args: &[Value]) -> Result<Value, Fault> {
    let both_int = |a: &Value, b: &Value| matches!(a, Value::Int(_)) && matches!(b, Value::Int(_));
    match (class, method, args.len()) {
        // ── java.lang.Math ──
        ("Math", "abs", 1) => Ok(match &args[0] {
            Value::Int(n) => Value::Int(n.abs()),
            other => Value::float(other.to_float().abs()),
        }),
        ("Math", "max", 2) => Ok(if both_int(&args[0], &args[1]) {
            Value::Int(args[0].to_int().max(args[1].to_int()))
        } else {
            Value::float(args[0].to_float().max(args[1].to_float()))
        }),
        ("Math", "min", 2) => Ok(if both_int(&args[0], &args[1]) {
            Value::Int(args[0].to_int().min(args[1].to_int()))
        } else {
            Value::float(args[0].to_float().min(args[1].to_float()))
        }),
        ("Math", "pow", 2) => Ok(Value::float(args[0].to_float().powf(args[1].to_float()))),
        ("Math", "sqrt", 1) => Ok(Value::float(args[0].to_float().sqrt())),
        ("Math", "floor", 1) => Ok(Value::float(args[0].to_float().floor())),
        ("Math", "ceil", 1) => Ok(Value::float(args[0].to_float().ceil())),
        // Java `Math.round(double)` = `(long) Math.floor(a + 0.5d)` — ties round
        // toward positive infinity (round(-2.5) == -2), unlike Rust's `round`.
        ("Math", "round", 1) => Ok(Value::Int((args[0].to_float() + 0.5).floor() as i64)),

        // ── java.lang.Integer / Long ──
        ("Integer", "parseInt", 1) => parse_int_radix(&args[0].as_str_cow(), 10, true),
        ("Integer", "parseInt", 2) => {
            let radix = args[1].to_int();
            parse_int_radix(&args[0].as_str_cow(), radix, true)
        }
        ("Long", "parseLong", 1) => parse_int_radix(&args[0].as_str_cow(), 10, false),
        // `Integer.valueOf(String)` parses; `Integer.valueOf(int)` is identity.
        ("Integer", "valueOf", 1) => match &args[0] {
            Value::Str(s) => parse_int_radix(s, 10, true),
            other => Ok(Value::Int(other.to_int())),
        },
        // `Integer.toString(int)` / `Integer.toString(int, radix)`.
        ("Integer", "toString", 1) => Ok(Value::str(args[0].to_int().to_string())),
        ("Integer", "toString", 2) => Ok(Value::str(int_to_radix_string(
            args[0].to_int(),
            args[1].to_int(),
        ))),

        // ── java.lang.Boolean ──
        ("Boolean", "parseBoolean", 1) => Ok(Value::bool(
            args[0].as_str_cow().eq_ignore_ascii_case("true"),
        )),

        // ── java.lang.String ──
        // `String.valueOf(x)` renders any value with Java's `println` rules.
        ("String", "valueOf", 1) => Ok(Value::str(java_str(&args[0]))),
        // `String.format(fmt, args…)` — printf-style formatting (subset).
        ("String", "format", _) if !args.is_empty() => {
            let fmt = args[0].as_str_cow().into_owned();
            java_format(&fmt, &args[1..])
        }

        // ── java.util.Arrays ──
        // `Arrays.toString(a)` — shallow `[e0, e1, …]` (null → "null").
        ("Arrays", "toString", 1) => Ok(Value::str(arrays_to_string(&args[0]))),

        _ => Err(Fault::internal(format!(
            "javars: unsupported static method `{class}.{method}` with {} argument(s)",
            args.len()
        ))),
    }
}

/// `Arrays.toString(a)` — a shallow `[e0, e1, …]` rendering (Java's
/// `java.util.Arrays.toString`), each element via [`java_str`]. A `null`
/// reference renders as `null`.
fn arrays_to_string(v: &Value) -> String {
    match v {
        Value::Obj(id) => HEAP.with(|h| {
            let h = h.borrow();
            match h.get(*id as usize) {
                Some(HostObj::Array(a)) => {
                    let inner: Vec<String> = a.iter().map(java_str).collect();
                    format!("[{}]", inner.join(", "))
                }
                _ => java_str(v),
            }
        }),
        Value::Undef => "null".to_string(),
        _ => java_str(v),
    }
}

/// `String.format(fmt, args…)` — a faithful subset of `java.util.Formatter`:
/// conversions `d s S f b B x X o c %` and `%n`, with `-` (left-justify), `0`
/// (zero-pad), `+` (leading sign) flags, an optional width, and an optional
/// `.precision` (decimals for `f`, max length for `s`). Unsupported conversions
/// surface an error rather than a wrong string.
fn java_format(fmt: &str, args: &[Value]) -> Result<Value, Fault> {
    let mut out = String::new();
    let mut chars = fmt.chars().peekable();
    let mut argi = 0usize;
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        // flags
        let mut left = false;
        let mut zero = false;
        let mut plus = false;
        while let Some(&f) = chars.peek() {
            match f {
                '-' => left = true,
                '0' => zero = true,
                '+' => plus = true,
                ' ' | '#' | ',' | '(' => {}
                _ => break,
            }
            chars.next();
        }
        // width
        let mut width = String::new();
        while let Some(&d) = chars.peek() {
            if d.is_ascii_digit() {
                width.push(d);
                chars.next();
            } else {
                break;
            }
        }
        // .precision
        let mut prec: Option<usize> = None;
        if chars.peek() == Some(&'.') {
            chars.next();
            let mut p = String::new();
            while let Some(&d) = chars.peek() {
                if d.is_ascii_digit() {
                    p.push(d);
                    chars.next();
                } else {
                    break;
                }
            }
            prec = Some(p.parse().unwrap_or(0));
        }
        let conv = chars
            .next()
            .ok_or_else(|| Fault::internal("javars: String.format: dangling `%`"))?;
        let width_n: Option<usize> = width.parse().ok();
        match conv {
            '%' => out.push('%'),
            'n' => out.push('\n'),
            _ => {
                let arg = args.get(argi).ok_or_else(|| {
                    Fault::internal("javars: String.format: not enough arguments")
                })?;
                argi += 1;
                let (s, numeric) = format_conversion(conv, arg, prec, plus)?;
                out.push_str(&pad(&s, width_n, left, zero && numeric));
            }
        }
    }
    Ok(Value::str(out))
}

/// Render one `String.format` conversion. Returns the rendered text and whether
/// it is a numeric conversion (which may be zero-padded).
fn format_conversion(
    conv: char,
    arg: &Value,
    prec: Option<usize>,
    plus: bool,
) -> Result<(String, bool), Fault> {
    let sign = |neg: bool| {
        if neg {
            ""
        } else if plus {
            "+"
        } else {
            ""
        }
    };
    match conv {
        'd' => {
            let n = arg.to_int();
            Ok((format!("{}{n}", sign(n < 0)), true))
        }
        'f' => {
            let x = arg.to_float();
            let p = prec.unwrap_or(6);
            Ok((
                format!("{}{x:.p$}", sign(x.is_sign_negative() && x != 0.0)),
                true,
            ))
        }
        's' => {
            let mut s = java_str(arg);
            if let Some(p) = prec {
                s = s.chars().take(p).collect();
            }
            Ok((s, false))
        }
        'S' => {
            let mut s = java_str(arg).to_uppercase();
            if let Some(p) = prec {
                s = s.chars().take(p).collect();
            }
            Ok((s, false))
        }
        'b' => Ok((java_bool(arg).to_string(), false)),
        'B' => Ok((java_bool(arg).to_string().to_uppercase(), false)),
        'x' => Ok((format!("{:x}", arg.to_int()), true)),
        'X' => Ok((format!("{:X}", arg.to_int()), true)),
        'o' => Ok((format!("{:o}", arg.to_int()), true)),
        'c' => Ok((java_str(arg), false)),
        other => Err(Fault::internal(format!(
            "javars: String.format: unsupported conversion `%{other}`"
        ))),
    }
}

/// Java `%b`: `true` for a `true` Boolean, `false` for `false`/`null`, `true`
/// for any other non-null value.
fn java_bool(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Undef => false,
        _ => true,
    }
}

/// Pad `s` to `width` (char count). Left-justify with `-`; otherwise right-
/// justify, zero-padding after any leading sign when `zero` is set.
fn pad(s: &str, width: Option<usize>, left: bool, zero: bool) -> String {
    let w = match width {
        Some(w) => w,
        None => return s.to_string(),
    };
    let len = s.chars().count();
    if len >= w {
        return s.to_string();
    }
    let fill = w - len;
    if left {
        format!("{s}{}", " ".repeat(fill))
    } else if zero {
        // Zero-pad after a leading sign (`-`/`+`).
        if let Some(rest) = s.strip_prefix(['-', '+']) {
            let sign = &s[..1];
            format!("{sign}{}{rest}", "0".repeat(fill))
        } else {
            format!("{}{s}", "0".repeat(fill))
        }
    } else {
        format!("{}{s}", " ".repeat(fill))
    }
}

/// Parse a signed integer in the given radix with `java.lang.Integer`'s exact
/// rules: no surrounding whitespace is tolerated, the radix must be in
/// `[Character.MIN_RADIX, Character.MAX_RADIX]`, and the value must fit the
/// target type (`int` for `parseInt`, `long` for `parseLong`) — every failure
/// carries Java's own `NumberFormatException` detail message.
fn parse_int_radix(s: &str, radix: i64, int_width: bool) -> Result<Value, Fault> {
    let nfe = |m: String| Fault::java("NumberFormatException", m);
    if radix < 2 {
        return Err(nfe(format!("radix {radix} less than Character.MIN_RADIX")));
    }
    if radix > 36 {
        return Err(nfe(format!(
            "radix {radix} greater than Character.MAX_RADIX"
        )));
    }
    // Java quotes the raw input and, for a non-decimal radix, names it.
    let bad = || {
        if radix == 10 {
            nfe(format!("For input string: \"{s}\""))
        } else {
            nfe(format!("For input string: \"{s}\" under radix {radix}"))
        }
    };
    let n = i64::from_str_radix(s, radix as u32).map_err(|_| bad())?;
    if int_width && i32::try_from(n).is_err() {
        return Err(bad());
    }
    Ok(Value::Int(n))
}

/// Render `n` in the given radix (2..=36), matching `Integer.toString(i, radix)`.
fn int_to_radix_string(n: i64, radix: i64) -> String {
    if !(2..=36).contains(&radix) {
        // Java falls back to radix 10 for an out-of-range radix.
        return n.to_string();
    }
    let radix = radix as u64;
    if n == 0 {
        return "0".to_string();
    }
    let neg = n < 0;
    let mut v = (n as i128).unsigned_abs();
    let mut digits = Vec::new();
    while v > 0 {
        let d = (v % radix as u128) as u32;
        digits.push(std::char::from_digit(d, radix as u32).unwrap());
        v /= radix as u128;
    }
    if neg {
        digits.push('-');
    }
    digits.iter().rev().collect()
}

/// Install the debug line-marker builtin used by `java --dap`. The marker fires
/// synchronously at each statement; it delegates to the DAP server, which pauses
/// in place when the line is a breakpoint or step target.
pub fn install_debug(vm: &mut VM) {
    install(vm);
    vm.register_builtin(DBG_LINE, b_dbg_line);
}

/// The `DBG_LINE` marker builtin: hand control to the DAP server for this line,
/// then return `null` (popped by the trailing `Op::Pop` the compiler emits).
fn b_dbg_line(vm: &mut VM, _argc: u8) -> Value {
    crate::dap::on_debug_line(vm);
    Value::Undef
}

/// `System.out.println` builtin: pop `argc` values (0 or 1 in slice 1), print
/// them Java-formatted followed by a newline, and return `null`.
fn b_println(vm: &mut VM, argc: u8) -> Value {
    print_args(vm, argc, true, false)
}

/// `System.out.print` builtin: as [`b_println`] but with no trailing newline.
fn b_print(vm: &mut VM, argc: u8) -> Value {
    print_args(vm, argc, false, false)
}

/// `System.err.println` builtin: as [`b_println`] but on stderr.
fn b_eprintln(vm: &mut VM, argc: u8) -> Value {
    print_args(vm, argc, true, true)
}

/// `System.err.print` builtin: as [`b_print`] but on stderr.
fn b_eprint(vm: &mut VM, argc: u8) -> Value {
    print_args(vm, argc, false, true)
}

fn print_args(vm: &mut VM, argc: u8, newline: bool, err: bool) -> Value {
    use std::io::Write;
    // Pop the args (pushed left-to-right, so the last is on top) and restore
    // source order.
    let mut vals = Vec::with_capacity(argc as usize);
    for _ in 0..argc {
        vals.push(vm.stack.pop().unwrap_or(Value::Undef));
    }
    vals.reverse();
    // Format once, then write to the selected stream. Boxing the lock keeps the
    // two branches on one write path.
    let text: String = vals.iter().map(java_str).collect();
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut lock: Box<dyn Write> = if err {
        Box::new(stderr.lock())
    } else {
        Box::new(stdout.lock())
    };
    let _ = write!(lock, "{text}");
    if newline {
        let _ = writeln!(lock);
    }
    // `println`/`print` are `void`; the CallBuiltin result is discarded by a
    // trailing Pop in statement position.
    Value::Undef
}

/// Render a value with Java's `String.valueOf`/`println` rules (as opposed to
/// fusevm's shell-flavoured `as_str_cow`): booleans as `true`/`false`, whole
/// floats with a trailing `.0`, `Undef` as `null`.
pub fn java_str(v: &Value) -> String {
    match v {
        Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        Value::Float(f) => format_double(*f),
        Value::Undef => "null".to_string(),
        // A heap handle renders like Java's default `Object.toString`
        // (`ClassName@hex`). A user `toString()` override is dispatched by the
        // compiler before the value reaches here (see `Compiler::method_call`),
        // so this default only shows for classes that declare none.
        Value::Obj(id) => obj_default_str(*id),
        other => other.as_str_cow().into_owned(),
    }
}

/// Java's default `toString` for a heap object: `ClassName@<identity-hash>` for
/// an instance, `[@<hash>` for an array. The hash is the handle (deterministic
/// within a run) rather than a JVM identity hash.
fn obj_default_str(id: u32) -> String {
    HEAP.with(|h| {
        let h = h.borrow();
        match h.get(id as usize) {
            // An enum constant carries its name in a synthesized field, and
            // `Enum.toString()` returns exactly that. Reading it here is what
            // makes `String.valueOf(color)` and `Arrays.toString(values())`
            // print `RED` rather than `Color@1` — a builtin cannot re-enter the
            // VM to call the Java-level `toString()`.
            Some(HostObj::Instance { class, fields }) => match fields.get(crate::ast::ENUM_NAME) {
                Some(n) if !matches!(n, Value::Undef) => n.as_str_cow().into_owned(),
                _ => format!("{class}@{id:x}"),
            },
            Some(HostObj::Array(_)) => format!("[@{id:x}"),
            None => format!("(obj:{id})"),
        }
    })
}

/// Java's `Double.toString` prints whole values with a trailing `.0`
/// (`3.0`, not `3`) and keeps a decimal point; non-finite values print as
/// `Infinity`/`-Infinity`/`NaN`.
fn format_double(f: f64) -> String {
    if f.is_nan() {
        return "NaN".to_string();
    }
    if f.is_infinite() {
        return if f < 0.0 { "-Infinity" } else { "Infinity" }.to_string();
    }
    if f == 0.0 {
        // Java distinguishes the signed zeroes: `-0.0` prints with its sign.
        return if f.is_sign_negative() { "-0.0" } else { "0.0" }.to_string();
    }

    // `Double.toString` uses plain decimal only inside [1e-3, 1e7); outside that
    // range it switches to "computerized scientific notation". Rust's `{}` never
    // switches, so the range test has to be explicit or large/small magnitudes
    // print as long digit strings (`25000000.0` where Java says `2.5E7`).
    let mag = f.abs();
    if (1e-3..1e7).contains(&mag) {
        let s = format!("{f}");
        // Java always keeps a fractional digit: `1.0`, never `1`.
        return if s.contains('.') { s } else { format!("{s}.0") };
    }

    // Scientific form. Rust renders `2.5e7` / `1e7`; Java wants `2.5E7` / `1.0E7`
    // — an uppercase exponent, no `+`, and a mantissa that always carries a
    // fractional digit.
    let s = format!("{f:e}");
    let (mantissa, exp) = match s.split_once('e') {
        Some((m, e)) => (m, e),
        None => return s,
    };
    let mantissa = if mantissa.contains('.') {
        mantissa.to_string()
    } else {
        format!("{mantissa}.0")
    };
    format!("{mantissa}E{exp}")
}

/// Strict numeric hook: fusevm calls this only for an operation with a
/// non-numeric operand. In slice 1 that is Java's `String` `+` overload plus
/// value comparisons against strings; all-numeric arithmetic never reaches here
/// (it stays on the native fast path and the JIT).
pub fn numeric_hook(op: NumOp, a: &Value, b: &Value) -> Result<Value, String> {
    match op {
        // Java `+`: if either side is non-numeric (a String), concatenate using
        // Java's value-to-string rules.
        NumOp::Add => Ok(Value::str(format!("{}{}", java_str(a), java_str(b)))),
        // Value equality/ordering against a string operand (Java `.equals`/
        // `.compareTo`-style; `==` reference identity is not modeled in slice 1).
        NumOp::Eq => Ok(Value::bool(java_str(a) == java_str(b))),
        NumOp::Ne => Ok(Value::bool(java_str(a) != java_str(b))),
        NumOp::Lt => Ok(Value::bool(java_str(a) < java_str(b))),
        NumOp::Gt => Ok(Value::bool(java_str(a) > java_str(b))),
        NumOp::Le => Ok(Value::bool(java_str(a) <= java_str(b))),
        NumOp::Ge => Ok(Value::bool(java_str(a) >= java_str(b))),
        // Arithmetic other than `+` on a non-numeric operand is a type error in
        // Java (`"a" - 1` does not compile). Report it rather than coercing.
        NumOp::Sub | NumOp::Mul | NumOp::Div | NumOp::Mod | NumOp::Pow => Err(format!(
            "javars: operator `{op:?}` is not defined for operands `{}` and `{}`",
            java_str(a),
            java_str(b)
        )),
        NumOp::Neg => Err(format!(
            "javars: unary `-` is not defined for `{}`",
            java_str(a)
        )),
    }
}

/// Java floating-point `/`: IEEE-754 semantics, including the infinities and
/// NaN that a zero divisor produces. Both operands are coerced to `f64`, which
/// is correct because the compiler only routes a division here when at least one
/// side is not statically integral.
fn b_div(vm: &mut VM, _argc: u8) -> Value {
    let b = vm.stack.pop().unwrap_or(Value::Undef);
    let a = vm.stack.pop().unwrap_or(Value::Undef);
    Value::float(as_f64(&a) / as_f64(&b))
}

/// Coerce a value to `f64` for the floating division path.
fn as_f64(v: &Value) -> f64 {
    match v {
        Value::Int(i) => *i as f64,
        Value::Float(f) => *f,
        Value::Bool(b) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        other => other.as_str_cow().parse::<f64>().unwrap_or(f64::NAN),
    }
}
