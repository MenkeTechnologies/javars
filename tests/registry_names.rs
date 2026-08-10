//! The same last-write-wins hazard as `builtin_ids.rs`, one namespace over: the
//! registries javars keys by **name** rather than by integer id.
//!
//! `builtin_ids.rs` guards the `pub const NAME: u16` space, where a repeated
//! number makes `VM::register_builtin` drop the earlier handler. The name-keyed
//! tables have the identical defect and no compiler help at all:
//!
//!   * `prelude::THROWABLES` / `prelude::FUNCTIONAL` are plain data. A name
//!     listed twice makes [`crate::prelude`] emit the type **twice** into the
//!     injected compilation unit; the class table (`compiler.rs` `out.insert`)
//!     and the supertype map (`lib.rs` `supertype_map`, a `HashMap` `collect`)
//!     both keep the *last* one. So the second entry's `extends` silently
//!     replaces the first's, and a `catch` clause that used to match stops
//!     matching. Nothing in Rust objects: a duplicate tuple in a `const` array
//!     is not an error, not a warning, not even a lint.
//!
//!   * `reference::REFERENCE` is the corpus behind LSP completion, LSP hover,
//!     and the published `docs/reference.html`. A repeated
//!     `(name, chapter, signature)` is a duplicated completion item and a
//!     doubled hover section — the same entry twice. (Repeated `(name, chapter)`
//!     with *different* signatures is the deliberate representation of an
//!     overload, so the key is the triple.)
//!
//!   * every name-keyed `match` in `src/` — `host.rs` `string_method`,
//!     `list_method`, `map_method`, `set_method`, `static_method`,
//!     `collection_static`, `jdk_supers`; `compiler.rs` `static_call_java_type`,
//!     `wrapper_constant`, `collection_kind`; `lexer.rs` `keyword_or_ident` — is
//!     a dispatch table keyed by a string. Rust's `unreachable_patterns` lint
//!     covers *some* of these, but it is a warning: `cargo build` still exits 0
//!     and still ships the shadowed arm.
//!
//! A duplicate in any of them builds clean and fails at runtime somewhere that
//! names an unrelated feature. This test reads the registries out of the source
//! *text*, the way `builtin_ids.rs` reads the id constants, and fails on a
//! repeated key.

use std::collections::BTreeMap;
use std::path::PathBuf;

// ── source scanning ────────────────────────────────────────────────────────

/// A source file prepared for structural scanning.
struct Scan {
    path: String,
    /// The original text, for reading string-literal contents back out.
    src: String,
    /// `src` with every comment byte blanked and every string/char literal
    /// *body* replaced by `x`, byte-for-byte the same length. Brace, paren and
    /// `=>` scanning runs over this, so a `{` inside a doc comment or a `"` in
    /// an example string can never be mistaken for structure.
    code: String,
}

impl Scan {
    fn new(path: &std::path::Path, src: String) -> Scan {
        let b = src.as_bytes();
        let mut out = b.to_vec();
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                b'/' if i + 1 < b.len() && b[i + 1] == b'/' => {
                    while i < b.len() && b[i] != b'\n' {
                        out[i] = b' ';
                        i += 1;
                    }
                }
                b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                    let mut depth = 0usize;
                    while i < b.len() {
                        if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
                            depth += 1;
                            out[i] = b' ';
                            out[i + 1] = b' ';
                            i += 2;
                        } else if b[i] == b'*' && i + 1 < b.len() && b[i + 1] == b'/' {
                            depth -= 1;
                            out[i] = b' ';
                            out[i + 1] = b' ';
                            i += 2;
                            if depth == 0 {
                                break;
                            }
                        } else {
                            if b[i] != b'\n' {
                                out[i] = b' ';
                            }
                            i += 1;
                        }
                    }
                }
                // A raw string: `r`, some `#`s, then the body up to the closing
                // quote followed by the same number of `#`s.
                b'r' if b[i + 1..].iter().take_while(|c| **c == b'#').count() + i + 1 < b.len()
                    && b[i + 1 + b[i + 1..].iter().take_while(|c| **c == b'#').count()] == b'"'
                    && (i == 0 || !b[i - 1].is_ascii_alphanumeric() && b[i - 1] != b'_') =>
                {
                    let hashes = b[i + 1..].iter().take_while(|c| **c == b'#').count();
                    i += hashes + 2;
                    while i < b.len() {
                        if b[i] == b'"' && b[i + 1..].iter().take(hashes).all(|c| *c == b'#') {
                            i += hashes + 1;
                            break;
                        }
                        if b[i] != b'\n' {
                            out[i] = b'x';
                        }
                        i += 1;
                    }
                }
                b'"' => {
                    i += 1;
                    while i < b.len() && b[i] != b'"' {
                        if b[i] == b'\\' {
                            out[i] = b'x';
                            i += 1;
                        }
                        if i < b.len() {
                            if b[i] != b'\n' {
                                out[i] = b'x';
                            }
                            i += 1;
                        }
                    }
                    i += 1;
                }
                // `'a'` and `'\n'` are char literals; `'static` is a lifetime and
                // must stay visible as ordinary code.
                b'\'' if i + 2 < b.len() && (b[i + 1] == b'\\' || b[i + 2] == b'\'') => {
                    i += 1;
                    while i < b.len() && b[i] != b'\'' {
                        if b[i] == b'\\' {
                            out[i] = b'x';
                            i += 1;
                        }
                        if i < b.len() {
                            out[i] = b'x';
                            i += 1;
                        }
                    }
                    i += 1;
                }
                _ => i += 1,
            }
        }
        Scan {
            path: path.display().to_string(),
            src,
            code: String::from_utf8(out).expect("masking preserves UTF-8"),
        }
    }

    /// `file:line` for a byte offset.
    fn at(&self, off: usize) -> String {
        let line = self.src[..off].matches('\n').count() + 1;
        format!("{}:{}", self.path, line)
    }
}

