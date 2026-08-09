//! A hand-written Java lexer.
//!
//! Produces the token stream the parser consumes. Covers the slice-1 surface:
//! identifiers/keywords, integer/floating/string/char literals, and the
//! operators and punctuation used by declarations, expressions, and the
//! C-style control statements. Line/block/`//`-doc comments are skipped.

use std::fmt;

/// A lexical token with its 1-based source line (for error reporting).
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: Tok,
    pub line: u32,
}

/// Token kinds.
#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    // literals & names
    Int(i64),
    /// An `L`-suffixed integer literal (`3000000000L`). Java types it `long`,
    /// which is what keeps it out of the 32-bit `int` wrap — so the suffix has
    /// to survive lexing rather than being dropped with the other ones.
    Long(i64),
    Float(f64),
    /// An `f`/`F`-suffixed floating literal (`1.5f`). Java types it `float`,
    /// which is a *32-bit* value — a distinct token because the width is what
    /// makes `1.0f / 3.0f` print `0.33333334` rather than the `double` answer.
    Float32(f64),
    Str(String),
    /// A `char` literal (`'a'`, `'\n'`). Java's `char` is a 16-bit *integral*
    /// type, so this is distinct from [`Tok::Str`]: `'a' + 1` is 98, not `"a1"`.
    Char(char),
    Ident(String),
    // keywords
    Class,
    Public,
    Static,
    Void,
    If,
    Else,
    While,
    For,
    Do,
    Switch,
    Case,
    Default,
    Return,
    Break,
    Continue,
    True,
    False,
    New,
    // punctuation
    LBrace,
    RBrace,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Semi,
    Comma,
    Dot,
    Question,
    Colon,
    /// `->` — the lambda arrow (and the arrow form of `switch`).
    Arrow,
    /// `::` — the method-reference separator (`String::length`).
    ColonColon,
    // operators
    Assign,
    PlusAssign,
    MinusAssign,
    StarAssign,
    SlashAssign,
    PercentAssign,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    PlusPlus,
    MinusMinus,
    EqEq,
    NotEq,
    Lt,
    Gt,
    Le,
    Ge,
    AndAnd,
    OrOr,
    Not,
    /// `&` — bitwise AND on integers, non-short-circuit AND on booleans.
    Amp,
    /// `|` — bitwise OR on integers, non-short-circuit OR on booleans. Also the
    /// separator of a multi-catch (`catch (A | B e)`).
    Pipe,
    /// `^` — bitwise XOR on integers, logical XOR on booleans.
    Caret,
    /// `~` — bitwise complement.
    Tilde,
    /// `<<` — left shift.
    Shl,
    /// `>>` — arithmetic (sign-propagating) right shift.
    Shr,
    /// `>>>` — logical (zero-fill) right shift.
    Ushr,
    AmpAssign,
    PipeAssign,
    CaretAssign,
    ShlAssign,
    ShrAssign,
    UshrAssign,
    Eof,
}

impl Tok {
    /// How many closing `>` of a generic type-argument list this token spells.
    ///
    /// The lexer takes the longest match, so `Map<String, List<String>>` ends in
    /// one [`Tok::Shr`] rather than two [`Tok::Gt`]. Every generic-skipping
    /// depth counter asks this instead of matching `Gt` alone, so a nested type
    /// argument closes correctly.
    pub fn generic_closers(&self) -> i32 {
        match self {
            Tok::Gt => 1,
            Tok::Shr => 2,
            Tok::Ushr => 3,
            _ => 0,
        }
    }
}

