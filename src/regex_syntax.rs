//! `java.util.regex.Pattern`'s syntax check, ported from its recursive-descent
//! parser (`Pattern.expr`/`sequence`/`atom`/`closure`/`group0`/`clazz`/
//! `range`/`escape` and the escape helpers in OpenJDK 21's `Pattern.java`).
//!
//! javars matches with `fancy-regex` (`src/regex.rs`), whose own parse error
//! names neither the JDK's description nor its index. This pass walks the
//! pattern exactly as the JDK does — the same cursor over the same
//! code-point array, after the same `\Q…\E` rewrite — building only the nodes study needs,
//! and stops at the first error with `PatternSyntaxException`'s description
//! and its index (`cursor - 1`, as `Pattern.error` reports it). It keeps of
//! each node only what `Node.study` reads, which is how the JDK decides that a
//! look-behind "does not have an obvious maximum length".
//!
//! Whether a `\p{…}` property name exists is decided from the tables
//! `scripts/GenRegexNames.java` generates off the JDK (`src/regex_names.rs`),
//! and whether a `\N{…}` character name does from `Character.codePointOf`'s
//! own data (`src/uninames.rs`).

/// `Pattern.UNIX_LINES`.
pub const UNIX_LINES: u32 = 0x01;
/// `Pattern.CASE_INSENSITIVE`.
pub const CASE_INSENSITIVE: u32 = 0x02;
/// `Pattern.COMMENTS`.
pub const COMMENTS: u32 = 0x04;
/// `Pattern.MULTILINE`.
pub const MULTILINE: u32 = 0x08;
/// `Pattern.LITERAL`.
pub const LITERAL: u32 = 0x10;
/// `Pattern.DOTALL`.
pub const DOTALL: u32 = 0x20;
/// `Pattern.UNICODE_CASE`.
pub const UNICODE_CASE: u32 = 0x40;
/// `Pattern.CANON_EQ`.
pub const CANON_EQ: u32 = 0x80;
/// `Pattern.UNICODE_CHARACTER_CLASS`.
pub const UNICODE_CHARACTER_CLASS: u32 = 0x100;

/// A pattern the JDK refuses: `PatternSyntaxException(desc, pattern, index)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxError {
    pub desc: String,
    pub index: i64,
}

/// The `PatternSyntaxException.getMessage()` text for `err` on `pattern`:
/// the description, ` near index N`, the pattern on its own line, and a
/// caret under the index when the index is inside the pattern.
pub fn message(err: &SyntaxError, pattern: &str) -> String {
    let mut out = err.desc.clone();
    if err.index >= 0 {
        out.push_str(&format!(" near index {}", err.index));
    }
    out.push('\n');
    out.push_str(pattern);
    let units: Vec<u16> = crate::jstr::units(pattern).collect();
    if err.index >= 0 && (err.index as usize) < units.len() {
        out.push('\n');
        for &u in &units[..err.index as usize] {
            out.push(if u == u16::from(b'\t') { '\t' } else { ' ' });
        }
        out.push('^');
    }
    out
}

/// Check `pattern` under `flags` as `Pattern.compile` does.
pub fn check(pattern: &str, flags: u32) -> Result<(), SyntaxError> {
    if pattern.is_empty() || flags & LITERAL != 0 {
        return Ok(());
    }
    let mut temp: Vec<u32> = pattern.chars().map(crate::jstr::char_value).collect();
    let len = remove_qe_quoting(&mut temp);
    temp.truncate(len);
    temp.extend([0, 0]);
    let mut p = Parser {
        temp,
        len,
        cursor: 0,
        // `UNICODE_CHARACTER_CLASS` implies `UNICODE_CASE`, as the
        // `Pattern` constructor makes it.
        flags: if flags & UNICODE_CHARACTER_CLASS != 0 {
            flags | UNICODE_CASE
        } else {
            flags
        },
        groups: 1,
        names: Vec::new(),
        last_lazy: false,
        last_possessive: false,
    };
    p.expr()?;
    if p.cursor != p.len {
        if p.peek() == u32::from(')') {
            return Err(p.error("Unmatched closing ')'"));
        } else if p.cursor == p.len + 1 && p.temp[p.len - 1] == u32::from('\\') {
            return Err(p.error("Unescaped trailing backslash"));
        }
        return Err(p.error("Unexpected internal error"));
    }
    Ok(())
}

/// `Pattern.RemoveQEQuoting`: rewrite each `\Q…\E` into escaped literals,
/// in place, answering the new length.
fn remove_qe_quoting(temp: &mut Vec<u32>) -> usize {
    let p_len = temp.len();
    let mut i = 0;
    while i + 1 < p_len {
        if temp[i] != u32::from('\\') {
            i += 1;
        } else if temp[i + 1] != u32::from('Q') {
            i += 2;
        } else {
            break;
        }
    }
    if i + 1 >= p_len {
        return p_len;
    }
    let mut out: Vec<u32> = temp[..i].to_vec();
    i += 2;
    let (mut in_quote, mut begin_quote) = (true, true);
    let at = |t: &[u32], k: usize| t.get(k).copied().unwrap_or(0);
    while i < p_len {
        let c = temp[i];
        i += 1;
        let ascii = c < 0x80;
        if !ascii || is_alpha(c) {
            out.push(c);
        } else if is_digit(c) {
            if begin_quote {
                out.extend(['\\', 'x', '3'].map(u32::from));
            }
            out.push(c);
        } else if c != u32::from('\\') {
            if in_quote {
                out.push(u32::from('\\'));
            }
            out.push(c);
        } else if in_quote {
            if at(temp, i) == u32::from('E') {
                i += 1;
                in_quote = false;
            } else {
                out.extend(['\\', '\\'].map(u32::from));
            }
        } else if at(temp, i) == u32::from('Q') {
            i += 1;
            in_quote = true;
            begin_quote = true;
            continue;
        } else {
            out.push(c);
            if i != p_len {
                out.push(temp[i]);
                i += 1;
            }
        }
        begin_quote = false;
    }
    let n = out.len();
    *temp = out;
    n
}

