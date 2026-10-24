//! Java's `String` model on fusevm's UTF-8 `Value::Str`.
//!
//! A Java string is a sequence of UTF-16 code units, and every index the
//! language exposes — `length`, `charAt`, `substring`, `indexOf`, a
//! `StringBuilder` offset, a `Matcher` position — counts those units. fusevm
//! holds text as a Rust `String`, and javars does not modify fusevm, so this
//! module is the bridge: the storage stays UTF-8 and the semantics are UTF-16.
//!
//! # Representation
//!
//! A **well-formed** Java string (every surrogate paired) is stored as exactly
//! the UTF-8 Rust would hold for it. ASCII, the rest of the BMP, and astral
//! characters need no translation, so `println`, `equals`, hashing a map key and
//! every library routine that reads text see ordinary Rust text.
//!
//! Java can also hold an **unpaired surrogate** — `"😀".substring(0, 1)`,
//! `String.valueOf(highSurrogate)` — which UTF-8 cannot encode. Such a unit
//! `u` is stored as the *stand-in* scalar `U+10F800 + (u - 0xD800)`, the top
//! 2048 code points of Supplementary Private Use Area-B. Each stand-in is one
//! UTF-16 unit when measured, reads back as `u` from `charAt`, and is written as
//! `?` when the text leaves the program (`println`, `getBytes`), which is what
//! the JDK's UTF-8 encoder substitutes for an unpaired surrogate.
//!
//! **Canonical form:** a high stand-in is never immediately followed by a low
//! stand-in — Java would see a surrogate *pair* there, and the pair is stored
//! as its real character. [`push_unit`], [`push_str`] and [`canon`] restore the
//! form wherever text is joined, so two equal Java strings are always equal
//! Rust strings and `equals`/`hashCode`/map lookups need no special case.
//!
//! The price is that the genuine characters `U+10F800..=U+10FFFF` cannot be
//! held: every entry point that would create one ([`checked_code_point`], the
//! lexer) refuses it with a javars error rather than reading it as a surrogate.
//!
//! # Indexing cost
//!
//! An ASCII string's unit index is its byte index. Any other string needs a
//! walk, and the ordinary `for (i = 0; i < s.length(); i++) s.charAt(i)` loop
//! would pay that walk per call. [`Index`] is the per-string answer — the unit
//! length, and for non-ASCII text the byte offset of every unit — and a small
//! thread-local cache keeps the indexes of the strings most recently indexed,
//! keyed by the `Arc` that owns the text (see [`adopt`]).

use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

/// The stand-in for the unpaired surrogate `0xD800`; `0xDFFF` maps to
/// `STAND_IN_BASE + 0x7FF` = `U+10FFFF`.
const STAND_IN_BASE: u32 = 0x10_F800;

/// Whether `cp` is a code point javars reserves for stand-ins and so cannot
/// hold as a character.
pub fn is_reserved(cp: u32) -> bool {
    (STAND_IN_BASE..=0x10_FFFF).contains(&cp)
}

/// Whether `u` is a UTF-16 surrogate unit.
pub fn is_surrogate(u: u32) -> bool {
    (0xD800..=0xDFFF).contains(&u)
}

/// The stored character for the unpaired surrogate `u`.
fn stand_in(u: u16) -> char {
    char::from_u32(STAND_IN_BASE + u32::from(u - 0xD800)).expect("stand-in range is scalar")
}

/// The surrogate a stored character stands for, if it is a stand-in.
pub fn unpaired(c: char) -> Option<u16> {
    is_reserved(c as u32).then(|| (c as u32 - STAND_IN_BASE + 0xD800) as u16)
}

/// The value Java's `char` holds for a stored one-unit character: the
/// surrogate for a stand-in, the code point otherwise.
pub fn char_value(c: char) -> u32 {
    unpaired(c).map_or(c as u32, u32::from)
}