impl fmt::Display for Tok {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

/// Lex `src` into a token vector terminated by `Tok::Eof`.
pub fn lex(src: &str) -> Result<Vec<Token>, String> {
    let bytes = src.as_bytes();
    let mut i = 0usize;
    let mut line = 1u32;
    let mut out = Vec::new();

    while i < bytes.len() {
        let c = bytes[i] as char;

        // whitespace
        if c == '\n' {
            line += 1;
            i += 1;
            continue;
        }
        if c.is_whitespace() {
            i += 1;
            continue;
        }

        // comments
        if c == '/' && i + 1 < bytes.len() {
            match bytes[i + 1] as char {
                '/' => {
                    i += 2;
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                    continue;
                }
                '*' => {
                    i += 2;
                    while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                        if bytes[i] == b'\n' {
                            line += 1;
                        }
                        i += 1;
                    }
                    i += 2; // consume closing */
                    continue;
                }
                _ => {}
            }
        }

        // identifiers & keywords
        if c.is_ascii_alphabetic() || c == '_' || c == '$' {
            let start = i;
            while i < bytes.len() {
                let ch = bytes[i] as char;
                if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' {
                    i += 1;
                } else {
                    break;
                }
            }
            let word = &src[start..i];
            out.push(Token {
                kind: keyword_or_ident(word),
                line,
            });
            continue;
        }

        // `0x…` / `0X…` hex and `0b…` / `0B…` binary integer literals. Neither
        // takes a fractional part or an exponent, so they are lexed ahead of the
        // decimal path rather than inside it. Java reads them as a *bit
        // pattern*: `0xFFFFFFFF` is the `int` -1 and `0xFFFFFFFFFFFFFFFFL` is the
        // `long` -1, so the digits are parsed unsigned and then reinterpreted at
        // the literal's width.
        if c == '0'
            && i + 1 < bytes.len()
            && matches!(bytes[i + 1], b'x' | b'X' | b'b' | b'B')
            && i + 2 < bytes.len()
            && (bytes[i + 2] as char).is_ascii_alphanumeric()
        {
            let radix = if matches!(bytes[i + 1], b'x' | b'X') {
                16
            } else {
                2
            };
            i += 2;
            let start = i;
            while i < bytes.len()
                && ((bytes[i] as char).is_ascii_alphanumeric() || bytes[i] == b'_')
            {
                i += 1;
            }
            let mut digits = src[start..i].replace('_', "");
            // Only `L`/`l` is a suffix here — `d`/`f` are hex *digits*, and Java
            // has no `d`/`f`-suffixed hex integer literal.
            let is_long = digits.ends_with(['L', 'l']);
            if is_long {
                digits.pop();
            }
            let bits = u64::from_str_radix(&digits, radix)
                .map_err(|_| format!("javars: bad integer literal `{digits}` on line {line}"))?;
            let v = if is_long {
                bits as i64
            } else {
                bits as u32 as i32 as i64
            };
            out.push(Token {
                kind: if is_long { Tok::Long(v) } else { Tok::Int(v) },
                line,
            });
            continue;
        }