fn is_alpha(c: u32) -> bool {
    char::from_u32(c).is_some_and(|c| c.is_ascii_alphabetic())
}
fn is_digit(c: u32) -> bool {
    (u32::from('0')..=u32::from('9')).contains(&c)
}
fn is_alnum(c: u32) -> bool {
    is_alpha(c) || is_digit(c)
}
fn is_hex(c: u32) -> bool {
    char::from_u32(c).is_some_and(|c| c.is_ascii_hexdigit())
}
/// `ASCII.isSpace`: space, `\t`, `\n`, `\u000B`, `\f`, `\r`.
fn is_space(c: u32) -> bool {
    matches!(c, 0x20 | 0x09..=0x0D)
}

/// `Pattern.MAX_REPS`.
const MAX_REPS: i32 = 0x7FFF_FFFF;

/// The node shapes `Pattern.compile` builds, as far as `Node.study` — which
/// decides a look-behind's "obvious maximum length" — tells them apart.
#[derive(Debug, Clone, PartialEq)]
enum Node {
    /// A `CharProperty`: one character, a class, `.`, or a class escape. A
    /// greedy `*`/`+`/`{n,}` on one is a [`Node::Greedy`].
    Prop,
    /// A `Slice` (`min == max`, the literal run's length) or `\R`'s
    /// `LineEnding` (1 to 2 characters).
    Fixed(i32, i32),
    /// An assertion that matches no characters: an anchor, a boundary, a
    /// look-around.
    Zero,
    /// `\X`: at least one character, no maximum it reports.
    NonDet(i32),
    /// A numbered or named back-reference.
    BackRef,
    /// `CharPropertyGreedy` with its minimum.
    Greedy(i32),
    /// `Curly` (or a deterministic group's `GroupCurly`) over its atom.
    Curly(Seq, i32, i32),
    /// `Ques` over its atom; `true` for an atomic `(?>…)` group.
    Ques(Seq, bool),
    /// `Branch`: its alternatives, `None` for an empty one.
    Branch(Vec<Option<Seq>>),
    /// A non-deterministic group under a quantifier (`Loop`/`LazyLoop`).
    Loop,
    /// A group not yet given its closure (only while parsing one).
    Group(Seq),
}

type Seq = Vec<Node>;

/// `Pattern.TreeInfo`, with Java's `int` arithmetic.
#[derive(Debug, Clone, Copy)]
struct Info {
    min: i32,
    max: i32,
    max_valid: bool,
    deterministic: bool,
}

impl Info {
    fn new() -> Info {
        Info {
            min: 0,
            max: 0,
            max_valid: true,
            deterministic: true,
        }
    }
}

/// `Node.study` over `seq` followed by the end of the enclosing atom: each
/// node's own `study`, ported, answering whether the whole is deterministic.
fn study(seq: &[Node], info: &mut Info) -> bool {
    let Some((node, rest)) = seq.split_first() else {
        return info.deterministic;
    };
    match node {
        Node::Prop => {
            info.min = info.min.wrapping_add(1);
            info.max = info.max.wrapping_add(1);
        }
        Node::Fixed(a, b) => {
            info.min = info.min.wrapping_add(*a);
            info.max = info.max.wrapping_add(*b);
        }
        Node::Zero => {}
        Node::NonDet(a) => {
            info.min = info.min.wrapping_add(*a);
            info.deterministic = false;
        }
        Node::BackRef => info.max_valid = false,
        Node::Greedy(cmin) => {
            info.min = info.min.wrapping_add(*cmin);
            if info.max_valid {
                info.max = info.max.wrapping_add(MAX_REPS);
            }
            info.deterministic = false;
        }
        Node::Curly(atom, cmin, cmax) => {
            let (min_l, max_l, max_v, detm) =
                (info.min, info.max, info.max_valid, info.deterministic);
            *info = Info::new();
            study(atom, info);
            let mut temp = info.min.wrapping_mul(*cmin).wrapping_add(min_l);
            if temp < min_l {
                temp = 0xFFF_FFFF;
            }
            info.min = temp;
            if max_v & info.max_valid {
                temp = info.max.wrapping_mul(*cmax).wrapping_add(max_l);
                info.max = temp;
                if temp < max_l {
                    info.max_valid = false;
                }
            } else {
                info.max_valid = false;
            }
            info.deterministic = info.deterministic && cmin == cmax && detm;
        }
        Node::Ques(atom, independent) => {
            if *independent {
                study(atom, info);
            } else {
                let min_l = info.min;
                study(atom, info);
                info.min = min_l;
                info.deterministic = false;
            }
        }
        Node::Branch(alts) => {
            let (mut min_l, mut max_l, mut max_v) = (info.min, info.max, info.max_valid);
            let (mut min_l2, mut max_l2) = (i32::MAX, -1i32);
            for alt in alts {
                *info = Info::new();
                if let Some(a) = alt {
                    study(a, info);
                }
                min_l2 = min_l2.min(info.min);
                max_l2 = max_l2.max(info.max);
                max_v &= info.max_valid;
            }
            min_l = min_l.wrapping_add(min_l2);
            max_l = max_l.wrapping_add(max_l2);
            *info = Info::new();
            study(rest, info);
            info.min = info.min.wrapping_add(min_l);
            info.max = info.max.wrapping_add(max_l);
            info.max_valid &= max_v;
            info.deterministic = false;
            return false;
        }
        Node::Loop => {
            info.max_valid = false;
            info.deterministic = false;
            return false;
        }
        Node::Group(body) => {
            let mut whole = body.clone();
            whole.extend_from_slice(rest);
            return study(&whole, info);
        }
    }
    study(rest, info)
}