/// The stored character for one Java `char` value (or a code point above the
/// BMP). A surrogate becomes its stand-in.
pub fn char_of(unit_or_cp: u32) -> Option<char> {
    if is_surrogate(unit_or_cp) {
        Some(stand_in(unit_or_cp as u16))
    } else {
        char::from_u32(unit_or_cp)
    }
}

/// A code point a program supplies (`Character.toChars`, `appendCodePoint`,
/// `Character.toString(int)`) as the text it denotes: one or two units.
/// `Err` carries the refusal for a code point javars reserves; `Ok(None)` is a
/// value that is no code point at all, which the caller reports in the JDK's
/// own words.
pub fn checked_code_point(cp: i64) -> Result<Option<char>, String> {
    match u32::try_from(cp) {
        Ok(c) if is_reserved(c) => Err(reserved_message(c)),
        Ok(c) => Ok(char_of(c)),
        Err(_) => Ok(None),
    }
}

/// The refusal for a reserved code point.
pub fn reserved_message(cp: u32) -> String {
    format!(
        "javars: code point U+{cp:X} is reserved: javars stores an unpaired surrogate there, so it cannot hold the character itself"
    )
}

/// The UTF-16 units of one stored character.
fn units_of(c: char) -> (u16, Option<u16>) {
    if let Some(u) = unpaired(c) {
        return (u, None);
    }
    let mut buf = [0u16; 2];
    let enc = c.encode_utf16(&mut buf);
    (enc[0], enc.get(1).copied())
}

/// The UTF-16 width of one stored character.
fn width(c: char) -> usize {
    if c as u32 > 0xFFFF && unpaired(c).is_none() {
        2
    } else {
        1
    }
}

/// The Java `char`s of `s`, in order.
pub fn units(s: &str) -> impl Iterator<Item = u16> + '_ {
    s.chars().flat_map(|c| {
        let (a, b) = units_of(c);
        std::iter::once(a).chain(b)
    })
}

/// Whether `s` holds a stand-in. The UTF-8 of `U+10F800..` is `F4 8F A0..BF
/// xx`, so a scan for the `F4` lead byte rules almost every string out.
pub fn has_stand_in(s: &str) -> bool {
    let b = s.as_bytes();
    b.iter().enumerate().any(|(i, &x)| {
        x == 0xF4 && b.get(i + 1) == Some(&0x8F) && b.get(i + 2).is_some_and(|y| *y >= 0xA0)
    })
}

/// `s.length()` — the UTF-16 unit count.
///
/// Every character contributes one unit per UTF-8 lead byte, plus one more for
/// an astral character (a `F0..F4` lead); a stand-in is astral in UTF-8 but one
/// unit, so it is taken back out. The first sum vectorizes.
pub fn len(s: &str) -> usize {
    let b = s.as_bytes();
    let n: usize = b
        .iter()
        .map(|&x| usize::from((x & 0xC0) != 0x80) + usize::from(x >= 0xF0))
        .sum();
    if b.contains(&0xF4) {
        n - s.chars().filter(|c| unpaired(*c).is_some()).count()
    } else {
        n
    }
}

/// Text built from Java `char`s, pairs joined into their character and any
/// other surrogate kept as its stand-in.
pub fn from_units(units: impl IntoIterator<Item = u16>) -> String {
    let mut out = String::new();
    for u in units {
        push_unit(&mut out, u);
    }
    out
}

/// Append one Java `char` to `out`, completing a pair with a trailing high
/// stand-in when `u` is its low half.
pub fn push_unit(out: &mut String, u: u16) {
    if (0xDC00..=0xDFFF).contains(&u) {
        if let Some(hi) = out.chars().next_back().and_then(unpaired) {
            if (0xD800..=0xDBFF).contains(&hi) {
                out.pop();
                out.push(char::from_u32(pair(hi, u)).expect("a surrogate pair is a scalar"));
                return;
            }
        }
    }
    out.push(char_of(u32::from(u)).expect("a UTF-16 unit is storable"));
}