/// Every `.rs` file under `src/`, recursively, masked for scanning.
fn sources() -> Vec<Scan> {
    fn walk(dir: &std::path::Path, out: &mut Vec<Scan>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        for p in paths {
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                let text = std::fs::read_to_string(&p).expect("read source");
                out.push(Scan::new(&p, text));
            }
        }
    }
    let mut out = Vec::new();
    walk(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut out,
    );
    assert!(!out.is_empty(), "no sources found under src/");
    out
}

/// The byte offsets of the delimiters bracketing the group that opens at
/// `open` (which must index an opening bracket in `code`).
fn matching(code: &str, open: usize) -> Option<usize> {
    let b = code.as_bytes();
    let mut depth = 0i32;
    for (i, c) in b.iter().enumerate().skip(open) {
        match c {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Split `s` on `sep` at bracket depth 0.
fn split_top(s: &str, sep: u8) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, c) in s.as_bytes().iter().enumerate() {
        match c {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            _ if *c == sep && depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// The contents of `s` when it is exactly one string literal, else `None`.
/// Read from the *original* text at the same offsets the masked copy reports.
fn as_string_literal(scan: &Scan, span: std::ops::Range<usize>) -> Option<String> {
    let masked = scan.code[span.clone()].trim();
    if masked.len() < 2 || !masked.starts_with('"') || !masked.ends_with('"') {
        return None;
    }
    // One literal, not two concatenated by something: the masked body is all `x`.
    if masked[1..masked.len() - 1].contains('"') {
        return None;
    }
    let lead = scan.code[span.clone()].len() - scan.code[span.clone()].trim_start().len();
    let start = span.start + lead;
    Some(scan.src[start + 1..start + masked.len() - 1].to_string())
}

// ── registry 1 & 2: the prelude's `const` tables ───────────────────────────

/// The first `k` string literals of every top-level tuple in the `const` array
/// named `name`, as (key, file:line).
fn const_table(scan: &Scan, name: &str, k: usize) -> Vec<(Vec<String>, String)> {
    let Some(decl) = scan.code.find(&format!("pub const {name}:")) else {
        return Vec::new();
    };
    // `= &[`, not the first `&[` — that one is inside the declared *type*
    // (`pub const THROWABLES: &[(&str, &str)] = &[…]`).
    let open = decl + scan.code[decl..].find("= &[").expect("const array literal") + 3;
    let close = matching(&scan.code, open).expect("unterminated const array");
    let mut out = Vec::new();
    let mut i = open + 1;
    while i < close {
        let c = scan.code.as_bytes()[i];
        if c == b'(' {
            let end = matching(&scan.code, i).expect("unterminated tuple");
            let fields = split_top(&scan.code[i + 1..end], b',');
            let mut key = Vec::new();
            let mut off = i + 1;
            for f in fields.iter().take(k) {
                if let Some(s) = as_string_literal(scan, off..off + f.len()) {
                    key.push(s);
                }
                off += f.len() + 1;
            }
            if key.len() == k {
                out.push((key, scan.at(i)));
            }
            i = end + 1;
        } else {
            i += 1;
        }
    }
    out
}

/// Report the keys that appear more than once.
fn duplicates(entries: &[(Vec<String>, String)]) -> Vec<String> {
    let mut by_key: BTreeMap<Vec<String>, Vec<String>> = BTreeMap::new();
    for (k, at) in entries {
        by_key.entry(k.clone()).or_default().push(at.clone());
    }
    by_key
        .iter()
        .filter(|(_, v)| v.len() > 1)
        .map(|(k, v)| format!("  `{}` declared at {}", k.join("`, `"), v.join(" and ")))
        .collect()
}

#[test]
fn no_prelude_type_is_declared_twice() {
    let scan = sources()
        .into_iter()
        .find(|s| s.path.ends_with("prelude.rs"))
        .expect("src/prelude.rs");
    let throwables = const_table(&scan, "THROWABLES", 1);
    let functional = const_table(&scan, "FUNCTIONAL", 1);
    assert!(
        throwables.len() >= 20 && functional.len() >= 19,
        "expected the whole prelude to be found, got {} throwables and {} \
         functional interfaces — the table syntax this test scans for must have \
         changed",
        throwables.len(),
        functional.len()
    );
    // Both tables feed the *same* injected compilation unit, so a name repeated
    // across them collides exactly as a name repeated within one does: a class
    // and an interface of the same name, and the class table keeps one.
    let all: Vec<(Vec<String>, String)> = throwables.into_iter().chain(functional).collect();
    let dups = duplicates(&all);
    assert!(
        dups.is_empty(),
        "a prelude type is declared more than once — `prelude_source`/\
         `functional_source` emit it twice, and the class table \
         (`compiler.rs` `out.insert`) and supertype map (`lib.rs` \
         `supertype_map`) both keep only the last, so the earlier entry's \
         `extends` is silently discarded and a `catch` that matched it stops \
         matching:\n{}",
        dups.join("\n")
    );
}

// ── registry 3: the language-reference corpus ──────────────────────────────

#[test]
fn no_reference_entry_is_declared_twice() {
    let scan = sources()
        .into_iter()
        .find(|s| s.path.ends_with("reference.rs"))
        .expect("src/reference.rs");
    // (name, chapter, signature). A repeated (name, chapter) pair is how an
    // overload is written — `substring(int)` and `substring(int, int)` are two
    // entries on purpose — so the signature is part of the key.
    let entries = const_table(&scan, "REFERENCE", 3);
    assert!(
        entries.len() > 300,
        "expected the whole reference corpus to be found, got {} entries — the \
         entry syntax this test scans for must have changed",
        entries.len()
    );
    let dups = duplicates(&entries);
    assert!(
        dups.is_empty(),
        "a reference entry is declared more than once — it becomes a duplicated \
         LSP completion item, a doubled hover section, and a repeated row in \
         `docs/reference.html`:\n{}",
        dups.join("\n")
    );
}

// ── registry 4: every name-keyed `match` dispatch table ────────────────────

/// One `match` block's name-keyed arms, as (key, file:line).
struct Block {
    at: String,
    arms: Vec<(Vec<String>, String)>,
}

/// Every `match` block in `scan`, with the arms whose pattern is built purely
/// out of string literals and simple literal tokens. An arm with a binding, a
/// guard, or any non-literal component is skipped: those are not table lookups
/// and a repeat of one is not necessarily a shadow.
fn match_blocks(scan: &Scan) -> Vec<Block> {
    let code = &scan.code;
    let b = code.as_bytes();
    let mut out = Vec::new();
    let mut search = 0;
    while let Some(rel) = code[search..].find("match ") {
        let kw = search + rel;
        search = kw + 6;
        // A word-boundary `match`, not the tail of `rematch`.
        if kw > 0 && (b[kw - 1].is_ascii_alphanumeric() || b[kw - 1] == b'_') {
            continue;
        }
        // The scrutinee runs to the `{` that opens the arm list.
        let mut i = kw + 6;
        let mut depth = 0i32;
        let body = loop {
            if i >= b.len() {
                break None;
            }
            match b[i] {
                b'(' | b'[' => depth += 1,
                b')' | b']' => depth -= 1,
                b'{' if depth == 0 => break Some(i),
                b';' if depth == 0 => break None,
                _ => {}
            }
            i += 1;
        };
        let Some(body) = body else { continue };
        let Some(end) = matching(code, body) else {
            continue;
        };
        // Walk the arm list line by line. rustfmt puts every arm pattern at the
        // start of a line, and an arm pattern is the only thing that can carry a
        // `=>` on a line that begins at the arm list's own bracket depth — a
        // continuation line or a nested `match`'s arm begins deeper.
        let mut arms = Vec::new();
        let mut depth = 0i32;
        let mut pos = body + 1;
        while pos <= end {
            let nl = code[pos..end + 1]
                .find('\n')
                .map_or(end + 1, |n| pos + n + 1);
            let line = &code[pos..nl.min(end + 1)];
            if depth == 0 {
                if let Some(arrow) = find_top_arrow(line) {
                    if let Some(keys) = arm_keys(scan, pos..pos + arrow) {
                        for k in keys {
                            arms.push((k, scan.at(pos)));
                        }
                    }
                }
            }
            for c in line.bytes() {
                match c {
                    b'(' | b'[' | b'{' => depth += 1,
                    b')' | b']' | b'}' => depth -= 1,
                    _ => {}
                }
            }
            pos = nl;
        }
        if !arms.is_empty() {
            out.push(Block {
                at: scan.at(body),
                arms,
            });
        }
    }
    out
}

/// The byte offset of this line's `=>` at bracket depth 0, if any.
fn find_top_arrow(line: &str) -> Option<usize> {
    let b = line.as_bytes();
    let mut depth = 0i32;
    for i in 0..b.len().saturating_sub(1) {
        match b[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'=' if depth == 0 && b[i + 1] == b'>' => return Some(i),
            _ => {}
        }
    }
    None
}

/// The lookup keys an arm pattern claims, or `None` when the pattern is not a
/// literal table key. Or-patterns are expanded, at the top level
/// (`("a", 0) | ("b", 1)`) and inside a tuple field (`("a" | "b", 0)`) alike.
fn arm_keys(scan: &Scan, span: std::ops::Range<usize>) -> Option<Vec<Vec<String>>> {
    let text = &scan.code[span.clone()];
    // A guarded arm is disambiguated by its guard, not by its key.
    if text.contains(" if ") {
        return None;
    }
    let mut out = Vec::new();
    let mut off = span.start;
    for alt in split_top(text, b'|') {
        let trimmed = alt.trim();
        let lead = alt.len() - alt.trim_start().len();
        let start = off + lead;
        off += alt.len() + 1;
        if trimmed.starts_with('(') {
            let inner_start = start + 1;
            let inner = &trimmed[1..trimmed.len() - 1];
            if !trimmed.ends_with(')') {
                return None;
            }
            let mut fields: Vec<Vec<String>> = Vec::new();
            let mut foff = inner_start;
            let mut any_string = false;
            for f in split_top(inner, b',') {
                if f.trim().is_empty() {
                    foff += f.len() + 1;
                    continue;
                }
                let mut choices = Vec::new();
                let mut coff = foff;
                for c in split_top(f, b'|') {
                    let ct = c.trim();
                    if let Some(s) = as_string_literal(scan, coff..coff + c.len()) {
                        any_string = true;
                        choices.push(format!("{s:?}"));
                    } else if !ct.is_empty()
                        && ct
                            .bytes()
                            .all(|x| x.is_ascii_alphanumeric() || x == b'_' || x == b':')
                    {
                        choices.push(ct.to_string());
                    } else {
                        return None;
                    }
                    coff += c.len() + 1;
                }
                fields.push(choices);
                foff += f.len() + 1;
            }
            if !any_string {
                return None;
            }
            // Cross-product of the per-field alternatives.
            let mut rows: Vec<Vec<String>> = vec![Vec::new()];
            for field in fields {
                rows = rows
                    .iter()
                    .flat_map(|r| {
                        field.iter().map(move |c| {
                            let mut r = r.clone();
                            r.push(c.clone());
                            r
                        })
                    })
                    .collect();
            }
            out.extend(rows);
        } else {
            let s = as_string_literal(scan, start..start + trimmed.len())?;
            out.push(vec![format!("{s:?}")]);
        }
    }
    (!out.is_empty()).then_some(out)
}

#[test]
fn no_dispatch_arm_key_is_matched_twice() {
    let mut total = 0usize;
    let mut wide = 0usize;
    let mut failures = Vec::new();
    for scan in sources() {
        for block in match_blocks(&scan) {
            total += block.arms.len();
            if block.arms.len() >= 5 {
                wide += 1;
            }
            let dups = duplicates(&block.arms);
            if !dups.is_empty() {
                failures.push(format!(
                    "in the `match` at {}:\n{}",
                    block.at,
                    dups.join("\n")
                ));
            }
        }
    }
    assert!(
        total > 400 && wide >= 15,
        "expected every name-keyed dispatch table to be found, got {total} arms \
         across {wide} tables of 5+ arms — the arm syntax this test scans for \
         must have changed"
    );
    assert!(
        failures.is_empty(),
        "a dispatch key is matched twice in one `match` — the first arm wins and \
         the second is dead code, so the method/class/keyword it was added for \
         runs the *other* arm's body:\n{}",
        failures.join("\n")
    );
}