/// An escape's reading: the one character it is, or the node it builds.
enum Esc {
    Char(u32),
    Node(Node),
}

impl Esc {
    fn char(&self) -> Option<u32> {
        match self {
            Esc::Char(c) => Some(*c),
            Esc::Node(_) => None,
        }
    }
}

/// The three group shapes a closure treats differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    Plain,
    Independent,
    Zero,
}

struct Parser {
    /// The pattern's code points, `\Q…\E` rewritten, with two `0`s after it.
    temp: Vec<u32>,
    /// The pattern's length in `temp`, without the terminators.
    len: usize,
    cursor: usize,
    /// The flags in force, which inline `(?x)` groups change.
    flags: u32,
    /// `capturingGroupCount`: one more than the groups opened so far.
    groups: usize,
    /// The named groups opened so far.
    names: Vec<String>,
    /// Whether the last quantifier read was lazy (`*?`) or possessive (`*+`).
    last_lazy: bool,
    last_possessive: bool,
}

const fn ch(c: char) -> u32 {
    c as u32
}

impl Parser {
    fn has(&self, f: u32) -> bool {
        self.flags & f != 0
    }

    fn at(&self, i: usize) -> u32 {
        self.temp.get(i).copied().unwrap_or(0)
    }

    fn error(&self, desc: &str) -> SyntaxError {
        SyntaxError {
            desc: desc.to_string(),
            index: self.cursor as i64 - 1,
        }
    }

    fn peek(&mut self) -> u32 {
        let c = self.at(self.cursor);
        if self.has(COMMENTS) {
            self.peek_past_whitespace(c)
        } else {
            c
        }
    }

    fn read(&mut self) -> u32 {
        let c = self.at(self.cursor);
        self.cursor += 1;
        if self.has(COMMENTS) {
            self.parse_past_whitespace(c)
        } else {
            c
        }
    }

    fn next(&mut self) -> u32 {
        self.cursor += 1;
        let c = self.at(self.cursor);
        if self.has(COMMENTS) {
            self.peek_past_whitespace(c)
        } else {
            c
        }
    }

    fn next_escaped(&mut self) -> u32 {
        self.cursor += 1;
        self.at(self.cursor)
    }

    fn skip(&mut self) -> u32 {
        let c = self.at(self.cursor + 1);
        self.cursor += 2;
        c
    }

    fn unread(&mut self) {
        self.cursor -= 1;
    }

    fn accept(&mut self, want: u32, desc: &str) -> Result<(), SyntaxError> {
        let mut c = self.at(self.cursor);
        self.cursor += 1;
        if self.has(COMMENTS) {
            c = self.parse_past_whitespace(c);
        }
        if c != want {
            return Err(self.error(desc));
        }
        Ok(())
    }

    fn is_line_separator(&self, c: u32) -> bool {
        if self.has(UNIX_LINES) {
            c == ch('\n')
        } else {
            c == ch('\n') || c == ch('\r') || (c | 1) == 0x2029 || c == 0x85
        }
    }

    fn peek_past_whitespace(&mut self, mut c: u32) -> u32 {
        while is_space(c) || c == ch('#') {
            while is_space(c) {
                self.cursor += 1;
                c = self.at(self.cursor);
            }
            if c == ch('#') {
                c = self.peek_past_line();
            }
        }
        c
    }

    fn parse_past_whitespace(&mut self, mut c: u32) -> u32 {
        while is_space(c) || c == ch('#') {
            while is_space(c) {
                c = self.at(self.cursor);
                self.cursor += 1;
            }
            if c == ch('#') {
                c = self.parse_past_line();
            }
        }
        c
    }

    fn parse_past_line(&mut self) -> u32 {
        let mut c = self.at(self.cursor);
        self.cursor += 1;
        while c != 0 && !self.is_line_separator(c) {
            c = self.at(self.cursor);
            self.cursor += 1;
        }
        if c == 0 && self.cursor > self.len {
            self.cursor = self.len;
            c = self.at(self.cursor);
            self.cursor += 1;
        }
        c
    }

    fn peek_past_line(&mut self) -> u32 {
        self.cursor += 1;
        let mut c = self.at(self.cursor);
        while c != 0 && !self.is_line_separator(c) {
            self.cursor += 1;
            c = self.at(self.cursor);
        }
        if c == 0 && self.cursor > self.len {
            self.cursor = self.len;
            c = self.at(self.cursor);
        }
        c
    }

