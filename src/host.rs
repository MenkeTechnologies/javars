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
//! 2. **`+` overloading, and the arithmetic fusevm declines to answer.** Java's
//!    `+` is string concatenation when either operand is a `String`. fusevm runs
//!    *strict* once a numeric hook is installed, delegating to [`numeric_hook`]
//!    both any operation with a non-numeric operand — where `+` concatenates via
//!    the same [`java_str`] — and the numeric pairs it cannot answer exactly: an
//!    `i64` overflow, and a mixed `Int`/`Float` pair whose integer is past 2^53.
//!    The numeric pairs get Java's `long` wrapping and binary numeric promotion,
//!    never a concatenation; see [`java_numeric`].

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
/// `[className, message]`; `argc == 2`. Goes through the same `raise` path a
/// host-detected fault does, so the throwable is catchable.
pub const JFAULT: u16 = 726;

/// Builtin id for `main`'s `String[] args` — a fresh Java array of the program
/// arguments the CLI collected. Called once by the compiler's prologue, so the
/// array the program sees is its own (mutating it cannot affect a later read).
pub const JARGV: u16 = 727;

// ── Lambdas ──
// A lambda outlives the frame it was written in, but a javars local lives in a
// fusevm call-frame slot that does not. So a lambda becomes a heap closure that
// snapshots every enclosing local **by value** at the point the literal runs.
// Java only lets a lambda capture effectively-final locals, so a snapshot is
// observationally exact — and it is the only model that gives the enhanced
// `for` its per-iteration capture.

/// Build a closure. Stack `[cap0, …, capK, nameIdx, params, ncap]` (`ncap` on
/// top); `argc == ncap + 3`. `nameIdx` is the chunk name index of the lambda
/// body's subroutine and `params` its declared parameter count. Pushes the
/// closure's `Obj` handle.
pub const JMAKE_CLOSURE: u16 = 728;
/// Invoke a closure. Stack `[closure, arg0, …, argN]` (last argument on top);
/// `argc == N + 2` (the closure plus its arguments). Runs the body in its own
/// fusevm call frame through a nested `VM::run` and pushes its result.
pub const JCLOSURE_CALL: u16 = 729;

/// The runtime "class" [`JCLASSOF`] reports for a closure. `#` is not a legal
/// Java identifier character, so a user class can never collide with it; the
/// compiler's virtual-dispatch chain uses it as the arm that routes a
/// functional-interface call to the lambda body.
pub const LAMBDA_CLASS: &str = "#lambda";

// ── java.util collections ──

/// `new ArrayList<>()` / `new HashMap<>()` / … — allocate an empty collection.
/// Stack `[kindName, seedOrUndef]` (`seed` on top); `argc == 2`. `seed` is the
/// collection or array a copy constructor was given, or `null`. Pushes the
/// collection's `Obj` handle.
pub const JCOLL_NEW: u16 = 730;
/// An instance method on a collection receiver. Stack
/// `[recv, arg0, …, argN, methodName]` (`methodName` on top); `argc` counts all
/// of them. Same shape as [`JSTR_DISPATCH`], which is what routes a
/// statically-untyped receiver here.
pub const JCOLL_DISPATCH: u16 = 731;
/// The elements of an enhanced-`for` iterable, as a Java array. An array
/// receiver is returned unchanged; a collection is snapshotted into a fresh
/// array. Stack `[iterable]`; `argc == 1`. Emitted only when the compiler could
/// not prove the iterable is already an array, so array loops are unchanged.
pub const JITER_ARRAY: u16 = 732;

/// `>>>` — the logical (zero-fill) right shift. Stack `[value, count, width]`
/// (`width` on top, 32 or 64); `argc == 3`. fusevm's `Op::Shr` is always
/// arithmetic on 64 bits, so an `int` `>>>` — which must zero-fill at 32 — has
/// no native spelling; the compiler has already masked `count` to the operand's
/// width before the call.
pub const JUSHR: u16 = 733;

/// A narrowing primitive cast, `(ty) value`. Stack `[value, tyName]`
/// (`tyName` on top); `argc == 2`. Java's narrowing conversions are real value
/// changes: `(int) 3.9` truncates toward zero, `(int) 1e18` *saturates* to
/// `Integer.MAX_VALUE`, `(byte) 200` wraps to -56, and `(char) 70000` wraps to
/// its low 16 bits. Widening and identity casts never reach here — the compiler
/// emits the operand alone.
pub const JCAST: u16 = 734;

/// Java's *string conversion* of a `char` (JLS 5.1.11): the code point becomes
/// the one-character String. `argc == 1`. A `char` runs as an integer so that
/// `'a' + 1` is 98, and the compiler emits this at every point where the value
/// crosses into a String — `println(c)`, `"x" + c`, `String.valueOf(c)`, a
/// `String`-method argument — and where a `char` is boxed to a `Character` (a
/// collection element, which javars models as the one-character String). A
/// `char[]` operand converts element-wise, which is what makes
/// `Arrays.toString(s.toCharArray())` print `[a, b, c]`. Any other value passes
/// through unchanged.
pub const JCHR_STR: u16 = 735;

/// A checked *reference* cast, `(RefType) value`. Stack `[value, typeName]`
/// (`typeName` on top); `argc == 2`. The cast changes no representation — the
/// host heap already carries each object's class — so all it does is verify
/// one, raising `ClassCastException` when the runtime class is not the target
/// or a subtype of it. `null` casts to anything, and a value whose runtime
/// class javars cannot name exactly (an array, a collection, a lambda) passes
/// through unchecked rather than inventing a failure.
pub const JCHECKCAST: u16 = 736;

/// Round the top-of-stack value to 32-bit `float` precision. `argc == 1`.
///
/// fusevm has one floating representation (`f64`), so Java's `float` is modeled
/// as a `double` that is *kept* at `f32` precision: the compiler emits this
/// after every arithmetic operation whose static Java type is `float`, which is
/// what makes `1.0f / 3.0f` the `f32` 0.33333334 rather than the `f64` answer.
/// The same per-site narrowing the 32-bit `int` wrap uses, one width down.
pub const JF32: u16 = 737;

/// `Float.toString` of the top-of-stack value. `argc == 1`.
///
/// A `float` and a `double` holding the same bits print differently — Java's
/// shortest-round-trip is computed against the *type's* precision, so `0.1f`
/// prints `0.1` where the `double` with those bits prints
/// `0.10000000149011612`. The value model cannot tell them apart, so the
/// compiler emits this wherever a statically-`float` value crosses into a
/// String. A `float[]` operand converts element-wise.
pub const JF32_STR: u16 = 738;

/// One arithmetic operation performed at 32-bit `float` width. Stack
/// `[lhs, rhs, op]` (`op` on top, one of the [`f32_op`] constants); `argc == 3`.
///
/// Rounding the `f64` result afterwards is *not* the same computation: a double
/// rounding can land a ulp away from the single one Java performs.
/// `16777217.0f * 0.2f` is 3355443.2 in Java and 3355443.3 if the product is
/// formed in `f64` first. So a `float` operation is done in `f32` throughout,
/// which is why it costs a builtin rather than a native op — the only Java
/// arithmetic in javars that does.
pub const JF32_ARITH: u16 = 739;

/// `Comparable.compareTo` on a receiver that is not a user class instance.
/// Stack `[recv, arg, tag]` (`tag` on top); `argc == 3`.
///
/// Every boxed type spells `compareTo` differently and the answers are not
/// interchangeable: `Integer`/`Long` return the *sign* only, `Byte`/`Short`/
/// `Character` return the arithmetic difference, `Double`/`Float` go through
/// `Double.compare` (so `NaN` sorts above everything and `-0.0` below `0.0`),
/// `Boolean` is `false < true`, and `String` is the first differing `char`'s
/// difference. Routing them all through the `String` method — which is what
/// happened before this builtin existed — answered `Integer.valueOf(10)
/// .compareTo(9)` with `-8` (`'1' - '9'`) where Java answers `1`.
///
/// `tag` is the receiver's static Java type when the compiler knew it, else the
/// empty string, in which case the runtime value picks the rule.
pub const JCOMPARE_TO: u16 = 740;

/// `String.format(fmt, args…)`. Stack `[fmt, arg0, …, argN, tags]` (`tags` on
/// top); `argc == N + 2`.
///
/// `java.util.Formatter` type-checks every conversion against the *boxed class*
/// of its argument and throws `IllegalFormatConversionException` on a mismatch
/// — `%d` of a `Double`, `%f` of an `Integer`, `%c` of a `String`. fusevm's
/// value model cannot supply that class: one `Value::Int` stands for `Integer`,
/// `Long`, `Short`, `Byte` and `char` alike. So the compiler, which does know
/// each argument's static Java type, sends the boxed class names along in
/// `tags` — one per argument, `\x1f`-separated, an empty entry where the type
/// was not inferable (a lambda parameter, an erased `List.get`). The runtime
/// value picks the class for those.
pub const JFORMAT: u16 = 741;

/// The [`JF32_ARITH`] operator codes, shared with the compiler.
pub mod f32_op {
    pub const ADD: i64 = 0;
    pub const SUB: i64 = 1;
    pub const MUL: i64 = 2;
    pub const DIV: i64 = 3;
    pub const REM: i64 = 4;
}

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
    /// A lambda: the chunk name index of its body subroutine, its declared
    /// parameter count, and the enclosing locals it snapshotted by value. The
    /// body's prologue expects the parameters first, then the captures.
    Closure {
        name_idx: u16,
        params: u8,
        captures: Vec<Value>,
    },
    /// A `java.util.List` (`ArrayList`) — elements in list order.
    List {
        items: Vec<Value>,
        fixed: Fixity,
        /// Structural-modification counter, Java's `AbstractList.modCount`.
        /// Every `add`/`remove`/`clear` and every `sort` bumps it; a `subList`
        /// view snapshots it and refuses to operate once it has moved, which is
        /// how Java reports a view whose backing list changed underneath it.
        mods: u64,
    },
    /// A `List.subList(from, to)` **view**. It owns no elements: every read and
    /// write goes to the window `[offset, offset + len)` of `parent`, which is
    /// itself either a `List` or another view. That is what makes the aliasing
    /// real in both directions — a write through the parent shows in the view
    /// and a write through the view shows in the parent — rather than the copy
    /// that would answer correctly right up until someone wrote to it.
    SubList {
        parent: u32,
        offset: usize,
        len: usize,
        /// The backing list's `mods` when this view was created. A mismatch is
        /// Java's `ConcurrentModificationException`.
        exp_mods: u64,
    },
    /// A `java.util.Map`. Entries are stored in *insertion* order whatever the
    /// implementation; [`Order`] decides what order iteration and `toString`
    /// present them in.
    Map {
        entries: Vec<(Value, Value)>,
        order: Order,
    },
    /// A `java.util.Set`, stored and ordered exactly like [`HostObj::Map`].
    ///
    /// `fixed` carries the same distinction [`HostObj::List`] draws: `Set.of` is
    /// an immutable set, not a `HashSet`. Without it a `Set.of` value was
    /// indistinguishable from `new HashSet<>()`, so it answered `instanceof
    /// HashSet` `true` (Java: `false`) and accepted `add`/`remove`/`clear`
    /// silently (Java: `UnsupportedOperationException`). Only `Mutable` and
    /// `Immutable` occur — there is no `Arrays.asSet` to produce a fixed-size
    /// one — but the shared vocabulary keeps the two collections' guards
    /// identical.
    Set {
        items: Vec<Value>,
        order: Order,
        fixed: Fixity,
    },
}

/// What a collection's iteration order is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Order {
    /// `HashMap`/`HashSet` — Java's bucket order (see [`hash_order`]).
    Hash,
    /// `LinkedHashMap`/`LinkedHashSet` — insertion order.
    Insertion,
    /// `TreeMap`/`TreeSet` — ascending natural order of the keys/elements.
    Sorted,
}

/// Whether a list accepts structural modification. `Arrays.asList` is
/// fixed-size (`set` yes, `add`/`remove` no) and `List.of` is fully immutable —
/// both throw `UnsupportedOperationException` in Java, so javars throws too
/// rather than silently accepting the write.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fixity {
    Mutable,
    /// `Arrays.asList` — elements may be replaced, the length may not change.
    FixedSize,
    /// `List.of` — nothing may change.
    Immutable,
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
    /// The program arguments `main`'s `String[]` parameter is bound to — what
    /// the CLI collected after the file name. Set by [`set_argv`] before the run.
    static ARGV: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// Install the program arguments `main`'s `String[]` parameter will see. Call
/// before running the chunk.
pub fn set_argv(argv: Vec<String>) {
    ARGV.with(|a| *a.borrow_mut() = argv);
}

/// Clear the object heap (and superclass table stays until reset). Called at the
/// start of each program run so a fresh program never sees a prior run's handles.
pub fn heap_reset() {
    HEAP.with(|h| h.borrow_mut().clear());
    SUPERS.with(|s| s.borrow_mut().clear());
    PENDING.with(|p| *p.borrow_mut() = None);
    EXC_ENABLED.with(|e| e.set(false));
    ARGV.with(|a| a.borrow_mut().clear());
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
        let name =
            crate::prelude::qualified_throwable(f.class).unwrap_or_else(|| f.class.to_string());
        // Java's uncaught report names a messageless throwable with no trailing
        // `": "`, exactly as its `toString()` does.
        let detail = if f.msg.is_empty() {
            String::new()
        } else {
            format!(": {}", f.msg)
        };
        ffi_fault(vm, format!("Exception in thread \"main\" {name}{detail}"));
        return Value::Undef;
    }
    let mut fields = HashMap::new();
    // A fault raised with no message is Java's no-argument constructor, whose
    // `detailMessage` stays `null` — so `getMessage()` answers `null` and
    // `toString()` prints the class name alone. Storing an empty *String*
    // instead printed a bare trailing `": "` on every messageless throwable
    // (`java.lang.UnsupportedOperationException: `).
    let msg = if f.msg.is_empty() {
        Value::Undef
    } else {
        Value::str(f.msg)
    };
    fields.insert("detailMessage".to_string(), msg);
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

/// The *direct* supertypes of the JDK types javars's value model names, spelled
/// the way the JDK declares them so each line can be checked against one
/// `extends`/`implements` clause rather than against a flattened closure.
///
/// This is the single definition of the JDK half of the supertype graph. The
/// runtime type test (`instanceof`, and the `catch` matching that shares its
/// builtin) needs it exact; the reference cast walks the same graph and then
/// adds, separately and by name, the siblings it cannot prove wrong
/// (see [`cast_allowed`]).
///
/// `java.lang.Object` is not an edge here. Every non-null reference is an
/// `Object` whether or not its class appears in this table, so that is answered
/// once at the top of [`is_instance_of`] instead of being reachable only from
/// the classes that happen to be listed.
fn jdk_supers(class: &str) -> &'static [&'static str] {
    match class {
        // java.lang
        "String" => &["CharSequence", "Comparable", "Serializable"],
        "Integer" | "Double" => &["Number", "Comparable"],
        "Number" => &["Serializable"],
        "Boolean" | "Character" => &["Comparable", "Serializable"],
        "Enum" => &["Comparable", "Serializable"],
        // The throwable chain itself comes from the prelude's declarations,
        // which reach `Throwable` and stop; `Throwable implements Serializable`
        // is the one edge above it.
        "Throwable" => &["Serializable"],
        // java.util — the collection interfaces. `List` gained
        // `SequencedCollection` in Java 21 and `Set` did not, which is why they
        // are not one arm.
        "List" => &["Collection", "SequencedCollection"],
        "Set" => &["Collection"],
        "SequencedCollection" => &["Collection"],
        "SequencedSet" => &["Set", "SequencedCollection"],
        "Collection" => &["Iterable"],
        "SortedSet" => &["SequencedSet"],
        "NavigableSet" => &["SortedSet"],
        "SequencedMap" => &["Map"],
        "SortedMap" => &["SequencedMap"],
        "NavigableMap" => &["SortedMap"],
        "AbstractCollection" => &["Collection"],
        "AbstractList" => &["AbstractCollection", "List"],
        "AbstractSet" => &["AbstractCollection", "Set"],
        "AbstractMap" => &["Map"],
        // java.util — the concrete kinds javars models. `LinkedHashMap` and
        // `LinkedHashSet` extend their hash counterparts; the tree kinds do not,
        // which is the pair a name-matching answer gets wrong.
        "ArrayList" => &["AbstractList", "RandomAccess", "Cloneable", "Serializable"],
        "HashMap" => &["AbstractMap", "Cloneable", "Serializable"],
        "LinkedHashMap" => &["HashMap", "SequencedMap"],
        "TreeMap" => &["AbstractMap", "NavigableMap", "Cloneable", "Serializable"],
        "HashSet" => &["AbstractSet", "Cloneable", "Serializable"],
        "LinkedHashSet" => &["HashSet", "SequencedSet"],
        "TreeSet" => &["AbstractSet", "NavigableSet", "Cloneable", "Serializable"],
        // The internal names [`value_class`] gives the shapes Java spells with
        // syntax, or with a class the JDK does not export, so no user type can
        // collide with one. An array is `Cloneable` and `Serializable` and
        // nothing else.
        //
        // The three list views are each a `List` that is not an `ArrayList`,
        // and they do not agree with one another either: `List.of` answers
        // `AbstractList` `false` where the other two answer `true`, and
        // `subList` alone is not `Serializable`. One shared name would have to
        // get two of the three wrong, and `Fixity` plus the `SubList` variant
        // already tell them apart.
        "[]" => &["Cloneable", "Serializable"],
        "List$immutable" => &["AbstractCollection", "List", "RandomAccess", "Serializable"],
        // `Set.of` reaches `AbstractCollection` but NOT `AbstractSet`, and is
        // not `Cloneable` — the two edges that separate it from every `new`
        // set, measured against the JDK rather than assumed from the `List.of`
        // line above (which does carry `RandomAccess`; a set does not).
        "Set$immutable" => &["AbstractCollection", "Set", "Serializable"],
        "List$fixed" => &["AbstractList", "RandomAccess", "Serializable"],
        "List$sub" => &["AbstractList", "RandomAccess"],
        _ => &[],
    }
}

