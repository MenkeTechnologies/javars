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
//!    never a concatenation; see `java_numeric`.

use fusevm::{NumOp, Value, VM};
use std::cell::{Cell, RefCell};
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

/// Builtin id for Java's 64-bit integral division (`long / long`).
///
/// fusevm's native `Op::Div` computes in `f64` and javars truncates the result
/// with `Op::TruncInt`. For two `int`-width operands that is exact — both fit a
/// `f64` mantissa, and the quotient's distance to the nearest integer is at
/// least `1/|b| >= 2^-31` while the rounding error is at most `|a| * 2^-53 <=
/// 2^-22 * 2^-31`, so the rounding can never cross an integer boundary — and
/// the compiler keeps the native pair there so the JIT can trace it.
///
/// A `long` operand breaks both halves of that argument. Above 2^53 the operand
/// itself no longer survives the round trip, so `9007199254740993L / 1`
/// answered 9007199254740992, and `Long.MAX_VALUE / 2` answered
/// 4611686018427387904 for a true quotient of 4611686018427387903. Separately,
/// `Long.MIN_VALUE / -1` overflows to 2^63 as a float and `TruncInt` *saturates*
/// to `i64::MAX`, where Java wraps back to `Long.MIN_VALUE`. So 64-bit integral
/// division routes here instead and divides in `i64`.
pub const JIDIV: u16 = 745;

/// `new StringBuilder(…)` / `new StringBuffer(…)` — stack `[kind, arg]`, where
/// `kind` is the class's simple name and `arg` is the constructor's single
/// argument (`Undef` for the no-arg form). Which constructor that is is read
/// from the value: an `Int` is the capacity, anything else is the initial
/// content, which is exactly the split Java's `(int)` / `(String)` /
/// `(CharSequence)` overloads make.
///
/// Every method on the builder goes through [`JSTR_DISPATCH`] like any other
/// erased receiver; only allocation needs a builtin of its own, because the
/// object is a host shape rather than a class instance.
pub const JSB_NEW: u16 = 746;

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

/// `Math.round(float)` of the top-of-stack value. `argc == 1`.
///
/// `Math.round` is two methods in Java, and they do not agree: the `double` one
/// answers a `long`, the `float` one an `int`. Only the compiler knows which
/// overload a call site selected, and the difference is observable at the
/// extremes — `Math.round(1.0e20f)` is `Integer.MAX_VALUE` where the `double`
/// overload's answer is `Long.MAX_VALUE`. So a statically-`float` argument
/// routes here instead of through [`JSTATIC_DISPATCH`], the same reason
/// [`JF32_ARITH`] exists.
pub const JF32_ROUND: u16 = 743;

/// `x.getClass()` — the Java *binary* name of a value's runtime class. Stack
/// `[value, arrayDescriptor]` (the descriptor on top); `argc == 2`.
///
/// Distinct from [`JCLASSOF`], which answers the bare class name the compiler's
/// virtual-dispatch chain compares against and the empty string for everything
/// that is not a user instance. That was also what `getClass()` returned, so
/// `new ArrayList<>().getClass().getName()` printed nothing — while
/// `binary_name`, reached only from the `ClassCastException` message, already
/// knew the answer was `java.util.ArrayList`. The two now share it.
///
/// The descriptor argument carries what the *value* cannot: an array's element
/// type is erased at runtime, so `[I` versus `[Ljava.lang.String;` is knowable
/// only from the receiver's static type, which the compiler supplies. It is the
/// empty string when the receiver is not statically an array.
pub const JBINARY_CLASS: u16 = 744;

/// Box a primitive into its `java.lang` wrapper. Stack `[value, class]`
/// (`class` on top, an index into [`BOX_CLASSES`]); `argc == 2`.
///
/// Emitted wherever Java performs a *boxing conversion* — assigning an `int`
/// expression to an `Integer`, `Integer.valueOf(x)`, a cast to a wrapper type.
/// The result is a heap handle, so `==` on two of them is reference identity,
/// which is what the language says it is.
pub const JBOX: u16 = 747;

/// Unbox a wrapper back to its primitive; the identity function on a value that
/// is not boxed. Stack `[value]`; `argc == 1`.
///
/// Emitted wherever Java performs an *unboxing conversion* — assigning an
/// `Integer` expression to an `int`, passing one to a primitive parameter — so
/// that a later `==` between two such variables compares numbers rather than
/// the handles they were copied from.
pub const JUNBOX: u16 = 748;

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

/// Java's string conversion of one value, run with a VM in hand so a user
/// `toString()` override can be called. Stack `[value]`; `argc == 1`.
///
/// The compiler emits this only for a concatenation operand whose static type
/// does not name a user class (an `Object`, an erased `get()`) *and* only when
/// the program declares an override somewhere — see
/// [`Compiler::emit_stringified`](crate::compiler). Without it the operand
/// would reach fusevm's `Op::Add`, whose `NumericHook` takes three values and
/// no VM, so the override could not run and `"" + o` would disagree with
/// `println(o)` for the same object.
pub const JSTRINGIFY: u16 = 742;

/// The overload-width codes a `Math` static that is overloaded on width takes
/// as its extra operand, shared with the compiler.
///
/// `Math.addExact` and friends are declared at `int` *and* `long`, and
/// `Math.clamp` at four widths; the two integral ones disagree exactly where
/// the method is interesting (`Math.addExact(2000000000, 2000000000)` throws
/// for the `int` overload and answers 4000000000 for the `long` one). Java
/// resolves that from the arguments' static types, which only the compiler has,
/// so it sends the answer along.
pub mod width {
    /// `int`.
    pub const INT: i64 = 0;
    /// `long`.
    pub const LONG: i64 = 1;
    /// `float` — `Math.clamp` only.
    pub const FLOAT: i64 = 2;
    /// `double` — `Math.clamp` only.
    pub const DOUBLE: i64 = 3;
}

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
        /// Key -> position accelerator; see [`KeyIndex`].
        index: KeyIndex,
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
        /// Element -> position accelerator; see [`KeyIndex`].
        index: KeyIndex,
    },
    /// A `java.lang.StringBuilder` (or `StringBuffer`) — the mutable character
    /// sequence, which `+` concatenation cannot stand in for once a program
    /// builds one in a loop.
    ///
    /// `cap` tracks `capacity()` rather than Rust's own allocation, because it
    /// is *observable*: the JDK starts at 16 (plus the initial content's
    /// length), and grows to `2 * old + 2` or the required size, whichever is
    /// larger. A `Vec`'s growth policy would answer a different number.
    Builder {
        s: String,
        /// `s.chars().count()`, maintained by every mutation.
        ///
        /// Recomputing it per call made `append` — the one method a builder
        /// exists for — walk the whole buffer every time, so building a string
        /// of n characters cost O(n²): 400k appends took 9.96s of CPU against
        /// 0.30s for 50k, where linear would be 2.4s. It also answers "is this
        /// buffer all ASCII?" in O(1) (`s.len() == len`, since UTF-8 spends one
        /// byte per character exactly then), which is what lets `charAt` and
        /// the rest index by byte instead of decoding to the i-th character.
        len: usize,
        cap: usize,
        /// `true` for a `StringBuffer`, which differs from `StringBuilder` only
        /// in its class name here: javars runs one thread, so the synchronized
        /// methods are unobservable.
        buffer: bool,
    },
    /// A boxed primitive — `java.lang.Integer` and its seven siblings.
    ///
    /// Java's wrapper types are *reference* types, and `==` on two of them is
    /// reference identity, not value equality. A bare [`Value::Int`] cannot
    /// carry an identity, so `Integer a = 128, b = 128; a == b` answered `true`
    /// where Java answers `false`. Putting the box on the heap gives it the
    /// identity the language says it has, and gives it a *class* besides:
    /// `Integer.valueOf(1).equals(Long.valueOf(1))` is `false` in Java and one
    /// `Value::Int` could not tell the two apart.
    ///
    /// `v` is the primitive it wraps — `Value::Int` for the five integral
    /// classes, `Value::Float` for `Float`/`Double`, `Value::Bool` for
    /// `Boolean`. Every numeric surface unboxes it (see [`unboxed`]), so the
    /// box is observable only where Java makes it observable: `==`, `equals`,
    /// `hashCode`, and `getClass`.
    /// A marker: the payload lives in [`BOXES`], not here.
    ///
    /// Both halves — the class and the primitive — are read from surfaces that
    /// already hold a borrow of this heap: a boxed element inside a `List` is
    /// compared while the list itself is borrowed, so reading the box out of
    /// `HEAP` would be a re-entrant borrow and panic. A separate `RefCell`
    /// cannot collide with this one. The slot is still allocated here so a box
    /// owns a handle no other object can be given, which is what makes `==` on
    /// two of them mean anything.
    Boxed,
}

/// The eight wrapper classes, indexed by the code the compiler passes [`JBOX`].
///
/// The order is fixed: `JBOX`'s first argument is an index into this table, so
/// reordering it would silently rebox every literal as a different class.
/// `Boolean` is deliberately absent. Its cache covers `true` and `false` both,
/// so every autoboxed `Boolean` pair Java can produce is already the same
/// object and `==` on them is always `true` — boxing it would buy no fidelity
/// while putting a heap handle where the VM tests truth, which is not a numeric
/// surface and so would not unbox.
pub const BOX_CLASSES: [&str; 7] = [
    "Integer",
    "Long",
    "Short",
    "Byte",
    "Character",
    "Float",
    "Double",
];

/// The index into [`BOX_CLASSES`] a wrapper class name boxes as, or `None` for
/// a name that is not a wrapper. The compiler and the host share this so a
/// spelling can never mean one class on one side and another on the other.
pub fn box_class_code(name: &str) -> Option<i64> {
    BOX_CLASSES
        .iter()
        .position(|c| *c == name)
        .map(|i| i as i64)
}

/// A hashable stand-in for the [`Value`]s a `Map` key or a `Set` element can
/// take, used only to bucket them inside [`KeyIndex`].
///
/// Two values that [`value_eq`] calls equal MUST produce the same key, or a
/// lookup would miss where the scan it replaces would hit. Equal keys need not
/// mean equal values — the candidates in a bucket are still checked with
/// `value_eq` — so a collision is free and only a *missing* one would be a bug.
/// `index_key` therefore declines (answering `None`, which makes the caller
/// scan) for exactly the values where the correspondence is not provable.
#[derive(PartialEq, Eq, Hash)]
enum IndexKey {
    Int(i64),
    Str(String),
    Bool(bool),
    Obj(u32),
    Null,
}

/// The magnitude below which an `i64` and an `f64` denote the same integers
/// one-for-one. Above it several `i64`s round to one `f64`, so
/// `value_eq(Int, Float)` can hold for a pair whose [`IndexKey`]s differ.
const EXACT_INT_FLOAT: f64 = 9_007_199_254_740_992.0; // 2^53

/// The bucket `v` belongs in, or `None` when no provably-correct one exists.
fn index_key(v: &Value) -> Option<IndexKey> {
    Some(match v {
        Value::Int(n) => IndexKey::Int(*n),
        Value::Str(s) => IndexKey::Str(s.as_str().to_string()),
        Value::Bool(b) => IndexKey::Bool(*b),
        // A box buckets with the primitive it wraps, not with its handle, so
        // `map.put(Integer.valueOf(1), x)` and `map.get(1)` meet. The bucket is
        // an accelerator — every candidate is still confirmed with `value_eq`,
        // which is where the class check lives — so sharing one is free.
        Value::Obj(id) => match unboxed(v) {
            Some(inner) => return index_key(&inner),
            None => IndexKey::Obj(*id),
        },
        Value::Undef => IndexKey::Null,
        // `value_eq` compares an integral and a floating value numerically, so
        // a `Float` has to land in the same bucket the equal `Int` does. That
        // is only sound where the two representations agree exactly: below
        // 2^53 an integral `f64` names one `i64` and vice versa. Everything
        // else — a fraction, a NaN, a magnitude past 2^53 — declines.
        Value::Float(f) => {
            let f = *f;
            if f.fract() != 0.0 || f.abs() >= EXACT_INT_FLOAT {
                return None;
            }
            IndexKey::Int(f as i64)
        }
        _ => return None,
    })
}

/// The key -> position accelerator a `Map` and a `Set` carry.
///
/// Both store their entries in a `Vec` (insertion order is what every
/// `toString` and every iteration is derived from), so finding a key was a
/// linear scan and `n` insertions cost O(n²): 20k `HashMap.put`s took 1.53s
/// against 0.10s for 5k. Java's is O(1), and a program that fills a map in a
/// loop is ordinary.
///
/// The index is an accelerator, never the authority: a hit is confirmed with
/// [`value_eq`] against the candidates in the bucket, and anything it cannot
/// represent falls back to the scan it replaces. Two conditions force that
/// fallback — a stored key with no [`IndexKey`] (`unindexed > 0`), and a
/// structural change that moved existing positions (`dirty`), which is repaired
/// by one rebuild on the next lookup rather than by tracking the shift.
struct KeyIndex {
    by_key: HashMap<IndexKey, Vec<usize>>,
    /// Stored keys with no `IndexKey`. While non-zero the index is incomplete.
    unindexed: usize,
    /// Positions have moved since the index was built.
    dirty: bool,
}

impl Default for KeyIndex {
    /// A fresh index is **stale**, not empty. A collection can be constructed
    /// already holding entries (`new HashMap<>(other)`, `Set.of(…)`, a `keySet`
    /// view), and an index that claimed to be complete would then answer
    /// "absent" for every one of them. Starting dirty makes the first lookup
    /// build it from whatever the collection actually holds, so no construction
    /// site has to remember to.
    fn default() -> Self {
        KeyIndex {
            by_key: HashMap::new(),
            unindexed: 0,
            dirty: true,
        }
    }
}

impl KeyIndex {
    /// Record the key now sitting at `at`, which must be the last position.
    fn push(&mut self, k: &Value, at: usize) {
        match index_key(k) {
            Some(key) => self.by_key.entry(key).or_default().push(at),
            None => self.unindexed += 1,
        }
    }

    /// Mark every recorded position stale. The next lookup rebuilds.
    fn invalidate(&mut self) {
        self.dirty = true;
    }

    fn rebuild<'a>(&mut self, keys: impl Iterator<Item = &'a Value>) {
        self.by_key.clear();
        self.unindexed = 0;
        self.dirty = false;
        for (i, k) in keys.enumerate() {
            self.push(k, i);
        }
    }

    /// Where `q` sits among `items`, whose key is read by `key_of`.
    ///
    /// `None` means the index cannot answer and the caller must scan;
    /// `Some(None)` is a confirmed absence.
    fn find<T>(
        &self,
        items: &[T],
        key_of: impl Fn(&T) -> &Value,
        q: &Value,
    ) -> Option<Option<usize>> {
        if self.dirty || self.unindexed > 0 {
            return None;
        }
        let key = index_key(q)?;
        Some(self.by_key.get(&key).and_then(|cands| {
            cands
                .iter()
                .copied()
                .find(|&i| items.get(i).is_some_and(|it| value_eq(key_of(it), q)))
        }))
    }
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
    /// Simple class name → Java's binary name (`Outer$Nested`), populated by
    /// [`set_binary_names`] before a run. Only nested types have an entry that
    /// differs from the key.
    static BINARY: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
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
    /// The wrapper caches the JLS mandates: `(class index, value) -> handle`.
    ///
    /// JLS 5.1.7 requires `valueOf` to return the *same* object for every
    /// `boolean`, every `char` in `0..=127`, and every `byte`, `short`, `int`
    /// and `long` in `-128..=127`. That requirement is the whole reason
    /// `Integer a = 127, b = 127; a == b` is `true` while the same pair at 128
    /// is `false`, so the cache is not an optimization here — it is the
    /// observable behaviour.
    static BOX_CACHE: RefCell<HashMap<(usize, i64), u32>> = RefCell::new(HashMap::new());
    /// Every live box: handle -> (wrapper class, the primitive it wraps).
    ///
    /// Separate from `HEAP` on purpose — see [`HostObj::Boxed`]. Indexed by the
    /// handle rather than hashed on it, because unboxing is on the path of every
    /// arithmetic operation a wrapper takes part in and every erased read.
    static BOXES: RefCell<Vec<Option<(&'static str, Value)>>> = const { RefCell::new(Vec::new()) };
}

/// Box `v` as the wrapper class at index `code` in [`BOX_CLASSES`], returning
/// the handle.
///
/// Values in the JLS-mandated cache range share one handle; everything else
/// gets a fresh one, which is exactly the identity Java gives it.
fn box_value(code: usize, v: Value) -> Value {
    let class = BOX_CLASSES[code];
    // Only the integral classes, `Character` and `Boolean` have a cache, and
    // only over the range the JLS names. `Float`/`Double` have none at all —
    // `Double d1 = 1.0, d2 = 1.0; d1 == d2` is `false` for every value.
    let cached = match class {
        "Integer" | "Long" | "Short" | "Byte" => match &v {
            Value::Int(n) if (-128..=127).contains(n) => Some(*n),
            _ => None,
        },
        "Character" => match &v {
            Value::Int(n) if (0..=127).contains(n) => Some(*n),
            _ => None,
        },
        _ => None,
    };
    let Some(key) = cached else {
        return Value::Obj(alloc_box(class, v));
    };
    if let Some(id) = BOX_CACHE.with(|c| c.borrow().get(&(code, key)).copied()) {
        return Value::Obj(id);
    }
    let id = alloc_box(class, v);
    BOX_CACHE.with(|c| c.borrow_mut().insert((code, key), id));
    Value::Obj(id)
}

/// Give a box a heap handle of its own and record its payload in [`BOXES`].
fn alloc_box(class: &'static str, v: Value) -> u32 {
    let id = heap_alloc(HostObj::Boxed);
    BOXES.with(|b| {
        let mut b = b.borrow_mut();
        b.resize(id as usize + 1, None);
        b[id as usize] = Some((class, v));
    });
    id
}

/// The primitive inside a boxed wrapper, or `None` for anything else.
///
/// Every numeric, rendering, hashing and equality surface calls this first, so
/// a box that reaches one behaves as the primitive it wraps. That is what makes
/// the model safe to introduce incrementally: a site that has not been taught
/// about boxes answers exactly as it did before rather than answering wrongly.
fn unboxed(v: &Value) -> Option<Value> {
    let Value::Obj(id) = v else {
        return None;
    };
    BOXES.with(|b| {
        b.borrow()
            .get(*id as usize)?
            .as_ref()
            .map(|(_, v)| v.clone())
    })
}

/// The wrapper class of a boxed value, or `None` for anything else.
fn box_class(v: &Value) -> Option<&'static str> {
    let Value::Obj(id) = v else {
        return None;
    };
    BOXES.with(|b| b.borrow().get(*id as usize)?.as_ref().map(|(c, _)| *c))
}

/// `v` with any box removed — the identity function on everything else.
fn deboxed(v: &Value) -> Value {
    unboxed(v).unwrap_or_else(|| v.clone())
}

/// `to_int` / `to_float` **through any box**.
///
/// fusevm's own converters see a boxed wrapper as the heap handle it is and
/// answer 0, silently: `arr[anInteger]` indexed element 0 and
/// `String.format("%d", anInteger)` printed 0. Every host builtin that wants a
/// number out of a value therefore asks here instead. On an unboxed value the
/// answer is fusevm's exactly, so converting a call site cannot change what it
/// already answered — which is why the conversion could be mechanical.
trait JavaNumeric {
    /// The value as an `i64`, unboxing a wrapper first.
    fn jint(&self) -> i64;
    /// The value as an `f64`, unboxing a wrapper first.
    fn jfloat(&self) -> f64;
}

impl JavaNumeric for Value {
    fn jint(&self) -> i64 {
        Value::to_int(&deboxed(self))
    }