/// Append `s` to `out`, joining a high stand-in at the seam with a low one.
pub fn push_str(out: &mut String, s: &str) {
    let mut rest = s;
    if let Some(first) = s.chars().next() {
        if let Some(lo) = unpaired(first) {
            push_unit(out, lo);
            rest = &s[first.len_utf8()..];
        }
    }
    out.push_str(rest);
}

/// `a + b` with the seam repaired — Java's concatenation.
pub fn concat(a: &str, b: &str) -> String {
    let mut out = String::with_capacity(a.len() + b.len());
    out.push_str(a);
    push_str(&mut out, b);
    out
}

/// `s` in canonical form: any high stand-in followed by a low one joined into
/// its character. The identity, without a copy, on text holding no stand-in.
pub fn canon(s: String) -> String {
    if !has_stand_in(&s) {
        return s;
    }
    from_units(units(&s))
}

/// The text as it leaves the program: each stand-in written as `?`, which is
/// what the JDK's UTF-8 encoder substitutes for an unpaired surrogate.
pub fn encoded(s: &str) -> Cow<'_, str> {
    if !has_stand_in(s) {
        return Cow::Borrowed(s);
    }
    Cow::Owned(
        s.chars()
            .map(|c| if unpaired(c).is_some() { '?' } else { c })
            .collect(),
    )
}

/// `String.compareTo`: the difference of the first differing `char`s, else of
/// the lengths, over the UTF-16 units.
pub fn compare(a: &str, b: &str) -> i64 {
    let mut x = units(a);
    let mut y = units(b);
    loop {
        match (x.next(), y.next()) {
            (Some(p), Some(q)) if p != q => return i64::from(p) - i64::from(q),
            (Some(_), Some(_)) => {}
            _ => return len(a) as i64 - len(b) as i64,
        }
    }
}

/// [`compare`] as an ordering — `String`'s natural order for sorting.
pub fn order(a: &str, b: &str) -> std::cmp::Ordering {
    let wide = |s: &str| s.as_bytes().iter().any(|&x| x >= 0xEE);
    if !wide(a) && !wide(b) {
        return a.cmp(b);
    }
    units(a).cmp(units(b))
}

/// `String.hashCode`: `s[0]*31^(n-1) + … + s[n-1]` over the UTF-16 units.
pub fn hash(s: &str) -> i32 {
    units(s).fold(0i32, |h, u| h.wrapping_mul(31).wrapping_add(i32::from(u)))
}

/// Whether `hi, lo` is a surrogate pair.
pub fn is_pair(hi: u16, lo: u16) -> bool {
    (0xD800..=0xDBFF).contains(&hi) && (0xDC00..=0xDFFF).contains(&lo)
}

/// The code point the surrogate pair `hi, lo` encodes.
pub fn pair(hi: u16, lo: u16) -> u32 {
    0x10000 + ((u32::from(hi) - 0xD800) << 10) + (u32::from(lo) - 0xDC00)
}

/// The code point at unit `i` of `units` (`Character.codePointAt(char[], int,
/// int)`'s rule): a high surrogate followed by a low one before `limit` reads
/// as the pair.
pub fn code_point_at(units: &[u16], i: usize, limit: usize) -> u32 {
    match units.get(i + 1) {
        Some(&lo) if i + 1 < limit && is_pair(units[i], lo) => pair(units[i], lo),
        _ => u32::from(units[i]),
    }
}

/// The code point that ends just before unit `i` (`codePointBefore`): a low
/// surrogate preceded by a high one at or after `start` reads as the pair.
pub fn code_point_before(units: &[u16], i: usize, start: usize) -> u32 {
    let lo = units[i - 1];
    if i >= start + 2 && is_pair(units[i - 2], lo) {
        return pair(units[i - 2], lo);
    }
    u32::from(lo)
}