    /// `Pattern.expr`: sequences separated by `|`, a `Branch` when there is
    /// more than one (an empty alternative is the branch's `null` atom).
    fn expr(&mut self) -> Result<Seq, SyntaxError> {
        let mut alts: Vec<Option<Seq>> = Vec::new();
        loop {
            let seq = self.sequence()?;
            alts.push((!seq.is_empty()).then_some(seq));
            if self.peek() != ch('|') {
                break;
            }
            self.next();
        }
        Ok(if alts.len() == 1 {
            alts.pop().flatten().unwrap_or_default()
        } else {
            vec![Node::Branch(alts)]
        })
    }

    /// `Pattern.sequence`.
    fn sequence(&mut self) -> Result<Seq, SyntaxError> {
        let mut seq = Seq::new();
        loop {
            let c = self.peek();
            let node = match char::from_u32(c).unwrap_or('\u{FFFD}') {
                '(' => {
                    // A group parses its own closure.
                    if let Some(nodes) = self.group0()? {
                        seq.extend(nodes);
                    }
                    continue;
                }
                '[' => {
                    self.clazz(true)?;
                    Node::Prop
                }
                '\\' => {
                    let e = self.next_escaped();
                    if e == ch('p') || e == ch('P') {
                        let one_letter = self.next() != ch('{');
                        if one_letter {
                            self.unread();
                        }
                        self.family(one_letter)?;
                        Node::Prop
                    } else {
                        self.unread();
                        self.atom()?
                    }
                }
                '^' | '$' => {
                    self.next();
                    Node::Zero
                }
                '.' => {
                    self.next();
                    Node::Prop
                }
                '|' | ')' => break,
                ']' | '}' => self.atom()?,
                '?' | '*' | '+' => {
                    self.next();
                    return Err(self.error(&format!(
                        "Dangling meta character '{}'",
                        char::from_u32(c).unwrap_or('?')
                    )));
                }
                '\0' if self.cursor >= self.len => break,
                _ => self.atom()?,
            };
            let node = self.closure(node)?;
            seq.push(node);
        }
        Ok(seq)
    }

    /// `Pattern.atom`: a run of literal characters and literal escapes (a
    /// `Slice`, or one character's `CharProperty`), or the one non-literal
    /// escape it starts with.
    fn atom(&mut self) -> Result<Node, SyntaxError> {
        let mut first = 0usize;
        let mut prev: Option<usize> = None;
        let mut c = self.peek();
        loop {
            match char::from_u32(c).unwrap_or('\u{FFFD}') {
                '*' | '+' | '?' | '{' => {
                    if first > 1 {
                        if let Some(p) = prev {
                            self.cursor = p;
                        }
                        first -= 1;
                    }
                }
                '$' | '.' | '^' | '(' | '[' | '|' | ')' => {}
                '\\' => {
                    let e = self.next_escaped();
                    if e == ch('p') || e == ch('P') {
                        if first > 0 {
                            self.unread();
                            break;
                        }
                        let one_letter = self.next() != ch('{');
                        if one_letter {
                            self.unread();
                        }
                        self.family(one_letter)?;
                        return Ok(Node::Prop);
                    }
                    self.unread();
                    prev = Some(self.cursor);
                    match self.escape(false, first == 0, false)? {
                        Esc::Char(_) => {
                            first += 1;
                            c = self.peek();
                            continue;
                        }
                        Esc::Node(n) if first == 0 => return Ok(n),
                        Esc::Node(_) => {
                            if let Some(p) = prev {
                                self.cursor = p;
                            }
                        }
                    }
                }
                '\0' if self.cursor >= self.len => {}
                _ => {
                    prev = Some(self.cursor);
                    first += 1;
                    c = self.next();
                    continue;
                }
            }
            break;
        }
        Ok(if first == 1 {
            Node::Prop
        } else {
            Node::Fixed(first as i32, first as i32)
        })
    }

    /// `Pattern.ref`: a back-reference takes as many digits as name an
    /// existing group.
    fn backref(&mut self, mut num: usize) -> Node {
        loop {
            let c = self.peek();
            if !is_digit(c) {
                break;
            }
            let next = num * 10 + (c - ch('0')) as usize;
            if self.groups - 1 < next {
                break;
            }
            num = next;
            self.read();
        }
        Node::BackRef
    }