    fn jfloat(&self) -> f64 {
        Value::to_float(&deboxed(self))
    }
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
    // The wrapper cache holds heap handles, so it has to go with the heap it
    // indexes into: a surviving entry would name a slot the next program's own
    // objects occupy, and `Integer.valueOf(1)` would answer someone else's list.
    BOX_CACHE.with(|c| c.borrow_mut().clear());
    BOXES.with(|b| b.borrow_mut().clear());
    SUPERS.with(|s| s.borrow_mut().clear());
    BINARY.with(|b| b.borrow_mut().clear());
    PENDING.with(|p| *p.borrow_mut() = None);
    EXC_ENABLED.with(|e| e.set(false));
    ARGV.with(|a| a.borrow_mut().clear());
    // All three are keyed to the OUTGOING chunk: each gate answers whether
    // *that* chunk declared an override, and an entry ip indexes its ops.
    // Carrying any of them into the next program would render — or compare —
    // through unrelated bytecode.
    USER_TOSTRING.with(|c| c.set(None));
    USER_EQUALS.with(|c| c.set(None));
    MEMBER_ENTRY.with(|t| t.borrow_mut().clear());
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
    ///
    /// `class` is stored as the *simple* name, because that is the only spelling
    /// the rest of the machinery reads: a `catch` clause names its type simply,
    /// [`crate::prelude::qualified_throwable`] recognises only the simple name,
    /// and the uncaught report qualifies it on the way out. A call site that
    /// wrote the qualified form got a throwable whose class matched no `catch`
    /// clause and whose report was left unqualified — `Double.parseDouble("q")`
    /// aborted with `javars: Exception in thread "main"
    /// java.lang.NumberFormatException: …` where the `Float.parseFloat` arm two
    /// lines away, spelled simply, was catchable. Both spellings now arrive
    /// here as one, so the defect cannot be reintroduced at a new call site.
    fn java(class: &'static str, msg: impl Into<String>) -> Self {
        Fault {
            class: class.rsplit('.').next().unwrap_or(class),
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

/// Record each user class's Java *binary* name (`Outer$Nested`). Call before
/// running the chunk; read by `qualified_or_binary`.
pub fn set_binary_names(map: HashMap<String, String>) {
    BINARY.with(|b| *b.borrow_mut() = map);
}

/// The name `getClass().getName()` reports for `class`: the qualified form for a
/// modeled JDK type (`java.lang.Object`), the binary form for a user one
/// (`Outer$Nested`), and the simple name for a top-level user class.
///
/// javars flattens nesting into one namespace, so the simple name is what every
/// value carries at runtime; the nesting only has to be recovered at the two
/// places Java shows it — `getName()` and the default `toString()`.
fn qualified_or_binary(class: &str) -> String {
    if let Some(q) = crate::prelude::qualified_class(class) {
        return q;
    }
    BINARY.with(|b| {
        b.borrow()
            .get(class)
            .cloned()
            .unwrap_or_else(|| class.to_string())
    })
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
        // The six `Number` wrappers. Only `Integer` and `Double` were listed
        // while those were the only two classes a bare `Value::Int`/`Float`
        // could answer as; a boxed value now names its own class, so
        // `Long.valueOf(1) instanceof Number` has a class to walk from.
        "Integer" | "Double" | "Long" | "Short" | "Byte" | "Float" => &["Number", "Comparable"],
        "Number" => &["Serializable"],
        "Boolean" | "Character" => &["Comparable", "Serializable"],
        "Enum" => &["Comparable", "Serializable"],
        // Both builders extend the package-private `AbstractStringBuilder`,
        // which is what carries `CharSequence` and `Appendable`; only
        // `Comparable` and `Serializable` are declared on the concrete classes.
        "StringBuilder" | "StringBuffer" => {
            &["AbstractStringBuilder", "Comparable", "Serializable"]
        }
        "AbstractStringBuilder" => &["CharSequence", "Appendable"],
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
    vm.register_builtin(JIDIV, b_idiv);
    vm.register_builtin(JUSHR, b_ushr);
    vm.register_builtin(JCAST, b_cast);
    vm.register_builtin(JCHR_STR, b_chr_str);
    vm.register_builtin(JCHECKCAST, b_checkcast);
    vm.register_builtin(JF32, b_f32);
    vm.register_builtin(JF32_STR, b_f32_str);
    vm.register_builtin(JF32_ARITH, b_f32_arith);
    vm.register_builtin(JF32_ROUND, b_f32_round);
    vm.register_builtin(JBINARY_CLASS, b_binary_class);
    vm.register_builtin(JBOX, b_box);
    vm.register_builtin(JUNBOX, b_unbox);
    vm.register_builtin(JCOMPARE_TO, b_compare_to);
    vm.register_builtin(JFORMAT, b_format);
    vm.register_builtin(JSTRINGIFY, b_stringify);
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
    vm.register_builtin(JSB_NEW, b_sb_new);
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
    let depth = args.first().map(|v| v.jint()).unwrap_or(0).max(0) as usize;
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
                // [`qualified_or_binary`], not `qualified_throwable`: the latter
                // answers `None` for a user-defined throwable and left the
                // report printing the *simple* name, so an uncaught
                // `class MyEx extends RuntimeException` nested in `T` read
                // `Exception in thread "main" MyEx: boom` where Java prints the
                // binary name `T$MyEx: boom`. The modeled throwables are
                // unaffected — `qualified_or_binary` consults the same table
                // first.
                let name = qualified_or_binary(class);
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

/// [`JBINARY_CLASS`] — `x.getClass()`, as the binary name `getName()` reports.
///
/// Everything the answer depends on already existed: [`value_class`] names the
/// runtime class of every shape the value model has, and [`binary_name`] maps
/// that to the JDK's own spelling — including the private classes `List.of`
/// and `Arrays.asList` return. Only `getClass()` was not asking them.
/// [`JBOX`] — box a primitive into the wrapper class named by the code on top
/// of the stack.
fn b_box(vm: &mut VM, argc: u8) -> Value {
    let args = pop_args(vm, argc);
    let code = args
        .get(1)
        .map(as_i64)
        .unwrap_or(0)
        .clamp(0, BOX_CLASSES.len() as i64 - 1) as usize;
    let v = args.first().cloned().unwrap_or(Value::Undef);
    // `null` is not boxed. Java's boxing conversion applies to a *primitive*,
    // and the one place a null can reach a boxing site is an already-reference
    // expression the compiler could not type; boxing it would turn `null` into
    // an object and make `x == null` false.
    if matches!(v, Value::Undef) {
        return v;
    }
    box_value(code, v)
}

/// [`JUNBOX`] — the primitive inside a wrapper, or the value unchanged.
fn b_unbox(vm: &mut VM, argc: u8) -> Value {
    let args = pop_args(vm, argc);
    let v = args.first().cloned().unwrap_or(Value::Undef);
    deboxed(&v)
}

/// Java `==` on two heap references: the same handle.
///
/// Reached from [`numeric_hook`], which routes a pair of handles here before it
/// considers them as numbers. That ordering is the whole model: two boxed
/// wrappers holding 128 are *numerically* equal and Java still answers `false`,
/// because `==` on two references compares the references.
fn ref_eq(a: &Value, b: &Value) -> bool {
    matches!((a, b), (Value::Obj(x), Value::Obj(y)) if x == y)
}

fn b_binary_class(vm: &mut VM, argc: u8) -> Value {
    let args = pop_args(vm, argc);
    let array_type = args.get(1).map(|h| h.as_str_cow().into_owned());
    let Some(v) = args.first() else {
        return Value::str("");
    };
    // An array's element type is erased, so the compiler's static spelling is
    // the only source for `[I` / `[Ljava.lang.String;`.
    if matches!(value_class(v).as_deref(), Some("[]")) {
        return Value::str(
            array_type
                .as_deref()
                .and_then(array_descriptor)
                .unwrap_or_default(),
        );
    }
    // A lambda keeps the dispatch sentinel: Java names one
    // `Class$$Lambda/0x…`, which is not reproducible (BUGS.md).
    match value_class(v) {
        Some(class) if class == LAMBDA_CLASS => Value::str(class),
        Some(class) => Value::str(binary_name(&class, v).unwrap_or(class)),
        None => Value::str(""),
    }
}

/// The JVM field descriptor `Class.getName()` reports for the array type `ty`,
/// spelled the way Java source does (`int[]`, `String[][]`).
///
/// Java names an array by its descriptor rather than by its source spelling —
/// `int[]` is `[I`, `String[][]` is `[[Ljava.lang.String;` — in the dotted form
/// `getName()` uses rather than the slashed class-file one. A reference
/// component goes through [`qualified_or_binary`], so a nested user class
/// resolves to `[LT$A;` and not `[LA;`.
fn array_descriptor(ty: &str) -> Option<String> {
    let component = ty.strip_suffix("[]")?;
    if let Some(inner) = array_descriptor(component) {
        return Some(format!("[{inner}"));
    }
    Some(format!(
        "[{}",
        match component {
            "int" => "I".to_string(),
            "long" => "J".to_string(),
            "short" => "S".to_string(),
            "byte" => "B".to_string(),
            "char" => "C".to_string(),
            "double" => "D".to_string(),
            "float" => "F".to_string(),
            "boolean" => "Z".to_string(),
            other => format!("L{};", jdk_name(other)),
        }
    ))
}

/// The simple name `Class.getSimpleName()` reports, given a binary name: the
/// text after the last `$` of a nested class, else after the last `.` of a
/// package-qualified one.
///
/// An array is the exception — Java answers with the *source* spelling of the
/// type (`int[]`, `String[]`), not with the descriptor `getName()` gave — so a
/// descriptor is decoded back rather than truncated.
fn simple_class_name(binary: &str) -> String {
    if let Some(component) = binary.strip_prefix('[') {
        // A one-character primitive descriptor is only a descriptor *inside* an
        // array type. At the top level `B` is the ordinary binary name of a
        // class the program called `B`, and answering `byte` for it renamed
        // every user class whose name is one of the eight descriptor letters.
        let elem = descriptor_primitive(component)
            .map(str::to_string)
            .unwrap_or_else(|| simple_class_name(component));
        return format!("{elem}[]");
    }
    if let Some(reference) = binary.strip_prefix('L').and_then(|r| r.strip_suffix(';')) {
        return simple_class_name(reference);
    }
    let after_package = binary.rsplit('.').next().unwrap_or(binary);
    after_package
        .rsplit('$')
        .next()
        .unwrap_or(after_package)
        .to_string()
}

/// The Java source name of a primitive field descriptor, for the array half of
/// [`simple_class_name`].
fn descriptor_primitive(d: &str) -> Option<&'static str> {
    Some(match d {
        "I" => "int",
        "J" => "long",
        "S" => "short",
        "B" => "byte",
        "C" => "char",
        "D" => "double",
        "F" => "float",
        "Z" => "boolean",
        _ => return None,
    })
}

// ── Lambdas ─────────────────────────────────────────────────────────────────

/// [`JMAKE_CLOSURE`] — snapshot the captures and register the closure.
fn b_make_closure(vm: &mut VM, _argc: u8) -> Value {
    let ncap = vm.stack.pop().unwrap_or(Value::Undef).jint() as usize;
    let params = vm.stack.pop().unwrap_or(Value::Undef).jint() as u8;
    let name_idx = vm.stack.pop().unwrap_or(Value::Undef).jint() as u16;
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
    // A wrapper hashes as its primitive. The two arms below already split
    // `Integer` from `Long` by magnitude, which is exact: an `Integer` can hold
    // nothing outside `i32`, so a value that does is necessarily a `Long`.
    if let Some(inner) = unboxed(v) {
        return java_hash(&inner);
    }
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
        // `Double.hashCode` folds `doubleToLongBits` the way `Long` does. The
        // *canonical* bits, not the raw ones: `doubleToLongBits` collapses
        // every `NaN` encoding to one pattern, so two `NaN`s hash alike — which
        // they must, since `Double.equals` already calls them equal.
        Value::Float(f) => {
            let bits = canonical_bits(*f);
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
    // `equals` between two wrappers is class-sensitive: `Integer.valueOf(1)
    // .equals(Long.valueOf(1))` is `false` in Java, and telling those two apart
    // is what the box's class tag is for. A box against a *bare* value is
    // compared numerically, because a bare `Value::Int` carries no class to
    // disagree with — it is whichever wrapper the context autoboxed it into.
    match (box_class(a), box_class(b)) {
        (Some(x), Some(y)) if x != y => return false,
        (Some(_), _) | (_, Some(_)) => return value_eq(&deboxed(a), &deboxed(b)),
        _ => {}
    }
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
                // `Double.equals` is *not* `==`: it compares
                // `doubleToLongBits`, so `NaN` equals itself and `-0.0` does
                // not equal `0.0` — which is what decides whether a
                // `HashSet<Double>` keeps two `NaN`s apart and whether
                // `list.contains(Double.NaN)` can ever answer true. The total
                // order [`float_compare`] already implements is the same
                // predicate, so the two cannot drift.
                (Value::Float(_), Value::Float(_)) => float_compare(a.jfloat(), b.jfloat()) == 0,
                _ => a.jfloat() == b.jfloat(),
            }
        }
        _ => false,
    }
}

/// Java's `q.equals(other)` where `q` may be a class instance whose class (or
/// an ancestor) declares one — the comparison every collection membership test
/// performs internally.
///
/// The receiver is `q`, not `other`, because that is the direction the JDK
/// calls in: `ArrayList.indexOf(o)` runs `o.equals(element)`, `HashMap.getNode`
/// runs `key.equals(storedKey)`, and `HashSet.add(e)` runs `e.equals(stored)`.
/// An asymmetric user `equals` therefore answers here exactly as it does there.
/// With no user body in play this is [`value_eq`].
fn eq_call(vm: &mut VM, q: &Value, other: &Value) -> bool {
    match user_equals(vm, q) {
        Some((id, entry)) => run_equals(vm, entry, id, other),
        None => value_eq(q, other),
    }
}

/// `java.util.Objects.equals(a, b)` — `a == b || (a != null && a.equals(b))`.
/// The null half is what separates it from [`eq_call`]: `Objects.equals(null,
/// null)` is `true` and `Objects.equals(null, x)` is `false` without calling
/// anything. It is also what a `record`'s derived `equals` uses for a reference
/// component.
fn objects_equals(vm: &mut VM, a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Undef, Value::Undef) => true,
        (Value::Undef, _) => false,
        _ => eq_call(vm, a, b),
    }
}

/// The handle and `equals(Object)` entry ip of a value that is a class instance
/// whose class resolves a user body; `None` for everything else — a scalar, a
/// collection handle, or an instance that inherits `java.lang.Object`'s
/// identity `equals`.
fn user_equals(vm: &VM, v: &Value) -> Option<(u32, usize)> {
    let Value::Obj(id) = v else {
        return None;
    };
    if !any_user_equals(vm) {
        return None;
    }
    let class = instance_class(v)?;
    Some((*id, equals_entry(vm, &class)?))
}

/// Run a user `equals(Object)` body with `id` as the receiver and return what it
/// answered.
///
/// A throwable already in flight stops the call, for the same reason
/// [`run_tostring`] stops: the enclosing frame is unwinding and a body's side
/// effects must not run twice. One raised *by* the body leaves `PENDING` set for
/// the calling builtin to surface, and the verdict is discarded with it.
fn run_equals(vm: &mut VM, entry: usize, id: u32, other: &Value) -> bool {
    if PENDING.with(|p| p.borrow().is_some()) {
        return false;
    }
    let stack_base = vm.stack.len();
    vm.stack.push(Value::Obj(id));
    vm.stack.push(other.clone());
    matches!(run_sub(vm, entry, stack_base), Value::Bool(true))
}

/// The element comparisons one collection call needs a user `equals()` for,
/// resolved *before* the heap borrow the call itself takes.
///
/// Java compares a collection's elements with `equals`, not with identity, and
/// running a user body needs `&mut VM` with no borrow of the heap slab
/// outstanding — the body reads its own fields, and may allocate. So the
/// comparisons happen up front, in [`eq_plan`], and the borrowed section
/// consumes plain data. `None` — every program that declares no `equals` — puts
/// each site back on [`value_eq`] and the code path javars has always taken.
enum EqPlan {
    /// The position `args[0]` was found at, as an index into the receiver's
    /// *storage* order — which is what the borrowed section indexes. For a `Map`
    /// the search ran over its keys, except under `containsValue`, where it ran
    /// over its values.
    Index(Option<usize>),
    /// `List.equals(other)`: the whole pairwise verdict.
    Same(bool),
    /// `Set.addAll(c)`: the elements of `c` not already present, in order,
    /// counting the ones accepted earlier in the same call.
    Fresh(Vec<Value>),
}

/// Where `q` sits in `items`: the position a user `equals()` already found, or
/// javars's value model when no user body was in play.
///
/// The plan scanned in the direction the calling method scans and stopped at the
/// first hit, so `from_end` only selects the fallback's direction —
/// `List.lastIndexOf` is the one caller that passes `true`.
fn eq_index(eq: Option<&EqPlan>, items: &[Value], q: &Value, from_end: bool) -> Option<usize> {
    match eq {
        Some(EqPlan::Index(at)) => *at,
        _ if from_end => items.iter().rposition(|x| value_eq(x, q)),
        _ => items.iter().position(|x| value_eq(x, q)),
    }
}

/// The receiver's elements in *storage* order — the order the borrowed section
/// indexes. Distinct from [`sequence_items`], which presents a `Set` in its
/// iteration order and so would misalign the verdict vector. A `Map` answers
/// with its keys.
fn eq_elements(recv: &Value) -> Option<Vec<Value>> {
    let Value::Obj(id) = recv else {
        return None;
    };
    if let Some(window) = sublist_items(*id as usize) {
        return window.ok();
    }
    HEAP.with(|h| match h.borrow().get(*id as usize) {
        Some(HostObj::List { items, .. }) | Some(HostObj::Set { items, .. }) => Some(items.clone()),
        Some(HostObj::Map { entries, .. }) => {
            Some(entries.iter().map(|(k, _)| k.clone()).collect())
        }
        _ => None,
    })
}

/// A `Map`'s values in storage order, for `containsValue`.
fn eq_map_values(recv: &Value) -> Option<Vec<Value>> {
    let Value::Obj(id) = recv else {
        return None;
    };
    HEAP.with(|h| match h.borrow().get(*id as usize) {
        Some(HostObj::Map { entries, .. }) => {
            Some(entries.iter().map(|(_, v)| v.clone()).collect())
        }
        _ => None,
    })
}

/// Which collection shape a handle is, for deciding whether a method name
/// compares by value at all — `List.remove(int)` removes by index where
/// `Set.remove(Object)` removes by equality, and `List.add` appends where
/// `Set.add` de-duplicates. A `Set`/`Map` carries its iteration order too,
/// because that is what says whether it is a *hash* container.
enum EqShape {
    List,
    Set(Order),
    Map(Order),
}

fn eq_shape(recv: &Value) -> Option<EqShape> {
    let Value::Obj(id) = recv else {
        return None;
    };
    if is_sublist(*id as usize) {
        return Some(EqShape::List);
    }
    HEAP.with(|h| match h.borrow().get(*id as usize) {
        Some(HostObj::List { .. }) => Some(EqShape::List),
        Some(HostObj::Set { order, .. }) => Some(EqShape::Set(*order)),
        Some(HostObj::Map { order, .. }) => Some(EqShape::Map(*order)),
        _ => None,
    })
}

/// Whether a *hash* container may trust `class`'s `equals`.
///
/// `HashMap`/`HashSet` find an element only when its `hashCode` puts it in the
/// bucket being searched, so a class that overrides `equals` and leaves
/// `hashCode` alone is genuinely not found by Java either — two instances get
/// distinct JVM identity hashes and never meet. javars cannot compute a JVM
/// identity hash, so the *declaration* is the signal: a class that declares
/// `hashCode`, or a `record`/`enum` whose derived one is consistent by
/// construction, is trusted; anything else keeps the identity comparison Java
/// effectively performs. `ArrayList` hashes nothing and so asks this of nobody.
fn hash_consistent(vm: &VM, class: &str) -> bool {
    member_entry(vm, class, HASHCODE_SUFFIX).is_some()
        || is_subclass_of(class, "Record")
        || is_subclass_of(class, "Enum")
}

/// Whether an iteration order belongs to a hash-bucketed container.
/// `Order::Sorted` is a `TreeMap`/`TreeSet`, which locates by `compareTo` rather
/// than by `equals` — a different question, and one javars does not answer here.
fn is_hashed(order: Order) -> bool {
    matches!(order, Order::Hash | Order::Insertion)
}

/// Resolve the comparisons a user `equals()` decides for one collection call.
///
/// Only the methods whose answer Java takes from `equals` build a plan, and only
/// when a body is actually reachable from the value being compared — so a
/// program with no `equals`, or one whose elements are `String`s and boxed
/// primitives, pays a flag read and nothing else.
fn eq_plan(
    vm: &mut VM,
    recv: &Value,
    method: &str,
    args: &[Value],
    arg_seqs: &[Option<Vec<Value>>],
) -> Option<EqPlan> {
    if !any_user_equals(vm) {
        return None;
    }
    let shape = eq_shape(recv)?;
    // `AbstractList.equals` compares position by position with *this* list's
    // element as the receiver, so the bodies it reaches are the receiver's.
    if method == "equals" && args.len() == 1 && matches!(shape, EqShape::List) {
        let mine = eq_elements(recv)?;
        let other = arg_seqs.first()?.clone()?;
        if !mine.iter().any(|v| user_equals(vm, v).is_some()) {
            return None;
        }
        if mine.len() != other.len() {
            return Some(EqPlan::Same(false));
        }
        let mut same = true;
        for (a, b) in mine.iter().zip(&other) {
            if !eq_call(vm, a, b) {
                same = false;
                break;
            }
        }
        return Some(EqPlan::Same(same));
    }
    // `Set.addAll` asks the membership question once per added element, against
    // a set that grows as the earlier ones are accepted.
    if method == "addAll" && args.len() == 1 && matches!(shape, EqShape::Set(o) if is_hashed(o)) {
        let mut items = eq_elements(recv)?;
        let add = arg_seqs.first()?.clone()?;
        if !add.iter().any(|v| trusted_equals(vm, v, true)) {
            return None;
        }
        let mut fresh = Vec::new();
        for v in add {
            let mut seen = false;
            for x in &items {
                if eq_call(vm, &v, x) {
                    seen = true;
                    break;
                }
            }
            if !seen {
                items.push(v.clone());
                fresh.push(v);
            }
        }
        return Some(EqPlan::Fresh(fresh));
    }
    // Everything else is "where does `args[0]` sit in the receiver".
    let (by_value, hashed) = match shape {
        EqShape::List => (
            matches!(
                (method, args.len()),
                ("contains", 1) | ("indexOf", 1) | ("lastIndexOf", 1) | ("removeObject", 1)
            ),
            false,
        ),
        EqShape::Set(order) => (
            matches!(
                (method, args.len()),
                ("contains", 1) | ("add", 1) | ("remove", 1)
            ) && is_hashed(order),
            true,
        ),
        EqShape::Map(order) => (
            matches!(
                (method, args.len()),
                ("get", 1)
                    | ("getOrDefault", 2)
                    | ("containsKey", 1)
                    | ("containsValue", 1)
                    | ("remove", 1)
                    | ("put", 2)
                    | ("putIfAbsent", 2)
            ) && is_hashed(order),
            true,
        ),
    };
    if !by_value {
        return None;
    }
    let q = args.first()?;
    if !trusted_equals(vm, q, hashed) {
        return None;
    }
    let against = if method == "containsValue" {
        eq_map_values(recv)?
    } else {
        eq_elements(recv)?
    };
    // The scan stops at the first hit, in the direction the calling method
    // scans, so a user body's side effects fire exactly as often as Java's do.
    let at = if method == "lastIndexOf" {
        (0..against.len())
            .rev()
            .find(|i| eq_call(vm, q, &against[*i]))
    } else {
        (0..against.len()).find(|i| eq_call(vm, q, &against[*i]))
    };
    Some(EqPlan::Index(at))
}

