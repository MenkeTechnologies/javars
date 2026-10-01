//! Console and text input: `System.in`, `java.util.Scanner`,
//! `java.io.BufferedReader`/`InputStreamReader`/`StringReader`, and
//! `java.util.StringTokenizer`.
//!
//! Every reader is a cursor over characters. A reader over a `String` owns its
//! text; every reader over `System.in` shares one process-wide cursor, filled a
//! line at a time on demand — so a program that prompts and then reads sees its
//! prompt before it blocks, and a `Scanner` and a `BufferedReader` over the
//! same stream consume one sequence of characters between them.
//!
//! The semantics are the JDK's, measured on openjdk 27 in the root locale:
//!
//!   * `Scanner` delimits tokens by `\p{javaWhitespace}+`. `next*()` skips the
//!     delimiters first; `nextLine()` does not, and answers the rest of the
//!     current line (empty right after a `nextInt()`), consuming the line
//!     terminator — `\n`, `\r\n`, `\r`, ` `, ` ` or `\u0085`.
//!   * A `nextInt()` whose token is not an `int` throws
//!     `InputMismatchException` and leaves the token unread; a token that *is*
//!     an integer but out of range carries `For input string: "…"` as its
//!     message, any other carries none. The root locale's grouping separator is
//!     accepted (`1,234` is 1234); `0x1F`, `1_000`, and `3.0` are not integers.
//!   * Running out of input is `NoSuchElementException` — `No line found` from
//!     `nextLine()`, no message from `next()`. A closed `Scanner` answers
//!     `IllegalStateException: Scanner closed` to every query.
//!   * `BufferedReader.readLine()` answers `null` at the end of input, and a
//!     closed one throws `IOException: Stream closed`.

use std::cell::RefCell;
use std::io::{BufRead, Write};

/// A character cursor: the characters read so far, the read position, and
/// whether more can arrive.
#[derive(Default)]
pub struct Cursor {
    buf: Vec<char>,
    pos: usize,
    /// No more characters will arrive: the text was given whole, or stdin hit
    /// its end.
    done: bool,
}

impl Cursor {
    fn text(s: &str) -> Cursor {
        Cursor {
            buf: s.chars().collect(),
            pos: 0,
            done: true,
        }
    }

    /// The character at absolute index `i`, reading more input while `i` is
    /// past what has arrived. `None` at the end of input.
    fn at(&mut self, i: usize) -> Option<char> {
        while i >= self.buf.len() {
            if self.done || !self.fill() {
                return None;
            }
        }
        Some(self.buf[i])
    }

    /// Read one more line of stdin. Only a stdin cursor is ever not `done`.
    fn fill(&mut self) -> bool {
        // A prompt printed with `print` has to be visible before the read
        // blocks on the answer to it.
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        match std::io::stdin().lock().read_line(&mut line) {
            Ok(n) if n > 0 => {
                self.buf.extend(line.chars());
                true
            }
            _ => {
                self.done = true;
                false
            }
        }
    }

    /// The rest of the current line and the index just past its terminator, or
    /// `None` at the end of input.
    fn line_from(&mut self, start: usize) -> Option<(String, usize)> {
        self.at(start)?;
        let mut out = String::new();
        let mut i = start;
        while let Some(c) = self.at(i) {
            i += 1;
            match c {
                '\n' | '\u{2028}' | '\u{2029}' | '\u{85}' => return Some((out, i)),
                '\r' => {
                    if self.at(i) == Some('\n') {
                        i += 1;
                    }
                    return Some((out, i));
                }
                _ => out.push(c),
            }
        }
        Some((out, i))
    }

    /// The next whitespace-delimited token and the index just past it, without
    /// consuming anything. `None` when only delimiters remain.
    fn token_from(&mut self, start: usize) -> Option<(String, usize)> {
        let mut i = start;
        while is_java_whitespace(self.at(i)?) {
            i += 1;
        }
        let mut tok = String::new();
        while let Some(c) = self.at(i) {
            if is_java_whitespace(c) {
                break;
            }
            tok.push(c);
            i += 1;
        }
        Some((tok, i))
    }
}

thread_local! {
    /// The one cursor every reader over `System.in` shares.
    static STDIN: RefCell<Cursor> = RefCell::new(Cursor::default());
}