        // numbers (int or float)
        if c.is_ascii_digit() {
            let start = i;
            let mut is_float = false;
            while i < bytes.len() && ((bytes[i] as char).is_ascii_digit() || bytes[i] == b'_') {
                i += 1;
            }
            if i < bytes.len()
                && bytes[i] == b'.'
                && i + 1 < bytes.len()
                && (bytes[i + 1] as char).is_ascii_digit()
            {
                is_float = true;
                i += 1;
                while i < bytes.len() && ((bytes[i] as char).is_ascii_digit() || bytes[i] == b'_') {
                    i += 1;
                }
            }
            // Exponent part: `1e3`, `1.5e-3`, `2E+2`. Java allows an exponent with
            // or without a fractional part, and it makes the literal floating
            // point either way. Only consume the `e` when a valid exponent
            // actually follows, so `1.foo` and an identifier starting with `e`
            // are left alone.
            if i < bytes.len() && matches!(bytes[i], b'e' | b'E') {
                let mut j = i + 1;
                if j < bytes.len() && matches!(bytes[j], b'+' | b'-') {
                    j += 1;
                }
                if j < bytes.len() && (bytes[j] as char).is_ascii_digit() {
                    is_float = true;
                    i = j;
                    while i < bytes.len() && (bytes[i] as char).is_ascii_digit() {
                        i += 1;
                    }
                }
            }
            // Float/double suffixes only make the literal floating; the `L`
            // suffix is kept, because it is what makes the literal a `long` and
            // therefore exempt from Java's 32-bit `int` wrap. The slice ends
            // here rather than being alpha-trimmed, because trimming trailing
            // letters would also eat the `e` of an exponent.
            let num_end = i;
            let mut is_long = false;
            let mut is_f32 = false;
            if i < bytes.len() && matches!(bytes[i], b'L' | b'l' | b'f' | b'F' | b'd' | b'D') {
                match bytes[i] {
                    b'f' | b'F' => {
                        is_float = true;
                        is_f32 = true;
                    }
                    b'd' | b'D' => is_float = true,
                    _ => is_long = true,
                }
                i += 1;
            }
            // Java lets `_` separate digits anywhere inside a literal
            // (`1_000_000`, `3.141_592`); it carries no value, so it is dropped
            // before parsing.
            let text = src[start..num_end].replace('_', "");
            let text = text.as_str();
            if is_float {
                let v: f64 = text
                    .parse()
                    .map_err(|_| format!("javars: bad float literal `{text}` on line {line}"))?;
                out.push(Token {
                    kind: if is_f32 {
                        Tok::Float32(v)
                    } else {
                        Tok::Float(v)
                    },
                    line,
                });
            } else {
                // A leading `0` on a multi-digit integer is Java's octal form:
                // `017` is 15. `0` itself stays decimal zero.
                let v: i64 = if text.len() > 1 && text.starts_with('0') {
                    i64::from_str_radix(&text[1..], 8)
                        .map_err(|_| format!("javars: bad octal literal `{text}` on line {line}"))?
                } else {
                    text.parse().map_err(|_| {
                        format!("javars: bad integer literal `{text}` on line {line}")
                    })?
                };
                out.push(Token {
                    kind: if is_long { Tok::Long(v) } else { Tok::Int(v) },
                    line,
                });
            }
            continue;
        }

        // string literals
        if c == '"' {
            i += 1;
            let mut s = String::new();
            while i < bytes.len() && bytes[i] != b'"' {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 1;
                    s.push(unescape(bytes[i] as char));
                    i += 1;
                } else {
                    // Decode a full UTF-8 char so multibyte literals survive.
                    let ch = src[i..].chars().next().unwrap();
                    if ch == '\n' {
                        line += 1;
                    }
                    s.push(ch);
                    i += ch.len_utf8();
                }
            }
            if i >= bytes.len() {
                return Err(format!(
                    "javars: unterminated string literal on line {line}"
                ));
            }
            i += 1; // closing quote
            out.push(Token {
                kind: Tok::Str(s),
                line,
            });
            continue;
        }

        // char literals — a 16-bit integral value (its code point), which is
        // what makes `'a' + 1` arithmetic rather than concatenation.
        if c == '\'' {
            i += 1;
            let ch = if bytes[i] == b'\\' {
                i += 1;
                let c = unescape(bytes[i] as char);
                i += 1;
                c
            } else {
                let c = src[i..].chars().next().unwrap();
                i += c.len_utf8();
                c
            };
            if i >= bytes.len() || bytes[i] != b'\'' {
                return Err(format!("javars: unterminated char literal on line {line}"));
            }
            i += 1;
            out.push(Token {
                kind: Tok::Char(ch),
                line,
            });
            continue;
        }

        // operators & punctuation (longest match first)
        let two = if i + 1 < bytes.len() {
            &src[i..i + 2]
        } else {
            ""
        };
        let three = src.get(i..i + 3).unwrap_or("");
        let four = src.get(i..i + 4).unwrap_or("");
        let (kind, adv) = match four {
            ">>>=" => (Tok::UshrAssign, 4),
            _ => match three {
                ">>>" => (Tok::Ushr, 3),
                ">>=" => (Tok::ShrAssign, 3),
                "<<=" => (Tok::ShlAssign, 3),
                _ => lex_short(two, c, line)?,
            },
        };
        out.push(Token { kind, line });
        i += adv;
    }

    out.push(Token {
        kind: Tok::Eof,
        line,
    });
    Ok(out)
}