/// Whether `v`'s own `equals` is the one this collection would consult: a body
/// has to exist, and a hash container additionally needs a `hashCode` it can
/// trust (see [`hash_consistent`]).
fn trusted_equals(vm: &VM, v: &Value, hashed: bool) -> bool {
    if user_equals(vm, v).is_none() {
        return false;
    }
    !hashed || instance_class(v).is_some_and(|c| hash_consistent(vm, &c))
}

/// Ascending natural order (`Comparable`) for the sorted collections and
/// `Collections.sort`: numbers numerically, strings lexicographically by
/// `char`, `null` first. Mixed kinds fall back to a stable "equal".
///
/// The floating fallback is [`float_compare`], not `partial_cmp`. A `TreeSet`
/// orders its elements by `Double.compareTo`, which is a *total* order —
/// `partial_cmp` answers `None` for a `NaN` operand and `Equal` for `-0.0`
/// against `0.0`, so a `TreeSet<Double>` collapsed the two zeroes into one
/// element and put `NaN` wherever the sort happened to leave it.
/// `Collections.sort` already reached the right answer through the
/// `compareTo` lambda the compiler supplies; this is the sibling path that did
/// not.
fn natural_cmp(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Value::Str(x), Value::Str(y)) => x.cmp(y),
        (Value::Undef, Value::Undef) => Ordering::Equal,
        (Value::Undef, _) => Ordering::Less,
        (_, Value::Undef) => Ordering::Greater,
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        _ => float_compare(a.jfloat(), b.jfloat()).cmp(&0),
    }
}