    /// `Pattern.escape`: the code point of an escape that is one character,
    /// else the node it stands for (a class, an anchor, a reference).
    fn escape(&mut self, in_class: bool, create: bool, is_range: bool) -> Result<Esc, SyntaxError> {
        let c = self.skip();
        let e = char::from_u32(c).unwrap_or('\u{FFFD}');
        match e {
            '0' => return self.octal().map(Esc::Char),
            '1'..='9' if !in_class => {
                let n = if create {
                    self.backref((c - ch('0')) as usize)
                } else {
                    Node::BackRef
                };
                return Ok(Esc::Node(n));
            }
            'A' | 'B' | 'G' | 'Z' | 'z' if !in_class => return Ok(Esc::Node(Node::Zero)),
            'R' if !in_class => return Ok(Esc::Node(Node::Fixed(1, 2))),
            'X' if !in_class => return Ok(Esc::Node(Node::NonDet(1))),
            'D' | 'H' | 'S' | 'V' | 'W' | 'd' | 'h' | 's' | 'w' => {
                return Ok(Esc::Node(Node::Prop))
            }
            'N' => return self.named_char().map(Esc::Char),
            'a' => return Ok(Esc::Char(7)),
            'b' if !in_class => {
                if create && self.peek() == ch('{') {
                    if self.skip() == ch('g') {
                        if self.read() == ch('}') {
                            return Ok(Esc::Node(Node::Zero));
                        }
                        return Err(self.error("Illegal/unsupported escape sequence"));
                    }
                    self.unread();
                    self.unread();
                }
                return Ok(Esc::Node(Node::Zero));
            }
            'c' => return self.control().map(Esc::Char),
            'e' => return Ok(Esc::Char(0x1B)),
            'f' => return Ok(Esc::Char(0x0C)),
            'k' if !in_class => {
                if self.read() != ch('<') {
                    return Err(self.error("\\k is not followed by '<' for named capturing group"));
                }
                let first = self.read();
                let name = self.group_name(first)?;
                if !self.names.contains(&name) {
                    return Err(
                        self.error(&format!("named capturing group <{name}> does not exist"))
                    );
                }
                return Ok(Esc::Node(Node::BackRef));
            }
            'n' => return Ok(Esc::Char(u32::from('\n'))),
            'r' => return Ok(Esc::Char(u32::from('\r'))),
            't' => return Ok(Esc::Char(u32::from('\t'))),
            'u' => return self.unicode().map(Esc::Char),
            'v' => {
                return Ok(if is_range {
                    Esc::Char(0x0B)
                } else {
                    Esc::Node(Node::Prop)
                })
            }
            'x' => return self.hex().map(Esc::Char),
            // Every other ASCII letter is reserved, as are the digits and
            // anchors inside a class; anything else escapes itself.
            'C' | 'E' | 'F' | 'I' | 'J' | 'K' | 'L' | 'M' | 'O' | 'P' | 'Q' | 'T' | 'U' | 'Y'
            | 'g' | 'i' | 'j' | 'l' | 'm' | 'o' | 'p' | 'q' | 'y' => {}
            '1'..='9' | 'A' | 'B' | 'G' | 'R' | 'X' | 'Z' | 'z' | 'b' | 'k' => {}
            _ => return Ok(Esc::Char(c)),
        }
        Err(self.error("Illegal/unsupported escape sequence"))
    }

    /// `Pattern.clazz`: a `[…]` class, nested classes and `&&` included.
    /// `prev` and `curr` stand for the JDK's two predicates by whether each
    /// is set; `has_bits` for its bit set holding a character.
    fn clazz(&mut self, consume: bool) -> Result<(), SyntaxError> {
        let (mut prev, mut curr, mut has_bits) = (false, false, false);
        let mut c = self.next();
        if c == ch('^') && self.at(self.cursor - 1) == ch('[') {
            c = self.next();
        }
        loop {
            match char::from_u32(c).unwrap_or('\u{FFFD}') {
                '[' => {
                    self.clazz(true)?;
                    curr = true;
                    prev = true;
                    c = self.peek();
                    continue;
                }
                '&' => {
                    c = self.next();
                    if c == ch('&') {
                        c = self.next();
                        let mut right = false;
                        while c != ch(']') && c != ch('&') {
                            if c == ch('[') {
                                self.clazz(true)?;
                            } else {
                                self.unread();
                                self.clazz(false)?;
                            }
                            right = true;
                            c = self.peek();
                        }
                        if has_bits {
                            if !prev {
                                curr = true;
                            }
                            prev = true;
                            has_bits = false;
                        }
                        if right {
                            curr = true;
                        }
                        if !prev {
                            if !right {
                                return Err(self.error("Bad class syntax"));
                            }
                            prev = true;
                        } else if !curr {
                            return Err(self.error("Bad intersection syntax"));
                        }
                        continue;
                    }
                    // A single `&` is a literal.
                    self.unread();
                }
                '\0' if self.cursor >= self.len => {
                    return Err(self.error("Unclosed character class"));
                }
                ']' if prev || has_bits => {
                    if consume {
                        self.next();
                    }
                    return Ok(());
                }
                _ => {}
            }
            if self.range()? {
                has_bits = true;
            } else {
                curr = true;
                prev = true;
            }
            c = self.peek();
        }
    }

    /// `Pattern.range`: one character, range or escape inside a class —
    /// `true` when it went into the class's bit set (a character below
    /// U+0100 that `bitsOrSingle` keeps there), `false` for a predicate.
    fn range(&mut self) -> Result<bool, SyntaxError> {
        let mut c = self.peek();
        if c == ch('\\') {
            c = self.next_escaped();
            if c == ch('p') || c == ch('P') {
                let one_letter = self.next() != ch('{');
                if one_letter {
                    self.unread();
                }
                self.family(one_letter)?;
                return Ok(false);
            }
            let is_range = self.at(self.cursor + 1) == ch('-');
            self.unread();
            match self.escape(true, true, is_range)?.char() {
                Some(v) => c = v,
                None => return Ok(false),
            }
        } else {
            self.next();
        }
        if self.peek() == ch('-') {
            let end = self.at(self.cursor + 1);
            if end == ch('[') {
                return Ok(self.goes_to_bits(c));
            }
            if end != ch(']') {
                self.next();
                let mut m = self.peek();
                if m == ch('\\') {
                    m = self.escape(true, false, true)?.char().unwrap_or(0);
                } else {
                    self.next();
                }
                if m < c {
                    return Err(self.error("Illegal character range"));
                }
                return Ok(false);
            }
        }
        Ok(self.goes_to_bits(c))
    }