/// `Character.isWhitespace`: the Unicode space separators except the
/// non-breaking ones, plus the ASCII controls `\t \n \u000B \f \r` and the
/// four information separators `\u001C`–`\u001F`.
fn is_java_whitespace(c: char) -> bool {
    match c {
        '\u{a0}' | '\u{2007}' | '\u{202f}' => false,
        '\u{1c}'..='\u{1f}' => true,
        _ => c.is_whitespace() && c != '\u{85}',
    }
}

/// What a reader is, which decides the methods it answers and its class name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// `System.in` itself.
    InputStream,
    InputStreamReader,
    StringReader,
    BufferedReader,
    Scanner,
}

impl Kind {
    /// The fully-qualified class name `getClass().getName()` reports.
    pub fn class_name(self) -> &'static str {
        match self {
            Kind::InputStream => "java.io.BufferedInputStream",
            Kind::InputStreamReader => "java.io.InputStreamReader",
            Kind::StringReader => "java.io.StringReader",
            Kind::BufferedReader => "java.io.BufferedReader",
            Kind::Scanner => "java.util.Scanner",
        }
    }
}

/// One reader object.
pub struct Reader {
    pub kind: Kind,
    /// `None` reads `System.in`'s shared cursor.
    own: Option<Cursor>,
    closed: bool,
}

/// What a reader call answers.
pub enum Out {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
    Unit,
    Lines(Vec<String>),
}