/// Match the one- and two-character operators and punctuation.
///
/// Split out of [`lex`] so the three- and four-character shift-assignment forms
/// can be tried first without nesting the whole table another level deep.
fn lex_short(two: &str, c: char, line: u32) -> Result<(Tok, usize), String> {
    Ok(match two {
        "+=" => (Tok::PlusAssign, 2),
        "-=" => (Tok::MinusAssign, 2),
        "*=" => (Tok::StarAssign, 2),
        "/=" => (Tok::SlashAssign, 2),
        "%=" => (Tok::PercentAssign, 2),
        "++" => (Tok::PlusPlus, 2),
        "--" => (Tok::MinusMinus, 2),
        "==" => (Tok::EqEq, 2),
        "!=" => (Tok::NotEq, 2),
        "<=" => (Tok::Le, 2),
        ">=" => (Tok::Ge, 2),
        "&&" => (Tok::AndAnd, 2),
        "||" => (Tok::OrOr, 2),
        "&=" => (Tok::AmpAssign, 2),
        "|=" => (Tok::PipeAssign, 2),
        "^=" => (Tok::CaretAssign, 2),
        "<<" => (Tok::Shl, 2),
        // `>>` is lexed here rather than being left as two `Gt`s, so the
        // shift is a single token; the generic-argument skippers ask
        // [`Tok::generic_closers`] instead of matching `Gt`, which is what
        // keeps `List<List<String>>` parsing.
        ">>" => (Tok::Shr, 2),
        // `->` cannot collide with `-` followed by `>`: Java has no prefix
        // `>`, so a `-`/`>` adjacency is only ever the arrow. `::` likewise
        // cannot collide with the ternary/label `:`.
        "->" => (Tok::Arrow, 2),
        "::" => (Tok::ColonColon, 2),
        _ => match c {
            '{' => (Tok::LBrace, 1),
            '}' => (Tok::RBrace, 1),
            '(' => (Tok::LParen, 1),
            ')' => (Tok::RParen, 1),
            '[' => (Tok::LBracket, 1),
            ']' => (Tok::RBracket, 1),
            ';' => (Tok::Semi, 1),
            ',' => (Tok::Comma, 1),
            '.' => (Tok::Dot, 1),
            '?' => (Tok::Question, 1),
            ':' => (Tok::Colon, 1),
            '=' => (Tok::Assign, 1),
            '+' => (Tok::Plus, 1),
            '-' => (Tok::Minus, 1),
            '*' => (Tok::Star, 1),
            '/' => (Tok::Slash, 1),
            '%' => (Tok::Percent, 1),
            '<' => (Tok::Lt, 1),
            '>' => (Tok::Gt, 1),
            '!' => (Tok::Not, 1),
            '&' => (Tok::Amp, 1),
            '|' => (Tok::Pipe, 1),
            '^' => (Tok::Caret, 1),
            '~' => (Tok::Tilde, 1),
            other => {
                return Err(format!(
                    "javars: unexpected character `{other}` on line {line}"
                ))
            }
        },
    })
}

fn keyword_or_ident(word: &str) -> Tok {
    match word {
        "class" => Tok::Class,
        "public" => Tok::Public,
        "static" => Tok::Static,
        "void" => Tok::Void,
        "if" => Tok::If,
        "else" => Tok::Else,
        "while" => Tok::While,
        "for" => Tok::For,
        "do" => Tok::Do,
        "switch" => Tok::Switch,
        "case" => Tok::Case,
        "default" => Tok::Default,
        "return" => Tok::Return,
        "break" => Tok::Break,
        "continue" => Tok::Continue,
        "true" => Tok::True,
        "false" => Tok::False,
        "new" => Tok::New,
        _ => Tok::Ident(word.to_string()),
    }
}

fn unescape(c: char) -> char {
    match c {
        'n' => '\n',
        't' => '\t',
        'r' => '\r',
        '0' => '\0',
        '\\' => '\\',
        '"' => '"',
        '\'' => '\'',
        other => other,
    }
}