/// Allocate the collection `kind` names, seeded from `seed` when a copy
/// constructor supplied one.
fn new_collection(vm: &mut VM, kind: &str, seed: &Value) -> Result<Value, Fault> {
    let obj = match kind {
        "ArrayList" | "LinkedList" | "List" => HostObj::List {
            mods: 0,
            items: sequence_items(seed).unwrap_or_default(),
            fixed: Fixity::Mutable,
        },
        "HashMap" | "Map" => HostObj::Map {
            entries: map_entries(seed).unwrap_or_default(),
            order: Order::Hash,
            index: KeyIndex::default(),
        },
        "LinkedHashMap" => HostObj::Map {
            entries: map_entries(seed).unwrap_or_default(),
            order: Order::Insertion,
            index: KeyIndex::default(),
        },
        "TreeMap" => HostObj::Map {
            entries: map_entries(seed).unwrap_or_default(),
            order: Order::Sorted,
            index: KeyIndex::default(),
        },
        "HashSet" | "Set" => HostObj::Set {
            items: distinct(vm, &sequence_items(seed).unwrap_or_default()),
            order: Order::Hash,
            fixed: Fixity::Mutable,
            index: KeyIndex::default(),
        },
        "LinkedHashSet" => HostObj::Set {
            items: distinct(vm, &sequence_items(seed).unwrap_or_default()),
            order: Order::Insertion,
            fixed: Fixity::Mutable,
            index: KeyIndex::default(),
        },
        "TreeSet" => HostObj::Set {
            items: distinct(vm, &sequence_items(seed).unwrap_or_default()),
            order: Order::Sorted,
            fixed: Fixity::Mutable,
            index: KeyIndex::default(),
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
///
/// De-duplication is the same membership question `Set.add` asks, so it goes
/// through the element's own `equals` when the element declares one; `Set.of`
/// and `new HashSet<>(list)` would otherwise keep two equal records.
fn distinct(vm: &mut VM, vals: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(vals.len());
    for v in vals {
        let mut seen = false;
        for x in &out {
            let equal = if trusted_equals(vm, v, true) {
                eq_call(vm, v, x)
            } else {
                value_eq(v, x)
            };
            if equal {
                seen = true;
                break;
            }
        }
        if !seen {
            out.push(v.clone());
        }
    }
    out
}

/// The first value of `vals` that repeats an earlier one, under the same
/// comparison [`distinct`] de-duplicates with. `None` when they are all
/// distinct.
fn first_repeat(vm: &mut VM, vals: &[Value]) -> Option<Value> {
    for (i, v) in vals.iter().enumerate() {
        for x in &vals[..i] {
            let equal = if trusted_equals(vm, v, true) {
                eq_call(vm, v, x)
            } else {
                value_eq(v, x)
            };
            if equal {
                return Some(v.clone());
            }
        }
    }
    None
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

// ── java.lang.StringBuilder / StringBuffer ───────────────────────────────
//
// The builder is a host shape (`HostObj::Builder`) rather than a class
// instance: its state is one growable string, and every method is a string
// operation. Allocation gets its own builtin ([`JSB_NEW`]); the methods reach
// [`builder_method`] from [`b_str_dispatch`], which is where every receiver
// whose static type is not a user class or a collection already lands.
//
// Index and length semantics use Unicode scalar positions, the same
// simplification [`string_method`] documents: Java counts UTF-16 units, so a
// builder holding an astral character reports a length one smaller here. Every
// bounds failure carries the JDK's own detail message, which is not one wording
// but three — `Index i out of bounds for length n` for a single index,
// `Range [s, e) out of bounds for length n` for a pair, and
// `String index out of range: n` for `setLength`.

/// Java's default `StringBuilder` capacity, and the slack `new
/// StringBuilder(str)` adds to its argument's length.
pub const SB_DEFAULT_CAP: usize = 16;

/// The `StringIndexOutOfBoundsException` a single out-of-range index raises.
fn sb_index_fault(i: i64, len: usize) -> Fault {
    Fault::java(
        "StringIndexOutOfBoundsException",
        format!("Index {i} out of bounds for length {len}"),
    )
}

/// The `StringIndexOutOfBoundsException` an out-of-range `[start, end)` pair
/// raises.
fn sb_range_fault(start: i64, end: i64, len: usize) -> Fault {
    Fault::java(
        "StringIndexOutOfBoundsException",
        format!("Range [{start}, {end}) out of bounds for length {len}"),
    )
}

/// Validate a scalar index against `len`, answering its byte offset in `s`.
fn sb_char_offset(s: &str, i: i64, len: usize) -> Result<usize, Fault> {
    if i < 0 || i as usize >= len {
        return Err(sb_index_fault(i, len));
    }
    Ok(sb_byte_of(s, len, i as usize))
}

/// The byte offset of the `n`-th character of a builder holding `len` of them.
///
/// A buffer whose byte length equals its character count holds nothing but
/// ASCII — UTF-8 spends one byte per character exactly then — so the character
/// index *is* the byte index and no decoding is needed. That is the common case
/// by a wide margin, and it turns `charAt` (and every other indexed operation)
/// from a walk from the start into an O(1) read.
fn sb_byte_of(s: &str, len: usize, n: usize) -> usize {
    if s.len() == len {
        return n.min(s.len());
    }
    s.char_indices().nth(n).map(|(b, _)| b).unwrap_or(s.len())
}

/// Validate a `[start, end)` pair the way `AbstractStringBuilder`'s
/// `checkRangeSIOOBE` does, answering the two byte offsets.
fn sb_range(s: &str, start: i64, end: i64, len: usize) -> Result<(usize, usize), Fault> {
    if start < 0 || start > end || end as usize > len {
        return Err(sb_range_fault(start, end, len));
    }
    Ok((
        sb_byte_of(s, len, start as usize),
        sb_byte_of(s, len, end as usize),
    ))
}

/// The next capacity `AbstractStringBuilder` grows to when `min` characters no
/// longer fit: `2 * old + 2`, or `min` when even that is too small.
fn sb_grow(cap: usize, min: usize) -> usize {
    if min <= cap {
        cap
    } else {
        (cap * 2 + 2).max(min)
    }
}

/// The text one `append`/`insert` argument contributes. javars has already
/// converted a `char` argument to its one-character String and a `float` to
/// `Float.toString` at the call site (`emit_char_string`), so this is the same
/// rendering every other Java string conversion uses.
///
/// An array argument is joined rather than rendered, because `append(char[])`
/// and `insert(int, char[])` are overloads that write the characters — the same
/// reading `String.valueOf(char[])` already takes, and for the same reason.
fn sb_arg_str(vm: &mut VM, v: &Value) -> String {
    match array_items(v) {
        Some(items) => items.iter().map(|e| java_str_vm(vm, e)).collect(),
        None => java_str_vm(vm, v),
    }
}

/// Evaluate `recv.method(args)` on a `StringBuilder`/`StringBuffer` receiver.
///
/// `None` means the name is not a builder method at all, which lets the caller
/// fall through to `java.lang.Object`'s (`equals`, `hashCode`, `getClass`) —
/// the three a builder genuinely inherits, and the reason `equals` compares
/// identity here as it does in Java rather than comparing the text.
fn builder_method(
    vm: &mut VM,
    id: u32,
    method: &str,
    args: &[Value],
) -> Option<Result<Value, Fault>> {
    // Arguments render before the heap borrow: a rendering may run a user
    // `toString()`, which re-enters the VM and can allocate.
    let rendered: Vec<String> = args.iter().map(|a| sb_arg_str(vm, a)).collect();
    Some(HEAP.with(|h| {
        let mut heap = h.borrow_mut();
        let Some(HostObj::Builder {
            s, len: count, cap, ..
        }) = heap.get_mut(id as usize)
        else {
            return Err(Fault::internal("javars: dangling StringBuilder handle"));
        };
        // The maintained character count. Every arm that changes the text
        // writes the new one back through `count`, from the delta it already
        // knows — no method walks the buffer to find out how long it is.
        let len = *count;
        let this = Value::Obj(id);
        match (method, args.len()) {
            ("toString", 0) | ("substring", 0) => Ok(Value::str(s.clone())),
            ("length", 0) => Ok(Value::Int(len as i64)),
            ("isEmpty", 0) => Ok(Value::bool(s.is_empty())),
            ("capacity", 0) => Ok(Value::Int(*cap as i64)),
            // `ensureCapacity` and `trimToSize` are allocation hints. The first
            // is observable through `capacity()`; the second is not, because
            // javars stores the text in a `String` that is already trimmed.
            ("ensureCapacity", 1) => {
                let want = args[0].jint();
                if want > 0 {
                    *cap = sb_grow(*cap, want as usize);
                }
                Ok(Value::Undef)
            }
            ("trimToSize", 0) => {
                *cap = len;
                Ok(Value::Undef)
            }
            ("append", 1) => {
                s.push_str(&rendered[0]);
                *count = len + rendered[0].chars().count();
                *cap = sb_grow(*cap, *count);
                Ok(this)
            }
            // `append(char[])` is a different method from `append(Object)`, and
            // it is the *only* one-argument `append` that rejects `null`: it
            // reads the array's length, so a null argument is an NPE where
            // `append((Object) null)` appends `"null"`. The two are one value at
            // runtime, so the compiler picks the overload from the argument's
            // static type and sends this name when it is `char[]`.
            ("appendChars", 1) => {
                if matches!(args[0], Value::Undef) {
                    return Err(Fault::java(
                        "NullPointerException",
                        "Cannot read the array length because \"str\" is null",
                    ));
                }
                s.push_str(&rendered[0]);
                *count = len + rendered[0].chars().count();
                *cap = sb_grow(*cap, *count);
                Ok(this)
            }
            ("appendCodePoint", 1) => {
                let cp = args[0].jint();
                match u32::try_from(cp).ok().and_then(char::from_u32) {
                    Some(c) => {
                        s.push(c);
                        *count = len + 1;
                        *cap = sb_grow(*cap, *count);
                        Ok(this)
                    }
                    None => Err(Fault::java(
                        "IllegalArgumentException",
                        format!("Not a valid Unicode code point: 0x{cp:X}"),
                    )),
                }
            }
            ("repeat", 2) => {
                let n = args[1].jint().max(0) as usize;
                s.push_str(&rendered[0].repeat(n));
                *count = len + rendered[0].chars().count() * n;
                *cap = sb_grow(*cap, *count);
                Ok(this)
            }
            ("charAt", 1) => {
                let at = sb_char_offset(s, args[0].jint(), len)?;
                Ok(Value::Int(s[at..].chars().next().map_or(0, |c| c as i64)))
            }
            ("setCharAt", 2) => {
                let at = sb_char_offset(s, args[0].jint(), len)?;
                let old = s[at..].chars().next().map_or(0, char::len_utf8);
                s.replace_range(at..at + old, &rendered[1]);
                *count = len - 1 + rendered[1].chars().count();
                Ok(Value::Undef)
            }
            ("deleteCharAt", 1) => {
                let at = sb_char_offset(s, args[0].jint(), len)?;
                let old = s[at..].chars().next().map_or(0, char::len_utf8);
                s.replace_range(at..at + old, "");
                *count = len - 1;
                Ok(this)
            }
            // `delete` and `replace` clamp the end to the length before the
            // range check, which is why `delete(2, 100)` truncates rather than
            // throwing while `substring(1, 9)` throws.
            ("delete", 2) => {
                let start = args[0].jint();
                let end = args[1].jint().min(len as i64);
                let (a, b) = sb_range(s, start, end, len)?;
                s.replace_range(a..b, "");
                *count = len - (end - start) as usize;
                Ok(this)
            }
            ("replace", 3) => {
                let start = args[0].jint();
                let end = args[1].jint().min(len as i64);
                let (a, b) = sb_range(s, start, end, len)?;
                s.replace_range(a..b, &rendered[2]);
                *count = len - (end - start) as usize + rendered[2].chars().count();
                *cap = sb_grow(*cap, *count);
                Ok(this)
            }
            ("substring", 1) => {
                let (a, b) = sb_range(s, args[0].jint(), len as i64, len)?;
                Ok(Value::str(s[a..b].to_string()))
            }
            ("substring", 2) | ("subSequence", 2) => {
                let (a, b) = sb_range(s, args[0].jint(), args[1].jint(), len)?;
                Ok(Value::str(s[a..b].to_string()))
            }
            // `insert`'s bounds failure names the *builder's* length as the
            // range end, which is what `checkOffset` reports:
            // `Range [9, 3) out of bounds for length 3`.
            ("insert", 2) => {
                let at = args[0].jint();
                if at < 0 || at as usize > len {
                    return Err(sb_range_fault(at, len as i64, len));
                }
                s.insert_str(sb_byte_of(s, len, at as usize), &rendered[1]);
                *count = len + rendered[1].chars().count();
                *cap = sb_grow(*cap, *count);
                Ok(this)
            }
            // `reverse` reverses code points, not UTF-16 units, so a surrogate
            // pair survives it — which is what Java's own `reverse` guarantees.
            // The length is unchanged, so `count` is left alone.
            ("reverse", 0) => {
                *s = s.chars().rev().collect();
                Ok(this)
            }
            ("setLength", 1) => {
                let n = args[0].jint();
                if n < 0 {
                    return Err(Fault::java(
                        "StringIndexOutOfBoundsException",
                        format!("String index out of range: {n}"),
                    ));
                }
                let n = n as usize;
                if n < len {
                    s.truncate(sb_byte_of(s, len, n));
                } else {
                    // Java pads the extra positions with the NUL character,
                    // which is observable: after `setLength(4)` on "ab",
                    // `charAt(3)` is 0.
                    s.extend(std::iter::repeat('\0').take(n - len));
                }
                *count = n;
                *cap = sb_grow(*cap, n);
                Ok(Value::Undef)
            }
            ("indexOf", 1) => Ok(Value::Int(char_index_of(s, &rendered[0]))),
            ("indexOf", 2) => {
                let from = args[1].jint().clamp(0, len as i64) as usize;
                let byte = sb_byte_of(s, len, from);
                Ok(Value::Int(match char_index_of(&s[byte..], &rendered[0]) {
                    -1 => -1,
                    i => i + from as i64,
                }))
            }
            ("lastIndexOf", 1) => Ok(Value::Int(char_last_index_of(s, &rendered[0], len as i64))),
            ("lastIndexOf", 2) => Ok(Value::Int(char_last_index_of(
                s,
                &rendered[0],
                args[1].jint(),
            ))),
            // `compareTo(StringBuilder)` is `String.compareTo` on the contents
            // (Java 11+); `equals` is NOT — it stays reference identity, which
            // is why it is left to `object_method`.
            ("compareTo", 1) => Ok(Value::Int(compare_strings(s, &rendered[0], false))),
            ("chars", 0) | ("codePoints", 0) => Err(Fault::internal(format!(
                "javars: unsupported StringBuilder method `{method}` with 0 argument(s)"
            ))),
            _ => Err(Fault::internal(format!(
                "javars: unsupported StringBuilder method `{method}` with {} argument(s)",
                args.len()
            ))),
        }
    }))
}

/// True when the handle points at a `StringBuilder`/`StringBuffer` — the test
/// that routes a statically-untyped receiver away from the `String` methods.
fn is_builder(v: &Value) -> Option<u32> {
    let Value::Obj(id) = v else { return None };
    HEAP.with(|h| match h.borrow().get(*id as usize) {
        Some(HostObj::Builder { .. }) => Some(*id),
        _ => None,
    })
}

/// [`JSB_NEW`] — allocate a `StringBuilder`/`StringBuffer`.
fn b_sb_new(vm: &mut VM, argc: u8) -> Value {
    let args = pop_args(vm, argc);
    let buffer = args
        .first()
        .map(|v| v.as_str_cow().into_owned())
        .unwrap_or_default()
        == "StringBuffer";
    let seed = args.get(1).cloned().unwrap_or(Value::Undef);
    // The three constructors, told apart by the argument's runtime shape the
    // same way Java's overload resolution tells them apart statically.
    let (s, cap) = match &seed {
        // `new StringBuilder((String) null)` dereferences its argument before
        // it sizes anything; the no-argument form reaches here as the capacity
        // 16 it is defined as, so `Undef` can only be a real `null`.
        Value::Undef => {
            return raise(
                vm,
                Fault::java(
                    "NullPointerException",
                    "Cannot invoke \"String.length()\" because \"str\" is null",
                ),
            )
        }
        Value::Int(n) => {
            if *n < 0 {
                return raise(vm, Fault::java("NegativeArraySizeException", n.to_string()));
            }
            (String::new(), *n as usize)
        }
        other => {
            let text = java_str_vm(vm, other);
            let n = text.chars().count();
            (text, n + SB_DEFAULT_CAP)
        }
    };
    let len = s.chars().count();
    Value::Obj(heap_alloc(HostObj::Builder {
        s,
        len,
        cap,
        buffer,
    }))
}

/// [`JCOLL_NEW`] — see [`new_collection`].
fn b_coll_new(vm: &mut VM, argc: u8) -> Value {
    let args = pop_args(vm, argc);
    let kind = args
        .first()
        .map(|v| v.as_str_cow().into_owned())
        .unwrap_or_default();
    let seed = args.get(1).cloned().unwrap_or(Value::Undef);
    match new_collection(vm, &kind, &seed) {
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

/// Pop the operands of a **fixed-shape** builtin into `N` slots, deepest first
/// — the order [`pop_args`] answers in, without the `Vec` it allocates.
///
/// A field read and a field write know exactly how many operands they take, so
/// the heap allocation buys them nothing, and they sit on the hottest path an
/// object-heavy loop has: one per field access. Operands beyond the `N` the
/// builtin declares (a shape the compiler does not emit) are discarded, and
/// fewer than `N` leaves the missing slots `Undef` rather than underflowing.
fn pop_fixed<const N: usize>(vm: &mut VM, argc: u8) -> [Value; N] {
    let mut out = std::array::from_fn(|_| Value::Undef);
    for _ in N..argc as usize {
        let _ = vm.stack.pop();
    }
    for slot in out.iter_mut().take(N.min(argc as usize)).rev() {
        *slot = vm.stack.pop().unwrap_or(Value::Undef);
    }
    out
}

/// `new T[n]` — build an `n`-element array filled with the element default
/// (stack `[size, default]`).
fn b_array_new(vm: &mut VM, argc: u8) -> Value {
    let args = pop_args(vm, argc);
    let size = args.first().map(|v| v.jint()).unwrap_or(0);
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
    let sizes: Vec<i64> = args.iter().map(|v| v.jint()).collect();
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
    let idx = args.get(1).map(|v| v.jint()).unwrap_or(0);
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
    let idx = args.get(1).map(|v| v.jint()).unwrap_or(0);
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
    // The compiler emits this builtin with the receiver pushed first and the
    // field name on top, so both come off the stack directly. Going through
    // `pop_args` cost a `Vec` allocation, and copying the name out of its
    // `Cow` cost a `String` allocation — two mallocs on the path a loop over an
    // object's fields takes once per read. `Value::Str`'s `as_str_cow` borrows,
    // and `HashMap<String, _>` looks up by `&str`, so neither is needed.
    let [recv, name_v] = pop_fixed::<2>(vm, argc);
    let name = name_v.as_str_cow();
    let name = name.as_ref();
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
                Ok(fields.get(name).cloned().unwrap_or(Value::Undef))
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
    // Same fixed shape as [`b_field_get`], and the same two allocations saved.
    let [recv, name_v, val] = pop_fixed::<3>(vm, argc);
    let name = name_v.as_str_cow();
    let name = name.as_ref();
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
                // The name is only copied when the field is *new*; an
                // assignment to an existing one — which every loop body does —
                // writes through the entry already there.
                match fields.get_mut(name) {
                    Some(slot) => *slot = val.clone(),
                    None => {
                        fields.insert(name.to_string(), val.clone());
                    }
                }
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
                    // The whole point of the box: `Integer` and `Long` are
                    // different classes for a value one `Value::Int` holds.
                    HostObj::Boxed => box_class(v)?.to_string(),
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
                    HostObj::Builder { buffer, .. } => if *buffer {
                        "StringBuffer"
                    } else {
                        "StringBuilder"
                    }
                    .to_string(),
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
        return match make_sublist(id, args[0].jint(), args[1].jint()) {
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
        return Value::str(java_str_vm(vm, recv));
    }
    // Likewise `addAll`/`equals` read their argument collection: snapshot it
    // first, because the borrow below is exclusive.
    let arg_seqs: Vec<Option<Vec<Value>>> = args.iter().map(sequence_items).collect();
    // Java answers a membership question with the element's own `equals`, whose
    // body needs the VM and no outstanding borrow — so it runs here, ahead of
    // the borrow, and the sections below read its verdicts.
    let eq = eq_plan(vm, recv, method, args, &arg_seqs);
    // A throwable one of those bodies raised aborts the call rather than
    // answering from a half-resolved plan.
    if PENDING.with(|p| p.borrow().is_some()) {
        return Value::Undef;
    }
    let eq = eq.as_ref();
    if is_sublist(id) {
        return match sublist_method(id, method, args, &arg_seqs, eq) {
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
                let r = list_method(items, *fixed, method, args, &arg_seqs, eq);
                // Any length change is a structural modification, which is what
                // Java's `modCount` counts (a `remove` that finds nothing does
                // not bump it there either).
                if items.len() != before {
                    *mods += 1;
                }
                r
            }
            HostObj::Map {
                entries,
                order,
                index,
            } => map_method(entries, *order, index, method, args, eq),
            HostObj::Set {
                items,
                fixed,
                index,
                ..
            } => set_method(items, *fixed, index, method, args, &arg_seqs, eq),
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
    eq: Option<&EqPlan>,
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
    let out = list_method(&mut window, fixed, method, args, arg_seqs, eq)?;
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
                    invoke_closure(vm, cmp, &[items[j].clone(), items[i].clone()]).jint() >= 0
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
    eq: Option<&EqPlan>,
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
            let at = args[0].jint();
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
            let i = bounds(args[0].jint(), items.len())?;
            items[i].clone()
        }
        ("set", 2) => {
            replace()?;
            let i = bounds(args[0].jint(), items.len())?;
            std::mem::replace(&mut items[i], args[1].clone())
        }
        // `List.remove(int)` removes by index — the overload Java picks for an
        // integral argument. The `remove(Object)` overload arrives under the
        // distinct name `removeObject`, chosen by the compiler from the
        // argument's static type (the same question Java answers statically),
        // because a boxed `Integer` and an `int` are one value here.
        ("remove", 1) => {
            structural()?;
            let i = bounds(args[0].jint(), items.len())?;
            items.remove(i)
        }
        // `List.remove(Object)` — removes the first element equal to the
        // argument and answers whether one was found.
        ("removeObject", 1) => {
            structural()?;
            match eq_index(eq, items, &args[0], false) {
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
        ("contains", 1) => Value::bool(eq_index(eq, items, &args[0], false).is_some()),
        ("indexOf", 1) => Value::Int(eq_index(eq, items, &args[0], false).map_or(-1, |i| i as i64)),
        ("lastIndexOf", 1) => {
            Value::Int(eq_index(eq, items, &args[0], true).map_or(-1, |i| i as i64))
        }
        ("addAll", 1) => {
            structural()?;
            let add = arg_seqs[0].clone().unwrap_or_default();
            let changed = !add.is_empty();
            items.extend(add);
            Value::bool(changed)
        }
        ("equals", 1) => match eq {
            Some(EqPlan::Same(same)) => Value::bool(*same),
            _ => {
                let other = arg_seqs[0].clone().unwrap_or_default();
                Value::bool(
                    other.len() == items.len()
                        && items.iter().zip(&other).all(|(a, b)| value_eq(a, b)),
                )
            }
        },
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
    index: &mut KeyIndex,
    method: &str,
    args: &[Value],
    eq: Option<&EqPlan>,
) -> Result<NewColl, Fault> {
    // A stale index is repaired once, here, rather than at every arm: the
    // methods below either read it or invalidate it, and only this entry point
    // knows the keys to rebuild it from.
    if index.dirty {
        index.rebuild(entries.iter().map(|(k, _)| k));
    }
    // Only one arm below runs per call, so the single verdict vector is
    // unambiguous: it indexes the keys for every key-addressed method, and the
    // values for `containsValue`.
    //
    // A user `equals` puts the verdict in `eq` and it wins; otherwise
    // [`value_eq`] decides and the index answers in its place when it can.
    let find = |entries: &Vec<(Value, Value)>, index: &KeyIndex, k: &Value| match eq {
        Some(EqPlan::Index(at)) => *at,
        _ => match index.find(entries, |(x, _)| x, k) {
            Some(at) => at,
            None => entries.iter().position(|(x, _)| value_eq(x, k)),
        },
    };
    let out = match (method, args.len()) {
        ("size", 0) => NewColl::Value(Value::Int(entries.len() as i64)),
        ("isEmpty", 0) => NewColl::Value(Value::bool(entries.is_empty())),
        // A re-`put` keeps the entry's original insertion position, which is
        // what Java's linked/bucket layouts both do.
        // A fresh key lands at the end, which is the one shape the index can
        // record without rebuilding — and the shape a loop that fills a map
        // takes every iteration.
        ("put", 2) => NewColl::Value(match find(entries, index, &args[0]) {
            Some(i) => std::mem::replace(&mut entries[i].1, args[1].clone()),
            None => {
                index.push(&args[0], entries.len());
                entries.push((args[0].clone(), args[1].clone()));
                Value::Undef
            }
        }),
        ("putIfAbsent", 2) => NewColl::Value(match find(entries, index, &args[0]) {
            Some(i) => entries[i].1.clone(),
            None => {
                index.push(&args[0], entries.len());
                entries.push((args[0].clone(), args[1].clone()));
                Value::Undef
            }
        }),
        ("get", 1) => NewColl::Value(
            find(entries, index, &args[0]).map_or(Value::Undef, |i| entries[i].1.clone()),
        ),
        ("getOrDefault", 2) => NewColl::Value(
            find(entries, index, &args[0])
                .map_or_else(|| args[1].clone(), |i| entries[i].1.clone()),
        ),
        ("containsKey", 1) => NewColl::Value(Value::bool(find(entries, index, &args[0]).is_some())),
        ("containsValue", 1) => NewColl::Value(Value::bool(match eq {
            Some(EqPlan::Index(at)) => at.is_some(),
            _ => entries.iter().any(|(_, v)| value_eq(v, &args[0])),
        })),
        // A removal shifts every later position, so the index is marked stale
        // instead of being repaired here; the next lookup rebuilds it, which
        // costs no more than the scan the index replaced.
        ("remove", 1) => NewColl::Value(match find(entries, index, &args[0]) {
            Some(i) => {
                index.invalidate();
                entries.remove(i).1
            }
            None => Value::Undef,
        }),
        ("clear", 0) => {
            entries.clear();
            index.rebuild(std::iter::empty());
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
                index: KeyIndex::default(),
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
    index: &mut KeyIndex,
    method: &str,
    args: &[Value],
    arg_seqs: &[Option<Vec<Value>>],
    eq: Option<&EqPlan>,
) -> Result<NewColl, Fault> {
    // See the note at the top of `map_method`: one repair point, here.
    if index.dirty {
        index.rebuild(items.iter());
    }
    // The membership question every arm below asks. `eq` (a user `equals`) wins
    // when it has a verdict; otherwise the index answers in `value_eq`'s place
    // when it can, and the scan runs when it cannot.
    let member = |items: &Vec<Value>, index: &KeyIndex, q: &Value| match eq {
        Some(EqPlan::Index(at)) => *at,
        _ => match index.find(items, |x| x, q) {
            Some(at) => at,
            None => items.iter().position(|x| value_eq(x, q)),
        },
    };
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
            if member(items, index, &args[0]).is_some() {
                Value::bool(false)
            } else {
                index.push(&args[0], items.len());
                items.push(args[0].clone());
                Value::bool(true)
            }
        }
        ("contains", 1) => Value::bool(member(items, index, &args[0]).is_some()),
        ("remove", 1) => {
            structural()?;
            match member(items, index, &args[0]) {
                Some(i) => {
                    index.invalidate();
                    items.remove(i);
                    Value::bool(true)
                }
                None => Value::bool(false),
            }
        }
        ("clear", 0) => {
            structural()?;
            items.clear();
            index.rebuild(std::iter::empty());
            Value::Undef
        }
        ("addAll", 1) => {
            structural()?;
            match eq {
                Some(EqPlan::Fresh(fresh)) => {
                    let changed = !fresh.is_empty();
                    for v in fresh {
                        index.push(v, items.len());
                        items.push(v.clone());
                    }
                    Value::bool(changed)
                }
                _ => {
                    let mut changed = false;
                    for v in arg_seqs[0].clone().unwrap_or_default() {
                        if member(items, index, &v).is_none() {
                            index.push(&v, items.len());
                            items.push(v);
                            changed = true;
                        }
                    }
                    Value::bool(changed)
                }
            }
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
    // A `StringBuilder`/`StringBuffer` receiver. This runs ahead of
    // `object_method` so `sb.toString()` answers the contents rather than
    // `java.lang.StringBuilder@<id>`, and `builder_method` declines the three
    // names a builder really does inherit from `Object` so they fall through.
    if let Some(id) = is_builder(&recv) {
        if !matches!(method.as_str(), "equals" | "hashCode" | "getClass") {
            if let Some(r) = builder_method(vm, id, &method, &args) {
                return match r {
                    Ok(v) => v,
                    Err(f) => raise(vm, f),
                };
            }
        }
    }
    // A class instance whose own class declares no such method inherits
    // `java.lang.Object`'s — including `new Object()` itself, which has no class
    // body at all.
    // `o.toString()` on an `Object`-typed receiver resolves the override the
    // same way rendering does; a class that declares none still falls to the
    // `Class@hash` form `object_method` supplies just below.
    if method == "toString"
        && args.is_empty()
        && any_user_tostring(vm)
        && instance_class(&recv).is_some()
    {
        return Value::str(java_str_vm(vm, &recv));
    }
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
    // A boxed number's own methods, *before* the receiver is stringified. They
    // are not `String` methods, so falling through to that table did not fail —
    // it answered from the receiver's text: `Integer.valueOf(300).hashCode()`
    // was 50547, the hash of `"300"`, where Java's is 300.
    if let Some(v) = boxed_method(&recv, &method, args.len()) {
        return v;
    }
    // The same, for a receiver that is a *wrapper handle*. The three methods
    // whose answer depends on the wrapper's class are answered here; everything
    // else re-enters with the primitive, so the tables above serve a boxed
    // receiver exactly as they serve a bare one.
    if unboxed(&recv).is_some() {
        match (method.as_str(), args.len()) {
            ("equals", 1) => return Value::bool(value_eq(&recv, &args[0])),
            ("hashCode", 0) => {
                return java_hash(&recv).map_or(Value::Undef, |h| Value::Int(h.into()))
            }
            _ => {}
        }
        let inner = deboxed(&recv);
        vm.stack.push(inner);
        for a in args {
            vm.stack.push(a);
        }
        vm.stack.push(Value::str(method));
        return b_str_dispatch(vm, argc);
    }
    // Borrowed, not copied. `into_owned` cloned the whole receiver on every
    // single `String` method call, so `s.charAt(i)` over an n-character string
    // moved n bytes per call and n² across the loop — 40k characters meant
    // 1.6GB of memcpy for a walk that reads 40k of them. `recv` is a local, so
    // the borrow is independent of the `&mut VM` the arms below take.
    let s = recv.as_str_cow();
    // `"%s".formatted(x)` renders `x`, so it needs the VM `string_method` has
    // not got. Every other `String` method reads text only.
    if method == "formatted" && any_user_tostring(vm) {
        return match java_format(&s, &args, &[], Some(&mut *vm)) {
            Ok(v) => v,
            Err(f) => raise(vm, f),
        };
    }
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
/// The runtime class of a class-instance handle; `None` for every other value
/// (an array, a collection, a closure, a primitive).
fn instance_class(v: &Value) -> Option<String> {
    let Value::Obj(id) = v else {
        return None;
    };
    HEAP.with(|h| match h.borrow().get(*id as usize) {
        Some(HostObj::Instance { class, .. }) => Some(class.clone()),
        _ => None,
    })
}

fn object_method(recv: &Value, method: &str, args: &[Value]) -> Option<Value> {
    let Value::Obj(id) = recv else {
        return None;
    };
    // A `StringBuilder` inherits `Object`'s `equals`/`hashCode` unchanged — it
    // overrides neither, so two builders holding the same text are unequal and
    // a builder's hash is its identity. Left out of this gate the call reached
    // the `String` table through the receiver's rendering, and both answered
    // for the *text*.
    let inherits_object = HEAP.with(|h| {
        matches!(
            h.borrow().get(*id as usize),
            Some(HostObj::Instance { .. } | HostObj::Builder { .. })
        )
    });
    if !inherits_object {
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

/// The methods a boxed primitive answers itself, for a receiver javars models
/// as a bare value rather than as a heap instance.
///
/// `Number`'s six converters plus `Boolean.booleanValue`, `Character.charValue`
/// and `Object.hashCode`. Without this the receiver was rendered to text and
/// the call went to the `String` table, which has no `intValue` (an error) but
/// *does* have `hashCode` — so a boxed number's hash was the hash of its
/// digits. `Integer.valueOf(300).hashCode()` answered 50547 against Java's 300.
///
/// The converters are the JLS narrowing conversions and need no per-box arity:
/// `intValue()` truncates to 32 bits, which is identity for an `Integer` and
/// the wrap Java performs for a `Long` (`Long.valueOf(4294967296L).intValue()`
/// is 0). A floating receiver saturates rather than wraps, which is what Java's
/// `(int) aDouble` does and what Rust's `as` already gives.
fn boxed_method(recv: &Value, method: &str, argc: usize) -> Option<Value> {
    if argc != 0 || matches!(recv, Value::Obj(_) | Value::Undef) {
        return None;
    }
    // `Number.intValue()` is `(int) value`, and the two receiver kinds narrow
    // differently: a `double` *saturates* at the `int` bounds (Java's
    // floating-to-integral conversion), while a `long` *wraps*. Going through
    // `long` first would saturate at the wrong width and then wrap —
    // `Double.valueOf(1e30).intValue()` came out -1 instead of
    // `Integer.MAX_VALUE`. `shortValue`/`byteValue` are `(short) intValue()`
    // and `(byte) intValue()`, so both derive from this one.
    let int_value = match recv {
        Value::Float(f) => *f as i32,
        other => other.jint() as i32,
    };
    let long_value = match recv {
        Value::Float(f) => *f as i64,
        other => other.jint(),
    };
    Some(match method {
        // `String.hashCode` is a different function and the `String` table
        // already answers it; a one-character `String` is javars's `char`, and
        // `Character.hashCode` is the code point, which is the same number.
        "hashCode" if !matches!(recv, Value::Str(_)) => Value::Int(java_hash(recv)?.into()),
        "intValue" => Value::Int(int_value.into()),
        "shortValue" => Value::Int((int_value as i16).into()),
        "byteValue" => Value::Int((int_value as i8).into()),
        "longValue" => Value::Int(long_value),
        "doubleValue" => Value::float(recv.jfloat()),
        "floatValue" => Value::float(recv.jfloat() as f32 as f64),
        "booleanValue" if matches!(recv, Value::Bool(_)) => recv.clone(),
        // A `char` is carried as its code point (or as the one-character
        // `String` a rendered one becomes), so unboxing it is identity — the
        // compiler types the call `char`, which is what makes it print as a
        // character rather than as a number.
        "charValue" => recv.clone(),
        _ => return None,
    })
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
    // A null *argument* is an NPE too, and javars compared against the coerced
    // empty string instead: `"abc".compareTo(null)` answered 3. Every box
    // dereferences the argument to read its `value`, and names its own
    // parameter while doing so — measured on openjdk 21.0.12, one per type
    // rather than one shared wording, because the parameter names differ.
    if matches!(b, Value::Undef) {
        let param = match tag.as_str() {
            "Integer" | "int" => "anotherInteger",
            "Long" | "long" => "anotherLong",
            "Double" | "double" | "Float" | "float" => "anotherDouble",
            "Character" | "char" => "anotherCharacter",
            "Boolean" | "boolean" => "b",
            "String" | "CharSequence" => "anotherString",
            // The compiler could not name the receiver's type; the runtime value
            // decides, the same way the comparison below does.
            _ => match &a {
                Value::Str(_) => "anotherString",
                Value::Float(_) => "anotherDouble",
                Value::Bool(_) => "b",
                _ => "anotherInteger",
            },
        };
        return raise(
            vm,
            Fault::java(
                "NullPointerException",
                format!("Cannot read field \"value\" because \"{param}\" is null"),
            ),
        );
    }
    // `StringBuilder.compareTo` (Java 11+) is `String.compareTo` on the two
    // contents. It has to be answered before the arms below, all of which would
    // read two heap handles as integers and always answer 0.
    if is_builder(&a).is_some() || is_builder(&b).is_some() {
        return Value::Int(compare_strings(&java_str(&a), &java_str(&b), false));
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

/// `Math.nextAfter(start, direction)`: the representable `double` adjacent to
/// `start` in `direction`, or `direction` itself when the two are equal.
///
/// Consecutive `double`s of the same sign are consecutive integers when their
/// bits are read as an `i64`, which is what makes the step one increment.
/// Crossing zero is the one place that breaks — the two zeros are `0` and the
/// sign bit — so it is handled directly.
fn next_after(start: f64, direction: f64) -> f64 {
    if start.is_nan() || direction.is_nan() {
        return f64::NAN;
    }
    if start == direction {
        return direction;
    }
    let up = direction > start;
    if start == 0.0 {
        // Both zeros step to the smallest subnormal of the target's sign.
        return if up {
            f64::from_bits(1)
        } else {
            -f64::from_bits(1)
        };
    }
    let bits = start.to_bits() as i64;
    // Away from zero is "up" in magnitude for a positive value and "down" for a
    // negative one, which is exactly whether the step matches the sign.
    let step = if (start > 0.0) == up { 1 } else { -1 };
    f64::from_bits((bits + step) as u64)
}

/// `Math.ulp(d)`: the distance from `d` to the next `double` away from zero.
fn double_ulp(d: f64) -> f64 {
    if d.is_nan() {
        return f64::NAN;
    }
    if d.is_infinite() {
        return f64::INFINITY;
    }
    let d = d.abs();
    if d == f64::MAX {
        // One step further would be infinity, so the gap is measured downward.
        return d - next_after(d, f64::NEG_INFINITY);
    }
    next_after(d, f64::INFINITY) - d
}

/// The number of Unicode scalars in `s` — javars's `String.length()`.
///
/// Counting the bytes that are not UTF-8 continuation bytes is the same answer
/// `chars().count()` gives without decoding anything, and the compiler
/// vectorizes it.
fn char_count(s: &str) -> usize {
    s.as_bytes().iter().filter(|b| (*b & 0xC0) != 0x80).count()
}

/// The `i`-th character of `s`, or `None` when `i` is past the end.
///
/// `chars().nth(i)` decodes every character before the wanted one, which made
/// the ordinary `for (i = 0; i < s.length(); i++) s.charAt(i)` walk quadratic:
/// 40k characters took 13.9s where 5k took 0.25s. Java's `charAt` is a constant-
/// time array read, so the shape a program writes has to stay affordable.
///
/// The prefix test is what recovers it. If every byte before `i` is ASCII then
/// each of those characters is one byte, so the character index *is* the byte
/// index and the character at it can be read directly. `is_ascii` on a slice is
/// a vectorized byte scan rather than a decode loop, which leaves the walk with
/// a constant small enough that the quadratic shape stops mattering at the
/// sizes a program actually reaches. A string that really does hold multi-byte
/// characters falls back to decoding.
fn char_at(s: &str, i: usize) -> Option<char> {
    let bytes = s.as_bytes();
    if i < bytes.len() && bytes[..i].is_ascii() {
        return s[i..].chars().next();
    }
    s.chars().nth(i)
}

/// The byte offset of the `i`-th character of `s`, clamped to its end. The same
/// ASCII-prefix shortcut [`char_at`] takes, for the callers that want to slice
/// rather than to read one character.
fn char_byte_of(s: &str, i: usize) -> usize {
    let bytes = s.as_bytes();
    if i <= bytes.len() && bytes[..i].is_ascii() {
        return i;
    }
    s.char_indices().nth(i).map(|(b, _)| b).unwrap_or(s.len())
}

/// Evaluate a `java.lang.String` method on `s`. Index/length semantics use
/// Unicode scalar (`char`) positions — exact for the ASCII/BMP common case and
/// consistent with javars's existing "a `char` literal is a one-character
/// string" model (astral characters, which Java counts as two UTF-16 units,
/// count as one here — the same documented simplification). An out-of-range
/// index raises Java's `StringIndexOutOfBoundsException` with its exact detail
/// message; an unknown method is a javars internal error.
fn string_method(s: &str, method: &str, args: &[Value]) -> Result<Value, Fault> {
    let char_len = || char_count(s) as i64;
    null_string_argument(method, args)?;
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
            let i = args[0].jint();
            match usize::try_from(i).ok().and_then(|i| char_at(s, i)) {
                Some(c) => Ok(Value::Int(c as i64)),
                None => Err(Fault::java(
                    "StringIndexOutOfBoundsException",
                    format!("Index {i} out of bounds for length {}", char_len()),
                )),
            }
        }
        ("substring", 1) => substring(s, args[0].jint(), char_len()),
        ("substring", 2) => substring(s, args[0].jint(), args[1].jint()),
        ("indexOf", 1) => Ok(Value::Int(char_index_of(s, &args[0].as_str_cow()))),
        // `indexOf(t, from)` starts the search at `from`; the result is still an
        // index into the whole string.
        ("indexOf", 2) => {
            // Java clamps `fromIndex` at *both* ends before searching, so a
            // start past the end answers `-1` for a real needle and the
            // string's length for the empty one — `"abc".indexOf("", 9)` is 3,
            // not 9. Clamping only the negative end returned the unclamped
            // `from` back through the empty-needle hit.
            let from = args[1].jint().clamp(0, char_len()) as usize;
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
            args[1].jint(),
        ))),
        ("codePointAt", 1) => {
            let i = args[0].jint();
            match usize::try_from(i).ok().and_then(|i| char_at(s, i)) {
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
        ("strip", 0) => Ok(Value::str(s.trim_matches(java_is_whitespace).to_string())),
        ("stripLeading", 0) => Ok(Value::str(
            s.trim_start_matches(java_is_whitespace).to_string(),
        )),
        ("stripTrailing", 0) => Ok(Value::str(
            s.trim_end_matches(java_is_whitespace).to_string(),
        )),
        ("isBlank", 0) => Ok(Value::bool(s.chars().all(java_is_whitespace))),
        ("hashCode", 0) => Ok(Value::Int(
            java_hash(&Value::str(s.to_string())).unwrap_or(0).into(),
        )),
        // Interning is unobservable here: javars compares strings by value.
        ("intern", 0) | ("toString", 0) => Ok(Value::str(s.to_string())),
        ("contentEquals", 1) => Ok(Value::bool(s == args[0].as_str_cow().as_ref())),
        // `x.getClass()` evaluates to the runtime class's *binary name*
        // ([`JBINARY_CLASS`]), so `Class`'s own two accessors land here:
        // `getName()` is that string and `getSimpleName()` drops the package
        // and enclosing-class qualifiers off it.
        ("getSimpleName", 0) => Ok(Value::str(simple_class_name(s).to_string())),
        ("getName", 0) => Ok(Value::str(s.to_string())),
        // A `char[]` of code points, matching `charAt` — so `a[i] - 'a'` is
        // arithmetic. `Arrays.toString`/`String.valueOf` of one are routed
        // through [`JCHR_STR`] by the compiler, which knows the element type.
        ("toCharArray", 0) => Ok(Value::Obj(heap_alloc(HostObj::Array(
            s.chars().map(|c| Value::Int(c as i64)).collect(),
        )))),
        // `"%s".formatted(x)` is `String.format("%s", x)` with the receiver as
        // the format string.
        ("formatted", _) => java_format(s, args, &[], None),
        // The four `java.util.regex` methods, on the engine in `crate::regex`.
        // `split(regex)` is `split(regex, 0)`: trailing empty fields are dropped
        // (interior ones are not), and a no-match returns the whole input.
        ("split", 1) | ("split", 2) => {
            let compiled = crate::regex::compile(&args[0].as_str_cow());
            let pat = compiled.as_ref().as_ref().map_err(|e| pattern_fault(e))?;
            let limit = args.get(1).map_or(0, JavaNumeric::jint);
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
        // `startsWith(prefix, offset)` tests at a character offset instead of at
        // the start. An offset outside the string is `false`, not a fault —
        // Java bounds-checks it rather than throwing.
        ("startsWith", 2) => {
            let off = args[1].jint();
            let len = char_count(s);
            Ok(Value::bool(usize::try_from(off).is_ok_and(|o| {
                o <= len && s[char_byte_of(s, o)..].starts_with(args[0].as_str_cow().as_ref())
            })))
        }
        ("endsWith", 1) => Ok(Value::bool(s.ends_with(args[0].as_str_cow().as_ref()))),
        ("concat", 1) => Ok(Value::str(format!("{s}{}", args[0].as_str_cow()))),
        ("replace", 2) => Ok(Value::str(
            s.replace(args[0].as_str_cow().as_ref(), &args[1].as_str_cow()),
        )),
        ("repeat", 1) => {
            let n = args[0].jint();
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
    // The statics that *render* an argument come next, for the same reason:
    // `static_method` has no VM, so it cannot run a user `toString()`. The gate
    // keeps a program that declares no override on the original path.
    if any_user_tostring(vm) {
        if let Some(v) = rendering_static(vm, &class, &method, &args) {
            return v;
        }
    }
    match static_method(&class, &method, &args) {
        Ok(v) => v,
        Err(f) => raise(vm, f),
    }
}

/// The stdlib statics whose whole job is to render an argument, re-implemented
/// here over [`java_str_vm`] so a user `toString()` answers for them too. Each
/// is the same rendering [`static_method`] does, with the VM-less `java_str`
/// swapped for the VM-holding one; `None` for every other static, and the
/// caller only asks when the gate is on.
fn rendering_static(vm: &mut VM, class: &str, method: &str, args: &[Value]) -> Option<Value> {
    Some(match (class, method, args.len()) {
        // `String.valueOf(char[])` concatenates the characters rather than
        // rendering the array, which is why the array case comes first.
        ("String", "valueOf", 1) => Value::str(match array_items(&args[0]) {
            Some(items) => items.iter().map(|v| java_str_vm(vm, v)).collect::<String>(),
            None => java_str_vm(vm, &args[0]),
        }),
        // A null delimiter falls through to [`static_method`], which raises the
        // `NullPointerException` Java does. Answering here would join on the
        // coerced empty string instead. (A null *element* is fine — Java renders
        // it "null" — so only the separator is checked.)
        ("String", "join", n) if n >= 2 && !matches!(args[0], Value::Undef) => {
            let sep = args[0].as_str_cow().into_owned();
            let parts: Vec<String> = match (sequence_items(&args[1]), n) {
                (Some(items), 2) => items.iter().map(|v| java_str_vm(vm, v)).collect(),
                _ => args[1..].iter().map(|v| java_str_vm(vm, v)).collect(),
            };
            Value::str(parts.join(&sep))
        }
        ("Arrays", "toString", 1) => match array_items(&args[0]) {
            Some(items) => {
                let inner: Vec<String> = items.iter().map(|v| java_str_vm(vm, v)).collect();
                Value::str(format!("[{}]", inner.join(", ")))
            }
            None => Value::str(java_str_vm(vm, &args[0])),
        },
        ("Arrays", "deepToString", 1) => Value::str(deep_to_string_vm(vm, &args[0])),
        _ => return None,
    })
}

/// [`arrays_deep_to_string`] with the VM in hand, so a nested array's *elements*
/// render through their overrides.
fn deep_to_string_vm(vm: &mut VM, v: &Value) -> String {
    match array_items(v) {
        Some(items) => {
            let inner: Vec<String> = items.iter().map(|e| deep_to_string_vm(vm, e)).collect();
            format!("[{}]", inner.join(", "))
        }
        None => java_str_vm(vm, v),
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
        // `Set.of` is the one set-building factory that *rejects* a repeat
        // rather than dropping it — `Set.of(1, 1)` is an
        // `IllegalArgumentException` naming the element, not a one-element set.
        // Silently de-duplicating turned a program Java refuses to run into one
        // that ran and answered.
        ("Set", "of") => {
            let items = varargs_items(args);
            let unique = distinct(vm, &items);
            if unique.len() != items.len() {
                let dup = first_repeat(vm, &items).unwrap_or(Value::Undef);
                return Some(Err(Fault::java(
                    "IllegalArgumentException",
                    format!("duplicate element: {}", java_str_vm(vm, &dup)),
                )));
            }
            Ok(Value::Obj(heap_alloc(HostObj::Set {
                items: unique,
                order: Order::Hash,
                fixed: Fixity::Immutable,
                index: KeyIndex::default(),
            })))
        }
        // `Objects.equals(a, b)` — `a == b || (a != null && a.equals(b))`. It is
        // here rather than in the VM-less `static_method` because the `a.equals`
        // half runs a user body, which needs the VM.
        ("Objects", "equals") if args.len() == 2 => {
            Ok(Value::bool(objects_equals(vm, &args[0], &args[1])))
        }
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
    let both_int = |a: &Value, b: &Value| {
        matches!(deboxed(a), Value::Int(_)) && matches!(deboxed(b), Value::Int(_))
    };
    match (class, method, args.len()) {
        // ── java.lang.Math ──
        // `wrapping_abs`, not `abs`: Rust's `abs` panics on `i64::MIN`, so
        // `Math.abs(Long.MIN_VALUE)` aborted the process where Java answers
        // `Long.MIN_VALUE` — "if the argument is equal to the value of
        // Long.MIN_VALUE, the most negative representable long value, the result
        // is that same value, which is negative" (Math.abs javadoc). The `int`
        // overload's identical case is narrowed by the compiler's trailing
        // `emit_wrap32`; this is the 64-bit one, which had no guard at all.
        ("Math", "abs", 1) => Ok(match &args[0] {
            Value::Int(n) => Value::Int(n.wrapping_abs()),
            other => Value::float(other.jfloat().abs()),
        }),
        ("Math", "max", 2) => Ok(if both_int(&args[0], &args[1]) {
            Value::Int(args[0].jint().max(args[1].jint()))
        } else {
            Value::float(max_double(args[0].jfloat(), args[1].jfloat()))
        }),
        ("Math", "min", 2) => Ok(if both_int(&args[0], &args[1]) {
            Value::Int(args[0].jint().min(args[1].jint()))
        } else {
            Value::float(min_double(args[0].jfloat(), args[1].jfloat()))
        }),
        ("Math", "pow", 2) => Ok(Value::float(pow_double(args[0].jfloat(), args[1].jfloat()))),
        ("Math", "sqrt", 1) => Ok(Value::float(args[0].jfloat().sqrt())),
        ("Math", "floor", 1) => Ok(Value::float(args[0].jfloat().floor())),
        ("Math", "ceil", 1) => Ok(Value::float(args[0].jfloat().ceil())),
        ("Math", "round", 1) => Ok(Value::Int(round_double(args[0].jfloat()))),
        // `Math.floorDiv`/`floorMod` round toward negative infinity, unlike `/`
        // and `%` which truncate toward zero: `floorDiv(-7, 2)` is -4.
        ("Math", "floorDiv", 2) => floor_div(args[0].jint(), args[1].jint()).map(Value::Int),
        ("Math", "floorMod", 2) => {
            let (a, b) = (args[0].jint(), args[1].jint());
            // Wrapping for the same reason `floor_div` wraps: with
            // `Long.MIN_VALUE` and -1 the quotient is `Long.MIN_VALUE` and
            // `q * b` overflows, which panicked and aborted. Java answers 0.
            floor_div(a, b).map(|q| Value::Int(a.wrapping_sub(q.wrapping_mul(b))))
        }
        // ── The width-overloaded `Math` statics ──
        // Each carries one extra operand, the [`width`] code the compiler
        // resolved from the arguments' static types. That is also what makes
        // the arity here (`3` for a two-argument method) distinct from any real
        // overload's, so a program cannot reach these arms by accident.
        ("Math", "addExact", 3) => {
            exact_arith(args[0].jint(), args[1].jint(), args[2].jint(), Exact::Add)
        }
        ("Math", "subtractExact", 3) => {
            exact_arith(args[0].jint(), args[1].jint(), args[2].jint(), Exact::Sub)
        }
        ("Math", "multiplyExact", 3) => {
            exact_arith(args[0].jint(), args[1].jint(), args[2].jint(), Exact::Mul)
        }
        // `Math.toIntExact(long)` is the narrowing the `Exact` family exists
        // for: in range it is the value, out of range it is `integer overflow`.
        ("Math", "toIntExact", 2) => {
            let v = args[0].jint();
            match i32::try_from(v) {
                Ok(n) => Ok(Value::Int(n.into())),
                Err(_) => Err(Fault::java("ArithmeticException", "integer overflow")),
            }
        }
        ("Math", "clamp", 4) => math_clamp(&args[0], &args[1], &args[2], args[3].jint()),
        ("Math", "signum", 1) => Ok(Value::float(match args[0].jfloat() {
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
        ("Math", "toRadians", 1) => Ok(Value::float(args[0].jfloat().to_radians())),
        ("Math", "toDegrees", 1) => Ok(Value::float(args[0].jfloat().to_degrees())),
        // The exactly-specified `double` statics, as opposed to the
        // transcendentals just above. Each is an IEEE operation or a walk over
        // the bit pattern, so there is one right answer and Rust gives it:
        // `rint` is roundToIntegralTiesToEven (2.5 is 2.0, 3.5 is 4.0, unlike
        // `round`'s half-up), `fma` is fusedMultiplyAdd with a single rounding,
        // and `ulp`/`nextUp`/`nextDown`/`nextAfter` step the representable
        // neighbours. They were unregistered — an error at the call site — for
        // want of a reason rather than for one.
        ("Math", "rint", 1) => Ok(Value::float(args[0].jfloat().round_ties_even())),
        ("Math", "copySign", 2) => Ok(Value::float(args[0].jfloat().copysign(args[1].jfloat()))),
        ("Math", "fma", 3) => Ok(Value::float(
            args[0].jfloat().mul_add(args[1].jfloat(), args[2].jfloat()),
        )),
        ("Math", "ulp", 1) => Ok(Value::float(double_ulp(args[0].jfloat()))),
        ("Math", "nextUp", 1) => Ok(Value::float(next_after(args[0].jfloat(), f64::INFINITY))),
        ("Math", "nextDown", 1) => Ok(Value::float(next_after(
            args[0].jfloat(),
            f64::NEG_INFINITY,
        ))),
        ("Math", "nextAfter", 2) => {
            Ok(Value::float(next_after(args[0].jfloat(), args[1].jfloat())))
        }

        // ── java.lang.Integer / Long ──
        // A null argument is rejected before the text is looked at; without the
        // guard it coerced to `""` and reported `For input string: ""`, which is
        // the message a genuinely empty string gets. See [`null_number_fault`].
        ("Integer", "parseInt", 1) if matches!(args[0], Value::Undef) => Err(null_number_fault()),
        ("Integer", "parseInt", 2) if matches!(args[0], Value::Undef) => Err(null_number_fault()),
        ("Long", "parseLong", 1) if matches!(args[0], Value::Undef) => Err(null_number_fault()),
        ("Integer", "parseInt", 1) => parse_int_radix(&args[0].as_str_cow(), 10, true),
        ("Integer", "parseInt", 2) => {
            let radix = args[1].jint();
            parse_int_radix(&args[0].as_str_cow(), radix, true)
        }
        ("Long", "parseLong", 1) => parse_int_radix(&args[0].as_str_cow(), 10, false),
        // `Integer.valueOf(String)` parses; `Integer.valueOf(int)` is identity.
        // `null` is neither: it is the `String` overload, and it faults. Left to
        // the `other` arm it read as the `int` overload and answered 0.
        ("Integer", "valueOf", 1) => match &args[0] {
            Value::Str(s) => parse_int_radix(s, 10, true),
            Value::Undef => Err(null_number_fault()),
            other => Ok(Value::Int(other.jint())),
        },
        // `Integer.toString(int)` / `Integer.toString(int, radix)`.
        ("Integer", "toString", 1) => Ok(Value::str(args[0].jint().to_string())),
        ("Integer", "toString", 2) => Ok(Value::str(int_to_radix_string(
            args[0].jint(),
            args[1].jint(),
        ))),
        // The unsigned radix renderings read the value as a *bit pattern* at its
        // declared width — `Integer.toHexString(-1)` is "ffffffff" and
        // `Long.toHexString(-1L)` is sixteen f's.
        ("Integer", "toBinaryString", 1) => {
            Ok(Value::str(format!("{:b}", args[0].jint() as i32 as u32)))
        }
        ("Integer", "toHexString", 1) => {
            Ok(Value::str(format!("{:x}", args[0].jint() as i32 as u32)))
        }
        ("Integer", "toOctalString", 1) => {
            Ok(Value::str(format!("{:o}", args[0].jint() as i32 as u32)))
        }
        ("Long", "toBinaryString", 1) => Ok(Value::str(format!("{:b}", args[0].jint() as u64))),
        ("Long", "toHexString", 1) => Ok(Value::str(format!("{:x}", args[0].jint() as u64))),
        ("Long", "toOctalString", 1) => Ok(Value::str(format!("{:o}", args[0].jint() as u64))),
        ("Integer" | "Long", "compare", 2) => {
            Ok(Value::Int(cmp_to_int(args[0].jint().cmp(&args[1].jint()))))
        }
        ("Integer" | "Long", "max", 2) => Ok(Value::Int(args[0].jint().max(args[1].jint()))),
        ("Integer" | "Long", "min", 2) => Ok(Value::Int(args[0].jint().min(args[1].jint()))),
        ("Integer" | "Long", "sum", 2) => {
            Ok(Value::Int(args[0].jint().wrapping_add(args[1].jint())))
        }
        ("Integer" | "Long", "signum", 1) => Ok(Value::Int(args[0].jint().signum())),
        // `Xxx.hashCode(x)` is the static spelling of the boxed instance
        // method, and each box folds a different width: `Integer` is the value,
        // `Long` folds its halves, `Double` folds `doubleToLongBits`, and
        // `Float` is `floatToIntBits` *unfolded* — so `Float.hashCode(1.5f)` is
        // 1069547520 where `Double.hashCode(1.5)` is 1073217536.
        ("Integer" | "Long" | "Double" | "Boolean" | "Character", "hashCode", 1) => {
            let v = match class {
                "Double" => Value::float(args[0].jfloat()),
                "Boolean" => Value::bool(matches!(args[0], Value::Bool(true))),
                "Character" => Value::Int(i64::from(char_arg(&args[0]) as u32)),
                _ => Value::Int(args[0].jint()),
            };
            Ok(Value::Int(java_hash(&v).unwrap_or(0).into()))
        }
        ("Float", "hashCode", 1) => Ok(Value::Int(
            ((args[0].jfloat() as f32).to_bits() as i32).into(),
        )),
        ("Long", "toString", 1) => Ok(Value::str(args[0].jint().to_string())),
        ("Long", "valueOf", 1) => match &args[0] {
            Value::Str(s) => parse_int_radix(s, 10, false),
            Value::Undef => Err(null_number_fault()),
            other => Ok(Value::Int(other.jint())),
        },

        // ── java.lang.Double ──
        // The floating parsers answer a null with a `NullPointerException`, not
        // with the integral parsers' `NumberFormatException` — see
        // [`null_float_parse_fault`]. javars reported `NumberFormatException:
        // empty String` for both, which is the wrong *class*, so a
        // `catch (NumberFormatException e)` caught what Java does not.
        ("Double" | "Float", "parseDouble" | "parseFloat" | "valueOf", 1)
            if matches!(args[0], Value::Undef) =>
        {
            Err(null_float_parse_fault())
        }
        ("Double", "parseDouble", 1) | ("Double", "valueOf", 1) => {
            let s = args[0].as_str_cow();
            parse_java_double(&s)
                .map(Value::float)
                .ok_or_else(|| Fault::java("NumberFormatException", float_format_message(&s)))
        }
        ("Double", "toString", 1) => Ok(Value::str(format_double(args[0].jfloat()))),

        // ── java.lang.Float ──
        // Every one of these answers at 32-bit precision, which is the whole
        // reason they are not aliases of the `Double` arm above.
        ("Float", "toString", 1) => Ok(Value::str(format_float(args[0].jfloat() as f32))),
        ("Float", "parseFloat", 1) | ("Float", "valueOf", 1) => {
            let s = args[0].as_str_cow();
            parse_java_double(&s)
                .map(|f| Value::float(f as f32 as f64))
                .ok_or_else(|| Fault::java("NumberFormatException", float_format_message(&s)))
        }
        ("Float", "compare", 2) => Ok(Value::Int(float_compare(
            f64::from(args[0].jfloat() as f32),
            f64::from(args[1].jfloat() as f32),
        ))),
        ("Float", "isNaN", 1) => Ok(Value::bool(args[0].jfloat().is_nan())),
        ("Float", "isInfinite", 1) => Ok(Value::bool(args[0].jfloat().is_infinite())),
        ("Double", "compare", 2) => Ok(Value::Int(float_compare(
            args[0].jfloat(),
            args[1].jfloat(),
        ))),
        ("Double", "isNaN", 1) => Ok(Value::bool(args[0].jfloat().is_nan())),
        ("Double", "isInfinite", 1) => Ok(Value::bool(args[0].jfloat().is_infinite())),

        // ── java.lang.Character ──
        // The argument is a `char` code point (`char_arg` also accepts the
        // one-character String a boxed `Character` is). `toUpperCase`/
        // `toLowerCase` return a `char`, so they return a code point too.
        ("Character", "isDigit", 1) => Ok(Value::bool(char_arg(&args[0]).is_ascii_digit())),
        ("Character", "isLetter", 1) => Ok(Value::bool(char_arg(&args[0]).is_alphabetic())),
        ("Character", "isLetterOrDigit", 1) => {
            Ok(Value::bool(char_arg(&args[0]).is_alphanumeric()))
        }
        ("Character", "isWhitespace", 1) => Ok(Value::bool(java_is_whitespace(char_arg(&args[0])))),
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
        // `copyValueOf(char[])` is `valueOf(char[])` — the JDK's own
        // implementation is one call to the other — so it takes the same arm
        // rather than a second reading of the array.
        ("String", "valueOf" | "copyValueOf", 1) => Ok(Value::str(match array_items(&args[0]) {
            Some(items) => items.iter().map(java_str).collect::<String>(),
            None => java_str(&args[0]),
        })),
        // `String.format(fmt, args…)` — printf-style formatting (subset).
        ("String", "format", _) if !args.is_empty() => {
            let fmt = args[0].as_str_cow().into_owned();
            java_format(&fmt, &args[1..], &[], None)
        }

        // `String.join(sep, a, b, …)`, `String.join(sep, array)`, and
        // `String.join(sep, iterable)` — Java's second overload takes an
        // `Iterable<CharSequence>`, so a `List`/`Set` argument joins its
        // *elements*. Matching only arrays here rendered the collection's own
        // `toString` as one part (`String.join("-", List.of("a","b"))` gave
        // `[a, b]` instead of `a-b`).
        // `String.join` dereferences the delimiter before it looks at anything
        // else, so a null one is an NPE rather than a join on "".
        ("String", "join", n) if n >= 2 && matches!(args[0], Value::Undef) => Err(Fault::java(
            "NullPointerException",
            "Cannot invoke \"java.lang.CharSequence.toString()\" because \"delimiter\" is null",
        )),
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
        // Both copies clamp their arguments, and Java does not: a bad length or
        // a reversed range threw where javars silently answered an array.
        // `Arrays.copyOf` allocates before it copies, so a negative length is
        // the allocation's own `NegativeArraySizeException`.
        ("Arrays", "copyOf", 2) => {
            let items = array_items(&args[0]).unwrap_or_default();
            let len = args[1].jint();
            if len < 0 {
                return Err(Fault::java("NegativeArraySizeException", len.to_string()));
            }
            let pad = element_default(&items);
            let mut out = items;
            out.resize(len as usize, pad);
            Ok(Value::Obj(heap_alloc(HostObj::Array(out))))
        }
        ("Arrays", "copyOfRange", 3) => {
            let items = array_items(&args[0]).unwrap_or_default();
            let (from, to) = (args[1].jint(), args[2].jint());
            // `Arrays.copyOfRange` checks the range itself and reports it with
            // the two endpoints alone; a `from` outside the source is left to
            // the `System.arraycopy` underneath, whose message names the
            // element type javars has erased, so that one keeps the class and
            // omits the text (BUGS.md).
            if from > to {
                return Err(Fault::java(
                    "IllegalArgumentException",
                    format!("{from} > {to}"),
                ));
            }
            if from < 0 || from > items.len() as i64 {
                return Err(Fault::java("ArrayIndexOutOfBoundsException", String::new()));
            }
            let (from, to) = (from as usize, to as usize);
            let pad = element_default(&items);
            let mut out: Vec<Value> = items
                .get(from..to.min(items.len()))
                .unwrap_or_default()
                .to_vec();
            out.resize(to - from, pad);
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
/// `from`, or -1.
///
/// The clamping order is the JDK's (`StringLatin1.lastIndexOf`) and it matters:
/// `fromIndex` is first pulled *down* to the last position a needle of this
/// length could start at, and only then rejected for being negative. Testing
/// the empty needle before that ordering — which is what a `clamp(0, len)`
/// does — answered `"abc".lastIndexOf("", -1)` with 0 where Java answers -1.
fn char_last_index_of(hay: &str, needle: &str, from: i64) -> i64 {
    let chars: Vec<char> = hay.chars().collect();
    let pat: Vec<char> = needle.chars().collect();
    let start = from.min(chars.len() as i64 - pat.len() as i64);
    if start < 0 {
        return -1;
    }
    if pat.is_empty() {
        return start;
    }
    (0..=start)
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
    // The split is by *which* group reference failed, not by the shared "No
    // group" prefix. `Matcher.appendExpandedReplacement` throws
    // `IndexOutOfBoundsException` for a numbered group that does not exist
    // (`$9`) and `IllegalArgumentException` for a named one (`${nope}`), so
    // keying on the prefix alone gave the named case the numbered case's
    // class. Measured on `openjdk 21.0.12`.
    if msg.starts_with("No group") && !msg.starts_with("No group with name") {
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

/// Which of the three exact binary operations [`exact_arith`] performs.
enum Exact {
    /// `Math.addExact`.
    Add,
    /// `Math.subtractExact`.
    Sub,
    /// `Math.multiplyExact`.
    Mul,
}

/// `Math.addExact` / `subtractExact` / `multiplyExact` at the width the
/// compiler resolved.
///
/// The `int` overloads compute in `i64` and then check the `i32` range, which is
/// exact: no sum, difference or product of two `i32`s can leave `i64`. The
/// `long` ones use the checked operations, because there is nothing wider to
/// compute in. Java's messages are `integer overflow` and `long overflow`, and
/// which one a program sees is the whole reason the width travels with the call.
fn exact_arith(a: i64, b: i64, width: i64, op: Exact) -> Result<Value, Fault> {
    if width == width::LONG {
        let r = match op {
            Exact::Add => a.checked_add(b),
            Exact::Sub => a.checked_sub(b),
            Exact::Mul => a.checked_mul(b),
        };
        return match r {
            Some(v) => Ok(Value::Int(v)),
            None => Err(Fault::java("ArithmeticException", "long overflow")),
        };
    }
    let r = match op {
        Exact::Add => a + b,
        Exact::Sub => a - b,
        Exact::Mul => a * b,
    };
    match i32::try_from(r) {
        Ok(n) => Ok(Value::Int(n.into())),
        Err(_) => Err(Fault::java("ArithmeticException", "integer overflow")),
    }
}

/// `Math.clamp(value, min, max)` at the width the compiler resolved.
///
/// The bounds decide the overload, so `clamp(aLong, 1, 10)` answers an `int` and
/// `clamp(aLong, 1L, 10L)` a `long`. Verified against openjdk 26.0.2 for the
/// cases that are not `min(max(v, lo), hi)`: `min` or `max` being NaN is
/// `IllegalArgumentException: min is NaN` / `max is NaN`, `min > max` is
/// `IllegalArgumentException: "<min> > <max>"` rendered at the overload's width,
/// a NaN *value* passes through as NaN, and the signed zeros order
/// (`clamp(-1.0, -0.0, 0.0)` is `-0.0`).
fn math_clamp(value: &Value, min: &Value, max: &Value, width: i64) -> Result<Value, Fault> {
    if width == width::INT || width == width::LONG {
        let (v, lo, hi) = (value.jint(), min.jint(), max.jint());
        if lo > hi {
            return Err(Fault::java(
                "IllegalArgumentException",
                format!("{lo} > {hi}"),
            ));
        }
        return Ok(Value::Int(v.max(lo).min(hi)));
    }
    let (v, lo, hi) = (value.jfloat(), min.jfloat(), max.jfloat());
    let render = |x: f64| {
        if width == width::FLOAT {
            format_float(x as f32)
        } else {
            format_double(x)
        }
    };
    if lo.is_nan() {
        return Err(Fault::java("IllegalArgumentException", "min is NaN"));
    }
    if hi.is_nan() {
        return Err(Fault::java("IllegalArgumentException", "max is NaN"));
    }
    if float_compare(lo, hi) > 0 {
        return Err(Fault::java(
            "IllegalArgumentException",
            format!("{} > {}", render(lo), render(hi)),
        ));
    }
    Ok(Value::float(java_min(java_max(v, lo), hi)))
}

/// Java's `Math.max(double, double)`: NaN wins, and `-0.0` is below `0.0`.
///
/// Rust's `f64::max` disagrees on both — it *drops* a NaN operand and treats the
/// two zeros as interchangeable — so the ordering comes from
/// [`float_compare`], which is `Double.compare`'s total order.
fn java_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        return f64::NAN;
    }
    if float_compare(a, b) >= 0 {
        a
    } else {
        b
    }
}

/// Java's `Math.min(double, double)`; [`java_max`]'s counterpart.
fn java_min(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        return f64::NAN;
    }
    if float_compare(a, b) <= 0 {
        a
    } else {
        b
    }
}

/// Java's `Math.floorDiv`: integer division rounded toward negative infinity
/// rather than toward zero, so `floorDiv(-7, 2)` is -4 where `-7 / 2` is -3.
fn floor_div(a: i64, b: i64) -> Result<i64, Fault> {
    if b == 0 {
        return Err(Fault::java("ArithmeticException", "/ by zero"));
    }
    let q = a.wrapping_div(b);
    // One correction step when the signs differ and the division was inexact.
    //
    // Every arithmetic step here wraps, because `Long.MIN_VALUE / -1` reaches
    // all three. The divide already used `wrapping_div`; plain `%` panicked
    // ("attempt to calculate the remainder with overflow") and aborted the
    // process, and `q - 1` would have been the next panic. Java's answer is
    // `Long.MIN_VALUE`: the remainder is 0, so no correction applies.
    Ok(if a.wrapping_rem(b) != 0 && (a ^ b) < 0 {
        q.wrapping_sub(1)
    } else {
        q
    })
}

/// Java's `Character.isWhitespace(int)`.
///
/// Rust's `char::is_whitespace` is the Unicode `White_Space` property, and the
/// two sets are different in *both* directions — which is why neither
/// `String.strip` nor `String.isBlank` can be spelled with it. Java's
/// definition (its own Javadoc) is a Unicode space separator that is not one of
/// the three non-breaking spaces, plus the five ASCII controls `\t\n\f\r`
/// and the four information separators ``–``. So `White_Space`
/// includes `U+00A0`, `U+2007`, `U+202F` and `U+0085` that Java excludes, and
/// excludes `U+001C`–`U+001F` that Java includes.
///
/// Enumerated rather than derived: Rust exposes no general-category table, and
/// the space separators have not changed since Unicode 4, so the list is stable
/// for as long as the property is.
fn java_is_whitespace(c: char) -> bool {
    matches!(c,
        // The ASCII controls Java names one by one, then SPACE.
        '\u{09}'..='\u{0D}' | '\u{1C}'..='\u{1F}' | '\u{20}'
        // Zs (SPACE_SEPARATOR) less the non-breaking U+00A0, U+2007, U+202F.
        | '\u{1680}' | '\u{2000}'..='\u{2006}' | '\u{2008}'..='\u{200A}'
        | '\u{205F}' | '\u{3000}'
        // Zl (LINE_SEPARATOR) and Zp (PARAGRAPH_SEPARATOR).
        | '\u{2028}' | '\u{2029}')
}

/// `Double.parseDouble` / `Float.parseFloat` — Java's accepted grammar, which is
/// not Rust's.
///
/// `f64::from_str` and `Double.valueOf` disagree at both ends. Rust accepts
/// `inf`, `infinity` and `nan` in any case, where Java accepts only the exact
/// spellings `Infinity` and `NaN` and throws on the rest; Rust rejects the
/// `d`/`D`/`f`/`F` type suffix that Java's `FloatingPointLiteral` allows, so
/// `Double.parseDouble("1d")` is 1.0 in Java and an error under `from_str`.
/// Both were live: `parseDouble("inf")` answered `Infinity` here and
/// `NumberFormatException` on `openjdk 21.0.12`.
///
/// The grammar is validated explicitly rather than delegated, so an input Rust
/// happens to accept cannot slip through a future toolchain. `None` is the
/// caller's `NumberFormatException`.
fn parse_java_double(text: &str) -> Option<f64> {
    // `FloatingDecimal.readJavaFormatString` trims first, with `String.trim()`
    // — chars <= U+0020, not the Unicode set.
    let s = text.trim_matches(|c: char| c <= ' ');
    let (negative, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    if body.is_empty() {
        return None;
    }
    let signed = |v: f64| if negative { -v } else { v };
    // Case-sensitive, and the sign is already off: `Double.parseDouble("nan")`
    // is an error in Java however Rust spells it.
    if body == "NaN" {
        return Some(f64::NAN);
    }
    if body == "Infinity" {
        return Some(signed(f64::INFINITY));
    }
    // The optional `FloatTypeSuffix`. A hex significand needs a `p` exponent to
    // be legal at all, so stripping a trailing `D`/`F` off one cannot turn an
    // invalid literal into a valid one.
    let digits = match body.as_bytes().last() {
        Some(b'f' | b'F' | b'd' | b'D') => &body[..body.len() - 1],
        _ => body,
    };
    if !is_java_decimal_literal(digits) {
        return None;
    }
    // The grammar is now known to be a subset of Rust's, so the conversion
    // itself — correctly rounded in both — can be delegated.
    digits.parse::<f64>().ok().map(signed)
}

/// The `NumberFormatException` message `Double.parseDouble`/`Float.parseFloat`
/// carries for input they reject.
///
/// The floating parsers do *not* share the integral ones' single message.
/// `FloatingDecimal.readJavaFormatString` trims, and answers `empty String` for
/// what is left of nothing — so `Double.parseDouble("")` and
/// `Double.parseDouble("   ")` both report that, where `Integer.parseInt("")`
/// reports `For input string: ""`. Measured on `openjdk 21.0.12`.
/// The failure `Integer.parseInt(null)` / `Long.parseLong(null)` /
/// `Integer.valueOf((String) null)` raises.
///
/// `Integer.parseInt` checks its argument for null *before* it looks at any
/// character, so the message is not the `For input string: ""` an empty string
/// gets — a null and an empty string are distinguishable outcomes. Measured on
/// `openjdk 21.0.12`.
fn null_number_fault() -> Fault {
    Fault::java("NumberFormatException", "Cannot parse null string")
}

/// The failure `Double.parseDouble(null)` / `Float.parseFloat(null)` raises —
/// which is a *different class* from the integral parsers' answer above.
///
/// `FloatingDecimal.readJavaFormatString` has no null check at all: it calls
/// `in.trim()` on its argument and the dereference is what fails, so a null
/// reaches the caller as a `NullPointerException`, not as the
/// `NumberFormatException` every other parse failure raises. A program that
/// writes `catch (NumberFormatException e)` around `Double.parseDouble` does not
/// catch this one, so answering with the integral parsers' class would let code
/// through a handler Java sends it past. The quoted name `in` is
/// `readJavaFormatString`'s own parameter, not a bytecode slot javars would have
/// to invent. Measured on `openjdk 21.0.12`.
fn null_float_parse_fault() -> Fault {
    Fault::java(
        "NullPointerException",
        "Cannot invoke \"String.trim()\" because \"in\" is null",
    )
}

fn float_format_message(text: &str) -> String {
    if text.trim_matches(|c: char| c <= ' ').is_empty() {
        "empty String".to_string()
    } else {
        format!("For input string: \"{text}\"")
    }
}

/// Whether `s` is a Java `FloatingPointLiteral` body: sign and type suffix
/// already removed, and no `Infinity`/`NaN`/hex form.
///
/// Java requires at least one digit somewhere in the significand (so `.` alone
/// and `e5` are errors) and at least one digit in an exponent that is present
/// at all. It permits no underscores, no leading/interior whitespace, and no
/// trailing text.
fn is_java_decimal_literal(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    let mut significand_digits = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
        significand_digits += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
            significand_digits += 1;
        }
    }
    if significand_digits == 0 {
        return false;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let exponent_start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == exponent_start {
            return false;
        }
    }
    i == b.len()
}

/// Java's `Math.max(double, double)`, ported from `java.lang.Math`.
///
/// Rust's `f64::max` is `fmax`, which *ignores* a NaN operand and answers the
/// other one; Java propagates it. The two also part over signed zero, where
/// `fmax` is permitted to return either. Both departures are load-bearing —
/// `Math.max(Double.NaN, 1.0)` is `NaN` in Java and `1.0` under `f64::max`.
fn max_double(a: f64, b: f64) -> f64 {
    if a.is_nan() {
        return a;
    }
    // Raw bits are safe here: NaN is already out, and only `-0.0` carries the
    // sign bit over a zero.
    if a == 0.0 && b == 0.0 && a.is_sign_negative() {
        return b;
    }
    if a >= b {
        a
    } else {
        b
    }
}

/// Java's `Math.min(double, double)`, ported from `java.lang.Math` — the mirror
/// of [`max_double`], including the NaN propagation `f64::min` does not do.
fn min_double(a: f64, b: f64) -> f64 {
    if a.is_nan() {
        return a;
    }
    if a == 0.0 && b == 0.0 && b.is_sign_negative() {
        return b;
    }
    if a <= b {
        a
    } else {
        b
    }
}

/// Java's `Math.pow`, which is IEEE 754 `pow` with two documented exceptions.
///
/// `java.lang.Math.pow`'s contract says "if the second argument is NaN, then
/// the result is NaN" with no carve-out for a base of 1, and "if the absolute
/// value of the first argument equals 1 and the second argument is infinite,
/// then the result is NaN". IEEE 754 — and so Rust's `powf` — instead makes
/// `pow(1, y)` equal 1 for *every* `y`, NaN and infinity included. The
/// zero-exponent rule comes first in Java's own list, so `pow(NaN, 0.0)` stays
/// 1.0; everything outside these cases is left to `powf`.
fn pow_double(a: f64, b: f64) -> f64 {
    if b == 0.0 {
        return 1.0;
    }
    if b.is_nan() || (a.abs() == 1.0 && b.is_infinite()) {
        return f64::NAN;
    }
    a.powf(b)
}

/// Java's `Math.round(double)`, ported from `java.lang.Math`.
///
/// The obvious spelling — `(long) Math.floor(a + 0.5)` — is the *pre-Java-7*
/// implementation, and it is wrong wherever `a + 0.5` is not exactly
/// representable: `0.49999999999999994 + 0.5` rounds up to exactly `1.0`, so
/// the naive form answers 1 where every JDK since 7 answers 0 (JDK-6430675).
/// Rust's `f64::round` is not it either — that is half-away-from-zero, so it
/// answers -3 for `-2.5` where Java's half-*up* answers -2.
///
/// The JDK avoids the addition entirely: it reads the significand as an
/// integer, shifts it down to leave one fractional bit, and rounds that bit
/// with `(x + 1) >> 1`. No intermediate rounding can occur, so the tie case is
/// decided by the bits actually present. `shift` outside `0..64` means the
/// value is either already a mathematical integer, smaller in magnitude than
/// 1/2, or non-finite — all four of which `(long) a` answers directly, and
/// Rust's saturating `as i64` matches Java's narrowing (0 for NaN, the
/// `Long` extremes for the infinities).
fn round_double(a: f64) -> i64 {
    // `DoubleConsts`: SIGNIFICAND_WIDTH 53, EXP_BIAS 1023.
    const EXP_BIT_MASK: i64 = 0x7FF0_0000_0000_0000u64 as i64;
    const SIGNIF_BIT_MASK: i64 = 0x000F_FFFF_FFFF_FFFF;
    let long_bits = a.to_bits() as i64;
    let biased_exp = (long_bits & EXP_BIT_MASK) >> (53 - 1);
    let shift = (53 - 2 + 1023) - biased_exp;
    if (shift & -64) == 0 {
        let mut r = (long_bits & SIGNIF_BIT_MASK) | (SIGNIF_BIT_MASK + 1);
        if long_bits < 0 {
            r = -r;
        }
        ((r >> shift) + 1) >> 1
    } else {
        a as i64
    }
}

/// Java's `Math.round(float)`, ported from `java.lang.Math`.
///
/// The same algorithm one width down, and it is a *separate* method rather than
/// a narrowing of [`round_double`] because its result is an `int`: Java
/// saturates `Math.round(1.0e20f)` at `Integer.MAX_VALUE`, where truncating the
/// `long` answer to 32 bits would give -1.
fn round_float(a: f32) -> i32 {
    // `FloatConsts`: SIGNIFICAND_WIDTH 24, EXP_BIAS 127.
    const EXP_BIT_MASK: i32 = 0x7F80_0000;
    const SIGNIF_BIT_MASK: i32 = 0x007F_FFFF;
    let int_bits = a.to_bits() as i32;
    let biased_exp = (int_bits & EXP_BIT_MASK) >> (24 - 1);
    let shift = (24 - 2 + 127) - biased_exp;
    if (shift & -32) == 0 {
        let mut r = (int_bits & SIGNIF_BIT_MASK) | (SIGNIF_BIT_MASK + 1);
        if int_bits < 0 {
            r = -r;
        }
        ((r >> shift) + 1) >> 1
    } else {
        a as i32
    }
}

/// The `-1`/`0`/`1` an `Integer.compare`-style method returns.
/// `Double.compare` / `Float.compare` — a *total* order over the doubles, which
/// `<`/`>`/`==` are not.
///
/// Java specifies three departures from the operators, all of which a
/// `record`'s derived `equals` and a `TreeSet<Double>` depend on: `-0.0` sorts
/// strictly below `0.0`, `NaN` compares equal to itself, and `NaN` sorts above
/// every other value including `+Infinity`. `partial_cmp` answers `None` for a
/// `NaN` operand and `Equal` for `0.0` against `-0.0`, so it cannot express any
/// of the three; the JDK's own implementation compares the raw bit patterns
/// once the numeric case is out of the way, and so does this.
fn float_compare(a: f64, b: f64) -> i64 {
    if a < b {
        return -1;
    }
    if a > b {
        return 1;
    }
    // Neither `<` nor `>` leaves exactly two cases: `0.0` against `-0.0`, and
    // any pair involving `NaN`. Both are settled the way the JDK settles them,
    // by comparing `doubleToLongBits` as a signed long — `-0.0` is
    // `Long.MIN_VALUE` and so below `0.0`'s zero, and a canonical `NaN` is
    // above every finite pattern and equal to itself. The sign bit needs no
    // folding here because the ordinary negatives never reach this point.
    let (ab, bb) = (canonical_bits(a), canonical_bits(b));
    cmp_to_int(ab.cmp(&bb))
}

/// `Double.doubleToLongBits` as a signed long: every `NaN` collapses to the one
/// canonical pattern the JDK reports, so two different `NaN` encodings compare
/// equal.
fn canonical_bits(v: f64) -> i64 {
    if v.is_nan() {
        0x7ff8_0000_0000_0000u64 as i64
    } else {
        v.to_bits() as i64
    }
}

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
    // Through a `Character` box too: `Character.isLetter(aCharacter)` reads the
    // wrapper as its code point, and the handle would otherwise render as text
    // whose first character is a digit of the id.
    match &deboxed(v) {
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
            "NullPointerException",
            "null array".to_string(),
        ));
    };
    HEAP.with(|h| match h.borrow_mut().get_mut(*id as usize) {
        Some(HostObj::Array(a)) => {
            f(a);
            Ok(())
        }
        _ => Err(Fault::java(
            "NullPointerException",
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
fn java_format(
    fmt: &str,
    args: &[Value],
    tags: &[&str],
    mut vm: Option<&mut VM>,
) -> Result<Value, Fault> {
    let mut out = String::new();
    let mut chars = fmt.chars().peekable();
    let mut argi = 0usize;
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        // The specifier's own source text, accumulated as it is consumed.
        // `MissingFormatArgumentException`'s message is the specifier verbatim
        // — `Format specifier '%,10.2f'` — so it cannot be rebuilt from the
        // parsed flags without re-deriving the original spelling and ordering.
        let mut spec = String::from("%");
        // An explicit argument index, `%2$s`. It is digits followed by `$`, so
        // it can only be told from a width by scanning past the digits first.
        let mut lead = String::new();
        while let Some(&d) = chars.peek() {
            if d.is_ascii_digit() {
                lead.push(d);
                spec.push(d);
                chars.next();
            } else {
                break;
            }
        }
        let mut explicit_index: Option<usize> = None;
        if chars.peek() == Some(&'$') {
            chars.next();
            spec.push('$');
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
        // `%​ d` shows a leading space where `%+d` would show a `+`, and `%#x`
        // writes the radix prefix Java calls the "alternate form". Both were
        // parsed and discarded, so ``String.format("% d", 42)`` answered `42`
        // (Java: ` 42`) and `%#x` of 255 answered `ff` (Java: `0xff`).
        let mut space = false;
        let mut alt = false;
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
                ' ' => space = true,
                '#' => alt = true,
                _ => break,
            }
            spec.push(f);
            chars.next();
        }
        // width
        let mut width = lead;
        while let Some(&d) = chars.peek() {
            if d.is_ascii_digit() {
                width.push(d);
                spec.push(d);
                chars.next();
            } else {
                break;
            }
        }
        // .precision
        let mut prec: Option<usize> = None;
        if chars.peek() == Some(&'.') {
            chars.next();
            spec.push('.');
            let mut p = String::new();
            while let Some(&d) = chars.peek() {
                if d.is_ascii_digit() {
                    p.push(d);
                    spec.push(d);
                    chars.next();
                } else {
                    break;
                }
            }
            // Java's width and precision are `int`. A digit string that does not
            // fit one is rejected outright, and the detail message is the
            // overflowed value — literally `-2147483648` for every such input,
            // measured on openjdk 21.0.12. javars parsed into `usize` instead:
            // `%.99999999999f` reached `format!("{:.*}", prec + 30, x)` and
            // *panicked* ("Formatting argument out of range"), and
            // `%99999999999d` reached `pad` and hung building the padding. Both
            // are catchable `IllegalFormatException`s in Java.
            prec = Some(int_format_field(&p, "IllegalFormatPrecisionException")?);
        }
        let conv = chars.next().ok_or_else(|| {
            // Java's own class for a `%` with nothing after it.
            Fault::java("UnknownFormatConversionException", "Conversion = '%'")
        })?;
        spec.push(conv);
        let width_n: Option<usize> = if width.is_empty() {
            None
        } else {
            Some(int_format_field(&width, "IllegalFormatWidthException")?)
        };
        let flags = FmtFlags {
            left,
            alt,
            plus,
            space,
            zero,
            group,
            parens,
        };
        check_format_flags(conv, &flags, width_n, prec, &spec)?;
        match conv {
            // Width applies to the literal conversions too: `%5%` is four
            // spaces and a `%`.
            '%' => out.push_str(&pad("", "%", "", width_n, left, false)),
            'n' => out.push('\n'),
            _ => {
                // An explicit `%n$` index does not advance the implicit cursor,
                // which is what lets `%2$s %1$s` repeat and reorder arguments.
                let idx = explicit_index.unwrap_or(argi);
                // Java's `MissingFormatArgumentException`, naming the specifier
                // that had no argument — not an internal javars error, which
                // aborted the run where Java lets the program catch it.
                let arg = args.get(idx).ok_or_else(|| {
                    Fault::java(
                        "MissingFormatArgumentException",
                        format!("Format specifier '{spec}'"),
                    )
                })?;
                if explicit_index.is_none() {
                    argi += 1;
                }
                let tag = tags.get(idx).copied().unwrap_or("");
                check_conversion(conv, arg, tag)?;
                let Rendered {
                    mut prefix,
                    mut body,
                    numeric,
                } = format_conversion(conv, arg, prec, &flags, tag, vm.as_deref_mut())?;
                if group && numeric {
                    body = group_digits(&body);
                }
                // The `(` flag wraps a negative number in parentheses instead of
                // showing its minus sign. The parentheses count toward the
                // width and the zero padding goes *inside* them, which is why
                // the sign travels as its own piece rather than glued to the
                // digits: `%(08d` of -1 is `(000001)`.
                let mut suffix = "";
                if parens && numeric && prefix == "-" {
                    prefix = "(".to_string();
                    suffix = ")";
                }
                out.push_str(&pad(&prefix, &body, suffix, width_n, left, zero && numeric));
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
    match java_format(&fmt, body, &tags, Some(&mut *vm)) {
        Ok(v) => v,
        Err(f) => raise(vm, f),
    }
}

/// The flag characters of one `String.format` conversion.
struct FmtFlags {
    left: bool,
    alt: bool,
    plus: bool,
    space: bool,
    zero: bool,
    group: bool,
    parens: bool,
}

impl FmtFlags {
    /// The flags this set holds that are also in `want`, spelled in
    /// `java.util.Formatter$Flags`' declaration order (`-`, `#`, `+`, ` `, `0`,
    /// `,`, `(`) — the order its `toString` uses, and therefore the order every
    /// flag-related exception message spells them in.
    fn spell(&self, want: &[char]) -> String {
        [
            ('-', self.left),
            ('#', self.alt),
            ('+', self.plus),
            (' ', self.space),
            ('0', self.zero),
            (',', self.group),
            ('(', self.parens),
        ]
        .iter()
        .filter(|(c, set)| *set && want.contains(c))
        .map(|(c, _)| *c)
        .collect()
    }
}

/// Reject the flag/conversion combinations `java.util.Formatter` rejects, with
/// its own exception class and detail message.
///
/// Every one of these used to be accepted and silently ignored, so
/// `String.format("%,x", 1)` answered `1` where Java throws — the format string
/// said something the program could not have meant, and nothing said so. The
/// checks run in the JDK's order, which is observable: `%,(x` reports `,` (the
/// per-conversion group check) rather than `(` (the later sign-flag check).
fn check_format_flags(
    conv: char,
    f: &FmtFlags,
    width: Option<usize>,
    prec: Option<usize>,
    spec: &str,
) -> Result<(), Fault> {
    let mismatch = |want: &[char]| -> Result<(), Fault> {
        let bad = f.spell(want);
        if bad.is_empty() {
            return Ok(());
        }
        Err(Fault::java(
            "FormatFlagsConversionMismatchException",
            format!("Conversion = {conv}, Flags = {bad}"),
        ))
    };
    let bad_flags = |want: &[char]| -> Fault {
        Fault::java(
            "IllegalFormatFlagsException",
            format!("Flags = '{}'", f.spell(want)),
        )
    };
    let missing_width = || Fault::java("MissingFormatWidthException", spec.to_string());
    let bad_precision = || {
        Fault::java(
            "IllegalFormatPrecisionException",
            prec.unwrap_or(0).to_string(),
        )
    };
    const ALL: &[char] = &['-', '#', '+', ' ', '0', ',', '('];
    // `%n` is a line separator, not a conversion: it takes no flag, no width,
    // and no precision.
    if conv == 'n' {
        if !f.spell(ALL).is_empty() {
            return Err(bad_flags(ALL));
        }
        if let Some(w) = width {
            return Err(Fault::java("IllegalFormatWidthException", w.to_string()));
        }
        if let Some(p) = prec {
            return Err(Fault::java(
                "IllegalFormatPrecisionException",
                p.to_string(),
            ));
        }
        return Ok(());
    }
    if conv == '%' {
        if f.left && width.is_none() {
            return Err(missing_width());
        }
        return Ok(());
    }
    match conv {
        // The general conversions take neither a sign nor a numeric layout.
        // `#` is checked *after* the width, which is why `%,#s` reports `,`.
        's' | 'S' | 'b' | 'B' | 'h' | 'H' => {
            let mut bad: Vec<char> = vec!['+', ' ', '0', ',', '('];
            if !matches!(conv, 's' | 'S') {
                bad.push('#');
            }
            mismatch(&bad)?;
            if f.left && width.is_none() {
                return Err(missing_width());
            }
            if matches!(conv, 's' | 'S') {
                mismatch(&['#'])?;
            }
        }
        'c' | 'C' => {
            if prec.is_some() {
                return Err(bad_precision());
            }
            mismatch(&['#', '+', ' ', '0', ',', '('])?;
            if f.left && width.is_none() {
                return Err(missing_width());
            }
        }
        // The numeric conversions share `checkNumeric`: `-`/`0` need a width,
        // and `+`/` ` and `-`/`0` are mutually exclusive.
        _ => {
            if width.is_none() && (f.left || f.zero) {
                return Err(missing_width());
            }
            if f.plus && f.space {
                return Err(bad_flags(&['+', ' ']));
            }
            if f.left && f.zero {
                return Err(bad_flags(&['-', '0']));
            }
            match conv {
                'd' => {
                    mismatch(&['#'])?;
                    if prec.is_some() {
                        return Err(bad_precision());
                    }
                }
                // The radix conversions render a two's-complement bit pattern,
                // which has no sign to decorate and no groups to separate.
                'o' | 'x' | 'X' => {
                    mismatch(&[','])?;
                    mismatch(&['+', ' ', '('])?;
                    if prec.is_some() {
                        return Err(bad_precision());
                    }
                }
                'e' | 'E' => mismatch(&[','])?,
                'g' | 'G' => mismatch(&['#'])?,
                _ => {}
            }
        }
    }
    Ok(())
}

/// One rendered conversion, split so the padding can be inserted in the right
/// place. `prefix` is the sign or radix marker (`-`, `+`, a leading space,
/// `0x`), `body` the digits or text; zero padding goes *between* them, which is
/// what makes `% 08d` of 1 ` 0000001` and `%#010x` of 255 `0x000000ff`.
struct Rendered {
    prefix: String,
    body: String,
    numeric: bool,
}

impl Rendered {
    fn text(body: String) -> Self {
        Rendered {
            prefix: String::new(),
            body,
            numeric: false,
        }
    }
}

/// Render one `String.format` conversion.
fn format_conversion(
    conv: char,
    arg: &Value,
    prec: Option<usize>,
    flags: &FmtFlags,
    tag: &str,
    vm: Option<&mut VM>,
) -> Result<Rendered, Fault> {
    // The sign piece a non-negative number carries: `+` for the `+` flag, a
    // space for the ` ` flag, nothing otherwise. A negative one carries its own
    // `-`, which the callers below put in `prefix`.
    let pos_sign = || {
        if flags.plus {
            "+"
        } else if flags.space {
            " "
        } else {
            ""
        }
    };
    let float_sign = |x: f64| {
        if x.is_sign_negative() {
            "-".to_string()
        } else {
            pos_sign().to_string()
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
        return Ok(Rendered::text(s));
    }
    let num = |prefix: String, body: String| Rendered {
        prefix,
        body,
        numeric: true,
    };
    match conv {
        'd' => {
            let n = arg.jint();
            Ok(num(
                if n < 0 {
                    "-".to_string()
                } else {
                    pos_sign().to_string()
                },
                n.unsigned_abs().to_string(),
            ))
        }
        'f' => {
            let x = arg.jfloat();
            // `#` on a fixed conversion forces the decimal point to appear even
            // at precision 0: `%#.0f` of 1.0 is `1.`.
            let mut body = fixed_half_up(x, prec.unwrap_or(6));
            if flags.alt && !body.contains('.') {
                body.push('.');
            }
            Ok(num(float_sign(x), body))
        }
        // `%s`/`%S` are `Formatter`'s call to the argument's own `toString()`, so
        // they are the two conversions a user override answers for. The rest
        // read the value numerically and never render an object.
        's' | 'S' => {
            let mut s = match vm {
                Some(vm) => java_str_vm(vm, arg),
                None => java_str(arg),
            };
            if conv == 'S' {
                s = s.to_uppercase();
            }
            if let Some(p) = prec {
                s = s.chars().take(p).collect();
            }
            Ok(Rendered::text(s))
        }
        // The general conversions all truncate to the precision, not just `%s`
        // — `%.2b` of `true` is `tr`.
        'b' | 'B' => {
            let mut s = java_bool(arg).to_string();
            if conv == 'B' {
                s = s.to_uppercase();
            }
            if let Some(p) = prec {
                s = s.chars().take(p).collect();
            }
            Ok(Rendered::text(s))
        }
        // The radix conversions read the argument as an *unsigned* bit pattern
        // at the width its declared type has — `%x` of the `int` -1 is
        // `ffffffff` and of the `long` -1 eight more `f`s. javars stores both in
        // one 64-bit `Value::Int`, so the width comes from the compiler's type
        // tag; without it every negative `int` rendered sixteen digits.
        'x' | 'X' | 'o' => {
            let bits = radix_bits(arg, tag);
            let body = match conv {
                'x' => format!("{bits:x}"),
                'X' => format!("{bits:X}"),
                _ => format!("{bits:o}"),
            };
            // `#` writes Java's alternate form: `0x`/`0X` for hex, a leading
            // `0` for octal. It sits ahead of any zero padding.
            let prefix = if flags.alt {
                match conv {
                    'x' => "0x",
                    'X' => "0X",
                    _ => "0",
                }
            } else {
                ""
            };
            Ok(num(prefix.to_string(), body))
        }
        'c' => Ok(Rendered::text(match arg {
            // `%c` on an integer renders its code point as a character.
            Value::Int(n) => char::from_u32(*n as u32).unwrap_or('\u{fffd}').to_string(),
            other => java_str(other),
        })),
        // Java's `%e` always writes a two-digit exponent with an explicit sign
        // (`1.234568e+03`), where Rust's `{:e}` writes `1.234568e3`.
        'e' | 'E' => {
            let x = arg.jfloat();
            // `sci_notation` carries a negative sign; the split rendering wants
            // the magnitude, so it is stripped and re-supplied as the prefix.
            let s = sci_notation(x, prec.unwrap_or(6));
            let body = s.strip_prefix('-').unwrap_or(&s).to_string();
            let body = if conv == 'E' {
                body.to_uppercase()
            } else {
                body
            };
            Ok(num(float_sign(x), body))
        }
        // `%g` picks fixed or scientific by the value's magnitude; Java's
        // precision counts *significant* digits and defaults to 6.
        'g' | 'G' => {
            let x = arg.jfloat();
            let p = prec.unwrap_or(6).max(1);
            let s = if x != 0.0 && (x.abs() < 1e-4 || x.abs() >= 10f64.powi(p as i32)) {
                sci_notation(x, p - 1)
            } else {
                let exp = if x == 0.0 {
                    0
                } else {
                    x.abs().log10().floor() as i32
                };
                fixed_half_up(x, (p as i32 - 1 - exp).max(0) as usize)
            };
            let body = s.strip_prefix('-').unwrap_or(&s).to_string();
            let body = if conv == 'G' {
                body.to_uppercase()
            } else {
                body
            };
            Ok(num(float_sign(x), body))
        }
        // `%h` is the argument's `hashCode()` in hex, or "null".
        'h' | 'H' => {
            let s = match arg {
                Value::Undef => "null".to_string(),
                other => format!("{:x}", java_hash(other).unwrap_or(0) as u32),
            };
            let mut s = if conv == 'H' { s.to_uppercase() } else { s };
            if let Some(p) = prec {
                s = s.chars().take(p).collect();
            }
            Ok(Rendered::text(s))
        }
        // Java's own class and wording for a conversion character it does not
        // define. javars reported an internal error, which the program could not
        // catch even though `catch (IllegalArgumentException e)` catches it in
        // Java (`UnknownFormatConversionException` is an `IllegalFormatException`
        // is an `IllegalArgumentException` — read off `getSuperclass()` on
        // openjdk 21.0.12).
        other => Err(Fault::java(
            "UnknownFormatConversionException",
            format!("Conversion = '{other}'"),
        )),
    }
}

/// The `NullPointerException` a `String` method raises when its first argument
/// is `null`, or `Ok(())` when this method tolerates one.
///
/// Every one of these dereferences its argument, so Java fails before it can
/// compute anything. javars coerced `null` to `""` instead and answered:
/// `"abc".compareTo(null)` was 3, `"abc".startsWith(null)` was `true`,
/// `"abc".split(null)` returned the whole string. A wrong answer where Java
/// throws is worse than either, because nothing marks it.
///
/// `equals` and `equalsIgnoreCase` are deliberately absent: they are specified
/// to answer `false` for a null argument rather than throw, and both already do.
///
/// The messages name the JDK's own parameter (`anotherString`, `prefix`,
/// `regex`) and the member it dereferenced, which is fixed text per method
/// rather than the bytecode-slot provenance javars cannot reproduce. Each is
/// quoted from a run on openjdk 21.0.12.
fn null_string_argument(method: &str, args: &[Value]) -> Result<(), Fault> {
    if !matches!(args.first(), Some(Value::Undef)) {
        return Ok(());
    }
    let detail = match method {
        "compareTo" => "Cannot read field \"value\" because \"anotherString\" is null",
        "compareToIgnoreCase" => "Cannot read field \"value\" because \"s2\" is null",
        "contains" => "Cannot invoke \"java.lang.CharSequence.toString()\" because \"s\" is null",
        "replace" => {
            "Cannot invoke \"java.lang.CharSequence.toString()\" because \"target\" is null"
        }
        "indexOf" | "lastIndexOf" => "Cannot invoke \"String.coder()\" because \"str\" is null",
        "startsWith" => "Cannot invoke \"String.length()\" because \"prefix\" is null",
        "endsWith" => "Cannot invoke \"String.length()\" because \"suffix\" is null",
        "concat" => "Cannot invoke \"String.isEmpty()\" because \"str\" is null",
        "split" | "matches" | "replaceAll" | "replaceFirst" => {
            "Cannot invoke \"String.length()\" because \"regex\" is null"
        }
        _ => return Ok(()),
    };
    Err(Fault::java("NullPointerException", detail))
}

/// Parse a format specifier's width or precision the way Java does: as an `int`.
///
/// A digit string too long for an `int` is not a large width, it is an error —
/// and Java's detail message for it is the overflowed value, which is
/// `-2147483648` for every such input (measured on openjdk 21.0.12). `class` is
/// which of the two exceptions to raise, since the digits are parsed the same
/// way for both.
fn int_format_field(digits: &str, class: &'static str) -> Result<usize, Fault> {
    match digits.parse::<i32>() {
        Ok(n) if n >= 0 => Ok(n as usize),
        _ => Err(Fault::java(class, i32::MIN.to_string())),
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
    // Java rounds the value's *shortest round-trip decimal*, not its exact
    // binary expansion. `%.2f` of 1.005 is `1.01` because the digits it rounds
    // are `1.005`, where the exact value is 1.00499999999999989…, and `%.20f` of
    // 0.1 is `0.10000000000000000000` rather than …0555. Rust's `{}` for `f64`
    // is that same shortest representation and never uses exponent notation, so
    // it is the digit string to cut — padded with zeros when the requested
    // precision runs past the digits the value actually has.
    let mut exact = format!("{}", x.abs());
    if !exact.contains('.') {
        exact.push('.');
    }
    let point = exact.find('.').unwrap_or(exact.len());
    let cut = point + if prec == 0 { 0 } else { prec + 1 };
    while exact.len() <= point + prec + 1 {
        exact.push('0');
    }
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
    // The shortest round-trip digits, the same source `fixed_half_up` rounds:
    // `%.2e` of 1.005 is `1.01e+00`, and `%.20e` of 0.1 is
    // `1.00000000000000000000e-01` rather than the exact expansion's …05551.
    // Rust's `{:e}` with no precision is exactly those digits in scientific
    // form; a precision short of them is what the HALF_UP cut below decides.
    let expanded = format!("{:e}", x);
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

/// The unsigned bit pattern `%x`/`%X`/`%o` renders, read at the width of the
/// argument's *declared* type. javars keeps every integral value in one 64-bit
/// `Value::Int`, so the width has to come from the compiler's type tag; an
/// argument it could not type falls back to the narrowest width that still
/// holds the value, which is `int` for everything an `int` can hold — the same
/// default [`boxed_class`] applies.
fn radix_bits(arg: &Value, tag: &str) -> u64 {
    let n = arg.jint();
    match tag {
        "byte" | "Byte" => n as u8 as u64,
        "short" | "Short" => n as u16 as u64,
        "long" | "Long" => n as u64,
        "int" | "Integer" => n as i32 as u32 as u64,
        _ if i32::try_from(n).is_ok() => n as i32 as u32 as u64,
        _ => n as u64,
    }
}

/// Lay one conversion out in `width` columns (char count).
///
/// `prefix` is the sign or radix marker and `suffix` the `(` flag's closing
/// parenthesis; both count toward the width, and zero padding goes *between*
/// the prefix and the body — which is what makes `% 08d` of 1 ` 0000001` and
/// `%(08d` of -1 `(000001)`. Left-justify with `-`, otherwise right-justify.
fn pad(
    prefix: &str,
    body: &str,
    suffix: &str,
    width: Option<usize>,
    left: bool,
    zero: bool,
) -> String {
    let joined = || format!("{prefix}{body}{suffix}");
    let w = match width {
        Some(w) => w,
        None => return joined(),
    };
    let len = prefix.chars().count() + body.chars().count() + suffix.chars().count();
    if len >= w {
        return joined();
    }
    let fill = w - len;
    if left {
        format!("{prefix}{body}{suffix}{}", " ".repeat(fill))
    } else if zero {
        format!("{prefix}{}{body}{suffix}", "0".repeat(fill))
    } else {
        format!("{}{prefix}{body}{suffix}", " ".repeat(fill))
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

/// [`JSTRINGIFY`] — Java's string conversion of one value, with the VM in hand.
fn b_stringify(vm: &mut VM, _argc: u8) -> Value {
    let v = vm.stack.pop().unwrap_or(Value::Undef);
    Value::str(java_str_vm(vm, &v))
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
    // two branches on one write path. Rendering runs user `toString()` bodies,
    // which may themselves print — so it happens before the lock is taken, and a
    // throwable one of them raised aborts the write rather than emitting the
    // half-built text.
    let text: String = if any_user_tostring(vm) {
        let text: String = vals.iter().map(|v| java_str_vm(vm, v)).collect();
        if PENDING.with(|p| p.borrow().is_some()) {
            return Value::Undef;
        }
        text
    } else {
        vals.iter().map(java_str).collect()
    };
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
        // (`ClassName@hex`). Reaching a user `toString()` override needs a VM to
        // run the body in, which this signature has not got — every rendering
        // surface that holds one calls [`java_str_vm`] instead.
        Value::Obj(id) => obj_default_str(*id),
        other => other.as_str_cow().into_owned(),
    }
}

/// [`java_str`] with a VM in hand, so a class instance whose class (or an
/// ancestor) declares `toString()` renders through that body, at every depth of
/// a nested collection.
///
/// Every rendering surface that holds a `&mut VM` routes here, which is what
/// keeps them agreeing: `println(o)`, `"" + o` (via [`JSTRINGIFY`]),
/// `String.valueOf(o)`, `Arrays.toString`, `list.toString()`, `String.join`,
/// and `%s`. When the program declares no override at all the gate
/// (`any_user_tostring`) is off and this is [`java_str`] exactly.
pub fn java_str_vm(vm: &mut VM, v: &Value) -> String {
    match v {
        Value::Obj(id) if any_user_tostring(vm) => obj_str_vm(vm, *id),
        other => java_str(other),
    }
}

/// The mangled suffix `toString()`'s subroutine is registered under.
const TOSTRING_SUFFIX: &str = "#toString#";

/// The mangled suffix an *overriding* `equals` is registered under. Java's
/// collections call `equals(Object)` and nothing else, so a class that declares
/// `equals(C)` has written an overload rather than an override and does not
/// appear here — which is the answer Java gives too.
const EQUALS_SUFFIX: &str = "#equals#Object";

/// The mangled suffix a user `hashCode()` is registered under. Its *presence* is
/// what [`hash_consistent`] reads; javars does not call the body.
const HASHCODE_SUFFIX: &str = "#hashCode#";

thread_local! {
    /// Whether the running chunk registers any `Class#toString#` subroutine,
    /// computed once per run. `None` until the first rendering asks.
    static USER_TOSTRING: Cell<Option<bool>> = const { Cell::new(None) };
    /// The same question for `Class#equals#Object`. `None` until the first
    /// element comparison asks.
    static USER_EQUALS: Cell<Option<bool>> = const { Cell::new(None) };
    /// (mangled suffix, class name) → the entry ip that class resolves the
    /// member to, walking supertypes; `None` for a class that inherits
    /// `java.lang.Object`'s. Memoised because rendering — or searching — a list
    /// asks once per element.
    static MEMBER_ENTRY: RefCell<HashMap<(&'static str, String), Option<usize>>> =
        RefCell::new(HashMap::new());
}

/// Whether the chunk declares a user `toString()` anywhere. False means every
/// rendering surface keeps the bytecode and the code path it always had.
fn any_user_tostring(vm: &VM) -> bool {
    cached_flag(&USER_TOSTRING, vm, TOSTRING_SUFFIX)
}

/// Whether the chunk declares an `equals(Object)` anywhere — a class of its own,
/// or the one a `record` or `enum` has synthesized. False means every collection
/// comparison keeps [`value_eq`] and the code path javars has always taken.
fn any_user_equals(vm: &VM) -> bool {
    cached_flag(&USER_EQUALS, vm, EQUALS_SUFFIX)
}

/// Whether any subroutine name carries `suffix`, answered once per run.
fn cached_flag(
    cell: &'static std::thread::LocalKey<Cell<Option<bool>>>,
    vm: &VM,
    suffix: &str,
) -> bool {
    cell.with(|c| match c.get() {
        Some(b) => b,
        None => {
            let b = vm.chunk.names.iter().any(|n| n.contains(suffix));
            c.set(Some(b));
            b
        }
    })
}

/// The entry ip of the `toString()` a runtime class resolves.
fn tostring_entry(vm: &VM, class: &str) -> Option<usize> {
    member_entry(vm, class, TOSTRING_SUFFIX)
}

/// The entry ip of the `equals(Object)` a runtime class resolves.
fn equals_entry(vm: &VM, class: &str) -> Option<usize> {
    member_entry(vm, class, EQUALS_SUFFIX)
}

/// The entry ip of the member body a runtime class resolves, following the
/// supertype chain the way the compiler's own dispatch does — a subclass that
/// declares none inherits its parent's body, and only a class that reaches
/// `java.lang.Object` without finding one has no body at all.
///
/// `suffix` is the mangled tail the member's subroutine is registered under
/// (`Class` + [`TOSTRING_SUFFIX`] / [`EQUALS_SUFFIX`]); the walk is shared
/// because `toString` and `equals` resolve by exactly the same rule.
fn member_entry(vm: &VM, class: &str, suffix: &'static str) -> Option<usize> {
    let cache_key = (suffix, class.to_string());
    if let Some(hit) = MEMBER_ENTRY.with(|t| t.borrow().get(&cache_key).copied()) {
        return hit;
    }
    let mut stack = vec![class.to_string()];
    let mut seen = std::collections::HashSet::new();
    let mut found = None;
    while let Some(cur) = stack.pop() {
        if !seen.insert(cur.clone()) {
            continue;
        }
        let key = format!("{cur}{suffix}");
        if let Some(i) = vm.chunk.names.iter().position(|n| *n == key) {
            if let Some(entry) = vm.chunk.find_sub(i as u16) {
                found = Some(entry);
                break;
            }
        }
        SUPERS.with(|s| {
            if let Some(sups) = s.borrow().get(&cur) {
                stack.extend(sups.iter().cloned());
            }
        });
    }
    MEMBER_ENTRY.with(|t| t.borrow_mut().insert(cache_key, found));
    found
}

/// What a heap object needs in order to render, read out of the heap in one
/// borrow so the borrow is dropped before any user body runs — a `toString()`
/// reads its own fields, and a nested one allocates.
enum RenderShape {
    /// A class instance: its runtime class, and the enum constant name when the
    /// synthesized field carries one.
    Instance(String, Option<String>),
    /// A `List`/`SubList`/`Set`, already in presentation order.
    Sequence(Vec<Value>),
    /// A `Map`, already in presentation order.
    Entries(Vec<(Value, Value)>),
    /// An array, a lambda, or a dangling handle — nothing to recurse into, so
    /// the pure renderer answers.
    Opaque,
}

/// [`java_str_vm`]'s heap case: snapshot the shape, drop the borrow, then either
/// run the override or recurse into the elements.
fn obj_str_vm(vm: &mut VM, id: u32) -> String {
    let shape = HEAP.with(|h| {
        let h = h.borrow();
        match h.get(id as usize) {
            Some(HostObj::Instance { class, fields }) => RenderShape::Instance(
                class.clone(),
                fields
                    .get(crate::ast::ENUM_NAME)
                    .filter(|n| !matches!(n, Value::Undef))
                    .map(|n| n.as_str_cow().into_owned()),
            ),
            Some(HostObj::List { items, .. }) => RenderShape::Sequence(items.clone()),
            Some(HostObj::Set { items, order, .. }) => RenderShape::Sequence(
                present_order(items, *order)
                    .into_iter()
                    .map(|i| items[i].clone())
                    .collect(),
            ),
            Some(HostObj::Map { entries, order, .. }) => {
                let keys: Vec<Value> = entries.iter().map(|(k, _)| k.clone()).collect();
                RenderShape::Entries(
                    present_order(&keys, *order)
                        .into_iter()
                        .map(|i| entries[i].clone())
                        .collect(),
                )
            }
            _ => RenderShape::Opaque,
        }
    });
    match shape {
        // A `SubList` owns no elements, so its window is read through the
        // parent — outside the borrow above, which `sublist_items` takes itself.
        RenderShape::Opaque if is_sublist(id as usize) => {
            let items = sublist_items(id as usize)
                .and_then(Result::ok)
                .unwrap_or_default();
            render_sequence_vm(vm, &items)
        }
        RenderShape::Opaque => obj_default_str(id),
        // `Enum.toString()` returns the constant's name unless the enum declares
        // its own override, and the override wins — the same precedence Java's
        // virtual dispatch gives it.
        RenderShape::Instance(class, enum_name) => match tostring_entry(vm, &class) {
            Some(entry) => run_tostring(vm, entry, id),
            None => enum_name.unwrap_or_else(|| obj_default_str(id)),
        },
        RenderShape::Sequence(items) => render_sequence_vm(vm, &items),
        RenderShape::Entries(entries) => {
            let body: Vec<String> = entries
                .iter()
                .map(|(k, v)| format!("{}={}", java_str_vm(vm, k), java_str_vm(vm, v)))
                .collect();
            format!("{{{}}}", body.join(", "))
        }
    }
}

/// `[a, b, c]` with each element rendered through its own `toString()`.
fn render_sequence_vm(vm: &mut VM, items: &[Value]) -> String {
    let body: Vec<String> = items.iter().map(|e| java_str_vm(vm, e)).collect();
    format!("[{}]", body.join(", "))
}

/// Run a user `toString()` body on `id` and return what it answered.
///
/// A throwable already in flight stops the call: the enclosing frame is
/// unwinding, and rendering must not start a body whose side effects would run
/// a second time. One raised *by* the body leaves `PENDING` set for the calling
/// builtin to surface, and the half-built text is discarded with it.
fn run_tostring(vm: &mut VM, entry: usize, id: u32) -> String {
    if PENDING.with(|p| p.borrow().is_some()) {
        return String::new();
    }
    let stack_base = vm.stack.len();
    vm.stack.push(Value::Obj(id));
    let out = run_sub(vm, entry, stack_base);
    // Java's string conversion of a `toString()` that answered `null` is the
    // four characters "null", not an empty string.
    java_str(&out)
}

/// Java's default `toString` for a heap object: `ClassName@<identity-hash>` for
/// an instance, `[@<hash>` for an array. The class name is the qualified one
/// `getClass().getName()` reports (`java.lang.Object`, not `Object`), and the
/// hash is the handle (deterministic within a run) rather than a JVM identity
/// hash.
fn obj_default_str(id: u32) -> String {
    // A wrapper renders as the primitive it holds, and two of the eight need
    // their class to do it: a `char` rides `Value::Int`, so `Character` has to
    // turn the code point back into the character, and `Float.toString` is the
    // 32-bit rendering (`0.1f` prints `0.1`, not the `double` widening's
    // `0.10000000149011612`). Computed before the heap borrow below so the
    // formatting helpers are free to touch the heap themselves.
    let handle = Value::Obj(id);
    if let (Some(class), Some(v)) = (box_class(&handle), unboxed(&handle)) {
        return match class {
            "Character" => char::from_u32(as_i64(&v) as u32)
                .map(String::from)
                .unwrap_or_default(),
            "Float" => java_str(&float_to_string(&v)),
            _ => java_str(&v),
        };
    }
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
                _ => format!("{}@{id:x}", qualified_or_binary(class)),
            },
            Some(HostObj::Array(_)) => format!("[@{id:x}"),
            // `StringBuilder.toString()` IS its contents, so every rendering
            // surface — `println(sb)`, `"" + sb`, `%s`, a list element — shows
            // the text rather than a handle.
            Some(HostObj::Builder { s, .. }) => s.clone(),
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
            Some(HostObj::Map { entries, order, .. }) => render_map(entries, *order),
            // Java renders a lambda as `Class$$Lambda/0x…@<identity hash>`,
            // which is not reproducible (and not stable across JVM runs), so
            // javars prints a fixed marker instead. See `BUGS.md`.
            Some(HostObj::Closure { .. }) => format!("<lambda>@{id:x}"),
            Some(HostObj::Boxed) => unreachable!("a box is answered above"),
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
    // A boxed wrapper is a number too: every arithmetic and relational operator
    // Java allows on one unboxes it first (JLS 5.1.8), so the hook must reach
    // `java_numeric` for it rather than falling into the `String` arms and
    // concatenating.
    matches!(v, Value::Int(_) | Value::Float(_))
        || unboxed(v).is_some_and(|inner| matches!(inner, Value::Int(_) | Value::Float(_)))
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
/// `java_numeric` now answers all three cases and is reached on operand
/// shape, so no numeric pair can fall into a `String` arm again.
pub fn numeric_hook(op: NumOp, a: &Value, b: &Value) -> Result<Value, String> {
    // `Neg` is unary — fusevm passes `Undef` as the second operand, so it can
    // never satisfy the two-number gate below and is answered first.
    if op == NumOp::Neg && is_java_number(a) {
        return java_numeric(op, &deboxed(a), &Value::Int(0));
    }
    // `==`/`!=` between two heap references is reference identity — before the
    // numeric gate below, because two boxed wrappers are numbers as well as
    // references and Java compares them as references. A mixed box/primitive
    // pair is *not* caught here and falls through to the numeric path, which is
    // Java's rule too: `anInteger == anInt` unboxes (JLS 15.21.1).
    if matches!((a, b), (Value::Obj(_), Value::Obj(_))) {
        match op {
            NumOp::Eq => return Ok(Value::bool(ref_eq(a, b))),
            NumOp::Ne => return Ok(Value::bool(!ref_eq(a, b))),
            _ => {}
        }
    }
    if is_java_number(a) && is_java_number(b) {
        return java_numeric(op, &deboxed(a), &deboxed(b));
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

/// Java's 64-bit integral `/`, divided in `i64` rather than in `f64`. See
/// [`JIDIV`] for why the native float pair cannot serve a `long`.
///
/// `wrapping_div`, not `/`: Rust's `/` panics on `i64::MIN / -1`, and that
/// overflow check runs in release too. Java defines the case — JLS 15.17.2, "if
/// the dividend is the negative integer of largest possible magnitude for its
/// type, and the divisor is -1, then integer overflow occurs and the result is
/// equal to the dividend" — so the answer is `i64::MIN`, which is exactly what
/// `wrapping_div` gives. A zero divisor cannot reach here: the compiler emits
/// [`Compiler::emit_zero_divisor_check`] ahead of the call, which raises
/// `ArithmeticException` first. Guarding it anyway keeps a panic out of the
/// builtin regardless of how it is reached.
fn b_idiv(vm: &mut VM, _argc: u8) -> Value {
    let b = as_i64(&vm.stack.pop().unwrap_or(Value::Undef));
    let a = as_i64(&vm.stack.pop().unwrap_or(Value::Undef));
    Value::Int(if b == 0 { 0 } else { a.wrapping_div(b) })
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
    let a = args.first().map(JavaNumeric::jfloat).unwrap_or(0.0) as f32;
    let b = args.get(1).map(JavaNumeric::jfloat).unwrap_or(0.0) as f32;
    let r = match args.get(2).map(JavaNumeric::jint).unwrap_or(f32_op::ADD) {
        f32_op::SUB => a - b,
        f32_op::MUL => a * b,
        f32_op::DIV => a / b,
        f32_op::REM => a % b,
        _ => a + b,
    };
    Value::float(r as f64)
}

/// [`JF32_ROUND`] — `Math.round(float)`, answering an `int`.
fn b_f32_round(vm: &mut VM, _argc: u8) -> Value {
    let v = vm.stack.pop().unwrap_or(Value::Undef);
    Value::Int(round_float(v.jfloat() as f32).into())
}

/// [`JF32_STR`] — `Float.toString`, or element-wise over a `float[]`.
fn b_f32_str(vm: &mut VM, _argc: u8) -> Value {
    let v = vm.stack.pop().unwrap_or(Value::Undef);
    // A boxed `Float` is rendered from the primitive it wraps; without this the
    // `Value::Obj` arm below would see a handle that is not an array and hand
    // the box straight back, which renders through `Double`'s rules instead.
    let v = unboxed(&v).unwrap_or(v);
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
/// This is the *target* half of the same question `value_class` answers for a
/// value, and it lives here so the two halves read one list. The compiler asks
/// it before emitting a [`JCHECKCAST`] at all: a name that is neither a
/// declared class nor one of these is a type javars does not model — a type
/// *variable* after erasure, an array type, a JDK class it has never heard of —
/// and a check it cannot decide must not invent a failure.
///
/// `Object` is deliberately absent: every value satisfies it, so a check would
/// only cost ops. Every other name here appears in `jdk_supers`, is produced
/// by `value_class`, or is one of the wrapper siblings `cast_allowed`
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
        "String"
        | "Integer"
        | "Long"
        | "Short"
        | "Byte"
        | "Double"
        | "Float"
        | "Boolean"
        | "Character"
        | "Number"
        | "CharSequence"
        | "Comparable"
        | "Cloneable"
        | "Iterable"
        | "Enum"
        | "Record"
        | "Object"
        | "StringBuilder"
        | "StringBuffer"
        | "AbstractStringBuilder"
        | "Appendable" => {
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
        // Not a modeled JDK type, so it is a user class: Java names a nested one
        // `Outer$Nested`, which is what [`qualified_or_binary`] recovers.
        other => qualified_or_binary(other),
    }
}

/// The 64-bit value a narrowing integral cast starts from: a floating operand
/// truncates toward zero first, and a `char` (a one-character string) yields its
/// code point.
fn cast_to_i64(v: &Value) -> i64 {
    match unboxed(v).as_ref().unwrap_or(v) {
        Value::Float(f) => *f as i64,
        other => as_i64(other),
    }
}

/// Coerce a value to `i64`. A one-character string is a `char`, and its code
/// point is its numeric value — `(int) 'a'` is 97.
fn as_i64(v: &Value) -> i64 {
    // A wrapper is its primitive everywhere a number is wanted; the box exists
    // for `==`, `equals`, `hashCode` and `getClass` and nothing else.
    if let Some(inner) = unboxed(v) {
        return as_i64(&inner);
    }
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
    if let Some(inner) = unboxed(v) {
        return as_f64(&inner);
    }
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