/// A Java exception: simple class name and message (`None` for none).
pub type Fail = (&'static str, Option<String>);

impl Reader {
    /// A reader of `kind` over `System.in`.
    pub fn stdin(kind: Kind) -> Reader {
        Reader {
            kind,
            own: None,
            closed: false,
        }
    }

    /// A reader of `kind` over the characters of `s`.
    pub fn text(kind: Kind, s: &str) -> Reader {
        Reader {
            kind,
            own: Some(Cursor::text(s)),
            closed: false,
        }
    }

    /// Wrap `inner` as a reader of `kind`, taking over its characters: a
    /// `BufferedReader` around a `StringReader` reads what the `StringReader`
    /// would have.
    pub fn wrap(kind: Kind, inner: &mut Reader) -> Reader {
        Reader {
            kind,
            own: inner
                .own
                .as_mut()
                .map(|c| std::mem::replace(c, Cursor::text(""))),
            closed: false,
        }
    }

    /// Run `f` on this reader's cursor. Characters already consumed are
    /// dropped first — never during `f`, whose indices are absolute — so a long
    /// stream does not accumulate.
    fn with<R>(&mut self, f: impl FnOnce(&mut Cursor) -> R) -> R {
        match &mut self.own {
            Some(c) => f(c),
            None => STDIN.with(|s| {
                let mut c = s.borrow_mut();
                if c.pos > 1 << 16 {
                    let pos = c.pos;
                    c.buf.drain(..pos);
                    c.pos = 0;
                }
                f(&mut c)
            }),
        }
    }

    /// Run `method(args)` — `args` already rendered to text where a method
    /// takes text. `None` when the reader has no such method.
    pub fn call(&mut self, method: &str, args: &[ArgVal]) -> Option<Result<Out, Fail>> {
        match self.kind {
            Kind::Scanner => self.scanner(method, args),
            Kind::BufferedReader
            | Kind::InputStreamReader
            | Kind::StringReader
            | Kind::InputStream => self.reader(method, args),
        }
    }

    fn scanner(&mut self, method: &str, args: &[ArgVal]) -> Option<Result<Out, Fail>> {
        if method == "close" && args.is_empty() {
            self.closed = true;
            return Some(Ok(Out::Unit));
        }
        if self.closed {
            return Some(Err((
                "IllegalStateException",
                Some("Scanner closed".into()),
            )));
        }
        if !args.is_empty() {
            return None;
        }
        let no_elem: Fail = ("NoSuchElementException", None);
        Some(match method {
            "hasNext" => Ok(Out::Bool(self.with(|c| c.token_from(c.pos).is_some()))),
            "hasNextLine" => Ok(Out::Bool(self.with(|c| c.at(c.pos).is_some()))),
            "next" => self.with(|c| match c.token_from(c.pos) {
                Some((t, end)) => {
                    c.pos = end;
                    Ok(Out::Str(t))
                }
                None => Err(no_elem),
            }),
            "nextLine" => self.with(|c| match c.line_from(c.pos) {
                Some((l, end)) => {
                    c.pos = end;
                    Ok(Out::Str(l))
                }
                None => Err(("NoSuchElementException", Some("No line found".into()))),
            }),
            "hasNextInt" | "hasNextLong" | "hasNextShort" | "hasNextByte" | "hasNextDouble"
            | "hasNextFloat" | "hasNextBoolean" => {
                let want = &method["hasNext".len()..];
                Ok(Out::Bool(self.with(|c| {
                    c.token_from(c.pos)
                        .is_some_and(|(t, _)| parse_token(want, &t).is_ok())
                })))
            }
            "nextInt" | "nextLong" | "nextShort" | "nextByte" | "nextDouble" | "nextFloat"
            | "nextBoolean" => {
                let want = &method["next".len()..];
                self.with(|c| match c.token_from(c.pos) {
                    None => Err(no_elem),
                    Some((t, end)) => {
                        let v =
                            parse_token(want, &t).map_err(|msg| ("InputMismatchException", msg))?;
                        c.pos = end;
                        Ok(v)
                    }
                })
            }
            _ => return None,
        })
    }

    fn reader(&mut self, method: &str, args: &[ArgVal]) -> Option<Result<Out, Fail>> {
        if method == "close" && args.is_empty() {
            self.closed = true;
            return Some(Ok(Out::Unit));
        }
        if self.closed {
            return Some(Err(("IOException", Some("Stream closed".into()))));
        }
        let line_reader = self.kind == Kind::BufferedReader;
        Some(match (method, args) {
            ("readLine", []) if line_reader => Ok(self.with(|c| match c.line_from(c.pos) {
                Some((l, end)) => {
                    c.pos = end;
                    Out::Str(l)
                }
                None => Out::Null,
            })),
            ("lines", []) if line_reader => Ok(Out::Lines(self.with(|c| {
                let mut out = Vec::new();
                while let Some((l, end)) = c.line_from(c.pos) {
                    c.pos = end;
                    out.push(l);
                }
                out
            }))),
            ("read", []) => Ok(Out::Int(self.with(|c| match c.at(c.pos) {
                Some(ch) => {
                    c.pos += 1;
                    ch as i64
                }
                None => -1,
            }))),
            // A `StringReader` is always ready; stdin is ready while a line it
            // already read is unconsumed, which is what never blocks.
            ("ready", []) => Ok(Out::Bool(self.with(|c| c.done || c.pos < c.buf.len()))),
            ("skip", [ArgVal::Int(n)]) => Ok(Out::Int(self.with(|c| {
                let mut k = 0;
                while k < *n && c.at(c.pos).is_some() {
                    c.pos += 1;
                    k += 1;
                }
                k
            }))),
            _ => return None,
        })
    }
}

/// An argument a reader method takes.
pub enum ArgVal {
    Int(i64),
    Str(String),
}

/// Parse a `Scanner` token as the type `want` names (`Int`, `Double`, …).
/// `Err` carries the `InputMismatchException` message: `For input string`
/// for an integer out of range, nothing for a token of the wrong shape.
fn parse_token(want: &str, t: &str) -> Result<Out, Option<String>> {
    match want {
        "Boolean" => match t.to_ascii_lowercase().as_str() {
            "true" => Ok(Out::Bool(true)),
            "false" => Ok(Out::Bool(false)),
            _ => Err(None),
        },
        "Double" | "Float" => parse_decimal(t)
            .map(|v| Out::Float(if want == "Float" { v as f32 as f64 } else { v }))
            .ok_or(None),
        _ => {
            let digits = integer_digits(t).ok_or(None)?;
            let (lo, hi) = match want {
                "Int" => (i32::MIN as i64, i32::MAX as i64),
                "Short" => (i16::MIN as i64, i16::MAX as i64),
                "Byte" => (i8::MIN as i64, i8::MAX as i64),
                _ => (i64::MIN, i64::MAX),
            };
            match digits.parse::<i64>() {
                Ok(v) if (lo..=hi).contains(&v) => Ok(Out::Int(v)),
                _ => Err(Some(format!("For input string: \"{digits}\""))),
            }
        }
    }
}

/// The digits of a `Scanner` integer token with its grouping separators
/// removed, keeping a `-` sign and dropping a `+`; `None` when the token is not
/// an integer. A grouped numeral is a leading group of one to three digits (not
/// starting with `0`) and then groups of exactly three.
fn integer_digits(t: &str) -> Option<String> {
    let (neg, body) = match t.as_bytes().first()? {
        b'-' => (true, &t[1..]),
        b'+' => (false, &t[1..]),
        _ => (false, t),
    };
    if body.is_empty() || !body.bytes().all(|b| b.is_ascii_digit() || b == b',') {
        return None;
    }
    if body.contains(',') {
        let groups: Vec<&str> = body.split(',').collect();
        let first = groups[0];
        if first.is_empty() || first.len() > 3 || first.starts_with('0') {
            return None;
        }
        if groups[1..].iter().any(|g| g.len() != 3) {
            return None;
        }
    }
    let digits: String = body.chars().filter(|c| *c != ',').collect();
    Some(if neg { format!("-{digits}") } else { digits })
}

/// A `Scanner` decimal token: an optional sign, then `NaN`, `Infinity`, or a
/// (possibly grouped) numeral with an optional fraction and exponent.
fn parse_decimal(t: &str) -> Option<f64> {
    let (neg, body) = match t.as_bytes().first()? {
        b'-' => (true, &t[1..]),
        b'+' => (false, &t[1..]),
        _ => (false, t),
    };
    let v = match body {
        "NaN" => f64::NAN,
        "Infinity" => f64::INFINITY,
        _ => {
            let (mantissa, exp) = match body.find(['e', 'E']) {
                Some(i) => (&body[..i], Some(&body[i + 1..])),
                None => (body, None),
            };
            if let Some(e) = exp {
                let e = e.strip_prefix(['+', '-']).unwrap_or(e);
                if e.is_empty() || !e.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
            }
            let (int, frac) = match mantissa.split_once('.') {
                Some((i, f)) => (i, Some(f)),
                None => (mantissa, None),
            };
            if !frac.is_none_or(|f| f.bytes().all(|b| b.is_ascii_digit())) {
                return None;
            }
            if int.is_empty() && frac.is_none_or(str::is_empty) {
                return None;
            }
            let int = if int.is_empty() {
                String::new()
            } else {
                integer_digits(int).filter(|d| !d.starts_with('-'))?
            };
            let plain = format!(
                "{int}{}{}",
                frac.map(|f| format!(".{f}")).unwrap_or_default(),
                exp.map(|e| format!("e{e}")).unwrap_or_default()
            );
            plain.parse::<f64>().ok()?
        }
    };
    Some(if neg { -v } else { v })
}

/// `java.util.StringTokenizer`: the tokens of a string split on any of a set
/// of delimiter characters, read front to back.
pub struct Tokenizer {
    tokens: Vec<String>,
    next: usize,
}

impl Tokenizer {
    /// `new StringTokenizer(s, delims, returnDelims)`. The default delimiters
    /// are `" \t\n\r\f"`.
    pub fn new(s: &str, delims: Option<&str>, return_delims: bool) -> Tokenizer {
        let delims: Vec<char> = delims.unwrap_or(" \t\n\r\x0c").chars().collect();
        let mut tokens = Vec::new();
        let mut cur = String::new();
        for c in s.chars() {
            if delims.contains(&c) {
                if !cur.is_empty() {
                    tokens.push(std::mem::take(&mut cur));
                }
                if return_delims {
                    tokens.push(c.to_string());
                }
            } else {
                cur.push(c);
            }
        }
        if !cur.is_empty() {
            tokens.push(cur);
        }
        Tokenizer { tokens, next: 0 }
    }

    pub fn call(&mut self, method: &str, argc: usize) -> Option<Result<Out, Fail>> {
        Some(match (method, argc) {
            ("hasMoreTokens" | "hasMoreElements", 0) => {
                Ok(Out::Bool(self.next < self.tokens.len()))
            }
            ("countTokens", 0) => Ok(Out::Int((self.tokens.len() - self.next) as i64)),
            ("nextToken" | "nextElement", 0) => match self.tokens.get(self.next) {
                Some(t) => {
                    self.next += 1;
                    Ok(Out::Str(t.clone()))
                }
                None => Err(("NoSuchElementException", None)),
            },
            _ => return None,
        })
    }
}