/// The number of code points in `units` — a pair is one, an unpaired
/// surrogate one too.
pub fn code_point_count(units: &[u16]) -> usize {
    char::decode_utf16(units.iter().copied()).count()
}

/// `offsetByCodePoints`: the unit index `offset` code points away from
/// `index`, or `None` when the walk leaves the text. A pair counts once in
/// either direction; an unpaired surrogate counts once.
pub fn offset_by_code_points(units: &[u16], index: usize, offset: i64) -> Option<usize> {
    let is_hi = |u: u16| (0xD800..=0xDBFF).contains(&u);
    let is_lo = |u: u16| (0xDC00..=0xDFFF).contains(&u);
    let mut x = index;
    if offset >= 0 {
        for _ in 0..offset {
            if x >= units.len() {
                return None;
            }
            x += 1;
            if is_hi(units[x - 1]) && x < units.len() && is_lo(units[x]) {
                x += 1;
            }
        }
    } else {
        for _ in 0..offset.unsigned_abs() {
            if x == 0 {
                return None;
            }
            x -= 1;
            if is_lo(units[x]) && x > 0 && is_hi(units[x - 1]) {
                x -= 1;
            }
        }
    }
    Some(x)
}

/// `AbstractStringBuilder.reverse`: the units reversed, then every pair the
/// reversal turned around (`low, high`) swapped back, so a pair survives as
/// the character it was.
pub fn reverse_units(units: &mut [u16]) {
    units.reverse();
    let mut i = 0;
    while i + 1 < units.len() {
        if (0xDC00..=0xDFFF).contains(&units[i]) && (0xD800..=0xDBFF).contains(&units[i + 1]) {
            units.swap(i, i + 1);
            i += 2;
        } else {
            i += 1;
        }
    }
}

/// Where a unit index falls in the UTF-8 text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum At {
    /// The index starts the character at this byte offset (or is the end).
    Byte(usize),
    /// The index is the low half of the astral character at this byte offset.
    Inside(usize),
}

/// The UTF-16 view of one string: its unit length, and for non-ASCII text the
/// byte offset of the character holding each unit.
#[derive(Debug)]
pub struct Index {
    len: usize,
    /// Empty for ASCII text, where a unit index is a byte index. Otherwise one
    /// entry per unit; the two units of an astral character share the
    /// character's offset.
    starts: Vec<u32>,
}

impl Index {
    fn build(s: &str) -> Index {
        if s.is_ascii() {
            return Index {
                len: s.len(),
                starts: Vec::new(),
            };
        }
        let mut starts = Vec::with_capacity(s.len());
        for (b, c) in s.char_indices() {
            for _ in 0..width(c) {
                starts.push(b as u32);
            }
        }
        Index {
            len: starts.len(),
            starts,
        }
    }

    /// The unit length.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the text is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether the text is all ASCII.
    pub fn is_ascii(&self) -> bool {
        self.starts.is_empty()
    }

    /// Where unit `i` (`0..=len`) falls.
    pub fn at(&self, s: &str, i: usize) -> At {
        if self.is_ascii() || i >= self.len {
            return At::Byte(if self.is_ascii() {
                i.min(s.len())
            } else {
                s.len()
            });
        }
        let b = self.starts[i] as usize;
        if i > 0 && self.starts[i - 1] as usize == b {
            At::Inside(b)
        } else {
            At::Byte(b)
        }
    }

    /// The `char` at unit `i < len`.
    pub fn unit(&self, s: &str, i: usize) -> u16 {
        match self.at(s, i) {
            At::Byte(b) if self.is_ascii() => u16::from(s.as_bytes()[b]),
            At::Byte(b) => units_of(s[b..].chars().next().expect("in range")).0,
            At::Inside(b) => units_of(s[b..].chars().next().expect("in range"))
                .1
                .expect("astral"),
        }
    }