/// True when `class` is `target`, a (transitive) subclass of it, or a type that
/// implements/extends the interface `target` — walking the supertype graph
/// (superclass + interfaces).
///
/// The graph has two halves and both are walked at every node: the program's own
/// declarations ([`SUPERS`], set before the run) and the JDK's ([`jdk_supers`]).
/// A user class that `implements Comparable` needs the second half to reach
/// `Serializable`, and a modeled `TreeMap` has no entry in the first at all.
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
            stack.extend(jdk_supers(&cur).iter().map(|t| t.to_string()));
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
    vm.register_builtin(JUSHR, b_ushr);
    vm.register_builtin(JCAST, b_cast);
    vm.register_builtin(JCHR_STR, b_chr_str);
    vm.register_builtin(JCHECKCAST, b_checkcast);
    vm.register_builtin(JF32, b_f32);
    vm.register_builtin(JF32_STR, b_f32_str);
    vm.register_builtin(JF32_ARITH, b_f32_arith);
    vm.register_builtin(JCOMPARE_TO, b_compare_to);
    vm.register_builtin(JFORMAT, b_format);
    vm.register_builtin(JTHROW, b_throw);
    vm.register_builtin(JEXC_PENDING, b_exc_pending);
    vm.register_builtin(JEXC_TAKE, b_exc_take);
    vm.register_builtin(JEXC_DEPTH, b_exc_depth);
    vm.register_builtin(JEXC_CUT, b_exc_cut);
    vm.register_builtin(JEXC_ABORT, b_exc_abort);
    vm.register_builtin(JFAULT, b_fault);
    vm.register_builtin(JARGV, b_argv);
    vm.register_builtin(JMAKE_CLOSURE, b_make_closure);
    vm.register_builtin(JCLOSURE_CALL, b_closure_call);
    vm.register_builtin(JCOLL_NEW, b_coll_new);
    vm.register_builtin(JCOLL_DISPATCH, b_coll_dispatch);
    vm.register_builtin(JITER_ARRAY, b_iter_array);
}

/// `main`'s `String[] args` — a fresh array of the program arguments.
fn b_argv(vm: &mut VM, argc: u8) -> Value {
    pop_args(vm, argc);
    let elems = ARGV.with(|a| a.borrow().iter().cloned().map(Value::str).collect());
    Value::Obj(heap_alloc(HostObj::Array(elems)))
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
/// directly. The Java-level `toString()` override is not called from here — it
/// is the same rendering boundary `java_str` keeps (BUGS.md), and this path
/// only serves the uncaught report — so it reproduces the same text: the class
/// name — qualified with
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
                let name =
                    crate::prelude::qualified_throwable(class).unwrap_or_else(|| class.clone());
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
                // A lambda's "class" is the sentinel the dispatch chain tests,
                // so a functional-interface call routes to the closure body.
                Some(HostObj::Closure { .. }) => Value::str(LAMBDA_CLASS),
                _ => Value::str(""),
            }
        }),
        _ => Value::str(""),
    }
}

// ── Lambdas ─────────────────────────────────────────────────────────────────

/// [`JMAKE_CLOSURE`] — snapshot the captures and register the closure.
fn b_make_closure(vm: &mut VM, _argc: u8) -> Value {
    let ncap = vm.stack.pop().unwrap_or(Value::Undef).to_int() as usize;
    let params = vm.stack.pop().unwrap_or(Value::Undef).to_int() as u8;
    let name_idx = vm.stack.pop().unwrap_or(Value::Undef).to_int() as u16;
    let mut captures = Vec::with_capacity(ncap);
    for _ in 0..ncap {
        captures.push(vm.stack.pop().unwrap_or(Value::Undef));
    }
    captures.reverse();
    Value::Obj(heap_alloc(HostObj::Closure {
        name_idx,
        params,
        captures,
    }))
}

/// A copy of a closure handle's metadata, if `v` is one.
fn closure_meta(v: &Value) -> Option<(u16, u8, Vec<Value>)> {
    let Value::Obj(id) = v else {
        return None;
    };
    HEAP.with(|h| {
        let h = h.borrow();
        match h.get(*id as usize) {
            Some(HostObj::Closure {
                name_idx,
                params,
                captures,
            }) => Some((*name_idx, *params, captures.clone())),
            _ => None,
        }
    })
}

/// [`JCLOSURE_CALL`] — invoke a closure with the arguments already on the stack.
///
/// The body is an ordinary javars subroutine, so it runs in a real fusevm call
/// frame; the frame is entered by hand (rather than by `Op::Call`) because the
/// entry address comes from the closure value, not from a compile-time name
/// operand. A nested `VM::run` drives it, exactly as the sibling fusevm
/// frontends (kotlinrs, groovyrs, scalars) drive their closure bodies.
fn b_closure_call(vm: &mut VM, argc: u8) -> Value {
    let n = argc.saturating_sub(1) as usize;
    let mut args = Vec::with_capacity(n);
    for _ in 0..n {
        args.push(vm.stack.pop().unwrap_or(Value::Undef));
    }
    args.reverse();
    let clo = vm.stack.pop().unwrap_or(Value::Undef);
    // An exception already in flight must not start another body running: the
    // enclosing frame is unwinding and would otherwise re-run side effects.
    if PENDING.with(|p| p.borrow().is_some()) {
        return Value::Undef;
    }
    let Some((name_idx, params, captures)) = closure_meta(&clo) else {
        return raise(
            vm,
            Fault::java(
                "NullPointerException",
                "Cannot invoke a functional interface method because the target is null"
                    .to_string(),
            ),
        );
    };
    let Some(entry) = vm.chunk.find_sub(name_idx) else {
        return raise(vm, Fault::internal("javars: lambda body not found"));
    };
    // The prologue binds exactly `params` arguments then the captures, so a
    // mismatched arity is padded with `null` / truncated rather than corrupting
    // the frame.
    let stack_base = vm.stack.len();
    for i in 0..params as usize {
        vm.stack.push(args.get(i).cloned().unwrap_or(Value::Undef));
    }
    for cap in captures {
        vm.stack.push(cap);
    }
    run_sub(vm, entry, stack_base)
}

// ── java.util collections ────────────────────────────────────────────────────
//
// Every collection is a `HostObj` on the same slab arrays and instances live
// on, so `List` aliasing, `==` identity, and passing one to a method all behave
// like Java references with no extra machinery.
//
// Entries are always *stored* in insertion order; the implementation's
// [`Order`] is applied when they are iterated, printed, or handed to
// `keySet()`/`values()`. That keeps `LinkedHashMap` free and makes `HashMap`'s
// order a pure function of the keys — see [`hash_order`].

/// Java's `Object.hashCode()` for the value kinds javars models. `None` for a
/// heap object, whose Java hash is an identity hash javars cannot reproduce (and
/// whose iteration order is therefore not reproducible in Java either).
fn java_hash(v: &Value) -> Option<i32> {
    Some(match v {
        // `String.hashCode` is specified: s[0]*31^(n-1) + … + s[n-1]. Java
        // counts UTF-16 code units; javars counts scalars, the same
        // `char`-model simplification the `String` methods already make.
        Value::Str(s) => s
            .chars()
            .fold(0i32, |h, c| h.wrapping_mul(31).wrapping_add(c as i32)),
        // `Integer.hashCode` is the value; `Long.hashCode` folds the halves.
        Value::Int(n) => {
            if *n >= i32::MIN as i64 && *n <= i32::MAX as i64 {
                *n as i32
            } else {
                (*n ^ ((*n as u64) >> 32) as i64) as i32
            }
        }
        // `Double.hashCode` folds `doubleToLongBits` the way `Long` does.
        Value::Float(f) => {
            let bits = f.to_bits() as i64;
            (bits ^ ((bits as u64) >> 32) as i64) as i32
        }
        Value::Bool(b) => {
            if *b {
                1231
            } else {
                1237
            }
        }
        _ => return None,
    })
}

/// The order a `HashMap`/`HashSet` iterates `keys` in, as indices into `keys`.
///
/// Java lays entries out in a power-of-two table, indexing with
/// `(capacity - 1) & (h ^ (h >>> 16))`, appending within a bucket and preserving
/// relative order across a resize. Iteration then walks bucket 0 upward. So the
/// order is exactly a *stable* sort of the insertion sequence by bucket index —
/// verified against OpenJDK 26 for `String` and `Integer` keys, including across
/// the resize at 13 entries.
///
/// Two things are not modeled, and neither is reproducible in Java either: a bin
/// that treeifies (8 collisions in one bucket with a table of 64+) and a key
/// whose `hashCode` is the JVM identity hash. A key with no modeled hash keeps
/// insertion order.
fn hash_order(keys: &[Value]) -> Vec<usize> {
    let n = keys.len();
    let mut cap = 16usize;
    while n > cap * 3 / 4 {
        cap *= 2;
    }
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by_key(|&i| {
        let h = java_hash(&keys[i]).unwrap_or(0) as u32;
        ((cap as u32 - 1) & (h ^ (h >> 16))) as usize
    });
    idx
}

/// The order `items` are presented in under `order`, as indices into `items`.
fn present_order(items: &[Value], order: Order) -> Vec<usize> {
    match order {
        Order::Insertion => (0..items.len()).collect(),
        Order::Hash => hash_order(items),
        Order::Sorted => {
            let mut idx: Vec<usize> = (0..items.len()).collect();
            idx.sort_by(|&a, &b| natural_cmp(&items[a], &items[b]));
            idx
        }
    }
}

/// Java's `equals` for the value kinds javars models: value equality for
/// strings, numbers, and booleans; reference identity for a heap object (the
/// same simplification javars's `==` already makes — a user `equals` override is
/// not called).
fn value_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::Obj(x), Value::Obj(y)) => x == y,
        (Value::Undef, Value::Undef) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
            // `Integer.equals(Long)` is false in Java, but javars has one
            // integral kind, so numeric equality is compared by value.
            match (a, b) {
                (Value::Int(x), Value::Int(y)) => x == y,
                _ => a.to_float() == b.to_float(),
            }
        }
        _ => false,
    }
}

/// Ascending natural order (`Comparable`) for the sorted collections and
/// `Collections.sort`: numbers numerically, strings lexicographically by
/// `char`, `null` first. Mixed kinds fall back to a stable "equal".
fn natural_cmp(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Value::Str(x), Value::Str(y)) => x.cmp(y),
        (Value::Undef, Value::Undef) => Ordering::Equal,
        (Value::Undef, _) => Ordering::Less,
        (_, Value::Undef) => Ordering::Greater,
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        _ => a
            .to_float()
            .partial_cmp(&b.to_float())
            .unwrap_or(Ordering::Equal),
    }
}

/// Allocate the collection `kind` names, seeded from `seed` when a copy
/// constructor supplied one.
fn new_collection(kind: &str, seed: &Value) -> Result<Value, Fault> {
    let obj = match kind {
        "ArrayList" | "LinkedList" | "List" => HostObj::List {
            mods: 0,
            items: sequence_items(seed).unwrap_or_default(),
            fixed: Fixity::Mutable,
        },
        "HashMap" | "Map" => HostObj::Map {
            entries: map_entries(seed).unwrap_or_default(),
            order: Order::Hash,
        },
        "LinkedHashMap" => HostObj::Map {
            entries: map_entries(seed).unwrap_or_default(),
            order: Order::Insertion,
        },
        "TreeMap" => HostObj::Map {
            entries: map_entries(seed).unwrap_or_default(),
            order: Order::Sorted,
        },
        "HashSet" | "Set" => HostObj::Set {
            items: distinct(&sequence_items(seed).unwrap_or_default()),
            order: Order::Hash,
            fixed: Fixity::Mutable,
        },
        "LinkedHashSet" => HostObj::Set {
            items: distinct(&sequence_items(seed).unwrap_or_default()),
            order: Order::Insertion,
            fixed: Fixity::Mutable,
        },
        "TreeSet" => HostObj::Set {
            items: distinct(&sequence_items(seed).unwrap_or_default()),
            order: Order::Sorted,
            fixed: Fixity::Mutable,
        },
        other => {
            return Err(Fault::internal(format!(
                "javars: `{other}` is not a collection javars models"
            )))
        }
    };
    Ok(Value::Obj(heap_alloc(obj)))
}

/// The distinct values of `vals`, keeping the first of each repeat — what
/// building a `Set` from a sequence produces.
fn distinct(vals: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(vals.len());
    for v in vals {
        if !out.iter().any(|x| value_eq(x, v)) {
            out.push(v.clone());
        }
    }
    out
}

/// The elements of any sequence-shaped heap object — an array, a `List`, or a
/// `Set` (in presentation order) — cloned out from under the heap borrow.
fn sequence_items(v: &Value) -> Option<Vec<Value>> {
    let Value::Obj(id) = v else {
        return None;
    };
    // A `subList` view holds no elements of its own — its window has to be read
    // out of the backing list, and only after the view is checked against it,
    // so a stale view raises rather than reporting the wrong slice.
    if let Some(items) = sublist_items(*id as usize) {
        return items.ok();
    }
    HEAP.with(|h| {
        let h = h.borrow();
        match h.get(*id as usize) {
            Some(HostObj::Array(items)) | Some(HostObj::List { items, .. }) => Some(items.clone()),
            Some(HostObj::Set { items, order, .. }) => Some(
                present_order(items, *order)
                    .into_iter()
                    .map(|i| items[i].clone())
                    .collect(),
            ),
            _ => None,
        }
    })
}

/// The elements a `subList` view currently presents, or the comodification
/// fault if its backing list moved. `None` when the handle is not a view.
fn sublist_items(id: usize) -> Option<Result<Vec<Value>, Fault>> {
    if !is_sublist(id) {
        return None;
    }
    Some(checked_window(id).and_then(|(root, offset, len)| {
        HEAP.with(|h| match h.borrow().get(root) {
            Some(HostObj::List { items, .. }) => Ok(items[offset..offset + len].to_vec()),
            _ => Err(Fault::internal("javars: dangling subList backing")),
        })
    }))
}

/// The entries of a `Map` heap object, in insertion order.
fn map_entries(v: &Value) -> Option<Vec<(Value, Value)>> {
    let Value::Obj(id) = v else {
        return None;
    };
    HEAP.with(|h| match h.borrow().get(*id as usize) {
        Some(HostObj::Map { entries, .. }) => Some(entries.clone()),
        _ => None,
    })
}

/// True when the handle points at a collection — the test that routes a
/// statically-untyped receiver away from the `String` methods.
fn is_collection(v: &Value) -> bool {
    let Value::Obj(id) = v else {
        return false;
    };
    HEAP.with(|h| {
        matches!(
            h.borrow().get(*id as usize),
            Some(
                HostObj::List { .. }
                    | HostObj::Map { .. }
                    | HostObj::Set { .. }
                    | HostObj::SubList { .. }
            )
        )
    })
}

/// [`JCOLL_NEW`] — see [`new_collection`].
fn b_coll_new(vm: &mut VM, argc: u8) -> Value {
    let args = pop_args(vm, argc);
    let kind = args
        .first()
        .map(|v| v.as_str_cow().into_owned())
        .unwrap_or_default();
    let seed = args.get(1).cloned().unwrap_or(Value::Undef);
    match new_collection(&kind, &seed) {
        Ok(v) => v,
        Err(f) => raise(vm, f),
    }
}

/// [`JITER_ARRAY`] — the elements of an enhanced-`for` iterable as an array.
fn b_iter_array(vm: &mut VM, argc: u8) -> Value {
    let args = pop_args(vm, argc);
    let it = args.into_iter().next().unwrap_or(Value::Undef);
    // Already an array: hand the same handle back, so an array loop keeps
    // aliasing (mutating `a[i]` inside the loop is visible).
    if let Value::Obj(id) = it {
        if HEAP.with(|h| matches!(h.borrow().get(id as usize), Some(HostObj::Array(_)))) {
            return it;
        }
    }
    match sequence_items(&it) {
        Some(items) => Value::Obj(heap_alloc(HostObj::Array(items))),
        None if matches!(it, Value::Undef) => raise(
            vm,
            Fault::java(
                "NullPointerException",
                "Cannot iterate over a null reference".to_string(),
            ),
        ),
        None => raise(
            vm,
            Fault::internal("javars: the enhanced `for` needs an array or a collection"),
        ),
    }
}

