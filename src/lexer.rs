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
    Float(f64),
    Str(String),
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
    Eof,
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

        // numbers (int or float)
        if c.is_ascii_digit() {
            let start = i;
            let mut is_float = false;
            while i < bytes.len() && (bytes[i] as char).is_ascii_digit() {
                i += 1;
            }
            if i < bytes.len()
                && bytes[i] == b'.'
                && i + 1 < bytes.len()
                && (bytes[i + 1] as char).is_ascii_digit()
            {
                is_float = true;
                i += 1;
                while i < bytes.len() && (bytes[i] as char).is_ascii_digit() {
                    i += 1;
                }
            }
            // integer/long/float/double suffixes are accepted and dropped
            if i < bytes.len() && matches!(bytes[i], b'L' | b'l' | b'f' | b'F' | b'd' | b'D') {
                if matches!(bytes[i], b'f' | b'F' | b'd' | b'D') {
                    is_float = true;
                }
                i += 1;
            }
            let text = src[start..i].trim_end_matches(|ch: char| ch.is_ascii_alphabetic());
            if is_float {
                let v: f64 = text
                    .parse()
                    .map_err(|_| format!("javars: bad float literal `{text}` on line {line}"))?;
                out.push(Token {
                    kind: Tok::Float(v),
                    line,
                });
            } else {
                let v: i64 = text
                    .parse()
                    .map_err(|_| format!("javars: bad integer literal `{text}` on line {line}"))?;
                out.push(Token {
                    kind: Tok::Int(v),
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

        // char literals — modeled as a one-char string (slice 1)
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
                kind: Tok::Str(ch.to_string()),
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
        let (kind, adv) = match two {
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
                '=' => (Tok::Assign, 1),
                '+' => (Tok::Plus, 1),
                '-' => (Tok::Minus, 1),
                '*' => (Tok::Star, 1),
                '/' => (Tok::Slash, 1),
                '%' => (Tok::Percent, 1),
                '<' => (Tok::Lt, 1),
                '>' => (Tok::Gt, 1),
                '!' => (Tok::Not, 1),
                other => {
                    return Err(format!(
                        "javars: unexpected character `{other}` on line {line}"
                    ))
                }
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