    /// The unit index of byte offset `b`, a character boundary of `s`.
    pub fn unit_of_byte(&self, s: &str, b: usize) -> usize {
        if self.is_ascii() {
            return b;
        }
        if b >= s.len() {
            return self.len;
        }
        // `starts` is sorted, and the first unit of a character is the first
        // entry holding its offset.
        self.starts.partition_point(|&x| (x as usize) < b)
    }

    /// The units `[b, e)` of `s` as text — `substring`'s answer. A boundary
    /// inside a pair keeps the half on its side as a stand-in.
    pub fn slice(&self, s: &str, b: usize, e: usize) -> String {
        if b >= e {
            return String::new();
        }
        let (bb, head) = match self.at(s, b) {
            At::Byte(x) => (x, None),
            At::Inside(x) => {
                let c = s[x..].chars().next().expect("in range");
                (x + c.len_utf8(), units_of(c).1)
            }
        };
        let (eb, tail) = match self.at(s, e) {
            At::Byte(x) => (x, None),
            At::Inside(x) => (
                x,
                Some(units_of(s[x..].chars().next().expect("in range")).0),
            ),
        };
        // `b` and `e` inside the same character: the one half between them.
        if bb > eb {
            return stand_in(head.or(tail).expect("a split pair")).to_string();
        }
        let mut out = String::with_capacity(eb - bb + 8);
        if let Some(h) = head {
            out.push(stand_in(h));
        }
        out.push_str(&s[bb..eb]);
        if let Some(t) = tail {
            out.push(stand_in(t));
        }
        out
    }
}

/// `s.indexOf(needle, from)` in units. `from` is clamped to `[0, length]`
/// first, so an empty needle answers that clamped start.
///
/// A needle holding no stand-in is searched bytewise: its units form whole
/// characters, so a unit-level match can only begin on a character boundary of
/// `s`. One that does hold a stand-in — an unpaired half, which may match half
/// of a stored pair — is searched unit by unit.
pub fn index_of(s: &str, ix: &Index, needle: &str, from: i64) -> i64 {
    let from = from.clamp(0, ix.len() as i64) as usize;
    if needle.is_empty() {
        return from as i64;
    }
    if has_stand_in(needle) {
        let (h, n): (Vec<u16>, Vec<u16>) = (units(s).collect(), units(needle).collect());
        return (from..h.len())
            .find(|&i| h[i..].starts_with(&n))
            .map_or(-1, |i| i as i64);
    }
    let start = match ix.at(s, from) {
        At::Byte(b) => b,
        At::Inside(b) => b + s[b..].chars().next().map_or(0, char::len_utf8),
    };
    s[start..]
        .find(needle)
        .map_or(-1, |p| ix.unit_of_byte(s, start + p) as i64)
}

/// `s.lastIndexOf(needle, from)` in units. The clamping order is the JDK's
/// (`StringLatin1.lastIndexOf`) and it matters: `from` is first pulled *down*
/// to the last position a needle of this length could start at, and only then
/// rejected for being negative — so `"abc".lastIndexOf("", -1)` is -1.
pub fn last_index_of(s: &str, ix: &Index, needle: &str, from: i64) -> i64 {
    let n_len = len(needle);
    let start = from.min(ix.len() as i64 - n_len as i64);
    if start < 0 {
        return -1;
    }
    if needle.is_empty() {
        return start;
    }
    let start = start as usize;
    if ix.is_ascii() && needle.is_ascii() {
        return s[..start + n_len].rfind(needle).map_or(-1, |p| p as i64);
    }
    let (h, n): (Vec<u16>, Vec<u16>) = (units(s).collect(), units(needle).collect());
    (0..=start)
        .rev()
        .find(|&i| h[i..].starts_with(&n))
        .map_or(-1, |i| i as i64)
}

/// Strings shorter than this are indexed afresh each time: the walk is
/// cheaper than keeping them in the cache.
const CACHE_MIN_BYTES: usize = 64;

/// How many recently indexed strings keep their [`Index`].
const CACHE_SLOTS: usize = 4;

