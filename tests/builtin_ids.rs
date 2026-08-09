//! The builtin-ID space is a hand-assigned integer namespace, and nothing in
//! Rust protects it.
//!
//! Every host builtin is a `pub const NAME: u16 = <n>;` in `src/host.rs`, and
//! `VM::register_builtin(id, f)` is last-write-wins. Two people adding a builtin
//! on separate branches both reach for "the next number", the two files merge
//! with no conflict marker because the lines do not touch, and the second
//! registration silently replaces the first handler. The call sites of the
//! displaced builtin then run the *other* builtin's body — no error, no warning,
//! a wrong answer at runtime.
//!
//! That exact defect has shipped twice in sibling frontends (`MAKE_ORDERING` and
//! `MAKE_QUEUE` both at 754; `INDEX_ISSET` colliding with `LIST_ELEM_GET` at
//! 105). This test is the guard: it reads the constants out of the source text —
//! not out of the compiled crate, where a duplicate is perfectly legal — and
//! fails on a repeated number, a repeated name, or a builtin registered twice.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// Every `.rs` file under `src/`, recursively.
fn sources() -> Vec<(PathBuf, String)> {
    fn walk(dir: &std::path::Path, out: &mut Vec<(PathBuf, String)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                let text = std::fs::read_to_string(&p).expect("read source");
                out.push((p, text));
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

/// The `pub const NAME: u16 = <n>;` declarations, as (name, id, file:line).
fn declared_ids() -> Vec<(String, u16, String)> {
    let mut out = Vec::new();
    for (path, text) in sources() {
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            let Some(rest) = line.strip_prefix("pub const ") else {
                continue;
            };
            let Some((name, value)) = rest.split_once(": u16 = ") else {
                continue;
            };
            let Some(digits) = value.strip_suffix(';') else {
                continue;
            };
            let Ok(id) = digits.trim().parse::<u16>() else {
                continue;
            };
            out.push((
                name.trim().to_string(),
                id,
                format!("{}:{}", path.display(), i + 1),
            ));
        }
    }
    assert!(
        out.len() > 30,
        "expected the whole builtin table to be found, got {} constants — the \
         declaration syntax this test scans for must have changed",
        out.len()
    );
    out
}

#[test]
fn no_two_builtins_share_an_id() {
    let mut by_id: BTreeMap<u16, Vec<(String, String)>> = BTreeMap::new();
    for (name, id, at) in declared_ids() {
        by_id.entry(id).or_default().push((name, at));
    }
    let collisions: Vec<String> = by_id
        .iter()
        .filter(|(_, v)| v.len() > 1)
        .map(|(id, v)| {
            let who: Vec<String> = v.iter().map(|(n, at)| format!("{n} ({at})")).collect();
            format!("  id {id} is claimed by {}", who.join(" and "))
        })
        .collect();
    assert!(
        collisions.is_empty(),
        "builtin IDs must be unique — `register_builtin` keeps only the last \
         handler registered for an ID, so the earlier one is silently \
         unreachable:\n{}",
        collisions.join("\n")
    );
}

#[test]
fn no_builtin_name_is_declared_twice() {
    let mut by_name: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, id, at) in declared_ids() {
        by_name
            .entry(name)
            .or_default()
            .push(format!("{id} at {at}"));
    }
    let dups: Vec<String> = by_name
        .iter()
        .filter(|(_, v)| v.len() > 1)
        .map(|(n, v)| format!("  {n}: {}", v.join(", ")))
        .collect();
    assert!(
        dups.is_empty(),
        "a builtin constant is declared more than once:\n{}",
        dups.join("\n")
    );
}

#[test]
fn every_builtin_is_registered_exactly_once() {
    let host = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("host.rs"),
    )
    .expect("read src/host.rs");
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for line in host.lines() {
        let Some(rest) = line.trim().strip_prefix("vm.register_builtin(") else {
            continue;
        };
        let Some((id, _)) = rest.split_once(',') else {
            continue;
        };
        *counts.entry(id.trim().to_string()).or_default() += 1;
    }
    let twice: Vec<String> = counts
        .iter()
        .filter(|(_, n)| **n > 1)
        .map(|(id, n)| format!("  {id} registered {n} times"))
        .collect();
    assert!(
        twice.is_empty(),
        "a builtin is registered more than once — the later handler wins:\n{}",
        twice.join("\n")
    );

    // Every declared ID must have a handler. An unregistered one is a call site
    // that dispatches into nothing.
    let declared: Vec<String> = declared_ids().into_iter().map(|(n, _, _)| n).collect();
    let missing: Vec<&String> = declared
        .iter()
        .filter(|n| n.starts_with('J') || *n == "DBG_LINE")
        .filter(|n| !counts.contains_key(*n))
        .collect();
    assert!(
        missing.is_empty(),
        "declared but never registered: {missing:?}"
    );
}
