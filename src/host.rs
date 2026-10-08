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

/// Builtin id for Java `/` whose operand types are not statically known — an
/// untyped lambda parameter (`IntStream.range(0, 3).map(i -> i / 2)`), an erased
/// `Supplier<Integer>.get()`. The runtime value decides what the static type
/// could not: two integral values divide integrally (and a zero divisor is
/// `ArithmeticException`), anything floating divides as IEEE-754 the way
/// [`JDIV`] does. Sending these through `JDIV` answered `0.5` for `1 / 2`.
pub const JDIV_DYN: u16 = 751;

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

/// `new String(x)` — a `String` with an **identity of its own**. Stack
/// `[text]`; `argc == 1`.
///
/// Java's `String` is a reference type, so `new String("ab") == "ab"` is
/// `false`: the constructor is specified to produce a *fresh* object, which is
/// the only reason the expression is ever written. A `String`'s identity here
/// is its `Arc`, so this re-allocates the text — every other path that
/// *produces* a string already allocates, and this is the one path that would
/// otherwise pass an existing object through.
pub const JNEW_STRING: u16 = 749;

/// Append elements to an array already on the stack. Stack
/// `[array, e1, …, en]`; `argc == n + 1`, and the array is answered back.
///
/// The VM passes an argument count in a `u8`, so one [`JARRAY_LIT`] can carry
/// at most 255 elements — and `{1.0, 2.0, …}` with more than that used to
/// truncate the count silently, producing an array of `len % 256` elements
/// holding the *last* of them. A 4000-element lookup table became a
/// 160-element one and every index into it was wrong or out of bounds. The
/// literal is built in chunks instead: the first 255 elements make the array
/// and each further chunk is appended through here.
pub const JARRAY_EXTEND: u16 = 750;

/// Unbox a wrapper back to its primitive; the identity function on a value that
/// is not boxed. Stack `[value]`; `argc == 1`.
///
/// Emitted wherever Java performs an *unboxing conversion* — assigning an
/// `Integer` expression to an `int`, passing one to a primitive parameter — so
/// that a later `==` between two such variables compares numbers rather than
/// the handles they were copied from.
pub const JUNBOX: u16 = 748;

/// [`JUNBOX`] for a source whose static type is a wrapper: Java's unboxing
/// conversion calls `intValue()` (or its sibling) on the reference, so a `null`
/// raises `NullPointerException` instead of flowing into the primitive slot.
/// Stack `[value, class]` (`class` on top, an index into [`BOX_CLASSES`], or
/// `BOX_CLASSES.len()` for `Boolean`); `argc == 2`.
pub const JUNBOX_NONNULL: u16 = 752;

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

/// The argument snapshots a call whose arguments are all scalars borrows
/// instead of allocating: every entry is `None`, which is what both readers
/// answer for anything that is not a `Value::Obj`. Four entries covers every
/// collection method javars models; `coll_method` slices to the call's arity
/// and falls back to building a `Vec` for anything longer.
static NO_ARG_SEQS: [Option<Vec<Value>>; 4] = [None, None, None, None];
static NO_ARG_ENTRIES: [Option<Vec<(Value, Value)>>; 4] = [None, None, None, None];

/// The two snapshot slices `coll_method` hands every collection callee: one
/// entry per argument, `Some` only where that argument is a `Value::Obj` the
/// reader could resolve. Named because the pair appears in signatures.
type ArgSeqs<'a> = &'a [Option<Vec<Value>>];
type ArgEntries<'a> = &'a [Option<Vec<(Value, Value)>>];

/// One object on the host-owned Java heap. `Value::Obj(id)` indexes [`HEAP`].
enum HostObj {
    /// `System.in`, a `Scanner`, or a `java.io` reader (see [`crate::jio`]).
    Reader(crate::jio::Reader),
    /// A `java.util.StringTokenizer`.
    Tokenizer(crate::jio::Tokenizer),
    /// A `java.util.Random` (see [`crate::jrandom`]).
    Random(crate::jrandom::Random),
    /// An `Int`/`Long`/`DoubleSummaryStatistics`.
    Stats(SummaryStats),
    /// A `java.util.BitSet` (see [`crate::jbitset`]).
    Bits(crate::jbitset::BitSet),
    /// An `AtomicInteger`, `AtomicLong` or `AtomicBoolean`.
    Atomic { kind: AtomicKind, value: Value },
    /// A `java.util.regex.Pattern`: the source as written, its flags, and the
    /// source the engine compiles (see [`regex_source`]).
    RegexPattern {
        shown: String,
        flags: i64,
        source: String,
    },
    /// A `java.util.regex.Matcher`.
    RegexMatcher(Box<RegexMatcher>),
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
        /// `Some` when this is the `values()` view a map handed out rather than
        /// a list of its own; see [`SetView`], which draws the same
        /// distinction for the map's other two views. A `values()` result is
        /// the odd one of the three: Java's is a `Collection` and **not** a
        /// `List`, so it is not interchangeable with an `ArrayList` even
        /// though every method a program calls on it is the same.
        view: Option<ViewOf>,
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
        /// The same distinction [`HostObj::List`] draws: `Map.of` is an
        /// immutable map, not a `HashMap`. Without it a `Map.of` value was
        /// indistinguishable from `new HashMap<>()`, so it answered
        /// `instanceof HashMap` `true` (Java: `false`) and accepted
        /// `put`/`remove`/`clear` silently (Java:
        /// `UnsupportedOperationException`).
        fixed: Fixity,
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
        /// Whether this set is one of its own or a view a `Map` handed out;
        /// see [`SetView`].
        view: SetView,
        /// Element -> position accelerator; see [`KeyIndex`].
        index: KeyIndex,
    },
    /// A `java.util.Map.Entry` — one key/value pair, as `entrySet()` hands it
    /// out and as `Map.entry(k, v)` builds one from nothing.
    ///
    /// `owner` is the map the pair was read out of. It is what makes
    /// `setValue` write *through* to that map, which is the one thing an entry
    /// is for that a two-element snapshot could not do: Java specifies
    /// `entry.setValue(v)` as a write to the backing map, and a `for (var e :
    /// m.entrySet()) e.setValue(…)` loop is the ordinary way to rewrite every
    /// value in place. A `Map.entry(k, v)` pair owns no map, so it answers
    /// `UnsupportedOperationException` instead — exactly as the JDK's
    /// `KeyValueHolder` does.
    ///
    /// `owner` also decides the entry's *class*: the JDK gives each map
    /// implementation its own private node type (`HashMap$Node`,
    /// `LinkedHashMap$Entry`, `TreeMap$Entry`) and an ownerless pair is a
    /// `java.util.KeyValueHolder`. All four are `Map.Entry` and none of them is
    /// `Serializable` — measured, not assumed.
    ///
    /// The pair is stored *outside* `HEAP`, in [`PAIRS`], for the reason
    /// [`HostObj::Boxed`] gives for a box: an entry sitting in a `Set` is
    /// compared and hashed while that set is borrowed, so reading its key back
    /// out of `HEAP` would be a re-entrant borrow and panic. The slot is still
    /// allocated here so an entry owns a handle no other object can be given.
    Entry,
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
    /// A `java.util.Iterator` over a `List` or a `Set`.
    ///
    /// It reads its source *live* rather than snapshotting it, which is what
    /// makes `remove()` write through and — for a `List`, the one collection
    /// that carries a `mods` counter — makes a structural change underneath the
    /// iterator the `ConcurrentModificationException` Java raises rather than a
    /// silent walk of stale elements.
    Iterator {
        /// The collection being walked.
        source: u32,
        /// The index `next()` will return.
        pos: usize,
        /// The index `next()` last returned, which `remove()` deletes. `None`
        /// before the first `next()` and after a `remove()`, both of which make
        /// `remove()` an `IllegalStateException` in Java.
        last: Option<usize>,
        /// The source `List`'s `mods` when this iterator was created, bumped by
        /// its own `remove()`. Always 0 for a `Set`, which has no counter — see
        /// the note in `iterator_method`.
        exp_mods: u64,
        /// `true` for the `ListIterator` `List.listIterator` hands out, which
        /// adds the backward walk (`hasPrevious`/`previous`), the two index
        /// queries, and the `set`/`add` writes. A plain `iterator()` answers
        /// none of them, exactly as `java.util.Iterator` declares none.
        bidi: bool,
        /// `true` for a `descendingIterator()`: `pos` and `last` then count
        /// from the *end* of the source, so the cursor arithmetic (and
        /// `remove()` leaving the cursor on the gap) is the forward walk's,
        /// read through the mirror.
        desc: bool,
    },
    /// A `java.util.PriorityQueue`: the binary heap array itself, laid out by
    /// the JDK's own `siftUp`/`siftDown`, so iteration and `toString` show the
    /// same (heap, not sorted) order the reference does.
    PQueue {
        items: Vec<Value>,
        /// The ordering closure — the one the program passed, or the
        /// `(a, b) -> a.compareTo(b)` the compiler supplies for natural order.
        cmp: Value,
        /// `true` when `cmp` is that supplied natural order, which is what
        /// makes `comparator()` answer `null`.
        natural: bool,
        /// `AbstractList`-style `modCount`: bumped by every structural change,
        /// read by [`HostObj::PQIter`] to report a concurrent modification.
        mods: u64,
    },
    /// `PriorityQueue.Itr`. Walking the heap array is plain, but `remove()` is
    /// not: deleting slot `last` may lift the heap's last element into an
    /// already-visited slot, and the JDK then remembers that element in
    /// `forgetMeNot` and hands it out after the array walk ends. This carries
    /// the same state so the sequence of `next()` results is the reference's.
    PQIter {
        source: u32,
        cursor: usize,
        last: Option<usize>,
        forget: std::collections::VecDeque<Value>,
        last_elt: Option<Value>,
        exp_mods: u64,
    },
    /// A `java.util.Optional` — present with a value, or empty.
    ///
    /// A *value* in Java (`Optional.of("x").equals(Optional.of("x"))` is
    /// `true`) but still a reference: two `Optional.of("x")` are distinct
    /// objects, so `==` between them is `false`. Both fall out of the heap
    /// object — `equals` compares the contents, `==` the handles.
    ///
    /// `class` is `Optional` or one of the three primitive specializations
    /// (`OptionalInt`, `OptionalLong`, `OptionalDouble`), which differ in their
    /// rendering (`OptionalDouble[4.0]`) and in the name of their accessor
    /// (`getAsDouble` rather than `get`).
    Optional {
        class: &'static str,
        value: Option<Value>,
    },
    /// A `java.util.stream.Stream` — a source and the pipeline built over it.
    ///
    /// Nothing is evaluated until a terminal operation runs, which is what Java
    /// specifies and what a program can *see*: `l.stream().peek(p).limit(2)`
    /// calls `p` twice, not once per source element.
    Stream {
        source: Source,
        stages: Vec<Stage>,
        kind: StreamKind,
    },
    /// A `java.util.stream.Collector`, as the recipe `collect` will run.
    Collector {
        kind: &'static str,
        args: Vec<Value>,
    },
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

/// Where a stream's elements come from.
///
/// A finite source is a `Vec`. `Stream.iterate(seed, f)` and
/// `Stream.generate(s)` are unbounded, so they are held as the recipe and
/// pulled one element at a time by the terminal that drives the pipeline — a
/// `limit` or a short-circuiting terminal stops the pulling, exactly as it
/// stops Java's spliterator, and `f`/`s` run only as often as an element is
/// actually demanded.
#[derive(Clone)]
enum Source {
    Items(Vec<Value>),
    Iterate {
        seed: Value,
        f: Value,
    },
    Generate(Value),
    /// `Stream.concat(a, b)`: the two stream handles, driven in order.
    Concat(Box<(Value, Value)>),
}

impl Source {
    fn cursor(self) -> SourceCursor {
        match self {
            Source::Items(v) => SourceCursor::Items(v.into_iter()),
            Source::Iterate { seed, f } => SourceCursor::Iterate {
                prev: None,
                seed: Some(seed),
                f,
            },
            Source::Generate(s) => SourceCursor::Generate(s),
            Source::Concat(_) => unreachable!("`stream_drive` expands a concatenation"),
        }
    }
}

/// A [`Source`] being consumed.
enum SourceCursor {
    Items(std::vec::IntoIter<Value>),
    /// `Stream.iterate`'s spliterator: the seed first, then `f` applied to the
    /// previous element — computed at the pull that needs it, never ahead.
    Iterate {
        prev: Option<Value>,
        seed: Option<Value>,
        f: Value,
    },
    Generate(Value),
}

impl SourceCursor {
    /// The next element, or `None` when a finite source is exhausted or a user
    /// closure raised.
    fn pull(&mut self, vm: &mut VM) -> Option<Value> {
        let v = match self {
            SourceCursor::Items(it) => return it.next(),
            SourceCursor::Iterate { prev, seed, f } => {
                let v = match seed.take() {
                    Some(s) => s,
                    None => invoke_closure(vm, f, std::slice::from_ref(prev.as_ref()?)),
                };
                *prev = Some(v.clone());
                v
            }
            SourceCursor::Generate(s) => invoke_closure(vm, s, &[]),
        };
        if pending() {
            return None;
        }
        Some(v)
    }
}

/// One stage of a stream pipeline, in the order the program wrote them.
///
/// `Distinct` and `Sorted` are *stateful barriers*: neither can answer for an
/// element without having seen every element before it, so the pipeline is
/// evaluated in segments split at them — which is what Java's own
/// implementation does, and why `peek` before a `sorted` runs for every element
/// while `peek` before a `limit` does not.
#[derive(Clone)]
enum Stage {
    /// `filter(p)` — keep the elements `p` accepts.
    Filter(Value),
    /// `map(f)` / `mapToInt(f)` / `mapToObj(f)` — one element in, one out.
    Map(Value),
    /// `flatMap(f)` — one element in, the elements of the stream `f` answers out.
    FlatMap(Value),
    /// `peek(f)` — run `f` for its effect and pass the element through.
    Peek(Value),
    /// `limit(n)` — pass at most `n` elements, then end the pipeline.
    Limit(i64),
    /// `skip(n)` — drop the first `n`.
    Skip(i64),
    /// `takeWhile(p)` — pass elements while `p` accepts them; the first one
    /// it rejects ends the pipeline.
    TakeWhile(Value),
    /// `dropWhile(p)` — drop elements while `p` accepts them, then pass
    /// everything; the stage's counter records that the prefix is over.
    DropWhile(Value),
    /// `distinct()` — a barrier.
    Distinct,
    /// `sorted()` / `sorted(cmp)` — a barrier.
    Sorted(Option<Value>),
    /// The element becomes a `double` — appended after every stage whose result
    /// a `DoubleStream` holds (`mapToDouble`, `asDoubleStream`, `map` on a
    /// `DoubleStream`). Java converts there, and a later untyped `x / 2` or a
    /// `boxed()` rendering reads the value's own kind: `DoubleStream.of(4, 3)`
    /// holding the integers 4 and 3 printed `[4, 3]` and divided integrally.
    Widen,
}

/// Which of the four stream shapes a pipeline is, which decides what its
/// terminals answer: `IntStream.max()` is an `OptionalInt` and
/// `DoubleStream.max()` an `OptionalDouble`, where `Stream.max(cmp)` is a plain
/// `Optional`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StreamKind {
    /// `Stream<T>`.
    Ref,
    /// `IntStream`.
    Int,
    /// `LongStream`.
    Long,
    /// `DoubleStream`.
    Double,
}

impl StreamKind {
    /// The `Optional` class this shape's `min`/`max`/`findFirst` answers.
    fn optional_class(self) -> &'static str {
        match self {
            StreamKind::Ref => "Optional",
            StreamKind::Int => "OptionalInt",
            StreamKind::Long => "OptionalLong",
            StreamKind::Double => "OptionalDouble",
        }
    }
}

/// An `IntSummaryStatistics`, `LongSummaryStatistics` or
/// `DoubleSummaryStatistics`: the JDK's own fields, accumulated by its own
/// `accept` and `combine` (`java.util.*SummaryStatistics`, openjdk 27).
#[derive(Clone)]
struct SummaryStats {
    /// `Int`, `Long` or `Double` — which of the three classes this is.
    kind: StreamKind,
    count: i64,
    /// The integral classes' `long` sum and extremes. An `int` one starts its
    /// extremes at `Integer.MAX_VALUE`/`MIN_VALUE`, a `long` one at `Long`'s.
    sum: i64,
    min: i64,
    max: i64,
    /// The `double` class's Kahan pair, its naive sum (which answers an
    /// infinite total the compensation turned into NaN), and its extremes,
    /// which start at the two infinities.
    dsum: f64,
    comp: f64,
    simple: f64,
    dmin: f64,
    dmax: f64,
}

impl SummaryStats {
    fn new(kind: StreamKind) -> Self {
        let (min, max) = match kind {
            StreamKind::Int => (i64::from(i32::MAX), i64::from(i32::MIN)),
            _ => (i64::MAX, i64::MIN),
        };
        SummaryStats {
            kind,
            count: 0,
            sum: 0,
            min,
            max,
            dsum: 0.0,
            comp: 0.0,
            simple: 0.0,
            dmin: f64::INFINITY,
            dmax: f64::NEG_INFINITY,
        }
    }

    /// `sumWithCompensation`.
    fn add_compensated(&mut self, value: f64) {
        let tmp = value - self.comp;
        let velvel = self.dsum + tmp;
        self.comp = (velvel - self.dsum) - tmp;
        self.dsum = velvel;
    }

    /// `accept(value)`.
    fn accept(&mut self, v: &Value) {
        self.count += 1;
        if self.kind == StreamKind::Double {
            let d = deboxed(v).jfloat();
            self.simple += d;
            self.add_compensated(d);
            self.dmin = min_double(self.dmin, d);
            self.dmax = max_double(self.dmax, d);
        } else {
            let n = deboxed(v).jint();
            self.sum = self.sum.wrapping_add(n);
            self.min = self.min.min(n);
            self.max = self.max.max(n);
        }
    }

    /// `combine(other)`.
    fn combine(&mut self, o: &SummaryStats) {
        self.count += o.count;
        if self.kind == StreamKind::Double {
            self.simple += o.simple;
            self.add_compensated(o.dsum);
            self.add_compensated(-o.comp);
            self.dmin = min_double(self.dmin, o.dmin);
            self.dmax = max_double(self.dmax, o.dmax);
        } else {
            self.sum = self.sum.wrapping_add(o.sum);
            self.min = self.min.min(o.min);
            self.max = self.max.max(o.max);
        }
    }

    /// `getSum()`: the compensated total for `double`, unless that is a NaN
    /// the naive sum shows to be an infinity.
    fn double_sum(&self) -> f64 {
        let tmp = self.dsum - self.comp;
        if tmp.is_nan() && self.simple.is_infinite() {
            self.simple
        } else {
            tmp
        }
    }

    fn get_sum(&self) -> Value {
        match self.kind {
            StreamKind::Double => Value::float(self.double_sum()),
            _ => Value::Int(self.sum),
        }
    }

    fn get_min(&self) -> Value {
        match self.kind {
            StreamKind::Double => Value::float(self.dmin),
            _ => Value::Int(self.min),
        }
    }

    fn get_max(&self) -> Value {
        match self.kind {
            StreamKind::Double => Value::float(self.dmax),
            _ => Value::Int(self.max),
        }
    }

    /// `getAverage()`: `0.0` for no values.
    fn average(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else if self.kind == StreamKind::Double {
            self.double_sum() / self.count as f64
        } else {
            self.sum as f64 / self.count as f64
        }
    }

    fn class_name(&self) -> &'static str {
        match self.kind {
            StreamKind::Double => "DoubleSummaryStatistics",
            StreamKind::Long => "LongSummaryStatistics",
            _ => "IntSummaryStatistics",
        }
    }

    /// `toString()`, through the same `String.format` the JDK's calls.
    fn render(&self) -> String {
        let fmt = if self.kind == StreamKind::Double {
            "%s{count=%d, sum=%f, min=%f, average=%f, max=%f}"
        } else {
            "%s{count=%d, sum=%d, min=%d, average=%f, max=%d}"
        };
        let args = [
            Value::str(self.class_name().to_string()),
            Value::Int(self.count),
            self.get_sum(),
            self.get_min(),
            Value::float(self.average()),
            self.get_max(),
        ];
        match java_format(fmt, &args, &[], None) {
            Ok(v) => v.as_str_cow().into_owned(),
            Err(_) => String::new(),
        }
    }

    /// The statistics of `items`, accepted in order.
    fn of(kind: StreamKind, items: &[Value]) -> Self {
        let mut s = SummaryStats::new(kind);
        for v in items {
            s.accept(v);
        }
        s
    }
}

/// Which `java.util.concurrent.atomic` class an [`HostObj::Atomic`] is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AtomicKind {
    Int,
    Long,
    Bool,
}

/// The value an atomic holds, normalized to its width: an `AtomicInteger`
/// wraps at 32 bits as `int` arithmetic does.
fn atomic_norm(kind: AtomicKind, v: &Value) -> Value {
    match kind {
        AtomicKind::Int => Value::Int(i64::from(v.jint() as i32)),
        AtomicKind::Long => Value::Int(v.jint()),
        AtomicKind::Bool => Value::bool(matches!(deboxed(v), Value::Bool(true))),
    }
}

/// Read or replace an atomic's value; `None` for any other value.
fn atomic_cell(v: &Value, put: Option<Value>) -> Option<(AtomicKind, Value)> {
    let Value::Obj(id) = v else {
        return None;
    };
    HEAP.with(|h| match h.borrow_mut().get_mut(*id as usize) {
        Some(HostObj::Atomic { kind, value }) => {
            let old = value.clone();
            if let Some(new) = put {
                *value = atomic_norm(*kind, &new);
            }
            Some((*kind, old))
        }
        _ => None,
    })
}

/// A method call on an `AtomicInteger`/`AtomicLong`/`AtomicBoolean`; `None`
/// for any other receiver. javars runs one thread, so every operation is the
/// plain read-modify-write its name describes, with `int` wrap for an
/// `AtomicInteger`; the update functions run on the VM.
fn atomic_method(vm: &mut VM, recv: &Value, method: &str, args: &[Value]) -> Option<Value> {
    let (kind, cur) = atomic_cell(recv, None)?;
    let store = |v: Value| {
        atomic_cell(recv, Some(v));
    };
    let norm = |v: Value| atomic_norm(kind, &v);
    let add = |d: i64| norm(Value::Int(cur.jint().wrapping_add(d)));
    Some(match (method, args) {
        ("get" | "getPlain" | "getAcquire" | "getOpaque" | "intValue" | "longValue", []) => cur,
        ("doubleValue", []) => Value::float(cur.jint() as f64),
        ("set" | "lazySet" | "setPlain" | "setRelease" | "setOpaque", [v]) => {
            store(v.clone());
            Value::Undef
        }
        ("getAndSet", [v]) => {
            store(v.clone());
            cur
        }
        ("incrementAndGet", []) => {
            let n = add(1);
            store(n.clone());
            n
        }
        ("decrementAndGet", []) => {
            let n = add(-1);
            store(n.clone());
            n
        }
        ("getAndIncrement", []) => {
            store(add(1));
            cur
        }
        ("getAndDecrement", []) => {
            store(add(-1));
            cur
        }
        ("addAndGet", [d]) => {
            let n = add(d.jint());
            store(n.clone());
            n
        }
        ("getAndAdd", [d]) => {
            store(add(d.jint()));
            cur
        }
        ("compareAndSet" | "weakCompareAndSet" | "weakCompareAndSetPlain", [e, n]) => {
            let hit = value_eq(&cur, &norm(e.clone()));
            if hit {
                store(n.clone());
            }
            Value::bool(hit)
        }
        ("updateAndGet" | "getAndUpdate", [f]) => {
            let n = norm(invoke_closure(vm, f, std::slice::from_ref(&cur)));
            if pending() {
                return Some(Value::Undef);
            }
            store(n.clone());
            if method == "updateAndGet" {
                n
            } else {
                cur
            }
        }
        ("accumulateAndGet" | "getAndAccumulate", [x, f]) => {
            let n = norm(invoke_closure(vm, f, &[cur.clone(), x.clone()]));
            if pending() {
                return Some(Value::Undef);
            }
            store(n.clone());
            if method == "accumulateAndGet" {
                n
            } else {
                cur
            }
        }
        ("toString", []) => Value::str(java_str(&cur)),
        _ => raise(
            vm,
            Fault::internal(format!(
                "javars: unsupported atomic method `{method}` with {} argument(s)",
                args.len()
            )),
        ),
    })
}

/// Run `f` on the `BitSet` a handle names; `None` for any other value.
fn with_bits<R>(v: &Value, f: impl FnOnce(&mut crate::jbitset::BitSet) -> R) -> Option<R> {
    let Value::Obj(id) = v else {
        return None;
    };
    HEAP.with(|h| match h.borrow_mut().get_mut(*id as usize) {
        Some(HostObj::Bits(b)) => Some(f(b)),
        _ => None,
    })
}

/// A method call on a `java.util.BitSet` receiver; `None` for any other.
///
/// The methods that read a second set copy it out first, and the ones that
/// answer a new object (`get(from, to)`, `clone`, `stream`) allocate it after
/// the receiver's borrow is released.
fn bitset_method(recv: &Value, method: &str, args: &[Value]) -> Option<Result<Value, Fault>> {
    use crate::jbitset::BitSet;
    let refuse = |(class, msg): crate::jbitset::Refusal| Fault::java(class, msg);
    let other = match (method, args) {
        ("and" | "or" | "xor" | "andNot" | "intersects" | "equals", [o]) => {
            Some(with_bits(o, |b| b.clone()))
        }
        _ => None,
    };
    let me = with_bits(recv, |b| b.clone())?;
    let alloc = |b: BitSet| Value::Obj(heap_alloc(HostObj::Bits(b)));
    match (method, args) {
        ("get", [f, t]) => return Some(me.slice(f.jint(), t.jint()).map(alloc).map_err(refuse)),
        ("clone", []) => return Some(Ok(alloc(me.cloned()))),
        ("stream", []) => {
            let items = me.ones().into_iter().map(Value::Int).collect();
            return Some(Ok(stream_of(items, StreamKind::Int)));
        }
        _ => {}
    }
    let npe = || Fault::java("NullPointerException", String::new());
    with_bits(recv, |b| match (method, args) {
        ("get", [i]) => b.get(i.jint()).map(Value::bool).map_err(refuse),
        ("set", [i]) => b.set(i.jint(), true).map(|()| Value::Undef).map_err(refuse),
        ("set", [i, Value::Bool(on)]) => {
            b.set(i.jint(), *on).map(|()| Value::Undef).map_err(refuse)
        }
        ("set", [f, t]) => b
            .range(f.jint(), t.jint(), |_| true)
            .map(|()| Value::Undef)
            .map_err(refuse),
        ("set", [f, t, on]) => {
            let on = matches!(on, Value::Bool(true));
            b.range(f.jint(), t.jint(), move |_| on)
                .map(|()| Value::Undef)
                .map_err(refuse)
        }
        ("clear", [i]) => b
            .set(i.jint(), false)
            .map(|()| Value::Undef)
            .map_err(refuse),
        ("clear", [f, t]) => b
            .range(f.jint(), t.jint(), |_| false)
            .map(|()| Value::Undef)
            .map_err(refuse),
        ("clear", []) => {
            b.clear_all();
            Ok(Value::Undef)
        }
        ("flip", [i]) => b.flip(i.jint()).map(|()| Value::Undef).map_err(refuse),
        ("flip", [f, t]) => b
            .range(f.jint(), t.jint(), |v| !v)
            .map(|()| Value::Undef)
            .map_err(refuse),
        ("cardinality", []) => Ok(Value::Int(b.cardinality())),
        ("length", []) => Ok(Value::Int(b.length())),
        ("size", []) => Ok(Value::Int(b.size())),
        ("isEmpty", []) => Ok(Value::bool(b.is_empty())),
        ("nextSetBit", [i]) => b.next_set(i.jint()).map(Value::Int).map_err(refuse),
        ("nextClearBit", [i]) => b.next_clear(i.jint()).map(Value::Int).map_err(refuse),
        ("previousSetBit", [i]) => b.previous(i.jint(), true).map(Value::Int).map_err(refuse),
        ("previousClearBit", [i]) => b.previous(i.jint(), false).map(Value::Int).map_err(refuse),
        ("and" | "or" | "xor" | "andNot", [_]) => {
            let o = other.clone().flatten().ok_or_else(npe)?;
            let op: fn(u64, u64) -> u64 = match method {
                "and" => |a, b| a & b,
                "or" => |a, b| a | b,
                "xor" => |a, b| a ^ b,
                _ => |a, b| a & !b,
            };
            b.combine(&o, op);
            Ok(Value::Undef)
        }
        ("intersects", [_]) => {
            let o = other.clone().flatten().ok_or_else(npe)?;
            Ok(Value::bool(b.intersects(&o)))
        }
        ("equals", [_]) => Ok(Value::bool(
            other.clone().flatten().is_some_and(|o| b.same_bits(&o)),
        )),
        ("hashCode", []) => Ok(Value::Int(i64::from(b.hash()))),
        ("toString", []) => Ok(Value::str(b.render())),
        _ => Err(Fault::internal(format!(
            "javars: unsupported BitSet method `{method}` with {} argument(s)",
            args.len()
        ))),
    })
}

/// Run `f` on the summary statistics a handle names; `None` for any other
/// value.
fn with_stats<R>(v: &Value, f: impl FnOnce(&mut SummaryStats) -> R) -> Option<R> {
    let Value::Obj(id) = v else {
        return None;
    };
    HEAP.with(|h| match h.borrow_mut().get_mut(*id as usize) {
        Some(HostObj::Stats(s)) => Some(f(s)),
        _ => None,
    })
}

/// A method call on an `Int`/`Long`/`DoubleSummaryStatistics`; `None` for any
/// other receiver.
fn stats_method(recv: &Value, method: &str, args: &[Value]) -> Option<Result<Value, Fault>> {
    // `combine` reads another statistics object, so it is copied out before
    // the receiver is borrowed.
    let other = match (method, args) {
        ("combine", [o]) => Some(with_stats(o, |s| s.clone())),
        _ => None,
    };
    with_stats(recv, |s| match (method, args) {
        ("getCount", []) => Ok(Value::Int(s.count)),
        ("getSum", []) => Ok(s.get_sum()),
        ("getMin", []) => Ok(s.get_min()),
        ("getMax", []) => Ok(s.get_max()),
        ("getAverage", []) => Ok(Value::float(s.average())),
        ("toString", []) => Ok(Value::str(s.render())),
        ("accept", [v]) => {
            s.accept(v);
            Ok(Value::Undef)
        }
        ("combine", [_]) => match other.flatten() {
            Some(o) => {
                s.combine(&o);
                Ok(Value::Undef)
            }
            None => Err(Fault::java("NullPointerException", String::new())),
        },
        _ => Err(Fault::internal(format!(
            "javars: unsupported {} method `{method}` with {} argument(s)",
            s.class_name(),
            args.len()
        ))),
    })
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
            // A `Map.Entry` is compared by its *pair*, so two equal entries
            // have two different handles and bucketing on the handle would make
            // `m.entrySet().contains(Map.entry(k, v))` miss where the scan it
            // replaces hits. It declines, which is what the accelerator is
            // specified to do wherever the correspondence is not provable.
            None if entry_pair(v).is_some() => return None,
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
    ///
    /// `table` is the length of the JDK's bucket array, 0 while none is
    /// allocated; `init` is the capacity the first allocation will take, which
    /// is what `HashMap.threshold` holds until then (0 for the default 16).
    /// They follow the JDK's own sizing — see [`HashTable`] — because the order
    /// depends on the table, and the table on the container's history: a
    /// `new HashMap<>(64)`, a set that grew and then shrank, and a `HashSet`
    /// copied from a collection each iterate differently from one that merely
    /// holds the same keys.
    Hash { table: u32, init: u32 },
    /// `LinkedHashMap`/`LinkedHashSet` — insertion order.
    Insertion,
    /// `TreeMap`/`TreeSet`. Ordered by the keys' natural order, or — when
    /// `by_cmp` — by the `Comparator` the collection was constructed with
    /// ([`SORT_CMP`]). A comparator-ordered collection is *stored* in that
    /// order, kept so by [`rerank`] after every call that can add, because
    /// only a VM re-entry can run the comparator and presentation has no VM.
    ///
    /// `desc` marks a `descendingMap()`/`descendingSet()` copy: stored and
    /// located exactly as its ascending source is, presented in reverse, and
    /// navigated with first/last, floor/ceiling and lower/higher swapped.
    Sorted { by_cmp: bool, desc: bool },
}

impl Order {
    /// A `HashMap`/`HashSet` with no table allocated yet and default sizing.
    const HASH: Order = Order::Hash { table: 0, init: 0 };
}

/// Whether a `Set` on the heap is one of its own or a view a `Map` handed out.
///
/// It exists for two questions the JDK answers differently for the two, both
/// measured against openjdk 21.0.12:
///
///   * `getClass().getName()`. A `new HashSet<>()` is a `java.util.HashSet`;
///     `m.keySet()` is a private class named for the map that produced it
///     (`java.util.HashMap$KeySet`, `java.util.TreeMap$EntrySet`, …), and
///     `m.keySet() instanceof HashSet` is therefore `false`.
///   * `add`. Every map view refuses it with `UnsupportedOperationException`,
///     because there is no value to give a key that arrived on its own. A set
///     of its own accepts it.
///
/// The payload is the shape of the map behind the view, which is what picks
/// among the JDK's per-implementation names.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SetView {
    /// A set of its own — `new HashSet<>()`, `Set.of`, `Collectors.toSet`.
    Own,
    /// `map.keySet()`.
    Keys(ViewOf),
    /// `map.entrySet()`.
    Entries(ViewOf),
}

/// Which `java.util.Map` implementation a view was taken from. The JDK names
/// each view class after it, and the immutable factory's views are named after
/// neither the map nor the ordinary abstract classes — `Map.of(…).keySet()` is
/// an anonymous `java.util.AbstractMap$1`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewOf {
    Hash,
    Linked,
    Tree,
    Immutable,
}

impl ViewOf {
    /// The map implementation a map with this order and fixity presents as.
    fn of(order: Order, fixed: Fixity) -> ViewOf {
        match (fixed, order) {
            (Fixity::Immutable, _) => ViewOf::Immutable,
            (_, Order::Hash { .. }) => ViewOf::Hash,
            (_, Order::Insertion) => ViewOf::Linked,
            (_, Order::Sorted { .. }) => ViewOf::Tree,
        }
    }

    /// The discriminator that goes into the internal class tag a view or an
    /// entry carries (`Set$keys$hash`, `Entry$tree`). It is a name no Java
    /// program can write, which is the point: [`binary_name`] turns it into the
    /// JDK's own name and nothing else ever sees it.
    fn tag(self) -> &'static str {
        match self {
            ViewOf::Hash => "hash",
            ViewOf::Linked => "linked",
            ViewOf::Tree => "tree",
            ViewOf::Immutable => "immutable",
        }
    }
}

/// The map implementation an entry with this owner belongs to.
///
/// An entry with no owner is `Map.entry(k, v)`, whose class is
/// `java.util.KeyValueHolder` — which is also the class of an *immutable* map's
/// entries, so the two answer alike.
fn entry_view(owner: Option<u32>) -> ViewOf {
    let Some(id) = owner else {
        return ViewOf::Immutable;
    };
    HEAP.with(|h| match h.borrow().get(id as usize) {
        Some(HostObj::Map { order, fixed, .. }) => ViewOf::of(*order, *fixed),
        // The owner is gone or is not a map, which the heap makes impossible —
        // an entry only ever names the map it was read out of, and the heap is
        // never compacted. Answer as an ownerless pair rather than panicking.
        _ => ViewOf::Immutable,
    })
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
    /// Functional interface → the name of its single abstract method, populated
    /// by [`set_functional_sams`] before a run. It is what lets an *object* of a
    /// class that implements one (`class Rev implements Comparator<String>`) go
    /// wherever a lambda goes: the host calls the method the interface names.
    static SAMS: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
    /// The `Comparator` of each comparator-ordered `TreeMap`/`TreeSet`, by heap
    /// handle (see [`Order::Sorted`]). Cleared with the heap.
    static SORT_CMP: RefCell<HashMap<u32, Value>> = RefCell::new(HashMap::new());
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
    /// Java's string pool: text -> the interned `String` for it.
    ///
    /// Seeded from the chunk's constants by [`intern_literals`], so the canonical
    /// object for a text that appears as a literal *is* that literal — which is
    /// what makes `("a" + b).intern() == "ab"` `true`. A text with no literal
    /// interns the first value offered for it.
    static INTERNED: RefCell<HashMap<String, Value>> = RefCell::new(HashMap::new());
    /// Every live box: handle -> (wrapper class, the primitive it wraps).
    ///
    /// Separate from `HEAP` on purpose — see [`HostObj::Boxed`]. Indexed by the
    /// handle rather than hashed on it, because unboxing is on the path of every
    /// arithmetic operation a wrapper takes part in and every erased read.
    static BOXES: RefCell<Vec<Option<(&'static str, Value)>>> = const { RefCell::new(Vec::new()) };
    /// Every live `Map.Entry`: handle -> the pair and the map it belongs to.
    ///
    /// Separate from `HEAP` for the reason [`HostObj::Entry`] gives, and
    /// indexed by the handle for the reason [`BOXES`] gives: an entry inside a
    /// `Set` is compared against every candidate a `contains` walks.
    static PAIRS: RefCell<Vec<Option<Pair>>> = const { RefCell::new(Vec::new()) };
    /// For every map that has handed out entries: the entries it handed out.
    /// See [`entry_for`] for why a map must hand out the *same* entry for a key
    /// every time, and [`OwnerEntries`] for the accelerator and the scan.
    static ENTRY_INDEX: RefCell<HashMap<u32, OwnerEntries>> = RefCell::new(HashMap::new());
    /// Whether any map has handed out an entry yet.
    ///
    /// A program that never calls `entrySet` — nearly all of them — can then
    /// skip the entry bookkeeping on the collection path with one `Cell` read
    /// rather than a hash lookup per collection call.
    static ENTRIES_LIVE: Cell<bool> = const { Cell::new(false) };
    /// A `keySet()`/`entrySet()`/`values()` result, or a navigable range or
    /// descending copy (`headMap`, `subSet`, `descendingMap`, …), by heap
    /// handle, to the collection it was taken from and whether it is one of
    /// the navigable kind. See [`write_through`] and [`navigable_view`].
    static MAP_VIEWS: RefCell<HashMap<u32, (u32, bool)>> = RefCell::new(HashMap::new());
    /// The hash a heap-object key was filed under when it last entered a
    /// `HashMap`/`HashSet`, by handle. See [`file_key_hash`].
    static FILED_HASH: RefCell<HashMap<u32, i32>> = RefCell::new(HashMap::new());
    /// The hash containers a `merge`/`compute`/`computeIfAbsent` is running on,
    /// by handle. See [`coll_method`].
    static COMPUTING: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
}

/// Which class an entry with no map behind it belongs to. A map's own nodes
/// and `Map.entry(k, v)` are [`PairKind::Node`]; the other two are the public
/// `java.util.AbstractMap` implementations a program constructs itself.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PairKind {
    /// A map's node, or `Map.entry`'s `KeyValueHolder` when ownerless.
    Node,
    /// `AbstractMap.SimpleEntry`: `setValue` writes the entry itself.
    Simple,
    /// `AbstractMap.SimpleImmutableEntry`: `setValue` is refused.
    SimpleImmutable,
}

/// A `Map.Entry`'s payload — see [`HostObj::Entry`].
#[derive(Clone)]
struct Pair {
    key: Value,
    value: Value,
    /// The map `setValue` writes back to, or `None` for a `Map.entry(k, v)`
    /// pair that belongs to no map.
    owner: Option<u32>,
    /// True once the owner stopped holding this key.
    ///
    /// Java's entry *is* the map's node, so it reads the map's current value
    /// until the key is removed — and then keeps the last value it had, while a
    /// later re-insertion of the same key builds a *new* node this one never
    /// sees. Measured on `openjdk 21.0.12.1`:
    ///
    /// ```text
    /// m.put("k", 1); e = m.entrySet().iterator().next();
    /// m.put("k", 99);   e.getValue()  99
    /// m.remove("k");    e.getValue()  99   (the map no longer has the key)
    /// m.put("k", 7);    e.getValue()  99   (a new node; this one stays dead)
    ///                   e == m.entrySet().iterator().next()   false
    /// ```
    ///
    /// javars keeps a *copy* of the pair rather than pointing at the map's
    /// slot, so [`reconcile_entry`] carries each map write across to the
    /// entries that map owns and detaches the ones whose key has gone.
    detached: bool,
    /// The owner's [`OwnerEntries::generation`] this copy was last brought up
    /// to. While it matches, the copy is known current and costs nothing to
    /// read; when it lags, one keyed lookup repairs it.
    seen: u64,
    /// The entry's class when it belongs to no map; see [`PairKind`].
    kind: PairKind,
}

/// Give an entry a heap handle of its own and record its pair in [`PAIRS`].
fn alloc_entry(key: Value, value: Value, owner: Option<u32>) -> Value {
    alloc_pair(key, value, owner, PairKind::Node)
}

/// The [`PairKind`] of the `AbstractMap` entry class named `class`.
fn simple_pair_kind(class: &str) -> PairKind {
    if class == "SimpleEntry" {
        PairKind::Simple
    } else {
        PairKind::SimpleImmutable
    }
}

/// [`alloc_entry`] for an entry of any [`PairKind`].
fn alloc_pair(key: Value, value: Value, owner: Option<u32>, kind: PairKind) -> Value {
    let id = heap_alloc(HostObj::Entry);
    PAIRS.with(|p| {
        let mut p = p.borrow_mut();
        p.resize(id as usize + 1, None);
        p[id as usize] = Some(Pair {
            key,
            value,
            owner,
            detached: false,
            seen: 0,
            kind,
        });
    });
    Value::Obj(id)
}

/// The pair inside a `Map.Entry` handle, or `None` for anything else.
///
/// Every equality, hashing and rendering surface asks this first, so a value
/// that is not an entry answers exactly as it did before rather than wrongly.
fn entry_pair(v: &Value) -> Option<Pair> {
    let Value::Obj(id) = v else {
        return None;
    };
    let pair = PAIRS.with(|p| p.borrow().get(*id as usize)?.clone())?;
    // Every read of an entry comes through here, which makes it the place the
    // copy is brought level with the map — see [`reconcile_entry`].
    Some(reconcile_entry(*id, pair))
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

/// Seed the string pool with the chunk's `String` literals. Call after
/// [`heap_reset`] and before running.
///
/// `intern()` has to answer the *literal's* object for a text that has one, and
/// the literals live in the constant pool rather than anywhere the host can
/// reach at call time — so the pool is walked once here. Reading a text that
/// never appears as a literal is not an error: the first value offered for it
/// becomes canonical, which is what the JDK does too.
pub fn intern_literals(chunk: &fusevm::Chunk) {
    INTERNED.with(|table| {
        let mut table = table.borrow_mut();
        for c in &chunk.constants {
            if let Value::Str(text) = c {
                table
                    .entry(text.as_str().to_string())
                    .or_insert_with(|| c.clone());
            }
        }
    });
}

/// `s.intern()` — the canonical `String` for `s`'s text.
fn intern(v: &Value) -> Value {
    let text = java_str(v);
    INTERNED.with(|table| {
        table
            .borrow_mut()
            .entry(text)
            .or_insert_with(|| v.clone())
            .clone()
    })
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
    PAIRS.with(|p| p.borrow_mut().clear());
    // Keyed by heap handle like the caches above, so it goes with the heap for
    // the same reason: a surviving bucket would hand the next program's map an
    // entry belonging to this one's.
    ENTRY_INDEX.with(|x| x.borrow_mut().clear());
    ENTRIES_LIVE.with(|e| e.set(false));
    MAP_VIEWS.with(|m| m.borrow_mut().clear());
    FILED_HASH.with(|f| f.borrow_mut().clear());
    COMPUTING.with(|c| c.borrow_mut().clear());
    INTERNED.with(|i| i.borrow_mut().clear());
    SUPERS.with(|s| s.borrow_mut().clear());
    SORT_CMP.with(|s| s.borrow_mut().clear());
    EXIT_CODE.with(|c| c.set(None));
    STDIN_HANDLE.with(|s| s.set(None));
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

/// Record each functional interface's single abstract method name. Call before
/// running the chunk; read by `instance_sam`.
pub fn set_functional_sams(map: HashMap<String, String>) {
    SAMS.with(|s| *s.borrow_mut() = map);
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
        "PriorityQueue" => &["AbstractQueue", "Serializable"],
        "AbstractQueue" => &["AbstractCollection", "Queue"],
        "Queue" => &["Collection"],
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
        "Iterator$of" => &["Iterator"],
        "ListIterator$of" => &["ListIterator"],
        "ListIterator" => &["Iterator"],
        "Optional" | "OptionalInt" | "OptionalLong" | "OptionalDouble" => &["Serializable"],
        "Stream$of" => &["BaseStream"],
        "Collector$of" => &["Collector"],
        // `Map.of` reaches `AbstractMap` — unlike `Set.of`, which stops at
        // `AbstractCollection` — and is not `Cloneable`. Measured against the
        // JDK rather than copied from the line above.
        "Map$immutable" => &["AbstractMap", "Map", "Serializable"],
        "List$fixed" => &["AbstractList", "RandomAccess", "Serializable"],
        "List$sub" => &["AbstractList", "RandomAccess"],
        // The `Map` views. Every one of them is an `AbstractSet` — including
        // the immutable factory's `keySet`, which is `AbstractMap`'s own
        // anonymous one — and *none* of them is `Cloneable` or `Serializable`,
        // which is what separates a view from the `HashSet` it used to be
        // modeled as. The immutable `entrySet` is the exception: it stops at
        // `AbstractCollection`, exactly as `Set.of` does.
        "Set$keys$hash" | "Set$keys$linked" | "Set$keys$tree" | "Set$keys$immutable" => {
            &["AbstractSet"]
        }
        "Set$entries$hash" | "Set$entries$linked" | "Set$entries$tree" => &["AbstractSet"],
        "Set$entries$immutable" => &["AbstractCollection", "Set"],
        // A `Map.Entry` is an `Entry` and an `Object` and nothing else — not
        // `Serializable`, whichever map produced it. `Entry` is the simple name
        // `Map.Entry` flattens to, which is the name an `instanceof` writes.
        // A `values()` view stops at `AbstractCollection`: it is a
        // `Collection` and it is not a `List`, which is the one place the three
        // map views disagree with each other.
        "List$values$hash"
        | "List$values$linked"
        | "List$values$tree"
        | "List$values$immutable" => &["AbstractCollection"],
        "Entry$hash" | "Entry$linked" | "Entry$tree" | "Entry$immutable" => &["Entry"],
        // The two public `AbstractMap` entries are `Serializable`, unlike the
        // map nodes.
        "Entry$simple" | "Entry$simpleImmutable" => &["Entry", "Serializable"],
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
    vm.register_builtin(JARRAY_EXTEND, b_array_extend);
    vm.register_builtin(JARRAY_GET, b_array_get);
    vm.register_builtin(JARRAY_SET, b_array_set);
    vm.register_builtin(JNEW, b_new);
    vm.register_builtin(JFIELD_GET, b_field_get);
    vm.register_builtin(JFIELD_SET, b_field_set);
    vm.register_builtin(JINSTANCEOF, b_instanceof);
    vm.register_builtin(JCLASSOF, b_classof);
    vm.register_builtin(JDIV, b_div);
    vm.register_builtin(JIDIV, b_idiv);
    vm.register_builtin(JDIV_DYN, b_div_dyn);
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
    vm.register_builtin(JUNBOX_NONNULL, b_unbox_nonnull);
    vm.register_builtin(JNEW_STRING, b_new_string);
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
    // Nor is a value that is already a wrapper: boxing converts a primitive,
    // and a source the compiler could not type (`Integer i = it.next()`
    // through an erased interface) arrives boxed already. Wrapping it again
    // left a box inside a box, which arithmetic unwrapped only once, so
    // `i + 1` concatenated.
    if unboxed(&v).is_some() {
        return v;
    }
    box_value(code, v)
}

/// [`JNEW_STRING`] — a `String` with a fresh identity.
fn b_new_string(vm: &mut VM, argc: u8) -> Value {
    let args = pop_args(vm, argc);
    // `to_string` rather than a clone: a clone would share the argument's `Arc`
    // and the constructor's whole job is not to.
    Value::str(args.first().map(java_str).unwrap_or_default())
}

/// [`JUNBOX`] — the primitive inside a wrapper, or the value unchanged.
fn b_unbox(vm: &mut VM, argc: u8) -> Value {
    let args = pop_args(vm, argc);
    let v = args.first().cloned().unwrap_or(Value::Undef);
    deboxed(&v)
}

/// [`JUNBOX_NONNULL`] — [`b_unbox`], raising the `NullPointerException` the
/// wrapper's `xxxValue()` call raises on a `null` reference. The message keeps
/// the operation half of the JVM's helpful text and drops the provenance
/// clause, as every other modeled null dereference does (see `NULL_ARRAY_LOAD`).
fn b_unbox_nonnull(vm: &mut VM, argc: u8) -> Value {
    let args = pop_args(vm, argc);
    let v = args.first().cloned().unwrap_or(Value::Undef);
    if !matches!(v, Value::Undef) {
        return deboxed(&v);
    }
    let code = args.get(1).map(|c| c.jint()).unwrap_or(0);
    let (class, prim) = match usize::try_from(code).ok().and_then(|i| BOX_CLASSES.get(i)) {
        Some(&class) => (class, unboxed_primitive(class)),
        None => ("Boolean", "boolean"),
    };
    raise(
        vm,
        Fault::java(
            "NullPointerException",
            format!(
                "Cannot invoke \"java.lang.{class}.{prim}Value()\" because the receiver is null"
            ),
        ),
    )
}

/// The primitive a [`BOX_CLASSES`] wrapper holds.
fn unboxed_primitive(class: &str) -> &'static str {
    match class {
        "Integer" => "int",
        "Long" => "long",
        "Short" => "short",
        "Byte" => "byte",
        "Character" => "char",
        "Float" => "float",
        _ => "double",
    }
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
    // A local type's binary name puts javac's ordinal between the `$` and
    // the simple name (`T$1Point`); an anonymous class is the ordinal alone
    // (`T$1`), whose simple name is empty.
    match after_package.rsplit_once('$') {
        Some((_, tail)) => tail.trim_start_matches(|c: char| c.is_ascii_digit()),
        None => after_package,
    }
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

/// The single abstract method a user object supplies as a functional value: the
/// entry ip of its body and its parameter count, or `None` when `v` is not an
/// instance of a class that implements a functional interface.
///
/// `new Rev()` where `class Rev implements Comparator<String>` is as much a
/// comparator as `(a, b) -> …` is, and `list.sort(new Rev())` must call its
/// `compare`. The interface is found by walking the class's supertypes; the
/// body by walking them again for the first class declaring that method — the
/// rule virtual dispatch follows — so an inherited implementation is found too.
fn instance_sam(vm: &VM, v: &Value) -> Option<(usize, usize)> {
    let class = instance_class(v)?;
    let mut sam = None;
    walk_supertypes(&class, &mut |cur| {
        sam = SAMS.with(|s| s.borrow().get(cur).cloned());
        sam.is_some()
    });
    method_entry(vm, &class, &sam?)
}

/// Visit `class` and its supertypes breadth-first — nearest first, the order
/// a declaration is inherited in — until `found` answers `true`.
fn walk_supertypes(class: &str, found: &mut dyn FnMut(&str) -> bool) {
    let mut queue = std::collections::VecDeque::from([class.to_string()]);
    let mut seen = std::collections::HashSet::new();
    while let Some(cur) = queue.pop_front() {
        if !seen.insert(cur.clone()) {
            continue;
        }
        if found(&cur) {
            return;
        }
        SUPERS.with(|s| queue.extend(s.borrow().get(&cur).cloned().unwrap_or_default()));
    }
}

/// The entry ip and parameter count of the instance method `name` that
/// `class` resolves — its own declaration, else the nearest inherited one.
fn method_entry(vm: &VM, class: &str, name: &str) -> Option<(usize, usize)> {
    let tail = format!("#{name}#");
    let mut hit = None;
    walk_supertypes(class, &mut |cur| {
        let prefix = format!("{cur}{tail}");
        hit = vm.chunk.names.iter().enumerate().find_map(|(i, n)| {
            let tys = n.strip_prefix(&prefix)?;
            let entry = vm.chunk.find_sub(i as u16)?;
            Some((
                entry,
                if tys.is_empty() {
                    0
                } else {
                    tys.split(',').count()
                },
            ))
        });
        hit.is_some()
    });
    hit
}

/// One comparison of a sorted collection's order: `compare(a, b)` through its
/// comparator, or — when `cmp` is `null`, a natural-order collection of user
/// objects — `a.compareTo(b)`, falling back to [`natural_cmp`] for the values
/// javars orders itself. `None` when the call raised.
fn rank_compare(vm: &mut VM, cmp: &Value, a: &Value, b: &Value) -> Option<i64> {
    let n = if matches!(cmp, Value::Undef) {
        match instance_class(a).and_then(|c| method_entry(vm, &c, "compareTo")) {
            Some((entry, 1)) => {
                let stack_base = vm.stack.len();
                vm.stack.push(a.clone());
                vm.stack.push(b.clone());
                run_sub(vm, entry, stack_base).jint()
            }
            _ => natural_cmp(a, b) as i64,
        }
    } else {
        invoke_closure(vm, cmp, &[a.clone(), b.clone()]).jint()
    };
    (!pending()).then_some(n)
}

/// Whether `v` is a user object that orders itself — its class declares (or
/// inherits) a one-argument `compareTo`.
fn self_ordering(vm: &VM, v: &Value) -> bool {
    instance_class(v)
        .and_then(|c| method_entry(vm, &c, "compareTo"))
        .is_some_and(|(_, arity)| arity == 1)
}

/// A value that can be called as a functional interface: a lambda, or an
/// object whose class implements one.
fn is_callable(vm: &VM, v: &Value) -> bool {
    closure_meta(v).is_some() || instance_sam(vm, v).is_some()
}

/// The parameter count of a callable value's single abstract method.
fn callable_arity(vm: &VM, v: &Value) -> Option<usize> {
    closure_meta(v)
        .map(|(_, params, _)| params as usize)
        .or_else(|| instance_sam(vm, v).map(|(_, arity)| arity))
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
        // An object of a class implementing the functional interface: the call
        // is that interface's abstract method on the object, receiver first.
        if let Some((entry, arity)) = instance_sam(vm, &clo) {
            let stack_base = vm.stack.len();
            vm.stack.push(clo);
            for i in 0..arity {
                vm.stack.push(args.get(i).cloned().unwrap_or(Value::Undef));
            }
            return run_sub(vm, entry, stack_base);
        }
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
    // A wrapper hashes as its primitive, by its own class's rule. The width
    // matters for two of them: a `Long` folds its halves even when its value
    // would fit an `int` (`Long.valueOf(-1).hashCode()` is 0, not -1), and a
    // `Float` is `floatToIntBits` unfolded rather than `Double`'s fold.
    if let Some(inner) = unboxed(v) {
        return Some(match box_class(v) {
            Some("Long") => long_hash(inner.jint()),
            Some("Float") => float_hash(inner.jfloat()),
            _ => return java_hash(&inner),
        });
    }
    Some(match v {
        // `String.hashCode` is specified: s[0]*31^(n-1) + … + s[n-1]. Java
        // counts UTF-16 code units; javars counts scalars, the same
        // `char`-model simplification the `String` methods already make.
        Value::Str(s) => s
            .chars()
            .fold(0i32, |h, c| h.wrapping_mul(31).wrapping_add(c as i32)),
        // `Integer.hashCode` is the value; `Long.hashCode` folds the halves.
        // An unboxed integer carries no width, so one in `int` range hashes as
        // an `Integer` (a boxed `Long` is told apart above).
        Value::Int(n) => match i32::try_from(*n) {
            Ok(i) => i,
            Err(_) => long_hash(*n),
        },
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

/// `Long.hashCode(n)`: the two 32-bit halves XORed, `(int) (n ^ (n >>> 32))`.
fn long_hash(n: i64) -> i32 {
    (n ^ ((n as u64) >> 32) as i64) as i32
}

/// `Float.hashCode(f)`: `floatToIntBits`, which collapses every `NaN` to
/// `0x7fc00000` and is not folded.
fn float_hash(f: f64) -> i32 {
    let f = f as f32;
    if f.is_nan() {
        0x7fc0_0000
    } else {
        f.to_bits() as i32
    }
}

/// The hash one *element* of a collection contributes, which is what
/// `e.hashCode()` on that element answers.
///
/// [`java_hash`] declines a heap handle; `Object.hashCode` on one is the handle
/// itself (see [`object_method`]), and `null` contributes 0 the way Java's
/// `AbstractList.hashCode` specifies. A class that declares its own `hashCode`
/// still does not have that body run — see BUGS.md — so this is the identity
/// hash for a user instance, exactly as a direct `x.hashCode()` is.
fn element_hash(v: &Value) -> i32 {
    match java_hash(v) {
        Some(h) => h,
        // `Map.Entry.hashCode` is specified as `keyHash ^ valueHash`, not the
        // identity hash a handle would otherwise get. It is what makes
        // `m.entrySet().hashCode() == m.hashCode()`, since [`map_hash`] sums
        // exactly that quantity over the same pairs.
        None => match entry_pair(v) {
            Some(p) => element_hash(&p.key) ^ element_hash(&p.value),
            None => match v {
                Value::Obj(id) => *id as i32,
                _ => 0,
            },
        },
    }
}

/// `getKey` / `getValue` / `setValue` and the three `Object` methods a
/// `Map.Entry` overrides, on an [`HostObj::Entry`] receiver.
///
/// `None` for any other receiver or for `getClass`, which leaves the call to
/// the dispatch that follows — an entry's binary name is decided by
/// [`binary_name`] like every other shape's.
///
/// `setValue` is the reason an entry carries its map rather than a copy of the
/// pair: Java specifies it as a write to the *backing map*, and rewriting every
/// value in place through `for (var e : m.entrySet()) e.setValue(f(e))` is the
/// ordinary way to use one. An entry with no map behind it — `Map.entry(k, v)`,
/// or one read out of a `Map.of` — refuses with the JDK's own message.
fn entry_method(recv: &Value, method: &str, args: &[Value]) -> Option<Result<Value, Fault>> {
    let pair = entry_pair(recv)?;
    let Value::Obj(id) = recv else {
        return None;
    };
    Some(match (method, args.len()) {
        ("getKey", 0) => Ok(pair.key),
        ("getValue", 0) => Ok(pair.value),
        // `AbstractMap.SimpleEntry` holds its own value; the immutable one
        // refuses with a bare `UnsupportedOperationException`, not the
        // `"not supported"` of `KeyValueHolder`.
        ("setValue", 1) if pair.kind == PairKind::Simple => {
            Ok(store_entry_value(*id, args[0].clone()))
        }
        ("setValue", 1) if pair.kind == PairKind::SimpleImmutable => {
            Err(Fault::java("UnsupportedOperationException", String::new()))
        }
        ("setValue", 1) => match pair.owner {
            // A live entry writes through to its map, which is the whole point
            // of the method.
            Some(owner) if !pair.detached && map_is_writable(owner) => {
                Ok(set_entry_value(*id, owner, &pair.key, args[0].clone()))
            }
            // A *detached* entry writes only itself, exactly as the JDK's dead
            // node does. Measured: after `m.remove("k")` on a map that held
            // `k=1`, `e.setValue(42)` answers `1`, `e.getValue()` is then `42`,
            // and `m` is still `{}`.
            Some(owner) if map_is_writable(owner) => Ok(store_entry_value(*id, args[0].clone())),
            // An entry read out of a `Map.of`, or a `Map.entry(k, v)` that
            // belongs to no map at all.
            _ => Err(Fault::java(
                "UnsupportedOperationException",
                "not supported".to_string(),
            )),
        },
        ("toString", 0) => Ok(Value::str(java_str(recv))),
        ("equals", 1) => Ok(Value::bool(value_eq(recv, &args[0]))),
        ("hashCode", 0) => Ok(Value::Int(element_hash(recv).into())),
        _ => return None,
    })
}

/// True when the map at `id` accepts a `put` — an entry read out of a `Map.of`
/// refuses `setValue` for the same reason the map refuses `put`.
fn map_is_writable(id: u32) -> bool {
    HEAP.with(|h| {
        matches!(
            h.borrow().get(id as usize),
            Some(HostObj::Map { fixed, .. }) if *fixed != Fixity::Immutable
        )
    })
}

/// Write an entry's own copy of the value, answering the one it held.
fn store_entry_value(id: u32, v: Value) -> Value {
    PAIRS.with(|p| {
        let mut p = p.borrow_mut();
        p.get_mut(id as usize)
            .and_then(|s| s.as_mut())
            .map(|pair| std::mem::replace(&mut pair.value, v))
            .unwrap_or(Value::Undef)
    })
}

/// Write `v` through an entry to the map that owns it, answering the value the
/// entry held before.
///
/// The map is only touched where the key is still in it: an entry whose key has
/// since been removed is a *detached* node in the JDK too, and writing to it
/// updates the node and nothing else.
///
/// No other entry has to be told: [`entry_for`] hands out exactly one live
/// entry per key per map, so the entry written here is the only one on that key
/// — which is also the shape Java has, where it is the map's single node.
fn set_entry_value(id: u32, owner: u32, key: &Value, v: Value) -> Value {
    let old = store_entry_value(id, v.clone());
    HEAP.with(|h| {
        if let Some(HostObj::Map { entries, .. }) = h.borrow_mut().get_mut(owner as usize) {
            if let Some((_, slot)) = entries.iter_mut().find(|(k, _)| value_eq(k, key)) {
                *slot = v;
            }
        }
    });
    old
}

/// The entries one map has handed out.
#[derive(Default)]
struct OwnerEntries {
    /// Every live entry of this map, in hand-out order. The authority: an entry
    /// whose key [`index_key`] cannot bucket is here and nowhere else.
    live: Vec<u32>,
    /// The bucket accelerator [`existing_entry`] tries first. Only keys
    /// `index_key` can bucket appear, on the same terms as [`KeyIndex`] — an
    /// accelerator that declines is correct, one that misses is not.
    by_key: HashMap<IndexKey, u32>,
    /// How many times this map has been mutated since it first handed out an
    /// entry. An entry carrying a smaller [`Pair::seen`] is stale and repairs
    /// itself on its next read.
    generation: u64,
}

/// The entry this map already hands out for `key`, or a fresh one.
///
/// Java's `entrySet()` builds nothing: the map keeps one node per key and the
/// view walks them, so two `entrySet().iterator().next()` calls on the same map
/// answer the *same object* and `==` between them is `true`. Handing out a new
/// pair per call made that `false`, and made a loop that calls `entrySet()`
/// repeatedly allocate a fresh entry per key per iteration.
///
/// A detached entry is never reused: its key has been removed, and Java builds
/// a new node when the same key comes back.
fn entry_for(owner: u32, key: Value, value: Value) -> Value {
    if let Some(id) = existing_entry(owner, &key) {
        // The caller read this value straight out of the map, so the copy is
        // current as of the map's present generation.
        let generation = ENTRY_INDEX.with(|x| x.borrow().get(&owner).map_or(0, |o| o.generation));
        PAIRS.with(|p| {
            if let Some(pair) = p.borrow_mut().get_mut(id as usize).and_then(|s| s.as_mut()) {
                pair.value = value;
                pair.seen = generation;
            }
        });
        return Value::Obj(id);
    }
    let bucket = index_key(&key);
    let v = alloc_entry(key, value, Some(owner));
    if let Value::Obj(id) = &v {
        ENTRIES_LIVE.with(|e| e.set(true));
        ENTRY_INDEX.with(|x| {
            let mut x = x.borrow_mut();
            let owned = x.entry(owner).or_default();
            owned.live.push(*id);
            if let Some(bucket) = bucket {
                owned.by_key.insert(bucket, *id);
            }
            let generation = owned.generation;
            PAIRS.with(|p| {
                if let Some(pair) = p
                    .borrow_mut()
                    .get_mut(*id as usize)
                    .and_then(|s| s.as_mut())
                {
                    pair.seen = generation;
                }
            });
        });
    }
    v
}

/// The live entry `owner` hands out for `key`, if it has one.
fn existing_entry(owner: u32, key: &Value) -> Option<u32> {
    // The key of a live (non-detached) entry, for confirming a candidate. The
    // candidate is reconciled first: a map that dropped this key has not walked
    // out to its entries to say so, and reusing one whose key is gone would
    // revive a node Java replaces.
    let live_key = |id: u32| {
        let pair = PAIRS.with(|p| p.borrow().get(id as usize)?.clone())?;
        let pair = reconcile_entry(id, pair);
        (pair.owner == Some(owner) && !pair.detached).then_some(pair.key)
    };
    let (hit, others) = ENTRY_INDEX.with(|x| {
        let x = x.borrow();
        let Some(owned) = x.get(&owner) else {
            return (None, Vec::new());
        };
        match index_key(key) {
            // `by_key` holds every live entry whose key can be bucketed, and
            // `index_key` puts equal keys in the same bucket, so for a key it
            // accepts an empty bucket is a *confirmed* absence. Scanning anyway
            // would clone the entry list once per key and make building an
            // `entrySet` of n keys quadratic — measured at 2,000 keys as 13.0G
            // instructions against 0.35G.
            Some(b) => (owned.by_key.get(&b).copied(), Vec::new()),
            // A key it declines to bucket can only be found by the scan the
            // accelerator replaces, over this map's entries only.
            None => (None, owned.live.clone()),
        }
    });
    if let Some(id) = hit {
        // A bucket is an accelerator; the candidate is still confirmed.
        return live_key(id).filter(|k| value_eq(k, key)).map(|_| id);
    }
    others
        .into_iter()
        .find(|id| live_key(*id).is_some_and(|k| value_eq(&k, key)))
}

/// Mark an entry detached and drop it from [`ENTRY_INDEX`], so the next
/// `entrySet()` over a re-inserted key builds a new one.
fn detach_entry(id: u32) {
    let removed = PAIRS.with(|p| {
        let mut p = p.borrow_mut();
        let pair = p.get_mut(id as usize).and_then(|s| s.as_mut())?;
        pair.detached = true;
        Some((pair.owner?, pair.key.clone()))
    });
    let Some((owner, key)) = removed else {
        return;
    };
    let bucket = index_key(&key);
    ENTRY_INDEX.with(|x| {
        let mut x = x.borrow_mut();
        let Some(owned) = x.get_mut(&owner) else {
            return;
        };
        owned.live.retain(|e| *e != id);
        // Only where this entry is the one filed under the bucket: a later
        // entry on an equal key may already have replaced it.
        if let Some(bucket) = bucket {
            if owned.by_key.get(&bucket) == Some(&id) {
                owned.by_key.remove(&bucket);
            }
        }
    });
}

/// Note that a map changed, so the entries it handed out know to re-read.
///
/// One increment, whatever the map holds and however many entries it gave out
/// — the repair itself is deferred to the entry that is actually read, in
/// [`reconcile_entry`]. Doing the repair here instead would walk every entry on
/// every `put`, which is quadratic on the ordinary shape of taking `entrySet()`
/// and then filling the map (measured: 2,000 keys took 7.05s that way against
/// 0.03s before the entry work).
fn bump_entry_generation(owner: u32) {
    ENTRY_INDEX.with(|x| {
        if let Some(owned) = x.borrow_mut().get_mut(&owner) {
            owned.generation = owned.generation.wrapping_add(1);
        }
    });
}

/// The value `owner` holds for `key` right now, or `None` if it holds no such
/// key.
///
/// Goes through the map's own [`KeyIndex`], so a repair is one hash lookup
/// rather than a walk of the map. The borrow is shared, and `value_eq` may take
/// one of its own, which a `RefCell` allows.
fn map_value_of(owner: u32, key: &Value) -> Option<Value> {
    HEAP.with(|h| {
        let heap = h.borrow();
        let Some(HostObj::Map { entries, index, .. }) = heap.get(owner as usize) else {
            return None;
        };
        match index.find(entries, |(k, _)| k, key) {
            // The index answered: a position, or a confirmed absence.
            Some(hit) => hit.map(|i| entries[i].1.clone()),
            // It declined (stale, or a key it cannot bucket) — scan, as every
            // other caller of `find` does when it declines.
            None => entries
                .iter()
                .find(|(k, _)| value_eq(k, key))
                .map(|(_, v)| v.clone()),
        }
    })
}

/// Bring one entry's copy level with its map, if the map has moved since.
///
/// javars' entry holds a *copy* of the pair where Java's **is** the map's node.
/// Rather than push every map write out to the entries, the map counts its
/// mutations and an entry repairs itself the first time it is read at a newer
/// count: one keyed lookup, once per entry per mutation that is actually
/// observed, and nothing at all for an entry nobody looks at.
///
/// An entry whose key the map no longer holds [detaches](Pair::detached) here,
/// which is the moment Java's node leaves the table.
fn reconcile_entry(id: u32, pair: Pair) -> Pair {
    let Some(owner) = pair.owner.filter(|_| !pair.detached) else {
        return pair;
    };
    let generation = ENTRY_INDEX.with(|x| x.borrow().get(&owner).map(|o| o.generation));
    let Some(generation) = generation else {
        return pair;
    };
    if generation == pair.seen {
        return pair;
    }
    match map_value_of(owner, &pair.key) {
        Some(v) => {
            let mut fresh = pair;
            fresh.value = v;
            fresh.seen = generation;
            write_pair(id, fresh.clone());
            fresh
        }
        None => {
            detach_entry(id);
            let mut dead = pair;
            dead.detached = true;
            dead.seen = generation;
            write_pair(id, dead.clone());
            dead
        }
    }
}

/// Detach every entry of `owner` whose key the map no longer holds.
///
/// Runs only on a call that left the map smaller, which is the only way a key
/// can go. One keyed lookup per entry handed out, and nothing for a map that
/// handed out none.
fn detach_orphaned_entries(owner: u32) {
    let live: Vec<u32> = ENTRY_INDEX.with(|x| {
        x.borrow()
            .get(&owner)
            .map_or(Vec::new(), |o| o.live.clone())
    });
    for id in live {
        let Some(pair) = PAIRS.with(|p| p.borrow().get(id as usize)?.clone()) else {
            continue;
        };
        // `reconcile_entry` detaches exactly the ones whose key is gone, and
        // brings the rest forward while it is looking.
        reconcile_entry(id, pair);
    }
}

/// Replace an entry's stored pair wholesale.
fn write_pair(id: u32, pair: Pair) {
    PAIRS.with(|p| {
        if let Some(slot) = p.borrow_mut().get_mut(id as usize) {
            *slot = Some(pair);
        }
    });
}

/// `hasNext` / `next` / `remove` on an [`HostObj::Iterator`] receiver, plus the
/// `ListIterator` methods when the iterator came from `listIterator`.
///
/// `None` for any other receiver, which leaves the call to the dispatch that
/// follows.
///
/// The cursor model is `java.util.ArrayList.ListItr`'s: `pos` is the index the
/// next `next()` returns (its `cursor`) and `last` is `lastRet`. `next()` and
/// `previous()` both set `last`; `remove()` deletes it and moves the cursor
/// onto the gap; `set()` overwrites it without touching the modification
/// count; `add()` inserts at the cursor, steps past the new element, and
/// forgets `last`, so a following `set`/`remove` is `IllegalStateException`.
fn iterator_method(recv: &Value, method: &str, args: &[Value]) -> Option<Result<Value, Fault>> {
    let Value::Obj(id) = recv else {
        return None;
    };
    let id = *id;
    let state = HEAP.with(|h| match h.borrow().get(id as usize) {
        Some(HostObj::Iterator {
            source,
            pos,
            last,
            exp_mods,
            bidi,
            desc,
        }) => Some((*source, *pos, *last, *exp_mods, *bidi, *desc)),
        _ => None,
    })?;
    let (source, pos, last, exp_mods, bidi, desc) = state;
    let argc = args.len();
    let mut items = sequence_items(&Value::Obj(source)).unwrap_or_default();
    if desc {
        items.reverse();
    }
    let stale = || iter_mods(source) != exp_mods;
    // `Collections.enumeration`/`emptyEnumeration` hand out this same cursor;
    // an `Enumeration` names its two moves differently and its `asIterator()`
    // walks on from where it stands.
    let method = match method {
        "hasMoreElements" => "hasNext",
        "nextElement" => "next",
        "asIterator" if argc == 0 => return Some(Ok(recv.clone())),
        other => other,
    };
    Some(match (method, argc, bidi) {
        ("hasNext", 0, _) => Ok(Value::bool(pos < items.len())),
        ("next", 0, _) => {
            if stale() {
                return Some(Err(comodification()));
            }
            match items.get(pos) {
                Some(v) => {
                    set_cursor(id, pos + 1, Some(pos), exp_mods);
                    Ok(v.clone())
                }
                None => Err(Fault::java("NoSuchElementException", String::new())),
            }
        }
        ("hasPrevious", 0, true) => Ok(Value::bool(pos > 0)),
        ("nextIndex", 0, true) => Ok(Value::Int(pos as i64)),
        ("previousIndex", 0, true) => Ok(Value::Int(pos as i64 - 1)),
        ("previous", 0, true) => {
            if stale() {
                return Some(Err(comodification()));
            }
            match pos
                .checked_sub(1)
                .and_then(|at| items.get(at).map(|v| (at, v)))
            {
                Some((at, v)) => {
                    set_cursor(id, at, Some(at), exp_mods);
                    Ok(v.clone())
                }
                None => Err(Fault::java("NoSuchElementException", String::new())),
            }
        }
        ("set", 1, true) => {
            let Some(at) = last else {
                return Some(Err(Fault::java("IllegalStateException", String::new())));
            };
            if stale() {
                return Some(Err(comodification()));
            }
            HEAP.with(|h| {
                if let Some(HostObj::List { items, .. }) = h.borrow_mut().get_mut(source as usize) {
                    if let Some(slot) = items.get_mut(at) {
                        *slot = args[0].clone();
                    }
                }
            });
            Ok(Value::Undef)
        }
        ("add", 1, true) => {
            if stale() {
                return Some(Err(comodification()));
            }
            let mods = HEAP.with(|h| match h.borrow_mut().get_mut(source as usize) {
                Some(HostObj::List { items, mods, .. }) if pos <= items.len() => {
                    items.insert(pos, args[0].clone());
                    *mods += 1;
                    Some(*mods)
                }
                _ => None,
            });
            match mods {
                Some(mods) => {
                    set_cursor(id, pos + 1, None, mods);
                    Ok(Value::Undef)
                }
                None => Err(comodification()),
            }
        }
        // `remove()` deletes the element `next()`/`previous()` last returned
        // and leaves the cursor on the gap, so the following `next()` sees the
        // element that shifted into it. Calling it twice, or before any move,
        // is Java's `IllegalStateException`.
        ("remove", 0, _) => {
            let Some(at) = last else {
                return Some(Err(Fault::java("IllegalStateException", String::new())));
            };
            if stale() {
                return Some(Err(comodification()));
            }
            // A descending cursor counts from the end, so the element it names
            // sits at the mirrored index of the source.
            let gone = if desc { items.len() - 1 - at } else { at };
            let removed = HEAP.with(|h| {
                let mut heap = h.borrow_mut();
                match heap.get_mut(source as usize) {
                    Some(HostObj::List { items, mods, .. }) if gone < items.len() => {
                        items.remove(gone);
                        *mods += 1;
                        Some(*mods)
                    }
                    // A set *presents* its elements in its order (sorted for a
                    // `TreeSet`, bucket order for a `HashSet`) but stores them
                    // in insertion order, so the element the cursor returned
                    // is found by value rather than by position.
                    Some(HostObj::Set {
                        items: stored,
                        index,
                        ..
                    }) if at < items.len() => {
                        let Some(slot) = stored.iter().position(|v| value_eq(v, &items[at])) else {
                            return None;
                        };
                        stored.remove(slot);
                        index.invalidate();
                        Some(0)
                    }
                    _ => None,
                }
            });
            match removed {
                Some(mods) => {
                    set_cursor(id, at, None, mods);
                    Ok(Value::Undef)
                }
                None => Err(Fault::java("IllegalStateException", String::new())),
            }
        }
        _ => Err(Fault::internal(format!(
            "javars: unsupported {} method `{method}` with {argc} argument(s)",
            if bidi { "ListIterator" } else { "Iterator" }
        ))),
    })
}

/// A fresh forward iterator over the collection at `source` — a
/// `ListIterator` when `bidi`.
fn new_iterator(source: u32, bidi: bool) -> Value {
    Value::Obj(heap_alloc(HostObj::Iterator {
        source,
        pos: 0,
        last: None,
        exp_mods: iter_mods(source),
        bidi,
        desc: false,
    }))
}

/// Store an iterator's cursor state after a move or a write.
fn set_cursor(id: u32, to: usize, ret: Option<usize>, mods: u64) {
    HEAP.with(|h| {
        if let Some(HostObj::Iterator {
            pos,
            last,
            exp_mods,
            ..
        }) = h.borrow_mut().get_mut(id as usize)
        {
            *pos = to;
            *last = ret;
            *exp_mods = mods;
        }
    });
}

/// `Collection.toArray()`, `toArray(T[])`, and `toArray(IntFunction<T[]>)`.
///
/// The array form follows `AbstractCollection.toArray(T[])`: an array at least
/// as long as the collection is filled in place and returned, with the slot
/// just past the last element set to `null` when there is room; a shorter one
/// only names the type, and a fresh array is returned. The generator form is
/// `toArray(generator.apply(0))`, as `Collection`'s default declares it.
fn collection_to_array(
    vm: &mut VM,
    items: Vec<Value>,
    arg: Option<&Value>,
) -> Result<Value, Fault> {
    let fresh = |items: Vec<Value>| Ok(Value::Obj(heap_alloc(HostObj::Array(items))));
    let target = match arg {
        None => return fresh(items),
        Some(Value::Undef) => return Err(Fault::java("NullPointerException", String::new())),
        Some(g) if closure_meta(g).is_some() => {
            let a = invoke_closure(vm, g, &[Value::Int(0)]);
            if pending() {
                return Ok(Value::Undef);
            }
            a
        }
        Some(a) => a.clone(),
    };
    let Value::Obj(aid) = target else {
        return Err(Fault::internal("javars: `toArray` needs an array argument"));
    };
    let filled = HEAP.with(|h| match h.borrow_mut().get_mut(aid as usize) {
        Some(HostObj::Array(slots)) if slots.len() >= items.len() => {
            let n = items.len();
            for (slot, v) in slots.iter_mut().zip(&items) {
                *slot = v.clone();
            }
            if let Some(after) = slots.get_mut(n) {
                *after = Value::Undef;
            }
            true
        }
        _ => false,
    });
    if filled {
        Ok(target)
    } else {
        fresh(items)
    }
}

/// Whether a user closure raised, leaving a throwable pending.
fn pending() -> bool {
    PENDING.with(|p| p.borrow().is_some())
}

/// A `java.util.PriorityQueue`'s heap array, comparator, natural-order flag,
/// and modification count, cloned out from under the heap borrow so the
/// comparator can re-enter the VM.
fn pq_state(id: u32) -> Option<(Vec<Value>, Value, bool, u64)> {
    HEAP.with(|h| match h.borrow().get(id as usize) {
        Some(HostObj::PQueue {
            items,
            cmp,
            natural,
            mods,
        }) => Some((items.clone(), cmp.clone(), *natural, *mods)),
        _ => None,
    })
}

/// Store a heap array back after an operation rearranged it; `structural`
/// bumps the modification count the way `PriorityQueue.modCount++` does.
fn pq_store(id: u32, heap: Vec<Value>, structural: bool) {
    HEAP.with(|h| {
        if let Some(HostObj::PQueue { items, mods, .. }) = h.borrow_mut().get_mut(id as usize) {
            *items = heap;
            if structural {
                *mods += 1;
            }
        }
    });
}

/// `PriorityQueue.siftUp`: place `x` at `k` and bubble it toward the root
/// while it compares strictly below its parent.
fn pq_sift_up(vm: &mut VM, heap: &mut [Value], mut k: usize, x: Value, cmp: &Value) {
    while k > 0 {
        let parent = (k - 1) >> 1;
        if invoke_closure(vm, cmp, &[x.clone(), heap[parent].clone()]).jint() >= 0 {
            break;
        }
        heap[k] = heap[parent].clone();
        k = parent;
    }
    heap[k] = x;
}

/// `PriorityQueue.siftDown` over the first `n` slots: place `x` at `k` and
/// sink it below the smaller child (the right one only when it compares
/// strictly smaller) while that child compares strictly below `x`. Returns
/// the slot `x` came to rest in.
fn pq_sift_down(
    vm: &mut VM,
    heap: &mut [Value],
    mut k: usize,
    x: Value,
    n: usize,
    cmp: &Value,
) -> usize {
    let half = n >> 1;
    while k < half {
        let mut child = 2 * k + 1;
        let right = child + 1;
        if right < n
            && invoke_closure(vm, cmp, &[heap[child].clone(), heap[right].clone()]).jint() > 0
        {
            child = right;
        }
        if invoke_closure(vm, cmp, &[x.clone(), heap[child].clone()]).jint() <= 0 {
            break;
        }
        heap[k] = heap[child].clone();
        k = child;
    }
    heap[k] = x;
    k
}

/// `PriorityQueue.heapify`: sift down every parent, last one first — the
/// layout the collection constructor and a bulk removal leave behind.
fn pq_heapify(vm: &mut VM, heap: &mut [Value], cmp: &Value) {
    let n = heap.len();
    for i in (0..n / 2).rev() {
        let x = heap[i].clone();
        pq_sift_down(vm, heap, i, x, n, cmp);
    }
}

/// `PriorityQueue.removeAt`: fill slot `i` with the last element and restore
/// the heap, sinking it and — when it did not move — floating it instead.
///
/// Returns the moved element when it floated *above* `i`, which is the one
/// case an iterator standing at `i` would otherwise never visit it; `None`
/// otherwise, exactly as the JDK's `removeAt` does.
fn pq_remove_at(vm: &mut VM, heap: &mut Vec<Value>, i: usize, cmp: &Value) -> Option<Value> {
    let moved = heap.pop()?;
    if i == heap.len() {
        return None;
    }
    let n = heap.len();
    if pq_sift_down(vm, heap, i, moved.clone(), n, cmp) != i {
        return None;
    }
    pq_sift_up(vm, heap, i, moved.clone(), cmp);
    // `siftUp` either left it in slot `i` or moved it toward the root.
    if matches!(&heap[i], Value::Obj(a) if matches!(&moved, Value::Obj(b) if a == b))
        || (!matches!(moved, Value::Obj(_)) && value_eq(&heap[i], &moved))
    {
        None
    } else {
        Some(moved)
    }
}

/// Build a `PriorityQueue` from the constructor's arguments.
///
/// The compiler passes up to two source arguments plus the natural-order
/// comparator it synthesizes. The JDK's overloads are told apart by their
/// runtime shape: a closure is the comparator (`(Comparator)` or
/// `(int, Comparator)`), an integer is a capacity with no observable effect,
/// and a collection seeds the queue. A `PriorityQueue` seed hands over its heap
/// array and its comparator unchanged, a sorted set its ascending order (a
/// valid heap already); any other collection is copied and heapified.
fn new_priority_queue(
    vm: &mut VM,
    a0: &Value,
    a1: &Value,
    natural_cmp: &Value,
) -> Result<Value, Fault> {
    let explicit = [a1, a0].into_iter().find(|v| is_callable(vm, v)).cloned();
    let seed = match a0 {
        Value::Obj(sid) if !is_callable(vm, a0) => Some(*sid),
        _ => None,
    };
    let (mut cmp, mut natural) = match explicit {
        Some(c) => (c, false),
        None => (natural_cmp.clone(), true),
    };
    let mut heap = Vec::new();
    if let Some(sid) = seed {
        if let Some((items, c, nat, _)) = pq_state(sid) {
            heap = items;
            cmp = c;
            natural = nat;
        } else {
            let sorted = HEAP.with(|h| {
                matches!(
                    h.borrow().get(sid as usize),
                    Some(HostObj::Set {
                        order: Order::Sorted { .. },
                        ..
                    })
                )
            });
            heap = sequence_items(a0).ok_or_else(|| {
                Fault::internal("javars: `new PriorityQueue<>(…)` needs a collection")
            })?;
            if heap.iter().any(|v| matches!(v, Value::Undef)) {
                return Err(Fault::java("NullPointerException", String::new()));
            }
            if !sorted {
                pq_heapify(vm, &mut heap, &cmp);
            }
        }
    }
    Ok(Value::Obj(heap_alloc(HostObj::PQueue {
        items: heap,
        cmp,
        natural,
        mods: 0,
    })))
}

/// The methods of a `java.util.PriorityQueue` receiver.
///
/// `None` for any other receiver, and for the methods the generic collection
/// path already answers from [`sequence_items`] in heap-array order
/// (`stream`, `forEach`, `toString`, `toArray`) — which is the order
/// `PriorityQueue`'s own iterator and `toString` present.
fn pq_method(vm: &mut VM, id: u32, method: &str, args: &[Value]) -> Option<Value> {
    let (mut heap, cmp, natural, mods) = pq_state(id)?;
    let fail =
        |vm: &mut VM, class: &'static str| Some(raise(vm, Fault::java(class, String::new())));
    let result = match (method, args.len()) {
        ("add" | "offer", 1) => {
            if matches!(args[0], Value::Undef) {
                return fail(vm, "NullPointerException");
            }
            let k = heap.len();
            heap.push(Value::Undef);
            pq_sift_up(vm, &mut heap, k, args[0].clone(), &cmp);
            Value::bool(true)
        }
        ("addAll", 1) => {
            if matches!(&args[0], Value::Obj(o) if *o == id) {
                return fail(vm, "IllegalArgumentException");
            }
            let items = sequence_items(&args[0]).unwrap_or_default();
            for v in &items {
                if matches!(v, Value::Undef) {
                    pq_store(id, heap, true);
                    return fail(vm, "NullPointerException");
                }
                let k = heap.len();
                heap.push(Value::Undef);
                pq_sift_up(vm, &mut heap, k, v.clone(), &cmp);
            }
            Value::bool(!items.is_empty())
        }
        ("peek", 0) => return Some(heap.first().cloned().unwrap_or(Value::Undef)),
        ("element", 0) => match heap.first() {
            Some(v) => return Some(v.clone()),
            None => return fail(vm, "NoSuchElementException"),
        },
        ("poll" | "remove", 0) => {
            if heap.is_empty() {
                return if method == "poll" {
                    Some(Value::Undef)
                } else {
                    fail(vm, "NoSuchElementException")
                };
            }
            let top = heap[0].clone();
            pq_remove_at(vm, &mut heap, 0, &cmp);
            top
        }
        ("remove", 1) => match (0..heap.len()).find(|&i| eq_call(vm, &args[0], &heap[i])) {
            Some(i) => {
                pq_remove_at(vm, &mut heap, i, &cmp);
                Value::bool(true)
            }
            None => return Some(Value::bool(false)),
        },
        ("contains", 1) => {
            return Some(Value::bool(
                (0..heap.len()).any(|i| eq_call(vm, &args[0], &heap[i])),
            ))
        }
        ("size", 0) => return Some(Value::Int(heap.len() as i64)),
        ("isEmpty", 0) => return Some(Value::bool(heap.is_empty())),
        ("clear", 0) => {
            heap.clear();
            Value::Undef
        }
        // `bulkRemove`: drop every element the predicate accepts, then
        // re-heapify what is left.
        ("removeIf", 1) => {
            let mut kept = Vec::with_capacity(heap.len());
            for v in &heap {
                let drop = invoke_closure(vm, &args[0], std::slice::from_ref(v));
                if pending() {
                    return Some(Value::Undef);
                }
                if !matches!(drop, Value::Bool(true)) {
                    kept.push(v.clone());
                }
            }
            if kept.len() == heap.len() {
                return Some(Value::bool(false));
            }
            heap = kept;
            pq_heapify(vm, &mut heap, &cmp);
            Value::bool(true)
        }
        ("iterator", 0) => {
            return Some(Value::Obj(heap_alloc(HostObj::PQIter {
                source: id,
                cursor: 0,
                last: None,
                forget: std::collections::VecDeque::new(),
                last_elt: None,
                exp_mods: mods,
            })))
        }
        ("comparator", 0) => return Some(if natural { Value::Undef } else { cmp }),
        // `PriorityQueue` inherits `Object`'s identity `equals`/`hashCode`.
        ("equals", 1) => return Some(Value::bool(matches!(&args[0], Value::Obj(o) if *o == id))),
        ("hashCode", 0) => return Some(Value::Int(i64::from(id))),
        _ => return None,
    };
    if pending() {
        return Some(Value::Undef);
    }
    pq_store(id, heap, true);
    Some(result)
}

/// `hasNext` / `next` / `remove` on a [`HostObj::PQIter`], following
/// `PriorityQueue.Itr` line for line — including the `forgetMeNot` elements a
/// `remove()` lifted into already-visited slots, which `next()` returns once
/// the array walk is done.
fn pq_iter_method(
    vm: &mut VM,
    recv: &Value,
    method: &str,
    argc: usize,
) -> Option<Result<Value, Fault>> {
    let Value::Obj(it) = recv else {
        return None;
    };
    let it = *it;
    let (source, mut cursor, mut last, mut forget, mut last_elt, mut exp_mods) =
        HEAP.with(|h| match h.borrow().get(it as usize) {
            Some(HostObj::PQIter {
                source,
                cursor,
                last,
                forget,
                last_elt,
                exp_mods,
            }) => Some((
                *source,
                *cursor,
                *last,
                forget.clone(),
                last_elt.clone(),
                *exp_mods,
            )),
            _ => None,
        })?;
    let (mut heap, cmp, _, mods) = pq_state(source)?;
    let result = match (method, argc) {
        ("hasNext", 0) => Ok(Value::bool(cursor < heap.len() || !forget.is_empty())),
        ("next", 0) => {
            if exp_mods != mods {
                return Some(Err(comodification()));
            }
            if cursor < heap.len() {
                last = Some(cursor);
                cursor += 1;
                Ok(heap[cursor - 1].clone())
            } else {
                last = None;
                last_elt = forget.pop_front();
                match &last_elt {
                    Some(v) => Ok(v.clone()),
                    None => Err(Fault::java("NoSuchElementException", String::new())),
                }
            }
        }
        ("remove", 0) => {
            if exp_mods != mods {
                return Some(Err(comodification()));
            }
            if let Some(at) = last.take() {
                match pq_remove_at(vm, &mut heap, at, &cmp) {
                    None => cursor -= 1,
                    Some(moved) => forget.push_back(moved),
                }
            } else if let Some(v) = last_elt.take() {
                // `removeEq`: the first slot holding this very element.
                if let Some(i) = heap.iter().position(|x| match (x, &v) {
                    (Value::Obj(a), Value::Obj(b)) => a == b,
                    _ => value_eq(x, &v),
                }) {
                    pq_remove_at(vm, &mut heap, i, &cmp);
                }
            } else {
                return Some(Err(Fault::java("IllegalStateException", String::new())));
            }
            pq_store(source, heap, true);
            exp_mods = mods + 1;
            Ok(Value::Undef)
        }
        _ => Err(Fault::internal(format!(
            "javars: unsupported Iterator method `{method}` with {argc} argument(s)"
        ))),
    };
    HEAP.with(|h| {
        if let Some(HostObj::PQIter {
            cursor: c,
            last: l,
            forget: f,
            last_elt: e,
            exp_mods: m,
            ..
        }) = h.borrow_mut().get_mut(it as usize)
        {
            *c = cursor;
            *l = last;
            *f = forget;
            *e = last_elt;
            *m = exp_mods;
        }
    });
    Some(result)
}

/// `AbstractList.hashCode` — `31 * result + e.hashCode()`, seeded at 1.
fn list_hash(items: &[Value]) -> i64 {
    items.iter().fold(1i32, |acc, e| {
        acc.wrapping_mul(31).wrapping_add(element_hash(e))
    }) as i64
}

/// `AbstractSet.hashCode` — the *sum* of the element hashes, so it does not
/// depend on iteration order (which is the point: two equal sets in different
/// orders must hash alike).
fn set_hash(items: &[Value]) -> i64 {
    items
        .iter()
        .fold(0i32, |acc, e| acc.wrapping_add(element_hash(e))) as i64
}

/// `AbstractMap.hashCode` — the sum of the entries', each being
/// `keyHash ^ valueHash` (`Map.Entry.hashCode`).
fn map_hash(entries: &[(Value, Value)]) -> i64 {
    entries.iter().fold(0i32, |acc, (k, v)| {
        acc.wrapping_add(element_hash(k) ^ element_hash(v))
    }) as i64
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
/// `table` is the bucket array's length as [`HashTable`] tracked it; 0 — a
/// container no history was replayed for, such as `Set.of` — falls back to the
/// smallest table the default sizing would hold `keys` in.
///
/// Two things are not modeled, and neither is reproducible in Java either: a bin
/// that treeifies (8 collisions in one bucket with a table of 64+) and a key
/// whose `hashCode` is the JVM identity hash. A key with no modeled hash keeps
/// insertion order.
fn hash_order(keys: &[Value], table: u32) -> Vec<usize> {
    let cap = match table {
        0 => hash_capacity(keys.len()),
        t => t as usize,
    };
    let mut idx: Vec<usize> = (0..keys.len()).collect();
    idx.sort_by_key(|&i| hash_bucket(&keys[i], cap));
    idx
}

/// The table size a `HashMap`/`HashSet` holding `n` entries has: the default 16,
/// doubled while the load factor would exceed 0.75.
fn hash_capacity(n: usize) -> usize {
    let mut cap = 16usize;
    while n > cap * 3 / 4 {
        cap *= 2;
    }
    cap
}

/// `HashMap`'s table sizing, replayed over a container's history so the bucket
/// order is read off the table the JDK would actually have.
///
/// The JDK never shrinks a table — not on `remove`, not on `clear` — and grows
/// it on two different schedules: `putVal` (`put`, `putIfAbsent`, `putAll`,
/// `HashSet.add`) resizes *after* the insertion that takes the size past the
/// threshold, while `computeIfAbsent`/`compute`/`merge` resize *before* any
/// work, whenever the size is already past it. A constructor capacity and
/// `putAll` into an empty map pre-size the first allocation. Each method below
/// is the corresponding JDK 21 code path, the load factor being the default
/// 0.75 (javars accepts no other).
#[derive(Clone, Copy)]
struct HashTable {
    table: u32,
    init: u32,
}

impl HashTable {
    const MAX: u32 = 1 << 30;
    /// `MIN_TREEIFY_CAPACITY`. A table this long never grows for a crowded
    /// bin, so no bin needs counting once one is reached.
    const MIN_TREEIFY: u32 = 64;

    /// `HashMap.threshold`: the initial capacity until a table exists, then
    /// `(int) (capacity * 0.75f)`.
    fn threshold(self) -> u64 {
        match self.table {
            0 => self.init as u64,
            t => t as u64 * 3 / 4,
        }
    }

    /// `HashMap.resize`: double the table, or allocate the pending initial
    /// capacity, or the default 16.
    fn resize(&mut self) {
        self.table = match (self.table, self.init) {
            (0, 0) => 16,
            (0, init) => init,
            (t, _) => (t * 2).min(Self::MAX),
        };
    }

    /// `HashMap.tableSizeFor`: the smallest power of two at least `cap`, and
    /// at least 1.
    fn size_for(cap: u64) -> u32 {
        cap.clamp(1, Self::MAX as u64).next_power_of_two() as u32
    }

    /// `new HashMap<>(n)`: the first table will hold `tableSizeFor(n)` buckets.
    fn with_capacity(n: u64) -> HashTable {
        HashTable {
            table: 0,
            init: Self::size_for(n),
        }
    }

    /// `putVal` adding a new key that makes the size `size`; `mates(table)`
    /// counts the keys already in the new key's bin under a table that long.
    ///
    /// `putVal` appends to a bin and calls `treeifyBin` when the bin already
    /// held `TREEIFY_THRESHOLD` (8) nodes; see [`HashTable::treeify`].
    fn put_new(&mut self, size: usize, mates: impl Fn(u32) -> usize) {
        if self.table == 0 {
            self.resize();
        }
        if self.table < Self::MIN_TREEIFY && mates(self.table) >= 8 {
            self.treeify();
        }
        if size as u64 > self.threshold() {
            self.resize();
        }
    }

    /// `computeIfAbsent`/`compute`/`merge` having added a key to a bin that
    /// held `mates` other nodes. They count the bin differently from `putVal`
    /// and call `treeifyBin` once it held 7 (`TREEIFY_THRESHOLD - 1`).
    fn compute_inserted(&mut self, mates: usize) {
        if mates >= 7 {
            self.treeify();
        }
    }

    /// `treeifyBin`: below `MIN_TREEIFY_CAPACITY` (64) a crowded bin is
    /// relieved by a resize rather than turned into a tree. At 64 and above the
    /// bin does become a tree, which reorders it; that is not modeled.
    fn treeify(&mut self) {
        if self.table < Self::MIN_TREEIFY {
            self.resize();
        }
    }

    /// The check `computeIfAbsent`/`compute`/`merge` open with, made whether or
    /// not the key turns out to be present; `size` is the size before the call.
    fn compute_pre(&mut self, size: usize) {
        if size as u64 > self.threshold() || self.table == 0 {
            self.resize();
        }
    }

    /// `putMapEntries` sizing for a source of `s` mappings: pre-size the first
    /// table to `ceil(s / 0.75)`, or resize an existing one until `s` fits.
    fn presize(&mut self, s: usize) {
        if s == 0 {
            return;
        }
        if self.table == 0 {
            let t = (s as u64 * 4).div_ceil(3);
            if t > self.init as u64 {
                self.init = Self::size_for(t);
            }
        } else {
            while s as u64 > self.threshold() && self.table < Self::MAX {
                self.resize();
            }
        }
    }
}

/// The table of a `HashMap`/`HashSet` and its current size; `None` for any
/// other receiver.
fn hash_table(v: &Value) -> Option<(HashTable, usize)> {
    let Value::Obj(id) = v else {
        return None;
    };
    HEAP.with(|h| match h.borrow().get(*id as usize) {
        Some(HostObj::Map {
            order: Order::Hash { table, init },
            entries,
            ..
        }) => Some((
            HashTable {
                table: *table,
                init: *init,
            },
            entries.len(),
        )),
        Some(HostObj::Set {
            order: Order::Hash { table, init },
            items,
            ..
        }) => Some((
            HashTable {
                table: *table,
                init: *init,
            },
            items.len(),
        )),
        _ => None,
    })
}

/// Write back a table [`hash_table`] read and the caller grew.
fn store_hash_table(v: &Value, t: HashTable) {
    let Value::Obj(id) = v else {
        return;
    };
    HEAP.with(|h| match h.borrow_mut().get_mut(*id as usize) {
        Some(HostObj::Map { order, .. } | HostObj::Set { order, .. }) => {
            if let Order::Hash { table, init } = order {
                *table = t.table;
                *init = t.init;
            }
        }
        _ => {}
    })
}

/// Replay `putVal`'s growth for the keys that took a container from `before`
/// entries to its current size.
///
/// A `put` appends a new key, so the keys this replays are the last
/// `after - before` in storage order, each inserted after every key before it.
fn hash_grow_put(v: &Value, before: usize) {
    let Some((mut t, after)) = hash_table(v) else {
        return;
    };
    if after <= before {
        return;
    }
    // Only a table below `MIN_TREEIFY_CAPACITY` reads its bins, and it holds
    // at most 48 keys, so the copy is small exactly when it is taken.
    let keys = match t.table < HashTable::MIN_TREEIFY {
        true => hash_keys(v),
        false => Vec::new(),
    };
    for size in before + 1..=after {
        t.put_new(size, |table| {
            bin_mates(&keys[..size - 1], &keys[size - 1], table)
        });
    }
    store_hash_table(v, t);
}

/// Replay the treeify check of a `computeIfAbsent`/`compute`/`merge` that
/// added `key` (see [`HashTable::compute_inserted`]).
fn hash_grow_compute(v: &Value, key: &Value) {
    let Some((mut t, _)) = hash_table(v) else {
        return;
    };
    if t.table >= HashTable::MIN_TREEIFY {
        return;
    }
    let keys = hash_keys(v);
    let others: Vec<Value> = keys.into_iter().filter(|k| !value_eq(k, key)).collect();
    t.compute_inserted(bin_mates(&others, key, t.table));
    store_hash_table(v, t);
}

/// How many of `keys` share `key`'s bin in a table of length `table`.
fn bin_mates(keys: &[Value], key: &Value, table: u32) -> usize {
    let bin = hash_bucket(key, table as usize);
    keys.iter()
        .filter(|k| hash_bucket(k, table as usize) == bin)
        .count()
}

/// A `HashMap`'s keys or a `HashSet`'s elements, in storage order.
fn hash_keys(v: &Value) -> Vec<Value> {
    let Value::Obj(id) = v else {
        return Vec::new();
    };
    HEAP.with(|h| match h.borrow().get(*id as usize) {
        Some(HostObj::Map { entries, .. }) => entries.iter().map(|(k, _)| k.clone()).collect(),
        Some(HostObj::Set { items, .. }) => items.clone(),
        _ => Vec::new(),
    })
}

/// Which bin `key` lands in for a table of `cap` — Java's
/// `(capacity - 1) & (h ^ (h >>> 16))`.
fn hash_bucket(key: &Value, cap: usize) -> usize {
    let h = java_hash(key).or_else(|| filed_hash(key)).unwrap_or(0) as u32;
    ((cap as u32 - 1) & (h ^ (h >> 16))) as usize
}

/// The hash [`file_key_hash`] recorded for a heap-object key, if any.
fn filed_hash(key: &Value) -> Option<i32> {
    let Value::Obj(id) = key else {
        return None;
    };
    FILED_HASH.with(|f| f.borrow().get(id).copied())
}

/// Record the hash a heap-object `key` is filed under as it enters a
/// `HashMap`/`HashSet`.
///
/// Java's `HashMap.putVal` calls `key.hashCode()` once, at insertion, and keeps
/// the answer in the node; the bucket order is decided by that stored number.
/// For a `String` or a boxed primitive [`java_hash`] reproduces it from the
/// value alone, but a `record`, a user class declaring `hashCode()`, a
/// collection used as a key, and a `Map.Entry` all hash through a body or
/// through their contents — which needs the VM, and the ordering readers
/// ([`present_order`]) run without one, often under a heap borrow. So the hash
/// is computed here, where an insertion still has the VM, exactly when the JDK
/// computes it. Before this, every such key landed in bucket 0, so a
/// `HashSet<Point>` iterated in insertion order instead of Java's.
///
/// The record is per object, not per container: a key mutated *after* being
/// filed and then filed again elsewhere moves in both. That needs a key whose
/// hash changes while it sits in a hash container, which the `HashMap` contract
/// already leaves undefined.
///
/// A key whose `hashCode` is the JVM identity hash (an array, a plain instance,
/// a `PriorityQueue`) is not filed and keeps insertion order — Java's order for
/// one is not reproducible from run to run either.
fn file_key_hash(vm: &mut VM, key: &Value) {
    let Value::Obj(id) = key else {
        return;
    };
    if java_hash(key).is_some() {
        return;
    }
    let structural = HEAP.with(|h| {
        matches!(
            h.borrow().get(*id as usize),
            Some(
                HostObj::List { .. }
                    | HostObj::SubList { .. }
                    | HostObj::Set { .. }
                    | HostObj::Map { .. }
                    | HostObj::Entry
            )
        )
    });
    let h = if structural {
        match collection_hash(vm, key, "hashCode", 0) {
            Some(Value::Int(h)) => Some(h as i32),
            _ => entry_pair(key).map(|_| element_hash(key)),
        }
    } else {
        user_element_hash(vm, key)
    };
    if let Some(h) = h {
        FILED_HASH.with(|f| f.borrow_mut().insert(*id, h));
    }
}

/// File the keys an insertion method is about to add to a hash container (see
/// [`file_key_hash`]): the first argument of the single-key methods, and every
/// element or key of the source of `addAll`/`putAll`.
fn file_inserted_keys(vm: &mut VM, method: &str, args: &[Value]) {
    let Some(first) = args.first() else {
        return;
    };
    match method {
        // `merge`/`compute`/`computeIfAbsent` insert through `put`, which
        // files the key then; filing it here as well would run a user
        // `hashCode()` twice for the one call Java makes.
        "add" | "put" | "putIfAbsent" => file_key_hash(vm, first),
        "addAll" => {
            for v in sequence_items(first).unwrap_or_default() {
                file_key_hash(vm, &v);
            }
        }
        "putAll" => {
            for (k, _) in map_entries(first).unwrap_or_default() {
                file_key_hash(vm, &k);
            }
        }
        _ => {}
    }
}

/// Move the entry for `key` to the head of its hash bin.
///
/// `HashMap.put` links a new node at the *tail* of its bin, but
/// `computeIfAbsent`, `compute` and `merge` all reach their insert through
/// `newNode(hash, key, value, first)`, which links it at the *head*. So a key
/// added by one of those three iterates *before* every key it collides with,
/// where the same key added by `put` iterates after them. Measured on openjdk
/// 21.0.12.1, inserting `three` into a `HashMap` already holding `one` and
/// `two` (`three` shares `two`'s bin):
///
///   put / putIfAbsent / putAll           {one=1, two=2, three=3}
///   computeIfAbsent / compute / merge    {one=1, three=3, two=2}
///
/// javars stores a map as one entry vector and derives the hash order from it
/// by a stable sort on bin index, so a bin's chain *is* that vector's order
/// restricted to the bin — which makes the JDK's head-insert a move of the new
/// entry to just before the first entry it collides with. The relative order
/// then survives a resize, as it does in Java, because a split preserves it.
///
/// Only a hash map has bins; an insertion-ordered or sorted map derives nothing
/// from the vector's order and is left alone.
fn hash_bucket_head_insert(recv: &Value, key: &Value) {
    if !matches!(map_order(recv), Order::Hash { .. }) {
        return;
    }
    let Some(entries) = map_entries(recv) else {
        return;
    };
    let Some(at) = entries.iter().rposition(|(k, _)| value_eq(k, key)) else {
        return;
    };
    // The bin is the one the key went into, in the table it went into — not
    // one sized after the fact, which a `computeIfAbsent` that left the size
    // past the threshold has not grown yet.
    let cap = match hash_table(recv) {
        Some((HashTable { table, .. }, _)) if table > 0 => table as usize,
        _ => hash_capacity(entries.len()),
    };
    let bin = hash_bucket(&entries[at].0, cap);
    let Some(first) = entries.iter().position(|(k, _)| hash_bucket(k, cap) == bin) else {
        return;
    };
    if first >= at {
        return;
    }
    let mut out = entries;
    let moved = out.remove(at);
    out.insert(first, moved);
    write_map_entries(recv, out);
}

/// Replace a map's entry vector wholesale and stale its key index, which the
/// next lookup rebuilds. The one caller is [`hash_bucket_head_insert`], which
/// reorders rather than adds.
fn write_map_entries(recv: &Value, entries: Vec<(Value, Value)>) {
    let Value::Obj(id) = recv else {
        return;
    };
    HEAP.with(|h| {
        if let Some(HostObj::Map {
            entries: dst,
            index,
            ..
        }) = h.borrow_mut().get_mut(*id as usize)
        {
            *dst = entries;
            index.invalidate();
        }
    });
}

/// The order `items` are presented in under `order`, as indices into `items`.
fn present_order(items: &[Value], order: Order) -> Vec<usize> {
    match order {
        Order::Insertion => (0..items.len()).collect(),
        Order::Hash { table, .. } => hash_order(items, table),
        Order::Sorted { by_cmp, desc } => {
            let mut idx: Vec<usize> = (0..items.len()).collect();
            if !by_cmp {
                idx.sort_by(|&a, &b| natural_cmp(&items[a], &items[b]));
            }
            if desc {
                idx.reverse();
            }
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
    // `Map.Entry.equals` is a *value* comparison — the key and the value both —
    // and it holds across implementations: a `HashMap$Node` equals the
    // `KeyValueHolder` `Map.entry` builds from the same pair, which is what
    // makes `m.entrySet().contains(Map.entry(k, v))` answer `true`. Only a pair
    // against a pair; an entry against anything else falls to the identity
    // comparison below and is `false`, as `Map.entry("a", 1).equals("a=1")` is.
    if let (Some(x), Some(y)) = (entry_pair(a), entry_pair(b)) {
        return value_eq(&x.key, &y.key) && value_eq(&x.value, &y.value);
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
    if let Some(same) = collection_equals(vm, q, other) {
        return same;
    }
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

/// Whether `v` is a `List`, `Set` or `Map` handle (a `subList` view included) —
/// the shapes whose `equals` is the JDK's structural `AbstractList`/
/// `AbstractSet`/`AbstractMap` comparison rather than identity.
fn is_collection_handle(v: &Value) -> bool {
    matches!(v, Value::Obj(_)) && eq_shape(v).is_some()
}

/// `q.equals(other)` when `q` is a collection: `None` for anything else.
///
/// The JDK's three abstract bases, each against its own kind only (a `List`
/// never equals a `Set`):
///
///   * `AbstractList.equals` — same length, and position by position
///     `o1 == null ? o2 == null : o1.equals(o2)` with this list's element as
///     the receiver.
///   * `AbstractSet.equals` — same size and `containsAll(other)`, which asks
///     `e.equals(x)` with each element `e` of the *other* set as the receiver.
///   * `AbstractMap.equals` — same size, and for every entry of this map the
///     other's `get(key)` (the lookup key as receiver) equal to the value,
///     a `null` value additionally requiring `containsKey`.
///
/// It runs outside any heap borrow, so nested collections and user `equals`
/// bodies recurse through [`eq_call`]. A stale `subList` view compares as not
/// equal rather than raising here.
fn collection_equals(vm: &mut VM, q: &Value, other: &Value) -> Option<bool> {
    let shape = eq_shape(q)?;
    if let (Value::Obj(x), Value::Obj(y)) = (q, other) {
        if x == y {
            return Some(true);
        }
    }
    let Some(other_shape) = eq_shape(other) else {
        return Some(false);
    };
    // A lookup inside a hash container runs the probe's `equals` only when its
    // `hashCode` agrees, which is what `trusted_equals` decides.
    let key_eq = |vm: &mut VM, k: &Value, x: &Value| {
        if matches!(k, Value::Undef) {
            matches!(x, Value::Undef)
        } else if trusted_equals(vm, k, true) {
            eq_call(vm, k, x)
        } else {
            value_eq(k, x)
        }
    };
    Some(match (shape, other_shape) {
        (EqShape::List, EqShape::List) => {
            let (Some(a), Some(b)) = (sequence_items(q), sequence_items(other)) else {
                return Some(false);
            };
            a.len() == b.len() && a.iter().zip(&b).all(|(x, y)| objects_equals(vm, x, y))
        }
        (EqShape::Set(_), EqShape::Set(_)) => {
            let (Some(a), Some(b)) = (sequence_items(q), sequence_items(other)) else {
                return Some(false);
            };
            a.len() == b.len() && b.iter().all(|e| a.iter().any(|x| key_eq(vm, e, x)))
        }
        (EqShape::Map(_), EqShape::Map(_)) => {
            let (Some(a), Some(b)) = (map_entries(q), map_entries(other)) else {
                return Some(false);
            };
            a.len() == b.len()
                && a.iter()
                    .all(|(k, v)| match b.iter().find(|(ok, _)| key_eq(vm, k, ok)) {
                        Some((_, ov)) => objects_equals(vm, v, ov),
                        None => false,
                    })
        }
        _ => false,
    })
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
/// The hash one element contributes when a *user* `hashCode()` body decides it.
///
/// Run before the heap borrow, for the same reason [`eq_plan`] resolves the
/// element comparisons there: the body reads its own fields and may allocate, so
/// it cannot run under an outstanding borrow of the slab. `None` means no body
/// is reachable and [`element_hash`] answers, which is the path every program
/// that declares no `hashCode` — and every collection of `String`s and boxed
/// primitives — takes.
fn user_element_hash(vm: &mut VM, v: &Value) -> Option<i32> {
    let Value::Obj(id) = v else {
        return None;
    };
    let class = instance_class(v)?;
    let entry = member_entry(vm, &class, HASHCODE_SUFFIX)?;
    if PENDING.with(|p| p.borrow().is_some()) {
        return None;
    }
    let stack_base = vm.stack.len();
    vm.stack.push(Value::Obj(*id));
    match run_sub(vm, entry, stack_base) {
        Value::Int(h) => Some(h as i32),
        _ => None,
    }
}

/// A collection's own `hashCode`, computed before the heap borrow so a user
/// element body can run.
///
/// `None` for a receiver that is not a collection, or a call that is not
/// `hashCode()`, which leaves the borrowed section to answer as before.
fn collection_hash(vm: &mut VM, recv: &Value, method: &str, argc: usize) -> Option<Value> {
    if method != "hashCode" || argc != 0 {
        return None;
    }
    let each = |vm: &mut VM, e: &Value| user_element_hash(vm, e).unwrap_or_else(|| element_hash(e));
    if let Some(entries) = map_entries(recv) {
        let mut h = 0i32;
        for (k, v) in &entries {
            let (kh, vh) = (each(vm, k), each(vm, v));
            h = h.wrapping_add(kh ^ vh);
        }
        return Some(Value::Int(h.into()));
    }
    let items = sequence_items(recv)?;
    // A set's hash is the order-independent sum; a list's is the `31 * h + e`
    // fold. `value_class` is what tells them apart, and it is the same question
    // `render_sequence` asks to choose between `[…]` and `{…}`.
    let is_set = value_class(recv).is_some_and(|c| c.contains("Set"));
    let mut h = if is_set { 0i32 } else { 1i32 };
    for e in &items {
        let eh = each(vm, e);
        h = if is_set {
            h.wrapping_add(eh)
        } else {
            h.wrapping_mul(31).wrapping_add(eh)
        };
    }
    Some(Value::Int(h.into()))
}

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
    matches!(order, Order::Hash { .. } | Order::Insertion)
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
    // `equals` against another collection is the JDK's structural comparison,
    // decided whole out here: it may recurse into nested collections and user
    // `equals` bodies, neither of which the borrowed section can reach. Against
    // anything that is not a collection it is `false`, which the borrowed
    // section answers itself.
    if method == "equals" && args.len() == 1 && is_collection_handle(&args[0]) {
        return collection_equals(vm, recv, &args[0]).map(EqPlan::Same);
    }
    // A collection *argument* compares structurally too (`list.contains(aList)`,
    // `map.get(aList)`), so it needs a plan even when no class declares `equals`.
    if !any_user_equals(vm) && !args.first().is_some_and(is_collection_handle) {
        return None;
    }
    let shape = eq_shape(recv)?;
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
    // A collection's `equals` is structural and its `hashCode` is derived from
    // the same contents, so a hash container consults it too.
    if is_collection_handle(v) {
        return true;
    }
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
    // Through any box: a `new String(…)` handle orders by its text, and a boxed
    // wrapper by its number, exactly as the values they hold do.
    let (a, b) = (&deboxed(a), &deboxed(b));
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
    // `new TreeMap<>(comparator)` / `new TreeSet<>(comparator)`: empty, and
    // ordered by the comparator from the first insertion on.
    if matches!(kind, "TreeMap" | "TreeSet") && is_callable(vm, seed) {
        let order = Order::Sorted {
            by_cmp: true,
            desc: false,
        };
        let obj = if kind == "TreeMap" {
            HostObj::Map {
                fixed: Fixity::Mutable,
                entries: Vec::new(),
                order,
                index: KeyIndex::default(),
            }
        } else {
            HostObj::Set {
                items: Vec::new(),
                order,
                fixed: Fixity::Mutable,
                view: SetView::Own,
                index: KeyIndex::default(),
            }
        };
        let id = heap_alloc(obj);
        SORT_CMP.with(|s| s.borrow_mut().insert(id, seed.clone()));
        return Ok(Value::Obj(id));
    }
    // A `TreeMap`/`TreeSet` copy of a collection holding a `null` key throws
    // as its first `null` insertion would (see [`natural_order_null`]).
    let null_key = match kind {
        "TreeSet" => {
            sequence_items(seed).is_some_and(|s| s.iter().any(|v| matches!(v, Value::Undef)))
        }
        "TreeMap" => {
            map_entries(seed).is_some_and(|e| e.iter().any(|(k, _)| matches!(k, Value::Undef)))
        }
        _ => false,
    };
    if null_key {
        return Err(Fault::java("NullPointerException", String::new()));
    }
    // A negative initial capacity is refused by every constructor that takes
    // one, in its class's own words; `ArrayDeque` alone accepts it.
    if let Value::Int(n @ ..0) = seed {
        let what = match kind {
            "ArrayList" | "List" => Some("Illegal Capacity"),
            "HashMap" | "Map" | "HashSet" | "Set" | "LinkedHashMap" | "LinkedHashSet" => {
                Some("Illegal initial capacity")
            }
            _ => None,
        };
        if let Some(what) = what {
            return Err(Fault::java(
                "IllegalArgumentException",
                format!("{what}: {n}"),
            ));
        }
    }
    let obj = match kind {
        // `ArrayDeque`/`Deque`/`Queue` join `LinkedList` on the mutable-list
        // shape. The `Deque` methods work on the same `Vec` (head at index 0),
        // and the element order every one of them produces is the reference's.
        // What this model does not carry is the *class*: like `LinkedList`
        // before it, an `ArrayDeque` answers `getClass().getSimpleName()` with
        // `ArrayList`, and it accepts a null element where the real
        // `ArrayDeque` throws. BUGS.md records both.
        "ArrayList" | "LinkedList" | "ArrayDeque" | "Deque" | "Queue" | "List" => HostObj::List {
            mods: 0,
            items: sequence_items(seed).unwrap_or_default(),
            fixed: Fixity::Mutable,
            view: None,
        },
        "HashMap" | "Map" => HostObj::Map {
            fixed: Fixity::Mutable,
            entries: map_entries(seed).unwrap_or_default(),
            order: Order::HASH,
            index: KeyIndex::default(),
        },
        "LinkedHashMap" => HostObj::Map {
            fixed: Fixity::Mutable,
            entries: map_entries(seed).unwrap_or_default(),
            order: Order::Insertion,
            index: KeyIndex::default(),
        },
        "TreeMap" => HostObj::Map {
            fixed: Fixity::Mutable,
            entries: map_entries(seed).unwrap_or_default(),
            order: Order::Sorted {
                by_cmp: false,
                desc: false,
            },
            index: KeyIndex::default(),
        },
        "HashSet" | "Set" => HostObj::Set {
            items: distinct(vm, &sequence_items(seed).unwrap_or_default()),
            order: Order::HASH,
            fixed: Fixity::Mutable,
            view: SetView::Own,
            index: KeyIndex::default(),
        },
        "LinkedHashSet" => HostObj::Set {
            items: distinct(vm, &sequence_items(seed).unwrap_or_default()),
            order: Order::Insertion,
            fixed: Fixity::Mutable,
            view: SetView::Own,
            index: KeyIndex::default(),
        },
        "TreeSet" => HostObj::Set {
            items: distinct(vm, &sequence_items(seed).unwrap_or_default()),
            order: Order::Sorted {
                by_cmp: false,
                desc: false,
            },
            fixed: Fixity::Mutable,
            view: SetView::Own,
            index: KeyIndex::default(),
        },
        other => {
            return Err(Fault::internal(format!(
                "javars: `{other}` is not a collection javars models"
            )))
        }
    };
    // The table a hash container starts with. `new HashMap<>(n)` and
    // `new HashSet<>(n)` size the first allocation to `tableSizeFor(n)`;
    // `new HashMap<>(m)` is a `putAll` into an empty map; `new HashSet<>(c)` is
    // `HashMap.newHashMap(max(c.size(), 12))` followed by an `add` per element.
    let mut table = None;
    if matches!(kind, "HashSet" | "Set" | "HashMap" | "Map") {
        if let Value::Int(n) = seed {
            table = Some(HashTable::with_capacity(*n as u64));
        }
    }
    if matches!(kind, "HashSet" | "Set") {
        if let Some(seq) = sequence_items(seed) {
            let n = seq.len().max(12) as u64;
            table = Some(HashTable::with_capacity((n * 4).div_ceil(3)));
            for v in &seq {
                file_key_hash(vm, v);
            }
        }
    } else if matches!(kind, "HashMap" | "Map") {
        if let Some(entries) = map_entries(seed) {
            let mut t = HashTable { table: 0, init: 0 };
            t.presize(entries.len());
            table = Some(t);
            for (k, _) in &entries {
                file_key_hash(vm, k);
            }
        }
    }
    let id = heap_alloc(obj);
    if let Some(t) = table {
        let v = Value::Obj(id);
        store_hash_table(&v, t);
        hash_grow_put(&v, 0);
    }
    // A copy of user objects that order themselves is ranked by their
    // `compareTo` from the start.
    if matches!(kind, "TreeMap" | "TreeSet") {
        rank_by_compareto(vm, id, seed);
    }
    Ok(Value::Obj(id))
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
            Some(HostObj::PQueue { items, .. }) => Some(items.clone()),
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
                    | HostObj::PQueue { .. }
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
    // `chars`/`codePoints` name a *second* heap object, so they are answered
    // before the exclusive borrow below is taken — allocating under it panics
    // the interpreter, which is why these two used to refuse outright and
    // `sb.chars().count()` was a hard error where Java answers `sb.length()`.
    // The values are the `String` arm's: javars stores a builder's text as
    // Unicode scalars, exactly as it stores a string's.
    if matches!((method, args.len()), ("chars", 0) | ("codePoints", 0)) {
        let text = HEAP.with(|h| match h.borrow().get(id as usize) {
            Some(HostObj::Builder { s, len, .. }) => Some(s.chars().take(*len).collect::<Vec<_>>()),
            _ => None,
        })?;
        return Some(Ok(stream_of(
            text.into_iter()
                .map(|c| Value::Int(i64::from(c as u32)))
                .collect(),
            StreamKind::Int,
        )));
    }
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
            // `append(CharSequence, start, end)`: the window `[start, end)` of
            // the sequence's text, a `null` sequence reading as `"null"`. The
            // window is checked against that text's length with the JDK's
            // `checkFromToIndex` wording.
            ("append", 3) => {
                let chars: Vec<char> = rendered[0].chars().collect();
                let (start, end) = (args[1].jint(), args[2].jint());
                if start < 0 || start > end || end > chars.len() as i64 {
                    return Err(Fault::java(
                        "IndexOutOfBoundsException",
                        format!(
                            "Range [{start}, {end}) out of bounds for length {}",
                            chars.len()
                        ),
                    ));
                }
                s.extend(&chars[start as usize..end as usize]);
                *count = len + (end - start) as usize;
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
            // `append(char[], int, int)` — the array's characters from
            // `offset`, `len` of them. Java checks the window against the
            // array's length and reports the same `IndexOutOfBoundsException`
            // a bad `String.valueOf(char[], int, int)` does.
            ("appendChars", 3) => {
                let chars: Vec<char> = rendered[0].chars().collect();
                let (off, n) = (args[1].jint(), args[2].jint());
                if off < 0 || n < 0 || off + n > chars.len() as i64 {
                    return Err(Fault::java(
                        "IndexOutOfBoundsException",
                        format!("offset {off}, count {n}, length {}", chars.len()),
                    ));
                }
                let text: String = chars[off as usize..(off + n) as usize].iter().collect();
                s.push_str(&text);
                *count = len + n as usize;
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
                    s.extend(std::iter::repeat_n('\0', n - len));
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
            // `AbstractStringBuilder.codePointAt`/`codePointBefore` check the
            // index they read — `index - 1` for `codePointBefore` — against
            // the count, so `codePointBefore(0)` reports index -1.
            ("codePointAt", 1) | ("codePointBefore", 1) => {
                let at = args[0].jint() - i64::from(method == "codePointBefore");
                match usize::try_from(at).ok().and_then(|i| char_at(s, i)) {
                    Some(c) if (at as usize) < len => Ok(Value::Int(i64::from(c as u32))),
                    _ => Err(Fault::java(
                        "StringIndexOutOfBoundsException",
                        format!("Index {at} out of bounds for length {len}"),
                    )),
                }
            }
            // Characters are stored as Unicode scalars, so every one in range
            // is one code point.
            ("codePointCount", 2) => {
                let (b, e) = (args[0].jint(), args[1].jint());
                if b < 0 || e > len as i64 || b > e {
                    return Err(Fault::java(
                        "IndexOutOfBoundsException",
                        format!("Range [{b}, {e}) out of bounds for length {len}"),
                    ));
                }
                Ok(Value::Int(e - b))
            }
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
    // `new PriorityQueue<>(…)` arrives with its two constructor slots and the
    // natural-order comparator the compiler synthesized.
    if kind == "PriorityQueue" {
        let at = |i: usize| args.get(i).cloned().unwrap_or(Value::Undef);
        return match new_priority_queue(vm, &seed, &at(2), &at(3)) {
            Ok(v) => v,
            Err(f) => raise(vm, f),
        };
    }
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

/// [`JARRAY_EXTEND`] — append this call's elements to the array beneath them.
fn b_array_extend(vm: &mut VM, argc: u8) -> Value {
    let mut args = pop_args(vm, argc);
    if args.is_empty() {
        return Value::Undef;
    }
    let rest = args.split_off(1);
    let arr = args.remove(0);
    if let Value::Obj(id) = arr {
        HEAP.with(|h| {
            if let Some(HostObj::Array(elems)) = h.borrow_mut().get_mut(id as usize) {
                elems.extend(rest);
            }
        });
    }
    arr
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
                    // Named by its qualified class, which [`binary_name`] passes
                    // through: none of these is a name a program declares.
                    HostObj::Reader(r) => r.kind.class_name().to_string(),
                    HostObj::Tokenizer(_) => "java.util.StringTokenizer".to_string(),
                    HostObj::Random(_) => "java.util.Random".to_string(),
                    HostObj::Stats(s) => format!("java.util.{}", s.class_name()),
                    HostObj::Bits(_) => "java.util.BitSet".to_string(),
                    HostObj::Atomic { kind, .. } => match kind {
                        AtomicKind::Int => "java.util.concurrent.atomic.AtomicInteger",
                        AtomicKind::Long => "java.util.concurrent.atomic.AtomicLong",
                        AtomicKind::Bool => "java.util.concurrent.atomic.AtomicBoolean",
                    }
                    .to_string(),
                    HostObj::RegexPattern { .. } => "java.util.regex.Pattern".to_string(),
                    HostObj::RegexMatcher(_) => "java.util.regex.Matcher".to_string(),
                    HostObj::Instance { class, .. } => class.clone(),
                    // The whole point of the box: `Integer` and `Long` are
                    // different classes for a value one `Value::Int` holds.
                    HostObj::Boxed => box_class(v)?.to_string(),
                    // Not a name a program can write, like the other internal
                    // shapes — it exists so an iterator carries supertypes.
                    HostObj::Iterator { bidi: false, .. } => "Iterator$of".to_string(),
                    HostObj::PQIter { .. } => "Iterator$of".to_string(),
                    HostObj::Iterator { bidi: true, .. } => "ListIterator$of".to_string(),
                    HostObj::Optional { class, .. } => (*class).to_string(),
                    HostObj::Stream { .. } => "Stream$of".to_string(),
                    HostObj::Collector { .. } => "Collector$of".to_string(),
                    HostObj::Array(_) => "[]".to_string(),
                    // The `values()` view is not a `List` at all in Java, so
                    // it is read before the fixity — which would otherwise call
                    // it an `ArrayList`.
                    HostObj::List { view: Some(of), .. } => format!("List$values${}", of.tag()),
                    // `Arrays.asList` and `List.of` are `List`s that are not
                    // `ArrayList`s, and a `subList` view is a third answer
                    // again — see the note in [`jdk_supers`].
                    HostObj::List { fixed, .. } => match fixed {
                        Fixity::Mutable => "ArrayList".to_string(),
                        Fixity::FixedSize => "List$fixed".to_string(),
                        Fixity::Immutable => "List$immutable".to_string(),
                    },
                    HostObj::SubList { .. } => "List$sub".to_string(),
                    HostObj::PQueue { .. } => "PriorityQueue".to_string(),
                    HostObj::Map { order, fixed, .. } => match (fixed, order) {
                        (Fixity::Immutable, _) => "Map$immutable".to_string(),
                        (_, Order::Hash { .. }) => "HashMap".to_string(),
                        (_, Order::Insertion) => "LinkedHashMap".to_string(),
                        (_, Order::Sorted { .. }) => "TreeMap".to_string(),
                    },
                    // `Set.of` is not a `HashSet`, exactly as `List.of` is not
                    // an `ArrayList`; without the fixity it answered to both.
                    // A `keySet`/`entrySet` view is a `Set` that is not a
                    // `HashSet` (nor any other set a program can construct):
                    // the JDK gives each map implementation its own private
                    // view class. The marker is read before the fixity so a
                    // view of an immutable map does not answer `Set$immutable`,
                    // which is `Set.of`'s class and not a view's.
                    HostObj::Set {
                        view: SetView::Keys(of),
                        ..
                    } => format!("Set$keys${}", of.tag()),
                    HostObj::Set {
                        view: SetView::Entries(of),
                        ..
                    } => format!("Set$entries${}", of.tag()),
                    HostObj::Set { order, fixed, .. } => match (fixed, order) {
                        (Fixity::Mutable | Fixity::FixedSize, Order::Hash { .. }) => {
                            "HashSet".to_string()
                        }
                        (Fixity::Mutable | Fixity::FixedSize, Order::Insertion) => {
                            "LinkedHashSet".to_string()
                        }
                        (Fixity::Mutable | Fixity::FixedSize, Order::Sorted { .. }) => {
                            "TreeSet".to_string()
                        }
                        (Fixity::Immutable, _) => "Set$immutable".to_string(),
                    },
                    // An entry is named for the map it came out of, and an
                    // ownerless one (`Map.entry(k, v)`) is a `KeyValueHolder` —
                    // which is also what an *immutable* map's entries are, so
                    // the two share `ViewOf::Immutable` here.
                    HostObj::Entry => match entry_pair(v) {
                        Some(Pair {
                            kind: PairKind::Simple,
                            ..
                        }) => "Entry$simple".to_string(),
                        Some(Pair {
                            kind: PairKind::SimpleImmutable,
                            ..
                        }) => "Entry$simpleImmutable".to_string(),
                        pair => format!("Entry${}", entry_view(pair.and_then(|p| p.owner)).tag()),
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

/// Pop a name operand (a method, class or type-tag `String` the compiler pushed
/// as a constant) without copying it.
///
/// The obvious spelling, `pop().map(|v| v.as_str_cow().into_owned())`, allocates
/// and frees a `String` on **every** dispatched call, because `into_owned`
/// copies out of the `Cow::Borrowed` the constant already provides. Returning
/// the `Value` instead lets the caller borrow through it: the name is read, not
/// kept. The empty-stack fallback allocates, which is unreachable — the
/// compiler emits the name operand with the call — and keeps the previous
/// behaviour there exactly (an empty name, not `null`).
fn pop_name(vm: &mut VM) -> Value {
    vm.stack.pop().unwrap_or_else(|| Value::str(String::new()))
}

/// [`JCOLL_DISPATCH`] — an instance method on a collection receiver.
fn b_coll_dispatch(vm: &mut VM, argc: u8) -> Value {
    let method_name = pop_name(vm);
    let n = argc.saturating_sub(2) as usize;
    let mut args = Vec::with_capacity(n);
    for _ in 0..n {
        args.push(vm.stack.pop().unwrap_or(Value::Undef));
    }
    args.reverse();
    let recv = vm.stack.pop().unwrap_or(Value::Undef);
    let method = method_name.as_str_cow();
    coll_method(vm, &recv, &method, &args)
}

/// Which key a `NavigableMap`/`NavigableSet` navigation method names.
#[derive(Clone, Copy)]
enum Nav {
    First,
    Last,
    /// The greatest key `<=` the probe.
    Floor,
    /// The least key `>=` the probe.
    Ceiling,
    /// The greatest key `<` the probe.
    Lower,
    /// The least key `>` the probe.
    Higher,
}

/// The `TreeMap`/`TreeSet` navigation methods: `firstKey`/`floorKey`/…,
/// `firstEntry`/`ceilingEntry`/…, `pollFirstEntry`/`pollLastEntry` on a map and
/// `first`/`floor`/…/`pollFirst`/`pollLast` on a set. `None` for any other
/// method or any receiver that is not a sorted collection of its own, so a
/// `Deque`'s `pollFirst` never reaches here.
///
/// A sorted collection keeps its keys in insertion order and presents them
/// through [`natural_cmp`], so the answer is found in that same order. The
/// JDK's contract, measured on openjdk 27: `firstKey`/`lastKey` and a set's
/// `first`/`last` throw `NoSuchElementException` on an empty collection where
/// every other method answers `null`; a `null` probe is a
/// `NullPointerException` once there is a key to compare it with, and `null` on
/// an empty collection, which compares nothing. An `…Entry` answer is a
/// snapshot pair whose `setValue` is refused, as Java's is.
fn navigate(vm: &mut VM, recv: &Value, method: &str, args: &[Value]) -> Option<Value> {
    let Value::Obj(id) = recv else {
        return None;
    };
    let (nav, map_form, entry, poll) = match (method, args.len()) {
        ("firstKey", 0) => (Nav::First, true, false, false),
        ("lastKey", 0) => (Nav::Last, true, false, false),
        ("floorKey", 1) => (Nav::Floor, true, false, false),
        ("ceilingKey", 1) => (Nav::Ceiling, true, false, false),
        ("lowerKey", 1) => (Nav::Lower, true, false, false),
        ("higherKey", 1) => (Nav::Higher, true, false, false),
        ("firstEntry", 0) => (Nav::First, true, true, false),
        ("lastEntry", 0) => (Nav::Last, true, true, false),
        ("floorEntry", 1) => (Nav::Floor, true, true, false),
        ("ceilingEntry", 1) => (Nav::Ceiling, true, true, false),
        ("lowerEntry", 1) => (Nav::Lower, true, true, false),
        ("higherEntry", 1) => (Nav::Higher, true, true, false),
        ("pollFirstEntry", 0) => (Nav::First, true, true, true),
        ("pollLastEntry", 0) => (Nav::Last, true, true, true),
        ("first", 0) => (Nav::First, false, false, false),
        ("last", 0) => (Nav::Last, false, false, false),
        ("floor", 1) => (Nav::Floor, false, false, false),
        ("ceiling", 1) => (Nav::Ceiling, false, false, false),
        ("lower", 1) => (Nav::Lower, false, false, false),
        ("higher", 1) => (Nav::Higher, false, false, false),
        ("pollFirst", 0) => (Nav::First, false, false, true),
        ("pollLast", 0) => (Nav::Last, false, false, true),
        _ => return None,
    };
    // A descending copy navigates its ascending storage the other way round:
    // its first is the source's last, its floor the source's ceiling.
    let desc = HEAP.with(|h| match h.borrow().get(*id as usize) {
        Some(HostObj::Map { order, .. }) | Some(HostObj::Set { order, .. }) => {
            matches!(order, Order::Sorted { desc: true, .. })
        }
        _ => false,
    });
    let nav = if desc {
        match nav {
            Nav::First => Nav::Last,
            Nav::Last => Nav::First,
            Nav::Floor => Nav::Ceiling,
            Nav::Ceiling => Nav::Floor,
            Nav::Lower => Nav::Higher,
            Nav::Higher => Nav::Lower,
        }
    } else {
        nav
    };
    let pairs: Vec<(Value, Value)> = HEAP.with(|h| match h.borrow().get(*id as usize) {
        Some(HostObj::Map {
            entries,
            order: Order::Sorted { .. },
            ..
        }) if map_form => Some(entries.clone()),
        Some(HostObj::Set {
            items,
            order: Order::Sorted { .. },
            view: SetView::Own,
            ..
        }) if !map_form => Some(items.iter().map(|k| (k.clone(), Value::Undef)).collect()),
        _ => None,
    })?;
    use std::cmp::Ordering::{Greater, Less};
    // A comparator-ordered collection is stored in its comparator's order, so
    // a position *is* its rank there, and the probe is compared by calling the
    // comparator — `compare(probe, key)`, the operand order `TreeMap` uses.
    let ranked = SORT_CMP.with(|s| s.borrow().get(id).cloned());
    if ranked.is_none()
        && !args.is_empty()
        && matches!(deboxed(&args[0]), Value::Undef)
        && !pairs.is_empty()
    {
        return Some(raise(
            vm,
            Fault::java(
                "NullPointerException",
                "Cannot invoke \"java.lang.Comparable.compareTo(Object)\" because \"k1\" is null"
                    .to_string(),
            ),
        ));
    }
    // Each key's order against the probe (`key` vs `probe`).
    let mut vs_probe = Vec::with_capacity(pairs.len());
    if !args.is_empty() {
        for p in &pairs {
            vs_probe.push(match &ranked {
                Some(cmp) => match rank_compare(vm, cmp, &args[0], &p.0) {
                    Some(c) => 0.cmp(&c),
                    None => return Some(Value::Undef),
                },
                None => natural_cmp(&p.0, &args[0]),
            });
        }
    }
    let better = |cand: usize, best: usize, want_max: bool| {
        let o = match ranked {
            Some(_) => cand.cmp(&best),
            None => natural_cmp(&pairs[cand].0, &pairs[best].0),
        };
        if want_max {
            o == Greater
        } else {
            o == Less
        }
    };
    // `First`/`Last` take no probe and compare against none.
    vs_probe.resize(pairs.len(), std::cmp::Ordering::Equal);
    let mut found: Option<usize> = None;
    for (i, &ord) in vs_probe.iter().enumerate() {
        let (fits, want_max) = match nav {
            Nav::First => (true, false),
            Nav::Last => (true, true),
            Nav::Floor => (ord != Greater, true),
            Nav::Ceiling => (ord != Less, false),
            Nav::Lower => (ord == Less, true),
            Nav::Higher => (ord == Greater, false),
        };
        if fits && found.is_none_or(|b| better(i, b, want_max)) {
            found = Some(i);
        }
    }
    let Some((key, value)) = found.map(|i| pairs[i].clone()) else {
        let throws = !entry && !poll && matches!(nav, Nav::First | Nav::Last);
        return Some(if throws {
            raise(vm, Fault::java("NoSuchElementException", String::new()))
        } else {
            Value::Undef
        });
    };
    if poll {
        coll_method(vm, recv, "remove", std::slice::from_ref(&key));
    }
    Some(if entry {
        alloc_entry(key, value, None)
    } else {
        key
    })
}

/// Evaluate `recv.method(args)` on a collection. A removal through a map's
/// `keySet()`/`entrySet()`/`values()` is carried back to the map — see
/// [`write_through`].
fn coll_method(vm: &mut VM, recv: &Value, method: &str, args: &[Value]) -> Value {
    if let Some(f) = refused_view_write(recv, method) {
        return raise(vm, f);
    }
    if naturally_ordered(recv) {
        if let Some(out) = natural_order_null(vm, recv, method, args) {
            return out;
        }
    }
    let through = matches!(
        method,
        "remove" | "removeObject" | "removeIf" | "removeAll" | "retainAll" | "clear"
    )
    .then(|| map_view_snapshot(recv))
    .flatten();
    // A hash container's table grows on the schedule of the JDK method that
    // inserted (see [`HashTable`]). `merge`/`compute`/`computeIfAbsent` insert
    // through `put`, so the receiver is marked while one runs and that inner
    // `put` leaves the table to the outer call's own schedule.
    let computing = matches!(method, "merge" | "compute" | "computeIfAbsent");
    let grow = hash_table(recv).filter(|_| !in_compute(recv));
    if let Some((mut t, size)) = grow {
        file_inserted_keys(vm, method, args);
        if computing {
            t.compute_pre(size);
        } else if method == "putAll" {
            t.presize(args.first().and_then(map_entries).map_or(0, |e| e.len()));
        }
        store_hash_table(recv, t);
    }
    let marked = computing && grow.is_some();
    if marked {
        if let Value::Obj(id) = recv {
            COMPUTING.with(|c| c.borrow_mut().push(*id));
        }
    }
    let out = coll_method_ranked(vm, recv, method, args);
    if marked {
        COMPUTING.with(|c| c.borrow_mut().pop());
        if hash_table(recv).is_some_and(|(_, now)| now > grow.map_or(0, |(_, s)| s)) {
            hash_grow_compute(recv, &args[0]);
        }
    } else if let Some((_, size)) = grow {
        hash_grow_put(recv, size);
    }
    if let Some((map, before)) = through {
        write_through(vm, map, recv, before);
    }
    out
}

/// Insert a new `key` the way `computeIfAbsent` does once its table check has
/// run: through `put`, with the table left to the caller, then moved to the
/// head of its hash bin.
fn put_as_compute(vm: &mut VM, map: &Value, key: Value, value: Value) {
    let Value::Obj(id) = map else {
        return;
    };
    COMPUTING.with(|c| c.borrow_mut().push(*id));
    coll_method(vm, map, "put", &[key.clone(), value]);
    COMPUTING.with(|c| c.borrow_mut().pop());
    hash_bucket_head_insert(map, &key);
    hash_grow_compute(map, &key);
}

/// True when `v` is a `TreeMap`/`TreeSet` with no comparator — ordered by its
/// keys' own `compareTo`.
fn naturally_ordered(v: &Value) -> bool {
    let Value::Obj(id) = v else {
        return false;
    };
    HEAP.with(|h| {
        matches!(
            h.borrow().get(*id as usize),
            Some(
                HostObj::Map {
                    order: Order::Sorted { by_cmp: false, .. },
                    ..
                } | HostObj::Set {
                    order: Order::Sorted { by_cmp: false, .. },
                    ..
                }
            )
        )
    })
}

/// A `null` key reaching a `TreeMap`/`TreeSet` that has no comparator.
///
/// The JDK cannot place it: `TreeMap.put` compares it (`compare(key, key)` on
/// an empty map, `Objects.requireNonNull(key)` otherwise) and `getEntry` — which
/// `get`, `containsKey`, `remove`, `replace` and `getOrDefault` share — requires
/// it non-null, so every one of them is a `NullPointerException`, on an empty
/// map too. `addAll`/`putAll` from an unsorted source insert one element at a
/// time, so the elements ahead of the `null` are added before it throws.
///
/// `None` lets the call proceed; `Some` is the call's whole answer.
fn natural_order_null(vm: &mut VM, recv: &Value, method: &str, args: &[Value]) -> Option<Value> {
    let npe = |vm: &mut VM| raise(vm, Fault::java("NullPointerException", String::new()));
    match method {
        "put" | "get" | "containsKey" | "remove" | "removeObject" | "putIfAbsent" | "merge"
        | "compute" | "computeIfAbsent" | "computeIfPresent" | "getOrDefault" | "replace"
        | "add" | "contains" => matches!(args.first(), Some(Value::Undef)).then(|| npe(vm)),
        "addAll" => {
            let items = sequence_items(args.first()?)?;
            let at = items.iter().position(|v| matches!(v, Value::Undef))?;
            for v in &items[..at] {
                coll_method(vm, recv, "add", std::slice::from_ref(v));
            }
            Some(npe(vm))
        }
        "putAll" => {
            let entries = map_entries(args.first()?)?;
            let at = entries
                .iter()
                .position(|(k, _)| matches!(k, Value::Undef))?;
            for (k, v) in &entries[..at] {
                coll_method(vm, recv, "put", &[k.clone(), v.clone()]);
            }
            Some(npe(vm))
        }
        _ => None,
    }
}

/// True while a `merge`/`compute`/`computeIfAbsent` on `recv` is running.
fn in_compute(recv: &Value) -> bool {
    let Value::Obj(id) = recv else {
        return false;
    };
    COMPUTING.with(|c| c.borrow().contains(id))
}

/// The map a `keySet()`/`entrySet()`/`values()` result was taken from, and the
/// elements the view holds right now — `None` for any other receiver.
fn map_view_snapshot(recv: &Value) -> Option<(u32, Vec<Value>)> {
    let Value::Obj(id) = recv else {
        return None;
    };
    let (map, _) = MAP_VIEWS.with(|m| {
        let m = m.borrow();
        if m.is_empty() {
            None
        } else {
            m.get(id).copied()
        }
    })?;
    Some((map, view_items(recv)?))
}

/// Remember that `view` (a fresh `keySet()`/`entrySet()`/`values()` result)
/// was taken from `map`, so a removal through it reaches the map.
fn register_map_view(view: &Value, map: u32) {
    if let Value::Obj(v) = view {
        MAP_VIEWS.with(|m| m.borrow_mut().insert(*v, (map, false)));
    }
}

/// The elements a view holds, in the order it presents them: a map view's
/// keys, any other view's elements.
fn view_items(view: &Value) -> Option<Vec<Value>> {
    if let Value::Obj(id) = view {
        let keys = HEAP.with(|h| match h.borrow().get(*id as usize) {
            Some(HostObj::Map { entries, order, .. }) => {
                let ks: Vec<Value> = entries.iter().map(|(k, _)| k.clone()).collect();
                Some(
                    present_order(&ks, *order)
                        .into_iter()
                        .map(|i| ks[i].clone())
                        .collect(),
                )
            }
            _ => None,
        });
        if keys.is_some() {
            return keys;
        }
    }
    sequence_items(view)
}

/// A write a navigable copy cannot carry back: javars models `headMap`,
/// `subSet`, `descendingMap` and their kin as copies whose *removals* reach
/// the source (see [`write_through`]) but whose insertions would not, so an
/// insertion through one is refused rather than silently lost.
fn refused_view_write(recv: &Value, method: &str) -> Option<Fault> {
    let Value::Obj(id) = recv else {
        return None;
    };
    let navigable = MAP_VIEWS.with(|m| m.borrow().get(id).is_some_and(|(_, nav)| *nav));
    (navigable
        && matches!(
            method,
            "put"
                | "putAll"
                | "putIfAbsent"
                | "merge"
                | "compute"
                | "computeIfAbsent"
                | "computeIfPresent"
                | "replace"
                | "replaceAll"
                | "add"
                | "addAll"
        ))
    .then(|| {
        Fault::internal(format!(
            "javars: `{method}` through a navigable range or descending view is not modeled"
        ))
    })
}

/// Carry a removal through a map view back to the map.
///
/// javars models the three views as copies (BUGS.md), so a held view still
/// does not see later changes to the map. What it does now do is what makes
/// the views worth calling for their own sake: `m.keySet().removeIf(…)`,
/// `m.entrySet().removeIf(e -> …)`, `m.values().remove(v)`,
/// `m.keySet().retainAll(c)`, and `it.remove()` on a view's iterator all
/// remove the entries from the map, where they used to leave it untouched.
///
/// Every one of those calls only *deletes* view elements, so what is left is a
/// subsequence of `before`. Matching it from the end recovers which elements
/// went — from the end, so that of two equal values the *first* is the one
/// `values().remove(v)` took, as `AbstractCollection.remove` takes the first
/// its iterator meets. A key set or entry set names its keys directly; a
/// removed value names the first entry, in the map's own order, that holds it.
fn write_through(vm: &mut VM, map: u32, view: &Value, before: Vec<Value>) {
    if pending() {
        return;
    }
    let Some(now) = view_items(view) else {
        return;
    };
    if now.len() >= before.len() {
        return;
    }
    let mut left = now.len();
    let mut gone = Vec::with_capacity(before.len() - now.len());
    for v in before.iter().rev() {
        if left > 0 && value_eq(v, &now[left - 1]) {
            left -= 1;
        } else {
            gone.push(v.clone());
        }
    }
    if left != 0 {
        return;
    }
    gone.reverse();
    let keyed = HEAP.with(|h| match h.borrow().get(view_handle(view)) {
        Some(HostObj::Set {
            view: SetView::Keys(_),
            ..
        }) => Some(false),
        Some(HostObj::Set {
            view: SetView::Entries(_),
            ..
        }) => Some(true),
        // A navigable copy holds its source's own keys or elements.
        Some(HostObj::Map { .. })
        | Some(HostObj::Set {
            view: SetView::Own, ..
        }) => Some(false),
        _ => None,
    });
    let keys: Vec<Value> = match keyed {
        Some(false) => gone,
        Some(true) => gone
            .iter()
            .filter_map(|e| entry_pair(e).map(|p| p.key))
            .collect(),
        None => {
            // `values()`: each removed value takes the first entry still
            // holding it, in the order the map iterates in.
            let Some(entries) = map_entries(&Value::Obj(map)) else {
                return;
            };
            let order = HEAP.with(|h| match h.borrow().get(map as usize) {
                Some(HostObj::Map { order, .. }) => Some(*order),
                _ => None,
            });
            let Some(order) = order else {
                return;
            };
            let ks: Vec<Value> = entries.iter().map(|(k, _)| k.clone()).collect();
            let mut taken = vec![false; entries.len()];
            let mut keys = Vec::with_capacity(gone.len());
            let walk = present_order(&ks, order);
            for v in &gone {
                if let Some(&i) = walk
                    .iter()
                    .find(|&&i| !taken[i] && value_eq(&entries[i].1, v))
                {
                    taken[i] = true;
                    keys.push(entries[i].0.clone());
                }
            }
            keys
        }
    };
    // Through the wrapper, so a removal through a view of a view (a
    // `headMap(k).keySet()`) carries on to the collection under both.
    let map = Value::Obj(map);
    for k in keys {
        coll_method(vm, &map, "remove", std::slice::from_ref(&k));
        if pending() {
            return;
        }
    }
}

/// `NavigableMap`/`NavigableSet`'s range and descending views on a `TreeMap`
/// or `TreeSet`: `headMap`/`tailMap`/`subMap` (both arities), `headSet`/
/// `tailSet`/`subSet`, `descendingMap`/`descendingSet`, and
/// `navigableKeySet`/`descendingKeySet`. `None` for any other method or
/// receiver.
///
/// Java's are live views; javars builds a copy of the entries in range, in the
/// source's order (reversed for the descending ones), registered with the
/// source so that a *removal* through it — `remove`, `clear`, `pollFirst…`,
/// `removeIf`, an iterator's `remove` — reaches the source as it does in Java.
/// An insertion through one is refused by [`refused_view_write`] rather than
/// silently kept from the source, and a copy does not see a later change to
/// the source. A range copy stays sorted by the source's comparator; a
/// descending copy keeps its reversed order as an insertion-ordered
/// collection, so it iterates, prints and queries as Java's does.
///
/// The bounds follow `TreeMap`'s rules: `head` excludes its key unless told
/// otherwise, `tail` includes it, `sub` is `[from, to)`; a `null` bound is a
/// `NullPointerException` and `from > to` an `IllegalArgumentException`
/// (`fromKey > toKey`).
fn navigable_view(vm: &mut VM, recv: &Value, method: &str, args: &[Value]) -> Option<Value> {
    let Value::Obj(id) = recv else {
        return None;
    };
    let id = *id;
    let is_map = HEAP.with(|h| match h.borrow().get(id as usize) {
        Some(HostObj::Map {
            order: Order::Sorted { .. },
            ..
        }) => Some(true),
        Some(HostObj::Set {
            order: Order::Sorted { .. },
            view: SetView::Own,
            ..
        }) => Some(false),
        _ => None,
    })?;
    let flag = |v: &Value| matches!(v, Value::Bool(true));
    type Bound = Option<(Value, bool)>;
    // (low bound, high bound, descending, keys only)
    let (lo, hi, descending, keys_only): (Bound, Bound, bool, bool) = match (is_map, method, args) {
        (true, "headMap", [k]) | (false, "headSet", [k]) => {
            (None, Some((k.clone(), false)), false, false)
        }
        (true, "headMap", [k, i]) | (false, "headSet", [k, i]) => {
            (None, Some((k.clone(), flag(i))), false, false)
        }
        (true, "tailMap", [k]) | (false, "tailSet", [k]) => {
            (Some((k.clone(), true)), None, false, false)
        }
        (true, "tailMap", [k, i]) | (false, "tailSet", [k, i]) => {
            (Some((k.clone(), flag(i))), None, false, false)
        }
        (true, "subMap", [a, b]) | (false, "subSet", [a, b]) => (
            Some((a.clone(), true)),
            Some((b.clone(), false)),
            false,
            false,
        ),
        (true, "subMap", [a, ai, b, bi]) | (false, "subSet", [a, ai, b, bi]) => (
            Some((a.clone(), flag(ai))),
            Some((b.clone(), flag(bi))),
            false,
            false,
        ),
        (true, "descendingMap", []) | (false, "descendingSet", []) => (None, None, true, false),
        (true, "navigableKeySet", []) => (None, None, false, true),
        (true, "descendingKeySet", []) => (None, None, true, true),
        _ => return None,
    };
    for (b, _) in lo.iter().chain(hi.iter()) {
        if matches!(deboxed(b), Value::Undef) {
            return Some(raise(
                vm,
                Fault::java("NullPointerException", String::new()),
            ));
        }
    }
    let ranked = SORT_CMP.with(|s| s.borrow().get(&id).cloned());
    // A view of a descending copy compares the way the copy presents: a
    // `descendingMap().headMap(k)` is the keys *above* `k`.
    let (by_cmp, src_desc) = HEAP.with(|h| match h.borrow().get(id as usize) {
        Some(HostObj::Map {
            order: Order::Sorted { by_cmp, desc },
            ..
        })
        | Some(HostObj::Set {
            order: Order::Sorted { by_cmp, desc },
            ..
        }) => (*by_cmp, *desc),
        _ => (false, false),
    });
    let sign = if src_desc { -1 } else { 1 };
    let compare = |vm: &mut VM, a: &Value, b: &Value| -> Option<i64> {
        let c = match &ranked {
            Some(c) => rank_compare(vm, c, a, b)?,
            None => natural_cmp(a, b) as i64,
        };
        Some(c.signum() * sign)
    };
    if let (Some((a, _)), Some((b, _))) = (&lo, &hi) {
        if compare(vm, a, b)? > 0 {
            return Some(raise(
                vm,
                Fault::java("IllegalArgumentException", "fromKey > toKey".to_string()),
            ));
        }
    }
    // The source's pairs in its presentation order (a set's values are unit).
    let (pairs, _, fixed) = HEAP.with(|h| match h.borrow().get(id as usize) {
        Some(HostObj::Map {
            entries,
            order,
            fixed,
            ..
        }) => {
            let ks: Vec<Value> = entries.iter().map(|(k, _)| k.clone()).collect();
            let pairs = present_order(&ks, *order)
                .into_iter()
                .map(|i| entries[i].clone())
                .collect::<Vec<_>>();
            (pairs, *order, *fixed)
        }
        Some(HostObj::Set {
            items,
            order,
            fixed,
            ..
        }) => {
            let pairs = present_order(items, *order)
                .into_iter()
                .map(|i| (items[i].clone(), Value::Undef))
                .collect::<Vec<_>>();
            (pairs, *order, *fixed)
        }
        _ => (Vec::new(), Order::Insertion, Fixity::Mutable),
    });
    let mut kept = Vec::with_capacity(pairs.len());
    for (k, v) in pairs {
        if let Some((b, incl)) = &lo {
            let c = compare(vm, &k, b)?;
            if c < 0 || (c == 0 && !incl) {
                continue;
            }
        }
        if let Some((b, incl)) = &hi {
            let c = compare(vm, &k, b)?;
            if c > 0 || (c == 0 && !incl) {
                continue;
            }
        }
        kept.push((k, v));
    }
    // `kept` is in the order the view presents only when the view and its
    // source face the same way; storage is always ascending, with the
    // direction carried by the order's `desc` flag.
    if src_desc {
        kept.reverse();
    }
    let order = Order::Sorted {
        by_cmp,
        desc: src_desc != descending,
    };
    let obj = if is_map && !keys_only {
        HostObj::Map {
            entries: kept,
            order,
            fixed,
            index: KeyIndex::default(),
        }
    } else {
        HostObj::Set {
            items: kept.into_iter().map(|(k, _)| k).collect(),
            order,
            fixed,
            view: SetView::Own,
            index: KeyIndex::default(),
        }
    };
    let view = heap_alloc(obj);
    if let Some(c) = ranked {
        SORT_CMP.with(|s| s.borrow_mut().insert(view, c));
    }
    MAP_VIEWS.with(|m| m.borrow_mut().insert(view, (id, true)));
    Some(Value::Obj(view))
}

/// The collection an [`HostObj::Iterator`] walks.
fn iterator_source(v: &Value) -> Option<Value> {
    let Value::Obj(id) = v else {
        return None;
    };
    HEAP.with(|h| match h.borrow().get(*id as usize) {
        Some(HostObj::Iterator { source, .. }) => Some(Value::Obj(*source)),
        _ => None,
    })
}

/// `Object.clone()` for a class instance: a shallow copy, or the JDK's
/// `CloneNotSupportedException` (its message the class name) when no
/// supertype is `Cloneable`. `None` for any other receiver.
fn instance_clone(recv: &Value) -> Option<Result<Value, Fault>> {
    let Value::Obj(id) = recv else {
        return None;
    };
    let (class, fields) = HEAP.with(|h| match h.borrow().get(*id as usize) {
        Some(HostObj::Instance { class, fields }) => Some((class.clone(), fields.clone())),
        _ => None,
    })?;
    let mut cloneable = false;
    walk_supertypes(&class, &mut |c| {
        cloneable = c == "Cloneable";
        cloneable
    });
    if !cloneable {
        return Some(Err(Fault::java(
            "CloneNotSupportedException",
            qualified_or_binary(&class),
        )));
    }
    Some(Ok(Value::Obj(heap_alloc(HostObj::Instance {
        class,
        fields,
    }))))
}

/// Whether heap slot `id` holds a map's `values()` view.
fn is_values_view(id: u32) -> bool {
    HEAP.with(|h| {
        matches!(
            h.borrow().get(id as usize),
            Some(HostObj::List { view: Some(_), .. })
        )
    })
}

/// Whether heap slot `id` holds a `Map`.
fn is_map_handle(id: usize) -> bool {
    HEAP.with(|h| matches!(h.borrow().get(id), Some(HostObj::Map { .. })))
}

/// The heap slot a handle names, or one past any real slot for a non-handle.
fn view_handle(v: &Value) -> usize {
    match v {
        Value::Obj(id) => *id as usize,
        _ => usize::MAX,
    }
}

/// Evaluate `recv.method(args)` on a collection, then restore a
/// comparator-ordered `TreeMap`/`TreeSet` to its comparator's order.
fn coll_method_ranked(vm: &mut VM, recv: &Value, method: &str, args: &[Value]) -> Value {
    if let (Value::Obj(id), Some(first)) = (recv, args.first()) {
        rank_by_compareto(vm, *id, first);
    }
    let ranked = match recv {
        Value::Obj(id) => SORT_CMP
            .with(|s| s.borrow().get(id).cloned())
            .map(|c| (*id, c)),
        _ => None,
    };
    let Some((id, cmp)) = ranked else {
        return coll_method_unranked(vm, recv, method, args);
    };
    let keys = stored_keys(id);
    let before = keys.len();
    // A sorted collection locates a key by its order, not by `equals`: the key
    // the comparator calls equal to the argument *is* the argument's key. So
    // that key stands in for the argument, and the lookup below — which
    // compares by value — finds exactly the entry `TreeMap.getEntry` would.
    let keyed = matches!(
        method,
        "contains"
            | "containsKey"
            | "get"
            | "getOrDefault"
            | "remove"
            | "add"
            | "put"
            | "putIfAbsent"
            | "merge"
            | "compute"
            | "computeIfAbsent"
            | "computeIfPresent"
            | "replace"
    );
    let mut args = args.to_vec();
    if keyed && !args.is_empty() && !keys.is_empty() {
        let (mut lo, mut hi) = (0, keys.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            let Some(c) = rank_compare(vm, &cmp, &args[0], &keys[mid]) else {
                return Value::Undef;
            };
            match c {
                0 => {
                    args[0] = keys[mid].clone();
                    break;
                }
                n if n < 0 => hi = mid,
                _ => lo = mid + 1,
            }
        }
    }
    let out = coll_method_unranked(vm, recv, method, &args);
    if !pending() {
        rerank(vm, id, &cmp, before);
    }
    out
}

/// Turn a natural-order `TreeMap`/`TreeSet` into one ranked by its keys' own
/// `compareTo` the first time a call hands it a user object that declares one
/// (`arg` itself, or an element of a collection `arg` — `addAll`, `putAll`).
///
/// [`natural_cmp`] orders the values javars models and calls every pair of
/// user objects equal, so a `TreeSet` of a `Comparable` record kept insertion
/// order. Only a VM re-entry can run `compareTo`, which is what the
/// comparator-ordered representation already does; `null` in [`SORT_CMP`]
/// stands for "the keys' own `compareTo`".
fn rank_by_compareto(vm: &mut VM, id: u32, arg: &Value) {
    if SORT_CMP.with(|s| s.borrow().contains_key(&id)) {
        return;
    }
    let natural_tree = HEAP.with(|h| {
        matches!(
            h.borrow().get(id as usize),
            Some(HostObj::Map {
                order: Order::Sorted {
                    by_cmp: false,
                    desc: false,
                },
                ..
            }) | Some(HostObj::Set {
                order: Order::Sorted {
                    by_cmp: false,
                    desc: false,
                },
                view: SetView::Own,
                ..
            })
        )
    });
    if !natural_tree {
        return;
    }
    let incoming = sequence_items(arg)
        .or_else(|| map_entries(arg).map(|es| es.into_iter().map(|(k, _)| k).collect()))
        .unwrap_or_else(|| vec![arg.clone()]);
    if !incoming.iter().any(|v| self_ordering(vm, v)) {
        return;
    }
    HEAP.with(|h| match h.borrow_mut().get_mut(id as usize) {
        Some(HostObj::Map { order, .. }) | Some(HostObj::Set { order, .. }) => {
            *order = Order::Sorted {
                by_cmp: true,
                desc: false,
            }
        }
        _ => {}
    });
    SORT_CMP.with(|s| s.borrow_mut().insert(id, Value::Undef));
    rerank(vm, id, &Value::Undef, 0);
}

/// The keys of a map, or the elements of a set, in stored order.
fn stored_keys(id: u32) -> Vec<Value> {
    HEAP.with(|h| match h.borrow().get(id as usize) {
        Some(HostObj::Map { entries, .. }) => entries.iter().map(|(k, _)| k.clone()).collect(),
        Some(HostObj::Set { items, .. }) => items.clone(),
        _ => Vec::new(),
    })
}

/// Place the keys a call appended to a comparator-ordered collection.
///
/// Storage is insertion order with every new key appended, so after a call the
/// first `before` keys are still in comparator order and only the tail is new.
/// Each new key is located by binary search against the comparator, called as
/// `compare(newKey, existingKey)` the way `TreeMap.put` calls it. A key the
/// comparator calls equal to one already present is *not* a new key in Java:
/// the set keeps the original element, and the map keeps the original key and
/// takes the new value — which the append left on the duplicate.
fn rerank(vm: &mut VM, id: u32, cmp: &Value, before: usize) {
    let (keys, values): (Vec<Value>, Option<Vec<Value>>) =
        HEAP.with(|h| match h.borrow().get(id as usize) {
            Some(HostObj::Map { entries, .. }) => (
                entries.iter().map(|(k, _)| k.clone()).collect(),
                Some(entries.iter().map(|(_, v)| v.clone()).collect()),
            ),
            Some(HostObj::Set { items, .. }) => (items.clone(), None),
            _ => (Vec::new(), None),
        });
    if keys.len() <= before {
        return;
    }
    // Positions into `keys`, kept in comparator order.
    let mut ranked: Vec<usize> = (0..before).collect();
    // Where a duplicate's value goes: (surviving position, new value).
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for new in before..keys.len() {
        let (mut lo, mut hi) = (0, ranked.len());
        let mut equal = None;
        while lo < hi {
            let mid = (lo + hi) / 2;
            let Some(c) = rank_compare(vm, cmp, &keys[new], &keys[ranked[mid]]) else {
                return;
            };
            match c {
                0 => {
                    equal = Some(ranked[mid]);
                    break;
                }
                n if n < 0 => hi = mid,
                _ => lo = mid + 1,
            }
        }
        match equal {
            Some(at) => merged.push((at, new)),
            None => ranked.insert(lo, new),
        }
    }
    let new_keys: Vec<Value> = ranked.iter().map(|&i| keys[i].clone()).collect();
    HEAP.with(|h| match h.borrow_mut().get_mut(id as usize) {
        Some(HostObj::Map { entries, index, .. }) => {
            let mut vals = values.unwrap_or_default();
            for (at, new) in merged {
                vals[at] = vals[new].clone();
            }
            *entries = ranked
                .iter()
                .zip(new_keys)
                .map(|(&i, k)| (k, vals[i].clone()))
                .collect();
            index.invalidate();
        }
        Some(HostObj::Set { items, index, .. }) => {
            *items = new_keys;
            index.invalidate();
        }
        _ => {}
    });
}

/// Evaluate `recv.method(args)` on a collection.
///
/// Every method that mutates takes the heap borrow, edits, and drops it before
/// returning; the two that run user code (`sort` with a comparator, `forEach`)
/// snapshot first and re-enter the VM with no borrow held, because a lambda body
/// can allocate.
fn coll_method_unranked(vm: &mut VM, recv: &Value, method: &str, args: &[Value]) -> Value {
    let Value::Obj(id) = recv else {
        return raise(
            vm,
            Fault::java(
                "NullPointerException",
                format!("Cannot invoke \"{method}()\" because the receiver is null"),
            ),
        );
    };
    if let Some(v) = pq_method(vm, *id, method, args) {
        return v;
    }
    // A `values()` view is a `Collection`, not a `List`: its one `remove` is
    // `remove(Object)`, so `m.values().remove(1)` removes the value 1 rather
    // than the element at index 1 — the reading the compiler picks for a
    // receiver it can only type as a list.
    let method = if method == "remove" && args.len() == 1 && is_values_view(*id) {
        "removeObject"
    } else {
        method
    };
    if method == "toArray" && args.len() <= 1 {
        if let Some(items) = sequence_items(recv) {
            return match collection_to_array(vm, items, args.first()) {
                Ok(v) => v,
                Err(f) => raise(vm, f),
            };
        }
    }
    let id = *id as usize;
    // Every method on a view checks it against its backing list first, exactly
    // as Java's `checkForComodification` does. `sublist_method` repeats the
    // check to get the window; this one covers the paths that bypass it
    // (`toString`, `sort`, `forEach`).
    if let Some(f) = stale_view(recv) {
        return raise(vm, f);
    }
    // A `Map.Entry` receiver reaches here when the compiler could type it
    // (`Map.Entry<K, V> e = …`), and reaches `entry_method` directly from
    // `b_str_dispatch` when it could not. One implementation, both routes.
    if let Some(r) = entry_method(recv, method, args) {
        return match r {
            Ok(v) => v,
            Err(f) => raise(vm, f),
        };
    }
    // `subList` allocates a view over the receiver, so it needs the receiver's
    // handle — which `list_method` (working on a plain `&mut Vec`) never sees.
    if method == "subList" && args.len() == 2 {
        return match make_sublist(id, args[0].jint(), args[1].jint()) {
            Ok(v) => v,
            Err(f) => raise(vm, f),
        };
    }
    if let Some(v) = navigable_view(vm, recv, method, args) {
        return v;
    }
    if let Some(v) = navigate(vm, recv, method, args) {
        return v;
    }
    // The two VM-re-entering methods are handled before any borrow is taken.
    match (method, args.len()) {
        ("sort", 1) => {
            let Some(items) = sequence_items(recv) else {
                return raise(vm, Fault::internal("javars: `sort` needs a List receiver"));
            };
            // `ImmutableCollections.AbstractImmutableList.sort` throws before
            // looking at the elements — an empty or already-sorted `List.of`
            // refuses too, and the comparator is never called.
            if collection_fixity(recv) == Some(Fixity::Immutable) {
                return raise(vm, unsupported());
            }
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
        // `removeIf` and `replaceAll` run a user predicate/operator per element,
        // so like `sort` they snapshot, re-enter the VM with no borrow held, and
        // write the result back.
        //
        // Which exception a receiver answers with is decided by its [`Fixity`],
        // and the three shapes disagree in a way a `catch` can see (measured on
        // openjdk 21.0.12):
        //
        //   List.of(1,2).removeIf(x -> false)      UnsupportedOperationException
        //   Arrays.asList(1,2).removeIf(x -> false)  false — the predicate ran
        //   Arrays.asList(1,2).removeIf(x -> true)   UnsupportedOperationException
        //   List.of(1,2).replaceAll(op)            UnsupportedOperationException
        //   Arrays.asList(1,2).replaceAll(x -> x*3)  [3, 6] — a set, not a resize
        //
        // `ImmutableCollections` overrides both to throw before it looks at the
        // argument, so `List.of(1,2).removeIf(null)` is the UOE and not the NPE
        // that `Arrays.asList(1,2).removeIf(null)` gives.
        // `removeAll(c)` and `retainAll(c)` are `removeIf` with the predicate
        // `c.contains(e)` (or its negation) — `ArrayList.batchRemove` and the
        // `AbstractCollection` default both ask `c` about each element in turn,
        // which is `e.equals(x)` for the `x` in `c`. They share its fixity rules:
        // `List.of(…).removeAll(c)` is the UOE outright, an `Arrays.asList`
        // view only once an element would actually go.
        ("removeIf" | "replaceAll" | "removeAll" | "retainAll", 1)
            if map_entries(recv).is_none() =>
        {
            let fixed = collection_fixity(recv).unwrap_or(Fixity::Mutable);
            if fixed == Fixity::Immutable {
                return raise(
                    vm,
                    Fault::java("UnsupportedOperationException", String::new()),
                );
            }
            if matches!(args[0], Value::Undef) {
                return raise(vm, Fault::java("NullPointerException", String::new()));
            }
            let Some(items) = sequence_items(recv) else {
                return raise(
                    vm,
                    Fault::internal(format!("javars: `{method}` needs a List or Set receiver")),
                );
            };
            if method == "replaceAll" {
                let mut out = Vec::with_capacity(items.len());
                for it in items {
                    out.push(invoke_closure(vm, &args[0], &[it]));
                    if PENDING.with(|p| p.borrow().is_some()) {
                        return Value::Undef;
                    }
                }
                // `ArrayList.replaceAll` bumps `modCount` whether or not any
                // element changed, so an outstanding `subList` view is stale
                // after it — verified against the reference, which throws
                // `ConcurrentModificationException` reading the view after
                // `l.replaceAll(x -> x)`.
                if let Err(f) = write_collection(recv, out, true) {
                    return raise(vm, f);
                }
                return Value::Undef;
            }
            let other = match method {
                "removeAll" | "retainAll" => match sequence_items(&args[0]) {
                    Some(o) => Some(o),
                    None => {
                        return raise(
                            vm,
                            Fault::internal(format!(
                                "javars: `{method}` needs a collection argument"
                            )),
                        )
                    }
                },
                _ => None,
            };
            let mut kept = Vec::with_capacity(items.len());
            for it in items.iter() {
                let drop = match &other {
                    Some(c) => {
                        let found = c.iter().any(|x| eq_call(vm, it, x));
                        found == (method == "removeAll")
                    }
                    None => matches!(
                        invoke_closure(vm, &args[0], std::slice::from_ref(it)),
                        Value::Bool(true)
                    ),
                };
                if PENDING.with(|p| p.borrow().is_some()) {
                    return Value::Undef;
                }
                if !drop {
                    kept.push(it.clone());
                }
            }
            let removed = kept.len() != items.len();
            // A fixed-size list can answer the query but not shrink: the JDK's
            // default `removeIf` only reaches `iterator.remove` when the
            // predicate said yes, so nothing to remove is a plain `false`.
            // The message is `remove`, not empty: a fixed-size list reaches
            // this through the default `Collection.removeIf`, which calls
            // `it.remove()`, and `AbstractList`'s iterator throws
            // `new UnsupportedOperationException("remove")` — naming the
            // operation it cannot do. The `List.of` refusal above comes from
            // `ImmutableCollections.uoe()` instead and carries no message, so
            // the two receivers differ in the text as well as in when they
            // throw.
            if removed && fixed == Fixity::FixedSize {
                return raise(
                    vm,
                    Fault::java("UnsupportedOperationException", "remove".to_string()),
                );
            }
            if removed {
                if let Err(f) = write_collection(recv, kept, true) {
                    return raise(vm, f);
                }
            }
            return Value::bool(removed);
        }
        // `AbstractCollection.containsAll`: `contains(x)` for each `x` of the
        // argument, which asks `x.equals(e)` of the receiver's elements.
        ("containsAll", 1) if map_entries(recv).is_none() => {
            if matches!(args[0], Value::Undef) {
                return raise(vm, Fault::java("NullPointerException", String::new()));
            }
            let (Some(items), Some(other)) = (sequence_items(recv), sequence_items(&args[0]))
            else {
                return raise(
                    vm,
                    Fault::internal("javars: `containsAll` needs two collections"),
                );
            };
            for x in &other {
                let found = items.iter().any(|e| eq_call(vm, x, e));
                if pending() {
                    return Value::Undef;
                }
                if !found {
                    return Value::bool(false);
                }
            }
            return Value::bool(true);
        }
        // The six `Map` methods that are *compound*: each is defined in the JDK
        // as a short sequence of `get`/`put`/`remove`/`containsKey`, and four of
        // them run a user function in the middle of it. javars had none of them,
        // so `map.computeIfAbsent(k, f)` — the ordinary way to fill a
        // multimap — was a hard `unsupported Map method` refusal.
        //
        // They are written here as that same sequence of primitive calls rather
        // than as fresh arms inside [`map_method`], for two reasons. The
        // primitives already carry the parts that are easy to get wrong and hard
        // to see: the key-equality plan (a user `equals` wins over `value_eq`),
        // the key index, and the rule that a re-`put` keeps an entry's original
        // position while a new key appends. And the function has to run with no
        // heap borrow held, which is why they sit up here beside `sort` and
        // `removeIf` rather than below the borrow.
        //
        // Two behaviours are NOT in the JDK's default bodies and are measured
        // from openjdk 21.0.12.1 instead:
        //
        //   * An immutable receiver refuses *before* the function runs, and
        //     refuses even when the default body would not have mutated —
        //     `Map.of("a",1).computeIfAbsent("a", f)` is the UOE, not `1`, and
        //     `Map.of(...).putAll(new HashMap<>())` is the UOE, not a no-op.
        //     `ImmutableCollections` overrides each of these to throw outright.
        //   * The function (and `merge`'s value) is null-checked up front, so a
        //     null one is the NPE even when the body would never have called it.
        // `remove(key, value)` and `replace(key, oldValue, newValue)`: `Map`'s
        // default bodies, which `HashMap`'s overrides agree with — act only when
        // the key is mapped (a `null` value included) and its value is
        // `Objects.equals` to the one named, and answer whether they did.
        ("remove", 2) | ("replace", 3) if map_entries(recv).is_some() => {
            if map_fixity(recv) == Some(Fixity::Immutable) {
                return raise(vm, unsupported());
            }
            let key = args[0].clone();
            let cur = coll_method(vm, recv, "get", std::slice::from_ref(&key));
            if pending() {
                return Value::Undef;
            }
            if !objects_equals(vm, &cur, &args[1]) {
                return Value::bool(false);
            }
            if matches!(cur, Value::Undef) {
                let mapped = coll_method(vm, recv, "containsKey", std::slice::from_ref(&key));
                if pending() || !matches!(mapped, Value::Bool(true)) {
                    return Value::bool(false);
                }
            }
            let _ = match method {
                "remove" => coll_method(vm, recv, "remove", &[key]),
                _ => coll_method(vm, recv, "put", &[key, args[2].clone()]),
            };
            return if pending() {
                Value::Undef
            } else {
                Value::bool(true)
            };
        }
        ("compute", 2)
        | ("computeIfAbsent", 2)
        | ("computeIfPresent", 2)
        | ("merge", 3)
        | ("replace", 2)
        | ("putAll", 1)
            if map_entries(recv).is_some() =>
        {
            if map_fixity(recv) == Some(Fixity::Immutable) {
                return raise(
                    vm,
                    Fault::java("UnsupportedOperationException", String::new()),
                );
            }
            // `Objects.requireNonNull` on the function, and on `merge`'s value,
            // before anything is read.
            let null_checked: &[usize] = match method {
                "compute" | "computeIfAbsent" | "computeIfPresent" => &[1],
                "merge" => &[1, 2],
                "putAll" => &[0],
                _ => &[],
            };
            if null_checked
                .iter()
                .any(|&i| matches!(args[i], Value::Undef))
            {
                return raise(vm, Fault::java("NullPointerException", String::new()));
            }
            if method == "putAll" {
                let Some(entries) = map_entries(&args[0]) else {
                    return raise(vm, Fault::internal("javars: `putAll` needs a Map argument"));
                };
                let order = map_order(&args[0]);
                let keys: Vec<Value> = entries.iter().map(|(k, _)| k.clone()).collect();
                for i in present_order(&keys, order) {
                    let (k, v) = entries[i].clone();
                    coll_method(vm, recv, "put", &[k, v]);
                    if PENDING.with(|p| p.borrow().is_some()) {
                        return Value::Undef;
                    }
                }
                return Value::Undef;
            }
            let key = args[0].clone();
            let old = coll_method(vm, recv, "get", std::slice::from_ref(&key));
            if PENDING.with(|p| p.borrow().is_some()) {
                return Value::Undef;
            }
            let absent = matches!(old, Value::Undef);
            // `replace` is the one arm with no function: it writes only over a
            // key the map already has, and answers with the value it displaced.
            // A key mapped to `null` is still mapped, so it is replaced too.
            if method == "replace" {
                if absent {
                    let mapped = coll_method(vm, recv, "containsKey", std::slice::from_ref(&key));
                    if pending() || !matches!(mapped, Value::Bool(true)) {
                        return Value::Undef;
                    }
                }
                coll_method(vm, recv, "put", &[key, args[1].clone()]);
                return old;
            }
            // `computeIfAbsent` on a key that is already mapped never calls the
            // function at all, and `computeIfPresent` on one that is not.
            if (method == "computeIfAbsent" && !absent) || (method == "computeIfPresent" && absent)
            {
                return old;
            }
            let fresh = match method {
                "computeIfAbsent" => invoke_closure(vm, &args[1], std::slice::from_ref(&key)),
                "computeIfPresent" | "compute" => {
                    invoke_closure(vm, &args[1], &[key.clone(), old.clone()])
                }
                // `merge` seeds an absent key with the value rather than calling
                // the function, and its function takes (old, value) — not the
                // (key, value) pair the `compute` family passes.
                _ if absent => args[1].clone(),
                _ => invoke_closure(vm, &args[2], &[old.clone(), args[1].clone()]),
            };
            if PENDING.with(|p| p.borrow().is_some()) {
                return Value::Undef;
            }
            if matches!(fresh, Value::Undef) {
                // A null result removes the entry — except under
                // `computeIfAbsent`, whose contract is to leave the map alone
                // and answer null when the function declines to supply a value.
                if method != "computeIfAbsent" && !absent {
                    coll_method(vm, recv, "remove", &[key]);
                }
                return Value::Undef;
            }
            coll_method(vm, recv, "put", &[key.clone(), fresh.clone()]);
            if PENDING.with(|p| p.borrow().is_some()) {
                return Value::Undef;
            }
            // A key these three *add* goes to the head of its hash bin, not the
            // tail `put` just gave it — see [`hash_bucket_head_insert`].
            if absent {
                hash_bucket_head_insert(recv, &key);
            }
            return fresh;
        }
        // `Map.replaceAll` — every value replaced in place by a `(key, value)`
        // function. Its `List`/`Set` namesake is the arm above, which excludes a
        // map receiver; a map's takes *two* parameters and moves no key, so each
        // result is written back with a `put` over a key the map already has.
        ("replaceAll", 1) if map_entries(recv).is_some() => {
            if map_fixity(recv) == Some(Fixity::Immutable) {
                return raise(
                    vm,
                    Fault::java("UnsupportedOperationException", String::new()),
                );
            }
            if matches!(args[0], Value::Undef) {
                return raise(vm, Fault::java("NullPointerException", String::new()));
            }
            let entries = map_entries(recv).unwrap_or_default();
            let order = map_order(recv);
            let keys: Vec<Value> = entries.iter().map(|(k, _)| k.clone()).collect();
            // Iterated in the order the map *presents*, not the order it stores:
            // the function is user code and can observe the sequence it is
            // called in, which for a `HashMap` is the bin walk.
            for i in present_order(&keys, order) {
                let (k, v) = entries[i].clone();
                let fresh = invoke_closure(vm, &args[0], &[k.clone(), v]);
                if PENDING.with(|p| p.borrow().is_some()) {
                    return Value::Undef;
                }
                coll_method(vm, recv, "put", &[k, fresh]);
                if PENDING.with(|p| p.borrow().is_some()) {
                    return Value::Undef;
                }
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
    // `hashCode` reads each element's, which may be a user body — same
    // constraint, same treatment.
    if let Some(h) = collection_hash(vm, recv, method, args.len()) {
        return h;
    }
    // `stream()` on a collection — a *second* heap object, like the iterator
    // below, and built from the elements as they stand.
    if method == "stream" && args.is_empty() {
        if let Some(items) = sequence_items(recv) {
            return stream_of(items, StreamKind::Ref);
        }
    }
    // An iterator names a *second* heap object, so both its construction and
    // its own methods run outside the exclusive borrow below.
    if method == "iterator" && args.is_empty() {
        if let Value::Obj(id) = recv {
            return Value::Obj(heap_alloc(HostObj::Iterator {
                source: *id,
                pos: 0,
                last: None,
                exp_mods: iter_mods(*id),
                bidi: false,
                desc: false,
            }));
        }
    }
    // `descendingIterator()` — `Deque`'s (`LinkedList`, `ArrayDeque`) and
    // `NavigableSet`'s (`TreeSet`): the same iterator walked from the end.
    if method == "descendingIterator" && args.is_empty() && is_descending_source(id) {
        return Value::Obj(heap_alloc(HostObj::Iterator {
            source: id as u32,
            pos: 0,
            last: None,
            exp_mods: iter_mods(id as u32),
            bidi: false,
            desc: true,
        }));
    }
    // `listIterator()` / `listIterator(n)` — the same second object, starting
    // its cursor at `n`. `ArrayList.listIterator` rejects a start outside
    // `[0, size]` with its own `"Index: n, Size: s"` message.
    if method == "listIterator" && args.len() <= 1 {
        if let Some(size) = list_size(id) {
            let start = args.first().map_or(0, |a| a.jint());
            if start < 0 || start as usize > size {
                return raise(
                    vm,
                    Fault::java(
                        "IndexOutOfBoundsException",
                        format!("Index: {start}, Size: {size}"),
                    ),
                );
            }
            return Value::Obj(heap_alloc(HostObj::Iterator {
                source: id as u32,
                pos: start as usize,
                last: None,
                exp_mods: iter_mods(id as u32),
                bidi: true,
                desc: false,
            }));
        }
    }
    // Likewise `addAll`/`equals` read their argument collection: snapshot it
    // first, because the borrow below is exclusive.
    //
    // `Map.equals` reads the *other* map's entries, which `sequence_items` does
    // not carry (a map is not a sequence), so it is snapshotted separately for
    // the same reason: the borrow below is exclusive, and reading the heap
    // under it panics.
    //
    // Only a `Value::Obj` can be either — both readers answer `None` for
    // anything else — and the overwhelming majority of calls (`add(int)`,
    // `get(int)`, `put(k, v)` over scalars) pass none, so the two `Vec`s were
    // two heap allocations per collection call that could only ever hold
    // `None`. Borrowing a shared all-`None` slice in that case removes them:
    // the values the callees see are identical, since a `Vec` of `None` and a
    // slice of `None` differ only in where they live.
    let none_seqs;
    let none_entries;
    let (arg_seqs, arg_entries): (ArgSeqs, ArgEntries) =
        if args.len() > NO_ARG_SEQS.len() || args.iter().any(|a| matches!(a, Value::Obj(_))) {
            none_seqs = args.iter().map(sequence_items).collect::<Vec<_>>();
            none_entries = args.iter().map(map_entries).collect::<Vec<_>>();
            (&none_seqs, &none_entries)
        } else {
            (&NO_ARG_SEQS[..args.len()], &NO_ARG_ENTRIES[..args.len()])
        };
    // Java answers a membership question with the element's own `equals`, whose
    // body needs the VM and no outstanding borrow — so it runs here, ahead of
    // the borrow, and the sections below read its verdicts.
    let eq = eq_plan(vm, recv, method, args, arg_seqs);
    // A throwable one of those bodies raised aborts the call rather than
    // answering from a half-resolved plan.
    if PENDING.with(|p| p.borrow().is_some()) {
        return Value::Undef;
    }
    let eq = eq.as_ref();
    if is_sublist(id) {
        return match sublist_method(id, method, args, arg_seqs, eq) {
            Ok(v) => v,
            Err(f) => raise(vm, f),
        };
    }
    // Set by the map arm below when the call left the map holding fewer keys.
    let mut shrank = false;
    let result = HEAP.with(|h| {
        let mut heap = h.borrow_mut();
        let Some(obj) = heap.get_mut(id) else {
            return Err(Fault::internal("javars: dangling collection handle"));
        };
        match obj {
            HostObj::List {
                items,
                fixed,
                mods,
                view,
            } => {
                // A `values()` view refuses `add`, whatever the map's fixity —
                // there is no key to file a bare value under. The same rule
                // `set_method` applies to the other two views.
                if view.is_some() && matches!(method, "add" | "addAll") {
                    return Err(Fault::java("UnsupportedOperationException", String::new()));
                }
                let before = items.len();
                let r = list_method(items, *fixed, method, args, arg_seqs, eq);
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
                fixed,
                index,
            } => {
                let before = entries.len();
                let r = map_method(
                    entries,
                    MapShape {
                        order: *order,
                        fixed: *fixed,
                    },
                    index,
                    method,
                    args,
                    arg_entries,
                    eq,
                );
                // A map holds one entry per key, so a key can only have left if
                // the count fell — and no map method both inserts and removes
                // in one call. That makes this the exact test for "an entry
                // javars handed out may have just been orphaned", and it costs
                // a length compare rather than a method-name list that a later
                // method could silently fall outside of.
                shrank = entries.len() < before;
                r
            }
            HostObj::Set {
                items,
                fixed,
                view,
                index,
                ..
            } => set_method(
                items,
                SetShape {
                    fixed: *fixed,
                    view: *view,
                },
                index,
                method,
                args,
                arg_seqs,
                eq,
            ),
            _ => Err(Fault::internal(format!(
                "javars: `{method}` is not a collection method"
            ))),
        }
    });
    // The one place every map mutation passes through. An entry javars handed
    // out is a copy of the map's pair, not the pair itself, so the map counts
    // the mutation here and the entries repair themselves when read. A program
    // that never took an `entrySet` pays one `Cell` read for this.
    if ENTRIES_LIVE.with(|e| e.get()) {
        bump_entry_generation(id as u32);
        // Values repair themselves lazily, but *detachment* cannot: once the
        // key is back, the map looks exactly as it did before it left, and an
        // entry reading it then would revive a node Java replaced. So the one
        // case that has to be settled while it is still visible is settled
        // here — and only here, on the calls that actually dropped a key.
        if shrank {
            detach_orphaned_entries(id as u32);
        }
    }
    match result {
        Ok(NewColl::Value(v)) => v,
        // A derived view (`keySet`, `values`) is allocated after the borrow is
        // released, because allocating touches the same slab.
        Ok(NewColl::Alloc(obj)) => {
            let view = Value::Obj(heap_alloc(obj));
            if matches!(method, "keySet" | "values") && args.is_empty() && is_map_handle(id) {
                register_map_view(&view, id as u32);
            }
            view
        }
        Ok(NewColl::Entries { pairs, fixed, of }) => {
            let items: Vec<Value> = pairs
                .into_iter()
                .map(|(key, value)| entry_for(id as u32, key, value))
                .collect();
            let view = Value::Obj(heap_alloc(HostObj::Set {
                items,
                // The pairs arrive already in the map's presentation order, so
                // the view walks them as they lie.
                order: Order::Insertion,
                fixed,
                view: SetView::Entries(of),
                index: KeyIndex::default(),
            }));
            register_map_view(&view, id as u32);
            view
        }
        Err(f) => raise(vm, f),
    }
}

/// A collection method's result: a plain value, or a new heap object that must
/// be allocated once the receiver's borrow has been dropped.
enum NewColl {
    Value(Value),
    Alloc(HostObj),
    /// `map.entrySet()` — the map's pairs in presentation order, to be turned
    /// into one [`HostObj::Entry`] each and gathered into a view set. Unlike
    /// [`NewColl::Alloc`] this needs the *receiver's* handle, because every
    /// entry has to remember the map `setValue` writes back to.
    Entries {
        pairs: Vec<(Value, Value)>,
        fixed: Fixity,
        of: ViewOf,
    },
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

/// The length of a plain `List` (not a view), or `None` for anything else.
/// Whether the collection at `id` answers `descendingIterator()`: a list shape
/// that is not a view (javars models `LinkedList` and `ArrayDeque` as one), or
/// a sorted set (`TreeSet`). `javac` has already refused the call on a type
/// that does not declare it.
fn is_descending_source(id: usize) -> bool {
    HEAP.with(|h| match h.borrow().get(id) {
        Some(HostObj::List { view: None, .. }) => true,
        Some(HostObj::Set { order, view, .. }) => {
            matches!(order, Order::Sorted { .. }) && matches!(view, SetView::Own)
        }
        _ => false,
    })
}

fn list_size(id: usize) -> Option<usize> {
    HEAP.with(|h| match h.borrow().get(id) {
        Some(HostObj::List { items, .. }) => Some(items.len()),
        _ => None,
    })
}

/// Allocate a plain `java.util.Optional`.
fn optional(v: Option<Value>) -> Value {
    optional_of("Optional", v)
}

/// Allocate an `Optional` of the given class — one of `Optional`,
/// `OptionalInt`, `OptionalLong`, `OptionalDouble`.
fn optional_of(class: &'static str, value: Option<Value>) -> Value {
    Value::Obj(heap_alloc(HostObj::Optional { class, value }))
}

/// The class and contents of an `Optional` handle, or `None` when the value is
/// not an `Optional` at all.
fn as_optional_full(v: &Value) -> Option<(&'static str, Option<Value>)> {
    let Value::Obj(id) = v else {
        return None;
    };
    HEAP.with(|h| match h.borrow().get(*id as usize) {
        Some(HostObj::Optional { class, value }) => Some((*class, value.clone())),
        _ => None,
    })
}

/// The source and pipeline of a `Stream` handle, or `None` for anything else.
fn as_stream(v: &Value) -> Option<(Source, Vec<Stage>, StreamKind)> {
    let Value::Obj(id) = v else {
        return None;
    };
    HEAP.with(|h| match h.borrow().get(*id as usize) {
        Some(HostObj::Stream {
            source,
            stages,
            kind,
        }) => Some((source.clone(), stages.clone(), *kind)),
        _ => None,
    })
}

/// Allocate a stream over `source`.
fn stream_of(source: Vec<Value>, kind: StreamKind) -> Value {
    // A `DoubleStream` holds doubles whatever its source spelled:
    // `DoubleStream.of(4, 3)` is `4.0, 3.0`.
    let source = if kind == StreamKind::Double {
        source
            .iter()
            .map(|v| Value::float(deboxed(v).jfloat()))
            .collect()
    } else {
        source
    };
    Value::Obj(heap_alloc(HostObj::Stream {
        source: Source::Items(source),
        stages: Vec::new(),
        kind,
    }))
}

/// Allocate the stream `recv` becomes with `stage` appended.
///
/// A stream is single-use in Java and javars does not enforce that, so appending
/// to a *copy* rather than mutating in place is the safe reading: a program that
/// (illegally) reuses one sees the pipeline it built, not one a later stage
/// extended underneath it.
fn stream_with(
    recv: &Value,
    kind: StreamKind,
    added: impl IntoIterator<Item = Stage>,
) -> Option<Value> {
    let (source, mut stages, _) = as_stream(recv)?;
    stages.extend(added);
    Some(Value::Obj(heap_alloc(HostObj::Stream {
        source,
        stages,
        kind,
    })))
}

/// Push one element through `stages`, calling `sink` for each element that
/// reaches the end. Answers `false` when the pipeline has been cancelled — by a
/// `limit` filling up, or by a short-circuiting terminal.
///
/// `counters` is one slot per stage, so a `limit` or a `skip` keeps its own
/// count across elements. It is split alongside `stages` so each stage reads its
/// own slot and never another's.
fn stream_push(
    vm: &mut VM,
    v: Value,
    stages: &[Stage],
    counters: &mut [i64],
    sink: &mut dyn FnMut(&mut VM, Value) -> bool,
) -> bool {
    let (Some(stage), rest) = (stages.first(), &stages[stages.len().min(1)..]) else {
        return sink(vm, v);
    };
    let (count, rest_counts) = counters.split_first_mut().expect("one counter per stage");
    match stage {
        Stage::Filter(p) => {
            if matches!(
                invoke_closure(vm, p, std::slice::from_ref(&v)),
                Value::Bool(true)
            ) {
                stream_push(vm, v, rest, rest_counts, sink)
            } else {
                true
            }
        }
        Stage::Map(f) => {
            let mapped = invoke_closure(vm, f, &[v]);
            stream_push(vm, mapped, rest, rest_counts, sink)
        }
        Stage::Widen => stream_push(
            vm,
            Value::float(deboxed(&v).jfloat()),
            rest,
            rest_counts,
            sink,
        ),
        Stage::FlatMap(f) => {
            let inner = invoke_closure(vm, f, &[v]);
            // The mapper answers a stream (or, tolerantly, a collection): its
            // elements are pushed on in place of the one that produced them.
            // A mapped stream is pulled one element at a time straight into the
            // rest of the pipeline, so a downstream `limit` or short-circuiting
            // terminal stops it — which is what lets the mapper answer an
            // unbounded stream, as it may since JDK 10.
            let elems = match as_stream(&inner) {
                Some((src, st, _)) => {
                    let mut go = true;
                    stream_drive(vm, src, &st, &mut |vm, x| {
                        go = stream_push(vm, x, rest, rest_counts, sink);
                        go
                    });
                    return go;
                }
                None => sequence_items(&inner).unwrap_or_default(),
            };
            for e in elems {
                if !stream_push(vm, e, rest, rest_counts, sink) {
                    return false;
                }
            }
            true
        }
        Stage::Peek(f) => {
            invoke_closure(vm, f, std::slice::from_ref(&v));
            stream_push(vm, v, rest, rest_counts, sink)
        }
        Stage::TakeWhile(p) => {
            if matches!(
                invoke_closure(vm, p, std::slice::from_ref(&v)),
                Value::Bool(true)
            ) {
                stream_push(vm, v, rest, rest_counts, sink)
            } else {
                false
            }
        }
        Stage::DropWhile(p) => {
            if *count == 0
                && matches!(
                    invoke_closure(vm, p, std::slice::from_ref(&v)),
                    Value::Bool(true)
                )
            {
                return true;
            }
            *count = 1;
            stream_push(vm, v, rest, rest_counts, sink)
        }
        Stage::Skip(n) => {
            *count += 1;
            if *count <= *n {
                true
            } else {
                stream_push(vm, v, rest, rest_counts, sink)
            }
        }
        // A full `limit` ends the pipeline rather than merely dropping the
        // element — which is what makes `peek(p).limit(2)` call `p` twice.
        Stage::Limit(n) => {
            if *count >= *n {
                return false;
            }
            *count += 1;
            let go = stream_push(vm, v, rest, rest_counts, sink);
            go && *count < *n
        }
        Stage::Distinct | Stage::Sorted(_) => {
            unreachable!("a barrier is split off before the element-wise walk")
        }
    }
}

/// Run `source` through `stages`, calling `sink` for each surviving element
/// until it answers `false` or the source is exhausted.
///
/// A stateful barrier — `distinct` or `sorted` — cannot answer for an element
/// without having seen every element before it, so the pipeline is evaluated in
/// segments split at the first one. That is Java's own shape, and it is why a
/// `peek` before a `sorted` runs for every element while a `peek` before a
/// `limit` does not.
fn stream_drive(
    vm: &mut VM,
    source: Source,
    stages: &[Stage],
    sink: &mut dyn FnMut(&mut VM, Value) -> bool,
) {
    if let Some(i) = stages
        .iter()
        .position(|s| matches!(s, Stage::Distinct | Stage::Sorted(_)))
    {
        let mut buf = Vec::new();
        stream_drive(vm, source, &stages[..i], &mut |_vm, v| {
            buf.push(v);
            true
        });
        let buf = match &stages[i] {
            Stage::Distinct => {
                let mut seen: Vec<Value> = Vec::new();
                for v in buf {
                    if !seen.iter().any(|x| value_eq(x, &v)) {
                        seen.push(v);
                    }
                }
                seen
            }
            Stage::Sorted(cmp) => sort_values(vm, buf, cmp.as_ref()),
            _ => unreachable!("the position above found a barrier"),
        };
        return stream_drive(vm, Source::Items(buf), &stages[i + 1..], sink);
    }
    let mut counters = vec![0i64; stages.len()];
    // `Stream.concat(a, b)`: drive `a`'s whole pipeline into this one, then
    // `b`'s — each part is pulled only as far as this pipeline still wants.
    if let Source::Concat(parts) = source {
        for part in [&parts.0, &parts.1] {
            let Some((src, st, _)) = as_stream(part) else {
                continue;
            };
            let mut go = true;
            stream_drive(vm, src, &st, &mut |vm, x| {
                go = stream_push(vm, x, stages, &mut counters, sink);
                go
            });
            if !go {
                return;
            }
        }
        return;
    }
    let mut source = source.cursor();
    while let Some(v) = source.pull(vm) {
        if !stream_push(vm, v, stages, &mut counters, sink) {
            break;
        }
    }
}

/// Sort by a comparator closure, or by the natural order when there is none.
///
/// A stable insertion sort driven by the comparator, because a user comparator
/// re-enters the VM and `slice::sort_by` cannot call back into it.
fn sort_values(vm: &mut VM, items: Vec<Value>, cmp: Option<&Value>) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(items.len());
    for v in items {
        let mut at = out.len();
        while at > 0 {
            let ord = match cmp {
                Some(c) => invoke_closure(vm, c, &[out[at - 1].clone(), v.clone()]).jint(),
                None => natural_cmp(&out[at - 1], &v) as i64,
            };
            if ord <= 0 {
                break;
            }
            at -= 1;
        }
        out.insert(at, v);
    }
    out
}

/// Every element a pipeline yields.
fn stream_collect(vm: &mut VM, source: Source, stages: &[Stage]) -> Vec<Value> {
    let mut out = Vec::new();
    stream_drive(vm, source, stages, &mut |_vm, v| {
        out.push(v);
        true
    });
    out
}

/// `java.util.stream.Stream`'s methods — the intermediate operations, which
/// answer a new stream, and the terminals, which run the pipeline.
///
/// `None` for any other receiver. Every terminal drives the pipeline exactly
/// once and the short-circuiting ones stop the source, which is what a `peek`
/// before a `limit` or a `findFirst` observes.
fn stream_method(
    vm: &mut VM,
    recv: &Value,
    method: &str,
    args: &[Value],
) -> Option<Result<Value, Fault>> {
    let (source, stages, kind) = as_stream(recv)?;
    let stage = |st: Stage| Ok(stream_with(recv, kind, Some(st)).expect("receiver is a stream"));
    let retyped = |k: StreamKind| Ok(stream_with(recv, k, None).expect("receiver is a stream"));
    let all = |vm: &mut VM| stream_collect(vm, source.clone(), &stages);
    Some(match (method, args.len()) {
        // ── intermediate ──
        ("filter", 1) => stage(Stage::Filter(args[0].clone())),
        // A `DoubleStream`'s `map` is a `DoubleUnaryOperator`, whose result is a
        // `double` even when the body is integral.
        ("map", 1) if kind == StreamKind::Double => {
            Ok(
                stream_with(recv, kind, [Stage::Map(args[0].clone()), Stage::Widen])
                    .expect("receiver is a stream"),
            )
        }
        ("map", 1) => stage(Stage::Map(args[0].clone())),
        ("flatMap", 1) => stage(Stage::FlatMap(args[0].clone())),
        ("peek", 1) => stage(Stage::Peek(args[0].clone())),
        ("limit", 1) => stage(Stage::Limit(args[0].jint())),
        ("skip", 1) => stage(Stage::Skip(args[0].jint())),
        ("takeWhile", 1) => stage(Stage::TakeWhile(args[0].clone())),
        ("dropWhile", 1) => stage(Stage::DropWhile(args[0].clone())),
        ("distinct", 0) => stage(Stage::Distinct),
        ("sorted", 0) => stage(Stage::Sorted(None)),
        ("sorted", 1) => stage(Stage::Sorted(Some(args[0].clone()))),
        // The mapping operations that also change the stream's *shape*, which
        // is what decides whether `max()` answers an `OptionalInt` or an
        // `OptionalDouble`.
        ("mapToInt", 1) => {
            Ok(
                stream_with(recv, StreamKind::Int, Some(Stage::Map(args[0].clone())))
                    .expect("receiver is a stream"),
            )
        }
        ("mapToLong", 1) => {
            Ok(
                stream_with(recv, StreamKind::Long, Some(Stage::Map(args[0].clone())))
                    .expect("receiver is a stream"),
            )
        }
        ("mapToDouble", 1) => Ok(stream_with(
            recv,
            StreamKind::Double,
            [Stage::Map(args[0].clone()), Stage::Widen],
        )
        .expect("receiver is a stream")),
        ("mapToObj", 1) => {
            Ok(
                stream_with(recv, StreamKind::Ref, Some(Stage::Map(args[0].clone())))
                    .expect("receiver is a stream"),
            )
        }
        ("boxed", 0) => retyped(StreamKind::Ref),
        ("asLongStream", 0) => retyped(StreamKind::Long),
        ("asDoubleStream", 0) => Ok(
            stream_with(recv, StreamKind::Double, [Stage::Widen]).expect("receiver is a stream")
        ),
        // ── terminal ──
        ("toList", 0) => Ok(list_value(all(vm), Fixity::Immutable)),
        ("toArray", 0) => Ok(Value::Obj(heap_alloc(HostObj::Array(all(vm))))),
        // `toArray(generator)`: the elements, in the array the generator makes.
        ("toArray", 1) => {
            let items = all(vm);
            collection_to_array(vm, items, Some(&args[0]))
        }
        ("count", 0) => Ok(Value::Int(all(vm).len() as i64)),
        ("forEach" | "forEachOrdered", 1) => {
            let f = args[0].clone();
            stream_drive(vm, source, &stages, &mut |vm, v| {
                invoke_closure(vm, &f, &[v]);
                true
            });
            Ok(Value::Undef)
        }
        ("sum", 0) => {
            let items = all(vm);
            Ok(match kind {
                StreamKind::Double => Value::float(compensated_sum(&items)),
                StreamKind::Int => Value::Int(i64::from(wrapping_sum(&items) as i32)),
                _ => Value::Int(wrapping_sum(&items)),
            })
        }
        // `summaryStatistics()` of a primitive stream: one `accept` per element.
        ("summaryStatistics", 0) if kind != StreamKind::Ref => {
            let items = all(vm);
            Ok(Value::Obj(heap_alloc(HostObj::Stats(SummaryStats::of(
                kind, &items,
            )))))
        }
        // `average` answers an `OptionalDouble` whatever the stream's width,
        // and an empty one for an empty stream rather than a NaN.
        ("average", 0) => {
            let items = all(vm);
            Ok(optional_of(
                "OptionalDouble",
                (!items.is_empty()).then(|| Value::float(stream_average(&items, kind))),
            ))
        }
        ("min" | "max", 0 | 1) => {
            let items = all(vm);
            let cmp = args.first().cloned();
            let sorted = sort_values(vm, items, cmp.as_ref());
            let pick = if method == "min" {
                sorted.first()
            } else {
                sorted.last()
            };
            Ok(optional_of(kind.optional_class(), pick.cloned()))
        }
        ("findFirst" | "findAny", 0) => {
            let mut found = None;
            stream_drive(vm, source, &stages, &mut |_vm, v| {
                found = Some(v);
                false
            });
            Ok(optional_of(kind.optional_class(), found))
        }
        ("anyMatch" | "allMatch" | "noneMatch", 1) => {
            // One walk answers all three: `allMatch` looks for a counterexample
            // and the other two for an example, so each stops at the first hit.
            let want = method != "allMatch";
            let p = args[0].clone();
            let mut hit = false;
            stream_drive(vm, source, &stages, &mut |vm, v| {
                let ok = matches!(invoke_closure(vm, &p, &[v]), Value::Bool(true));
                if ok == want {
                    hit = true;
                    return false;
                }
                true
            });
            Ok(Value::bool(match method {
                "anyMatch" => hit,
                "noneMatch" => !hit,
                _ => !hit,
            }))
        }
        ("reduce", 1) => {
            let items = all(vm);
            let f = args[0].clone();
            let mut acc: Option<Value> = None;
            for v in items {
                acc = Some(match acc {
                    Some(a) => invoke_closure(vm, &f, &[a, v]),
                    None => v,
                });
            }
            Ok(optional_of(kind.optional_class(), acc))
        }
        ("reduce", 2) => {
            let items = all(vm);
            let f = args[1].clone();
            let mut acc = args[0].clone();
            for v in items {
                acc = invoke_closure(vm, &f, &[acc, v]);
            }
            Ok(acc)
        }
        ("collect", 1) => {
            let items = all(vm);
            collect_with(vm, items, &args[0])
        }
        // `collect(supplier, accumulator, combiner)` — the mutable reduction
        // `StringBuilder::new, StringBuilder::append, StringBuilder::append`
        // spells. A sequential stream has one container and never combines:
        // `ReduceOps.makeRef` runs the supplier once and the accumulator per
        // element, in encounter order.
        ("collect", 3) => {
            let items = all(vm);
            let container = invoke_closure(vm, &args[0], &[]);
            for v in items {
                if pending() {
                    break;
                }
                invoke_closure(vm, &args[1], &[container.clone(), v]);
            }
            Ok(container)
        }
        _ => Err(Fault::internal(format!(
            "javars: unsupported Stream method `{method}` with {} argument(s)",
            args.len()
        ))),
    })
}

/// `Collectors.sumWithCompensation` folded over `items`, finished by
/// `Collectors.computeFinalSum` — the Kahan summation `DoubleStream.sum`,
/// `average`, and `summingDouble`/`averagingDouble` all share, so
/// `DoubleStream.of(0.1, 0.2, 0.3).sum()` is `0.6` and not the naive
/// `0.6000000000000001`. The simple sum rides along only to answer an
/// infinite total that the compensation term turned into `NaN`.
fn compensated_sum(items: &[Value]) -> f64 {
    let (mut sum, mut comp, mut simple) = (0.0f64, 0.0f64, 0.0f64);
    for v in items {
        let d = deboxed(v).jfloat();
        simple += d;
        let tmp = d - comp;
        let velvel = sum + tmp;
        comp = (velvel - sum) - tmp;
        sum = velvel;
    }
    let tmp = sum - comp;
    if tmp.is_nan() && simple.is_infinite() {
        simple
    } else {
        tmp
    }
}

/// An integral sum with Java's two's-complement wrap at 64 bits; an `int`
/// stream narrows the result to 32 bits afterwards, which is the same answer
/// as wrapping at every step.
fn wrapping_sum(items: &[Value]) -> i64 {
    items
        .iter()
        .fold(0i64, |acc, v| acc.wrapping_add(deboxed(v).jint()))
}

/// `average()` of a non-empty primitive stream. `IntStream`/`LongStream`
/// accumulate a `long` sum and divide it as a `double`; `DoubleStream` divides
/// the compensated sum.
fn stream_average(items: &[Value], kind: StreamKind) -> f64 {
    let total = if kind == StreamKind::Double {
        compensated_sum(items)
    } else {
        wrapping_sum(items) as f64
    };
    total / items.len() as f64
}

/// The stream shape a primitive stream class names.
fn primitive_stream_kind(class: &str) -> StreamKind {
    match class {
        "LongStream" => StreamKind::Long,
        "DoubleStream" => StreamKind::Double,
        _ => StreamKind::Int,
    }
}

/// The `'static` name a `Collectors.*` recipe is stored under, for the factory
/// arms that match several method names at once.
fn static_kind(method: &str) -> &'static str {
    const KINDS: &[&str] = &[
        "mapping",
        "filtering",
        "flatMapping",
        "collectingAndThen",
        "summingInt",
        "summingLong",
        "summingDouble",
        "averagingInt",
        "averagingLong",
        "averagingDouble",
        "summarizingInt",
        "summarizingLong",
        "summarizingDouble",
        "minBy",
        "maxBy",
        "toCollection",
    ];
    KINDS
        .iter()
        .find(|k| **k == method)
        .copied()
        .expect("every factory arm names a listed collector")
}

/// Allocate a `Collectors.*` recipe.
fn collector(kind: &'static str, args: Vec<Value>) -> Value {
    Value::Obj(heap_alloc(HostObj::Collector { kind, args }))
}

/// Allocate a `List` holding `items`.
fn list_value(items: Vec<Value>, fixed: Fixity) -> Value {
    Value::Obj(heap_alloc(HostObj::List {
        items,
        fixed,
        mods: 0,
        view: None,
    }))
}

/// Run a `Collectors.*` recipe over the elements a pipeline yielded.
fn collect_with(vm: &mut VM, items: Vec<Value>, collector: &Value) -> Result<Value, Fault> {
    let Value::Obj(id) = collector else {
        return Err(Fault::internal(
            "javars: `collect` takes a `Collectors.*` collector".to_string(),
        ));
    };
    let (kind, cargs) = HEAP
        .with(|h| match h.borrow().get(*id as usize) {
            Some(HostObj::Collector { kind, args }) => Some((*kind, args.clone())),
            _ => None,
        })
        .ok_or_else(|| {
            Fault::internal("javars: `collect` takes a `Collectors.*` collector".to_string())
        })?;
    Ok(match kind {
        "toList" => list_value(items, Fixity::Mutable),
        "toSet" => {
            // `HashSet::add` per element: the first of equal elements stays,
            // compared through a user `equals` where one is declared.
            for v in &items {
                file_key_hash(vm, v);
            }
            let set = Value::Obj(heap_alloc(HostObj::Set {
                items: distinct(vm, &items),
                order: Order::HASH,
                fixed: Fixity::Mutable,
                view: SetView::Own,
                index: KeyIndex::default(),
            }));
            hash_grow_put(&set, 0);
            set
        }
        "counting" => Value::Int(items.len() as i64),
        "joining" => {
            let sep = cargs.first().map(java_str).unwrap_or_default();
            let pre = cargs.get(1).map(java_str).unwrap_or_default();
            let suf = cargs.get(2).map(java_str).unwrap_or_default();
            let body: Vec<String> = items.iter().map(java_str).collect();
            Value::str(format!("{pre}{}{suf}", body.join(&sep)))
        }
        // `toMap(k, v)` refuses a repeated key the way `uniqKeysMapAccumulator`
        // does; `toMap(k, v, merge)` folds it with `Map.merge`; a fourth
        // argument supplies the map. Both write through the map's own `put`
        // path, so a `TreeMap::new` result is ordered like one.
        "toMap" => {
            let map = match cargs.get(3) {
                Some(factory) => invoke_closure(vm, factory, &[]),
                None => new_collection(vm, "HashMap", &Value::Undef)?,
            };
            for v in items {
                let key = invoke_closure(vm, &cargs[0], std::slice::from_ref(&v));
                let val = invoke_closure(vm, &cargs[1], std::slice::from_ref(&v));
                if pending() {
                    return Ok(Value::Undef);
                }
                if matches!(val, Value::Undef) {
                    return Err(Fault::java("NullPointerException", String::new()));
                }
                match cargs.get(2) {
                    Some(merge) => {
                        coll_method(vm, &map, "merge", &[key, val, merge.clone()]);
                    }
                    None => {
                        let prior =
                            coll_method(vm, &map, "putIfAbsent", &[key.clone(), val.clone()]);
                        if !matches!(prior, Value::Undef) {
                            return Err(Fault::java(
                                "IllegalStateException",
                                format!(
                                    "Duplicate key {} (attempted merging values {} and {})",
                                    java_str_vm(vm, &key),
                                    java_str_vm(vm, &prior),
                                    java_str_vm(vm, &val)
                                ),
                            ));
                        }
                    }
                }
                if pending() {
                    return Ok(Value::Undef);
                }
            }
            map
        }
        // `groupingBy(k)`, `groupingBy(k, downstream)`, and
        // `groupingBy(k, mapFactory, downstream)`. The JDK's accumulator is
        // `m.computeIfAbsent(key, k -> downstream.supplier().get())`, so the
        // *map* decides which keys are the same group — a user `equals`, a
        // `TreeMap`'s comparator — and a new key takes `computeIfAbsent`'s
        // place in its hash bin. Each group is held in the map as its index
        // into `groups` until every element is placed, then reduced by its
        // downstream collector (`toList` when none is named) and written back
        // over its own key.
        "groupingBy" => {
            let (factory, downstream) = match cargs.len() {
                3 => (Some(&cargs[1]), Some(&cargs[2])),
                2 => (None, Some(&cargs[1])),
                _ => (None, None),
            };
            let map = match factory {
                Some(f) => invoke_closure(vm, f, &[]),
                None => new_collection(vm, "HashMap", &Value::Undef)?,
            };
            let mut groups: Vec<(Value, Vec<Value>)> = Vec::new();
            for v in items {
                let key = invoke_closure(vm, &cargs[0], std::slice::from_ref(&v));
                if pending() {
                    return Ok(Value::Undef);
                }
                if matches!(key, Value::Undef) {
                    return Err(Fault::java(
                        "NullPointerException",
                        "element cannot be mapped to a null key".to_string(),
                    ));
                }
                if let Some((mut t, size)) = hash_table(&map) {
                    t.compute_pre(size);
                    store_hash_table(&map, t);
                }
                match coll_method(vm, &map, "get", std::slice::from_ref(&key)) {
                    Value::Int(at) => groups[at as usize].1.push(v),
                    _ => {
                        let at = Value::Int(groups.len() as i64);
                        put_as_compute(vm, &map, key.clone(), at);
                        groups.push((key, vec![v]));
                    }
                }
                if pending() {
                    return Ok(Value::Undef);
                }
            }
            for (key, group) in groups {
                let reduced = match downstream {
                    Some(d) => collect_with(vm, group, d)?,
                    None => list_value(group, Fixity::Mutable),
                };
                coll_method(vm, &map, "put", &[key, reduced]);
                if pending() {
                    return Ok(Value::Undef);
                }
            }
            map
        }
        // `partitioningBy(p[, downstream])`: always both keys, `false` first,
        // in a map that refuses `put` like the JDK's `Partition`.
        "partitioningBy" => {
            let (mut no, mut yes) = (Vec::new(), Vec::new());
            for v in items {
                let verdict = invoke_closure(vm, &cargs[0], std::slice::from_ref(&v));
                if pending() {
                    return Ok(Value::Undef);
                }
                if matches!(verdict, Value::Bool(true)) {
                    yes.push(v);
                } else {
                    no.push(v);
                }
            }
            let reduce = |vm: &mut VM, part: Vec<Value>| match cargs.get(1) {
                Some(d) => collect_with(vm, part, d),
                None => Ok(list_value(part, Fixity::Mutable)),
            };
            let (no, yes) = (reduce(vm, no)?, reduce(vm, yes)?);
            Value::Obj(heap_alloc(HostObj::Map {
                entries: vec![(Value::bool(false), no), (Value::bool(true), yes)],
                order: Order::Insertion,
                fixed: Fixity::Immutable,
                index: KeyIndex::default(),
            }))
        }
        // The adapters: transform or filter each element, or the finished
        // result, and hand the rest to the downstream collector.
        "mapping" | "filtering" | "flatMapping" => {
            let mut out = Vec::with_capacity(items.len());
            for v in items {
                let r = invoke_closure(vm, &cargs[0], std::slice::from_ref(&v));
                if pending() {
                    return Ok(Value::Undef);
                }
                match kind {
                    "mapping" => out.push(r),
                    "filtering" => {
                        if matches!(r, Value::Bool(true)) {
                            out.push(v);
                        }
                    }
                    _ => {
                        if let Some((src, st, _)) = as_stream(&r) {
                            out.extend(stream_collect(vm, src, &st));
                        }
                    }
                }
            }
            collect_with(vm, out, &cargs[1])?
        }
        "collectingAndThen" => {
            let done = collect_with(vm, items, &cargs[0])?;
            invoke_closure(vm, &cargs[1], &[done])
        }
        // `teeing(a, b, merger)`: both collectors see every element.
        "teeing" => {
            let a = collect_with(vm, items.clone(), &cargs[0])?;
            let b = collect_with(vm, items, &cargs[1])?;
            invoke_closure(vm, &cargs[2], &[a, b])
        }
        // The numeric reducers apply their mapper first. `summingInt` keeps an
        // `int` accumulator (it wraps), `summingLong` a `long`, and the
        // `double` forms the compensated sum. An average of nothing is `0.0`.
        "summingInt" | "summingLong" | "summingDouble" | "averagingInt" | "averagingLong"
        | "averagingDouble" => {
            let mut mapped = Vec::with_capacity(items.len());
            for v in items {
                mapped.push(invoke_closure(vm, &cargs[0], &[v]));
                if pending() {
                    return Ok(Value::Undef);
                }
            }
            match kind {
                "summingInt" => Value::Int(i64::from(wrapping_sum(&mapped) as i32)),
                "summingLong" => Value::Int(wrapping_sum(&mapped)),
                "summingDouble" => Value::float(compensated_sum(&mapped)),
                _ if mapped.is_empty() => Value::float(0.0),
                "averagingDouble" => Value::float(stream_average(&mapped, StreamKind::Double)),
                _ => Value::float(stream_average(&mapped, StreamKind::Long)),
            }
        }
        // `summarizingInt`/`Long`/`Double(mapper)`: the mapped values, accepted
        // in encounter order into one statistics object.
        "summarizingInt" | "summarizingLong" | "summarizingDouble" => {
            let mut stats = SummaryStats::new(match kind {
                "summarizingInt" => StreamKind::Int,
                "summarizingLong" => StreamKind::Long,
                _ => StreamKind::Double,
            });
            for v in items {
                let m = invoke_closure(vm, &cargs[0], &[v]);
                if pending() {
                    return Ok(Value::Undef);
                }
                stats.accept(&m);
            }
            Value::Obj(heap_alloc(HostObj::Stats(stats)))
        }
        // `minBy`/`maxBy` fold `BinaryOperator.minBy`/`maxBy` left to right, and
        // both keep the earlier element on a tie (`cmp(a, b) <= 0 ? a : b` and
        // `cmp(a, b) >= 0 ? a : b`).
        "minBy" | "maxBy" => {
            let mut best: Option<Value> = None;
            for v in items {
                best = Some(match best {
                    None => v,
                    Some(b) => {
                        let c = invoke_closure(vm, &cargs[0], &[b.clone(), v.clone()]).jint();
                        if pending() {
                            return Ok(Value::Undef);
                        }
                        let keep = if kind == "minBy" { c <= 0 } else { c >= 0 };
                        if keep {
                            b
                        } else {
                            v
                        }
                    }
                });
            }
            optional(best)
        }
        // `reducing(op)` answers an `Optional`; `reducing(identity, op)` and
        // `reducing(identity, mapper, op)` start from the identity.
        "reducing" => {
            let (identity, mapper, op) = match cargs.len() {
                1 => (None, None, &cargs[0]),
                2 => (Some(cargs[0].clone()), None, &cargs[1]),
                _ => (Some(cargs[0].clone()), Some(&cargs[1]), &cargs[2]),
            };
            let seeded = identity.is_some();
            let mut acc = identity;
            for v in items {
                let v = match mapper {
                    Some(m) => invoke_closure(vm, m, &[v]),
                    None => v,
                };
                acc = Some(match acc {
                    None => v,
                    Some(a) => invoke_closure(vm, op, &[a, v]),
                });
                if pending() {
                    return Ok(Value::Undef);
                }
            }
            if seeded {
                acc.unwrap_or(Value::Undef)
            } else {
                optional(acc)
            }
        }
        // `toCollection(supplier)`: every element through the collection's
        // own `add`, so a `TreeSet::new` sorts and de-duplicates.
        "toCollection" => {
            let target = invoke_closure(vm, &cargs[0], &[]);
            for v in items {
                coll_method(vm, &target, "add", &[v]);
                if pending() {
                    return Ok(Value::Undef);
                }
            }
            target
        }
        other => {
            return Err(Fault::internal(format!(
                "javars: unsupported collector `Collectors.{other}`"
            )))
        }
    })
}

/// `java.util.Optional`'s instance methods.
///
/// `None` for any other receiver, which leaves the call to the dispatch that
/// follows. The contents are read out before anything runs, because `map`,
/// `filter` and `ifPresent` invoke a user closure that can allocate.
fn optional_method(
    vm: &mut VM,
    recv: &Value,
    method: &str,
    args: &[Value],
) -> Option<Result<Value, Fault>> {
    let (class, inner) = as_optional_full(recv)?;
    let empty = || Fault::java("NoSuchElementException", "No value present");
    Some(match (method, args.len()) {
        ("isPresent", 0) => Ok(Value::bool(inner.is_some())),
        ("isEmpty", 0) => Ok(Value::bool(inner.is_none())),
        // The primitive specializations spell the accessor for their own width;
        // the class already knows which, so one arm serves all four.
        ("get" | "orElseThrow" | "getAsInt" | "getAsLong" | "getAsDouble", 0) => {
            inner.ok_or_else(empty)
        }
        ("orElse", 1) => Ok(inner.unwrap_or_else(|| args[0].clone())),
        ("orElseGet", 1) => Ok(match inner {
            Some(v) => v,
            None => invoke_closure(vm, &args[0], &[]),
        }),
        ("map", 1) => Ok(match inner {
            // `map` answers an empty `Optional` when the mapper answers `null`,
            // which is what distinguishes it from a plain transformation.
            Some(v) => {
                let mapped = invoke_closure(vm, &args[0], &[v]);
                optional((!matches!(mapped, Value::Undef)).then_some(mapped))
            }
            None => optional(None),
        }),
        // `flatMap(f)`: `f`'s own `Optional`, which must not be `null`.
        ("flatMap", 1) => match inner {
            Some(v) => {
                let r = invoke_closure(vm, &args[0], &[v]);
                if matches!(r, Value::Undef) && !pending() {
                    Err(Fault::java("NullPointerException", String::new()))
                } else {
                    Ok(r)
                }
            }
            None => Ok(optional(None)),
        },
        // `or(supplier)`: this `Optional` when present, else the supplier's.
        ("or", 1) => match inner {
            Some(_) => Ok(recv.clone()),
            None => {
                let r = invoke_closure(vm, &args[0], &[]);
                if matches!(r, Value::Undef) && !pending() {
                    Err(Fault::java("NullPointerException", String::new()))
                } else {
                    Ok(r)
                }
            }
        },
        // `orElseThrow(supplier)`: the value, or the supplier's throwable
        // thrown as `throw` throws it.
        ("orElseThrow", 1) => match inner {
            Some(v) => Ok(v),
            None => {
                let exc = invoke_closure(vm, &args[0], &[]);
                if !pending() {
                    if matches!(exc, Value::Undef) {
                        return Some(Err(Fault::java("NullPointerException", String::new())));
                    }
                    PENDING.with(|p| *p.borrow_mut() = Some(exc));
                }
                Ok(Value::Undef)
            }
        },
        // `stream()`: zero or one element, of the `Optional`'s own width.
        ("stream", 0) => Ok(stream_of(
            inner.into_iter().collect(),
            match class {
                "OptionalInt" => StreamKind::Int,
                "OptionalLong" => StreamKind::Long,
                "OptionalDouble" => StreamKind::Double,
                _ => StreamKind::Ref,
            },
        )),
        ("filter", 1) => Ok(match inner {
            Some(v) => {
                let keep = matches!(
                    invoke_closure(vm, &args[0], std::slice::from_ref(&v)),
                    Value::Bool(true)
                );
                optional(keep.then_some(v))
            }
            None => optional(None),
        }),
        ("ifPresent", 1) => {
            if let Some(v) = inner {
                invoke_closure(vm, &args[0], &[v]);
            }
            Ok(Value::Undef)
        }
        ("ifPresentOrElse", 2) => {
            match inner {
                Some(v) => invoke_closure(vm, &args[0], &[v]),
                None => invoke_closure(vm, &args[1], &[]),
            };
            Ok(Value::Undef)
        }
        // A value, so `equals` compares contents where `==` compares handles.
        ("equals", 1) => Ok(Value::bool(match (inner, as_optional_full(&args[0])) {
            (Some(x), Some((other, Some(y)))) => other == class && value_eq(&x, &y),
            (None, Some((other, None))) => other == class,
            _ => false,
        })),
        ("hashCode", 0) => Ok(Value::Int(match inner {
            Some(v) => element_hash(&v).into(),
            None => 0,
        })),
        ("toString", 0) => Ok(Value::str(java_str(recv))),
        _ => Err(Fault::internal(format!(
            "javars: unsupported Optional method `{method}` with {} argument(s)",
            args.len()
        ))),
    })
}

/// The `mods` counter of the `List` at `id`, or 0 for anything else — the
/// baseline an [`HostObj::Iterator`] compares against.
///
/// A `Set` has no counter, so an iterator over one cannot be fail-fast. Java's
/// is; javars's walks the set as it stands, which is noted in BUGS.md rather
/// than papered over with a counter invented here.
fn iter_mods(id: u32) -> u64 {
    list_mods(id as usize).unwrap_or(0)
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
        // Only `map_method` builds one, and a `subList` view is over a `List`.
        NewColl::Entries { .. } => {
            return Err(Fault::internal("javars: a subList has no entry set"))
        }
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
/// A map's [`Fixity`] — `Map.of(…)` is immutable, `new HashMap<>()` is not.
///
/// [`collection_fixity`] answers for a `List` or a `Set` only; a map reaches its
/// refusals through [`map_method`]'s `shape` instead, which the compound
/// methods in [`coll_method`] never see because they run before the borrow.
fn map_fixity(v: &Value) -> Option<Fixity> {
    let Value::Obj(id) = v else {
        return None;
    };
    HEAP.with(|h| match h.borrow().get(*id as usize) {
        Some(HostObj::Map { fixed, .. }) => Some(*fixed),
        _ => None,
    })
}

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
    if !is_callable(vm, cmp) {
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

/// The `null` an immutable collection refuses to be *asked about*.
///
/// `ImmutableCollections` rejects a `null` query as well as a `null` element:
/// `List.of(1, 2).contains(null)` throws rather than answering `false`, and so
/// do `indexOf`/`lastIndexOf`, `Set.of(…).contains`, and `Map.of(…)`'s
/// `get`/`getOrDefault`/`containsKey`/`containsValue`. javars answered
/// `false`/`null` for all of them, which is the worse kind of divergence: a
/// program that reaches the query at all takes a branch here that it cannot
/// take on a JVM.
///
/// Only the *message* still differs, and only for some receiver sizes. Where the
/// JDK reaches the query through `Objects.requireNonNull` — every `List.of`, and
/// the `SetN`/`MapN` shapes — its message is `null` and matches this one exactly.
/// Where it instead reaches an `o.equals(…)`/`pk.hashCode()` on the null itself
/// (`Set12`, `Map1`, `MapN.get`), the JDK's helpful NPE names the internal frame
/// variable — `Cannot invoke "Object.equals(Object)" because "o" is null` — which
/// is the helpful-NPE text BUGS.md already records javars cannot reproduce.
fn reject_null_probe(fixed: Fixity, v: &Value) -> Result<(), Fault> {
    match fixed == Fixity::Immutable && matches!(v, Value::Undef) {
        true => Err(Fault::java("NullPointerException", String::new())),
        false => Ok(()),
    }
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
    // An out-of-range index does not name one exception: the JDK's three list
    // shapes reach the bounds check through three different code paths, and each
    // reports in its own words. javars modelled all three as the `ArrayList`
    // one, so a program that caught `ArrayIndexOutOfBoundsException` around a
    // `List.of(1, 2, 3).get(5)` did not catch here what it catches there.
    // Measured on openjdk 21.0.12:
    //
    //   new ArrayList<>(…).get(5)  IndexOutOfBoundsException       Index 5 out of bounds for length 2
    //   Arrays.asList(…).get(5)    ArrayIndexOutOfBoundsException  Index 5 out of bounds for length 2
    //   List.of(1, 2).get(5)       IndexOutOfBoundsException       Index: 5 Size: 2
    //   List.of(1, 2, 3).get(5)    ArrayIndexOutOfBoundsException  Index 5 out of bounds for length 3
    //
    // The split inside `List.of` is the same one that already decides its class
    // name (`ImmutableCollections$List12` at one or two elements, `$ListN`
    // otherwise, zero included): `List12` holds its elements in two fields and
    // raises `outOfBounds` itself, while `ListN` and `Arrays$ArrayList` index a
    // backing array and let the array's own check fire.
    let bounds = |i: i64, len: usize| -> Result<usize, Fault> {
        if i < 0 || i as usize >= len {
            let (class, msg) = match fixed {
                Fixity::Immutable if (1..=2).contains(&len) => (
                    "IndexOutOfBoundsException",
                    format!("Index: {i} Size: {len}"),
                ),
                Fixity::Immutable | Fixity::FixedSize => (
                    "ArrayIndexOutOfBoundsException",
                    format!("Index {i} out of bounds for length {len}"),
                ),
                Fixity::Mutable => (
                    "IndexOutOfBoundsException",
                    format!("Index {i} out of bounds for length {len}"),
                ),
            };
            return Err(Fault::java(class, msg));
        }
        Ok(i as usize)
    };
    if matches!(
        (method, args.len()),
        ("contains", 1) | ("indexOf", 1) | ("lastIndexOf", 1)
    ) {
        reject_null_probe(fixed, &args[0])?;
    }
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
        // ── `Deque` / `Queue`, on the same `Vec` the `List` methods use ──
        //
        // Index 0 is the head, so `addFirst` inserts there and `addLast` pushes.
        // The three families differ only in what they do when the deque is
        // empty, and a program can see which one it called:
        //
        //   getFirst/getLast/element/removeFirst/removeLast/pop  NoSuchElementException
        //   peek*/poll*                                          null
        //   push/offer*/add*                                     n/a — they never fail
        //
        // `push`/`pop`/`peek` are the *stack* spellings, and they work on the
        // head: `q.push(1)` then `q.push(2)` leaves `[2, 1]`, which is why
        // `push` is `addFirst` and not `add`. `Queue`'s `offer`/`poll` work the
        // other way round — tail in, head out — so `offer` is `addLast`.
        ("addFirst" | "offerFirst" | "push", 1) => {
            structural()?;
            items.insert(0, args[0].clone());
            match method {
                "offerFirst" => Value::bool(true),
                _ => Value::Undef,
            }
        }
        ("addLast" | "offerLast" | "offer", 1) => {
            structural()?;
            items.push(args[0].clone());
            match method {
                "addLast" => Value::Undef,
                _ => Value::bool(true),
            }
        }
        ("getFirst" | "element" | "peekFirst" | "peek", 0)
        | ("getLast" | "peekLast", 0)
        // `Queue.remove()` is `removeFirst()`.
        | ("removeFirst" | "remove" | "pop" | "pollFirst" | "poll", 0)
        | ("removeLast" | "pollLast", 0) => {
            let from_tail = matches!(method, "getLast" | "peekLast" | "removeLast" | "pollLast");
            let removes = matches!(
                method,
                "removeFirst" | "remove" | "pop" | "pollFirst" | "poll" | "removeLast" | "pollLast"
            );
            // The `get`/`remove`/`element`/`pop` spellings throw on empty; the
            // `peek`/`poll` ones answer null. The JDK's
            // `NoSuchElementException` from a deque carries no detail message.
            let throws = matches!(
                method,
                "getFirst" | "getLast" | "element" | "removeFirst" | "removeLast" | "pop" | "remove"
            );
            if items.is_empty() {
                if throws {
                    return Err(Fault::java("NoSuchElementException", String::new()));
                }
                Value::Undef
            } else {
                if removes {
                    structural()?;
                }
                let at = if from_tail { items.len() - 1 } else { 0 };
                match removes {
                    true => items.remove(at),
                    false => items[at].clone(),
                }
            }
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
        ("hashCode", 0) => Value::Int(list_hash(items)),
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
/// The two properties of a `Map` that are not its contents: how it iterates and
/// whether it can be written to. Passed as one value because they always travel
/// together and always come from the same heap object.
#[derive(Clone, Copy)]
struct MapShape {
    /// What order iteration and `toString` present the entries in.
    order: Order,
    /// Whether the map accepts a mutator (`Map.of` does not).
    fixed: Fixity,
}

fn map_method(
    entries: &mut Vec<(Value, Value)>,
    shape: MapShape,
    index: &mut KeyIndex,
    method: &str,
    args: &[Value],
    arg_entries: &[Option<Vec<(Value, Value)>>],
    eq: Option<&EqPlan>,
) -> Result<NewColl, Fault> {
    // Every mutator on a `Map.of` map is Java's `UnsupportedOperationException`,
    // not a silent success. Checked once here, by name, rather than at each
    // arm: an immutable map refuses the whole set, and listing them in one
    // place is what keeps a newly added mutator from quietly escaping the rule.
    let MapShape { order, fixed } = shape;
    if fixed == Fixity::Immutable
        && matches!(
            method,
            "put"
                | "remove"
                | "clear"
                | "putAll"
                | "putIfAbsent"
                | "merge"
                | "replace"
                | "replaceAll"
                | "compute"
                | "computeIfAbsent"
                | "computeIfPresent"
        )
    {
        return Err(Fault::java("UnsupportedOperationException", String::new()));
    }
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
    if matches!(
        (method, args.len()),
        ("get", 1) | ("getOrDefault", 2) | ("containsKey", 1) | ("containsValue", 1)
    ) {
        reject_null_probe(fixed, &args[0])?;
    }
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
            // iterates and prints exactly as the map does. It is marked as a
            // view rather than passed off as a set of its own, which is what
            // makes `m.keySet().add(k)` the `UnsupportedOperationException`
            // Java raises (there is no value to give a bare key) and
            // `m.keySet() instanceof HashSet` the `false` Java answers.
            NewColl::Alloc(HostObj::Set {
                items: ordered,
                order: Order::Insertion,
                // A `keySet` view is removable-through in Java when the map is;
                // javars models it as a copy whose removals `write_through`
                // carries back, accepted where Java accepts them and refused
                // where Java refuses them.
                fixed,
                view: SetView::Keys(ViewOf::of(order, fixed)),
                index: KeyIndex::default(),
            })
        }
        // `entrySet` is the one view whose elements are objects in their own
        // right, so it cannot be built here: an entry carries a handle to the
        // map it came from, and this runs while that map is borrowed. The pairs
        // go back to `coll_method`, which allocates once the borrow is gone.
        ("entrySet", 0) => {
            let keys: Vec<Value> = entries.iter().map(|(k, _)| k.clone()).collect();
            let pairs = present_order(&keys, order)
                .into_iter()
                .map(|i| entries[i].clone())
                .collect();
            NewColl::Entries {
                pairs,
                fixed,
                of: ViewOf::of(order, fixed),
            }
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
                // The view follows the map: a removal through the values of a
                // `new HashMap<>()` is accepted (and carried back to the map by
                // `write_through`) where a removal through a `Map.of`'s is refused. The
                // marker is what refuses `add`, which Java refuses whatever the
                // map, since a bare value has no key to be filed under.
                fixed,
                view: Some(ViewOf::of(order, fixed)),
            })
        }
        ("hashCode", 0) => NewColl::Value(Value::Int(map_hash(entries))),
        // `AbstractMap.equals` — same size, and every key maps to an equal
        // value. Order does not enter into it, which is what makes a `HashMap`
        // equal to a `LinkedHashMap` holding the same entries.
        ("equals", 1) if matches!(eq, Some(EqPlan::Same(_))) => {
            NewColl::Value(Value::bool(matches!(eq, Some(EqPlan::Same(true)))))
        }
        ("equals", 1) => {
            let other = arg_entries.first().cloned().flatten();
            NewColl::Value(Value::bool(match other {
                Some(other) => {
                    other.len() == entries.len()
                        && entries.iter().all(|(k, v)| {
                            other
                                .iter()
                                .find(|(ok, _)| value_eq(ok, k))
                                .is_some_and(|(_, ov)| value_eq(ov, v))
                        })
                }
                None => false,
            }))
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
/// The two properties of a `Set` that are not its contents: whether it can be
/// written to and whether it is a map's view rather than a set of its own.
/// Passed as one value for the same reason as [`MapShape`].
#[derive(Clone, Copy)]
struct SetShape {
    /// Whether the set accepts a mutator (`Set.of` does not).
    fixed: Fixity,
    /// Which map view this is, or `Own` for a freestanding set.
    view: SetView,
}

fn set_method(
    items: &mut Vec<Value>,
    shape: SetShape,
    index: &mut KeyIndex,
    method: &str,
    args: &[Value],
    arg_seqs: &[Option<Vec<Value>>],
    eq: Option<&EqPlan>,
) -> Result<NewColl, Fault> {
    let SetShape { fixed, view } = shape;
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
    if (method, args.len()) == ("contains", 1) {
        reject_null_probe(fixed, &args[0])?;
    }
    // A map view refuses `add` whatever the map's fixity: `m.keySet().add(k)`
    // would have to invent a value for `k`, and `m.entrySet().add(e)` an entry
    // the map does not own. Both are `UnsupportedOperationException` on a
    // `new HashMap<>()` — measured — where the `fixed` guard above, reading
    // `Mutable`, would have let them through.
    if view != SetView::Own && matches!(method, "add" | "addAll") {
        return Err(Fault::java("UnsupportedOperationException", String::new()));
    }
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
        ("hashCode", 0) => Value::Int(set_hash(items)),
        // `AbstractSet.equals` — same size and every element present in the
        // other, again independent of order.
        ("equals", 1) => {
            if let Some(EqPlan::Same(same)) = eq {
                return Ok(NewColl::Value(Value::bool(*same)));
            }
            let other = arg_seqs[0].clone();
            Value::bool(match other {
                Some(other) => {
                    other.len() == items.len()
                        && items.iter().all(|e| other.iter().any(|o| value_eq(o, e)))
                }
                None => false,
            })
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
    let method_name = pop_name(vm);
    let method = method_name.as_str_cow();
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
    if let Some(r) = io_method(&recv, &method, &args) {
        return match r {
            Ok(v) => v,
            Err(f) => raise(vm, f),
        };
    }
    if let Some(r) = pattern_method(&recv, &method, &args) {
        return match r {
            Ok(v) => v,
            Err(f) => raise(vm, f),
        };
    }
    if let Some(r) = matcher_method(vm, &recv, &method, &args) {
        return match r {
            Ok(v) => v,
            Err(f) => raise(vm, f),
        };
    }
    if let Some(v) = atomic_method(vm, &recv, &method, &args) {
        return v;
    }
    if let Some(r) = bitset_method(&recv, &method, &args) {
        return match r {
            Ok(v) => v,
            Err(f) => raise(vm, f),
        };
    }
    if let Some(r) = stats_method(&recv, &method, &args) {
        return match r {
            Ok(v) => v,
            Err(f) => raise(vm, f),
        };
    }
    if let Some(r) = random_method(&recv, &method, &args) {
        return match r {
            Ok(v) => v,
            Err(f) => raise(vm, f),
        };
    }
    // A `Stream` receiver. Every stage and every terminal runs user closures, so
    // it takes the VM and sits with the other handle shapes.
    if let Some(r) = stream_method(vm, &recv, &method, &args) {
        return match r {
            Ok(v) => v,
            Err(f) => raise(vm, f),
        };
    }
    // An `Optional` receiver. Its `map`/`filter`/`ifPresent`/`orElseGet` run a
    // user closure, so it takes the VM and sits with the other handle shapes
    // rather than in the borrow-free tables below.
    if let Some(r) = optional_method(vm, &recv, &method, &args) {
        return match r {
            Ok(v) => v,
            Err(f) => raise(vm, f),
        };
    }
    // An `Iterator` receiver. It is not a collection, so without this it fell
    // through to the `String` table and `it.hasNext()` was
    // ``unsupported String method `hasNext` ``.
    // `it.remove()` over a map view removes the entry from the map as well.
    let through = (method == "remove" && args.is_empty())
        .then(|| iterator_source(&recv))
        .flatten()
        .and_then(|src| Some((map_view_snapshot(&src)?, src)));
    if let Some(r) = iterator_method(&recv, &method, &args) {
        if let (Ok(_), Some(((map, before), src))) = (&r, through) {
            write_through(vm, map, &src, before);
        }
        return match r {
            Ok(v) => v,
            Err(f) => raise(vm, f),
        };
    }
    if let Some(r) = pq_iter_method(vm, &recv, &method, args.len()) {
        return match r {
            Ok(v) => v,
            Err(f) => raise(vm, f),
        };
    }
    // A `Map.Entry` receiver. It is not a collection, so like an `Iterator` it
    // would otherwise fall through to the `String` table and `e.getKey()` would
    // be ``unsupported String method `getKey` ``.
    if let Some(r) = entry_method(&recv, &method, &args) {
        return match r {
            Ok(v) => v,
            Err(f) => raise(vm, f),
        };
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
        if !matches!(method.as_ref(), "equals" | "hashCode" | "getClass") {
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
    // `Object.clone()` on a class instance — reached through `super.clone()`
    // in an override, which is the only way a program calls it: a field-by-
    // field copy of the same runtime class, or `CloneNotSupportedException`
    // naming the class when it is not `Cloneable`.
    if method == "clone" && args.is_empty() {
        if let Some(r) = instance_clone(&recv) {
            return match r {
                Ok(v) => v,
                Err(f) => raise(vm, f),
            };
        }
    }
    // `T[].clone()`, the one member an array declares beyond `Object`'s (JLS
    // 10.7): a new array of the same length holding the same elements — a
    // shallow copy, so the rows of a cloned `int[][]` are shared.
    if method == "clone" && args.is_empty() {
        if let Some(items) = array_items(&recv) {
            return Value::Obj(heap_alloc(HostObj::Array(items)));
        }
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
        match (method.as_ref(), args.len()) {
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
    // Every remaining path reads the receiver as text, and an *argument* may
    // still be a box — a `new String(…)` handle, or a wrapper in
    // `s.equals(anInteger)`. Unboxing them here is the mirror of the receiver
    // unboxing above, and is the identity on everything that is not a box.
    let args: Vec<Value> = args.iter().map(deboxed).collect();
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
        // `intern()` answers the pool's object for the text, which is a
        // *different* object from the receiver whenever the receiver was built
        // at run time — the one string method whose whole purpose is to change
        // which object you hold.
        Ok(v) if method == "intern" => intern(&v),
        // Every other `String` method that would answer text equal to the
        // receiver's answers the receiver itself. That is the JDK's own
        // contract, stated method by method — `trim`, `strip`, `substring`,
        // `toLowerCase`, `replace`, `concat`, `toString` all say "this string if
        // unchanged" — and it is observable now that `==` compares identities:
        // `"ab".trim() == "ab"` is `true` in Java. One rule covers them because
        // the condition the JDK states is the same one each time.
        Ok(Value::Str(text)) if text.as_str() == s.as_ref() => recv,
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
    let tag_name = pop_name(vm);
    let tag = tag_name.as_str_cow();
    // Through any box, for the same reason [`natural_cmp`] is: `compareTo` reads
    // the value, not the handle.
    let b = deboxed(&vm.stack.pop().unwrap_or(Value::Undef));
    let a = deboxed(&vm.stack.pop().unwrap_or(Value::Undef));
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
        let param = match tag.as_ref() {
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
    Value::Int(match tag.as_ref() {
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
        ("substring", 2) | ("subSequence", 2) => substring(s, args[0].jint(), args[1].jint()),
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
        // `codePointBefore` checks `index - 1`, the position it reads.
        ("codePointAt", 1) | ("codePointBefore", 1) => {
            let i = args[0].jint() - i64::from(method == "codePointBefore");
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
        // Both answer the receiver's text; the caller turns that into the
        // receiver itself for `toString` and into the pool's object for
        // `intern`.
        ("intern", 0) | ("toString", 0) => Ok(Value::str(s.to_string())),
        // `contentEquals(CharSequence)` compares against the sequence's text —
        // a `StringBuilder`'s contents, not its handle — and dereferences a
        // `null` argument (`cs.length()`) before comparing anything.
        ("contentEquals", 1) => match &args[0] {
            Value::Undef => Err(Fault::java(
                "NullPointerException",
                "Cannot invoke \"java.lang.CharSequence.length()\" because \"cs\" is null",
            )),
            other => Ok(Value::bool(s == java_str(other))),
        },
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
        // `chars()` and `codePoints()` are the two `IntStream` views of a
        // string. They differ in Java only above the BMP, where `chars()` yields
        // the two surrogate code *units* and `codePoints()` the one code point;
        // javars stores a `String` as Unicode scalars, the same simplification
        // `length` and `charAt` already make, so both read the same values here
        // and `"é".chars().sum()` is 233 on either side.
        ("chars" | "codePoints", 0) => Ok(stream_of(
            s.chars().map(|c| Value::Int(i64::from(c as u32))).collect(),
            StreamKind::Int,
        )),
        // `lines()` splits on *terminators*, not separators: a trailing newline
        // ends the last line rather than starting an empty one, so `"a\n"` is
        // one line and `""` is none. All three of `\n`, `\r\n` and a lone `\r`
        // terminate — Rust's `str::lines` handles the first two and treats a
        // lone `\r` as ordinary text, which is why this scans by hand.
        // `regionMatches([ignoreCase,] toffset, other, ooffset, len)`: the
        // UTF-16 windows compared unit by unit, `false` for a window that
        // falls outside either string. The case-blind form is
        // `String.regionMatchesCI`'s: equal, or equal upper-cased, or equal
        // lower-cased after upper-casing.
        ("regionMatches", 4 | 5) => {
            let (ci, a) = if args.len() == 5 {
                (java_bool(&args[0]), &args[1..])
            } else {
                (false, args)
            };
            let other = a[1].as_str_cow();
            let (to, oo, len) = (a[0].jint(), a[2].jint(), a[3].jint());
            let x: Vec<u16> = s.encode_utf16().collect();
            let y: Vec<u16> = other.encode_utf16().collect();
            if oo < 0 || to < 0 || to > x.len() as i64 - len || oo > y.len() as i64 - len {
                return Ok(Value::bool(false));
            }
            let fold = |u: u16, upper: bool| -> u32 {
                match char::from_u32(u32::from(u)) {
                    Some(c) if upper => one_to_one_case(c, char::to_uppercase) as u32,
                    Some(c) => one_to_one_case(c, char::to_lowercase) as u32,
                    None => u32::from(u),
                }
            };
            let same = (0..len.max(0) as usize).all(|i| {
                let (c1, c2) = (x[to as usize + i], y[oo as usize + i]);
                c1 == c2
                    || (ci && {
                        let (u1, u2) = (fold(c1, true), fold(c2, true));
                        u1 == u2 || fold(u1 as u16, false) == fold(u2 as u16, false)
                    })
            });
            Ok(Value::bool(same))
        }
        // `codePointCount(begin, end)` over UTF-16 indices: a surrogate pair
        // inside the range is one code point, an unpaired surrogate one too.
        ("codePointCount", 2) => {
            let units: Vec<u16> = s.encode_utf16().collect();
            let (b, e) = (args[0].jint(), args[1].jint());
            if b < 0 || e > units.len() as i64 || b > e {
                return Err(Fault::java(
                    "IndexOutOfBoundsException",
                    format!("Range [{b}, {e}) out of bounds for length {}", units.len()),
                ));
            }
            let n = char::decode_utf16(units[b as usize..e as usize].iter().copied()).count();
            Ok(Value::Int(n as i64))
        }
        ("lines", 0) => Ok(stream_of(
            java_lines(s).into_iter().map(Value::str).collect(),
            StreamKind::Ref,
        )),
        ("translateEscapes", 0) => translate_escapes(s).map(Value::str),
        // `indent(n)`: `lines()`, each given `n` leading spaces or relieved of
        // up to `-n` leading whitespace characters (all of them for
        // `Integer.MIN_VALUE`), joined with `\n` and ended with one.
        ("indent", 1) => {
            if s.is_empty() {
                return Ok(Value::str(String::new()));
            }
            let n = args[0].jint();
            let mut out = String::new();
            for line in java_lines(s) {
                if n > 0 {
                    out.push_str(&" ".repeat(n as usize));
                    out.push_str(&line);
                } else if n == i64::from(i32::MIN) {
                    out.push_str(line.trim_start_matches(java_is_whitespace));
                } else {
                    let ws = line.chars().take_while(|&c| java_is_whitespace(c)).count();
                    let cut = ws.min(n.unsigned_abs() as usize);
                    out.extend(line.chars().skip(cut));
                }
                out.push('\n');
            }
            Ok(Value::str(out))
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
    let method_name = pop_name(vm);
    let class_name = pop_name(vm);
    let method = method_name.as_str_cow();
    let class = class_name.as_str_cow();
    let n = argc.saturating_sub(2) as usize; // minus class name and method name
    let mut args = Vec::with_capacity(n);
    for _ in 0..n {
        args.push(vm.stack.pop().unwrap_or(Value::Undef));
    }
    args.reverse();
    if let Some(r) = regex_static(&class, &method, &args) {
        return match r {
            Ok(v) => v,
            Err(f) => raise(vm, f),
        };
    }
    if let Some(r) = system_static(vm, &class, &method, &args) {
        return match r {
            Ok(v) => v,
            Err(f) => raise(vm, f),
        };
    }
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

// ── java.lang.System and console/text input ─────────────────────────────────

thread_local! {
    /// The status `System.exit` asked for, read by the `java` binary once the
    /// VM has halted. Cleared with the heap.
    static EXIT_CODE: Cell<Option<i32>> = const { Cell::new(None) };
    /// The heap handle of `System.in`, so the stream is one object however
    /// often it is named (`System.in == System.in`). Cleared with the heap.
    static STDIN_HANDLE: Cell<Option<u32>> = const { Cell::new(None) };
    /// The generator behind `Math.random()`, created on first use.
    static MATH_RANDOM: RefCell<Option<crate::jrandom::Random>> = const { RefCell::new(None) };
    /// The generator behind the one-argument `Collections.shuffle`.
    static SHUFFLE_RANDOM: RefCell<Option<crate::jrandom::Random>> = const { RefCell::new(None) };
}

/// The status a `System.exit` call asked for, if the program made one.
pub fn exit_code() -> Option<i32> {
    EXIT_CODE.with(Cell::get)
}

/// The `System` members beyond the two output streams, and the constructors of
/// the input classes [`crate::jio`] models. `None` for every other static.
fn system_static(
    vm: &mut VM,
    class: &str,
    method: &str,
    args: &[Value],
) -> Option<Result<Value, Fault>> {
    let null = |v: &Value| matches!(v, Value::Undef);
    let npe = || Err(Fault::java("NullPointerException", String::new()));
    let reader_of = |v: &Value| -> Option<u32> {
        match v {
            Value::Obj(id) => HEAP.with(|h| {
                matches!(h.borrow().get(*id as usize), Some(HostObj::Reader(_))).then_some(*id)
            }),
            _ => None,
        }
    };
    // A reader that takes over another reader's characters.
    let wrap = |kind: crate::jio::Kind, inner: u32| -> Value {
        let r = HEAP.with(|h| match h.borrow_mut().get_mut(inner as usize) {
            Some(HostObj::Reader(r)) => crate::jio::Reader::wrap(kind, r),
            _ => crate::jio::Reader::stdin(kind),
        });
        Value::Obj(heap_alloc(HostObj::Reader(r)))
    };
    use crate::jio::{Kind, Reader};
    Some(match (class, method, args) {
        ("System", "#in", []) => Ok(Value::Obj(STDIN_HANDLE.with(|s| match s.get() {
            Some(id) => id,
            None => {
                let id = heap_alloc(HostObj::Reader(Reader::stdin(Kind::InputStream)));
                s.set(Some(id));
                id
            }
        }))),
        // `System.exit` ends the program where it stands: no `finally` runs
        // and nothing after the call executes. The status is Java's `int`,
        // which the process reports modulo 256.
        ("System", "exit", [code]) => {
            EXIT_CODE.with(|c| c.set(Some(code.jint() as i32)));
            vm.request_halt();
            Ok(Value::Undef)
        }
        ("System", "currentTimeMillis", []) => Ok(Value::Int(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as i64),
        )),
        // An arbitrary but fixed origin, as Java's is: only differences between
        // two readings mean anything.
        ("System", "nanoTime", []) => {
            thread_local!(static ORIGIN: std::time::Instant = std::time::Instant::now());
            Ok(Value::Int(ORIGIN.with(|o| o.elapsed().as_nanos() as i64)))
        }
        ("System", "lineSeparator", []) => Ok(Value::str("\n".to_string())),
        ("System", "#flushOut", []) => {
            use std::io::Write;
            let _ = std::io::stdout().flush();
            Ok(Value::Undef)
        }
        ("System", "#flushErr", []) => {
            use std::io::Write;
            let _ = std::io::stderr().flush();
            Ok(Value::Undef)
        }
        ("System", "arraycopy", [src, sp, dst, dp, len, sty, dty]) => array_copy(
            src,
            sp.jint(),
            dst,
            dp.jint(),
            len.jint(),
            &sty.as_str_cow(),
            &dty.as_str_cow(),
        ),
        // `new AbstractMap.SimpleEntry<>(k, v)` and its immutable sibling, and
        // the copy constructor of each, which reads `getKey`/`getValue` off
        // any `Map.Entry`. Both accept `null` for either half.
        ("SimpleEntry" | "SimpleImmutableEntry", "#new", [k, v]) => Ok(alloc_pair(
            k.clone(),
            v.clone(),
            None,
            simple_pair_kind(class),
        )),
        ("SimpleEntry" | "SimpleImmutableEntry", "#new", [e]) => match entry_pair(e) {
            Some(p) => Ok(alloc_pair(p.key, p.value, None, simple_pair_kind(class))),
            None => npe(),
        },
        ("Scanner", "#new", [a]) if null(a) => npe(),
        ("Scanner", "#new", [Value::Str(s)]) => Ok(Value::Obj(heap_alloc(HostObj::Reader(
            Reader::text(Kind::Scanner, s),
        )))),
        ("Scanner", "#new", [a]) => Ok(wrap(Kind::Scanner, reader_of(a)?)),
        ("StringReader", "#new", [a]) if null(a) => npe(),
        ("StringReader", "#new", [a]) => Ok(Value::Obj(heap_alloc(HostObj::Reader(Reader::text(
            Kind::StringReader,
            &a.as_str_cow(),
        ))))),
        ("InputStreamReader" | "BufferedReader", "#new", [a, ..]) if null(a) => npe(),
        ("InputStreamReader", "#new", [a, ..]) => Ok(wrap(Kind::InputStreamReader, reader_of(a)?)),
        ("BufferedReader", "#new", [a, ..]) => Ok(wrap(Kind::BufferedReader, reader_of(a)?)),
        ("Random", "#new", []) => Ok(Value::Obj(heap_alloc(HostObj::Random(
            crate::jrandom::Random::unseeded(),
        )))),
        ("Random", "#new", [seed]) => Ok(Value::Obj(heap_alloc(HostObj::Random(
            crate::jrandom::Random::new(seed.jint()),
        )))),
        ("AtomicInteger" | "AtomicLong" | "AtomicBoolean", "#new", rest) if rest.len() <= 1 => {
            let kind = match class {
                "AtomicInteger" => AtomicKind::Int,
                "AtomicLong" => AtomicKind::Long,
                _ => AtomicKind::Bool,
            };
            let init = rest.first().cloned().unwrap_or(match kind {
                AtomicKind::Bool => Value::bool(false),
                _ => Value::Int(0),
            });
            Ok(Value::Obj(heap_alloc(HostObj::Atomic {
                kind,
                value: atomic_norm(kind, &init),
            })))
        }
        ("BitSet", "#new", []) => Ok(Value::Obj(heap_alloc(HostObj::Bits(
            crate::jbitset::BitSet::new(),
        )))),
        ("BitSet", "#new", [n]) => crate::jbitset::BitSet::with_bits(n.jint())
            .map(|b| Value::Obj(heap_alloc(HostObj::Bits(b))))
            .map_err(|(class, msg)| Fault::java(class, msg)),
        ("IntSummaryStatistics", "#new", []) => Ok(Value::Obj(heap_alloc(HostObj::Stats(
            SummaryStats::new(StreamKind::Int),
        )))),
        ("LongSummaryStatistics", "#new", []) => Ok(Value::Obj(heap_alloc(HostObj::Stats(
            SummaryStats::new(StreamKind::Long),
        )))),
        ("DoubleSummaryStatistics", "#new", []) => Ok(Value::Obj(heap_alloc(HostObj::Stats(
            SummaryStats::new(StreamKind::Double),
        )))),
        // `Math.random()` is `nextDouble()` of one generator the JDK creates on
        // first use, unseeded.
        ("Math", "random", []) => Ok(Value::float(MATH_RANDOM.with(|m| {
            m.borrow_mut()
                .get_or_insert_with(crate::jrandom::Random::unseeded)
                .next_double()
        }))),
        ("StringTokenizer", "#new", [s, rest @ ..]) if rest.len() <= 2 => {
            if null(s) || rest.first().is_some_and(null) {
                return Some(npe());
            }
            let delims = rest.first().map(|d| d.as_str_cow().into_owned());
            let ret = rest.get(1).is_some_and(|b| matches!(b, Value::Bool(true)));
            Ok(Value::Obj(heap_alloc(HostObj::Tokenizer(
                crate::jio::Tokenizer::new(&s.as_str_cow(), delims.as_deref(), ret),
            ))))
        }
        _ => return None,
    })
}

/// `System.arraycopy`, with the JDK's checks in the JDK's order: `null`, a
/// non-array, an element-type mismatch, then the four index checks. The
/// element types are erased at run time, so the compiler passes each array's
/// static type (`int[]`, `String[]`, or empty when unknown), which is also what
/// the messages name — a primitive array as `int[5]`, any reference array as
/// `object array[5]`.
fn array_copy(
    src: &Value,
    sp: i64,
    dst: &Value,
    dp: i64,
    len: i64,
    sty: &str,
    dty: &str,
) -> Result<Value, Fault> {
    let (Value::Obj(sid), Value::Obj(did)) = (src, dst) else {
        if matches!(src, Value::Undef) || matches!(dst, Value::Undef) {
            return Err(Fault::java("NullPointerException", String::new()));
        }
        let (which, v) = if matches!(src, Value::Obj(_)) {
            ("destination", dst)
        } else {
            ("source", src)
        };
        let class = value_class(v)
            .map(|c| binary_name(&c, v).unwrap_or(c))
            .unwrap_or_default();
        return Err(Fault::java(
            "ArrayStoreException",
            format!("arraycopy: {which} type {class} is not an array"),
        ));
    };
    let items = |id: u32| {
        HEAP.with(|h| match h.borrow().get(id as usize) {
            Some(HostObj::Array(a)) => Some(a.clone()),
            _ => None,
        })
    };
    for (which, v, id) in [("source", src, *sid), ("destination", dst, *did)] {
        if items(id).is_none() {
            let class = value_class(v)
                .map(|c| binary_name(&c, v).unwrap_or(c))
                .unwrap_or_default();
            return Err(Fault::java(
                "ArrayStoreException",
                format!("arraycopy: {which} type {class} is not an array"),
            ));
        }
    }
    let from = items(*sid).unwrap_or_default();
    let to_len = items(*did).map_or(0, |a| a.len());
    // A primitive element type is named as itself; every reference array is
    // an `object array`.
    let elem = |ty: &str| -> &'static str {
        match ty.strip_suffix("[]") {
            Some("int") => "int",
            Some("long") => "long",
            Some("short") => "short",
            Some("byte") => "byte",
            Some("char") => "char",
            Some("boolean") => "boolean",
            Some("float") => "float",
            Some("double") => "double",
            _ => "object array",
        }
    };
    let (se, de) = (elem(sty), elem(dty));
    let known = !sty.is_empty() && !dty.is_empty();
    if known && se != de && (se != "object array" || de != "object array") {
        return Err(Fault::java(
            "ArrayStoreException",
            format!("arraycopy: type mismatch: can not copy {se}[] into {de}[]"),
        ));
    }
    let oob = |msg: String| {
        Err(Fault::java(
            "ArrayIndexOutOfBoundsException",
            format!("arraycopy: {msg}"),
        ))
    };
    if sp < 0 {
        return oob(format!(
            "source index {sp} out of bounds for {se}[{}]",
            from.len()
        ));
    }
    if dp < 0 {
        return oob(format!(
            "destination index {dp} out of bounds for {de}[{to_len}]"
        ));
    }
    if len < 0 {
        return oob(format!("length {len} is negative"));
    }
    if sp + len > from.len() as i64 {
        return oob(format!(
            "last source index {} out of bounds for {se}[{}]",
            sp + len,
            from.len()
        ));
    }
    if dp + len > to_len as i64 {
        return oob(format!(
            "last destination index {} out of bounds for {de}[{to_len}]",
            dp + len
        ));
    }
    // Copied through a snapshot of the source range, which is what makes an
    // overlapping copy within one array behave "as if" through a temporary.
    let chunk = from[sp as usize..(sp + len) as usize].to_vec();
    HEAP.with(|h| {
        if let Some(HostObj::Array(a)) = h.borrow_mut().get_mut(*did as usize) {
            a[dp as usize..(dp + len) as usize].clone_from_slice(&chunk);
        }
    });
    Ok(Value::Undef)
}

/// A method call on a `System.in`/`Scanner`/reader/`StringTokenizer`
/// receiver, or `None` when the receiver is none of those.
/// Run `f` on the `java.util.Random` a handle names; `None` for any other
/// value, `null` included.
fn with_random<R>(v: &Value, f: impl FnOnce(&mut crate::jrandom::Random) -> R) -> Option<R> {
    let Value::Obj(id) = v else {
        return None;
    };
    HEAP.with(|h| match h.borrow_mut().get_mut(*id as usize) {
        Some(HostObj::Random(r)) => Some(f(r)),
        _ => None,
    })
}

/// A method call on a `java.util.Random` receiver; `None` for any other.
///
/// The overloads are told apart by arity and, for the bounded draws, by the
/// argument the compiler passed: `nextInt(int)`, `nextLong(long)` and
/// `nextDouble(double)` each have their own bound check and message
/// (`bound must be positive`, `bound must be finite and positive`).
fn random_method(recv: &Value, method: &str, args: &[Value]) -> Option<Result<Value, Fault>> {
    let iae = |m: &str| Fault::java("IllegalArgumentException", m.to_string());
    let int = |r: Result<i32, &str>| r.map(|n| Value::Int(i64::from(n))).map_err(iae);
    let long = |r: Result<i64, &str>| r.map(Value::Int).map_err(iae);
    let double = |r: Result<f64, &str>| r.map(Value::float).map_err(iae);
    // `nextBytes` writes into an array the generator does not own, so the
    // bytes are drawn first and stored once the receiver's borrow is released.
    if (method, args.len()) == ("nextBytes", 1) {
        let len = array_items(&args[0])?.len();
        let bytes = with_random(recv, |r| r.next_bytes(len))?;
        let filled: Vec<Value> = bytes
            .into_iter()
            .map(|b| Value::Int(i64::from(b)))
            .collect();
        return Some(array_mutate(&args[0], |a| *a = filled).map(|()| Value::Undef));
    }
    with_random(recv, |r| match (method, args) {
        ("nextInt", []) => Ok(Value::Int(i64::from(r.next_int()))),
        ("nextInt", [b]) => int(r.next_int_bounded(b.jint() as i32)),
        ("nextInt", [o, b]) => int(r.next_int_range(o.jint() as i32, b.jint() as i32)),
        ("nextLong", []) => Ok(Value::Int(r.next_long())),
        ("nextLong", [b]) => long(r.next_long_bounded(b.jint())),
        ("nextLong", [o, b]) => long(r.next_long_range(o.jint(), b.jint())),
        ("nextDouble", []) => Ok(Value::float(r.next_double())),
        ("nextDouble", [b]) => double(r.next_double_bounded(b.jfloat())),
        ("nextDouble", [o, b]) => double(r.next_double_range(o.jfloat(), b.jfloat())),
        ("nextFloat", []) => Ok(Value::float(f64::from(r.next_float()))),
        ("nextBoolean", []) => Ok(Value::bool(r.next_boolean())),
        ("nextGaussian", []) => Ok(Value::float(r.next_gaussian())),
        ("setSeed", [s]) => {
            r.set_seed(s.jint());
            Ok(Value::Undef)
        }
        _ => Err(Fault::internal(format!(
            "javars: unsupported Random method `{method}` with {} argument(s)",
            args.len()
        ))),
    })
}

fn io_method(recv: &Value, method: &str, args: &[Value]) -> Option<Result<Value, Fault>> {
    use crate::jio::{ArgVal, Out};
    let Value::Obj(id) = recv else {
        return None;
    };
    let jargs: Vec<ArgVal> = args
        .iter()
        .map(|a| match a {
            Value::Int(n) => ArgVal::Int(*n),
            other => ArgVal::Str(other.as_str_cow().into_owned()),
        })
        .collect();
    let out = HEAP.with(|h| match h.borrow_mut().get_mut(*id as usize) {
        Some(HostObj::Reader(r)) => Some(r.call(method, &jargs)),
        Some(HostObj::Tokenizer(t)) => Some(t.call(method, args.len())),
        _ => None,
    })?;
    let Some(out) = out else {
        return Some(Err(Fault::internal(format!(
            "javars: unsupported method `{method}` with {} argument(s) on {}",
            args.len(),
            value_class(recv).unwrap_or_default()
        ))));
    };
    Some(match out {
        Ok(Out::Str(s)) => Ok(Value::str(s)),
        Ok(Out::Int(n)) => Ok(Value::Int(n)),
        Ok(Out::Float(f)) => Ok(Value::float(f)),
        Ok(Out::Bool(b)) => Ok(Value::bool(b)),
        Ok(Out::Null | Out::Unit) => Ok(Value::Undef),
        Ok(Out::Lines(ls)) => Ok(stream_of(
            ls.into_iter().map(Value::str).collect(),
            StreamKind::Ref,
        )),
        Err((class, msg)) => Err(Fault::java(class, msg.unwrap_or_default())),
    })
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
        // `String.valueOf(Object)` is specified as `obj.toString()`, and a
        // `String`'s `toString()` is `this` — so it answers the very same
        // object, which `==` can now see.
        ("String", "valueOf", 1) if matches!(args[0], Value::Str(_)) => args[0].clone(),
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

/// `List.of`/`Set.of` refuse a `null` element, as `ImmutableCollections` does.
///
/// `Map.of` already rejected a `null` key or value; the two element factories
/// did not, so `List.of(1, null)` built a two-element list and printed
/// `[1, null]` where the JDK throws before the list exists. That is the
/// permissive direction — a program the reference refuses to run answered here
/// — and it is the one an `Optional`/null-check idiom leans on.
///
/// The JDK's `Objects.requireNonNull` carries no detail message, so neither does
/// this: `e.getMessage()` is `null` on both sides.
fn reject_null_element(items: &[Value]) -> Result<(), Fault> {
    match items.iter().any(|v| matches!(v, Value::Undef)) {
        true => Err(Fault::java("NullPointerException", String::new())),
        false => Ok(()),
    }
}

/// The hash `Objects.hashCode`/`Objects.hash` answers for one value.
///
/// A collection hashes by its *contents*, not by its handle:
/// `Objects.hashCode(List.of(0, 0))` is 961, the `31 * h + e` fold, and reading
/// the handle instead answered the slab index. [`collection_hash`] is the same
/// computation `list.hashCode()` already used, so the two agree by
/// construction rather than by a second implementation.
fn objects_hash_of(vm: &mut VM, v: &Value) -> i32 {
    if matches!(v, Value::Undef) {
        return 0;
    }
    if let Some(Value::Int(h)) = collection_hash(vm, v, "hashCode", 0) {
        return h as i32;
    }
    user_element_hash(vm, v).unwrap_or_else(|| element_hash(v))
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
            view: None,
        })))
    };
    Some(match (class, method) {
        // `Arrays.asList` is a fixed-size *view*: `set` works, `add` throws.
        ("Arrays", "asList") => list(varargs_items(args), Fixity::FixedSize),
        ("List", "of") => {
            let items = varargs_items(args);
            if let Err(f) = reject_null_element(&items) {
                return Some(Err(f));
            }
            list(items, Fixity::Immutable)
        }
        // `Set.of` is the one set-building factory that *rejects* a repeat
        // rather than dropping it — `Set.of(1, 1)` is an
        // `IllegalArgumentException` naming the element, not a one-element set.
        // Silently de-duplicating turned a program Java refuses to run into one
        // that ran and answered.
        ("Set", "of") => {
            let items = varargs_items(args);
            if let Err(f) = reject_null_element(&items) {
                return Some(Err(f));
            }
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
                order: Order::HASH,
                fixed: Fixity::Immutable,
                view: SetView::Own,
                index: KeyIndex::default(),
            })))
        }
        // `List.copyOf`/`Set.copyOf`/`Map.copyOf`: immutable copies that refuse
        // a `null` element, key or value as the `of` factories do. `Set.copyOf`
        // drops a repeat where `Set.of` refuses one — it is a copy of a
        // collection that may legitimately hold equal elements.
        ("List", "copyOf") if args.len() == 1 => {
            let Some(items) = sequence_items(&args[0]) else {
                return Some(Err(Fault::java("NullPointerException", String::new())));
            };
            if let Err(f) = reject_null_element(&items) {
                return Some(Err(f));
            }
            list(items, Fixity::Immutable)
        }
        ("Set", "copyOf") if args.len() == 1 => {
            let Some(items) = sequence_items(&args[0]) else {
                return Some(Err(Fault::java("NullPointerException", String::new())));
            };
            if let Err(f) = reject_null_element(&items) {
                return Some(Err(f));
            }
            Ok(Value::Obj(heap_alloc(HostObj::Set {
                items: distinct(vm, &items),
                order: Order::HASH,
                fixed: Fixity::Immutable,
                view: SetView::Own,
                index: KeyIndex::default(),
            })))
        }
        ("Map", "copyOf") if args.len() == 1 => {
            let Some(entries) = map_entries(&args[0]) else {
                return Some(Err(Fault::java("NullPointerException", String::new())));
            };
            if entries
                .iter()
                .any(|(k, v)| matches!(k, Value::Undef) || matches!(v, Value::Undef))
            {
                return Some(Err(Fault::java("NullPointerException", String::new())));
            }
            Ok(Value::Obj(heap_alloc(HostObj::Map {
                entries,
                order: Order::HASH,
                fixed: Fixity::Immutable,
                index: KeyIndex::default(),
            })))
        }
        // `Collections.emptySet`/`emptyMap`/`singleton`/`singletonMap`: the
        // immutable one- and zero-element collections. Unlike `Set.of`/`Map.of`
        // they accept a `null` element, key or value.
        // `Arrays.equals(a, b)`: the same array (or both `null`) is equal, one
        // `null` is not, and otherwise the lengths and then the elements —
        // `Objects.equals` for a reference array, so a user `equals` and a
        // collection's structural one decide; a `double[]` compares the
        // `doubleToLongBits` patterns, which [`value_eq`] already does.
        ("Arrays", "equals") if args.len() == 2 => {
            if same_array(&args[0], &args[1]) {
                return Some(Ok(Value::bool(true)));
            }
            let (Some(a), Some(b)) = (array_items(&args[0]), array_items(&args[1])) else {
                return Some(Ok(Value::bool(false)));
            };
            Ok(Value::bool(
                a.len() == b.len() && arrays_mismatch(vm, &a, &b, false).is_none(),
            ))
        }
        // `deepEquals` recurses into element pairs that are both arrays.
        ("Arrays", "deepEquals") if args.len() == 2 => {
            if same_array(&args[0], &args[1]) {
                return Some(Ok(Value::bool(true)));
            }
            let (Some(a), Some(b)) = (array_items(&args[0]), array_items(&args[1])) else {
                return Some(Ok(Value::bool(false)));
            };
            Ok(Value::bool(
                a.len() == b.len() && arrays_mismatch(vm, &a, &b, true).is_none(),
            ))
        }
        // `mismatch(a, b)`: the first index whose elements differ, else the
        // shorter length when one is a proper prefix of the other, else -1.
        // Both lengths are read first, so a `null` array is the NPE naming the
        // JDK's parameter.
        ("Arrays", "mismatch") if args.len() == 2 => {
            let Some(a) = array_items(&args[0]) else {
                return Some(Err(array_length_npe("a")));
            };
            let Some(b) = array_items(&args[1]) else {
                return Some(Err(array_length_npe("b")));
            };
            if same_array(&args[0], &args[1]) {
                return Some(Ok(Value::Int(-1)));
            }
            let n = a.len().min(b.len());
            let at = arrays_mismatch(vm, &a[..n], &b[..n], false);
            if pending() {
                return Some(Ok(Value::Undef));
            }
            Ok(Value::Int(match at {
                Some(i) => i as i64,
                None if a.len() != b.len() => n as i64,
                None => -1,
            }))
        }
        // `compare(a, b)`: lexicographic over the first mismatch, `null` first,
        // then the length difference. The compiler appends both arrays' static
        // types, because the element comparison is the element type's own:
        // `Integer.compare`'s -1/0/1 for `int[]`, but the *difference* for
        // `byte[]`, `short[]` and `char[]`, `String.compareTo`'s for `String[]`.
        ("Arrays", "compare") if args.len() == 4 => {
            if same_array(&args[0], &args[1]) {
                return Some(Ok(Value::Int(0)));
            }
            let (a, b) = match (array_items(&args[0]), array_items(&args[1])) {
                (Some(a), Some(b)) => (a, b),
                (None, _) => return Some(Ok(Value::Int(-1))),
                (_, None) => return Some(Ok(Value::Int(1))),
            };
            let elem = args[2].as_str_cow().trim_end_matches("[]").to_string();
            let n = a.len().min(b.len());
            let at = arrays_mismatch(vm, &a[..n], &b[..n], false);
            if pending() {
                return Some(Ok(Value::Undef));
            }
            match at {
                Some(i) => match (&a[i], &b[i]) {
                    (Value::Undef, _) => Ok(Value::Int(-1)),
                    (_, Value::Undef) => Ok(Value::Int(1)),
                    (x, y) => match array_element_compare(vm, x, y, &elem) {
                        Some(c) => Ok(Value::Int(c)),
                        None => return Some(Ok(Value::Undef)),
                    },
                },
                None => Ok(Value::Int(a.len() as i64 - b.len() as i64)),
            }
        }
        ("Collections", "emptySet") if args.is_empty() => {
            Ok(Value::Obj(heap_alloc(HostObj::Set {
                items: Vec::new(),
                order: Order::Insertion,
                fixed: Fixity::Immutable,
                view: SetView::Own,
                index: KeyIndex::default(),
            })))
        }
        ("Collections", "singleton") if args.len() == 1 => {
            Ok(Value::Obj(heap_alloc(HostObj::Set {
                items: vec![args[0].clone()],
                order: Order::Insertion,
                fixed: Fixity::Immutable,
                view: SetView::Own,
                index: KeyIndex::default(),
            })))
        }
        // `emptySortedSet`/`emptyNavigableSet`: an immutable sorted set, so
        // `first()` is `NoSuchElementException` and `add` is
        // `UnsupportedOperationException`.
        ("Collections", "emptySortedSet" | "emptyNavigableSet") if args.is_empty() => {
            Ok(Value::Obj(heap_alloc(HostObj::Set {
                items: Vec::new(),
                order: Order::Sorted {
                    by_cmp: false,
                    desc: false,
                },
                fixed: Fixity::Immutable,
                view: SetView::Own,
                index: KeyIndex::default(),
            })))
        }
        // `nCopies(n, o)`: an immutable list of `n` references to `o`. A
        // negative length is refused with `CopiesList`'s own message.
        ("Collections", "nCopies") if args.len() == 2 => {
            let n = args[0].jint();
            if n < 0 {
                return Some(Err(Fault::java(
                    "IllegalArgumentException",
                    format!("List length = {n}"),
                )));
            }
            list(vec![args[1].clone(); n as usize], Fixity::Immutable)
        }
        // The empty iterators: a cursor over an empty immutable list, so
        // `hasNext` is false, `next` is `NoSuchElementException` and `remove`
        // is `IllegalStateException`, as `Collections.EmptyIterator` answers.
        // An `Enumeration` is the same cursor under its own two method names.
        ("Collections", "emptyIterator" | "emptyEnumeration" | "emptyListIterator")
            if args.is_empty() =>
        {
            let Ok(Value::Obj(source)) = list(Vec::new(), Fixity::Immutable) else {
                unreachable!("`list` allocates a handle");
            };
            Ok(new_iterator(source, method == "emptyListIterator"))
        }
        // `enumeration(c)` walks `c` through its own iterator, so a change to
        // `c` behind it is a `ConcurrentModificationException`, as the JDK's
        // is.
        ("Collections", "enumeration") if args.len() == 1 => match &args[0] {
            Value::Obj(source) if sequence_items(&args[0]).is_some() => {
                Ok(new_iterator(*source, false))
            }
            _ => Err(Fault::java("NullPointerException", String::new())),
        },
        // `list(e)`: the elements the enumeration has left, in an `ArrayList`.
        ("Collections", "list") if args.len() == 1 => {
            let mut items = Vec::new();
            loop {
                match iterator_method(&args[0], "hasMoreElements", &[]) {
                    Some(Ok(more)) if more.is_truthy() => {}
                    Some(Ok(_)) => break,
                    Some(Err(f)) => return Some(Err(f)),
                    None => return Some(Err(Fault::java("NullPointerException", String::new()))),
                }
                match iterator_method(&args[0], "nextElement", &[]) {
                    Some(Ok(v)) => items.push(v),
                    Some(Err(f)) => return Some(Err(f)),
                    None => break,
                }
            }
            list(items, Fixity::Mutable)
        }
        ("Collections", "emptyMap") if args.is_empty() => {
            Ok(Value::Obj(heap_alloc(HostObj::Map {
                entries: Vec::new(),
                order: Order::Insertion,
                fixed: Fixity::Immutable,
                index: KeyIndex::default(),
            })))
        }
        ("Collections", "singletonMap") if args.len() == 2 => {
            Ok(Value::Obj(heap_alloc(HostObj::Map {
                entries: vec![(args[0].clone(), args[1].clone())],
                order: Order::Insertion,
                fixed: Fixity::Immutable,
                index: KeyIndex::default(),
            })))
        }
        // Whether `x` is a *key extractor* rather than a comparator, read off
        // the lambda's declared parameter count. `Comparator.thenComparing` is
        // overloaded on the two in Java and told apart by the target type,
        // which javars has no pass for; the arity is exact and is what the
        // prelude's `asComparator` branches on.
        ("Comparator", "isKeyExtractor") if args.len() == 1 => {
            Ok(Value::bool(callable_arity(vm, &args[0]) == Some(1)))
        }
        // ── java.util.stream sources ──
        ("Stream", "of") => Ok(stream_of(varargs_items(args), StreamKind::Ref)),
        // The three-argument `iterate(seed, hasNext, f)` bounds itself, so it is
        // collected up front; the unbounded two-argument form and `generate`
        // are a lazy [`Source`] below.
        ("Stream" | "IntStream" | "LongStream" | "DoubleStream", "iterate") if args.len() == 3 => {
            let mut items = Vec::new();
            let mut cur = args[0].clone();
            while matches!(
                invoke_closure(vm, &args[1], std::slice::from_ref(&cur)),
                Value::Bool(true)
            ) {
                items.push(cur.clone());
                cur = invoke_closure(vm, &args[2], &[cur]);
            }
            Ok(stream_of(
                items,
                if class == "Stream" {
                    StreamKind::Ref
                } else {
                    primitive_stream_kind(class)
                },
            ))
        }
        ("Stream" | "IntStream" | "LongStream" | "DoubleStream", "empty") if args.is_empty() => {
            Ok(stream_of(
                Vec::new(),
                if class == "Stream" {
                    StreamKind::Ref
                } else {
                    primitive_stream_kind(class)
                },
            ))
        }
        ("Stream", "ofNullable") if args.len() == 1 => Ok(stream_of(
            args.iter()
                .filter(|v| !matches!(v, Value::Undef))
                .cloned()
                .collect(),
            StreamKind::Ref,
        )),
        // `concat(a, b)` takes its shape from `a` — `IntStream.concat` answers
        // an `IntStream` — and a `null` part is the NPE `Objects.requireNonNull`
        // raises.
        ("Stream" | "IntStream" | "LongStream" | "DoubleStream", "concat") if args.len() == 2 => {
            let Some((_, _, kind)) = as_stream(&args[0]) else {
                return Some(Err(Fault::java("NullPointerException", String::new())));
            };
            if as_stream(&args[1]).is_none() {
                return Some(Err(Fault::java("NullPointerException", String::new())));
            }
            Ok(Value::Obj(heap_alloc(HostObj::Stream {
                source: Source::Concat(Box::new((args[0].clone(), args[1].clone()))),
                stages: Vec::new(),
                kind,
            })))
        }
        ("Stream" | "IntStream" | "LongStream" | "DoubleStream", "iterate" | "generate")
            if args.len() == if method == "iterate" { 2 } else { 1 } =>
        {
            let source = if method == "iterate" {
                Source::Iterate {
                    seed: args[0].clone(),
                    f: args[1].clone(),
                }
            } else {
                Source::Generate(args[0].clone())
            };
            let kind = if class == "Stream" {
                StreamKind::Ref
            } else {
                primitive_stream_kind(class)
            };
            // A `DoubleStream` holds doubles whatever its seed or supplier
            // spelled, the same widening `stream_of` gives a finite source.
            let stages = if kind == StreamKind::Double {
                vec![Stage::Widen]
            } else {
                Vec::new()
            };
            Ok(Value::Obj(heap_alloc(HostObj::Stream {
                source,
                stages,
                kind,
            })))
        }
        ("IntStream" | "LongStream" | "DoubleStream", "of") => {
            Ok(stream_of(varargs_items(args), primitive_stream_kind(class)))
        }
        // `range` is half-open and `rangeClosed` is not, which is the whole
        // difference between them.
        ("IntStream" | "LongStream", "range" | "rangeClosed") if args.len() == 2 => {
            let (lo, hi) = (args[0].jint(), args[1].jint());
            let hi = if method == "rangeClosed" { hi + 1 } else { hi };
            Ok(stream_of(
                (lo..hi).map(Value::Int).collect(),
                primitive_stream_kind(class),
            ))
        }
        // `Arrays.stream(a)` answers an `IntStream` for an `int[]` and a
        // `Stream<T>` for a reference array. The element *type* is erased at
        // run time, so the shape is read off the elements — which is exact for
        // every array a program can build, an `int[]` holding `Value::Int` and
        // a `double[]` holding `Value::Float`.
        //
        // `Arrays.stream(a, from, to)` streams a slice, bounds-checked first by
        // `Spliterators.checkFromToBounds` with its own messages.
        ("Arrays", "stream") if args.len() == 1 || args.len() == 3 => {
            let mut items = array_items(&args[0])
                .ok_or_else(|| Fault::java("NullPointerException", String::new()));
            if let (Ok(all), [_, from, to]) = (&mut items, args) {
                let (from, to) = (from.jint(), to.jint());
                let out_of_range = |i: i64| {
                    Fault::java(
                        "ArrayIndexOutOfBoundsException",
                        format!("Array index out of range: {i}"),
                    )
                };
                if from > to {
                    return Some(Err(Fault::java(
                        "ArrayIndexOutOfBoundsException",
                        format!("origin({from}) > fence({to})"),
                    )));
                }
                if from < 0 {
                    return Some(Err(out_of_range(from)));
                }
                if to > all.len() as i64 {
                    return Some(Err(out_of_range(to)));
                }
                *all = all[from as usize..to as usize].to_vec();
            }
            let items = match items {
                Ok(items) => items,
                Err(f) => return Some(Err(f)),
            };
            let kind = if items.iter().any(|v| matches!(v, Value::Float(_))) {
                StreamKind::Double
            } else if !items.is_empty() && items.iter().all(|v| matches!(v, Value::Int(_))) {
                StreamKind::Int
            } else {
                StreamKind::Ref
            };
            Ok(stream_of(items, kind))
        }
        // ── java.util.stream.Collectors ──
        ("Collectors", "toList" | "toUnmodifiableList") if args.is_empty() => {
            Ok(collector("toList", Vec::new()))
        }
        ("Collectors", "toSet" | "toUnmodifiableSet") if args.is_empty() => {
            Ok(collector("toSet", Vec::new()))
        }
        ("Collectors", "counting") if args.is_empty() => Ok(collector("counting", Vec::new())),
        ("Collectors", "joining") if args.len() <= 3 => Ok(collector("joining", args.to_vec())),
        ("Collectors", "toMap") if (2..=4).contains(&args.len()) => {
            Ok(collector("toMap", args.to_vec()))
        }
        ("Collectors", "groupingBy") if (1..=3).contains(&args.len()) => {
            Ok(collector("groupingBy", args.to_vec()))
        }
        ("Collectors", "partitioningBy") if (1..=2).contains(&args.len()) => {
            Ok(collector("partitioningBy", args.to_vec()))
        }
        ("Collectors", "reducing") if (1..=3).contains(&args.len()) => {
            Ok(collector("reducing", args.to_vec()))
        }
        ("Collectors", "mapping" | "filtering" | "flatMapping" | "collectingAndThen")
            if args.len() == 2 =>
        {
            Ok(collector(static_kind(method), args.to_vec()))
        }
        ("Collectors", "teeing") if args.len() == 3 => Ok(collector("teeing", args.to_vec())),
        (
            "Collectors",
            "summingInt" | "summingLong" | "summingDouble" | "averagingInt" | "averagingLong"
            | "averagingDouble" | "summarizingInt" | "summarizingLong" | "summarizingDouble"
            | "minBy" | "maxBy" | "toCollection",
        ) if args.len() == 1 => Ok(collector(static_kind(method), args.to_vec())),
        // `java.util.Optional`'s factories. `of` rejects `null` — that is the
        // whole distinction from `ofNullable`, and accepting it would make an
        // `Optional` that claims to be present and is not.
        ("Optional", "of") if args.len() == 1 => {
            if matches!(args[0], Value::Undef) {
                return Some(Err(Fault::java("NullPointerException", String::new())));
            }
            Ok(optional(Some(args[0].clone())))
        }
        ("Optional", "ofNullable") if args.len() == 1 => Ok(optional(
            (!matches!(args[0], Value::Undef)).then(|| args[0].clone()),
        )),
        ("Optional", "empty") if args.is_empty() => Ok(optional(None)),
        // `Map.of(k1, v1, k2, v2, …)` — an immutable map, rejecting a repeated
        // key rather than letting the later pair win, and rejecting a `null`
        // key outright. Both are what `java.util.ImmutableCollections` does, and
        // both turn a program Java refuses to run into one that answers.
        ("Map", "of") if args.len() % 2 == 0 => {
            let mut entries: Vec<(Value, Value)> = Vec::with_capacity(args.len() / 2);
            for pair in args.chunks(2) {
                let (k, v) = (pair[0].clone(), pair[1].clone());
                if matches!(k, Value::Undef) || matches!(v, Value::Undef) {
                    return Some(Err(Fault::java("NullPointerException", String::new())));
                }
                if entries.iter().any(|(prev, _)| value_eq(prev, &k)) {
                    return Some(Err(Fault::java(
                        "IllegalArgumentException",
                        format!("duplicate key: {}", java_str_vm(vm, &k)),
                    )));
                }
                entries.push((k, v));
            }
            Ok(Value::Obj(heap_alloc(HostObj::Map {
                entries,
                order: Order::HASH,
                fixed: Fixity::Immutable,
                index: KeyIndex::default(),
            })))
        }
        // `Map.entry(k, v)` — one immutable pair, belonging to no map. Both
        // halves are rejected when null, which is the whole difference from a
        // `HashMap` entry (a `HashMap` accepts a null key and a null value, and
        // `m.entrySet()` therefore hands out entries holding them).
        ("Map", "entry") if args.len() == 2 => {
            if matches!(args[0], Value::Undef) || matches!(args[1], Value::Undef) {
                return Some(Err(Fault::java("NullPointerException", String::new())));
            }
            Ok(alloc_entry(args[0].clone(), args[1].clone(), None))
        }
        // `Map.ofEntries(e1, e2, …)` — the same immutable map `Map.of` builds,
        // spelled as pairs. It applies `Map.of`'s rules (no null, no repeated
        // key) because it is the same factory underneath.
        ("Map", "ofEntries") => {
            let mut entries: Vec<(Value, Value)> = Vec::with_capacity(args.len());
            for arg in varargs_items(args) {
                let Some(pair) = entry_pair(&arg) else {
                    return Some(Err(Fault::java("NullPointerException", String::new())));
                };
                if entries.iter().any(|(prev, _)| value_eq(prev, &pair.key)) {
                    return Some(Err(Fault::java(
                        "IllegalArgumentException",
                        format!("duplicate key: {}", java_str_vm(vm, &pair.key)),
                    )));
                }
                entries.push((pair.key, pair.value));
            }
            Ok(Value::Obj(heap_alloc(HostObj::Map {
                entries,
                order: Order::HASH,
                fixed: Fixity::Immutable,
                index: KeyIndex::default(),
            })))
        }
        // `Objects.equals(a, b)` — `a == b || (a != null && a.equals(b))`. It is
        // here rather than in the VM-less `static_method` because the `a.equals`
        // half runs a user body, which needs the VM.
        // `Objects.compare(a, b, c)`: `a == b ? 0 : c.compare(a, b)`. The
        // identity test comes first, so two `null`s are equal without the
        // comparator ever seeing them.
        ("Objects", "compare") if args.len() == 3 => {
            let same = match (&args[0], &args[1]) {
                (Value::Undef, Value::Undef) => true,
                (Value::Obj(a), Value::Obj(b)) => a == b,
                _ => false,
            };
            if same {
                return Some(Ok(Value::Int(0)));
            }
            Ok(invoke_closure(vm, &args[2], &args[..2]))
        }
        // `Arrays.setAll(a, f)`: `a[i] = f.apply(i)` for every index, in order.
        ("Arrays", "setAll") if args.len() == 2 => {
            let Some(len) = array_items(&args[0]).map(|a| a.len()) else {
                return Some(Err(Fault::java("NullPointerException", String::new())));
            };
            for i in 0..len {
                let v = invoke_closure(vm, &args[1], &[Value::Int(i as i64)]);
                if pending() {
                    return Some(Ok(Value::Undef));
                }
                if let Err(f) = array_mutate(&args[0], |a| a[i] = v) {
                    return Some(Err(f));
                }
            }
            Ok(Value::Undef)
        }
        ("Objects", "equals") if args.len() == 2 => {
            Ok(Value::bool(objects_equals(vm, &args[0], &args[1])))
        }
        // The rest of `java.util.Objects`. Like `equals` they sit here rather
        // than in the VM-less `static_method` because each may reach a user
        // `hashCode`/`toString` body, which needs the VM.
        //
        // `Objects.hashCode(null)` is 0 and `Objects.toString(null)` is the
        // four-character string "null" — the whole point of the class is that
        // it answers for a null where the instance method throws.
        ("Objects", "hashCode") if args.len() == 1 => {
            Ok(Value::Int(objects_hash_of(vm, &args[0]).into()))
        }
        // `Objects.hash(a, b, …)` is `Arrays.hashCode` of the varargs array:
        // seeded at 1, folded `31 * h + e`. So `Objects.hash()` is 1 and
        // `Objects.hash((Object) null)` is 31, not 0.
        ("Objects", "hash") => {
            let items = varargs_items(args);
            let mut h = 1i32;
            for e in &items {
                h = h.wrapping_mul(31).wrapping_add(objects_hash_of(vm, e));
            }
            Ok(Value::Int(h.into()))
        }
        ("Objects", "toString") if matches!(args.len(), 1 | 2) => {
            Ok(match (&args[0], args.get(1)) {
                (Value::Undef, Some(d)) => d.clone(),
                (Value::Undef, None) => Value::str("null"),
                (v, _) => Value::str(java_str_vm(vm, v)),
            })
        }
        ("Objects", "isNull") if args.len() == 1 => {
            Ok(Value::bool(matches!(args[0], Value::Undef)))
        }
        ("Objects", "nonNull") if args.len() == 1 => {
            Ok(Value::bool(!matches!(args[0], Value::Undef)))
        }
        // `requireNonNull(obj)` throws a message-less NPE; the two-argument
        // form throws with the caller's text. `requireNonNullElse` names the
        // parameter it found null — `defaultObj` — because it is the default
        // that was required, the first argument being allowed to be null.
        // The `Supplier<String>` overload asks its supplier for the message
        // only when it throws (a `null` supplier is a `null` message).
        ("Objects", "requireNonNull") if matches!(args.len(), 1 | 2) => match &args[0] {
            Value::Undef => {
                let msg = match args.get(1) {
                    Some(s) if closure_meta(s).is_some() => {
                        let m = invoke_closure(vm, s, &[]);
                        if pending() {
                            return Some(Ok(Value::Undef));
                        }
                        m
                    }
                    Some(m) => m.clone(),
                    None => Value::Undef,
                };
                Err(Fault::java(
                    "NullPointerException",
                    match msg {
                        Value::Undef => String::new(),
                        m => java_str_vm(vm, &m),
                    },
                ))
            }
            v => Ok(v.clone()),
        },
        // `requireNonNullElseGet(obj, supplier)`: `obj` when it is not null,
        // else `requireNonNull(requireNonNull(supplier, "supplier").get(),
        // "supplier.get()")`.
        ("Objects", "requireNonNullElseGet") if args.len() == 2 => match &args[0] {
            Value::Undef => {
                if matches!(args[1], Value::Undef) {
                    return Some(Err(Fault::java("NullPointerException", "supplier")));
                }
                let got = invoke_closure(vm, &args[1], &[]);
                if pending() {
                    return Some(Ok(Value::Undef));
                }
                match got {
                    Value::Undef => Err(Fault::java("NullPointerException", "supplier.get()")),
                    v => Ok(v),
                }
            }
            v => Ok(v.clone()),
        },
        ("Objects", "requireNonNullElse") if args.len() == 2 => match (&args[0], &args[1]) {
            (Value::Undef, Value::Undef) => Err(Fault::java(
                "NullPointerException",
                "defaultObj".to_string(),
            )),
            (Value::Undef, d) => Ok(d.clone()),
            (v, _) => Ok(v.clone()),
        },
        // `Arrays.sort(a, cmp)`, `Arrays.sort(a, from, to)` and
        // `Arrays.sort(a, from, to, cmp)`. A comparator re-enters the VM, which is
        // why these live here and the one-argument form does not. The range is
        // checked the way `Arrays.rangeCheck` does, message for message, before
        // anything is read; a `null` comparator is natural order.
        ("Arrays", "sort") if matches!(args.len(), 2..=4) => {
            let Some(mut items) = array_items(&args[0]) else {
                return Some(Err(Fault::java("NullPointerException", String::new())));
            };
            let (from, to, cmp) = match args.len() {
                2 => (0, items.len() as i64, &args[1]),
                3 => (args[1].jint(), args[2].jint(), &Value::Undef),
                _ => (args[1].jint(), args[2].jint(), &args[3]),
            };
            if let Err(f) = arrays_range_check(items.len(), from, to) {
                return Some(Err(f));
            }
            let (from, to) = (from as usize, to as usize);
            let window = items[from..to].to_vec();
            sort_with(vm, window, cmp).map(|sorted| {
                items.splice(from..to, sorted);
                // The write-back cannot miss: the handle was an array a moment ago.
                let _ = array_mutate(&args[0], |a| *a = items);
                Value::Undef
            })
        }
        // `Collections.addAll(c, e…)`: `result |= c.add(e)` for each element,
        // through the collection's own `add`, so a set keeps its rules and an
        // immutable list refuses.
        ("Collections", "addAll") if !args.is_empty() => {
            let target = args[0].clone();
            if matches!(target, Value::Undef) {
                return Some(Err(Fault::java("NullPointerException", String::new())));
            }
            let mut changed = false;
            for e in varargs_items(&args[1..]) {
                let r = coll_method(vm, &target, "add", std::slice::from_ref(&e));
                if pending() {
                    return Some(Ok(Value::Undef));
                }
                changed |= matches!(r, Value::Bool(true));
            }
            Ok(Value::bool(changed))
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
            // `Collections.sort(l)` is `l.sort(null)`, which an immutable list
            // refuses unconditionally (see the `List.sort` arm).
            if collection_fixity(&args[0]) == Some(Fixity::Immutable) {
                return Some(Err(unsupported()));
            }
            let cmp = args.get(1).cloned().unwrap_or(Value::Undef);
            sort_with(vm, items, &cmp)
                .and_then(|sorted| write_list(&args[0], sorted))
                .map(|()| Value::Undef)
        }
        // `reverse` is a walk of `swap`s — `set` calls — so it is refused by an
        // immutable list once there is a pair to swap, and, being no structural
        // change, it leaves an outstanding `subList` view valid.
        ("Collections", "reverse") if args.len() == 1 => {
            let mut items = sequence_items(&args[0]).unwrap_or_default();
            if items.len() >= 2 && collection_fixity(&args[0]) == Some(Fixity::Immutable) {
                return Some(Err(unsupported()));
            }
            items.reverse();
            set_all(&args[0], items).map(|()| Value::Undef)
        }
        // `max`/`min` take an optional comparator, which may be a user closure
        // — so the pick goes through the same stable sort the stream terminals
        // use rather than through `Iterator::max_by`, which cannot call back
        // into the VM.
        ("Collections", "max") | ("Collections", "min") if matches!(args.len(), 1 | 2) => {
            let items = sequence_items(&args[0]).unwrap_or_default();
            if items.is_empty() {
                return Some(Err(Fault::java("NoSuchElementException", String::new())));
            }
            let sorted = sort_values(vm, items, args.get(1));
            Ok(if method == "max" {
                sorted.last().cloned().unwrap_or(Value::Undef)
            } else {
                sorted.first().cloned().unwrap_or(Value::Undef)
            })
        }
        // `shuffle(l [, rnd])`: the JDK's Fisher–Yates walk from the top,
        // `swap(l, i - 1, rnd.nextInt(i))` for `i` from the size down to 2, so
        // a seeded `Random` shuffles exactly as it does there. The one-argument
        // form draws from a generator of its own, as `Collections.r` is.
        ("Collections", "shuffle") if matches!(args.len(), 1 | 2) => {
            let Some(mut items) = sequence_items(&args[0]) else {
                return Some(Err(Fault::java("NullPointerException", String::new())));
            };
            if items.len() < 2 {
                return Some(Ok(Value::Undef));
            }
            if collection_fixity(&args[0]) == Some(Fixity::Immutable) {
                return Some(Err(Fault::java(
                    "UnsupportedOperationException",
                    String::new(),
                )));
            }
            let mut draw = |rnd: &mut crate::jrandom::Random| {
                for i in (2..=items.len()).rev() {
                    let j = rnd.next_int_bounded(i as i32).unwrap_or(0) as usize;
                    items.swap(i - 1, j);
                }
            };
            match args.get(1) {
                Some(r) => {
                    if with_random(r, draw).is_none() {
                        return Some(Err(Fault::java("NullPointerException", String::new())));
                    }
                }
                None => SHUFFLE_RANDOM.with(|s| {
                    draw(
                        s.borrow_mut()
                            .get_or_insert_with(crate::jrandom::Random::unseeded),
                    )
                }),
            }
            set_all(&args[0], items).map(|()| Value::Undef)
        }
        // `frequency(c, o)`: how many elements `o.equals`, or how many are
        // `null` when `o` is — the JDK's two loops.
        ("Collections", "frequency") if args.len() == 2 => {
            let Some(items) = sequence_items(&args[0]) else {
                return Some(Err(npe_invoke("java.util.Collection.iterator()", "c")));
            };
            let o = &args[1];
            let mut n = 0i64;
            for e in &items {
                let hit = match o {
                    Value::Undef => matches!(e, Value::Undef),
                    _ => eq_call(vm, o, e),
                };
                if pending() {
                    return Some(Ok(Value::Undef));
                }
                n += i64::from(hit);
            }
            Ok(Value::Int(n))
        }
        // `swap(l, i, j)` is `l.set(i, l.set(j, l.get(i)))`, through the list's
        // own methods, so the bounds message and the immutable refusal are the
        // ones `get`/`set` give, in the order the JDK reaches them.
        ("Collections", "swap") if args.len() == 3 => {
            let l = &args[0];
            if matches!(l, Value::Undef) {
                return Some(Err(npe_invoke("java.util.List.get(int)", "l")));
            }
            let (i, j) = (args[1].clone(), args[2].clone());
            let vi = coll_method(vm, l, "get", std::slice::from_ref(&i));
            if pending() {
                return Some(Ok(Value::Undef));
            }
            let vj = coll_method(vm, l, "set", &[j, vi]);
            if pending() {
                return Some(Ok(Value::Undef));
            }
            coll_method(vm, l, "set", &[i, vj]);
            Ok(Value::Undef)
        }
        // `binarySearch(l, key, cmp)`: `indexedBinarySearch`'s loop, the
        // comparator called as `cmp(midVal, key)`. The compiler supplies the
        // natural-order comparator when the program names none, so a user
        // `Comparable` is searched by its own `compareTo`.
        ("Collections", "binarySearch") if args.len() == 3 => {
            let Some(items) = sequence_items(&args[0]) else {
                return Some(Err(npe_invoke("java.util.List.size()", "list")));
            };
            let (mut low, mut high) = (0i64, items.len() as i64 - 1);
            while low <= high {
                let mid = (low + high) >> 1;
                let c = match &args[2] {
                    Value::Undef => natural_cmp(&items[mid as usize], &args[1]) as i64,
                    cmp => invoke_closure(vm, cmp, &[items[mid as usize].clone(), args[1].clone()])
                        .jint(),
                };
                if pending() {
                    return Some(Ok(Value::Undef));
                }
                match c.cmp(&0) {
                    std::cmp::Ordering::Less => low = mid + 1,
                    std::cmp::Ordering::Greater => high = mid - 1,
                    std::cmp::Ordering::Equal => return Some(Ok(Value::Int(mid))),
                }
            }
            Ok(Value::Int(-(low + 1)))
        }
        // `disjoint(c1, c2)`: the JDK iterates one collection and asks the
        // other's `contains`, choosing which by `Set`-ness and then by size, so
        // each side's own membership rules (and refusals) apply.
        ("Collections", "disjoint") if args.len() == 2 => {
            let is_set = |v: &Value| {
                matches!(v, Value::Obj(id) if HEAP.with(|h| matches!(
                    h.borrow().get(*id as usize),
                    Some(HostObj::Set { .. })
                )))
            };
            let (c1, c2) = (&args[0], &args[1]);
            // A `null` side fails where the JDK first touches it: `c1.size()`
            // or `c2.size()`, unless the other side is a `Set` and the null one
            // is the collection being iterated.
            for (v, other, name) in [(c1, c2, "c1"), (c2, c1, "c2")] {
                if matches!(v, Value::Undef) {
                    return Some(Err(if is_set(other) {
                        npe_invoke("java.util.Collection.iterator()", "iterate")
                    } else {
                        npe_invoke("java.util.Collection.size()", name)
                    }));
                }
            }
            let (Some(i1), Some(i2)) = (sequence_items(c1), sequence_items(c2)) else {
                return Some(Err(Fault::java("NullPointerException", String::new())));
            };
            let (iterate, contains) = if is_set(c1) {
                (i2, c1)
            } else if !is_set(c2) {
                if i1.is_empty() || i2.is_empty() {
                    return Some(Ok(Value::bool(true)));
                }
                if i1.len() > i2.len() {
                    (i2, c1)
                } else {
                    (i1, c2)
                }
            } else {
                (i1, c2)
            };
            for e in iterate {
                let hit = coll_method(vm, contains, "contains", &[e]);
                if pending() {
                    return Some(Ok(Value::Undef));
                }
                if matches!(hit, Value::Bool(true)) {
                    return Some(Ok(Value::bool(false)));
                }
            }
            Ok(Value::bool(true))
        }
        // `rotate(l, d)`: element `i` moves to `(i + d) mod size`. The JDK
        // writes with `set`, so an immutable list refuses only when something
        // actually moves, and a `subList` view stays valid.
        ("Collections", "rotate") if args.len() == 2 => {
            let Some(items) = sequence_items(&args[0]) else {
                return Some(Err(npe_invoke("java.util.List.size()", "list")));
            };
            let n = items.len() as i64;
            if n == 0 {
                return Some(Ok(Value::Undef));
            }
            // `distance` is an `int`; the remainder is taken at that width.
            let d = (args[1].jint() as i32 as i64).rem_euclid(n);
            if d == 0 {
                return Some(Ok(Value::Undef));
            }
            if collection_fixity(&args[0]) == Some(Fixity::Immutable) {
                return Some(Err(unsupported()));
            }
            let mut out = items.clone();
            for (i, v) in items.into_iter().enumerate() {
                out[((i as i64 + d) % n) as usize] = v;
            }
            set_all(&args[0], out).map(|()| Value::Undef)
        }
        // `fill(l, o)` sets every position, so only a non-empty immutable list
        // refuses it.
        ("Collections", "fill") if args.len() == 2 => {
            let Some(items) = sequence_items(&args[0]) else {
                return Some(Err(npe_invoke("java.util.List.size()", "list")));
            };
            if !items.is_empty() && collection_fixity(&args[0]) == Some(Fixity::Immutable) {
                return Some(Err(unsupported()));
            }
            set_all(&args[0], vec![args[1].clone(); items.len()]).map(|()| Value::Undef)
        }
        // `indexOfSubList`/`lastIndexOfSubList`: the JDK's brute-force scan,
        // comparing `target.get(i).equals(source.get(j))` (or both `null`).
        ("Collections", "indexOfSubList" | "lastIndexOfSubList") if args.len() == 2 => {
            let Some(source) = sequence_items(&args[0]) else {
                return Some(Err(npe_invoke("java.util.List.size()", "source")));
            };
            let Some(target) = sequence_items(&args[1]) else {
                return Some(Err(npe_invoke("java.util.List.size()", "target")));
            };
            let Some(max) = source.len().checked_sub(target.len()) else {
                return Some(Ok(Value::Int(-1)));
            };
            let candidates: Vec<usize> = if method == "indexOfSubList" {
                (0..=max).collect()
            } else {
                (0..=max).rev().collect()
            };
            for c in candidates {
                let mut all = true;
                for (i, t) in target.iter().enumerate() {
                    let s = &source[c + i];
                    let eq = match t {
                        Value::Undef => matches!(s, Value::Undef),
                        _ => eq_call(vm, t, s),
                    };
                    if pending() {
                        return Some(Ok(Value::Undef));
                    }
                    if !eq {
                        all = false;
                        break;
                    }
                }
                if all {
                    return Some(Ok(Value::Int(c as i64)));
                }
            }
            Ok(Value::Int(-1))
        }
        _ => return None,
    })
}

/// The first index at which `a` and `b` (already cut to a common length by the
/// caller where it matters) hold unequal elements, or `None`. Elements compare
/// as `Objects.equals`; with `deep`, a pair of arrays compares element-wise in
/// turn, as `Arrays.deepEquals` does. A raised `equals` stops the scan.
/// `a == b` on two array references: the same handle, or both `null`.
fn same_array(a: &Value, b: &Value) -> bool {
    ref_eq(a, b) || matches!((a, b), (Value::Undef, Value::Undef))
}

fn arrays_mismatch(vm: &mut VM, a: &[Value], b: &[Value], deep: bool) -> Option<usize> {
    if a.len() != b.len() {
        return Some(a.len().min(b.len()));
    }
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        let same = match (deep, array_items(x), array_items(y)) {
            (true, Some(xs), Some(ys)) => {
                ref_eq(x, y) || arrays_mismatch(vm, &xs, &ys, true).is_none()
            }
            _ => objects_equals(vm, x, y),
        };
        if pending() || !same {
            return Some(i);
        }
    }
    None
}

/// One element comparison of `Arrays.compare`, at the element type `elem`
/// (the static type with its `[]` removed; empty when unknown). `None` when a
/// user `compareTo` raised.
fn array_element_compare(vm: &mut VM, x: &Value, y: &Value, elem: &str) -> Option<i64> {
    let sign = |o: std::cmp::Ordering| cmp_to_int(o);
    let (dx, dy) = (deboxed(x), deboxed(y));
    let kind = match elem {
        "" | "Object" | "Comparable" => match box_class(x) {
            Some(c) => c,
            None => elem,
        },
        other => other,
    };
    Some(match kind {
        // `Byte.compare`, `Short.compare` and `Character.compare` are `x - y`.
        "byte" | "Byte" | "short" | "Short" | "char" | "Character" => dx.jint() - dy.jint(),
        "double" | "Double" | "float" | "Float" => double_compare(dx.jfloat(), dy.jfloat()),
        "boolean" | "Boolean" => {
            sign(matches!(dx, Value::Bool(true)).cmp(&matches!(dy, Value::Bool(true))))
        }
        _ => match (&dx, &dy) {
            (Value::Str(p), Value::Str(q)) => compare_strings(p, q, false),
            (Value::Int(p), Value::Int(q)) => sign(p.cmp(q)),
            (Value::Float(p), Value::Float(q)) => double_compare(*p, *q),
            _ => return rank_compare(vm, &Value::Undef, x, y),
        },
    })
}

/// The NPE a JDK method raises reading `.length` of its `null` array
/// parameter `var`.
fn array_length_npe(var: &str) -> Fault {
    Fault::java(
        "NullPointerException",
        format!("Cannot read the array length because \"{var}\" is null"),
    )
}

/// The helpful `NullPointerException` the JVM raises when a JDK method body
/// dereferences its `null` parameter `var` by calling `target` on it — the
/// message names the library's own parameter, as `-XX:+ShowCodeDetailsInExceptionMessages`
/// (on by default since JDK 15) does.
fn npe_invoke(target: &str, var: &str) -> Fault {
    Fault::java(
        "NullPointerException",
        format!("Cannot invoke \"{target}\" because \"{var}\" is null"),
    )
}

/// The `UnsupportedOperationException` an unmodifiable collection answers a
/// write with. Its message is `null`, so it prints as the bare class name.
fn unsupported() -> Fault {
    Fault::java("UnsupportedOperationException", String::new())
}

/// Replace a list's elements position by position, as a run of `set` calls
/// would: no structural modification, so an outstanding `subList` view of it
/// stays valid (`Collections.reverse`/`shuffle`/`rotate`/`fill` all write this
/// way in the JDK). The caller has already applied the list's write refusals.
fn set_all(target: &Value, items: Vec<Value>) -> Result<(), Fault> {
    let Value::Obj(id) = target else {
        return Ok(());
    };
    write_sequence(*id as usize, items, false)
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
/// Replace a `List`'s or `Set`'s elements wholesale.
///
/// [`write_sequence`] writes a `List` (and walks a `subList` chain to its root);
/// a `Set` keeps a position accelerator beside its elements, so replacing them
/// has to invalidate it or the next lookup answers from stale positions. This is
/// the one writer that covers both, which is what `removeIf`/`replaceAll` need:
/// they are the only mutators Java defines on `Collection` rather than on `List`.
fn write_collection(target: &Value, items: Vec<Value>, structural: bool) -> Result<(), Fault> {
    let Value::Obj(id) = target else {
        return Ok(());
    };
    let id = *id as usize;
    // Writing *through* a view is not the same as writing past it. The view
    // splices its window into the backing list and pushes the length change up
    // its own ancestor chain — Java's `SubList.updateSizeAndModCount` — so the
    // view it was called on stays usable, exactly as `v.add`/`v.remove`
    // already do. Routing a view's `removeIf` through the plain list writer
    // bumped the root's `modCount` without telling the view, and reading the
    // view back raised `ConcurrentModificationException` for a change it had
    // made itself.
    if is_sublist(id) {
        let (root, offset, len) = checked_window(id)?;
        let delta = items.len() as isize - len as isize;
        HEAP.with(|h| {
            if let Some(HostObj::List {
                items: dst, mods, ..
            }) = h.borrow_mut().get_mut(root)
            {
                dst.splice(offset..offset + len, items);
                if delta != 0 {
                    *mods += 1;
                }
            }
        });
        if delta != 0 {
            let new_mods = list_mods(root).unwrap_or_default();
            resize_ancestors(id, delta, new_mods);
        }
        return Ok(());
    }
    let is_set = HEAP.with(|h| matches!(h.borrow().get(id), Some(HostObj::Set { .. })));
    if !is_set {
        return write_sequence(id, items, structural);
    }
    HEAP.with(|h| {
        if let Some(HostObj::Set {
            items: dst, index, ..
        }) = h.borrow_mut().get_mut(id)
        {
            *dst = items;
            index.invalidate();
        }
    });
    Ok(())
}

/// The [`Fixity`] of a `List` (through any `subList` chain) or a `Set`.
///
/// [`sublist_root_fixity`] answers for lists only, and answering `None` for a
/// `Set` would read `Set.of(…)` as mutable — so `Set.of(1, 2).removeIf(p)`
/// would edit an immutable set instead of throwing.
fn collection_fixity(v: &Value) -> Option<Fixity> {
    if let Value::Obj(id) = v {
        if let Some(f) = HEAP.with(|h| match h.borrow().get(*id as usize) {
            Some(HostObj::Set { fixed, .. }) => Some(*fixed),
            _ => None,
        }) {
            return Some(f);
        }
    }
    sublist_root_fixity(v)
}

fn write_list(target: &Value, items: Vec<Value>) -> Result<(), Fault> {
    let Value::Obj(id) = target else {
        return Ok(());
    };
    // `Collections.sort` is `ArrayList.sort`, which bumps `modCount`, so an
    // outstanding `subList` view of the target goes stale exactly as it does
    // there. The `set`-based reorderings go through [`set_all`] instead.
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
        ("Math", "pow", 2) => Ok(Value::float(crate::fdlibm::pow(
            args[0].jfloat(),
            args[1].jfloat(),
        ))),
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
        // `Math.ceilDiv`/`ceilMod` (Java 18), the ceiling counterparts: the
        // remainder takes the sign opposite the divisor, so `ceilMod(7, 2)` is
        // -1.
        ("Math", "ceilDiv", 2) => ceil_div(args[0].jint(), args[1].jint()).map(Value::Int),
        ("Math", "ceilMod", 2) => {
            let (a, b) = (args[0].jint(), args[1].jint());
            ceil_div(a, b).map(|q| Value::Int(a.wrapping_sub(q.wrapping_mul(b))))
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
        // `multiplyHigh`: the upper 64 bits of the 128-bit product, signed or
        // (`unsignedMultiplyHigh`, Java 18) unsigned.
        ("Math", "multiplyHigh", 2) => Ok(Value::Int(
            ((i128::from(args[0].jint()) * i128::from(args[1].jint())) >> 64) as i64,
        )),
        ("Math", "unsignedMultiplyHigh", 2) => Ok(Value::Int(
            ((u128::from(args[0].jint() as u64) * u128::from(args[1].jint() as u64)) >> 64) as i64,
        )),
        ("Math", "toIntExact", 2) => {
            let v = args[0].jint();
            match i32::try_from(v) {
                Ok(n) => Ok(Value::Int(n.into())),
                Err(_) => Err(Fault::java("ArithmeticException", "integer overflow")),
            }
        }
        // The unary members of the family. Each is one of the binary ones with a
        // constant operand — Java specifies them that way and their overflow
        // messages agree — except `absExact`, whose message names the constant
        // it cannot represent.
        ("Math", "incrementExact", 2) => exact_arith(args[0].jint(), 1, args[1].jint(), Exact::Add),
        ("Math", "decrementExact", 2) => exact_arith(args[0].jint(), 1, args[1].jint(), Exact::Sub),
        ("Math", "negateExact", 2) => exact_arith(0, args[0].jint(), args[1].jint(), Exact::Sub),
        ("Math", "absExact", 2) => {
            let (v, width) = (args[0].jint(), args[1].jint());
            let min = if width == width::LONG {
                i64::MIN
            } else {
                i64::from(i32::MIN)
            };
            if v == min {
                let name = if width == width::LONG {
                    "Long.MIN_VALUE"
                } else {
                    "Integer.MIN_VALUE"
                };
                return Err(Fault::java(
                    "ArithmeticException",
                    format!("Overflow to represent absolute value of {name}"),
                ));
            }
            Ok(Value::Int(v.abs()))
        }
        // The three exact divisions. Each rounds differently and all three
        // overflow in exactly one place — `MIN_VALUE / -1`, whose quotient is
        // one past the width — while a zero divisor is `/ by zero` first.
        ("Math", "divideExact", 3) => exact_divide(
            args[0].jint(),
            args[1].jint(),
            args[2].jint(),
            Rounding::Truncate,
        ),
        ("Math", "floorDivExact", 3) => exact_divide(
            args[0].jint(),
            args[1].jint(),
            args[2].jint(),
            Rounding::Floor,
        ),
        ("Math", "ceilDivExact", 3) => exact_divide(
            args[0].jint(),
            args[1].jint(),
            args[2].jint(),
            Rounding::Ceil,
        ),
        ("Math", "clamp", 4) => math_clamp(&args[0], &args[1], &args[2], args[3].jint()),
        ("Math", "signum", 1) => Ok(Value::float(match args[0].jfloat() {
            f if f > 0.0 => 1.0,
            f if f < 0.0 => -1.0,
            f => f,
        })),
        // The transcendentals, answered by the port of the JDK's own fdlibm
        // (`crate::fdlibm`). Rust's libm is not used: it differs from fdlibm in
        // the last digit (a 180-value sweep against OpenJDK 26 diverged for
        // every one of these). `Math` delegates to `StrictMath` for all of them
        // except where HotSpot has an intrinsic, and on aarch64 every intrinsic
        // here agrees with `StrictMath` bit for bit (200,000 random arguments
        // each, openjdk 27). `sin` and `cos` are the exception — their aarch64
        // intrinsic disagrees with fdlibm on 3.9% of arguments — so they stay
        // unregistered: a clear error rather than a silently different last
        // digit.
        // `StrictMath.sin`/`cos` are fdlibm by specification, unlike the
        // `Math` pair above.
        ("StrictMath", "sin", 1) => Ok(Value::float(crate::fdlibm::sin(args[0].jfloat()))),
        ("StrictMath", "cos", 1) => Ok(Value::float(crate::fdlibm::cos(args[0].jfloat()))),
        ("Math", "tan", 1) => Ok(Value::float(crate::fdlibm::tan(args[0].jfloat()))),
        ("Math", "asin", 1) => Ok(Value::float(crate::fdlibm::asin(args[0].jfloat()))),
        ("Math", "acos", 1) => Ok(Value::float(crate::fdlibm::acos(args[0].jfloat()))),
        ("Math", "atan", 1) => Ok(Value::float(crate::fdlibm::atan(args[0].jfloat()))),
        ("Math", "exp", 1) => Ok(Value::float(crate::fdlibm::exp(args[0].jfloat()))),
        ("Math", "log", 1) => Ok(Value::float(crate::fdlibm::log(args[0].jfloat()))),
        ("Math", "log10", 1) => Ok(Value::float(crate::fdlibm::log10(args[0].jfloat()))),
        ("Math", "log1p", 1) => Ok(Value::float(crate::fdlibm::log1p(args[0].jfloat()))),
        ("Math", "expm1", 1) => Ok(Value::float(crate::fdlibm::expm1(args[0].jfloat()))),
        ("Math", "cbrt", 1) => Ok(Value::float(crate::fdlibm::cbrt(args[0].jfloat()))),
        ("Math", "sinh", 1) => Ok(Value::float(crate::fdlibm::sinh(args[0].jfloat()))),
        ("Math", "cosh", 1) => Ok(Value::float(crate::fdlibm::cosh(args[0].jfloat()))),
        ("Math", "tanh", 1) => Ok(Value::float(crate::fdlibm::tanh(args[0].jfloat()))),
        ("Math", "asinh", 1) => Ok(Value::float(crate::fdlibm::asinh(args[0].jfloat()))),
        ("Math", "acosh", 1) => Ok(Value::float(crate::fdlibm::acosh(args[0].jfloat()))),
        ("Math", "atanh", 1) => Ok(Value::float(crate::fdlibm::atanh(args[0].jfloat()))),
        ("Math", "atan2", 2) => Ok(Value::float(crate::fdlibm::atan2(
            args[0].jfloat(),
            args[1].jfloat(),
        ))),
        ("Math", "hypot", 2) => Ok(Value::float(crate::fdlibm::hypot(
            args[0].jfloat(),
            args[1].jfloat(),
        ))),
        ("Math", "IEEEremainder", 2) => Ok(Value::float(crate::fdlibm::ieee_remainder(
            args[0].jfloat(),
            args[1].jfloat(),
        ))),
        ("Math", "scalb", 2) => Ok(Value::float(crate::fdlibm::scalb(
            args[0].jfloat(),
            args[1].jint() as i32,
        ))),
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
        // `Integer.sum` is `int + int`, so it wraps at 32 bits;
        // `Long.sum` wraps at 64. Sharing one `i64` arm answered
        // `Integer.sum(Integer.MAX_VALUE, 1)` with 2147483648, a value no `int`
        // can hold, where the JDK gives -2147483648. Every other arithmetic
        // site in javars narrows per its static width; this one did not.
        ("Integer", "sum", 2) => Ok(Value::Int(
            (args[0].jint() as i32)
                .wrapping_add(args[1].jint() as i32)
                .into(),
        )),
        ("Long", "sum", 2) => Ok(Value::Int(args[0].jint().wrapping_add(args[1].jint()))),
        ("Integer" | "Long", "signum", 1) => Ok(Value::Int(args[0].jint().signum())),
        // The bit-twiddling statics. Each reads its argument as a two's
        // complement pattern at the declared width and answers a value of that
        // same width, so `Integer` narrows to `i32` and `Long` stays at 64:
        // `Integer.reverse(1)` is Integer.MIN_VALUE where `Long.reverse(1L)` is
        // Long.MIN_VALUE. Every one is exactly specified — a shift or a
        // population count, never a transcendental — so there is one right
        // answer and Rust's integer intrinsics give it.
        //
        // The rotate distance is taken modulo the width *by the JDK's own
        // spec* ("bits shifted out of the left hand … re-entering on the
        // right"), and a negative distance rotates the other way;
        // `u32::rotate_left` masks the same way, so `Integer.rotateLeft(1, -1)`
        // is Integer.MIN_VALUE on both sides.
        ("Integer", "bitCount", 1) => Ok(Value::Int((args[0].jint() as i32).count_ones().into())),
        ("Long", "bitCount", 1) => Ok(Value::Int(args[0].jint().count_ones().into())),
        ("Integer", "numberOfLeadingZeros", 1) => {
            Ok(Value::Int((args[0].jint() as i32).leading_zeros().into()))
        }
        ("Long", "numberOfLeadingZeros", 1) => {
            Ok(Value::Int(args[0].jint().leading_zeros().into()))
        }
        ("Integer", "numberOfTrailingZeros", 1) => {
            Ok(Value::Int((args[0].jint() as i32).trailing_zeros().into()))
        }
        ("Long", "numberOfTrailingZeros", 1) => {
            Ok(Value::Int(args[0].jint().trailing_zeros().into()))
        }
        // `highestOneBit(0)` and `lowestOneBit(0)` are 0 — the JDK returns the
        // isolated bit, and zero has none. `x & -x` isolates the lowest set
        // bit and gives 0 for zero without a branch.
        ("Integer", "highestOneBit", 1) => Ok(Value::Int(match args[0].jint() as i32 {
            0 => 0,
            n => (1u32 << (31 - n.leading_zeros())) as i32 as i64,
        })),
        ("Long", "highestOneBit", 1) => Ok(Value::Int(match args[0].jint() {
            0 => 0,
            n => (1u64 << (63 - n.leading_zeros())) as i64,
        })),
        ("Integer", "lowestOneBit", 1) => {
            let n = args[0].jint() as i32;
            Ok(Value::Int(i64::from(n & n.wrapping_neg())))
        }
        ("Long", "lowestOneBit", 1) => {
            let n = args[0].jint();
            Ok(Value::Int(n & n.wrapping_neg()))
        }
        ("Integer", "reverse", 1) => Ok(Value::Int(i64::from(
            (args[0].jint() as i32).reverse_bits(),
        ))),
        ("Long", "reverse", 1) => Ok(Value::Int(args[0].jint().reverse_bits())),
        ("Integer", "reverseBytes", 1) => {
            Ok(Value::Int(i64::from((args[0].jint() as i32).swap_bytes())))
        }
        ("Long", "reverseBytes", 1) => Ok(Value::Int(args[0].jint().swap_bytes())),
        // The 16-bit pair: `Short.reverseBytes` answers a (signed) `short`,
        // `Character.reverseBytes` an (unsigned) `char`.
        ("Short", "reverseBytes", 1) => {
            Ok(Value::Int(i64::from((args[0].jint() as i16).swap_bytes())))
        }
        ("Character", "reverseBytes", 1) => Ok(Value::Int(i64::from(
            (char_arg(&args[0]) as u32 as u16).swap_bytes(),
        ))),
        ("Integer", "rotateLeft", 2) => Ok(Value::Int(i64::from(
            (args[0].jint() as i32).rotate_left(args[1].jint() as u32),
        ))),
        ("Integer", "rotateRight", 2) => Ok(Value::Int(i64::from(
            (args[0].jint() as i32).rotate_right(args[1].jint() as u32),
        ))),
        ("Long", "rotateLeft", 2) => Ok(Value::Int(
            args[0].jint().rotate_left(args[1].jint() as u32),
        )),
        ("Long", "rotateRight", 2) => Ok(Value::Int(
            args[0].jint().rotate_right(args[1].jint() as u32),
        )),
        // `Xxx.hashCode(x)` is the static spelling of the boxed instance
        // method, and each box folds a different width: `Integer` is the value,
        // `Long` folds its halves, `Double` folds `doubleToLongBits`, and
        // `Float` is `floatToIntBits` *unfolded* — so `Float.hashCode(1.5f)` is
        // 1069547520 where `Double.hashCode(1.5)` is 1073217536.
        ("Integer" | "Double" | "Boolean" | "Character", "hashCode", 1) => {
            let v = match class {
                "Double" => Value::float(args[0].jfloat()),
                "Boolean" => Value::bool(matches!(args[0], Value::Bool(true))),
                "Character" => Value::Int(i64::from(char_arg(&args[0]) as u32)),
                _ => Value::Int(args[0].jint()),
            };
            Ok(Value::Int(java_hash(&v).unwrap_or(0).into()))
        }
        ("Long", "hashCode", 1) => Ok(Value::Int(long_hash(args[0].jint()).into())),
        ("Float", "hashCode", 1) => Ok(Value::Int(float_hash(args[0].jfloat()).into())),
        ("Long", "toString", 1) => Ok(Value::str(args[0].jint().to_string())),
        ("Long", "toString", 2) => Ok(Value::str(int_to_radix_string(
            args[0].jint(),
            args[1].jint(),
        ))),
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
            parse_java_double(&s, false)
                .map(Value::float)
                .ok_or_else(|| Fault::java("NumberFormatException", float_format_message(&s)))
        }
        ("Double", "toString", 1) => Ok(Value::str(format_double(args[0].jfloat()))),
        ("Double", "toHexString", 1) => Ok(Value::str(double_hex_string(args[0].jfloat()))),
        // `Float.toHexString` is `Double.toHexString` of the widened value,
        // except a subnormal `float`, which is first scaled into the `double`
        // subnormal range so its digits keep the `float`'s exponent: the JDK's
        // `scalb(f, -896)` with the `p-1022` rewritten to `p-126`.
        ("Float", "toHexString", 1) => {
            let f = args[0].jfloat() as f32;
            Ok(Value::str(if f != 0.0 && f.abs() < f32::MIN_POSITIVE {
                let s = double_hex_string(f64::from(f) * 2f64.powi(-896));
                s.replace("p-1022", "p-126")
            } else {
                double_hex_string(f64::from(f))
            }))
        }

        // ── java.lang.Float ──
        // Every one of these answers at 32-bit precision, which is the whole
        // reason they are not aliases of the `Double` arm above.
        ("Float", "toString", 1) => Ok(Value::str(format_float(args[0].jfloat() as f32))),
        ("Float", "parseFloat", 1) | ("Float", "valueOf", 1) => {
            let s = args[0].as_str_cow();
            parse_java_double(&s, true)
                .map(Value::float)
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
        // The bit-pattern conversions. The two non-raw ones collapse every NaN
        // to the canonical `0x7ff8000000000000L` / `0x7fc00000`, as the JDK
        // specifies; the raw ones keep the payload.
        ("Double", "doubleToLongBits", 1) => Ok(Value::Int(match args[0].jfloat() {
            d if d.is_nan() => 0x7ff8_0000_0000_0000,
            d => d.to_bits() as i64,
        })),
        ("Double", "doubleToRawLongBits", 1) => Ok(Value::Int(args[0].jfloat().to_bits() as i64)),
        ("Double", "longBitsToDouble", 1) => {
            Ok(Value::float(f64::from_bits(args[0].jint() as u64)))
        }
        ("Float", "floatToIntBits", 1) => Ok(Value::Int(match args[0].jfloat() as f32 {
            f if f.is_nan() => 0x7fc0_0000,
            f => i64::from(f.to_bits() as i32),
        })),
        ("Float", "floatToRawIntBits", 1) => Ok(Value::Int(i64::from(
            (args[0].jfloat() as f32).to_bits() as i32,
        ))),
        ("Float", "intBitsToFloat", 1) => Ok(Value::float(f64::from(f32::from_bits(
            args[0].jint() as u32,
        )))),
        ("Double", "isNaN", 1) => Ok(Value::bool(args[0].jfloat().is_nan())),
        ("Double", "isInfinite", 1) => Ok(Value::bool(args[0].jfloat().is_infinite())),
        ("Double" | "Float", "isFinite", 1) => Ok(Value::bool(args[0].jfloat().is_finite())),
        // `Double.max`/`min` are `Math.max`/`min` — NaN wins and -0.0 is below
        // 0.0 — and `sum` is `+`. The `Float` trio answers at 32 bits.
        ("Double", "max", 2) => Ok(Value::float(max_double(args[0].jfloat(), args[1].jfloat()))),
        ("Double", "min", 2) => Ok(Value::float(min_double(args[0].jfloat(), args[1].jfloat()))),
        ("Double", "sum", 2) => Ok(Value::float(args[0].jfloat() + args[1].jfloat())),
        ("Float", "max", 2) => Ok(Value::float(max_double(args[0].jfloat(), args[1].jfloat()))),
        ("Float", "min", 2) => Ok(Value::float(min_double(args[0].jfloat(), args[1].jfloat()))),
        ("Float", "sum", 2) => Ok(Value::float(f64::from(
            args[0].jfloat() as f32 + args[1].jfloat() as f32,
        ))),

        // ── java.lang.Integer / Long: radix parsing and the unsigned family ──
        ("Integer", "valueOf", 2) if matches!(args[0], Value::Undef) => Err(null_number_fault()),
        ("Integer", "valueOf", 2) => parse_int_radix(&args[0].as_str_cow(), args[1].jint(), true),
        ("Long", "parseLong" | "valueOf", 2) if matches!(args[0], Value::Undef) => {
            Err(null_number_fault())
        }
        ("Long", "parseLong" | "valueOf", 2) => {
            parse_int_radix(&args[0].as_str_cow(), args[1].jint(), false)
        }
        ("Integer", "decode", 1) if matches!(args[0], Value::Undef) => Err(null_number_fault()),
        ("Integer", "decode", 1) => java_decode(&args[0].as_str_cow()),
        ("Integer", "parseUnsignedInt", 1 | 2) if matches!(args[0], Value::Undef) => {
            Err(null_number_fault())
        }
        ("Integer", "parseUnsignedInt", n @ (1 | 2)) => {
            let radix = if n == 2 { args[1].jint() } else { 10 };
            parse_unsigned_int(&args[0].as_str_cow(), radix)
        }
        ("Long", "parseUnsignedLong", 1 | 2) if matches!(args[0], Value::Undef) => {
            Err(null_number_fault())
        }
        ("Long", "parseUnsignedLong", n @ (1 | 2)) => {
            let radix = if n == 2 { args[1].jint() } else { 10 };
            parse_unsigned_long(&args[0].as_str_cow(), radix)
        }
        ("Integer", "toUnsignedLong", 1) => Ok(Value::Int(args[0].jint() as u32 as i64)),
        ("Integer", "toUnsignedString", 1) => Ok(Value::str((args[0].jint() as u32).to_string())),
        ("Integer", "toUnsignedString", 2) => Ok(Value::str(int_to_radix_string(
            args[0].jint() as u32 as i64,
            args[1].jint(),
        ))),
        ("Long", "toUnsignedString", 1) => Ok(Value::str((args[0].jint() as u64).to_string())),
        ("Integer", "compareUnsigned", 2) => Ok(Value::Int(cmp_to_int(
            (args[0].jint() as u32).cmp(&(args[1].jint() as u32)),
        ))),
        ("Long", "compareUnsigned", 2) => Ok(Value::Int(cmp_to_int(
            (args[0].jint() as u64).cmp(&(args[1].jint() as u64)),
        ))),
        ("Integer", "divideUnsigned" | "remainderUnsigned", 2) => {
            let (a, b) = (args[0].jint() as u32, args[1].jint() as u32);
            if b == 0 {
                return Err(Fault::java("ArithmeticException", "/ by zero"));
            }
            let r = if method == "divideUnsigned" {
                a / b
            } else {
                a % b
            };
            Ok(Value::Int(i64::from(r as i32)))
        }
        ("Long", "divideUnsigned" | "remainderUnsigned", 2) => {
            let (a, b) = (args[0].jint() as u64, args[1].jint() as u64);
            if b == 0 {
                return Err(Fault::java("ArithmeticException", "/ by zero"));
            }
            let r = if method == "divideUnsigned" {
                a / b
            } else {
                a % b
            };
            Ok(Value::Int(r as i64))
        }

        // ── java.lang.Short / Byte ──
        // Parsed at `int` width, then range-checked with the JDK's own message.
        ("Short", "parseShort", 1 | 2) | ("Byte", "parseByte", 1 | 2) => parse_narrow(class, args),
        ("Short" | "Byte", "valueOf", 1 | 2) if matches!(args[0], Value::Str(_) | Value::Undef) => {
            parse_narrow(class, args)
        }
        ("Short" | "Byte", "valueOf", 1) => Ok(Value::Int(args[0].jint())),
        // `Short.compare`/`Byte.compare` are `x - y`, not a sign.
        ("Short" | "Byte", "compare", 2) => Ok(Value::Int(args[0].jint() - args[1].jint())),
        ("Short" | "Byte", "toString", 1) => Ok(Value::str(args[0].jint().to_string())),
        ("Short" | "Byte", "hashCode", 1) => Ok(Value::Int(args[0].jint())),
        ("Short", "toUnsignedInt", 1) => Ok(Value::Int(args[0].jint() & 0xFFFF)),
        ("Byte", "toUnsignedInt", 1) => Ok(Value::Int(args[0].jint() & 0xFF)),

        // ── java.lang.Boolean's logical statics ──
        ("Boolean", "logicalAnd", 2) => Ok(Value::bool(args[0].is_truthy() & args[1].is_truthy())),
        ("Boolean", "logicalOr", 2) => Ok(Value::bool(args[0].is_truthy() | args[1].is_truthy())),
        ("Boolean", "logicalXor", 2) => Ok(Value::bool(args[0].is_truthy() ^ args[1].is_truthy())),

        // ── java.lang.Character ──
        // The argument is a `char` code point (`char_arg` also accepts the
        // one-character String a boxed `Character` is). `toUpperCase`/
        // `toLowerCase` return a `char`, so they return a code point too.
        // `isDigit` is Unicode's DECIMAL_DIGIT_NUMBER, not ASCII: Java answers
        // `true` for `'٣'` (U+0663) and every other script's decimal digits.
        ("Character", "isDigit", 1) => Ok(Value::bool(
            decimal_digit_value(char_arg(&args[0]) as u32).is_some(),
        )),
        ("Character", "digit", 2) => Ok(Value::Int(java_digit(
            char_arg(&args[0]) as u32,
            args[1].jint(),
        ))),
        // `forDigit` is `digit`'s inverse over the ASCII lowercase alphabet, and
        // the NUL `char` for a digit or radix out of range.
        ("Character", "forDigit", 2) => {
            let (d, radix) = (args[0].jint(), args[1].jint());
            Ok(Value::Int(
                if (2..=36).contains(&radix) && (0..radix).contains(&d) {
                    std::char::from_digit(d as u32, radix as u32).map_or(0, |c| c as i64)
                } else {
                    0
                },
            ))
        }
        // Unicode's `Alphabetic` property — letters, letter numbers, and the
        // `Other_Alphabetic` marks — which is exactly Rust's `is_alphabetic`.
        ("Character", "isAlphabetic", 1) => Ok(Value::bool(char_arg(&args[0]).is_alphabetic())),
        // SPACE_SEPARATOR, LINE_SEPARATOR, PARAGRAPH_SEPARATOR — unlike
        // `isWhitespace`, the no-break spaces count and the ASCII controls do not.
        ("Character", "isSpaceChar", 1) => Ok(Value::bool(matches!(
            char_arg(&args[0]),
            '\u{20}' | '\u{A0}' | '\u{1680}' | '\u{2000}'
                ..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{2028}' | '\u{2029}'
        ))),
        // Read as a code point, not through `char_arg`: a surrogate has no
        // `char`, and `char_arg` would answer it as NUL — a control.
        ("Character", "isISOControl", 1) => Ok(Value::bool(matches!(
            match deboxed(&args[0]) {
                Value::Int(n) => n,
                _ => char_arg(&args[0]) as i64,
            },
            0..=0x1F | 0x7F..=0x9F
        ))),
        // `Character.compare(x, y)` is `x - y`, not a sign.
        ("Character", "compare", 2) => Ok(Value::Int(
            char_arg(&args[0]) as i64 - char_arg(&args[1]) as i64,
        )),
        ("Character", "charCount", 1) => {
            Ok(Value::Int(if args[0].jint() >= 0x10000 { 2 } else { 1 }))
        }
        ("Character", "valueOf", 1) => Ok(Value::Int(char_arg(&args[0]) as i64)),
        ("Character", "isLetter", 1) => Ok(Value::bool(java_is_letter(char_arg(&args[0])))),
        // Read as a code point, not a `char`: a lone surrogate is a legal
        // argument (and not an identifier part), where a `char` conversion
        // would turn it into U+0000, which is one.
        ("Character", "isJavaIdentifierStart", 1) => {
            Ok(Value::bool(java_ident_start(code_point_arg(&args[0]))))
        }
        ("Character", "isJavaIdentifierPart", 1) => {
            Ok(Value::bool(java_ident_part(code_point_arg(&args[0]))))
        }
        // `Character.toChars(cp)`: one `char` for a BMP code point, the UTF-16
        // surrogate pair for a supplementary one.
        ("Character", "toChars", 1) => {
            let cp = args[0].jint();
            let Some(c) = u32::try_from(cp).ok().filter(|c| *c <= 0x10FFFF) else {
                return Err(Fault::java(
                    "IllegalArgumentException",
                    format!("Not a valid Unicode code point: 0x{:X}", cp as i32),
                ));
            };
            let units: Vec<Value> = if c < 0x10000 {
                vec![Value::Int(i64::from(c))]
            } else {
                let v = c - 0x10000;
                vec![
                    Value::Int(i64::from(0xD800 + (v >> 10))),
                    Value::Int(i64::from(0xDC00 + (v & 0x3FF))),
                ]
            };
            Ok(Value::Obj(heap_alloc(HostObj::Array(units))))
        }
        // A letter or a DECIMAL digit: `is_alphanumeric` also takes the
        // OTHER_NUMBER characters (`'²'`, `'½'`), which Java's does not.
        ("Character", "isLetterOrDigit", 1) => {
            let c = char_arg(&args[0]);
            Ok(Value::bool(
                java_is_letter(c) || decimal_digit_value(c as u32).is_some(),
            ))
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
        ("Character", "toTitleCase", 1) => Ok(Value::Int(title_case(char_arg(&args[0])))),
        ("Character", "toString", 1) => Ok(Value::str(char_arg(&args[0]).to_string())),
        // `getNumericValue` is `digit(c, 36)` for every script's decimal digits
        // and the Latin letters (fullwidth included); the characters whose
        // numeric value is not a digit (Roman numerals, fractions) are -1 here.
        ("Character", "getNumericValue", 1) => {
            Ok(Value::Int(java_digit(char_arg(&args[0]) as u32, 36)))
        }

        // ── java.lang.Boolean ──
        ("Boolean", "parseBoolean", 1) => Ok(Value::bool(
            args[0].as_str_cow().eq_ignore_ascii_case("true"),
        )),
        // `Boolean.valueOf(boolean)` is the cached box of its argument and
        // `valueOf(String)` is `parseBoolean` boxed — a `null` string is
        // `false`. javars does not box `Boolean`, so either is the primitive.
        ("Boolean", "valueOf", 1) => Ok(Value::bool(match &args[0] {
            Value::Bool(b) => *b,
            Value::Undef => false,
            other => other.as_str_cow().eq_ignore_ascii_case("true"),
        })),

        // ── java.lang.String ──
        // `String.valueOf(x)` renders any value with Java's `println` rules —
        // except a `char[]`, whose overload concatenates the characters rather
        // than printing the array.
        // `copyValueOf(char[])` is `valueOf(char[])` — the JDK's own
        // implementation is one call to the other — so it takes the same arm
        // rather than a second reading of the array.
        // See the `valueOf` note in `static_method_vm`. `copyValueOf` is
        // *not* here: it is documented to copy, and its argument is a `char[]`
        // rather than a `String` in every legal call.
        ("String", "valueOf", 1) if matches!(args[0], Value::Str(_)) => Ok(args[0].clone()),
        // `valueOf(char[], offset, count)`, its `copyValueOf` twin, and the
        // `new String(char[], offset, count)` constructor: the characters of
        // `[offset, offset + count)`, range-checked the way
        // `String.checkBoundsOffCount` words it.
        ("String", "valueOf" | "copyValueOf", 3) => {
            let items = array_items(&args[0])
                .ok_or_else(|| Fault::java("NullPointerException", String::new()))?;
            let (off, count) = (args[1].jint(), args[2].jint());
            if off < 0 || count < 0 || off + count > items.len() as i64 {
                return Err(Fault::java(
                    "StringIndexOutOfBoundsException",
                    format!(
                        "Range [{off}, {off} + {count}) out of bounds for length {}",
                        items.len()
                    ),
                ));
            }
            Ok(Value::str(
                items[off as usize..(off + count) as usize]
                    .iter()
                    .map(java_str)
                    .collect::<String>(),
            ))
        }
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
        // `Objects.checkIndex`/`checkFromToIndex`/`checkFromIndexSize`: answer
        // the index, or `Preconditions.outOfBounds`'s message for the form.
        ("Objects", "checkIndex", 2) => {
            let (i, n) = (args[0].jint(), args[1].jint());
            if i < 0 || i >= n {
                return Err(Fault::java(
                    "IndexOutOfBoundsException",
                    format!("Index {i} out of bounds for length {n}"),
                ));
            }
            Ok(Value::Int(i))
        }
        ("Objects", "checkFromToIndex", 3) => {
            let (from, to, n) = (args[0].jint(), args[1].jint(), args[2].jint());
            if from < 0 || from > to || to > n {
                return Err(Fault::java(
                    "IndexOutOfBoundsException",
                    format!("Range [{from}, {to}) out of bounds for length {n}"),
                ));
            }
            Ok(Value::Int(from))
        }
        ("Objects", "checkFromIndexSize", 3) => {
            let (from, size, n) = (args[0].jint(), args[1].jint(), args[2].jint());
            if (n | from | size) < 0 || size > n - from {
                return Err(Fault::java(
                    "IndexOutOfBoundsException",
                    format!("Range [{from}, {from} + {size}) out of bounds for length {n}"),
                ));
            }
            Ok(Value::Int(from))
        }
        ("Arrays", "fill", 2) => {
            let v = args[1].clone();
            array_mutate(&args[0], |a| a.fill(v))?;
            Ok(Value::Undef)
        }
        // `Arrays.fill(a, from, to, v)` checks the range as `Arrays.rangeCheck`
        // does before it writes anything.
        ("Arrays", "fill", 4) => {
            let len = array_items(&args[0])
                .map(|a| a.len())
                .ok_or_else(|| Fault::java("NullPointerException", String::new()))?;
            let (from, to) = (args[1].jint(), args[2].jint());
            arrays_range_check(len, from, to)?;
            let v = args[3].clone();
            array_mutate(&args[0], |a| a[from as usize..to as usize].fill(v))?;
            Ok(Value::Undef)
        }
        // `Arrays.copyOf` pads with the element type's default when it grows.
        // javars erases the element type at runtime, so the pad is inferred from
        // element 0's kind — the only evidence available — and is `null` for an
        // empty source.
        // Both copies clamp their arguments, and Java does not: a bad length or
        // a reversed range threw where javars silently answered an array.
        // `Arrays.copyOf` allocates before it copies, so a negative length is
        // the allocation's own `NegativeArraySizeException`.
        ("Arrays", "copyOf", 2 | 3) => {
            let items = array_items(&args[0]).unwrap_or_default();
            let len = args[1].jint();
            if len < 0 {
                return Err(Fault::java("NegativeArraySizeException", len.to_string()));
            }
            let pad = array_pad(&args[2..], &items);
            let mut out = items;
            out.resize(len as usize, pad);
            Ok(Value::Obj(heap_alloc(HostObj::Array(out))))
        }
        ("Arrays", "copyOfRange", 3 | 4) => {
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
            let pad = array_pad(&args[3..], &items);
            let mut out: Vec<Value> = items
                .get(from..to.min(items.len()))
                .unwrap_or_default()
                .to_vec();
            out.resize(to - from, pad);
            Ok(Value::Obj(heap_alloc(HostObj::Array(out))))
        }
        // `Arrays.binarySearch` returns `-(insertion point) - 1` when absent,
        // exactly as the JDK does — and when the key is present *more than
        // once*, the index of whichever copy the JDK's own probe sequence lands
        // on. That is not "any match": `Arrays.binarySearch(new int[]{3, 3}, 3)`
        // is 0 and `binarySearch(new int[]{3, 3, 3, 3}, 3)` is 1, both fixed by
        // the loop below. Rust's `slice::binary_search_by` makes no such promise
        // and answered 1 and 3, so the two agreed only on arrays with no
        // duplicate key. The loop is `java.util.Arrays.binarySearch0`
        // transcribed, midpoint included (`(low + high) >>> 1`).
        ("Arrays", "binarySearch", 2) => {
            let items = array_items(&args[0]).unwrap_or_default();
            let key = &args[1];
            let mut low: i64 = 0;
            let mut high: i64 = items.len() as i64 - 1;
            let mut found = None;
            while low <= high {
                let mid = ((low as u64 + high as u64) >> 1) as i64;
                match natural_cmp(&items[mid as usize], key) {
                    std::cmp::Ordering::Less => low = mid + 1,
                    std::cmp::Ordering::Greater => high = mid - 1,
                    std::cmp::Ordering::Equal => {
                        found = Some(mid);
                        break;
                    }
                }
            }
            Ok(Value::Int(found.unwrap_or(-(low + 1))))
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

// ── java.util.regex.Pattern / Matcher ───────────────────────────────────────

/// `Pattern.CASE_INSENSITIVE`.
const RE_CASE_INSENSITIVE: i64 = 0x02;
/// `Pattern.LITERAL`.
const RE_LITERAL: i64 = 0x10;
/// `Pattern.DOTALL`.
const RE_DOTALL: i64 = 0x20;

/// A `java.util.regex.Matcher`: the JDK's `first`/`last`/`groups` state over
/// one input, held as byte offsets and reported in UTF-16 indices.
#[derive(Clone)]
struct RegexMatcher {
    /// The pattern as `Pattern.pattern()` answers it.
    shown: String,
    flags: i64,
    /// The source handed to [`crate::regex`]: `shown` with the flags spelled
    /// inline (`(?i)`, `(?s)`) and quoted under `LITERAL`.
    source: String,
    text: String,
    /// The start of the current match; `None` when there is none, which is
    /// the JDK's `first == -1`.
    first: Option<usize>,
    /// The end of the last match, where the next `find()` starts.
    last: usize,
    groups: crate::regex::Groups,
    /// Which compilation the current match came from, so `appendReplacement`
    /// expands it against the same one.
    mode: MatchMode,
    /// `lastAppendPosition`.
    append_pos: usize,
}

#[derive(Clone, Copy)]
enum MatchMode {
    Search,
    Whole,
    Prefix,
}

/// `Pattern.quote(s)`: `\Q…\E`, with every `\E` in `s` closed and re-opened.
fn regex_quote(s: &str) -> String {
    if !s.contains("\\E") {
        return format!("\\Q{s}\\E");
    }
    let mut out = String::from("\\Q");
    let mut rest = s;
    while let Some(i) = rest.find("\\E") {
        out.push_str(&rest[..i]);
        out.push_str("\\E\\\\E\\Q");
        rest = &rest[i + 2..];
    }
    out.push_str(rest);
    out.push_str("\\E");
    out
}

/// The source [`crate::regex`] compiles for `Pattern.compile(source, flags)`.
/// `CASE_INSENSITIVE` and `DOTALL` are the inline `(?i)`/`(?s)` the translator
/// already models and `LITERAL` is [`regex_quote`]; any other flag is refused
/// by name rather than ignored.
fn regex_source(shown: &str, flags: i64) -> Result<String, Fault> {
    let other = flags & !(RE_CASE_INSENSITIVE | RE_LITERAL | RE_DOTALL);
    if other != 0 {
        return Err(Fault::internal(format!(
            "javars: Pattern flag 0x{other:x} is not modeled"
        )));
    }
    let mut src = String::new();
    if flags & RE_CASE_INSENSITIVE != 0 {
        src.push_str("(?i)");
    }
    if flags & RE_DOTALL != 0 {
        src.push_str("(?s)");
    }
    if flags & RE_LITERAL != 0 {
        src.push_str(&regex_quote(shown));
    } else {
        src.push_str(shown);
    }
    Ok(src)
}

/// The compiled form of `source` for one matching mode, or its
/// `PatternSyntaxException`.
fn regex_compiled(source: &str, mode: MatchMode) -> Result<crate::regex::Compiled, Fault> {
    let c = match mode {
        MatchMode::Search => crate::regex::compile(source),
        MatchMode::Whole => crate::regex::compile_whole(source),
        MatchMode::Prefix => crate::regex::compile_prefix(source),
    };
    if let Err(e) = c.as_ref() {
        return Err(pattern_fault(e));
    }
    Ok(c)
}

/// The UTF-16 index of byte offset `b` — what every `Matcher` index reports.
fn utf16_at(text: &str, b: usize) -> i64 {
    text[..b].encode_utf16().count() as i64
}

/// The byte offset of UTF-16 index `i`; an index inside a surrogate pair
/// resolves to the character's end.
fn byte_at_utf16(text: &str, i: usize) -> usize {
    let mut units = 0;
    for (b, c) in text.char_indices() {
        if units >= i {
            return b;
        }
        units += c.len_utf16();
    }
    text.len()
}

impl RegexMatcher {
    fn new(shown: String, flags: i64, source: String, text: String) -> Self {
        RegexMatcher {
            shown,
            flags,
            source,
            text,
            first: None,
            last: 0,
            groups: Vec::new(),
            mode: MatchMode::Search,
            append_pos: 0,
        }
    }

    /// `reset()`.
    fn reset(&mut self) {
        self.first = None;
        self.last = 0;
        self.groups.clear();
        self.append_pos = 0;
    }

    /// `search(from)` / `match(from, anchor)`: try one match starting at byte
    /// `from`, recording it or clearing `first`.
    fn attempt(&mut self, from: usize, mode: MatchMode) -> Result<bool, Fault> {
        let c = regex_compiled(&self.source, mode)?;
        let pat = c.as_ref().as_ref().expect("checked by regex_compiled");
        self.groups.clear();
        match pat.captures_at(&self.text, from).map_err(engine_fault)? {
            Some(g) => {
                let (s, e) = g[0].expect("group 0 always participates");
                self.first = Some(s);
                self.last = e;
                self.groups = g;
                self.mode = mode;
                Ok(true)
            }
            None => {
                self.first = None;
                Ok(false)
            }
        }
    }

    /// `find()`: from the end of the last match, one character further when
    /// that match was empty.
    fn find(&mut self) -> Result<bool, Fault> {
        let mut next = self.last;
        if Some(next) == self.first {
            next = match self.text[next..].chars().next() {
                Some(c) => next + c.len_utf8(),
                None => self.text.len() + 1,
            };
        }
        if next > self.text.len() {
            self.groups.clear();
            return Ok(false);
        }
        self.attempt(next, MatchMode::Search)
    }

    /// The current match, or the JDK's `IllegalStateException`.
    fn check_match(&self) -> Result<(), Fault> {
        match self.first {
            Some(_) => Ok(()),
            None => Err(Fault::java("IllegalStateException", "No match found")),
        }
    }

    /// The span of group `g`, after the JDK's two checks.
    fn group_span(&self, g: i64) -> Result<Option<(usize, usize)>, Fault> {
        self.check_match()?;
        let count = self.group_count()?;
        if g < 0 || g as usize > count {
            return Err(Fault::java(
                "IndexOutOfBoundsException",
                format!("No group {g}"),
            ));
        }
        Ok(self.groups.get(g as usize).copied().flatten())
    }

    fn group_count(&self) -> Result<usize, Fault> {
        let c = regex_compiled(&self.source, MatchMode::Search)?;
        Ok(c.as_ref().as_ref().map_or(0, |p| p.group_count()))
    }

    /// The number a `(?<name>…)` group has, or the JDK's refusal.
    fn named(&self, name: &str) -> Result<i64, Fault> {
        self.check_match()?;
        let c = regex_compiled(&self.source, MatchMode::Search)?;
        c.as_ref()
            .as_ref()
            .ok()
            .and_then(|p| p.group_index(name))
            .map(|i| i as i64)
            .ok_or_else(|| {
                Fault::java(
                    "IllegalArgumentException",
                    format!("No group with name <{name}>"),
                )
            })
    }

    /// The replacement text for the current match.
    fn expanded(&self, replacement: &str) -> Result<String, Fault> {
        let first = self.first.expect("checked by the caller");
        let c = regex_compiled(&self.source, self.mode)?;
        let pat = c.as_ref().as_ref().expect("checked by regex_compiled");
        // An anchored compilation can only find its match from the start.
        let from = match self.mode {
            MatchMode::Search => first,
            _ => 0,
        };
        pat.expand_groups(&self.text, from, replacement)
            .map_err(replacement_fault)
    }
}

/// A `java.util.regex` static: `Pattern.compile`, `Pattern.matches`,
/// `Pattern.quote` and `Matcher.quoteReplacement`.
fn regex_static(class: &str, method: &str, args: &[Value]) -> Option<Result<Value, Fault>> {
    let npe = || Err(Fault::java("NullPointerException", String::new()));
    Some(match (class, method, args) {
        ("Pattern", "compile", [s, rest @ ..]) if rest.len() <= 1 => {
            if matches!(s, Value::Undef) {
                return Some(npe());
            }
            let shown = s.as_str_cow().into_owned();
            let flags = rest.first().map_or(0, JavaNumeric::jint);
            regex_source(&shown, flags).and_then(|src| {
                regex_compiled(&src, MatchMode::Search)?;
                Ok(Value::Obj(heap_alloc(HostObj::RegexPattern {
                    shown,
                    flags,
                    source: src,
                })))
            })
        }
        ("Pattern", "matches", [re, input]) => {
            let c = crate::regex::compile_whole(&re.as_str_cow());
            match c.as_ref() {
                Ok(p) => p
                    .matches_whole(&input.as_str_cow())
                    .map(Value::bool)
                    .map_err(engine_fault),
                Err(e) => Err(pattern_fault(e)),
            }
        }
        ("Pattern", "quote", [s]) => Ok(Value::str(regex_quote(&s.as_str_cow()))),
        // `Matcher.quoteReplacement`: a `\` before every `\` and `$`.
        ("Matcher", "quoteReplacement", [s]) => {
            let s = s.as_str_cow();
            let mut out = String::with_capacity(s.len());
            for c in s.chars() {
                if c == '\\' || c == '$' {
                    out.push('\\');
                }
                out.push(c);
            }
            Ok(Value::str(out))
        }
        _ => return None,
    })
}

/// A method call on a `Pattern` receiver; `None` for any other.
fn pattern_method(recv: &Value, method: &str, args: &[Value]) -> Option<Result<Value, Fault>> {
    let Value::Obj(id) = recv else {
        return None;
    };
    let (shown, flags, source) = HEAP.with(|h| match h.borrow().get(*id as usize) {
        Some(HostObj::RegexPattern {
            shown,
            flags,
            source,
        }) => Some((shown.clone(), *flags, source.clone())),
        _ => None,
    })?;
    Some(match (method, args) {
        ("matcher", [t]) => {
            if matches!(t, Value::Undef) {
                return Some(Err(Fault::java("NullPointerException", String::new())));
            }
            Ok(Value::Obj(heap_alloc(HostObj::RegexMatcher(Box::new(
                RegexMatcher::new(shown, flags, source, t.as_str_cow().into_owned()),
            )))))
        }
        ("pattern" | "toString", []) => Ok(Value::str(shown)),
        // `splitAsStream(input)`: `split(input)`'s fields as a stream.
        ("splitAsStream", [t]) => regex_compiled(&source, MatchMode::Search).and_then(|c| {
            let pat = c.as_ref().as_ref().expect("checked by regex_compiled");
            let parts = pat.split(&t.as_str_cow(), 0).map_err(engine_fault)?;
            Ok(stream_of(
                parts.into_iter().map(Value::str).collect(),
                StreamKind::Ref,
            ))
        }),
        ("flags", []) => Ok(Value::Int(flags)),
        ("split", [t, rest @ ..]) if rest.len() <= 1 => regex_compiled(&source, MatchMode::Search)
            .and_then(|c| {
                let pat = c.as_ref().as_ref().expect("checked by regex_compiled");
                let limit = rest.first().map_or(0, JavaNumeric::jint);
                let parts = pat.split(&t.as_str_cow(), limit).map_err(engine_fault)?;
                Ok(Value::Obj(heap_alloc(HostObj::Array(
                    parts.into_iter().map(Value::str).collect(),
                ))))
            }),
        _ => Err(Fault::internal(format!(
            "javars: unsupported Pattern method `{method}` with {} argument(s)",
            args.len()
        ))),
    })
}

/// Run `f` on the `Matcher` a handle names; `None` for any other value.
fn with_matcher<R>(v: &Value, f: impl FnOnce(&mut RegexMatcher) -> R) -> Option<R> {
    let Value::Obj(id) = v else {
        return None;
    };
    HEAP.with(|h| match h.borrow_mut().get_mut(*id as usize) {
        Some(HostObj::RegexMatcher(m)) => Some(f(m)),
        _ => None,
    })
}

/// A method call on a `Matcher` receiver; `None` for any other.
///
/// `appendReplacement`/`appendTail` write into a `StringBuilder`, which runs
/// through [`builder_method`] once the matcher's borrow is released.
fn matcher_method(
    vm: &mut VM,
    recv: &Value,
    method: &str,
    args: &[Value],
) -> Option<Result<Value, Fault>> {
    // The text an `append*` call adds to its builder, worked out under the
    // matcher's borrow and appended after it.
    let appended = match (method, args) {
        ("appendReplacement", [_, r]) => Some(with_matcher(recv, |m| {
            m.check_match()?;
            let first = m.first.expect("checked");
            let piece = format!(
                "{}{}",
                &m.text[m.append_pos..first],
                m.expanded(&r.as_str_cow())?
            );
            m.append_pos = m.last;
            Ok(piece)
        })?),
        ("appendTail", [_]) => Some(with_matcher(recv, |m| {
            Ok(m.text[m.append_pos..].to_string())
        })?),
        _ => None,
    };
    // `toMatchResult()` and `results()` hand out snapshots of the match
    // state — a frozen matcher answers `group`/`start`/`end` exactly as the
    // JDK's `MatchResult` does. `results()` does not reset: it runs `find()`
    // from wherever the matcher stands, one snapshot per match.
    if (method, args.len()) == ("toMatchResult", 0) {
        let snap = with_matcher(recv, |m| m.clone())?;
        return Some(Ok(Value::Obj(heap_alloc(HostObj::RegexMatcher(Box::new(
            snap,
        ))))));
    }
    if (method, args.len()) == ("results", 0) {
        let mut snaps = Vec::new();
        loop {
            match with_matcher(recv, |m| m.find().map(|hit| hit.then(|| m.clone())))? {
                Ok(Some(s)) => snaps.push(s),
                Ok(None) => break,
                Err(f) => return Some(Err(f)),
            }
        }
        let items = snaps
            .into_iter()
            .map(|s| Value::Obj(heap_alloc(HostObj::RegexMatcher(Box::new(s)))))
            .collect();
        return Some(Ok(stream_of(items, StreamKind::Ref)));
    }
    // `pattern()` allocates a `Pattern`, which cannot happen under the
    // matcher's borrow.
    if (method, args.len()) == ("pattern", 0) {
        let (shown, flags, source) =
            with_matcher(recv, |m| (m.shown.clone(), m.flags, m.source.clone()))?;
        return Some(Ok(Value::Obj(heap_alloc(HostObj::RegexPattern {
            shown,
            flags,
            source,
        }))));
    }
    if let Some(piece) = appended {
        let sb = &args[0];
        return Some(piece.and_then(|piece| {
            let Some(id) = is_builder(sb) else {
                return Err(Fault::java("NullPointerException", String::new()));
            };
            match builder_method(vm, id, "append", &[Value::str(piece)]) {
                Some(Err(f)) => Err(f),
                _ => Ok(if method == "appendTail" {
                    sb.clone()
                } else {
                    recv.clone()
                }),
            }
        }));
    }
    with_matcher(recv, |m| {
        let at = |m: &RegexMatcher, b: usize| Value::Int(utf16_at(&m.text, b));
        match (method, args) {
            ("find", []) => m.find().map(Value::bool),
            ("find", [s]) => {
                let limit = m.text.encode_utf16().count() as i64;
                let s = s.jint();
                if s < 0 || s > limit {
                    return Err(Fault::java(
                        "IndexOutOfBoundsException",
                        "Illegal start index",
                    ));
                }
                m.reset();
                let from = byte_at_utf16(&m.text, s as usize);
                m.attempt(from, MatchMode::Search).map(Value::bool)
            }
            ("matches", []) => m.attempt(0, MatchMode::Whole).map(Value::bool),
            ("lookingAt", []) => m.attempt(0, MatchMode::Prefix).map(Value::bool),
            ("hasMatch", []) => Ok(Value::bool(m.first.is_some())),
            ("group", []) => m.group_span(0).map(|s| match s {
                Some((a, b)) => Value::str(m.text[a..b].to_string()),
                None => Value::Undef,
            }),
            ("group", [n]) if matches!(n, Value::Str(_)) => {
                let g = m.named(&n.as_str_cow())?;
                m.group_span(g).map(|s| match s {
                    Some((a, b)) => Value::str(m.text[a..b].to_string()),
                    None => Value::Undef,
                })
            }
            ("group", [g]) => m.group_span(g.jint()).map(|s| match s {
                Some((a, b)) => Value::str(m.text[a..b].to_string()),
                None => Value::Undef,
            }),
            ("start" | "end", []) => {
                m.check_match()?;
                let (s, e) = m.groups[0].expect("group 0 always participates");
                Ok(at(m, if method == "start" { s } else { e }))
            }
            ("start" | "end", [g]) => {
                let g = match g {
                    Value::Str(_) => m.named(&g.as_str_cow())?,
                    other => other.jint(),
                };
                m.group_span(g).map(|s| match s {
                    Some((a, b)) => at(m, if method == "start" { a } else { b }),
                    None => Value::Int(-1),
                })
            }
            ("groupCount", []) => m.group_count().map(|n| Value::Int(n as i64)),
            ("reset", []) => {
                m.reset();
                Ok(recv.clone())
            }
            ("reset", [t]) => {
                m.text = t.as_str_cow().into_owned();
                m.reset();
                Ok(recv.clone())
            }
            ("replaceAll" | "replaceFirst", [r]) => {
                m.reset();
                let c = regex_compiled(&m.source, MatchMode::Search)?;
                let pat = c.as_ref().as_ref().expect("checked by regex_compiled");
                let out = pat
                    .replace(&m.text, &r.as_str_cow(), method == "replaceFirst")
                    .map_err(replacement_fault)?;
                // The JDK's loop ends on a failed `find()`, and `replaceFirst`
                // leaves the one match it made current.
                if method == "replaceFirst" {
                    m.find()?;
                } else {
                    m.first = None;
                }
                Ok(Value::str(out))
            }
            ("regionStart", []) => Ok(Value::Int(0)),
            ("regionEnd", []) => Ok(Value::Int(m.text.encode_utf16().count() as i64)),
            _ => Err(Fault::internal(format!(
                "javars: unsupported Matcher method `{method}` with {} argument(s)",
                args.len()
            ))),
        }
    })
}

/// The `PatternSyntaxException` a pattern the translator or the engine
/// refused raises, carrying its message.
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

/// Which way [`exact_divide`] rounds an inexact quotient.
enum Rounding {
    /// Toward zero — `Math.divideExact`, the `/` operator's own rounding.
    Truncate,
    /// Toward negative infinity — `Math.floorDivExact`.
    Floor,
    /// Toward positive infinity — `Math.ceilDivExact`.
    Ceil,
}

/// `Math.divideExact` / `floorDivExact` / `ceilDivExact` at the width the
/// compiler resolved.
///
/// All three overflow in exactly one place, `MIN_VALUE / -1`, whose quotient is
/// one past the width in every rounding — and a zero divisor is `/ by zero`
/// before that, which is the order openjdk 26.0.2 reports them in.
fn exact_divide(a: i64, b: i64, width: i64, round: Rounding) -> Result<Value, Fault> {
    if b == 0 {
        return Err(Fault::java("ArithmeticException", "/ by zero"));
    }
    let overflow = || {
        Fault::java(
            "ArithmeticException",
            if width == width::LONG {
                "long overflow"
            } else {
                "integer overflow"
            },
        )
    };
    let q = match round {
        Rounding::Truncate => a.checked_div(b),
        Rounding::Floor => Some(floor_div(a, b)?),
        // Ceiling is the floor plus one whenever the division was inexact,
        // whatever the signs: `ceilDiv(7, 2)` is 4 (floor 3) and
        // `ceilDiv(-7, 2)` is -3 (floor -4). Reusing `floor_div` keeps the one
        // correction step in a single place.
        Rounding::Ceil => floor_div(a, b)
            .map(|f| {
                if a.wrapping_rem(b) != 0 {
                    f.wrapping_add(1)
                } else {
                    f
                }
            })
            .map(Some)?,
    };
    let q = q.ok_or_else(overflow)?;
    if width == width::LONG {
        // `checked_div` already refused `i64::MIN / -1`; the two rounding arms
        // reach it through `wrapping` arithmetic, so they are checked here.
        if a == i64::MIN && b == -1 {
            return Err(overflow());
        }
        return Ok(Value::Int(q));
    }
    match i32::try_from(q) {
        Ok(n) => Ok(Value::Int(n.into())),
        Err(_) => Err(overflow()),
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

/// `Math.ceilDiv` (Java 18): the quotient rounded toward positive infinity —
/// [`floor_div`]'s mirror, corrected upward when the signs *agree* and the
/// division was inexact. `ceilDiv(7, 2)` is 4 and `ceilDiv(-7, 2)` is -3.
fn ceil_div(a: i64, b: i64) -> Result<i64, Fault> {
    if b == 0 {
        return Err(Fault::java("ArithmeticException", "/ by zero"));
    }
    let q = a.wrapping_div(b);
    Ok(if a.wrapping_rem(b) != 0 && (a ^ b) >= 0 {
        q.wrapping_add(1)
    } else {
        q
    })
}

/// The code points Unicode calls `Alphabetic` that Java's `Character.isLetter`
/// does not: the LETTER_NUMBER characters and the `Other_Alphabetic` marks
/// (`U+0345`, the Hebrew and Arabic vowel points, Indic vowel signs, …). Rust's
/// `is_alphabetic` is the Unicode property, so `isLetter` is that property
/// minus these inclusive ranges. Enumerated from openjdk 27
/// (`Character.isAlphabetic(c) && !Character.isLetter(c)` over
/// U+0000..U+10FFFF), because Rust exposes no general-category table.
const ALPHABETIC_NON_LETTERS: &[(u32, u32)] = &[
    (0x345, 0x345),
    (0x363, 0x36F),
    (0x5B0, 0x5BD),
    (0x5BF, 0x5BF),
    (0x5C1, 0x5C2),
    (0x5C4, 0x5C5),
    (0x5C7, 0x5C7),
    (0x610, 0x61A),
    (0x64B, 0x657),
    (0x659, 0x65F),
    (0x670, 0x670),
    (0x6D6, 0x6DC),
    (0x6E1, 0x6E4),
    (0x6E7, 0x6E8),
    (0x6ED, 0x6ED),
    (0x711, 0x711),
    (0x730, 0x73F),
    (0x7A6, 0x7B0),
    (0x816, 0x817),
    (0x81B, 0x823),
    (0x825, 0x827),
    (0x829, 0x82C),
    (0x897, 0x897),
    (0x8D4, 0x8DF),
    (0x8E3, 0x8E9),
    (0x8F0, 0x903),
    (0x93A, 0x93B),
    (0x93E, 0x94C),
    (0x94E, 0x94F),
    (0x955, 0x957),
    (0x962, 0x963),
    (0x981, 0x983),
    (0x9BE, 0x9C4),
    (0x9C7, 0x9C8),
    (0x9CB, 0x9CC),
    (0x9D7, 0x9D7),
    (0x9E2, 0x9E3),
    (0xA01, 0xA03),
    (0xA3E, 0xA42),
    (0xA47, 0xA48),
    (0xA4B, 0xA4C),
    (0xA51, 0xA51),
    (0xA70, 0xA71),
    (0xA75, 0xA75),
    (0xA81, 0xA83),
    (0xABE, 0xAC5),
    (0xAC7, 0xAC9),
    (0xACB, 0xACC),
    (0xAE2, 0xAE3),
    (0xAFA, 0xAFC),
    (0xB01, 0xB03),
    (0xB3E, 0xB44),
    (0xB47, 0xB48),
    (0xB4B, 0xB4C),
    (0xB56, 0xB57),
    (0xB62, 0xB63),
    (0xB82, 0xB82),
    (0xBBE, 0xBC2),
    (0xBC6, 0xBC8),
    (0xBCA, 0xBCC),
    (0xBD7, 0xBD7),
    (0xC00, 0xC04),
    (0xC3E, 0xC44),
    (0xC46, 0xC48),
    (0xC4A, 0xC4C),
    (0xC55, 0xC56),
    (0xC62, 0xC63),
    (0xC81, 0xC83),
    (0xCBE, 0xCC4),
    (0xCC6, 0xCC8),
    (0xCCA, 0xCCC),
    (0xCD5, 0xCD6),
    (0xCE2, 0xCE3),
    (0xCF3, 0xCF3),
    (0xD00, 0xD03),
    (0xD3E, 0xD44),
    (0xD46, 0xD48),
    (0xD4A, 0xD4C),
    (0xD57, 0xD57),
    (0xD62, 0xD63),
    (0xD81, 0xD83),
    (0xDCF, 0xDD4),
    (0xDD6, 0xDD6),
    (0xDD8, 0xDDF),
    (0xDF2, 0xDF3),
    (0xE31, 0xE31),
    (0xE34, 0xE3A),
    (0xE4D, 0xE4D),
    (0xEB1, 0xEB1),
    (0xEB4, 0xEB9),
    (0xEBB, 0xEBC),
    (0xECD, 0xECD),
    (0xF71, 0xF83),
    (0xF8D, 0xF97),
    (0xF99, 0xFBC),
    (0x102B, 0x1036),
    (0x1038, 0x1038),
    (0x103B, 0x103E),
    (0x1056, 0x1059),
    (0x105E, 0x1060),
    (0x1062, 0x1064),
    (0x1067, 0x106D),
    (0x1071, 0x1074),
    (0x1082, 0x108D),
    (0x108F, 0x108F),
    (0x109A, 0x109D),
    (0x16EE, 0x16F0),
    (0x1712, 0x1713),
    (0x1732, 0x1733),
    (0x1752, 0x1753),
    (0x1772, 0x1773),
    (0x17B6, 0x17C8),
    (0x1885, 0x1886),
    (0x18A9, 0x18A9),
    (0x1920, 0x192B),
    (0x1930, 0x1938),
    (0x1A17, 0x1A1B),
    (0x1A55, 0x1A5E),
    (0x1A61, 0x1A74),
    (0x1ABF, 0x1AC0),
    (0x1ACC, 0x1ACE),
    (0x1B00, 0x1B04),
    (0x1B35, 0x1B43),
    (0x1B80, 0x1B82),
    (0x1BA1, 0x1BA9),
    (0x1BAC, 0x1BAD),
    (0x1BE7, 0x1BF1),
    (0x1C24, 0x1C36),
    (0x1DD3, 0x1DF4),
    (0x2160, 0x2182),
    (0x2185, 0x2188),
    (0x24B6, 0x24E9),
    (0x2DE0, 0x2DFF),
    (0x3007, 0x3007),
    (0x3021, 0x3029),
    (0x3038, 0x303A),
    (0xA674, 0xA67B),
    (0xA69E, 0xA69F),
    (0xA6E6, 0xA6EF),
    (0xA802, 0xA802),
    (0xA80B, 0xA80B),
    (0xA823, 0xA827),
    (0xA880, 0xA881),
    (0xA8B4, 0xA8C3),
    (0xA8C5, 0xA8C5),
    (0xA8FF, 0xA8FF),
    (0xA926, 0xA92A),
    (0xA947, 0xA952),
    (0xA980, 0xA983),
    (0xA9B4, 0xA9BF),
    (0xA9E5, 0xA9E5),
    (0xAA29, 0xAA36),
    (0xAA43, 0xAA43),
    (0xAA4C, 0xAA4D),
    (0xAA7B, 0xAA7D),
    (0xAAB0, 0xAAB0),
    (0xAAB2, 0xAAB4),
    (0xAAB7, 0xAAB8),
    (0xAABE, 0xAABE),
    (0xAAEB, 0xAAEF),
    (0xAAF5, 0xAAF5),
    (0xABE3, 0xABEA),
    (0xFB1E, 0xFB1E),
    (0x10140, 0x10174),
    (0x10341, 0x10341),
    (0x1034A, 0x1034A),
    (0x10376, 0x1037A),
    (0x103D1, 0x103D5),
    (0x10A01, 0x10A03),
    (0x10A05, 0x10A06),
    (0x10A0C, 0x10A0F),
    (0x10D24, 0x10D27),
    (0x10D69, 0x10D69),
    (0x10EAB, 0x10EAC),
    (0x10EFA, 0x10EFC),
    (0x11000, 0x11002),
    (0x11038, 0x11045),
    (0x11073, 0x11074),
    (0x11080, 0x11082),
    (0x110B0, 0x110B8),
    (0x110C2, 0x110C2),
    (0x11100, 0x11102),
    (0x11127, 0x11132),
    (0x11145, 0x11146),
    (0x11180, 0x11182),
    (0x111B3, 0x111BF),
    (0x111CE, 0x111CF),
    (0x1122C, 0x11234),
    (0x11237, 0x11237),
    (0x1123E, 0x1123E),
    (0x11241, 0x11241),
    (0x112DF, 0x112E8),
    (0x11300, 0x11303),
    (0x1133E, 0x11344),
    (0x11347, 0x11348),
    (0x1134B, 0x1134C),
    (0x11357, 0x11357),
    (0x11362, 0x11363),
    (0x113B8, 0x113C0),
    (0x113C2, 0x113C2),
    (0x113C5, 0x113C5),
    (0x113C7, 0x113CA),
    (0x113CC, 0x113CD),
    (0x11435, 0x11441),
    (0x11443, 0x11445),
    (0x114B0, 0x114C1),
    (0x115AF, 0x115B5),
    (0x115B8, 0x115BE),
    (0x115DC, 0x115DD),
    (0x11630, 0x1163E),
    (0x11640, 0x11640),
    (0x116AB, 0x116B5),
    (0x1171D, 0x1172A),
    (0x1182C, 0x11838),
    (0x11930, 0x11935),
    (0x11937, 0x11938),
    (0x1193B, 0x1193C),
    (0x11940, 0x11940),
    (0x11942, 0x11942),
    (0x119D1, 0x119D7),
    (0x119DA, 0x119DF),
    (0x119E4, 0x119E4),
    (0x11A01, 0x11A0A),
    (0x11A35, 0x11A39),
    (0x11A3B, 0x11A3E),
    (0x11A51, 0x11A5B),
    (0x11A8A, 0x11A97),
    (0x11B60, 0x11B67),
    (0x11C2F, 0x11C36),
    (0x11C38, 0x11C3E),
    (0x11C92, 0x11CA7),
    (0x11CA9, 0x11CB6),
    (0x11D31, 0x11D36),
    (0x11D3A, 0x11D3A),
    (0x11D3C, 0x11D3D),
    (0x11D3F, 0x11D41),
    (0x11D43, 0x11D43),
    (0x11D47, 0x11D47),
    (0x11D8A, 0x11D8E),
    (0x11D90, 0x11D91),
    (0x11D93, 0x11D96),
    (0x11EF3, 0x11EF6),
    (0x11F00, 0x11F01),
    (0x11F03, 0x11F03),
    (0x11F34, 0x11F3A),
    (0x11F3E, 0x11F40),
    (0x12400, 0x1246E),
    (0x1611E, 0x1612E),
    (0x16F4F, 0x16F4F),
    (0x16F51, 0x16F87),
    (0x16F8F, 0x16F92),
    (0x16FF0, 0x16FF1),
    (0x16FF4, 0x16FF6),
    (0x1BC9E, 0x1BC9E),
    (0x1E000, 0x1E006),
    (0x1E008, 0x1E018),
    (0x1E01B, 0x1E021),
    (0x1E023, 0x1E024),
    (0x1E026, 0x1E02A),
    (0x1E08F, 0x1E08F),
    (0x1E6E3, 0x1E6E3),
    (0x1E6E6, 0x1E6E6),
    (0x1E6EE, 0x1E6EF),
    (0x1E6F5, 0x1E6F5),
    (0x1E947, 0x1E947),
    (0x1F130, 0x1F149),
    (0x1F150, 0x1F169),
    (0x1F170, 0x1F189),
];

/// Java's `Character.isLetter`: general category Lu, Ll, Lt, Lm, or Lo.
fn java_is_letter(c: char) -> bool {
    let c32 = c as u32;
    let i = ALPHABETIC_NON_LETTERS.partition_point(|&(lo, _)| lo <= c32);
    c.is_alphabetic() && !(i > 0 && c32 <= ALPHABETIC_NON_LETTERS[i - 1].1)
}

/// The first code point (the digit zero) of every run of Unicode
/// DECIMAL_DIGIT_NUMBER characters. Unicode guarantees each run is ten
/// contiguous code points, zero through nine, so a code point's digit value is
/// its distance from the zero below it. Enumerated from openjdk 27
/// (`Character.getType(c) == DECIMAL_DIGIT_NUMBER && Character.digit(c, 10) ==
/// 0` over U+0000..U+10FFFF), because Rust exposes no general-category table.
const DECIMAL_DIGIT_ZEROS: &[u32] = &[
    0x30, 0x660, 0x6F0, 0x7C0, 0x966, 0x9E6, 0xA66, 0xAE6, 0xB66, 0xBE6, 0xC66, 0xCE6, 0xD66,
    0xDE6, 0xE50, 0xED0, 0xF20, 0x1040, 0x1090, 0x17E0, 0x1810, 0x1946, 0x19D0, 0x1A80, 0x1A90,
    0x1B50, 0x1BB0, 0x1C40, 0x1C50, 0xA620, 0xA8D0, 0xA900, 0xA9D0, 0xA9F0, 0xAA50, 0xABF0, 0xFF10,
    0x104A0, 0x10D30, 0x10D40, 0x11066, 0x110F0, 0x11136, 0x111D0, 0x112F0, 0x11450, 0x114D0,
    0x11650, 0x116C0, 0x116D0, 0x116DA, 0x11730, 0x118E0, 0x11950, 0x11BF0, 0x11C50, 0x11D50,
    0x11DA0, 0x11DE0, 0x11F50, 0x16130, 0x16A60, 0x16AC0, 0x16B50, 0x16D70, 0x1CCF0, 0x1D7CE,
    0x1D7D8, 0x1D7E2, 0x1D7EC, 0x1D7F6, 0x1E140, 0x1E2F0, 0x1E4F0, 0x1E5F1, 0x1E950, 0x1FBF0,
];

/// The value of a DECIMAL_DIGIT_NUMBER code point, or `None` for any other.
fn decimal_digit_value(c: u32) -> Option<u32> {
    let i = DECIMAL_DIGIT_ZEROS.partition_point(|&z| z <= c);
    let zero = *DECIMAL_DIGIT_ZEROS.get(i.checked_sub(1)?)?;
    (c - zero < 10).then_some(c - zero)
}

/// The code points `Character.isJavaIdentifierStart` accepts that are not
/// letters: the currency symbols (Sc), the connector punctuation (Pc), and the
/// letter numbers (Nl). Enumerated from openjdk 27 (`isJavaIdentifierStart(c)
/// && !isLetter(c)` over U+0000..U+10FFFF), for the reason the digit table
/// above is: Rust exposes no general-category table.
const JAVA_IDENT_START_EXTRA: &[(u32, u32)] = &[
    (0x24, 0x24),
    (0x5F, 0x5F),
    (0xA2, 0xA5),
    (0x58F, 0x58F),
    (0x60B, 0x60B),
    (0x7FE, 0x7FF),
    (0x9F2, 0x9F3),
    (0x9FB, 0x9FB),
    (0xAF1, 0xAF1),
    (0xBF9, 0xBF9),
    (0xE3F, 0xE3F),
    (0x16EE, 0x16F0),
    (0x17DB, 0x17DB),
    (0x203F, 0x2040),
    (0x2054, 0x2054),
    (0x20A0, 0x20C1),
    (0x2160, 0x2182),
    (0x2185, 0x2188),
    (0x3007, 0x3007),
    (0x3021, 0x3029),
    (0x3038, 0x303A),
    (0xA6E6, 0xA6EF),
    (0xA838, 0xA838),
    (0xFDFC, 0xFDFC),
    (0xFE33, 0xFE34),
    (0xFE4D, 0xFE4F),
    (0xFE69, 0xFE69),
    (0xFF04, 0xFF04),
    (0xFF3F, 0xFF3F),
    (0xFFE0, 0xFFE1),
    (0xFFE5, 0xFFE6),
    (0x10140, 0x10174),
    (0x10341, 0x10341),
    (0x1034A, 0x1034A),
    (0x103D1, 0x103D5),
    (0x11FDD, 0x11FE0),
    (0x12400, 0x1246E),
    (0x16FF4, 0x16FF6),
    (0x1E2FF, 0x1E2FF),
    (0x1ECB0, 0x1ECB0),
];

/// The code points `Character.isJavaIdentifierPart` accepts beyond every
/// identifier start and every decimal digit: the combining marks (Mn, Mc) and
/// the identifier-ignorable controls and format characters. Enumerated from
/// openjdk 27 (`isJavaIdentifierPart(c) && !isJavaIdentifierStart(c) &&
/// !isDigit(c)`).
const JAVA_IDENT_PART_EXTRA: &[(u32, u32)] = &[
    (0x0, 0x8),
    (0xE, 0x1B),
    (0x7F, 0x9F),
    (0xAD, 0xAD),
    (0x300, 0x36F),
    (0x483, 0x487),
    (0x591, 0x5BD),
    (0x5BF, 0x5BF),
    (0x5C1, 0x5C2),
    (0x5C4, 0x5C5),
    (0x5C7, 0x5C7),
    (0x600, 0x605),
    (0x610, 0x61A),
    (0x61C, 0x61C),
    (0x64B, 0x65F),
    (0x670, 0x670),
    (0x6D6, 0x6DD),
    (0x6DF, 0x6E4),
    (0x6E7, 0x6E8),
    (0x6EA, 0x6ED),
    (0x70F, 0x70F),
    (0x711, 0x711),
    (0x730, 0x74A),
    (0x7A6, 0x7B0),
    (0x7EB, 0x7F3),
    (0x7FD, 0x7FD),
    (0x816, 0x819),
    (0x81B, 0x823),
    (0x825, 0x827),
    (0x829, 0x82D),
    (0x859, 0x85B),
    (0x890, 0x891),
    (0x897, 0x89F),
    (0x8CA, 0x903),
    (0x93A, 0x93C),
    (0x93E, 0x94F),
    (0x951, 0x957),
    (0x962, 0x963),
    (0x981, 0x983),
    (0x9BC, 0x9BC),
    (0x9BE, 0x9C4),
    (0x9C7, 0x9C8),
    (0x9CB, 0x9CD),
    (0x9D7, 0x9D7),
    (0x9E2, 0x9E3),
    (0x9FE, 0x9FE),
    (0xA01, 0xA03),
    (0xA3C, 0xA3C),
    (0xA3E, 0xA42),
    (0xA47, 0xA48),
    (0xA4B, 0xA4D),
    (0xA51, 0xA51),
    (0xA70, 0xA71),
    (0xA75, 0xA75),
    (0xA81, 0xA83),
    (0xABC, 0xABC),
    (0xABE, 0xAC5),
    (0xAC7, 0xAC9),
    (0xACB, 0xACD),
    (0xAE2, 0xAE3),
    (0xAFA, 0xAFF),
    (0xB01, 0xB03),
    (0xB3C, 0xB3C),
    (0xB3E, 0xB44),
    (0xB47, 0xB48),
    (0xB4B, 0xB4D),
    (0xB55, 0xB57),
    (0xB62, 0xB63),
    (0xB82, 0xB82),
    (0xBBE, 0xBC2),
    (0xBC6, 0xBC8),
    (0xBCA, 0xBCD),
    (0xBD7, 0xBD7),
    (0xC00, 0xC04),
    (0xC3C, 0xC3C),
    (0xC3E, 0xC44),
    (0xC46, 0xC48),
    (0xC4A, 0xC4D),
    (0xC55, 0xC56),
    (0xC62, 0xC63),
    (0xC81, 0xC83),
    (0xCBC, 0xCBC),
    (0xCBE, 0xCC4),
    (0xCC6, 0xCC8),
    (0xCCA, 0xCCD),
    (0xCD5, 0xCD6),
    (0xCE2, 0xCE3),
    (0xCF3, 0xCF3),
    (0xD00, 0xD03),
    (0xD3B, 0xD3C),
    (0xD3E, 0xD44),
    (0xD46, 0xD48),
    (0xD4A, 0xD4D),
    (0xD57, 0xD57),
    (0xD62, 0xD63),
    (0xD81, 0xD83),
    (0xDCA, 0xDCA),
    (0xDCF, 0xDD4),
    (0xDD6, 0xDD6),
    (0xDD8, 0xDDF),
    (0xDF2, 0xDF3),
    (0xE31, 0xE31),
    (0xE34, 0xE3A),
    (0xE47, 0xE4E),
    (0xEB1, 0xEB1),
    (0xEB4, 0xEBC),
    (0xEC8, 0xECE),
    (0xF18, 0xF19),
    (0xF35, 0xF35),
    (0xF37, 0xF37),
    (0xF39, 0xF39),
    (0xF3E, 0xF3F),
    (0xF71, 0xF84),
    (0xF86, 0xF87),
    (0xF8D, 0xF97),
    (0xF99, 0xFBC),
    (0xFC6, 0xFC6),
    (0x102B, 0x103E),
    (0x1056, 0x1059),
    (0x105E, 0x1060),
    (0x1062, 0x1064),
    (0x1067, 0x106D),
    (0x1071, 0x1074),
    (0x1082, 0x108D),
    (0x108F, 0x108F),
    (0x109A, 0x109D),
    (0x135D, 0x135F),
    (0x1712, 0x1715),
    (0x1732, 0x1734),
    (0x1752, 0x1753),
    (0x1772, 0x1773),
    (0x17B4, 0x17D3),
    (0x17DD, 0x17DD),
    (0x180B, 0x180F),
    (0x1885, 0x1886),
    (0x18A9, 0x18A9),
    (0x1920, 0x192B),
    (0x1930, 0x193B),
    (0x1A17, 0x1A1B),
    (0x1A55, 0x1A5E),
    (0x1A60, 0x1A7C),
    (0x1A7F, 0x1A7F),
    (0x1AB0, 0x1ABD),
    (0x1ABF, 0x1ADD),
    (0x1AE0, 0x1AEB),
    (0x1B00, 0x1B04),
    (0x1B34, 0x1B44),
    (0x1B6B, 0x1B73),
    (0x1B80, 0x1B82),
    (0x1BA1, 0x1BAD),
    (0x1BE6, 0x1BF3),
    (0x1C24, 0x1C37),
    (0x1CD0, 0x1CD2),
    (0x1CD4, 0x1CE8),
    (0x1CED, 0x1CED),
    (0x1CF4, 0x1CF4),
    (0x1CF7, 0x1CF9),
    (0x1DC0, 0x1DFF),
    (0x200B, 0x200F),
    (0x202A, 0x202E),
    (0x2060, 0x2064),
    (0x2066, 0x206F),
    (0x20D0, 0x20DC),
    (0x20E1, 0x20E1),
    (0x20E5, 0x20F0),
    (0x2CEF, 0x2CF1),
    (0x2D7F, 0x2D7F),
    (0x2DE0, 0x2DFF),
    (0x302A, 0x302F),
    (0x3099, 0x309A),
    (0xA66F, 0xA66F),
    (0xA674, 0xA67D),
    (0xA69E, 0xA69F),
    (0xA6F0, 0xA6F1),
    (0xA802, 0xA802),
    (0xA806, 0xA806),
    (0xA80B, 0xA80B),
    (0xA823, 0xA827),
    (0xA82C, 0xA82C),
    (0xA880, 0xA881),
    (0xA8B4, 0xA8C5),
    (0xA8E0, 0xA8F1),
    (0xA8FF, 0xA8FF),
    (0xA926, 0xA92D),
    (0xA947, 0xA953),
    (0xA980, 0xA983),
    (0xA9B3, 0xA9C0),
    (0xA9E5, 0xA9E5),
    (0xAA29, 0xAA36),
    (0xAA43, 0xAA43),
    (0xAA4C, 0xAA4D),
    (0xAA7B, 0xAA7D),
    (0xAAB0, 0xAAB0),
    (0xAAB2, 0xAAB4),
    (0xAAB7, 0xAAB8),
    (0xAABE, 0xAABF),
    (0xAAC1, 0xAAC1),
    (0xAAEB, 0xAAEF),
    (0xAAF5, 0xAAF6),
    (0xABE3, 0xABEA),
    (0xABEC, 0xABED),
    (0xFB1E, 0xFB1E),
    (0xFE00, 0xFE0F),
    (0xFE20, 0xFE2F),
    (0xFEFF, 0xFEFF),
    (0xFFF9, 0xFFFB),
    (0x101FD, 0x101FD),
    (0x102E0, 0x102E0),
    (0x10376, 0x1037A),
    (0x10A01, 0x10A03),
    (0x10A05, 0x10A06),
    (0x10A0C, 0x10A0F),
    (0x10A38, 0x10A3A),
    (0x10A3F, 0x10A3F),
    (0x10AE5, 0x10AE6),
    (0x10D24, 0x10D27),
    (0x10D69, 0x10D6D),
    (0x10EAB, 0x10EAC),
    (0x10EFA, 0x10EFF),
    (0x10F46, 0x10F50),
    (0x10F82, 0x10F85),
    (0x11000, 0x11002),
    (0x11038, 0x11046),
    (0x11070, 0x11070),
    (0x11073, 0x11074),
    (0x1107F, 0x11082),
    (0x110B0, 0x110BA),
    (0x110BD, 0x110BD),
    (0x110C2, 0x110C2),
    (0x110CD, 0x110CD),
    (0x11100, 0x11102),
    (0x11127, 0x11134),
    (0x11145, 0x11146),
    (0x11173, 0x11173),
    (0x11180, 0x11182),
    (0x111B3, 0x111C0),
    (0x111C9, 0x111CC),
    (0x111CE, 0x111CF),
    (0x1122C, 0x11237),
    (0x1123E, 0x1123E),
    (0x11241, 0x11241),
    (0x112DF, 0x112EA),
    (0x11300, 0x11303),
    (0x1133B, 0x1133C),
    (0x1133E, 0x11344),
    (0x11347, 0x11348),
    (0x1134B, 0x1134D),
    (0x11357, 0x11357),
    (0x11362, 0x11363),
    (0x11366, 0x1136C),
    (0x11370, 0x11374),
    (0x113B8, 0x113C0),
    (0x113C2, 0x113C2),
    (0x113C5, 0x113C5),
    (0x113C7, 0x113CA),
    (0x113CC, 0x113D0),
    (0x113D2, 0x113D2),
    (0x113E1, 0x113E2),
    (0x11435, 0x11446),
    (0x1145E, 0x1145E),
    (0x114B0, 0x114C3),
    (0x115AF, 0x115B5),
    (0x115B8, 0x115C0),
    (0x115DC, 0x115DD),
    (0x11630, 0x11640),
    (0x116AB, 0x116B7),
    (0x1171D, 0x1172B),
    (0x1182C, 0x1183A),
    (0x11930, 0x11935),
    (0x11937, 0x11938),
    (0x1193B, 0x1193E),
    (0x11940, 0x11940),
    (0x11942, 0x11943),
    (0x119D1, 0x119D7),
    (0x119DA, 0x119E0),
    (0x119E4, 0x119E4),
    (0x11A01, 0x11A0A),
    (0x11A33, 0x11A39),
    (0x11A3B, 0x11A3E),
    (0x11A47, 0x11A47),
    (0x11A51, 0x11A5B),
    (0x11A8A, 0x11A99),
    (0x11B60, 0x11B67),
    (0x11C2F, 0x11C36),
    (0x11C38, 0x11C3F),
    (0x11C92, 0x11CA7),
    (0x11CA9, 0x11CB6),
    (0x11D31, 0x11D36),
    (0x11D3A, 0x11D3A),
    (0x11D3C, 0x11D3D),
    (0x11D3F, 0x11D45),
    (0x11D47, 0x11D47),
    (0x11D8A, 0x11D8E),
    (0x11D90, 0x11D91),
    (0x11D93, 0x11D97),
    (0x11EF3, 0x11EF6),
    (0x11F00, 0x11F01),
    (0x11F03, 0x11F03),
    (0x11F34, 0x11F3A),
    (0x11F3E, 0x11F42),
    (0x11F5A, 0x11F5A),
    (0x13430, 0x13440),
    (0x13447, 0x13455),
    (0x1611E, 0x1612F),
    (0x16AF0, 0x16AF4),
    (0x16B30, 0x16B36),
    (0x16F4F, 0x16F4F),
    (0x16F51, 0x16F87),
    (0x16F8F, 0x16F92),
    (0x16FE4, 0x16FE4),
    (0x16FF0, 0x16FF1),
    (0x1BC9D, 0x1BC9E),
    (0x1BCA0, 0x1BCA3),
    (0x1CF00, 0x1CF2D),
    (0x1CF30, 0x1CF46),
    (0x1D165, 0x1D169),
    (0x1D16D, 0x1D182),
    (0x1D185, 0x1D18B),
    (0x1D1AA, 0x1D1AD),
    (0x1D242, 0x1D244),
    (0x1DA00, 0x1DA36),
    (0x1DA3B, 0x1DA6C),
    (0x1DA75, 0x1DA75),
    (0x1DA84, 0x1DA84),
    (0x1DA9B, 0x1DA9F),
    (0x1DAA1, 0x1DAAF),
    (0x1E000, 0x1E006),
    (0x1E008, 0x1E018),
    (0x1E01B, 0x1E021),
    (0x1E023, 0x1E024),
    (0x1E026, 0x1E02A),
    (0x1E08F, 0x1E08F),
    (0x1E130, 0x1E136),
    (0x1E2AE, 0x1E2AE),
    (0x1E2EC, 0x1E2EF),
    (0x1E4EC, 0x1E4EF),
    (0x1E5EE, 0x1E5EF),
    (0x1E6E3, 0x1E6E3),
    (0x1E6E6, 0x1E6E6),
    (0x1E6EE, 0x1E6EF),
    (0x1E6F5, 0x1E6F5),
    (0x1E8D0, 0x1E8D6),
    (0x1E944, 0x1E94A),
    (0xE0001, 0xE0001),
    (0xE0020, 0xE007F),
    (0xE0100, 0xE01EF),
];

/// Whether `c` falls in one of the sorted, disjoint `ranges`.
fn in_ranges(ranges: &[(u32, u32)], c: u32) -> bool {
    let i = ranges.partition_point(|&(lo, _)| lo <= c);
    i > 0 && c <= ranges[i - 1].1
}

/// `Character.isJavaIdentifierStart(int)`.
fn java_ident_start(c: u32) -> bool {
    char::from_u32(c).is_some_and(java_is_letter) || in_ranges(JAVA_IDENT_START_EXTRA, c)
}

/// `Character.isJavaIdentifierPart(int)`.
fn java_ident_part(c: u32) -> bool {
    java_ident_start(c) || decimal_digit_value(c).is_some() || in_ranges(JAVA_IDENT_PART_EXTRA, c)
}

/// `Character.digit(c, radix)`: a decimal digit of any script, or a Latin
/// letter (ASCII or fullwidth) standing for 10..35, when that value is below
/// `radix`; -1 otherwise, and for a radix outside 2..=36.
fn java_digit(c: u32, radix: i64) -> i64 {
    if !(2..=36).contains(&radix) {
        return -1;
    }
    let value = decimal_digit_value(c).or(match c {
        0x41..=0x5A => Some(c - 0x41 + 10),
        0x61..=0x7A => Some(c - 0x61 + 10),
        0xFF21..=0xFF3A => Some(c - 0xFF21 + 10),
        0xFF41..=0xFF5A => Some(c - 0xFF41 + 10),
        _ => None,
    });
    match value {
        Some(v) if i64::from(v) < radix => i64::from(v),
        _ => -1,
    }
}

/// Java's `Character.isWhitespace(int)`.
///
/// `String.lines()`'s lines: `\n`, `\r\n` and a lone `\r` each end one, and the
/// text after the last terminator is a line only when it is not empty, so
/// `"a\n"` is one line and `""` none. Rust's `str::lines` treats a lone `\r`
/// as ordinary text, which is why this scans by hand.
fn java_lines(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut line = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\n' => out.push(std::mem::take(&mut line)),
            '\r' => {
                if it.peek() == Some(&'\n') {
                    it.next();
                }
                out.push(std::mem::take(&mut line));
            }
            other => line.push(other),
        }
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

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

/// `Double.toHexString`: `NaN`/`Infinity` spelled out, a signed `0x0.0p0` for
/// zero, and otherwise `0x1.` (normal) or `0x0.` (subnormal, with the fixed
/// exponent `p-1022`) followed by the 13-digit significand with its trailing
/// zeros dropped — one `0` kept — and the unbiased binary exponent.
fn double_hex_string(d: f64) -> String {
    if d.is_nan() {
        return "NaN".to_string();
    }
    if d.is_infinite() {
        return if d > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    let sign = if d.is_sign_negative() { "-" } else { "" };
    if d == 0.0 {
        return format!("{sign}0x0.0p0");
    }
    let bits = d.to_bits();
    let biased = ((bits >> 52) & 0x7ff) as i64;
    let digits = format!("{:013x}", bits & 0x000f_ffff_ffff_ffff);
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    if biased == 0 {
        format!("{sign}0x0.{digits}p-1022")
    } else {
        format!("{sign}0x1.{digits}p{}", biased - 1023)
    }
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
///
/// `single` is `Float.parseFloat`: the text is rounded once, straight to 32
/// bits, since rounding to `f64` first and then to `f32` can land an ulp off.
/// A hexadecimal significand (`0x1.8p1`) is accepted too, as
/// `FloatingDecimal.parseHexString` does.
fn parse_java_double(text: &str, single: bool) -> Option<f64> {
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
    if let Some(hex) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        // `_` is literal syntax, not part of the string grammar.
        if hex.contains('_') {
            return None;
        }
        return crate::lexer::hex_float(hex, single).map(signed);
    }
    if !is_java_decimal_literal(digits) {
        return None;
    }
    // The grammar is now known to be a subset of Rust's, so the conversion
    // itself — correctly rounded in both — can be delegated.
    if single {
        digits.parse::<f32>().ok().map(|f| signed(f64::from(f)))
    } else {
        digits.parse::<f64>().ok().map(signed)
    }
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

/// `Character.toTitleCase`: UnicodeData's simple titlecase mapping. It is the
/// simple uppercase mapping except for the characters the table gives a
/// titlecase of their own — the Latin digraphs, whose titlecase form is the
/// capital-then-small letter (`ǆ` → `ǅ`), and the Georgian Mkhedruli letters,
/// which have an uppercase (Mtavruli) since Unicode 11 but title-case to
/// themselves.
fn title_case(c: char) -> i64 {
    match c as u32 {
        0x01C4..=0x01C6 => 0x01C5,
        0x01C7..=0x01C9 => 0x01C8,
        0x01CA..=0x01CC => 0x01CB,
        0x01F1..=0x01F3 => 0x01F2,
        0x10D0..=0x10FA | 0x10FD..=0x10FF => c as i64,
        _ => one_to_one_case(c, char::to_uppercase),
    }
}

/// A `Character` static's argument as a code point — the `int` overloads'
/// view, which keeps a lone surrogate the surrogate it is.
fn code_point_arg(v: &Value) -> u32 {
    match &deboxed(v) {
        Value::Int(n) => *n as u32,
        other => other.as_str_cow().chars().next().map_or(0, u32::from),
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
/// `java.util.Arrays.rangeCheck`: the bounds every `Arrays` range method
/// (`sort`, `fill`, …) validates, message for message, before touching `a`.
fn arrays_range_check(len: usize, from: i64, to: i64) -> Result<(), Fault> {
    if from > to {
        return Err(Fault::java(
            "IllegalArgumentException",
            format!("fromIndex({from}) > toIndex({to})"),
        ));
    }
    if from < 0 {
        return Err(Fault::java(
            "ArrayIndexOutOfBoundsException",
            format!("Array index out of range: {from}"),
        ));
    }
    if to > len as i64 {
        return Err(Fault::java(
            "ArrayIndexOutOfBoundsException",
            format!("Array index out of range: {to}"),
        ));
    }
    Ok(())
}

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
/// The pad a grown array copy takes: the compiler's, when it could read the
/// source's *static* element type and sent it as a trailing operand, and
/// otherwise [`element_default`]'s guess from element 0.
///
/// The compiler's is the only answer available for an **empty** source, which
/// has no element 0 to read — `Arrays.copyOf(new int[0], 2)` is `[0, 0]` in
/// Java and was `[null, null]` here.
fn array_pad(extra: &[Value], items: &[Value]) -> Value {
    match extra.first() {
        Some(v) => v.clone(),
        None => element_default(items),
    }
}

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
/// conversions `d s S f e E g G a A b B h H x X o c %` and `%n`, all seven
/// flags, an optional width, an optional `.precision`, and explicit (`%2$s`)
/// and relative (`%<s`) argument indexes. Unsupported conversions
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
    // The index the previous argument-consuming specifier used, which a `<`
    // flag (`%<s`) reads again. `Formatter.format` keeps it as `last`.
    let mut last: Option<usize> = None;
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
        // `<` re-uses the previous specifier's argument.
        let mut relative = false;
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
                '<' => relative = true,
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
                // `<` wins over an explicit index, as in `Formatter.format`,
                // whose relative case is checked first.
                let idx = if relative {
                    last
                } else {
                    Some(explicit_index.unwrap_or(argi))
                };
                // Java's `MissingFormatArgumentException`, naming the specifier
                // that had no argument — not an internal javars error, which
                // aborted the run where Java lets the program catch it. A `%<s`
                // with no previous specifier is the same failure.
                let missing = || {
                    Fault::java(
                        "MissingFormatArgumentException",
                        format!("Format specifier '{spec}'"),
                    )
                };
                let idx = idx.ok_or_else(missing)?;
                let arg = args.get(idx).ok_or_else(missing)?;
                if !relative && explicit_index.is_none() {
                    argi += 1;
                }
                last = Some(idx);
                let tag = tags.get(idx).copied().unwrap_or("");
                check_conversion(conv, arg, tag)?;
                let Rendered {
                    mut prefix,
                    mut body,
                    numeric,
                } = format_conversion(conv, arg, width_n, prec, &flags, tag, vm.as_deref_mut())?;
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
                // `checkFloat`: `checkBadFlags(PARENTHESES, GROUP)`, in that order.
                'a' | 'A' => {
                    mismatch(&['('])?;
                    mismatch(&[','])?;
                }
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
    width: Option<usize>,
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
        // A non-finite floating argument never reaches the digit formatter
        // (`Formatter.print(double)`): NaN is `NaN` with no sign at all, an
        // infinity is the sign the flags ask for — `(` and `)` around a
        // negative one under `(` — then `Infinity`, and neither takes zero
        // padding or grouping, so both are justified with spaces as text.
        'f' | 'e' | 'E' | 'g' | 'G' if !arg.jfloat().is_finite() => {
            let x = arg.jfloat();
            let upper = conv.is_ascii_uppercase();
            if x.is_nan() {
                return Ok(Rendered::text(
                    if upper { "NAN" } else { "NaN" }.to_string(),
                ));
            }
            let word = if upper { "INFINITY" } else { "Infinity" };
            Ok(Rendered::text(match (x < 0.0, flags.parens) {
                (true, true) => format!("({word})"),
                (true, false) => format!("-{word}"),
                (false, _) => format!("{}{word}", pos_sign()),
            }))
        }
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
        // `%a` is the hexadecimal floating-point form `Double.toHexString`
        // writes, rounded to the precision's hex digits.
        'a' | 'A' => Ok(Rendered::text(format_hex_float(
            arg.jfloat(),
            width,
            prec,
            flags,
            conv == 'A',
        ))),
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

/// `Formatter`'s `%a`/`%A` of a `double`, ported from `print(double, …)` and
/// its `HEXADECIMAL_FLOAT` branch. The whole field is one piece of text: its
/// zero padding is computed here, after the `0x`, from the *unpadded* digit
/// string — so a precision that appends zeros makes the field wider than the
/// width, exactly as the JDK does — and any remaining width is filled with
/// spaces by the caller.
fn format_hex_float(
    value: f64,
    width: Option<usize>,
    prec: Option<usize>,
    flags: &FmtFlags,
    upper: bool,
) -> String {
    if value.is_nan() {
        return if upper { "NAN" } else { "NaN" }.to_string();
    }
    // `Double.compare(value, 0.0) == -1`, so `-0.0` takes the minus sign.
    let neg = value.is_sign_negative();
    let mut sb = String::new();
    if neg {
        sb.push('-');
    } else if flags.plus {
        sb.push('+');
    } else if flags.space {
        sb.push(' ');
    }
    let v = value.abs();
    if v.is_infinite() {
        sb.push_str(if upper { "INFINITY" } else { "Infinity" });
        return sb;
    }
    // An absent precision means "every digit", and an explicit 0 means 1.
    let prec = match prec {
        None => 0,
        Some(0) => 1,
        Some(p) => p,
    };
    let s = hex_double(v, prec);
    sb.push_str(if upper { "0X" } else { "0x" });
    if flags.zero {
        let lead = if flags.space || flags.plus || neg {
            3
        } else {
            2
        };
        let width = width.unwrap_or(0) as isize;
        let zeros = width - s.len() as isize - lead;
        sb.extend(std::iter::repeat_n('0', zeros.max(0) as usize));
    }
    let idx = s.find('p').unwrap_or(s.len());
    let mut va = if upper {
        s[..idx].to_uppercase()
    } else {
        s[..idx].to_string()
    };
    if prec != 0 {
        // `addZeros`: pad the fraction out to `prec` digits.
        let dot = va.find('.');
        let out_prec = dot.map_or(0, |i| va.len() - i - 1);
        if out_prec < prec {
            if dot.is_none() {
                va.push('.');
            }
            va.extend(std::iter::repeat_n('0', prec - out_prec));
        }
    }
    sb.push_str(&va);
    sb.push(if upper { 'P' } else { 'p' });
    sb.push_str(&s[(idx + 1).min(s.len())..]);
    sb
}

/// `Double.toHexString(d).substring(2)` for a finite, non-negative `d`: the
/// significand's hex digits with trailing zeros dropped, and the binary
/// exponent (`-1022` for a subnormal).
fn hex_string_unsigned(d: f64) -> String {
    if d == 0.0 {
        return "0.0p0".to_string();
    }
    let bits = d.to_bits();
    let subnormal = d < f64::MIN_POSITIVE;
    let signif = format!(
        "{:x}",
        (bits & 0x000F_FFFF_FFFF_FFFF) | 0x1000_0000_0000_0000
    );
    let digits = signif[3..16].trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    let exp = if subnormal {
        -1022
    } else {
        ((bits >> 52) & 0x7ff) as i64 - 1023
    };
    format!("{}.{digits}p{exp}", if subnormal { '0' } else { '1' })
}

/// `Formatter.hexDouble`: `d` (finite, non-negative) rounded half-even to
/// `prec` hex digits of significand, as `Double.toHexString` would write the
/// rounded value. A subnormal is normalized first, so its rounded form has a
/// leading `1.` and a true exponent below -1022.
fn hex_double(d: f64, prec: usize) -> String {
    if d == 0.0 || prec == 0 || prec >= 13 {
        return hex_string_unsigned(d);
    }
    let get_exponent = |x: f64| ((x.to_bits() >> 52) & 0x7ff) as i64 - 1023;
    let subnormal = get_exponent(d) == -1023;
    let d = if subnormal { d * 2f64.powi(54) } else { d };
    let shift = 53 - (1 + prec as u32 * 4);
    let doppel = d.to_bits();
    let mut signif = (doppel & 0x7FFF_FFFF_FFFF_FFFF) >> shift;
    let rounding = doppel & !(!0u64 << shift);
    let least_zero = signif & 1 == 0;
    let round = (1u64 << (shift - 1)) & rounding != 0;
    let sticky = shift > 1 && (!(1u64 << (shift - 1)) & rounding) != 0;
    if (least_zero && round && sticky) || (!least_zero && round) {
        signif += 1;
    }
    let result = f64::from_bits(signif << shift);
    if result.is_infinite() {
        return "1.0p1024".to_string();
    }
    let res = hex_string_unsigned(result);
    if !subnormal {
        return res;
    }
    let idx = res.find('p').unwrap_or(res.len());
    let exp: i64 = res[idx + 1..].parse().unwrap_or(0);
    format!("{}p{}", &res[..idx], exp - 54)
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

/// `Short.parseShort`/`Byte.parseByte` (and their `valueOf(String[, radix])`):
/// `Integer.parseInt` at the given radix, then the JDK's range check and its
/// `Value out of range. Value:"…" Radix:…` message.
fn parse_narrow(class: &str, args: &[Value]) -> Result<Value, Fault> {
    if matches!(args[0], Value::Undef) {
        return Err(null_number_fault());
    }
    let s = args[0].as_str_cow();
    let radix = args.get(1).map_or(10, Value::jint);
    let n = parse_int_radix(&s, radix, true)?.jint();
    let fits = if class == "Short" {
        i16::try_from(n).is_ok()
    } else {
        i8::try_from(n).is_ok()
    };
    if !fits {
        return Err(Fault::java(
            "NumberFormatException",
            format!("Value out of range. Value:\"{s}\" Radix:{radix}"),
        ));
    }
    Ok(Value::Int(n))
}

/// `Integer.parseUnsignedInt(s, radix)`: a non-negative number up to 2^32 - 1,
/// answered as the `int` with that bit pattern. A leading `-` and a value past
/// the range each carry the JDK's own message.
fn parse_unsigned_int(s: &str, radix: i64) -> Result<Value, Fault> {
    let nfe = |m: String| Fault::java("NumberFormatException", m);
    if s.starts_with('-') {
        return Err(nfe(format!(
            "Illegal leading minus sign on unsigned string {s}."
        )));
    }
    let n = parse_int_radix(s, radix, false)?.jint();
    if n > i64::from(u32::MAX) {
        return Err(nfe(format!(
            "String value {s} exceeds range of unsigned int."
        )));
    }
    Ok(Value::Int(i64::from(n as u32 as i32)))
}

/// `Long.parseUnsignedLong(s, radix)`: the digits read as an unsigned 64-bit
/// value and returned as its two's-complement `long`. A value that fits a
/// signed `long` is parsed (and refused) exactly as `Long.parseLong` does; one
/// past it is accepted up to `2^64 - 1`, as the JDK's last-digit step does.
fn parse_unsigned_long(s: &str, radix: i64) -> Result<Value, Fault> {
    let nfe = |m: String| Fault::java("NumberFormatException", m);
    if s.starts_with('-') {
        return Err(nfe(format!(
            "Illegal leading minus sign on unsigned string {s}."
        )));
    }
    if let Ok(v) = parse_int_radix(s, radix, false) {
        return Ok(v);
    }
    let digits = s.strip_prefix('+').unwrap_or(s);
    let all_digits = (2..=36).contains(&radix)
        && !digits.is_empty()
        && digits.chars().all(|c| c.is_digit(radix as u32));
    if !all_digits {
        return parse_int_radix(s, radix, false);
    }
    match u64::from_str_radix(digits, radix as u32) {
        Ok(n) => Ok(Value::Int(n as i64)),
        Err(_) => Err(nfe(format!(
            "String value {s} exceeds range of unsigned long."
        ))),
    }
}

/// `Integer.decode`: an optional sign, then a `0x`/`0X`/`#` hexadecimal or a
/// leading-`0` octal prefix, then digits — ported from `java.lang.Integer`,
/// including where each of its messages comes from.
fn java_decode(s: &str) -> Result<Value, Fault> {
    let nfe = |m: &str| Fault::java("NumberFormatException", m.to_string());
    if s.is_empty() {
        return Err(nfe("Zero length string"));
    }
    let (neg, rest) = match s.as_bytes()[0] {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    let (radix, digits) = if let Some(d) = rest.strip_prefix("0x").or(rest.strip_prefix("0X")) {
        (16, d)
    } else if let Some(d) = rest.strip_prefix('#') {
        (16, d)
    } else if rest.len() > 1 && rest.starts_with('0') {
        (8, &rest[1..])
    } else {
        (10, rest)
    };
    if digits.starts_with('-') || digits.starts_with('+') {
        return Err(nfe("Sign character in wrong position"));
    }
    // The JDK parses the digits, negates, and on failure re-parses the signed
    // text — so a failure, and `-0x80000000`, are both decided on the signed
    // text, which is what this parses directly.
    let signed = if neg {
        format!("-{digits}")
    } else {
        digits.to_string()
    };
    parse_int_radix(&signed, radix, true)
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
    /// One `Map.Entry`.
    Pair(Value, Value),
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
            Some(HostObj::PQueue { items, .. }) => RenderShape::Sequence(items.clone()),
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
            // A single pair, which renders `key=value` rather than the braced
            // form a whole map takes. Both sides go through their own
            // `toString()`, so an entry holding a user instance shows the
            // override.
            Some(HostObj::Entry) => match entry_pair(&Value::Obj(id)) {
                Some(p) => RenderShape::Pair(p.key, p.value),
                None => RenderShape::Opaque,
            },
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
        RenderShape::Pair(k, v) => format!("{}={}", java_str_vm(vm, &k), java_str_vm(vm, &v)),
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
            Some(HostObj::PQueue { items, .. }) => render_sequence(items),
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
            // `Map.Entry.toString()` is `key + "=" + value` — the same shape a
            // map's own rendering gives each pair, which is why an `entrySet`
            // prints as `[a=1, b=2]`.
            Some(HostObj::Entry) => match entry_pair(&Value::Obj(id)) {
                Some(p) => format!("{}={}", java_str(&p.key), java_str(&p.value)),
                None => format!("(entry:{id})"),
            },
            // Java renders a lambda as `Class$$Lambda/0x…@<identity hash>`,
            // which is not reproducible (and not stable across JVM runs), so
            // javars prints a fixed marker instead. See `BUGS.md`.
            Some(HostObj::Closure { .. }) => format!("<lambda>@{id:x}"),
            // A reader renders as `Object.toString()` does: class, `@`, hash.
            Some(HostObj::Reader(r)) => format!("{}@{id:x}", r.kind.class_name()),
            Some(HostObj::Tokenizer(_)) => format!("java.util.StringTokenizer@{id:x}"),
            Some(HostObj::Random(_)) => format!("java.util.Random@{id:x}"),
            Some(HostObj::Stats(s)) => s.render(),
            Some(HostObj::Bits(b)) => b.render(),
            Some(HostObj::Atomic { value, .. }) => java_str(value),
            // `Pattern.toString()` is its source; `Matcher.toString()` names the
            // pattern, the region and the last match.
            Some(HostObj::RegexPattern { shown, .. }) => shown.clone(),
            Some(HostObj::RegexMatcher(m)) => format!(
                "java.util.regex.Matcher[pattern={} region=0,{} lastmatch={}]",
                m.shown,
                m.text.encode_utf16().count(),
                m.first
                    .and_then(|_| m.groups.first().copied().flatten())
                    .map_or("", |(a, b)| &m.text[a..b])
            ),
            Some(HostObj::Boxed) => unreachable!("a box is answered above"),
            Some(HostObj::Iterator { .. }) => format!("<iterator>@{id:x}"),
            Some(HostObj::PQIter { .. }) => format!("<iterator>@{id:x}"),
            // `Optional[x]` / `Optional.empty` — the JDK's own rendering, and
            // the same shape for the three primitive specializations.
            Some(HostObj::Optional { class, value }) => match value {
                Some(v) => format!("{class}[{}]", java_str(v)),
                None => format!("{class}.empty"),
            },
            Some(HostObj::Stream { .. }) => format!("<stream>@{id:x}"),
            Some(HostObj::Collector { .. }) => format!("<collector>@{id:x}"),
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
    if f.is_finite() && f != 0.0 {
        // Two rules pick a different decimal from the one Rust's `{}` gives,
        // and they apply to disjoint values: the widening only where the
        // shortest form is a single digit, the nearest-candidate rule only
        // where it is longer.
        let corrected = if shortest_is_one_digit(&sci) {
            widen_exact(f)
        } else {
            nearest_shortest(f, &sci)
        };
        if let Some((digits, exp)) = corrected {
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
/// Java's rule for choosing *among* the shortest decimals, for the `p >= 2`
/// branch where [`widen_exact`]'s two-digit widening does not apply.
///
/// `Double.toString` takes the candidates to be the decimals of the minimal
/// length `p` that round-trips and answers **the one nearest the value, ties
/// to even**. Rust's `{}` also answers a shortest round-tripping decimal, but
/// not always that one: where two `p`-digit decimals both round-trip it may
/// pick either, and on an exact tie it takes the one further from zero every
/// time. Measured over 200 constructed ties (`n + 0.25` and friends, whose
/// exact expansion ends in a `5` one digit past the shortest form) the two
/// disagreed at 101, Java's last digit was even at 101 of 101, and Rust's was
/// one higher at 101 of 101:
///
/// ```text
/// 2236669075947798.25      java 2.2366690759477982E15   rust …83E15
/// -2.98023223876953125E-8  java -2.9802322387695312E-8  rust …13E-8
/// ```
///
/// The nearest `p`-digit decimal is read off the value's **exact** decimal
/// expansion — a double is a dyadic rational, so that expansion terminates,
/// and asking Rust for more digits than it has pads with zeros rather than
/// rounding it. The answer is the digits before position `p`, carried when
/// what follows is past half and rounded to even on exactly half. It is at
/// least as near the value as Rust's, so it round-trips too: the decimals that
/// round-trip form an interval around the value and both lie in it.
///
/// Returns the corrected `(digits, exp10)`, or `None` for the no-change case,
/// which is nearly every value. The exact expansion is rendered only when a
/// neighbouring candidate round-trips — with a single candidate there is
/// nothing to choose between, and that is the common case.
fn nearest_shortest(v: f64, sci: &str) -> Option<(String, i32)> {
    let (mantissa, exp) = sci.split_once('e')?;
    let exp: i32 = exp.parse().ok()?;
    let digits: Vec<u8> = mantissa.bytes().filter(u8::is_ascii_digit).collect();
    let p = digits.len();
    let sign = if v < 0.0 { "-" } else { "" };
    let round_trips = |d: &[u8], e: i32| {
        let text = String::from_utf8_lossy(d).into_owned();
        format!("{sign}{}", sci_form(&text, e))
            .parse::<f64>()
            .is_ok_and(|c| c.to_bits() == v.to_bits())
    };
    // Is there a second candidate at all? Only the two neighbours can be one.
    let mut lower = digits.clone();
    lower[p - 1] -= 1;
    let mut upper = digits.clone();
    upper[p - 1] += 1;
    let contested = (digits[p - 1] > b'1' && round_trips(&lower, exp))
        || (digits[p - 1] < b'9' && round_trips(&upper, exp));
    if !contested {
        return None;
    }
    // Enough digits to render any `f64` exactly: the longest expansion is the
    // smallest subnormal's, at 751 significant digits. Rust's fixed-precision
    // formatting is exact, so everything past the value's own digits is zeros
    // rather than a rounding of what would have followed.
    let exact = format!("{:.*e}", 1080, v.abs());
    let (exact_mantissa, _) = exact.split_once('e')?;
    let e: Vec<u8> = exact_mantissa.bytes().filter(u8::is_ascii_digit).collect();
    let mut head: Vec<u8> = e.get(..p)?.to_vec();
    let tail = e.get(p..)?;
    let past_half = tail[0] > b'5' || (tail[0] == b'5' && tail[1..].iter().any(|&d| d != b'0'));
    let half = tail[0] == b'5' && tail[1..].iter().all(|&d| d == b'0');
    if past_half || (half && (head[p - 1] - b'0') % 2 == 1) {
        // Propagate the carry. `99…9` becomes `10…0`, which is `1` a decade up.
        let mut at = p;
        while at > 0 {
            at -= 1;
            if head[at] == b'9' {
                head[at] = b'0';
            } else {
                head[at] += 1;
                break;
            }
        }
        if head.iter().all(|&d| d == b'0') {
            let one = b"1".to_vec();
            return round_trips(&one, exp + 1).then(|| ("1".to_string(), exp + 1));
        }
    }
    if head == digits {
        return None;
    }
    round_trips(&head, exp).then(|| (String::from_utf8_lossy(&head).into_owned(), exp))
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
    // `==`/`!=` involving a heap reference is reference identity — before the
    // numeric gate below, because a boxed wrapper is a number as well as a
    // reference and Java compares two of them as references.
    //
    // The one exception is a *mixed* box/primitive pair of numbers, which Java
    // unboxes (JLS 15.21.1): `anInteger == anInt` compares the numbers. Two
    // boxes are NOT that case however numeric they are — that is the whole
    // point of the wrapper model — so the exception needs exactly one handle.
    // Everything else a handle can be compared against stays identity,
    // including a `new String(…)` box against a `String` literal, which is
    // `false` because the constructor produced a fresh object.
    let both_handles = matches!((a, b), (Value::Obj(_), Value::Obj(_)));
    let one_handle = matches!(a, Value::Obj(_)) || matches!(b, Value::Obj(_));
    let unboxing_pair = one_handle && !both_handles && is_java_number(a) && is_java_number(b);
    if one_handle && !unboxing_pair {
        match op {
            NumOp::Eq => return Ok(Value::bool(ref_eq(a, b))),
            NumOp::Ne => return Ok(Value::bool(!ref_eq(a, b))),
            _ => {}
        }
    }
    // Two `String`s compare as *references*, which is what Java's `==` does on
    // them. The reference is the `Arc`: a literal is one pool entry per distinct
    // text (see `Compiler::string_literal`), so two occurrences share a pointer
    // and `"ab" == "ab"` is `true`, while every string built at run time —
    // a concatenation, a `substring`, a `String.valueOf` — allocates its own and
    // is `==` to nothing else. No heap box and no extra allocation: the
    // identity was already there in the representation, unread.
    if let (Value::Str(x), Value::Str(y)) = (a, b) {
        match op {
            NumOp::Eq => return Ok(Value::bool(std::sync::Arc::ptr_eq(x, y))),
            NumOp::Ne => return Ok(Value::bool(!std::sync::Arc::ptr_eq(x, y))),
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

/// Java `/` on operands whose types only the runtime knows. See [`JDIV_DYN`].
/// A box is read through to its value first: an erased `Integer` is a handle.
/// The integral quotient is `i64`-wide, which is the documented width of every
/// statically-untyped integral operation (see BUGS.md).
fn b_div_dyn(vm: &mut VM, _argc: u8) -> Value {
    let b = deboxed(&vm.stack.pop().unwrap_or(Value::Undef));
    let a = deboxed(&vm.stack.pop().unwrap_or(Value::Undef));
    match (&a, &b) {
        (Value::Int(_), Value::Int(0)) => {
            raise(vm, Fault::java("ArithmeticException", "/ by zero"))
        }
        (Value::Int(x), Value::Int(y)) => Value::Int(x.wrapping_div(*y)),
        _ => Value::float(as_f64(&a) / as_f64(&b)),
    }
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
        qualified if qualified.starts_with("java.") => qualified.to_string(),
        "List$fixed" => "java.util.Arrays$ArrayList".to_string(),
        "List$immutable" => match len() {
            1 | 2 => "java.util.ImmutableCollections$List12".to_string(),
            _ => "java.util.ImmutableCollections$ListN".to_string(),
        },
        "Set$immutable" => match len() {
            1 | 2 => "java.util.ImmutableCollections$Set12".to_string(),
            _ => "java.util.ImmutableCollections$SetN".to_string(),
        },
        // The JDK's map factory has a one-entry specialization and nothing
        // between it and the general one, so `Map.of()` — with no entries at
        // all — is a `MapN` too.
        "Map$immutable" => match len() {
            1 => "java.util.ImmutableCollections$Map1".to_string(),
            _ => "java.util.ImmutableCollections$MapN".to_string(),
        },
        // The two `Map` views and the entries they hand out. Each map
        // implementation has its own private class for all three, and the
        // immutable factory's are named after neither the map nor the abstract
        // classes: its `keySet` is an anonymous `AbstractMap$1` and its
        // `entrySet` is an ordinary immutable set at one entry and an anonymous
        // `MapN$1` otherwise. Every name here was read off
        // `getClass().getName()` under the reference JDK.
        "Set$keys$hash" => "java.util.HashMap$KeySet".to_string(),
        "Set$keys$linked" => "java.util.LinkedHashMap$LinkedKeySet".to_string(),
        "Set$keys$tree" => "java.util.TreeMap$KeySet".to_string(),
        "Set$keys$immutable" => "java.util.AbstractMap$1".to_string(),
        "Set$entries$hash" => "java.util.HashMap$EntrySet".to_string(),
        "Set$entries$linked" => "java.util.LinkedHashMap$LinkedEntrySet".to_string(),
        "Set$entries$tree" => "java.util.TreeMap$EntrySet".to_string(),
        "Set$entries$immutable" => match len() {
            1 => "java.util.ImmutableCollections$Set12".to_string(),
            _ => "java.util.ImmutableCollections$MapN$1".to_string(),
        },
        "List$values$hash" => "java.util.HashMap$Values".to_string(),
        "List$values$linked" => "java.util.LinkedHashMap$LinkedValues".to_string(),
        "List$values$tree" => "java.util.TreeMap$Values".to_string(),
        "List$values$immutable" => "java.util.AbstractMap$2".to_string(),
        "Entry$hash" => "java.util.HashMap$Node".to_string(),
        "Entry$linked" => "java.util.LinkedHashMap$Entry".to_string(),
        "Entry$tree" => "java.util.TreeMap$Entry".to_string(),
        "Entry$immutable" => "java.util.KeyValueHolder".to_string(),
        "Entry$simple" => "java.util.AbstractMap$SimpleEntry".to_string(),
        "Entry$simpleImmutable" => "java.util.AbstractMap$SimpleImmutableEntry".to_string(),
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
        // A map's size is its entry count. It is here because the JDK names its
        // immutable map classes by size the way it names the list and set ones,
        // and `binary_name` asks all three the same question.
        Some(HostObj::Map { entries, .. }) => Some(entries.len()),
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
    "PriorityQueue",
    "AbstractQueue",
    "Queue",
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
/// The binary name for a class named in SOURCE, with no instance to read it
/// off — what a `T.class` literal evaluates to.
///
/// Shares `jdk_name` with the runtime path, so a literal and
/// `x.getClass().getName()` on the same type cannot disagree. A primitive is
/// its own keyword: `int.class.getName()` is `int`, not `java.lang.Integer`.
pub fn qualify_class_name(n: &str) -> String {
    if matches!(
        n,
        "int" | "long" | "short" | "byte" | "char" | "float" | "double" | "boolean" | "void"
    ) {
        return n.to_string();
    }
    crate::prelude::qualified_throwable(n).unwrap_or_else(|| jdk_name(n))
}

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
        | "PriorityQueue"
        | "AbstractQueue"
        | "Queue"
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

/// `String.translateEscapes` (Java 15): the escape sequences of a string
/// literal, transcribed from the JDK's loop. `\s` is a space, an octal escape
/// takes up to three digits (two more after a leading `0`-`3`, one more
/// otherwise), a backslash before a line terminator removes both. Anything else —
/// including a trailing lone backslash, which the JDK reads as a NUL escape —
/// is its `IllegalArgumentException`.
fn translate_escapes(s: &str) -> Result<String, Fault> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut from = 0;
    while from < chars.len() {
        let mut ch = chars[from];
        from += 1;
        if ch == '\\' {
            ch = if from < chars.len() {
                chars[from]
            } else {
                '\0'
            };
            from += 1;
            ch = match ch {
                'b' => '\u{8}',
                'f' => '\u{c}',
                'n' => '\n',
                'r' => '\r',
                's' => ' ',
                't' => '\t',
                '\'' | '"' | '\\' => ch,
                '0'..='7' => {
                    let limit = (from + if ch <= '3' { 2 } else { 1 }).min(chars.len());
                    let mut code = ch as u32 - '0' as u32;
                    while from < limit && ('0'..='7').contains(&chars[from]) {
                        code = (code << 3) | (chars[from] as u32 - '0' as u32);
                        from += 1;
                    }
                    char::from_u32(code).unwrap_or('\0')
                }
                '\n' => continue,
                '\r' => {
                    if from < chars.len() && chars[from] == '\n' {
                        from += 1;
                    }
                    continue;
                }
                other => {
                    return Err(Fault::java(
                        "IllegalArgumentException",
                        format!(
                            "Invalid escape sequence: \\{other} \\\\u{:04X}",
                            other as u32
                        ),
                    ))
                }
            };
        }
        out.push(ch);
    }
    Ok(out)
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
            "Map$immutable",
            "Iterator$of",
            "ListIterator$of",
            "Stream$of",
            "Collector$of",
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
