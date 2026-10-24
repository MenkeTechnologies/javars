//! `Character.getName`, from the names `scripts/GenUnicodeTables.java` dumps
//! out of the JDK into `src/data/uninames.bin` (its header describes the
//! format). A code point with no name of its own there is named as the JDK
//! names it: its block, then its code point in upper-case hex.

use crate::unicode_tables::{lookup, BLOCKS, GENERAL_CATEGORY};
use std::sync::OnceLock;

static BLOB: &[u8] = include_bytes!("data/uninames.bin");

/// The decoded names: the word dictionary, and each name as its code point
/// and word indexes, sorted by code point.
struct Names {
    words: Vec<&'static str>,
    entries: Vec<(u32, Vec<u16>)>,
}

fn names() -> &'static Names {
    static NAMES: OnceLock<Names> = OnceLock::new();
    NAMES.get_or_init(|| {
        let mut at = 0usize;
        let mut take = |n: usize| {
            let s = &BLOB[at..at + n];
            at += n;
            s
        };
        let u32_at = |b: &[u8]| u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        let nwords = u32_at(take(4)) as usize;
        let mut words = Vec::with_capacity(nwords);
        for _ in 0..nwords {
            let len = take(1)[0] as usize;
            words.push(std::str::from_utf8(take(len)).expect("generated names are UTF-8"));
        }
        let nnames = u32_at(take(4)) as usize;
        let mut entries = Vec::with_capacity(nnames);
        for _ in 0..nnames {
            let c = take(3);
            let cp = u32::from(c[0]) << 16 | u32::from(c[1]) << 8 | u32::from(c[2]);
            let n = take(1)[0] as usize;
            let ids = (0..n)
                .map(|_| {
                    let b = take(2);
                    u16::from_be_bytes([b[0], b[1]])
                })
                .collect();
            entries.push((cp, ids));
        }
        Names { words, entries }
    })
}

/// `Character.getName(cp)`: `None` (Java's `null`) for an unassigned code
/// point, else its Unicode name, or the block-and-hex name the JDK gives one
/// that has none of its own (`CJK UNIFIED IDEOGRAPHS 4E00`, or the bare hex
/// outside every block).
pub fn name(cp: u32) -> Option<String> {
    if cp > 0x10_FFFF || lookup(GENERAL_CATEGORY, cp, 0) == 0 {
        return None;
    }
    let n = names();
    if let Ok(i) = n.entries.binary_search_by_key(&cp, |(c, _)| *c) {
        let ids = &n.entries[i].1;
        let parts: Vec<&str> = ids.iter().map(|&w| n.words[w as usize]).collect();
        return Some(parts.join(" "));
    }
    Some(match lookup(BLOCKS, cp, "") {
        "" => format!("{cp:X}"),
        block => format!("{block} {cp:X}"),
    })
}

/// `Character.codePointOf(name)`: the code point a Unicode name names,
/// matched as the JDK matches it — trimmed and upper-cased — against the
/// names [`name`] answers, either one of the table's or the block-and-hex
/// form (`CJK UNIFIED IDEOGRAPHS 4E00`). `None` where the JDK throws.
pub fn code_point_of(name: &str) -> Option<u32> {
    static BY_NAME: OnceLock<std::collections::HashMap<String, u32>> = OnceLock::new();
    let key = name.trim_matches(|c: char| c <= ' ').to_uppercase();
    let n = names();
    let by_name = BY_NAME.get_or_init(|| {
        n.entries
            .iter()
            .map(|(cp, ids)| {
                let parts: Vec<&str> = ids.iter().map(|&w| n.words[w as usize]).collect();
                (parts.join(" "), *cp)
            })
            .collect()
    });
    if let Some(&cp) = by_name.get(&key) {
        return Some(cp);
    }
    let (_, hex) = key.rsplit_once(' ')?;
    let cp = u32::from_str_radix(hex, 16).ok()?;
    (name_of_valid(cp).as_deref() == Some(key.as_str())).then_some(cp)
}

/// [`name`] for a code point `Character.isValidCodePoint` accepts.
fn name_of_valid(cp: u32) -> Option<String> {
    (cp <= 0x10_FFFF).then(|| name(cp)).flatten()
}