thread_local! {
    /// Most recent first. Holding the `Arc` keeps the text alive, so no other
    /// string can occupy its address while the entry exists, and fusevm's
    /// copy-on-write never mutates a string that has a second owner — which is
    /// what makes `(pointer, length)` a sound key.
    static CACHE: RefCell<Vec<(Arc<String>, Rc<Index>)>> = const { RefCell::new(Vec::new()) };
    /// The string the current method call reads, offered by [`adopt`] so a
    /// cache miss on it can be kept.
    static ADOPTED: RefCell<Option<Arc<String>>> = const { RefCell::new(None) };
}

/// Offer `owner` for caching: an [`index`] of its text taken before the next
/// `adopt` is kept. Called with the receiver of a `String` method call.
pub fn adopt(owner: &Arc<String>) {
    if owner.len() >= CACHE_MIN_BYTES {
        ADOPTED.with(|a| *a.borrow_mut() = Some(Arc::clone(owner)));
    }
}

/// The UTF-16 [`Index`] of `s`, from the cache when `s` is a string indexed
/// recently.
pub fn index(s: &str) -> Rc<Index> {
    if s.len() < CACHE_MIN_BYTES {
        return Rc::new(Index::build(s));
    }
    let key = (s.as_ptr(), s.len());
    let hit = CACHE.with(|c| {
        let mut c = c.borrow_mut();
        let pos = c.iter().position(|(a, _)| (a.as_ptr(), a.len()) == key)?;
        let entry = c.remove(pos);
        let idx = Rc::clone(&entry.1);
        c.insert(0, entry);
        Some(idx)
    });
    if let Some(idx) = hit {
        return idx;
    }
    let idx = Rc::new(Index::build(s));
    let owner = ADOPTED.with(|a| {
        a.borrow()
            .as_ref()
            .filter(|o| (o.as_ptr(), o.len()) == key)
            .cloned()
    });
    if let Some(owner) = owner {
        CACHE.with(|c| {
            let mut c = c.borrow_mut();
            c.insert(0, (owner, Rc::clone(&idx)));
            c.truncate(CACHE_SLOTS);
        });
    }
    idx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grin() -> String {
        char::from_u32(0x1F600).unwrap().to_string()
    }

    #[test]
    fn length_counts_units() {
        let s = format!("a{}b", grin());
        assert_eq!(len(&s), 4);
        assert_eq!(index(&s).len(), 4);
        let hi = from_units([0xD83D]);
        assert_eq!(len(&hi), 1);
    }

    #[test]
    fn halves_rejoin_into_the_character() {
        let hi = from_units([0xD83D]);
        let lo = from_units([0xDE00]);
        assert_eq!(concat(&hi, &lo), grin());
        assert_eq!(canon(format!("{hi}{lo}")), grin());
        assert_eq!(concat(&lo, &hi).chars().count(), 2);
    }

    #[test]
    fn slicing_a_pair_keeps_each_half() {
        let s = format!("x{}y", grin());
        let ix = index(&s);
        assert_eq!(units(&ix.slice(&s, 1, 2)).collect::<Vec<_>>(), [0xD83D]);
        assert_eq!(
            units(&ix.slice(&s, 2, 4)).collect::<Vec<_>>(),
            [0xDE00, b'y' as u16]
        );
        assert_eq!(ix.slice(&s, 1, 3), grin());
        assert_eq!(ix.unit(&s, 2), 0xDE00);
    }

    #[test]
    fn order_is_utf16_order() {
        // U+FFFD sorts after a supplementary character's high surrogate.
        let a = grin();
        let b = "\u{FFFD}".to_string();
        assert_eq!(order(&a, &b), std::cmp::Ordering::Less);
        assert!(compare(&a, &b) < 0);
    }

    #[test]
    fn output_replaces_unpaired_halves() {
        let s = format!("a{}b", from_units([0xDE00]));
        assert_eq!(encoded(&s), "a?b");
    }
}