    /// `Pattern.bitsOrSingle`: whether `c` goes into the bit set.
    fn goes_to_bits(&self, c: u32) -> bool {
        let special = self.has(CASE_INSENSITIVE)
            && self.has(UNICODE_CASE)
            && matches!(
                c,
                0xff | 0xb5 | 0x49 | 0x69 | 0x53 | 0x73 | 0x4b | 0x6b | 0xc5 | 0xe5
            );
        c < 256 && !special
    }

    /// `Pattern.family`: the name of a `\p`/`\P` property — one letter, or
    /// braced — which must name a property the JDK knows (`property_known`).
    fn family(&mut self, single_letter: bool) -> Result<(), SyntaxError> {
        self.next();
        if single_letter {
            let c = self.at(self.cursor);
            self.read();
            let name: String = char::from_u32(c).into_iter().collect();
            return self.check_property(&name);
        }
        let i = self.cursor;
        let mark = self.len;
        self.temp[mark] = ch('}');
        while self.read() != ch('}') {}
        self.temp[mark] = 0;
        let j = self.cursor;
        if j > self.len {
            return Err(self.error("Unclosed character family"));
        }
        if i + 1 >= j {
            return Err(self.error("Empty character family"));
        }
        let name: String = self.temp[i..j - 1]
            .iter()
            .filter_map(|&c| char::from_u32(c))
            .collect();
        self.check_property(&name)
    }

    /// The rest of `Pattern.family`: an unknown name is refused in the JDK's
    /// words.
    fn check_property(&self, name: &str) -> Result<(), SyntaxError> {
        if let Some((key, value)) = name.split_once('=') {
            let key = key.to_ascii_lowercase();
            if !property_value_known(&key, value) {
                return Err(self.error(&format!(
                    "Unknown Unicode property {{name=<{key}>, value=<{value}>}}"
                )));
            }
        } else if !property_known(name, self.has(UNICODE_CHARACTER_CLASS)) {
            return Err(self.error(&format!("Unknown character property name {{{name}}}")));
        }
        Ok(())
    }

    /// `Pattern.groupname`: a Latin letter, then letters and digits, then `>`.
    fn group_name(&mut self, mut c: u32) -> Result<String, SyntaxError> {
        if !is_alpha(c) {
            return Err(self.error("capturing group name does not start with a Latin letter"));
        }
        let mut name = String::new();
        loop {
            name.push(char::from_u32(c).unwrap_or('?'));
            c = self.read();
            if !is_alnum(c) {
                break;
            }
        }
        if c != ch('>') {
            return Err(self.error("named capturing group is missing trailing '>'"));
        }
        Ok(name)
    }

    /// `Pattern.group0`: a group of any kind, then its closure. `None` for an
    /// inline modifier (`(?i)`), which is no node.
    fn group0(&mut self) -> Result<Option<Seq>, SyntaxError> {
        let saved = self.flags;
        let c = self.next();
        // `body` is what the group matches; `zero` marks a look-around, which
        // matches no characters of its own.
        let (body, kind) = if c == ch('?') {
            let k = self.skip();
            match char::from_u32(k).unwrap_or('\u{FFFD}') {
                ':' => (self.expr()?, Group::Plain),
                '=' | '!' => {
                    self.expr()?;
                    (Seq::new(), Group::Zero)
                }
                '>' => (self.expr()?, Group::Independent),
                '<' => {
                    let c = self.read();
                    if c != ch('=') && c != ch('!') {
                        let name = self.group_name(c)?;
                        if self.names.contains(&name) {
                            return Err(self.error(&format!(
                                "Named capturing group <{name}> is already defined"
                            )));
                        }
                        self.groups += 1;
                        self.names.push(name);
                        (self.expr()?, Group::Plain)
                    } else {
                        let behind = self.expr()?;
                        let mut info = Info::new();
                        study(&behind, &mut info);
                        if !info.max_valid {
                            return Err(self.error(
                                "Look-behind group does not have an obvious maximum length",
                            ));
                        }
                        (Seq::new(), Group::Zero)
                    }
                }
                '$' | '@' => return Err(self.error("Unknown group type")),
                _ => {
                    self.unread();
                    self.add_flags();
                    let c = self.read();
                    if c == ch(')') {
                        // An inline modifier only: in force from here on.
                        return Ok(None);
                    }
                    if c != ch(':') {
                        return Err(self.error("Unknown inline modifier"));
                    }
                    (self.expr()?, Group::Plain)
                }
            }
        } else {
            self.groups += 1;
            (self.expr()?, Group::Plain)
        };
        self.accept(ch(')'), "Unclosed group")?;
        self.flags = saved;
        // The group as one node, as its quantifier sees it: an atomic group
        // is the JDK's independent `Ques`, a look-around matches nothing.
        let head = match kind {
            Group::Plain => Node::Group(body.clone()),
            Group::Independent => Node::Ques(body.clone(), true),
            Group::Zero => Node::Zero,
        };
        let node = self.closure(head.clone())?;
        if node == head {
            // No closure: the group's nodes stand in the sequence as written.
            return Ok(Some(match kind {
                Group::Plain => body,
                _ => vec![head],
            }));
        }
        if kind == Group::Zero {
            return Ok(Some(vec![node]));
        }
        Ok(Some(vec![match node {
            Node::Ques(atom, _) if !self.last_possessive => {
                // `?` on a group is a `Branch` of the group and nothing.
                let atom = Some(atom);
                if self.last_lazy {
                    Node::Branch(vec![None, atom])
                } else {
                    Node::Branch(vec![atom, None])
                }
            }
            Node::Curly(atom, cmin, cmax) if !self.last_possessive => {
                // A deterministic body repeats as a `GroupCurly`, studied as
                // a `Curly`; any other as a `Loop`.
                let mut info = Info::new();
                if study(&atom, &mut info) {
                    Node::Curly(atom, cmin, cmax)
                } else {
                    Node::Loop
                }
            }
            other => other,
        }]))
    }