/// Run the subroutine at `entry` whose prologue values are already stacked above
/// `stack_base`, in its own call frame, and return its value.
///
/// The pushed frame's `return_ip` is past the end of the chunk, so the body's
/// `Op::ReturnValue` pops the frame and ends the nested run at exactly that
/// point. The interpreter's `ip` is saved and restored so the paused enclosing
/// dispatch loop resumes where it left off.
fn run_sub(vm: &mut VM, entry: usize, stack_base: usize) -> Value {
    let return_ip = vm.chunk.ops.len();
    vm.frames.push(fusevm::Frame {
        return_ip,
        stack_base,
        slots: Vec::new(),
        // Same identity `Op::Call` records: this frame enters the subroutine
        // at `entry`, so `Chunk::sub_slot_names` is reachable from it.
        entry_ip: Some(entry),
    });
    let saved_ip = vm.ip;
    vm.ip = entry;
    let result = vm.run();
    vm.ip = saved_ip;
    match result {
        fusevm::VMResult::Ok(v) => v,
        // A halt raised inside the body (an internal fault) leaves the halt flag
        // set, which stops the enclosing run too; hand back whatever is on top.
        fusevm::VMResult::Halted => vm.stack.pop().unwrap_or(Value::Undef),
        fusevm::VMResult::Error(e) => {
            ffi_fault(vm, format!("javars: {e}"));
            Value::Undef
        }
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
    Value::bool(is_instance_of(&obj, &target))
}

/// The class a value answers `instanceof` as, for every shape javars's value
/// model names one.
///
/// `None` means the class is genuinely not recorded rather than absent from this
/// list: `null`, and a lambda, whose closure carries its body and its captures
/// but not the functional interface it was assigned to.
///
/// The two names that are not legal Java identifiers — `[]` and `List$view` —
/// exist so an array and a non-`ArrayList` list view can carry supertypes in
/// [`jdk_supers`] without a user type ever being able to name them.
fn value_class(v: &Value) -> Option<String> {
    Some(match v {
        Value::Str(_) => "String".to_string(),
        Value::Int(_) => "Integer".to_string(),
        Value::Float(_) => "Double".to_string(),
        Value::Bool(_) => "Boolean".to_string(),
        Value::Obj(id) => {
            return HEAP.with(|h| {
                Some(match h.borrow().get(*id as usize)? {
                    HostObj::Instance { class, .. } => class.clone(),
                    HostObj::Array(_) => "[]".to_string(),
                    // `Arrays.asList` and `List.of` are `List`s that are not
                    // `ArrayList`s, and a `subList` view is a third answer
                    // again — see the note in [`jdk_supers`].
                    HostObj::List { fixed, .. } => match fixed {
                        Fixity::Mutable => "ArrayList".to_string(),
                        Fixity::FixedSize => "List$fixed".to_string(),
                        Fixity::Immutable => "List$immutable".to_string(),
                    },
                    HostObj::SubList { .. } => "List$sub".to_string(),
                    HostObj::Map { order, .. } => match order {
                        Order::Hash => "HashMap".to_string(),
                        Order::Insertion => "LinkedHashMap".to_string(),
                        Order::Sorted => "TreeMap".to_string(),
                    },
                    // `Set.of` is not a `HashSet`, exactly as `List.of` is not
                    // an `ArrayList`; without the fixity it answered to both.
                    HostObj::Set { order, fixed, .. } => match (fixed, order) {
                        (Fixity::Mutable | Fixity::FixedSize, Order::Hash) => "HashSet".to_string(),
                        (Fixity::Mutable | Fixity::FixedSize, Order::Insertion) => {
                            "LinkedHashSet".to_string()
                        }
                        (Fixity::Mutable | Fixity::FixedSize, Order::Sorted) => {
                            "TreeSet".to_string()
                        }
                        (Fixity::Immutable, _) => "Set$immutable".to_string(),
                    },
                    HostObj::Closure { .. } => return None,
                })
            });
        }
        _ => return None,
    })
}

/// Java's `x instanceof T`: true when `x` is a non-null reference whose runtime
/// class is `T`, a subclass of it, or a type implementing the interface `T`.
///
/// Two rules come before the graph walk, and both are the reason the previous
/// implementation — which answered only for a `String` and a user-class instance
/// and returned `false` for everything else — was wrong far more often than it
/// looked. `null` is an instance of nothing, including `Object`; and every
/// non-null reference *is* an `Object`, whatever javars models it as.
fn is_instance_of(v: &Value, target: &str) -> bool {
    let Some(class) = value_class(v) else {
        // `null` is an instance of nothing. A lambda is at least an `Object`;
        // its functional interface is not recorded, so that is as far as the
        // answer goes.
        return target == "Object" && matches!(v, Value::Obj(_));
    };
    target == "Object" || is_subclass_of(&class, target)
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

/// [`JCOLL_DISPATCH`] — an instance method on a collection receiver.
fn b_coll_dispatch(vm: &mut VM, argc: u8) -> Value {
    let method = vm
        .stack
        .pop()
        .map(|v| v.as_str_cow().into_owned())
        .unwrap_or_default();
    let n = argc.saturating_sub(2) as usize;
    let mut args = Vec::with_capacity(n);
    for _ in 0..n {
        args.push(vm.stack.pop().unwrap_or(Value::Undef));
    }
    args.reverse();
    let recv = vm.stack.pop().unwrap_or(Value::Undef);
    coll_method(vm, &recv, &method, &args)
}

/// Evaluate `recv.method(args)` on a collection.
///
/// Every method that mutates takes the heap borrow, edits, and drops it before
/// returning; the two that run user code (`sort` with a comparator, `forEach`)
/// snapshot first and re-enter the VM with no borrow held, because a lambda body
/// can allocate.
fn coll_method(vm: &mut VM, recv: &Value, method: &str, args: &[Value]) -> Value {
    let Value::Obj(id) = recv else {
        return raise(
            vm,
            Fault::java(
                "NullPointerException",
                format!("Cannot invoke \"{method}()\" because the receiver is null"),
            ),
        );
    };
    let id = *id as usize;
    // Every method on a view checks it against its backing list first, exactly
    // as Java's `checkForComodification` does. `sublist_method` repeats the
    // check to get the window; this one covers the paths that bypass it
    // (`toString`, `sort`, `forEach`).
    if let Some(f) = stale_view(recv) {
        return raise(vm, f);
    }
    // `subList` allocates a view over the receiver, so it needs the receiver's
    // handle — which `list_method` (working on a plain `&mut Vec`) never sees.
    if method == "subList" && args.len() == 2 {
        return match make_sublist(id, args[0].to_int(), args[1].to_int()) {
            Ok(v) => v,
            Err(f) => raise(vm, f),
        };
    }
    // The two VM-re-entering methods are handled before any borrow is taken.
    match (method, args.len()) {
        ("sort", 1) => {
            let Some(items) = sequence_items(recv) else {
                return raise(vm, Fault::internal("javars: `sort` needs a List receiver"));
            };
            let sorted = match sort_with(vm, items, &args[0]) {
                Ok(v) => v,
                Err(f) => return raise(vm, f),
            };
            // `ArrayList.sort` bumps `modCount` even though the length is
            // unchanged, so an outstanding view is invalidated by it — verified
            // against the reference JDK, which throws for a view read after
            // `Collections.sort(parent)`.
            if let Err(f) = write_sequence(id, sorted, true) {
                return raise(vm, f);
            }
            return Value::Undef;
        }
        ("forEach", 1) => {
            // A `Map`'s consumer takes (key, value); a List/Set's takes one.
            if let Some(entries) = map_entries(recv) {
                let order = map_order(recv);
                let keys: Vec<Value> = entries.iter().map(|(k, _)| k.clone()).collect();
                for i in present_order(&keys, order) {
                    let (k, v) = entries[i].clone();
                    invoke_closure(vm, &args[0], &[k, v]);
                }
            } else if let Some(items) = sequence_items(recv) {
                for it in items {
                    invoke_closure(vm, &args[0], &[it]);
                }
            }
            return Value::Undef;
        }
        _ => {}
    }
    // `toString` renders elements, which re-reads the heap (an element may be
    // another collection), so it runs before any borrow is taken.
    if method == "toString" && args.is_empty() {
        return Value::str(java_str(recv));
    }
    // Likewise `addAll`/`equals` read their argument collection: snapshot it
    // first, because the borrow below is exclusive.
    let arg_seqs: Vec<Option<Vec<Value>>> = args.iter().map(sequence_items).collect();
    if is_sublist(id) {
        return match sublist_method(id, method, args, &arg_seqs) {
            Ok(v) => v,
            Err(f) => raise(vm, f),
        };
    }
    let result = HEAP.with(|h| {
        let mut heap = h.borrow_mut();
        let Some(obj) = heap.get_mut(id) else {
            return Err(Fault::internal("javars: dangling collection handle"));
        };
        match obj {
            HostObj::List { items, fixed, mods } => {
                let before = items.len();
                let r = list_method(items, *fixed, method, args, &arg_seqs);
                // Any length change is a structural modification, which is what
                // Java's `modCount` counts (a `remove` that finds nothing does
                // not bump it there either).
                if items.len() != before {
                    *mods += 1;
                }
                r
            }
            HostObj::Map { entries, order } => map_method(entries, *order, method, args),
            HostObj::Set { items, fixed, .. } => set_method(items, *fixed, method, args, &arg_seqs),
            _ => Err(Fault::internal(format!(
                "javars: `{method}` is not a collection method"
            ))),
        }
    });
    match result {
        Ok(NewColl::Value(v)) => v,
        // A derived view (`keySet`, `values`) is allocated after the borrow is
        // released, because allocating touches the same slab.
        Ok(NewColl::Alloc(obj)) => Value::Obj(heap_alloc(obj)),
        Err(f) => raise(vm, f),
    }
}

/// A collection method's result: a plain value, or a new heap object that must
/// be allocated once the receiver's borrow has been dropped.
enum NewColl {
    Value(Value),
    Alloc(HostObj),
}

// ── `List.subList` views ────────────────────────────────────────────────────
//
// A view owns no elements. `resolve_window` walks the (possibly nested) parent
// chain down to the backing `List`, giving an absolute window into it; every
// operation reads that window out, runs the ordinary `list_method` on it, and
// writes it back. A length change there is a structural modification of the
// backing list, so it bumps the backing `mods` and is pushed up the ancestor
// chain — matching Java's `SubList.updateSizeAndModCount`, which keeps the
// enclosing views usable while a *sibling* view correctly goes stale.

/// True when a handle is a `subList` view rather than a list of its own.
fn is_sublist(id: usize) -> bool {
    HEAP.with(|h| matches!(h.borrow().get(id), Some(HostObj::SubList { .. })))
}

/// The comodification fault a value would raise if it were rendered — `Some`
/// only for a `subList` view whose backing list has been structurally modified
/// since. Java reports it because rendering a view iterates it; javars's
/// rendering is infallible, so the raising call sites consult this first.
fn stale_view(v: &Value) -> Option<Fault> {
    let Value::Obj(id) = v else {
        return None;
    };
    sublist_items(*id as usize)?.err()
}

/// A view resolved against its backing list: the backing `List`'s handle, the
/// absolute offset of the window, and its length. `None` when `id` is not a
/// view, or when the chain does not bottom out in a `List`.
fn resolve_window(id: usize) -> Option<(usize, usize, usize)> {
    HEAP.with(|h| {
        let heap = h.borrow();
        let Some(HostObj::SubList { len, .. }) = heap.get(id) else {
            return None;
        };
        let len = *len;
        let mut offset = 0;
        let mut cur = id;
        // The chain is built parent-first and can only ever be as deep as the
        // heap is long, which bounds the walk even if a handle were corrupted.
        for _ in 0..=heap.len() {
            match heap.get(cur) {
                Some(HostObj::SubList {
                    parent, offset: o, ..
                }) => {
                    offset += o;
                    cur = *parent as usize;
                }
                Some(HostObj::List { .. }) => return Some((cur, offset, len)),
                _ => return None,
            }
        }
        None
    })
}

/// The backing list's structural-modification count, or `None` for a handle
/// that is not a `List`.
fn list_mods(id: usize) -> Option<u64> {
    HEAP.with(|h| match h.borrow().get(id) {
        Some(HostObj::List { mods, .. }) => Some(*mods),
        _ => None,
    })
}

/// Java's `ConcurrentModificationException`: the view's snapshot of the backing
/// list's `modCount` no longer matches. Carries no detail message, exactly as
/// the JDK throws it.
fn comodification() -> Fault {
    Fault::java("ConcurrentModificationException", String::new())
}

/// Check a view against its backing list and return its resolved window.
fn checked_window(id: usize) -> Result<(usize, usize, usize), Fault> {
    let (root, offset, len) =
        resolve_window(id).ok_or_else(|| Fault::internal("javars: dangling subList view"))?;
    let exp = HEAP.with(|h| match h.borrow().get(id) {
        Some(HostObj::SubList { exp_mods, .. }) => Some(*exp_mods),
        _ => None,
    });
    if exp != list_mods(root) {
        return Err(comodification());
    }
    Ok((root, offset, len))
}

/// `list.subList(from, to)` on a list **or** on another view. The result is a
/// view of the receiver, so a nested `subList` composes offsets rather than
/// copying. Bounds are Java's, including its two distinct failures: a
/// out-of-range endpoint is an `IndexOutOfBoundsException` naming the offending
/// index, and a reversed range is an `IllegalArgumentException`.
fn make_sublist(id: usize, from: i64, to: i64) -> Result<Value, Fault> {
    let (root, size) = if is_sublist(id) {
        let (root, _, len) = checked_window(id)?;
        (root, len)
    } else {
        let len = HEAP
            .with(|h| match h.borrow().get(id) {
                Some(HostObj::List { items, .. }) => Some(items.len()),
                _ => None,
            })
            .ok_or_else(|| Fault::internal("javars: `subList` needs a List receiver"))?;
        (id, len)
    };
    if from < 0 {
        return Err(Fault::java(
            "IndexOutOfBoundsException",
            format!("fromIndex = {from}"),
        ));
    }
    if to > size as i64 {
        return Err(Fault::java(
            "IndexOutOfBoundsException",
            format!("toIndex = {to}"),
        ));
    }
    if from > to {
        return Err(Fault::java(
            "IllegalArgumentException",
            format!("fromIndex({from}) > toIndex({to})"),
        ));
    }
    let exp_mods = list_mods(root).unwrap_or_default();
    Ok(Value::Obj(heap_alloc(HostObj::SubList {
        parent: id as u32,
        offset: from as usize,
        len: (to - from) as usize,
        exp_mods,
    })))
}

/// Run an ordinary `List` method against a view's window of its backing list.
///
/// The window is lifted out, the shared [`list_method`] runs on it — so a view
/// answers `get`/`set`/`add`/`remove`/`contains`/`indexOf`/`equals` exactly as
/// a list does — and the result is spliced back, which is what makes a write
/// through the view land in the backing list.
fn sublist_method(
    id: usize,
    method: &str,
    args: &[Value],
    arg_seqs: &[Option<Vec<Value>>],
) -> Result<Value, Fault> {
    let (root, offset, len) = checked_window(id)?;
    let (mut window, fixed) = HEAP
        .with(|h| match h.borrow().get(root) {
            Some(HostObj::List { items, fixed, .. }) => {
                Some((items[offset..offset + len].to_vec(), *fixed))
            }
            _ => None,
        })
        .ok_or_else(|| Fault::internal("javars: dangling subList backing"))?;
    let out = list_method(&mut window, fixed, method, args, arg_seqs)?;
    let delta = window.len() as isize - len as isize;
    // The splice is unconditional: `set` rewrites an element without changing
    // the length, and that write has to reach the backing list too.
    HEAP.with(|h| {
        if let Some(HostObj::List { items, mods, .. }) = h.borrow_mut().get_mut(root) {
            items.splice(offset..offset + len, window);
            if delta != 0 {
                *mods += 1;
            }
        }
    });
    if delta != 0 {
        let new_mods = list_mods(root).unwrap_or_default();
        resize_ancestors(id, delta, new_mods);
    }
    Ok(match out {
        NewColl::Value(v) => v,
        NewColl::Alloc(obj) => Value::Obj(heap_alloc(obj)),
    })
}

/// Push a view's length change up its own ancestor chain and re-snapshot each
/// one's `modCount` — Java's `SubList.updateSizeAndModCount`. The views on the
/// path stay usable; every other outstanding view of the same list does not,
/// which is the behaviour that makes the stale one throw.
fn resize_ancestors(id: usize, delta: isize, new_mods: u64) {
    HEAP.with(|h| {
        let mut heap = h.borrow_mut();
        let mut cur = id;
        for _ in 0..=heap.len() {
            match heap.get_mut(cur) {
                Some(HostObj::SubList {
                    parent,
                    len,
                    exp_mods,
                    ..
                }) => {
                    *len = len.saturating_add_signed(delta);
                    *exp_mods = new_mods;
                    cur = *parent as usize;
                }
                _ => return,
            }
        }
    });
}

/// Replace a list's contents in place, optionally counting the write as a
/// structural modification. Through a view the elements land in its window.
fn write_sequence(id: usize, items: Vec<Value>, structural: bool) -> Result<(), Fault> {
    let (root, offset, len) = if is_sublist(id) {
        checked_window(id)?
    } else {
        (id, 0, usize::MAX)
    };
    HEAP.with(|h| {
        if let Some(HostObj::List {
            items: dst, mods, ..
        }) = h.borrow_mut().get_mut(root)
        {
            let end = len.min(dst.len().saturating_sub(offset)) + offset;
            dst.splice(offset..end, items);
            if structural {
                *mods += 1;
            }
        }
    });
    Ok(())
}

/// The presentation order of a `Map` handle.
fn map_order(v: &Value) -> Order {
    let Value::Obj(id) = v else {
        return Order::Insertion;
    };
    HEAP.with(|h| match h.borrow().get(*id as usize) {
        Some(HostObj::Map { order, .. }) => *order,
        _ => Order::Insertion,
    })
}

/// `Collections.sort(list, cmp)` / `list.sort(cmp)` — a stable sort driven by a
/// comparator closure, matching Java's stable `List.sort`. A `null` comparator
/// is natural order, exactly as Java specifies it.
fn sort_with(vm: &mut VM, mut items: Vec<Value>, cmp: &Value) -> Result<Vec<Value>, Fault> {
    if matches!(cmp, Value::Undef) {
        items.sort_by(natural_cmp);
        return Ok(items);
    }
    if closure_meta(cmp).is_none() {
        return Err(Fault::internal("javars: `sort` needs a Comparator lambda"));
    }
    // `sort_by` needs a total order it can trust; a user comparator may not give
    // one, so a bottom-up merge sort is used instead — stable (which `List.sort`
    // is specified to be), n log n, and it can never panic on an inconsistent
    // comparator the way `sort_by` can. Every sort that names no comparator now
    // arrives here too, because the compiler supplies `(a, b) -> a.compareTo(b)`
    // for those (see `natural_order_comparator`), so this is the one sort in the
    // frontend and its cost is the one that matters.
    let n = items.len();
    let mut buf: Vec<Value> = items.clone();
    let mut width = 1;
    while width < n {
        let mut lo = 0;
        while lo < n {
            let mid = (lo + width).min(n);
            let hi = (lo + 2 * width).min(n);
            let (mut i, mut j) = (lo, mid);
            for slot in &mut buf[lo..hi] {
                // Take from the left run unless the right one compares strictly
                // smaller — the tie going left is what makes the merge stable.
                let take_left = if i >= mid {
                    false
                } else if j >= hi {
                    true
                } else {
                    invoke_closure(vm, cmp, &[items[j].clone(), items[i].clone()]).to_int() >= 0
                };
                if take_left {
                    *slot = items[i].clone();
                    i += 1;
                } else {
                    *slot = items[j].clone();
                    j += 1;
                }
            }
            lo = hi;
        }
        std::mem::swap(&mut items, &mut buf);
        width *= 2;
    }
    Ok(items)
}

/// Invoke `clo` with `args` through the closure-call path, discarding the arity
/// bookkeeping the builtin ABI would otherwise do on the stack.
fn invoke_closure(vm: &mut VM, clo: &Value, args: &[Value]) -> Value {
    vm.stack.push(clo.clone());
    for a in args {
        vm.stack.push(a.clone());
    }
    b_closure_call(vm, args.len() as u8 + 1)
}

/// `java.util.List` methods.
fn list_method(
    items: &mut Vec<Value>,
    fixed: Fixity,
    method: &str,
    args: &[Value],
    arg_seqs: &[Option<Vec<Value>>],
) -> Result<NewColl, Fault> {
    // A structural change to `Arrays.asList` / `List.of` is Java's
    // `UnsupportedOperationException`, not a silent success.
    let structural = || match fixed {
        Fixity::Mutable => Ok(()),
        _ => Err(Fault::java("UnsupportedOperationException", String::new())),
    };
    let replace = || match fixed {
        Fixity::Immutable => Err(Fault::java("UnsupportedOperationException", String::new())),
        _ => Ok(()),
    };
    let bounds = |i: i64, len: usize| -> Result<usize, Fault> {
        if i < 0 || i as usize >= len {
            return Err(Fault::java(
                "IndexOutOfBoundsException",
                format!("Index {i} out of bounds for length {len}"),
            ));
        }
        Ok(i as usize)
    };
    let v = match (method, args.len()) {
        ("size", 0) => Value::Int(items.len() as i64),
        ("isEmpty", 0) => Value::bool(items.is_empty()),
        ("add", 1) => {
            structural()?;
            items.push(args[0].clone());
            Value::bool(true)
        }
        ("add", 2) => {
            structural()?;
            let at = args[0].to_int();
            if at < 0 || at as usize > items.len() {
                return Err(Fault::java(
                    "IndexOutOfBoundsException",
                    format!("Index: {at}, Size: {}", items.len()),
                ));
            }
            items.insert(at as usize, args[1].clone());
            Value::Undef
        }
        ("get", 1) => {
            let i = bounds(args[0].to_int(), items.len())?;
            items[i].clone()
        }
        ("set", 2) => {
            replace()?;
            let i = bounds(args[0].to_int(), items.len())?;
            std::mem::replace(&mut items[i], args[1].clone())
        }
        // `List.remove(int)` removes by index — the overload Java picks for an
        // integral argument. The `remove(Object)` overload arrives under the
        // distinct name `removeObject`, chosen by the compiler from the
        // argument's static type (the same question Java answers statically),
        // because a boxed `Integer` and an `int` are one value here.
        ("remove", 1) => {
            structural()?;
            let i = bounds(args[0].to_int(), items.len())?;
            items.remove(i)
        }
        // `List.remove(Object)` — removes the first element equal to the
        // argument and answers whether one was found.
        ("removeObject", 1) => {
            structural()?;
            match items.iter().position(|x| value_eq(x, &args[0])) {
                Some(i) => {
                    items.remove(i);
                    Value::bool(true)
                }
                None => Value::bool(false),
            }
        }
        ("clear", 0) => {
            structural()?;
            items.clear();
            Value::Undef
        }
        ("contains", 1) => Value::bool(items.iter().any(|x| value_eq(x, &args[0]))),
        ("indexOf", 1) => Value::Int(
            items
                .iter()
                .position(|x| value_eq(x, &args[0]))
                .map_or(-1, |i| i as i64),
        ),
        ("lastIndexOf", 1) => Value::Int(
            items
                .iter()
                .rposition(|x| value_eq(x, &args[0]))
                .map_or(-1, |i| i as i64),
        ),
        ("addAll", 1) => {
            structural()?;
            let add = arg_seqs[0].clone().unwrap_or_default();
            let changed = !add.is_empty();
            items.extend(add);
            Value::bool(changed)
        }
        ("equals", 1) => {
            let other = arg_seqs[0].clone().unwrap_or_default();
            Value::bool(
                other.len() == items.len() && items.iter().zip(&other).all(|(a, b)| value_eq(a, b)),
            )
        }
        _ => {
            return Err(Fault::internal(format!(
                "javars: unsupported List method `{method}` with {} argument(s)",
                args.len()
            )))
        }
    };
    Ok(NewColl::Value(v))
}

/// `java.util.Map` methods.
fn map_method(
    entries: &mut Vec<(Value, Value)>,
    order: Order,
    method: &str,
    args: &[Value],
) -> Result<NewColl, Fault> {
    let find =
        |entries: &Vec<(Value, Value)>, k: &Value| entries.iter().position(|(x, _)| value_eq(x, k));
    let out = match (method, args.len()) {
        ("size", 0) => NewColl::Value(Value::Int(entries.len() as i64)),
        ("isEmpty", 0) => NewColl::Value(Value::bool(entries.is_empty())),
        // A re-`put` keeps the entry's original insertion position, which is
        // what Java's linked/bucket layouts both do.
        ("put", 2) => NewColl::Value(match find(entries, &args[0]) {
            Some(i) => std::mem::replace(&mut entries[i].1, args[1].clone()),
            None => {
                entries.push((args[0].clone(), args[1].clone()));
                Value::Undef
            }
        }),
        ("putIfAbsent", 2) => NewColl::Value(match find(entries, &args[0]) {
            Some(i) => entries[i].1.clone(),
            None => {
                entries.push((args[0].clone(), args[1].clone()));
                Value::Undef
            }
        }),
        ("get", 1) => {
            NewColl::Value(find(entries, &args[0]).map_or(Value::Undef, |i| entries[i].1.clone()))
        }
        ("getOrDefault", 2) => NewColl::Value(
            find(entries, &args[0]).map_or_else(|| args[1].clone(), |i| entries[i].1.clone()),
        ),
        ("containsKey", 1) => NewColl::Value(Value::bool(find(entries, &args[0]).is_some())),
        ("containsValue", 1) => NewColl::Value(Value::bool(
            entries.iter().any(|(_, v)| value_eq(v, &args[0])),
        )),
        ("remove", 1) => NewColl::Value(match find(entries, &args[0]) {
            Some(i) => entries.remove(i).1,
            None => Value::Undef,
        }),
        ("clear", 0) => {
            entries.clear();
            NewColl::Value(Value::Undef)
        }
        ("keySet", 0) => {
            let keys: Vec<Value> = entries.iter().map(|(k, _)| k.clone()).collect();
            let ordered = present_order(&keys, order)
                .into_iter()
                .map(|i| keys[i].clone())
                .collect();
            // The view is a `Set` that already holds the map's order, so it
            // iterates and prints exactly as the map does.
            NewColl::Alloc(HostObj::Set {
                items: ordered,
                order: Order::Insertion,
                // A `keySet` view is writable in Java (a removal writes through
                // to the map); javars models it as a copy, so it is at least not
                // an immutable one.
                fixed: Fixity::Mutable,
            })
        }
        ("values", 0) => {
            let keys: Vec<Value> = entries.iter().map(|(k, _)| k.clone()).collect();
            let ordered = present_order(&keys, order)
                .into_iter()
                .map(|i| entries[i].1.clone())
                .collect();
            NewColl::Alloc(HostObj::List {
                mods: 0,
                items: ordered,
                fixed: Fixity::FixedSize,
            })
        }
        _ => {
            return Err(Fault::internal(format!(
                "javars: unsupported Map method `{method}` with {} argument(s)",
                args.len()
            )))
        }
    };
    Ok(out)
}

/// `java.util.Set` methods.
fn set_method(
    items: &mut Vec<Value>,
    fixed: Fixity,
    method: &str,
    args: &[Value],
    arg_seqs: &[Option<Vec<Value>>],
) -> Result<NewColl, Fault> {
    // A structural change to a `Set.of` is Java's `UnsupportedOperationException`,
    // not a silent success — the same rule `list_method` applies to `List.of`.
    // Java throws before deciding whether the change was a no-op, so the guard
    // runs before the membership test rather than after it.
    let structural = || match fixed {
        Fixity::Mutable => Ok(()),
        _ => Err(Fault::java("UnsupportedOperationException", String::new())),
    };
    let v = match (method, args.len()) {
        ("size", 0) => Value::Int(items.len() as i64),
        ("isEmpty", 0) => Value::bool(items.is_empty()),
        ("add", 1) => {
            structural()?;
            if items.iter().any(|x| value_eq(x, &args[0])) {
                Value::bool(false)
            } else {
                items.push(args[0].clone());
                Value::bool(true)
            }
        }
        ("contains", 1) => Value::bool(items.iter().any(|x| value_eq(x, &args[0]))),
        ("remove", 1) => {
            structural()?;
            match items.iter().position(|x| value_eq(x, &args[0])) {
                Some(i) => {
                    items.remove(i);
                    Value::bool(true)
                }
                None => Value::bool(false),
            }
        }
        ("clear", 0) => {
            structural()?;
            items.clear();
            Value::Undef
        }
        ("addAll", 1) => {
            structural()?;
            let mut changed = false;
            for v in arg_seqs[0].clone().unwrap_or_default() {
                if !items.iter().any(|x| value_eq(x, &v)) {
                    items.push(v);
                    changed = true;
                }
            }
            Value::bool(changed)
        }
        _ => {
            return Err(Fault::internal(format!(
                "javars: unsupported Set method `{method}` with {} argument(s)",
                args.len()
            )))
        }
    };
    Ok(NewColl::Value(v))
}

/// `[a, b, c]` — `AbstractCollection.toString`.
fn render_sequence(items: &[Value]) -> String {
    let body: Vec<String> = items.iter().map(java_str).collect();
    format!("[{}]", body.join(", "))
}

fn render_set(items: &[Value], order: Order) -> String {
    let ordered: Vec<Value> = present_order(items, order)
        .into_iter()
        .map(|i| items[i].clone())
        .collect();
    render_sequence(&ordered)
}

/// `{k=v, k=v}` — `AbstractMap.toString`.
fn render_map(entries: &[(Value, Value)], order: Order) -> String {
    let keys: Vec<Value> = entries.iter().map(|(k, _)| k.clone()).collect();
    let body: Vec<String> = present_order(&keys, order)
        .into_iter()
        .map(|i| format!("{}={}", java_str(&entries[i].0), java_str(&entries[i].1)))
        .collect();
    format!("{{{}}}", body.join(", "))
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
    // A lambda whose static type the compiler could not pin down lands here —
    // the erasure of a nested generic (`Supplier<Supplier<String>>.get()` is
    // declared to return `Object`) is the common case. Any method call on a
    // closure receiver is its single abstract method, because `javac` has
    // already rejected every other name, so it is invoked directly.
    if closure_meta(&recv).is_some() {
        vm.stack.push(recv);
        for a in args {
            vm.stack.push(a);
        }
        return b_closure_call(vm, n as u8 + 1);
    }
    // A collection receiver whose static type the compiler could not pin down
    // (an erased `Map.get` result, say) routes to the collection methods.
    if is_collection(&recv) {
        return coll_method(vm, &recv, &method, &args);
    }
    // A class instance whose own class declares no such method inherits
    // `java.lang.Object`'s — including `new Object()` itself, which has no class
    // body at all.
    if let Some(v) = object_method(&recv, &method, &args) {
        return v;
    }
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

/// The three `java.lang.Object` methods that have an answer without a class
/// body, for a class-instance receiver that declares no override:
///
///   * `equals(x)` is reference identity — the same heap handle.
///   * `hashCode()` is the identity hash. Java's is a JVM value that is not
///     reproducible across runs, so javars uses the heap handle: the properties
///     a program can rely on (stable within a run, equal for equal references)
///     hold, and the number itself is no more portable than Java's.
///   * `toString()` is `getClass().getName() + "@" + Integer.toHexString(hash)`.
///
/// `None` for any other method and for any non-instance handle (an array, a
/// collection, and a closure each have their own dispatch), so the caller falls
/// through to the `String` methods exactly as before.
fn object_method(recv: &Value, method: &str, args: &[Value]) -> Option<Value> {
    let Value::Obj(id) = recv else {
        return None;
    };
    let is_instance =
        HEAP.with(|h| matches!(h.borrow().get(*id as usize), Some(HostObj::Instance { .. })));
    if !is_instance {
        return None;
    }
    match (method, args.len()) {
        ("equals", 1) => Some(Value::bool(
            matches!(args[0], Value::Obj(other) if other == *id),
        )),
        ("hashCode", 0) => Some(Value::Int(i64::from(*id))),
        ("toString", 0) => Some(Value::str(obj_default_str(*id))),
        _ => None,
    }
}

/// `String.compareTo` / `compareToIgnoreCase`: the difference of the first
/// differing `char`, else the length difference. Java compares UTF-16 code
/// units; javars compares Unicode scalars, the same `char` simplification the
/// index-based methods make.
fn compare_strings(a: &str, b: &str, fold_case: bool) -> i64 {
    let norm = |s: &str| -> Vec<char> {
        if fold_case {
            s.chars().flat_map(|c| c.to_lowercase()).collect()
        } else {
            s.chars().collect()
        }
    };
    let (x, y) = (norm(a), norm(b));
    for (ca, cb) in x.iter().zip(&y) {
        if ca != cb {
            return *ca as i64 - *cb as i64;
        }
    }
    x.len() as i64 - y.len() as i64
}

/// `Double.compare`/`Float.compare`: the numeric order, except that it is a
/// *total* order — `NaN` compares greater than every other value including
/// itself, and `-0.0` compares less than `0.0`. Java gets both by falling back
/// to the raw bit pattern once `<` and `>` have both said no, which is what
/// `total_cmp` does; ordering by `f64` bits agrees with ordering by `f32` bits
/// on every value a `float` can hold, so one routine serves both.
fn double_compare(a: f64, b: f64) -> i64 {
    match a.total_cmp(&b) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

/// [`JCOMPARE_TO`] — `compareTo` on a boxed primitive or a `String`.
fn b_compare_to(vm: &mut VM, _argc: u8) -> Value {
    let tag = vm
        .stack
        .pop()
        .map(|v| v.as_str_cow().into_owned())
        .unwrap_or_default();
    let b = vm.stack.pop().unwrap_or(Value::Undef);
    let a = vm.stack.pop().unwrap_or(Value::Undef);
    // Java's own `Integer.compareTo(null)` is an NPE, and so is a call on a
    // `null` receiver. Both report the same way every other javars NPE does.
    if matches!(a, Value::Undef) {
        return raise(
            vm,
            Fault::java(
                "NullPointerException",
                "Cannot invoke \"java.lang.Comparable.compareTo(Object)\" because the receiver is null",
            ),
        );
    }
    // A class instance here means no user class declares a one-argument
    // `compareTo`, so there is no body to run — `javac` would have rejected the
    // call. Say so rather than comparing the object's default `toString`.
    if let Value::Obj(id) = a {
        let class = HEAP.with(|h| match h.borrow().get(id as usize) {
            Some(HostObj::Instance { class, .. }) => Some(class.clone()),
            _ => None,
        });
        if let Some(class) = class {
            return raise(
                vm,
                Fault::internal(format!(
                    "javars: class `{class}` does not declare `compareTo`"
                )),
            );
        }
    }
    Value::Int(match tag.as_str() {
        // Sign only: `Integer.compare` is `(x < y) ? -1 : ((x == y) ? 0 : 1)`.
        "Integer" | "int" | "Long" | "long" => as_i64(&a).cmp(&as_i64(&b)) as i64,
        // Difference: `Character.compareTo` is `this.value - other.value`, and
        // `Byte`/`Short` are the same subtraction. `as_i64` reads a boxed
        // `Character` in either of the two shapes javars stores it in — the code
        // point, and the one-character String a collection element carries.
        "Character" | "char" | "Short" | "short" | "Byte" | "byte" => as_i64(&a) - as_i64(&b),
        "Double" | "double" | "Float" | "float" => double_compare(as_f64(&a), as_f64(&b)),
        "Boolean" | "boolean" => i64::from(a.is_truthy()) - i64::from(b.is_truthy()),
        "String" | "CharSequence" => compare_strings(&a.as_str_cow(), &b.as_str_cow(), false),
        // The compiler could not name the receiver's type — an erased
        // `List.get` result is the usual reason. The runtime values decide, and
        // `Integer` is the reading a bare integer gets, being the box a literal
        // autoboxes to.
        _ => match (&a, &b) {
            (Value::Str(_), _) | (_, Value::Str(_)) => {
                compare_strings(&a.as_str_cow(), &b.as_str_cow(), false)
            }
            (Value::Float(_), _) | (_, Value::Float(_)) => double_compare(as_f64(&a), as_f64(&b)),
            (Value::Bool(_), Value::Bool(_)) => i64::from(a.is_truthy()) - i64::from(b.is_truthy()),
            _ => as_i64(&a).cmp(&as_i64(&b)) as i64,
        },
    })
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
        // `String.compareTo` is specified as the difference of the first
        // differing character, else the length difference — not merely its sign,
        // which is why a lexicographic `Ord` cannot stand in for it.
        ("compareTo", 1) => Ok(Value::Int(compare_strings(s, &args[0].as_str_cow(), false))),
        ("compareToIgnoreCase", 1) => {
            Ok(Value::Int(compare_strings(s, &args[0].as_str_cow(), true)))
        }
        // `charAt` returns a `char`, i.e. the code point — `"abc".charAt(2) + 1`
        // is 100, not "c1". The compiler converts it back to a String wherever
        // Java's string conversion applies.
        ("charAt", 1) => {
            let i = args[0].to_int();
            match usize::try_from(i).ok().and_then(|i| s.chars().nth(i)) {
                Some(c) => Ok(Value::Int(c as i64)),
                None => Err(Fault::java(
                    "StringIndexOutOfBoundsException",
                    format!("Index {i} out of bounds for length {}", char_len()),
                )),
            }
        }
        ("substring", 1) => substring(s, args[0].to_int(), char_len()),
        ("substring", 2) => substring(s, args[0].to_int(), args[1].to_int()),
        ("indexOf", 1) => Ok(Value::Int(char_index_of(s, &args[0].as_str_cow()))),
        // `indexOf(t, from)` starts the search at `from`; the result is still an
        // index into the whole string.
        ("indexOf", 2) => {
            let from = args[1].to_int().max(0) as usize;
            let tail: String = s.chars().skip(from).collect();
            let hit = char_index_of(&tail, &args[0].as_str_cow());
            Ok(Value::Int(if hit < 0 { -1 } else { hit + from as i64 }))
        }
        ("lastIndexOf", 1) => Ok(Value::Int(char_last_index_of(
            s,
            &args[0].as_str_cow(),
            char_len(),
        ))),
        ("lastIndexOf", 2) => Ok(Value::Int(char_last_index_of(
            s,
            &args[0].as_str_cow(),
            args[1].to_int(),
        ))),
        ("codePointAt", 1) => {
            let i = args[0].to_int();
            match usize::try_from(i).ok().and_then(|i| s.chars().nth(i)) {
                Some(c) => Ok(Value::Int(c as i64)),
                None => Err(Fault::java(
                    "StringIndexOutOfBoundsException",
                    format!("Index {i} out of bounds for length {}", char_len()),
                )),
            }
        }
        // `strip` follows Unicode whitespace where `trim` cuts at U+0020; Rust's
        // `trim` is the Unicode one, so `trim` keeps its own ASCII-control rule
        // above and these three use the Unicode definition.
        ("strip", 0) => Ok(Value::str(s.trim().to_string())),
        ("stripLeading", 0) => Ok(Value::str(s.trim_start().to_string())),
        ("stripTrailing", 0) => Ok(Value::str(s.trim_end().to_string())),
        ("isBlank", 0) => Ok(Value::bool(s.trim().is_empty())),
        ("hashCode", 0) => Ok(Value::Int(
            java_hash(&Value::str(s.to_string())).unwrap_or(0).into(),
        )),
        // Interning is unobservable here: javars compares strings by value.
        ("intern", 0) | ("toString", 0) => Ok(Value::str(s.to_string())),
        ("contentEquals", 1) => Ok(Value::bool(s == args[0].as_str_cow().as_ref())),
        // `x.getClass()` evaluates to the runtime class *name*, so `Class`'s own
        // two accessors land here: the simple name is that string, and the
        // qualified name adds `java.lang.` for the modeled JDK types.
        ("getSimpleName", 0) => Ok(Value::str(s.to_string())),
        ("getName", 0) => Ok(Value::str(
            crate::prelude::qualified_class(s).unwrap_or_else(|| s.to_string()),
        )),
        // A `char[]` of code points, matching `charAt` — so `a[i] - 'a'` is
        // arithmetic. `Arrays.toString`/`String.valueOf` of one are routed
        // through [`JCHR_STR`] by the compiler, which knows the element type.
        ("toCharArray", 0) => Ok(Value::Obj(heap_alloc(HostObj::Array(
            s.chars().map(|c| Value::Int(c as i64)).collect(),
        )))),
        // `"%s".formatted(x)` is `String.format("%s", x)` with the receiver as
        // the format string.
        ("formatted", _) => java_format(s, args, &[]),
        // The four `java.util.regex` methods, on the engine in `crate::regex`.
        // `split(regex)` is `split(regex, 0)`: trailing empty fields are dropped
        // (interior ones are not), and a no-match returns the whole input.
        ("split", 1) | ("split", 2) => {
            let compiled = crate::regex::compile(&args[0].as_str_cow());
            let pat = compiled.as_ref().as_ref().map_err(|e| pattern_fault(e))?;
            let limit = args.get(1).map_or(0, Value::to_int);
            let parts = pat.split(s, limit).map_err(engine_fault)?;
            Ok(Value::Obj(heap_alloc(HostObj::Array(
                parts.into_iter().map(Value::str).collect(),
            ))))
        }
        ("replaceAll", 2) | ("replaceFirst", 2) => {
            let compiled = crate::regex::compile(&args[0].as_str_cow());
            let pat = compiled.as_ref().as_ref().map_err(|e| pattern_fault(e))?;
            let out = pat
                .replace(s, &args[1].as_str_cow(), method == "replaceFirst")
                .map_err(replacement_fault)?;
            Ok(Value::str(out))
        }
        // `matches` matches the whole input, not a substring of it.
        ("matches", 1) => {
            let compiled = crate::regex::compile_whole(&args[0].as_str_cow());
            let pat = compiled.as_ref().as_ref().map_err(|e| pattern_fault(e))?;
            Ok(Value::bool(pat.matches_whole(s).map_err(engine_fault)?))
        }
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
    // The collection statics come first: two of them (`Collections.sort` with a
    // comparator) run user code, which `static_method` — which has no VM — could
    // not do.
    match collection_static(vm, &class, &method, &args) {
        Some(Ok(v)) => return v,
        Some(Err(f)) => return raise(vm, f),
        None => {}
    }
    match static_method(&class, &method, &args) {
        Ok(v) => v,
        Err(f) => raise(vm, f),
    }
}

/// The `java.util` statics: `Arrays.asList`, `List.of`/`Set.of`,
/// `Collections.sort`/`reverse`/`max`/`min`. `None` when `Class.method` is not
/// one of them, so the ordinary stdlib statics are reached unchanged.
fn collection_static(
    vm: &mut VM,
    class: &str,
    method: &str,
    args: &[Value],
) -> Option<Result<Value, Fault>> {
    let list = |items: Vec<Value>, fixed: Fixity| {
        Ok(Value::Obj(heap_alloc(HostObj::List {
            items,
            fixed,
            mods: 0,
        })))
    };
    Some(match (class, method) {
        // `Arrays.asList` is a fixed-size *view*: `set` works, `add` throws.
        ("Arrays", "asList") => list(varargs_items(args), Fixity::FixedSize),
        ("List", "of") => list(varargs_items(args), Fixity::Immutable),
        ("Set", "of") => Ok(Value::Obj(heap_alloc(HostObj::Set {
            items: distinct(&varargs_items(args)),
            order: Order::Hash,
            fixed: Fixity::Immutable,
        }))),
        ("Collections", "sort") if !args.is_empty() => {
            let items = match sequence_items(&args[0]) {
                Some(i) => i,
                None => {
                    return Some(Err(Fault::internal(
                        "javars: `Collections.sort` needs a List",
                    )))
                }
            };
            let cmp = args.get(1).cloned().unwrap_or(Value::Undef);
            sort_with(vm, items, &cmp)
                .and_then(|sorted| write_list(&args[0], sorted))
                .map(|()| Value::Undef)
        }
        ("Collections", "reverse") if args.len() == 1 => {
            let mut items = sequence_items(&args[0]).unwrap_or_default();
            items.reverse();
            write_list(&args[0], items).map(|()| Value::Undef)
        }
        ("Collections", "max") | ("Collections", "min") if args.len() == 1 => {
            let items = sequence_items(&args[0]).unwrap_or_default();
            let pick = if method == "max" {
                items.iter().max_by(|a, b| natural_cmp(a, b))
            } else {
                items.iter().min_by(|a, b| natural_cmp(a, b))
            };
            match pick {
                Some(v) => Ok(v.clone()),
                None => Err(Fault::java("NoSuchElementException", String::new())),
            }
        }
        _ => return None,
    })
}

/// The elements a varargs static receives. A lone *array* argument spreads —
/// `Arrays.asList(strArray)` is a list of the array's elements, not a
/// one-element list holding the array — which is what Java's varargs does for
/// every reference array. A lone `List`/`Set` argument does not spread, matching
/// Java exactly.
fn varargs_items(args: &[Value]) -> Vec<Value> {
    if let [Value::Obj(id)] = args {
        let spread = HEAP.with(|h| match h.borrow().get(*id as usize) {
            Some(HostObj::Array(items)) => Some(items.clone()),
            _ => None,
        });
        if let Some(items) = spread {
            return items;
        }
    }
    args.to_vec()
}

/// Overwrite a `List` handle's elements in place, so a sort or reverse is
/// visible through every reference to it — Java's semantics for these statics.
fn write_list(target: &Value, items: Vec<Value>) -> Result<(), Fault> {
    let Value::Obj(id) = target else {
        return Ok(());
    };
    // `Collections.sort`/`reverse` reorder in place; Java counts both as
    // structural modifications, so an outstanding `subList` view of the target
    // goes stale exactly as it does there.
    write_sequence(*id as usize, items, true)
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
        // `Math.floorDiv`/`floorMod` round toward negative infinity, unlike `/`
        // and `%` which truncate toward zero: `floorDiv(-7, 2)` is -4.
        ("Math", "floorDiv", 2) => floor_div(args[0].to_int(), args[1].to_int()).map(Value::Int),
        ("Math", "floorMod", 2) => {
            let (a, b) = (args[0].to_int(), args[1].to_int());
            floor_div(a, b).map(|q| Value::Int(a - q * b))
        }
        ("Math", "signum", 1) => Ok(Value::float(match args[0].to_float() {
            f if f > 0.0 => 1.0,
            f if f < 0.0 => -1.0,
            f => f,
        })),
        // The transcendentals (`sin`/`cos`/`tan`/`atan`/`atan2`/`exp`/`log`/
        // `log10`/`cbrt`/`hypot`) are deliberately absent. The JDK permits them
        // a 1-ulp error and answers from its own fdlibm-derived implementation,
        // which Rust's libm does not reproduce bit-for-bit: a 180-value
        // differential sweep against OpenJDK 26 diverged in the last digit for
        // every one of them (`sin` 14/180, `cbrt` 25/180, `tan` 5/10). An
        // unregistered method is a clear error; a silently different last digit
        // is not, so they stay out until a StrictMath port can answer exactly.
        ("Math", "toRadians", 1) => Ok(Value::float(args[0].to_float().to_radians())),
        ("Math", "toDegrees", 1) => Ok(Value::float(args[0].to_float().to_degrees())),

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
        // The unsigned radix renderings read the value as a *bit pattern* at its
        // declared width — `Integer.toHexString(-1)` is "ffffffff" and
        // `Long.toHexString(-1L)` is sixteen f's.
        ("Integer", "toBinaryString", 1) => {
            Ok(Value::str(format!("{:b}", args[0].to_int() as i32 as u32)))
        }
        ("Integer", "toHexString", 1) => {
            Ok(Value::str(format!("{:x}", args[0].to_int() as i32 as u32)))
        }
        ("Integer", "toOctalString", 1) => {
            Ok(Value::str(format!("{:o}", args[0].to_int() as i32 as u32)))
        }
        ("Long", "toBinaryString", 1) => Ok(Value::str(format!("{:b}", args[0].to_int() as u64))),
        ("Long", "toHexString", 1) => Ok(Value::str(format!("{:x}", args[0].to_int() as u64))),
        ("Long", "toOctalString", 1) => Ok(Value::str(format!("{:o}", args[0].to_int() as u64))),
        ("Integer" | "Long", "compare", 2) => Ok(Value::Int(cmp_to_int(
            args[0].to_int().cmp(&args[1].to_int()),
        ))),
        ("Integer" | "Long", "max", 2) => Ok(Value::Int(args[0].to_int().max(args[1].to_int()))),
        ("Integer" | "Long", "min", 2) => Ok(Value::Int(args[0].to_int().min(args[1].to_int()))),
        ("Integer" | "Long", "sum", 2) => {
            Ok(Value::Int(args[0].to_int().wrapping_add(args[1].to_int())))
        }
        ("Integer" | "Long", "signum", 1) => Ok(Value::Int(args[0].to_int().signum())),
        ("Long", "toString", 1) => Ok(Value::str(args[0].to_int().to_string())),
        ("Long", "valueOf", 1) => match &args[0] {
            Value::Str(s) => parse_int_radix(s, 10, false),
            other => Ok(Value::Int(other.to_int())),
        },

        // ── java.lang.Double ──
        ("Double", "parseDouble", 1) | ("Double", "valueOf", 1) => {
            let s = args[0].as_str_cow();
            match s.trim().parse::<f64>() {
                Ok(f) => Ok(Value::float(f)),
                Err(_) => Err(Fault::java(
                    "java.lang.NumberFormatException",
                    // Java quotes the offending text but not the word `For`.
                    format!("For input string: \"{s}\""),
                )),
            }
        }
        ("Double", "toString", 1) => Ok(Value::str(format_double(args[0].to_float()))),

        // ── java.lang.Float ──
        // Every one of these answers at 32-bit precision, which is the whole
        // reason they are not aliases of the `Double` arm above.
        ("Float", "toString", 1) => Ok(Value::str(format_float(args[0].to_float() as f32))),
        ("Float", "parseFloat", 1) | ("Float", "valueOf", 1) => {
            let s = args[0].as_str_cow();
            match s.trim().parse::<f32>() {
                Ok(f) => Ok(Value::float(f as f64)),
                Err(_) => Err(Fault::java(
                    "NumberFormatException",
                    format!("For input string: \"{s}\""),
                )),
            }
        }
        ("Float", "compare", 2) => Ok(Value::Int(cmp_to_int(
            (args[0].to_float() as f32)
                .partial_cmp(&(args[1].to_float() as f32))
                .unwrap_or(std::cmp::Ordering::Equal),
        ))),
        ("Float", "isNaN", 1) => Ok(Value::bool(args[0].to_float().is_nan())),
        ("Float", "isInfinite", 1) => Ok(Value::bool(args[0].to_float().is_infinite())),
        ("Double", "compare", 2) => Ok(Value::Int(cmp_to_int(
            args[0]
                .to_float()
                .partial_cmp(&args[1].to_float())
                .unwrap_or(std::cmp::Ordering::Equal),
        ))),
        ("Double", "isNaN", 1) => Ok(Value::bool(args[0].to_float().is_nan())),
        ("Double", "isInfinite", 1) => Ok(Value::bool(args[0].to_float().is_infinite())),

        // ── java.lang.Character ──
        // The argument is a `char` code point (`char_arg` also accepts the
        // one-character String a boxed `Character` is). `toUpperCase`/
        // `toLowerCase` return a `char`, so they return a code point too.
        ("Character", "isDigit", 1) => Ok(Value::bool(char_arg(&args[0]).is_ascii_digit())),
        ("Character", "isLetter", 1) => Ok(Value::bool(char_arg(&args[0]).is_alphabetic())),
        ("Character", "isLetterOrDigit", 1) => {
            Ok(Value::bool(char_arg(&args[0]).is_alphanumeric()))
        }
        ("Character", "isWhitespace", 1) => Ok(Value::bool(char_arg(&args[0]).is_whitespace())),
        ("Character", "isUpperCase", 1) => Ok(Value::bool(char_arg(&args[0]).is_uppercase())),
        ("Character", "isLowerCase", 1) => Ok(Value::bool(char_arg(&args[0]).is_lowercase())),
        // Java's `Character.toUpperCase(char)` is a *one-to-one* code-point map:
        // a character whose full uppercasing is multi-character (`ß`) is left
        // alone, unlike `String.toUpperCase`.
        ("Character", "toUpperCase", 1) => Ok(Value::Int(one_to_one_case(
            char_arg(&args[0]),
            char::to_uppercase,
        ))),
        ("Character", "toLowerCase", 1) => Ok(Value::Int(one_to_one_case(
            char_arg(&args[0]),
            char::to_lowercase,
        ))),
        ("Character", "toString", 1) => Ok(Value::str(char_arg(&args[0]).to_string())),
        ("Character", "getNumericValue", 1) => Ok(Value::Int(
            char_arg(&args[0]).to_digit(36).map_or(-1, i64::from),
        )),

        // ── java.lang.Boolean ──
        ("Boolean", "parseBoolean", 1) => Ok(Value::bool(
            args[0].as_str_cow().eq_ignore_ascii_case("true"),
        )),

        // ── java.lang.String ──
        // `String.valueOf(x)` renders any value with Java's `println` rules —
        // except a `char[]`, whose overload concatenates the characters rather
        // than printing the array.
        ("String", "valueOf", 1) => Ok(Value::str(match array_items(&args[0]) {
            Some(items) => items.iter().map(java_str).collect::<String>(),
            None => java_str(&args[0]),
        })),
        // `String.format(fmt, args…)` — printf-style formatting (subset).
        ("String", "format", _) if !args.is_empty() => {
            let fmt = args[0].as_str_cow().into_owned();
            java_format(&fmt, &args[1..], &[])
        }

        // `String.join(sep, a, b, …)`, `String.join(sep, array)`, and
        // `String.join(sep, iterable)` — Java's second overload takes an
        // `Iterable<CharSequence>`, so a `List`/`Set` argument joins its
        // *elements*. Matching only arrays here rendered the collection's own
        // `toString` as one part (`String.join("-", List.of("a","b"))` gave
        // `[a, b]` instead of `a-b`).
        ("String", "join", n) if n >= 2 => {
            let sep = args[0].as_str_cow().into_owned();
            let parts: Vec<String> = match (sequence_items(&args[1]), n) {
                (Some(items), 2) => items.iter().map(java_str).collect(),
                _ => args[1..].iter().map(java_str).collect(),
            };
            Ok(Value::str(parts.join(&sep)))
        }

        // ── java.lang.Boolean ──
        ("Boolean", "toString", 1) => Ok(Value::str(java_str(&args[0]))),
        ("Boolean", "compare", 2) => Ok(Value::Int(cmp_to_int(
            args[0].is_truthy().cmp(&args[1].is_truthy()),
        ))),

        // ── java.util.Arrays ──
        // `Arrays.toString(a)` — shallow `[e0, e1, …]` (null → "null").
        ("Arrays", "toString", 1) => Ok(Value::str(arrays_to_string(&args[0]))),
        // `Arrays.deepToString(a)` recurses into nested arrays, which is what a
        // rectangular `int[][]` needs.
        ("Arrays", "deepToString", 1) => Ok(Value::str(arrays_deep_to_string(&args[0]))),
        // `Arrays.sort(a)` sorts in place, so it mutates the heap array and
        // returns nothing.
        ("Arrays", "sort", 1) => {
            array_mutate(&args[0], |a| a.sort_by(natural_cmp))?;
            Ok(Value::Undef)
        }
        ("Arrays", "fill", 2) => {
            let v = args[1].clone();
            array_mutate(&args[0], |a| a.fill(v))?;
            Ok(Value::Undef)
        }
        ("Arrays", "equals", 2) => match (array_items(&args[0]), array_items(&args[1])) {
            (Some(x), Some(y)) => Ok(Value::bool(
                x.len() == y.len()
                    && x.iter()
                        .zip(y.iter())
                        .all(|(p, q)| natural_cmp(p, q) == std::cmp::Ordering::Equal),
            )),
            _ => Ok(Value::bool(false)),
        },
        // `Arrays.copyOf` pads with the element type's default when it grows.
        // javars erases the element type at runtime, so the pad is inferred from
        // element 0's kind — the only evidence available — and is `null` for an
        // empty source.
        ("Arrays", "copyOf", 2) => {
            let items = array_items(&args[0]).unwrap_or_default();
            let n = args[1].to_int().max(0) as usize;
            let pad = element_default(&items);
            let mut out = items;
            out.resize(n, pad);
            Ok(Value::Obj(heap_alloc(HostObj::Array(out))))
        }
        ("Arrays", "copyOfRange", 3) => {
            let items = array_items(&args[0]).unwrap_or_default();
            let (from, to) = (
                args[1].to_int().max(0) as usize,
                args[2].to_int().max(0) as usize,
            );
            let pad = element_default(&items);
            let mut out: Vec<Value> = items
                .get(from..to.min(items.len()))
                .unwrap_or_default()
                .to_vec();
            out.resize(to.saturating_sub(from), pad);
            Ok(Value::Obj(heap_alloc(HostObj::Array(out))))
        }
        // `Arrays.binarySearch` returns `-(insertion point) - 1` when absent,
        // exactly as the JDK does.
        ("Arrays", "binarySearch", 2) => {
            let items = array_items(&args[0]).unwrap_or_default();
            Ok(Value::Int(
                match items.binary_search_by(|p| natural_cmp(p, &args[1])) {
                    Ok(i) => i as i64,
                    Err(i) => -(i as i64) - 1,
                },
            ))
        }
        // `Arrays.hashCode(a)` — the JDK's documented `31 * result + e` fold,
        // seeded at 1, wrapping at 32 bits.
        ("Arrays", "hashCode", 1) => {
            let items = array_items(&args[0]).unwrap_or_default();
            let h = items.iter().fold(1i32, |acc, e| {
                acc.wrapping_mul(31).wrapping_add(java_hash(e).unwrap_or(0))
            });
            Ok(Value::Int(h as i64))
        }

        _ => Err(Fault::internal(format!(
            "javars: unsupported static method `{class}.{method}` with {} argument(s)",
            args.len()
        ))),
    }
}

/// The last index (in characters) at which `needle` starts at or before
/// `from`, or -1. Java's `lastIndexOf` counts an empty needle as `from` itself.
fn char_last_index_of(hay: &str, needle: &str, from: i64) -> i64 {
    let chars: Vec<char> = hay.chars().collect();
    let pat: Vec<char> = needle.chars().collect();
    if pat.is_empty() {
        return from.clamp(0, chars.len() as i64);
    }
    let start = from.min(chars.len() as i64 - pat.len() as i64);
    (0..=start.max(-1))
        .rev()
        .find(|&i| chars[i as usize..].starts_with(&pat))
        .unwrap_or(-1)
}

/// The literal text a `String` regex argument matches, for the pattern subset
/// javars supports.
///
/// javars links no regex engine, so `split`/`replaceAll`/`replaceFirst`/
/// `matches` accept only patterns with no metacharacter — which is what the
/// overwhelmingly common single-separator call is, and which the JDK itself
/// fast-paths. A pattern that would need real matching is reported rather than
/// silently treated as a literal and answered wrong.
fn pattern_fault(msg: &str) -> Fault {
    Fault::java("PatternSyntaxException", msg)
}

/// A replacement string Java rejects: a dangling `\`, a bare `$`, or a
/// reference to a group the pattern does not have. Java raises
/// `IllegalArgumentException` for the malformed forms and
/// `IndexOutOfBoundsException` for the missing group, and the message text
/// distinguishes them.
fn replacement_fault(msg: String) -> Fault {
    if msg.starts_with("No group") {
        Fault::java("IndexOutOfBoundsException", msg)
    } else {
        Fault::java("IllegalArgumentException", msg)
    }
}

/// The matching engine giving up (its backtrack limit), which is not a Java
/// outcome at all — Java would either answer or overflow its own stack. javars
/// reports rather than guessing.
fn engine_fault(msg: String) -> Fault {
    Fault::internal(format!("javars: regular expression failed: {msg}"))
}

/// Java's `Math.floorDiv`: integer division rounded toward negative infinity
/// rather than toward zero, so `floorDiv(-7, 2)` is -4 where `-7 / 2` is -3.
fn floor_div(a: i64, b: i64) -> Result<i64, Fault> {
    if b == 0 {
        return Err(Fault::java("java.lang.ArithmeticException", "/ by zero"));
    }
    let q = a.wrapping_div(b);
    // One correction step when the signs differ and the division was inexact.
    Ok(if a % b != 0 && (a ^ b) < 0 { q - 1 } else { q })
}

/// The `-1`/`0`/`1` an `Integer.compare`-style method returns.
fn cmp_to_int(o: std::cmp::Ordering) -> i64 {
    match o {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

/// The `char` a `Character.*` argument names. javars models a `char` as a
/// one-character string, and a numeric argument is a code point.
/// The code point of `c` after a one-to-one case mapping, or `c` itself when the
/// mapping expands to more than one character. Java's `Character.toUpperCase`
/// works on a single `char` and so has no way to express `ß` → `SS`; it returns
/// the argument unchanged there, where `String.toUpperCase` expands.
fn one_to_one_case<I: Iterator<Item = char>>(c: char, map: fn(char) -> I) -> i64 {
    let mut mapped = map(c);
    match (mapped.next(), mapped.next()) {
        (Some(m), None) => m as i64,
        _ => c as i64,
    }
}

fn char_arg(v: &Value) -> char {
    match v {
        Value::Int(n) => char::from_u32(*n as u32).unwrap_or('\u{0}'),
        other => other.as_str_cow().chars().next().unwrap_or('\u{0}'),
    }
}

/// A copy of the elements of `v` when it is a heap array, else `None`.
fn array_items(v: &Value) -> Option<Vec<Value>> {
    let Value::Obj(id) = v else {
        return None;
    };
    HEAP.with(|h| match h.borrow().get(*id as usize) {
        Some(HostObj::Array(a)) => Some(a.clone()),
        _ => None,
    })
}

/// Run `f` over `v`'s elements *in place* — what the mutating `Arrays` statics
/// (`sort`, `fill`) need, since they return `void` and are observed through the
/// caller's own handle.
fn array_mutate(v: &Value, f: impl FnOnce(&mut Vec<Value>)) -> Result<(), Fault> {
    let Value::Obj(id) = v else {
        return Err(Fault::java(
            "java.lang.NullPointerException",
            "null array".to_string(),
        ));
    };
    HEAP.with(|h| match h.borrow_mut().get_mut(*id as usize) {
        Some(HostObj::Array(a)) => {
            f(a);
            Ok(())
        }
        _ => Err(Fault::java(
            "java.lang.NullPointerException",
            "null array".to_string(),
        )),
    })
}

/// The value `Arrays.copyOf` pads a grown copy with. The element type is erased
/// at runtime, so it is read off element 0: a numeric array pads with its zero,
/// a boolean array with `false`, and anything else (including an empty source)
/// with `null`.
fn element_default(items: &[Value]) -> Value {
    match items.first() {
        Some(Value::Int(_)) => Value::Int(0),
        Some(Value::Float(_)) => Value::float(0.0),
        Some(Value::Bool(_)) => Value::bool(false),
        _ => Value::Undef,
    }
}

/// `Arrays.deepToString(a)` — like [`arrays_to_string`] but recursing into
/// nested arrays, which is what a rectangular `int[][]` needs.
fn arrays_deep_to_string(v: &Value) -> String {
    match array_items(v) {
        Some(items) => {
            let inner: Vec<String> = items.iter().map(arrays_deep_to_string).collect();
            format!("[{}]", inner.join(", "))
        }
        None => java_str(v),
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
fn java_format(fmt: &str, args: &[Value], tags: &[&str]) -> Result<Value, Fault> {
    let mut out = String::new();
    let mut chars = fmt.chars().peekable();
    let mut argi = 0usize;
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        // An explicit argument index, `%2$s`. It is digits followed by `$`, so
        // it can only be told from a width by scanning past the digits first.
        let mut lead = String::new();
        while let Some(&d) = chars.peek() {
            if d.is_ascii_digit() {
                lead.push(d);
                chars.next();
            } else {
                break;
            }
        }
        let mut explicit_index: Option<usize> = None;
        if chars.peek() == Some(&'$') {
            chars.next();
            // Java indexes arguments from 1.
            explicit_index = lead.parse::<usize>().ok().map(|n| n.saturating_sub(1));
            lead.clear();
        }
        // flags
        let mut left = false;
        let mut zero = false;
        let mut plus = false;
        let mut group = false;
        let mut parens = false;
        // A leading `0` already consumed as part of `lead` is the zero-pad flag,
        // not a width digit — Java has no zero-width conversion.
        if lead.starts_with('0') {
            zero = true;
            lead.remove(0);
        }
        while let Some(&f) = chars.peek() {
            match f {
                '-' => left = true,
                '0' => zero = true,
                '+' => plus = true,
                ',' => group = true,
                '(' => parens = true,
                ' ' | '#' => {}
                _ => break,
            }
            chars.next();
        }
        // width
        let mut width = lead;
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
                // An explicit `%n$` index does not advance the implicit cursor,
                // which is what lets `%2$s %1$s` repeat and reorder arguments.
                let idx = explicit_index.unwrap_or(argi);
                let arg = args.get(idx).ok_or_else(|| {
                    Fault::internal("javars: String.format: not enough arguments")
                })?;
                if explicit_index.is_none() {
                    argi += 1;
                }
                check_conversion(conv, arg, tags.get(idx).copied().unwrap_or(""))?;
                let (mut s, numeric) = format_conversion(conv, arg, prec, plus)?;
                if group && numeric {
                    s = group_digits(&s);
                }
                // The `(` flag wraps a negative number in parentheses instead of
                // showing its minus sign.
                if parens && numeric {
                    if let Some(rest) = s.strip_prefix('-') {
                        s = format!("({rest})");
                    }
                }
                out.push_str(&pad(&s, width_n, left, zero && numeric));
            }
        }
    }
    Ok(Value::str(out))
}

/// The boxed class `java.util.Formatter` sees for one argument: the static Java
/// type the compiler recorded when it had one, else the class the runtime value
/// implies. `None` for `null`, which every conversion accepts and prints as
/// `null`.
fn boxed_class(tag: &str, v: &Value) -> Option<&'static str> {
    if matches!(v, Value::Undef) {
        return None;
    }
    Some(match tag {
        "int" | "Integer" => "java.lang.Integer",
        "long" | "Long" => "java.lang.Long",
        "short" | "Short" => "java.lang.Short",
        "byte" | "Byte" => "java.lang.Byte",
        "char" | "Character" => "java.lang.Character",
        "double" | "Double" => "java.lang.Double",
        "float" | "Float" => "java.lang.Float",
        "boolean" | "Boolean" => "java.lang.Boolean",
        "String" => "java.lang.String",
        // No static type. The value model collapses `Integer`/`Long`/`Short`/
        // `Byte` onto one variant and `Double`/`Float` onto another, so the
        // widest of each group is the reading — `Integer` for an integer,
        // because that is what a literal autoboxes to.
        _ => match v {
            Value::Int(_) => "java.lang.Integer",
            Value::Float(_) => "java.lang.Double",
            Value::Bool(_) => "java.lang.Boolean",
            Value::Str(_) => "java.lang.String",
            // A heap object's class is not one the conversion table rejects.
            _ => return None,
        },
    })
}

/// Reject a conversion whose argument is the wrong boxed type, the way
/// `java.util.Formatter` does — `%d` takes the integral boxes only, `%f` the
/// floating ones only, `%c` a `Character` or an integral code point. `%s`,
/// `%b`, and `%h` take anything, and a `null` argument prints as `null` under
/// every conversion rather than throwing.
///
/// Without this, `String.format("%.2f", 3)` answered `3.00` where Java throws:
/// a silently-formatted wrong-typed argument instead of the program's real
/// behaviour.
fn check_conversion(conv: char, arg: &Value, tag: &str) -> Result<(), Fault> {
    let integral = [
        "java.lang.Integer",
        "java.lang.Long",
        "java.lang.Short",
        "java.lang.Byte",
    ];
    let ok = |cls: &str| -> bool {
        match conv {
            'd' | 'x' | 'X' | 'o' => integral.contains(&cls),
            'f' | 'e' | 'E' | 'g' | 'G' | 'a' | 'A' => {
                matches!(cls, "java.lang.Double" | "java.lang.Float")
            }
            'c' | 'C' => {
                cls == "java.lang.Character"
                    // javars models a `char` value as a one-character String
                    // wherever it has crossed into text (a collection element,
                    // a `%s` slot), so a String whose type the compiler could
                    // not name is a `char` as often as it is a `String`. Only a
                    // *declared* `String` is rejected here; see `check_char`.
                    || integral.contains(&cls)
            }
            _ => true,
        }
    };
    let Some(cls) = boxed_class(tag, arg) else {
        return Ok(());
    };
    if ok(cls) {
        return Ok(());
    }
    // The unknown-tag `%c` case: a bare String could be javars's `char`
    // spelling, so it is only rejected when the compiler named the type.
    if matches!(conv, 'c' | 'C') && tag.is_empty() {
        return Ok(());
    }
    Err(Fault::java(
        "IllegalFormatConversionException",
        format!("{conv} != {cls}"),
    ))
}

/// [`JFORMAT`] — `String.format` with the compiler's per-argument type tags.
fn b_format(vm: &mut VM, argc: u8) -> Value {
    let args = pop_args(vm, argc);
    let tag_blob = args
        .last()
        .map(|v| v.as_str_cow().into_owned())
        .unwrap_or_default();
    let tags: Vec<&str> = if tag_blob.is_empty() {
        Vec::new()
    } else {
        tag_blob.split('\x1f').collect()
    };
    let fmt = args
        .first()
        .map(|v| v.as_str_cow().into_owned())
        .unwrap_or_default();
    let body = &args[1..args.len().saturating_sub(1)];
    match java_format(&fmt, body, &tags) {
        Ok(v) => v,
        Err(f) => raise(vm, f),
    }
}

/// Render one `String.format` conversion. Returns the rendered text and whether
/// it is a numeric conversion (which may be zero-padded).
fn format_conversion(
    conv: char,
    arg: &Value,
    prec: Option<usize>,
    plus: bool,
) -> Result<(String, bool), Fault> {
    // The `+` flag shows an explicit sign on a *non-negative* number; a negative
    // one carries its own `-` from the rendering below.
    let sign = |neg: bool| {
        if neg {
            ""
        } else if plus {
            "+"
        } else {
            ""
        }
    };
    // Every `Formatter.print*` starts with `if (arg == null) print("null")`, so
    // a `null` renders as the four characters under every conversion — width and
    // precision still apply, and it is not numeric, so `%08d` of `null` pads
    // with spaces. `%b`/`%B` are the exception: they answer `false`.
    if matches!(arg, Value::Undef) && !matches!(conv, 'b' | 'B') {
        let mut s: String = "null".chars().take(prec.unwrap_or(4)).collect();
        if conv.is_ascii_uppercase() {
            s = s.to_uppercase();
        }
        return Ok((s, false));
    }
    // Renderings that build from `x.abs()` need the sign put back explicitly.
    let signed = |x: f64, body: String| {
        format!(
            "{}{body}",
            if x.is_sign_negative() {
                "-"
            } else {
                sign(false)
            }
        )
    };
    match conv {
        'd' => {
            let n = arg.to_int();
            Ok((format!("{}{n}", sign(n < 0)), true))
        }
        'f' => {
            let x = arg.to_float();
            Ok((signed(x, fixed_half_up(x, prec.unwrap_or(6))), true))
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
        'c' => Ok((
            match arg {
                // `%c` on an integer renders its code point as a character.
                Value::Int(n) => char::from_u32(*n as u32).unwrap_or('\u{fffd}').to_string(),
                other => java_str(other),
            },
            false,
        )),
        // Java's `%e` always writes a two-digit exponent with an explicit sign
        // (`1.234568e+03`), where Rust's `{:e}` writes `1.234568e3`.
        'e' | 'E' => {
            let x = arg.to_float();
            // `sci_notation` already carries a negative sign; only the `+` flag's
            // explicit plus has to be added.
            let s = sci_notation(x, prec.unwrap_or(6));
            let s = if x.is_sign_negative() {
                s
            } else {
                format!("{}{s}", sign(false))
            };
            Ok((if conv == 'E' { s.to_uppercase() } else { s }, true))
        }
        // `%g` picks fixed or scientific by the value's magnitude; Java's
        // precision counts *significant* digits and defaults to 6.
        'g' | 'G' => {
            let x = arg.to_float();
            let p = prec.unwrap_or(6).max(1);
            let s = if x != 0.0 && (x.abs() < 1e-4 || x.abs() >= 10f64.powi(p as i32)) {
                sci_notation(x, p - 1)
            } else {
                let exp = if x == 0.0 {
                    0
                } else {
                    x.abs().log10().floor() as i32
                };
                signed(x, fixed_half_up(x, (p as i32 - 1 - exp).max(0) as usize))
            };
            // Both branches already carry a negative sign.
            let s = if x.is_sign_negative() {
                s
            } else {
                format!("{}{s}", sign(false))
            };
            Ok((if conv == 'G' { s.to_uppercase() } else { s }, true))
        }
        // `%h` is the argument's `hashCode()` in hex, or "null".
        'h' | 'H' => {
            let s = match arg {
                Value::Undef => "null".to_string(),
                other => format!("{:x}", java_hash(other).unwrap_or(0) as u32),
            };
            Ok((if conv == 'H' { s.to_uppercase() } else { s }, false))
        }
        other => Err(Fault::internal(format!(
            "javars: String.format: unsupported conversion `%{other}`"
        ))),
    }
}

/// Java's `%f` rendering: fixed-point with `prec` decimals, rounded HALF_UP on
/// the double's *exact* decimal value.
///
/// Rust's `{:.p}` rounds half-to-even, so `%.0f` of 2.5 is 2 there and 3 in
/// Java. The exact value is materialised well past `prec` (a double's decision
/// digit is nowhere near that far out), the digit after the cut decides, and the
/// carry is propagated through the decimal string.
fn fixed_half_up(x: f64, prec: usize) -> String {
    if !x.is_finite() {
        return java_str(&Value::float(x));
    }
    let exact = format!("{:.*}", prec + 30, x.abs());
    let point = exact.find('.').unwrap_or(exact.len());
    let cut = point + if prec == 0 { 0 } else { prec + 1 };
    let round_up = exact[cut..]
        .chars()
        .find(char::is_ascii_digit)
        .is_some_and(|c| c >= '5');
    let mut digits: Vec<u8> = exact[..cut].bytes().collect();
    if round_up {
        let mut i = digits.len();
        loop {
            if i == 0 {
                digits.insert(0, b'1');
                break;
            }
            i -= 1;
            match digits[i] {
                b'.' => continue,
                b'9' => digits[i] = b'0',
                d => {
                    digits[i] = d + 1;
                    break;
                }
            }
        }
    }
    String::from_utf8(digits).unwrap_or_default()
}

/// Java's `%e` rendering: `<mantissa>e<sign><at least two exponent digits>`,
/// carrying the value's own sign.
fn sci_notation(x: f64, prec: usize) -> String {
    let neg = if x.is_sign_negative() { "-" } else { "" };
    if x == 0.0 {
        return format!("{neg}{:.*}e+00", prec, 0.0);
    }
    // The mantissa is rounded HALF_UP like `%f`'s digits, not half-to-even:
    // Java's `Formatter` rounds through `BigDecimal.ROUND_HALF_UP`, so
    // `%e` of 5592405.5 is `5.592406e+06` where Rust's `{:.6e}` gives
    // `5.592405e+06`. Rounding the mantissa *after* dividing by a power of ten
    // would not see the exact tie at all, so the digits are taken from the
    // value's own decimal expansion.
    let (digits, exp) = sci_digits_half_up(x.abs(), prec);
    let mantissa = if prec == 0 {
        digits
    } else {
        format!("{}.{}", &digits[..1], &digits[1..])
    };
    format!(
        "{neg}{mantissa}e{}{:02}",
        if exp < 0 { '-' } else { '+' },
        exp.abs()
    )
}

/// The first `prec + 1` significant digits of `x` (positive, finite, non-zero),
/// rounded HALF_UP, with the decimal exponent they belong to.
///
/// Taken from the value's decimal expansion rather than from a scaled mantissa,
/// because scaling by a power of ten is itself inexact and would round the tie
/// away before it could be seen.
fn sci_digits_half_up(x: f64, prec: usize) -> (String, i32) {
    // Enough digits past the cut for the HALF_UP decision, the same margin
    // `fixed_half_up` uses.
    let expanded = format!("{:.*e}", prec + 30, x);
    let (mantissa, exp) = expanded.split_once('e').unwrap_or((expanded.as_str(), "0"));
    let mut exp: i32 = exp.parse().unwrap_or(0);
    let all: Vec<u8> = mantissa
        .bytes()
        .filter(u8::is_ascii_digit)
        .map(|b| b - b'0')
        .collect();
    let keep = prec + 1;
    let mut digits: Vec<u8> = all.iter().copied().take(keep).collect();
    digits.resize(keep, 0);
    if all.get(keep).is_some_and(|d| *d >= 5) {
        let mut i = digits.len();
        loop {
            if i == 0 {
                // 999… carried out: the digits become 100… one exponent up.
                digits.insert(0, 1);
                digits.pop();
                exp += 1;
                break;
            }
            i -= 1;
            if digits[i] == 9 {
                digits[i] = 0;
            } else {
                digits[i] += 1;
                break;
            }
        }
    }
    (digits.iter().map(|d| (d + b'0') as char).collect(), exp)
}

/// Insert Java's `,` grouping separators into the integer part of a rendered
/// number, leaving any sign and fractional part alone.
fn group_digits(s: &str) -> String {
    let (sign, rest) = match s.strip_prefix(['-', '+']) {
        Some(r) => (&s[..1], r),
        None => ("", s),
    };
    let (int_part, frac) = match rest.find('.') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let digits: Vec<char> = int_part.chars().collect();
    let mut grouped = String::new();
    for (i, c) in digits.iter().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(*c);
    }
    format!("{sign}{grouped}{frac}")
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
    // Rendering a `subList` view iterates it, which is where Java reports a
    // backing list that moved — so the check happens before anything is
    // written, not after a wrong (empty) list has already reached the stream.
    if let Some(f) = vals.iter().find_map(stale_view) {
        return raise(vm, f);
    }
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
/// an instance, `[@<hash>` for an array. The class name is the qualified one
/// `getClass().getName()` reports (`java.lang.Object`, not `Object`), and the
/// hash is the handle (deterministic within a run) rather than a JVM identity
/// hash.
fn obj_default_str(id: u32) -> String {
    HEAP.with(|h| {
        let h = h.borrow();
        match h.get(id as usize) {
            // An enum constant carries its name in a synthesized field, and
            // `Enum.toString()` returns exactly that. Reading it here is what
            // makes `String.valueOf(color)` and `Arrays.toString(values())`
            // print `RED` rather than `Color@1` without calling the Java-level
            // `toString()`. Rendering does not call an override at all — not
            // because it could not (this runs under a builtin, which holds
            // `&mut VM`), but because `"" + obj` renders from the numeric hook,
            // which does not, and the two must not disagree. See BUGS.md.
            Some(HostObj::Instance { class, fields }) => match fields.get(crate::ast::ENUM_NAME) {
                Some(n) if !matches!(n, Value::Undef) => n.as_str_cow().into_owned(),
                _ => {
                    let name =
                        crate::prelude::qualified_class(class).unwrap_or_else(|| class.to_string());
                    format!("{name}@{id:x}")
                }
            },
            Some(HostObj::Array(_)) => format!("[@{id:x}"),
            Some(HostObj::List { items, .. }) => render_sequence(items),
            // A view renders its window of the backing list. Rendering cannot
            // raise, so a view whose backing list moved prints as though it
            // were empty rather than reporting the comodification the next
            // real method call does report.
            Some(HostObj::SubList { .. }) => render_sequence(
                &sublist_items(id as usize)
                    .and_then(Result::ok)
                    .unwrap_or_default(),
            ),
            Some(HostObj::Set { items, order, .. }) => render_set(items, *order),
            Some(HostObj::Map { entries, order }) => render_map(entries, *order),
            // Java renders a lambda as `Class$$Lambda/0x…@<identity hash>`,
            // which is not reproducible (and not stable across JVM runs), so
            // javars prints a fixed marker instead. See `BUGS.md`.
            Some(HostObj::Closure { .. }) => format!("<lambda>@{id:x}"),
            None => format!("(obj:{id})"),
        }
    })
}

/// Java's `Double.toString` prints whole values with a trailing `.0`
/// (`3.0`, not `3`) and keeps a decimal point; non-finite values print as
/// `Infinity`/`-Infinity`/`NaN`.
fn format_double(f: f64) -> String {
    // Rust's `{}`/`{:e}` are the shortest round-tripping decimal, which is what
    // Java selects too — except when that decimal has a single digit, where
    // Java widens the candidate set (see [`widen_exact`]).
    let sci = format!("{f:e}");
    if f.is_finite() && f != 0.0 && shortest_is_one_digit(&sci) {
        if let Some((digits, exp)) = widen_exact(f) {
            let sign = if f < 0.0 { "-" } else { "" };
            return format_ieee(
                f,
                format!("{sign}{}", plain_form(&digits, exp)),
                format!("{sign}{}", sci_form(&digits, exp)),
            );
        }
    }
    format_ieee(f, format!("{f}"), sci)
}

/// True when a Rust `{:e}` rendering has a single-digit mantissa (`1e-45`, not
/// `1.4e-45`) — the gate on Java's **two-digit widening**, the one rule its
/// `toString` specification applies that "shortest decimal that round-trips"
/// does not.
///
/// `Double.toString`/`Float.toString` take `R` to be every decimal that rounds
/// to the value and `p` to be the minimal length in `R`. For `p >= 2` the
/// candidates `T` are the decimals of length exactly `p` — shortest-round-trip.
/// **For `p < 2`, `T` is the decimals of length 1 _or 2_**, and the answer is
/// the member of `T` nearest the value (ties to even). So a one-digit shortest
/// form is not automatically the answer: a two-digit decimal that is closer
/// beats it.
///
/// For every normal value the two rules agree, because a normal's binary ulp is
/// some sixteen decimal orders below the value: the nearest two-digit decimal is
/// always the one-digit answer with a `0` appended, which canonicalizes straight
/// back (a decimal's length counts a mantissa not divisible by 10). Down at the
/// subnormal floor the binary ulp is the same size as the value, and they part:
/// `Double.MIN_VALUE` is 4.9406…E-324, which `5.0E-324` does round-trip to, but
/// `4.9E-324` is nearer — so Java prints `4.9E-324`. The same holds for
/// `Float.MIN_VALUE` (`1.4E-45`, not `1.0E-45`) and for every subnormal whose
/// shortest form is one digit. [`widen_exact`] does the widening.
fn shortest_is_one_digit(sci: &str) -> bool {
    sci.split_once('e')
        .is_some_and(|(mantissa, _)| mantissa.bytes().filter(u8::is_ascii_digit).count() == 1)
}

/// The two-digit widening itself (see [`shortest_is_one_digit`] for the rule).
///
/// Applied by rounding `|v|`'s **exact** decimal expansion to two significant
/// digits, half to even — which is precisely "the nearest decimal of length 1 or
/// 2, ties to even". The exact expansion is the only way to compare decimals
/// down there: `10^exp` is not itself a representable `double` at the subnormal
/// floor, so the arithmetic `v / 10^(exp-1)` underflows to zero. Nineteen
/// significant digits (`{:.18e}`, which Rust renders exactly) decide any
/// two-digit rounding.
///
/// Returns the widened `(digits, exp10)`, or `None` when the result canonicalizes
/// back to one digit (a trailing `0`) — the no-change case. Callers gate this on
/// [`shortest_is_one_digit`], because it is only the `p < 2` branch of the rule.
fn widen_exact(v: f64) -> Option<(String, i32)> {
    let exact = format!("{:.*e}", 18, v.abs());
    let (mantissa, exp) = exact.split_once('e')?;
    let exp: i32 = exp.parse().ok()?;
    let d: Vec<u8> = mantissa
        .bytes()
        .filter(u8::is_ascii_digit)
        .map(|b| b - b'0')
        .collect();
    let mut two = u32::from(d[0]) * 10 + u32::from(d[1]);
    let beyond_half = d[2] > 5 || (d[2] == 5 && d[3..].iter().any(|&x| x != 0));
    if beyond_half || (d[2] == 5 && two % 2 == 1) {
        two += 1;
    }
    // A carry out of `99` is `100`, i.e. one digit more and one decade up.
    let (two, exp) = if two == 100 {
        (10, exp + 1)
    } else {
        (two, exp)
    };
    if two % 10 == 0 {
        return None;
    }
    Some((two.to_string(), exp))
}

/// `Float.toString` — the same layout rules, but the shortest decimal is
/// computed against **32-bit** precision. That is the whole difference between
/// the two: the `f64` nearest `0.1f` prints as `0.10000000149011612` as a
/// `double` and as `0.1` as a `float`, because only 32 bits have to round-trip.
fn format_float(f: f32) -> String {
    if !f.is_finite() || f == 0.0 {
        return format_ieee(f as f64, String::new(), String::new());
    }
    // The digit selection works on magnitude; the sign is put back on both
    // renderings so `format_ieee` only has to choose between them.
    let (digits, exp10) = java_shortest_f32(f);
    // `Float.toString` carries the same two-digit widening as `Double`'s, over
    // the `float`'s own rounding interval — which is what makes
    // `Float.MIN_VALUE` print as `1.4E-45` rather than `1.0E-45`.
    let (digits, exp10) = if digits.len() == 1 {
        widen_exact(f64::from(f)).unwrap_or((digits, exp10))
    } else {
        (digits, exp10)
    };
    let sign = if f < 0.0 { "-" } else { "" };
    format_ieee(
        f as f64,
        format!("{sign}{}", plain_form(&digits, exp10)),
        format!("{sign}{}", sci_form(&digits, exp10)),
    )
}

/// The digits and decimal exponent `Float.toString` selects for `v`, as
/// (significant digits, exponent) where the value is `d.ddd × 10^exp`.
///
/// Java and Rust agree on the *length* — both emit the shortest decimal that
/// round-trips — but not always on which one. Java's rule (`Double.toString`'s
/// specification, which `Float`'s mirrors) picks the candidate closest to the
/// value and, when two are equidistant, the one whose last digit is **even**.
/// Rust's formatter breaks that final tie the other way, so `16777217.0f * 0.2f`
/// (exactly 3355443.25) prints `3355443.3` there and `3355443.2` in Java.
fn java_shortest_f32(v: f32) -> (String, i32) {
    let sci = format!("{v:e}");
    let (mantissa, exp) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let neg = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();

    // The only other candidate of the same length is one decimal ulp away, on
    // whichever side of the value Rust's answer is not.
    let here = rebuild(&digits, exp, neg);
    let delta = here - f64::from(v);
    if delta == 0.0 {
        return (digits, exp);
    }
    let toward = if (delta > 0.0) != neg { -1 } else { 1 };
    let Some((other_digits, other_exp)) = step_last_digit(&digits, exp, toward) else {
        return (digits, exp);
    };
    // A candidate that does not round-trip is not a candidate.
    let other = rebuild(&other_digits, other_exp, neg);
    if other as f32 != v {
        return (digits, exp);
    }
    let (d_here, d_other) = (delta.abs(), (other - f64::from(v)).abs());
    // Both distances sum to one decimal ulp, so "equidistant" is a comparison at
    // that scale; f64 carries eight more digits than the nine at stake here.
    let tie = (d_here - d_other).abs() <= (d_here + d_other) * 1e-9;
    if tie {
        let even = |d: &str| d.as_bytes().last().is_some_and(|b| (b - b'0') % 2 == 0);
        return if even(&digits) {
            (digits, exp)
        } else {
            (other_digits, other_exp)
        };
    }
    if d_other < d_here {
        (other_digits, other_exp)
    } else {
        (digits, exp)
    }
}

/// The value of `d.ddd × 10^exp`, signed.
fn rebuild(digits: &str, exp: i32, neg: bool) -> f64 {
    let mut s = String::new();
    if neg {
        s.push('-');
    }
    s.push_str(&digits[..1]);
    if digits.len() > 1 {
        s.push('.');
        s.push_str(&digits[1..]);
    }
    s.push('e');
    s.push_str(&exp.to_string());
    s.parse().unwrap_or(f64::NAN)
}

/// Add `step` (±1) to the last significant digit, carrying through. A carry out
/// of the leading digit shortens the digit string and bumps the exponent, which
/// keeps the candidate the same *length* as the one it came from.
fn step_last_digit(digits: &str, exp: i32, step: i8) -> Option<(String, i32)> {
    let mut d: Vec<u8> = digits.bytes().map(|b| b - b'0').collect();
    let mut i = d.len();
    if step > 0 {
        loop {
            if i == 0 {
                // 999… + 1 → 100… one decimal place up.
                let mut out = vec![1u8];
                out.resize(d.len(), 0);
                return Some((to_digits(&out), exp + 1));
            }
            i -= 1;
            if d[i] < 9 {
                d[i] += 1;
                break;
            }
            d[i] = 0;
        }
    } else {
        loop {
            if i == 0 {
                // 100… - 1 → 999… one decimal place down.
                return Some((("9").repeat(d.len()), exp - 1));
            }
            i -= 1;
            if d[i] > 0 {
                d[i] -= 1;
                break;
            }
            d[i] = 9;
        }
    }
    Some((to_digits(&d), exp))
}

fn to_digits(d: &[u8]) -> String {
    d.iter().map(|b| (b + b'0') as char).collect()
}

/// `d.ddd × 10^exp` written out in full, the form Java uses inside
/// [1e-3, 1e7). Always carries at least one fractional digit.
fn plain_form(digits: &str, exp: i32) -> String {
    if exp < 0 {
        return format!("0.{}{digits}", "0".repeat((-exp - 1) as usize));
    }
    let int_len = exp as usize + 1;
    if digits.len() <= int_len {
        format!("{digits}{}.0", "0".repeat(int_len - digits.len()))
    } else {
        format!("{}.{}", &digits[..int_len], &digits[int_len..])
    }
}

/// `d.ddd × 10^exp` in the `1.5e3` shape [`format_ieee`] uppercases.
fn sci_form(digits: &str, exp: i32) -> String {
    if digits.len() > 1 {
        format!("{}.{}e{exp}", &digits[..1], &digits[1..])
    } else {
        format!("{digits}e{exp}")
    }
}

/// The shared layout of `Double.toString` / `Float.toString`, given the value
/// and its shortest plain and scientific renderings at the right precision.
fn format_ieee(f: f64, plain: String, sci: String) -> String {
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

    // Java uses plain decimal only inside [1e-3, 1e7); outside that range it
    // switches to "computerized scientific notation". Rust's `{}` never
    // switches, so the range test has to be explicit or large/small magnitudes
    // print as long digit strings (`25000000.0` where Java says `2.5E7`).
    let mag = f.abs();
    if (1e-3..1e7).contains(&mag) {
        // Java always keeps a fractional digit: `1.0`, never `1`.
        return if plain.contains('.') {
            plain
        } else {
            format!("{plain}.0")
        };
    }

    // Scientific form. Rust renders `2.5e7` / `1e7`; Java wants `2.5E7` / `1.0E7`
    // — an uppercase exponent, no `+`, and a mantissa that always carries a
    // fractional digit.
    let (mantissa, exp) = match sci.split_once('e') {
        Some((m, e)) => (m, e),
        None => return sci,
    };
    let mantissa = if mantissa.contains('.') {
        mantissa.to_string()
    } else {
        format!("{mantissa}.0")
    };
    format!("{mantissa}E{exp}")
}

/// Whether `v` is one of Java's primitive numeric shapes on the fusevm value
/// model: `byte`/`short`/`char`/`int`/`long` ride [`Value::Int`], `float` and
/// `double` ride [`Value::Float`]. A `boolean`, a `String`, and every reference
/// type answer `false`.
///
/// [`numeric_hook`] gates its arithmetic on this predicate rather than on an
/// arm being written above the `String` ones. Java's `+` is overloaded, so a
/// catch-all concatenating arm will answer an *arithmetic* pair the moment a
/// numeric case is missing from the arms before it — and it answers with a
/// number-shaped `String` rather than an error, which is why the failure is
/// silent. Requiring both operands to be numbers up front makes the two paths
/// disjoint by construction instead of by ordering.
fn is_java_number(v: &Value) -> bool {
    matches!(v, Value::Int(_) | Value::Float(_))
}

/// One binary operation on two Java primitive numbers — the pairs fusevm hands
/// back rather than answering natively.
///
/// **Two `Value::Int`s are Java `long`s:** two's-complement and silently
/// wrapping, never a promotion to a wider representation. fusevm delegates such
/// a pair when the native operation overflows `i64` (`Long.MAX_VALUE + 1` is
/// `Long.MIN_VALUE`) or when `checked_rem` overflows, which is the single pair
/// `Long.MIN_VALUE % -1L` — Java answers `0`.
///
/// **A mixed `Int`/`Float` pair is Java's binary numeric promotion** (JLS
/// 5.6.2: if either operand is of type `double`, the other is converted to
/// `double`). fusevm delegates such a pair once the integer is past 2^53,
/// because converting it *rounds* and only the host knows whether the rounding
/// is a defect. **For Java it is not a defect: the language mandates the
/// conversion, so the rounded `double` is the correct answer and is returned
/// here deliberately.** Measured against `java` 26.0.2 with
/// `L = 3^34 = 16677181699666569L` and `R = 1.6677181699666568E16` (its
/// `double` image, a neighbouring value): `L == R` is `true` and `L + 2.0` is
/// `1.667718169966657E16`. The same pair in Ruby answers `false` — that
/// divergence is precisely why the decision belongs to the frontend and not to
/// the VM.
///
/// A zero divisor is Java's `ArithmeticException`; fusevm answers integral
/// `%` by zero natively so it does not currently arrive here, but the hook is
/// public and must not depend on that.
fn java_numeric(op: NumOp, a: &Value, b: &Value) -> Result<Value, String> {
    if let (Value::Int(x), Value::Int(y)) = (a, b) {
        let (x, y) = (*x, *y);
        return match op {
            NumOp::Add => Ok(Value::Int(x.wrapping_add(y))),
            NumOp::Sub => Ok(Value::Int(x.wrapping_sub(y))),
            NumOp::Mul => Ok(Value::Int(x.wrapping_mul(y))),
            NumOp::Div | NumOp::Mod if y == 0 => {
                Err("java.lang.ArithmeticException: / by zero".to_string())
            }
            // Both truncate toward zero, and both wrap on the one overflowing
            // pair: `Long.MIN_VALUE / -1L` is `Long.MIN_VALUE`, `% -1L` is `0`.
            NumOp::Div => Ok(Value::Int(x.wrapping_div(y))),
            NumOp::Mod => Ok(Value::Int(x.wrapping_rem(y))),
            NumOp::Eq => Ok(Value::bool(x == y)),
            NumOp::Ne => Ok(Value::bool(x != y)),
            NumOp::Lt => Ok(Value::bool(x < y)),
            NumOp::Gt => Ok(Value::bool(x > y)),
            NumOp::Le => Ok(Value::bool(x <= y)),
            NumOp::Ge => Ok(Value::bool(x >= y)),
            NumOp::Neg => Ok(Value::Int(x.wrapping_neg())),
            NumOp::Pow => Err(NO_POW.to_string()),
        };
    }
    // Promoted to `double`. Rust's `f64` operators are IEEE-754 with the same
    // NaN and infinity rules Java specifies, and its `%` is the truncated
    // remainder that takes the dividend's sign, like Java's — `7L % -2.5` is
    // `2.0` in both.
    let (x, y) = (as_f64(a), as_f64(b));
    match op {
        NumOp::Add => Ok(Value::float(x + y)),
        NumOp::Sub => Ok(Value::float(x - y)),
        NumOp::Mul => Ok(Value::float(x * y)),
        NumOp::Div => Ok(Value::float(x / y)),
        NumOp::Mod => Ok(Value::float(x % y)),
        NumOp::Eq => Ok(Value::bool(x == y)),
        NumOp::Ne => Ok(Value::bool(x != y)),
        NumOp::Lt => Ok(Value::bool(x < y)),
        NumOp::Gt => Ok(Value::bool(x > y)),
        NumOp::Le => Ok(Value::bool(x <= y)),
        NumOp::Ge => Ok(Value::bool(x >= y)),
        NumOp::Neg => Ok(Value::float(-x)),
        NumOp::Pow => Err(NO_POW.to_string()),
    }
}

/// Java has no exponentiation operator, so [`NumOp::Pow`] is never emitted;
/// `Math.pow` is a builtin call instead.
const NO_POW: &str = "javars: Java has no `**` operator";

/// Strict numeric hook: fusevm delegates here whenever it cannot answer an
/// operation itself under the strict policy. Three cases arrive:
///
/// 1. **A non-numeric operand** — Java's `String` `+` overload, and value
///    comparisons against a string. This is the case slice 1 was written for.
/// 2. **An all-integer operation fusevm could not complete in `i64`** — an
///    overflowing `Add`/`Sub`/`Mul`/`Neg`, or `Long.MIN_VALUE % -1L`.
/// 3. **A mixed `Int`/`Float` pair whose integer is past 2^53** — converting it
///    to `f64` would round, so fusevm hands over the operands instead of an
///    answer computed on a neighbouring value.
///
/// Case 3 is newer than this hook. The comment that stood here asserted that
/// "all-numeric arithmetic never reaches here (it stays on the native fast path
/// and the JIT)"; that was true when written and fusevm's strict-exactness fix
/// falsified it. Under the old text every mixed pair fell through to the
/// `String` arms below — `Add` returned a concatenation, the comparisons
/// answered by lexicographic string order, and the rest returned a type error.
/// [`java_numeric`] now answers all three cases and is reached on operand
/// shape, so no numeric pair can fall into a `String` arm again.
pub fn numeric_hook(op: NumOp, a: &Value, b: &Value) -> Result<Value, String> {
    // `Neg` is unary — fusevm passes `Undef` as the second operand, so it can
    // never satisfy the two-number gate below and is answered first.
    if op == NumOp::Neg && is_java_number(a) {
        return java_numeric(op, a, &Value::Int(0));
    }
    if is_java_number(a) && is_java_number(b) {
        return java_numeric(op, a, b);
    }
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

/// `>>>` — zero-fill right shift at `width` bits (32 for `int`, 64 for `long`).
/// See [`JUSHR`].
fn b_ushr(vm: &mut VM, _argc: u8) -> Value {
    let width = as_i64(&vm.stack.pop().unwrap_or(Value::Undef));
    let count = as_i64(&vm.stack.pop().unwrap_or(Value::Undef)) as u32;
    let value = as_i64(&vm.stack.pop().unwrap_or(Value::Undef));
    if width == 32 {
        Value::Int(((value as u32) >> count) as i32 as i64)
    } else {
        Value::Int(((value as u64) >> count) as i64)
    }
}

/// A narrowing primitive cast. See [`JCAST`].
///
/// Rust's `as` between a float and an integer saturates and maps NaN to 0,
/// which is exactly Java's narrowing rule for `double`/`float` → `int`/`long`;
/// the integral narrowings are plain two's-complement truncations.
fn b_cast(vm: &mut VM, _argc: u8) -> Value {
    let ty = vm.stack.pop().unwrap_or(Value::Undef);
    let ty = ty.as_str_cow().into_owned();
    let v = vm.stack.pop().unwrap_or(Value::Undef);
    match ty.as_str() {
        "int" => Value::Int(match &v {
            Value::Float(f) => *f as i32 as i64,
            other => as_i64(other) as i32 as i64,
        }),
        "long" => Value::Int(match &v {
            Value::Float(f) => *f as i64,
            other => as_i64(other),
        }),
        "short" => Value::Int(cast_to_i64(&v) as i16 as i64),
        "byte" => Value::Int(cast_to_i64(&v) as i8 as i64),
        // A `char` is a 16-bit *unsigned* integral value, so `(char) -1` is
        // 65535 — the one narrowing cast that does not sign-extend.
        "char" => Value::Int(i64::from(cast_to_i64(&v) as u16)),
        // `(double)` only has to make an integral operand floating; `(float)`
        // additionally rounds to 32-bit precision, which is a real value change
        // (`(float) 0.1` is not `0.1`).
        "double" => Value::float(as_f64(&v)),
        "float" => Value::float(as_f64(&v) as f32 as f64),
        // `boolean` and every reference type keep their representation.
        _ => v,
    }
}

/// [`JCHR_STR`] — Java's string conversion of a `char` code point. An integer
/// becomes the one-character String; a `char[]` converts element-wise (a fresh
/// array, so the operand is not mutated); anything else passes through, which
/// keeps the builtin safe to emit on a statically-`char` expression whose value
/// turned out to be `null`.
fn b_chr_str(vm: &mut VM, _argc: u8) -> Value {
    let v = vm.stack.pop().unwrap_or(Value::Undef);
    match &v {
        Value::Obj(_) => match array_items(&v) {
            Some(items) => Value::Obj(heap_alloc(HostObj::Array(
                items.iter().map(char_to_string).collect(),
            ))),
            None => v,
        },
        _ => char_to_string(&v),
    }
}

/// One `char` code point as its one-character String. A non-integer passes
/// through, so a `null` (or an already-boxed `Character`) is left alone.
fn char_to_string(v: &Value) -> Value {
    match v {
        Value::Int(n) => Value::str(char::from_u32(*n as u32).unwrap_or('\u{fffd}').to_string()),
        other => other.clone(),
    }
}

/// [`JF32`] — round to 32-bit `float` precision.
fn b_f32(vm: &mut VM, _argc: u8) -> Value {
    let v = vm.stack.pop().unwrap_or(Value::Undef);
    match &v {
        Value::Float(f) => Value::float(*f as f32 as f64),
        Value::Int(n) => Value::float(*n as f32 as f64),
        _ => v,
    }
}

/// [`JF32_ARITH`] — one arithmetic operation at 32-bit width.
fn b_f32_arith(vm: &mut VM, argc: u8) -> Value {
    let args = pop_args(vm, argc);
    let a = args.first().map(Value::to_float).unwrap_or(0.0) as f32;
    let b = args.get(1).map(Value::to_float).unwrap_or(0.0) as f32;
    let r = match args.get(2).map(Value::to_int).unwrap_or(f32_op::ADD) {
        f32_op::SUB => a - b,
        f32_op::MUL => a * b,
        f32_op::DIV => a / b,
        f32_op::REM => a % b,
        _ => a + b,
    };
    Value::float(r as f64)
}

/// [`JF32_STR`] — `Float.toString`, or element-wise over a `float[]`.
fn b_f32_str(vm: &mut VM, _argc: u8) -> Value {
    let v = vm.stack.pop().unwrap_or(Value::Undef);
    match &v {
        Value::Obj(_) => match array_items(&v) {
            Some(items) => Value::Obj(heap_alloc(HostObj::Array(
                items.iter().map(float_to_string).collect(),
            ))),
            None => v,
        },
        _ => float_to_string(&v),
    }
}

/// One `float` rendered the way `Float.toString` does. A non-floating value
/// passes through, so the builtin is safe on a statically-`float` expression
/// whose value turned out to be `null`.
fn float_to_string(v: &Value) -> Value {
    match v {
        Value::Float(f) => Value::str(format_float(*f as f32)),
        other => other.clone(),
    }
}

/// [`JCHECKCAST`] — the reference cast's runtime check.
///
/// The value's class comes from [`value_class`] — the same answer `instanceof`
/// reads. This path used to keep its own, narrower copy of that question, which
/// named only a `String`, a boxed primitive and a user instance and returned
/// `None` for every collection and array. The two then disagreed: `aList
/// instanceof String` was `false` while `(String) aList` passed, though a
/// program can only observe one runtime class per value.
fn b_checkcast(vm: &mut VM, argc: u8) -> Value {
    let args = pop_args(vm, argc);
    let value = args.first().cloned().unwrap_or(Value::Undef);
    let target = args
        .get(1)
        .map(|v| v.as_str_cow().into_owned())
        .unwrap_or_default();
    // `(Anything) null` succeeds in Java, and a lambda carries no interface to
    // check against — the two shapes [`value_class`] does not name.
    let Some(runtime) = value_class(&value) else {
        return value;
    };
    if cast_allowed(&runtime, &target, &value) {
        return value;
    }
    // The cast does not fit — but javars reports only a failure it can *name*.
    // An array's element type is erased, so `[I` and `[Ljava.lang.String;` are
    // both unavailable, and a `ClassCastException` whose message had to invent
    // the class it names would be worse than the miss.
    let Some(from) = binary_name(&runtime, &value) else {
        return value;
    };
    raise(
        vm,
        Fault::java("ClassCastException", cast_message(&from, &target)),
    );
    Value::Undef
}

/// The binary name `getClass().getName()` reports for a value whose class is
/// `class`, or `None` when javars cannot produce the JDK's exactly.
///
/// Most shapes are a straight qualification. The four that are not are the
/// collections the JDK implements with a private class whose identity depends
/// on the value rather than on its kind, each measured against the reference
/// JDK rather than inferred:
///
///   * `List.of` is `ImmutableCollections$List12` at one or two elements and
///     `$ListN` otherwise — including at zero, which is a `ListN`.
///   * `Set.of` splits the same way between `$Set12` and `$SetN`.
///   * `Arrays.asList` is `Arrays$ArrayList`, whatever its length.
///   * a `subList` is named for the *root* list it is a window onto, not for
///     itself: `ArrayList$SubList` over a mutable list,
///     `AbstractList$RandomAccessSubList` over `Arrays.asList`, and
///     `ImmutableCollections$SubList` over `List.of`. A view of a view keeps
///     the root's answer.
///
/// An array is the one shape with no answer at all: its element type is gone,
/// and `[I` and `[Ljava.lang.String;` differ only by it.
fn binary_name(class: &str, v: &Value) -> Option<String> {
    let len = || sequence_len(v).unwrap_or(0);
    Some(match class {
        "[]" => return None,
        "List$fixed" => "java.util.Arrays$ArrayList".to_string(),
        "List$immutable" => match len() {
            1 | 2 => "java.util.ImmutableCollections$List12".to_string(),
            _ => "java.util.ImmutableCollections$ListN".to_string(),
        },
        "Set$immutable" => match len() {
            1 | 2 => "java.util.ImmutableCollections$Set12".to_string(),
            _ => "java.util.ImmutableCollections$SetN".to_string(),
        },
        "List$sub" => match sublist_root_fixity(v) {
            Some(Fixity::Mutable) => "java.util.ArrayList$SubList".to_string(),
            Some(Fixity::FixedSize) => "java.util.AbstractList$RandomAccessSubList".to_string(),
            Some(Fixity::Immutable) => "java.util.ImmutableCollections$SubList".to_string(),
            None => return None,
        },
        other => crate::prelude::qualified_throwable(other).unwrap_or_else(|| jdk_name(other)),
    })
}

/// The element count of a list or set value, for the factories whose JDK class
/// depends on it.
fn sequence_len(v: &Value) -> Option<usize> {
    let Value::Obj(id) = v else { return None };
    HEAP.with(|h| match h.borrow().get(*id as usize) {
        Some(HostObj::List { items, .. }) | Some(HostObj::Set { items, .. }) => Some(items.len()),
        _ => None,
    })
}

/// The [`Fixity`] of the list at the root of a `subList` chain — a view of a
/// view is named for the list that actually owns the elements.
fn sublist_root_fixity(v: &Value) -> Option<Fixity> {
    let Value::Obj(id) = v else { return None };
    HEAP.with(|h| {
        let h = h.borrow();
        let mut cur = *id as usize;
        // The chain is finite (a view is created from an existing list), but
        // bound the walk anyway rather than trusting the heap not to cycle.
        for _ in 0..64 {
            match h.get(cur) {
                Some(HostObj::SubList { parent, .. }) => cur = *parent as usize,
                Some(HostObj::List { fixed, .. }) => return Some(*fixed),
                _ => return None,
            }
        }
        None
    })
}

/// Whether a value of runtime class `runtime` may be cast to `target`.
///
/// A user class walks the same supertype graph `instanceof` does. The
/// `java.lang` types are decided from the value model, which is why the
/// *integral* wrappers all answer yes to each other: `int`, `long`, `short`,
/// `byte` are one `Value::Int` here, so javars cannot prove a cast between them
/// wrong and does not pretend to. It can prove `(String) anInteger` wrong, and
/// that is the cast programs actually write.
fn cast_allowed(runtime: &str, target: &str, value: &Value) -> bool {
    if target == "Object" || runtime == target {
        return true;
    }
    // The exact supertype graph — the same one `instanceof` walks, so the two
    // cannot drift into disagreeing about what a type extends.
    if is_subclass_of(runtime, target) {
        return true;
    }
    // On top of it, and only here, the sibling types the value model cannot
    // tell apart: `int`/`long`/`short`/`byte` are one `Value::Int`, a `double`
    // and a `float` one `Value::Float`, and a boxed `Character` is the
    // one-character String javars models it as. A cast between any of these
    // cannot be proven wrong, so it is allowed rather than invented as a
    // failure. `instanceof` deliberately does NOT share this leniency: it has
    // to answer a boolean, and Java's answer for `42 instanceof Long` is
    // `false`.
    match runtime {
        "Integer" => matches!(target, "Long" | "Short" | "Byte" | "Character"),
        "Double" => target == "Float",
        "String" => target == "Character" && value.as_str_cow().chars().count() == 1,
        // `new LinkedList<>()` is modeled as the mutable list an `ArrayList` is,
        // so a `LinkedList` value arrives here calling itself an `ArrayList`.
        // Refusing `(LinkedList) aLinkedList` would be inventing a failure out
        // of javars's own modelling choice, which is exactly what the wrapper
        // arms above avoid.
        "ArrayList" => target == "LinkedList",
        _ => false,
    }
}

/// Java's `ClassCastException` detail message.
///
/// The leading `class X cannot be cast to class Y` is exact. Java appends a
/// parenthetical naming each class's module and class loader, which is
/// reproducible only when both are JDK types — for a user class the launcher's
/// loader is identified by an identity hash javars has no counterpart for — so
/// that clause is emitted for the JDK pair and dropped otherwise, the same
/// bounded omission `NullPointerException`'s provenance clause already makes.
fn cast_message(from: &str, target: &str) -> String {
    let qual = |n: &str| crate::prelude::qualified_throwable(n).unwrap_or_else(|| jdk_name(n));
    // `from` arrives already resolved by [`binary_name`], which is the only
    // side that can depend on the *value* rather than on the class name.
    let (r, t) = (from.to_string(), qual(target));
    let head = format!("class {r} cannot be cast to class {t}");
    if r.starts_with("java.") && t.starts_with("java.") {
        format!("{head} ({r} and {t} are in module java.base of loader 'bootstrap')")
    } else {
        head
    }
}

/// Whether a reference cast to `ty` is one javars can decide.
///
/// This is the *target* half of the same question [`value_class`] answers for a
/// value, and it lives here so the two halves read one list. The compiler asks
/// it before emitting a [`JCHECKCAST`] at all: a name that is neither a
/// declared class nor one of these is a type javars does not model — a type
/// *variable* after erasure, an array type, a JDK class it has never heard of —
/// and a check it cannot decide must not invent a failure.
///
/// `Object` is deliberately absent: every value satisfies it, so a check would
/// only cost ops. Every other name here appears in [`jdk_supers`], is produced
/// by [`value_class`], or is one of the wrapper siblings [`cast_allowed`]
/// answers for — which `castable_targets_are_closed_over_the_supertype_graph`
/// asserts, so this list cannot fall behind the graph.
pub fn is_checkable_cast_target(ty: &str) -> bool {
    CHECKABLE_CAST_TARGETS.contains(&ty)
}

/// The list [`is_checkable_cast_target`] answers from. A slice rather than a
/// `matches!` so the closure test can walk it.
const CHECKABLE_CAST_TARGETS: &[&str] = &[
    // java.lang, and the wrapper siblings `cast_allowed` decides by leniency.
    "String",
    "Integer",
    "Long",
    "Short",
    "Byte",
    "Double",
    "Float",
    "Boolean",
    "Character",
    "Number",
    "CharSequence",
    "Comparable",
    "Cloneable",
    "Iterable",
    "Enum",
    "Record",
    // java.io
    "Serializable",
    // java.util — the concrete kinds, then the interfaces above them.
    "List",
    "ArrayList",
    "LinkedList",
    "Collection",
    "SequencedCollection",
    "AbstractCollection",
    "AbstractList",
    "RandomAccess",
    "Set",
    "HashSet",
    "LinkedHashSet",
    "TreeSet",
    "SortedSet",
    "NavigableSet",
    "SequencedSet",
    "AbstractSet",
    "Map",
    "HashMap",
    "LinkedHashMap",
    "TreeMap",
    "SortedMap",
    "NavigableMap",
    "SequencedMap",
    "AbstractMap",
];

/// The qualified name of a modeled JDK type, or the bare name of a user class.
///
/// The package is part of the `ClassCastException` message and of the
/// module-and-loader clause that follows it, so a `java.util` type named as
/// though it were unpackaged would produce a message that is wrong in two
/// places at once.
fn jdk_name(n: &str) -> String {
    match n {
        "String" | "Integer" | "Long" | "Short" | "Byte" | "Double" | "Float" | "Boolean"
        | "Character" | "Number" | "CharSequence" | "Comparable" | "Cloneable" | "Iterable"
        | "Enum" | "Record" | "Object" => {
            format!("java.lang.{n}")
        }
        "Serializable" => "java.io.Serializable".to_string(),
        "List"
        | "ArrayList"
        | "LinkedList"
        | "Set"
        | "HashSet"
        | "LinkedHashSet"
        | "TreeSet"
        | "SortedSet"
        | "NavigableSet"
        | "SequencedSet"
        | "SequencedCollection"
        | "Collection"
        | "Map"
        | "HashMap"
        | "LinkedHashMap"
        | "TreeMap"
        | "SortedMap"
        | "NavigableMap"
        | "SequencedMap"
        | "AbstractCollection"
        | "AbstractList"
        | "AbstractSet"
        | "AbstractMap"
        | "RandomAccess" => format!("java.util.{n}"),
        other => other.to_string(),
    }
}

/// The 64-bit value a narrowing integral cast starts from: a floating operand
/// truncates toward zero first, and a `char` (a one-character string) yields its
/// code point.
fn cast_to_i64(v: &Value) -> i64 {
    match v {
        Value::Float(f) => *f as i64,
        other => as_i64(other),
    }
}

/// Coerce a value to `i64`. A one-character string is a `char`, and its code
/// point is its numeric value — `(int) 'a'` is 97.
fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i,
        Value::Float(f) => *f as i64,
        Value::Bool(b) => i64::from(*b),
        other => {
            let s = other.as_str_cow();
            let mut chars = s.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => c as i64,
                _ => s.parse::<i64>().unwrap_or(0),
            }
        }
    }
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

#[cfg(test)]
mod cast_target_tables {
    use super::*;

    /// Every supertype reachable from a checkable target is itself checkable.
    ///
    /// The cast walks [`is_subclass_of`], which climbs [`jdk_supers`] one edge
    /// at a time. If a name on that graph were missing from
    /// [`CHECKABLE_CAST_TARGETS`], the compiler would decline to emit the check
    /// at all for that target and the cast would silently pass — the exact
    /// shape of the gap this list replaced. Walking the closure means the list
    /// cannot fall behind the graph as `jdk_supers` grows.
    #[test]
    fn castable_targets_are_closed_over_the_supertype_graph() {
        let mut missing = Vec::new();
        for t in CHECKABLE_CAST_TARGETS {
            for sup in jdk_supers(t) {
                if !is_checkable_cast_target(sup) {
                    missing.push(format!("{t} -> {sup}"));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "supertypes reachable from a checkable target but not checkable \
             themselves (the cast would decline to check them):\n  {}",
            missing.join("\n  ")
        );
    }

    /// The internal names exist so a shape can carry supertypes without a user
    /// type being able to name one. A program cannot write them, so they must
    /// NOT be castable targets — but they must still carry a supertype line,
    /// or the value wearing one would reach nothing.
    #[test]
    fn the_internal_shape_names_are_unwritable_but_still_carry_supertypes() {
        for internal in [
            "[]",
            "List$immutable",
            "List$fixed",
            "List$sub",
            "Set$immutable",
        ] {
            assert!(
                !is_checkable_cast_target(internal),
                "`{internal}` is not a legal Java type name and must not be a cast target"
            );
            assert!(
                !jdk_supers(internal).is_empty(),
                "`{internal}` carries no supertypes, so a value wearing it reaches nothing"
            );
        }
    }

    /// `Object` is deliberately absent: every value satisfies it, so emitting a
    /// check would only cost ops.
    #[test]
    fn object_is_not_a_checkable_target() {
        assert!(!is_checkable_cast_target("Object"));
    }
}