    /// `Pattern.addFlag`/`subFlag`: the inline modifiers.
    fn add_flags(&mut self) {
        let mut c = self.peek();
        let mut on = true;
        loop {
            let bit = match char::from_u32(c).unwrap_or('\u{FFFD}') {
                'i' => CASE_INSENSITIVE,
                'm' => MULTILINE,
                's' => DOTALL,
                'd' => UNIX_LINES,
                'u' => UNICODE_CASE,
                'c' => CANON_EQ,
                'x' => COMMENTS,
                'U' => UNICODE_CHARACTER_CLASS | UNICODE_CASE,
                '-' if on => {
                    on = false;
                    c = self.next();
                    continue;
                }
                _ => return,
            };
            if on {
                self.flags |= bit;
            } else {
                self.flags &= !bit;
            }
            c = self.next();
        }
    }

    /// `Pattern.closure`: `prev` under an optional quantifier — the node the
    /// JDK builds for it, as far as studying it goes.
    fn closure(&mut self, prev: Node) -> Result<Node, SyntaxError> {
        let c = self.peek();
        let atom = |n: &Node| -> Seq {
            match n {
                Node::Group(body) => body.clone(),
                other => vec![other.clone()],
            }
        };
        Ok(match char::from_u32(c).unwrap_or('\u{FFFD}') {
            '?' => {
                self.qtype();
                Node::Ques(atom(&prev), false)
            }
            '*' | '+' => {
                let cmin = i32::from(c == ch('+'));
                self.curly(prev, cmin)
            }
            '{' => {
                let mut c = self.skip();
                if !is_digit(c) {
                    return Err(self.error("Illegal repetition"));
                }
                let mut cmin: i32 = 0;
                loop {
                    cmin = cmin
                        .checked_mul(10)
                        .and_then(|v| v.checked_add((c - ch('0')) as i32))
                        .ok_or_else(|| self.error("Illegal repetition range"))?;
                    c = self.read();
                    if !is_digit(c) {
                        break;
                    }
                }
                let cmax = if c == ch(',') {
                    c = self.read();
                    if c == ch('}') {
                        self.unread();
                        return Ok(self.curly(prev, cmin));
                    }
                    let mut m: i32 = 0;
                    while is_digit(c) {
                        m = m
                            .checked_mul(10)
                            .and_then(|v| v.checked_add((c - ch('0')) as i32))
                            .ok_or_else(|| self.error("Illegal repetition range"))?;
                        c = self.read();
                    }
                    m
                } else {
                    cmin
                };
                if c != ch('}') {
                    return Err(self.error("Unclosed counted closure"));
                }
                if cmax < cmin {
                    return Err(self.error("Illegal repetition range"));
                }
                self.unread();
                self.qtype();
                if cmin == 0 && cmax == 1 {
                    Node::Ques(atom(&prev), false)
                } else {
                    Node::Curly(atom(&prev), cmin, cmax)
                }
            }
            _ => prev,
        })
    }

    /// `Pattern.curly`: `*`, `+` and `{n,}`. A greedy one on a single
    /// character property is the JDK's `CharPropertyGreedy`.
    fn curly(&mut self, prev: Node, cmin: i32) -> Node {
        self.qtype();
        if !self.last_lazy && !self.last_possessive && prev == Node::Prop {
            return Node::Greedy(cmin);
        }
        let atom = match prev {
            Node::Group(body) => body,
            other => vec![other],
        };
        Node::Curly(atom, cmin, MAX_REPS)
    }

    /// `Pattern.qtype`: step past the quantifier and its `?`/`+` suffix,
    /// recording which it was.
    fn qtype(&mut self) {
        let c = self.next();
        self.last_lazy = c == ch('?');
        self.last_possessive = c == ch('+');
        if self.last_lazy || self.last_possessive {
            self.next();
        }
    }

    /// `Pattern.c`.
    fn control(&mut self) -> Result<u32, SyntaxError> {
        if self.cursor < self.len {
            return Ok(self.read() ^ 64);
        }
        Err(self.error("Illegal control escape sequence"))
    }

    /// `Pattern.o`.
    fn octal(&mut self) -> Result<u32, SyntaxError> {
        let octal = |c: u32| (ch('0')..=ch('7')).contains(&c);
        let n = self.read();
        if octal(n) {
            let m = self.read();
            if octal(m) {
                let o = self.read();
                if octal(o) && (ch('0')..=ch('3')).contains(&n) {
                    return Ok((n - ch('0')) * 64 + (m - ch('0')) * 8 + (o - ch('0')));
                }
                self.unread();
                return Ok((n - ch('0')) * 8 + (m - ch('0')));
            }
            self.unread();
            return Ok(n - ch('0'));
        }
        Err(self.error("Illegal octal escape sequence"))
    }

    /// `Pattern.x`.
    fn hex(&mut self) -> Result<u32, SyntaxError> {
        let digit = |c: u32| char::from_u32(c).and_then(|c| c.to_digit(16)).unwrap_or(0);
        let n = self.read();
        if is_hex(n) {
            let m = self.read();
            if is_hex(m) {
                return Ok(digit(n) * 16 + digit(m));
            }
        } else if n == ch('{') && is_hex(self.peek()) {
            let mut v: u32 = 0;
            let mut d;
            loop {
                d = self.read();
                if !is_hex(d) {
                    break;
                }
                v = (v << 4) + digit(d);
                if v > 0x10FFFF {
                    return Err(self.error("Hexadecimal codepoint is too big"));
                }
            }
            if d != ch('}') {
                return Err(self.error("Unclosed hexadecimal escape sequence"));
            }
            return Ok(v);
        }
        Err(self.error("Illegal hexadecimal escape sequence"))
    }

    /// `Pattern.uxxxx`.
    fn four_hex(&mut self) -> Result<u32, SyntaxError> {
        let mut n = 0;
        for _ in 0..4 {
            let c = self.read();
            if !is_hex(c) {
                return Err(self.error("Illegal Unicode escape sequence"));
            }
            n = n * 16 + char::from_u32(c).and_then(|c| c.to_digit(16)).unwrap_or(0);
        }
        Ok(n)
    }

    /// `Pattern.u`: a `\uXXXX` escape, two of them for a surrogate pair.
    fn unicode(&mut self) -> Result<u32, SyntaxError> {
        let n = self.four_hex()?;
        if (0xD800..=0xDBFF).contains(&n) {
            let cur = self.cursor;
            if self.read() == ch('\\') && self.read() == ch('u') {
                let lo = self.four_hex()?;
                if (0xDC00..=0xDFFF).contains(&lo) {
                    return Ok(crate::jstr::pair(n as u16, lo as u16));
                }
            }
            self.cursor = cur;
        }
        Ok(n)
    }

    /// `Pattern.N`: a `\N{name}` escape, the character `Character.codePointOf`
    /// names.
    fn named_char(&mut self) -> Result<u32, SyntaxError> {
        if self.read() == ch('{') {
            let i = self.cursor;
            while self.read() != ch('}') {
                if self.cursor >= self.len {
                    return Err(self.error("Unclosed character name escape sequence"));
                }
            }
            let name: String = self.temp[i..self.cursor - 1]
                .iter()
                .filter_map(|&c| char::from_u32(c))
                .collect();
            return crate::uninames::code_point_of(&name)
                .ok_or_else(|| self.error(&format!("Unknown character name [{name}]")));
        }
        Err(self.error("Illegal character name escape sequence"))
    }
}

/// Whether `Pattern.family` finds a property named `name` (no `=`): an
/// `In` prefix names a block, an `Is` prefix a binary property, a general
/// category or a script, and a bare name a POSIX class (under
/// `UNICODE_CHARACTER_CLASS`) or a category, POSIX or `java` predicate.
fn property_known(name: &str, unicode_classes: bool) -> bool {
    use crate::regex_names::*;
    let has = |table: &[&str], key: &str| table.binary_search(&key).is_ok();
    if let Some(block) = name.strip_prefix("In") {
        has(BLOCKS, &block.to_uppercase())
    } else if let Some(short) = name.strip_prefix("Is") {
        has(UNICODE_PROPERTIES, &short.to_uppercase())
            || has(PROPERTIES, short)
            || has(SCRIPTS, &short.to_uppercase())
    } else {
        (unicode_classes && has(POSIX, &name.to_uppercase())) || has(PROPERTIES, name)
    }
}

/// Whether `Pattern.family` finds `\p{key=value}`, `key` lower-cased.
fn property_value_known(key: &str, value: &str) -> bool {
    use crate::regex_names::*;
    let has = |table: &[&str], key: &str| table.binary_search(&key).is_ok();
    match key {
        "sc" | "script" => has(SCRIPTS, &value.to_uppercase()),
        "blk" | "block" => has(BLOCKS, &value.to_uppercase()),
        "gc" | "general_category" => has(PROPERTIES, value),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(p: &str) -> (String, i64) {
        let e = check(p, 0).expect_err(p);
        (e.desc, e.index)
    }

    #[test]
    fn look_behind_lengths_follow_node_study() {
        // Each verdict measured on openjdk 21.0.13: a single-character `*`
        // is a `CharPropertyGreedy`, whose maximum overflows without being
        // invalid; a group under `*` multiplies its length past `int`.
        const NO_MAX: &str = "Look-behind group does not have an obvious maximum length";
        for ok in [
            "(?<=a*)b",
            "(?<=(a)*)b",
            "(?<=ab*c)d",
            "(?<=a{2,})b",
            "(?<=(?=a*)b)c",
        ] {
            assert!(check(ok, 0).is_ok(), "{ok}");
        }
        assert_eq!(err("(?<=(ab)*)c"), (NO_MAX.to_string(), 8));
        assert_eq!(err("(?<=x(?:ab)*)c"), (NO_MAX.to_string(), 11));
        assert_eq!(err("(?<=\\1)(a)"), (NO_MAX.to_string(), 5));
    }
}
