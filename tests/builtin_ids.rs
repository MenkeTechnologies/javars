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
    // A floor on files *scanned*. `!out.is_empty()` is satisfied by finding one
    // file, so a `walk` that stopped recursing — or a `src/` layout change that
    // moved the builtin table into a subdirectory the walk missed — would leave
    // every collision test below scanning almost nothing and reporting success.
    assert!(
        out.len() >= 15,
        "expected the whole `src/` tree to be scanned, found {} .rs file(s) — \
         a partial walk makes every duplicate check below vacuous",
        out.len()
    );
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
        out.len() >= 44,
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
    // A floor on registrations *seen*. The duplicate check below is a filter
    // over `counts`, so it reports "no duplicates" just as confidently when
    // `counts` is empty — which is what a reformat that split
    // `vm.register_builtin(` across two lines would produce, since the scan is
    // a line-prefix match. The `missing` check further down would catch a
    // total failure, but not a partial one, and neither would name the cause.
    assert!(
        counts.len() >= 44,
        "expected every `vm.register_builtin(` call to be found, saw {} — the \
         call syntax this test scans for must have changed, and a scan that \
         finds nothing reports no duplicates",
        counts.len()
    );
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
    let builtins: Vec<&String> = declared
        .iter()
        .filter(|n| n.starts_with('J') || *n == "DBG_LINE")
        .collect();
    // A floor on the names that survive the `J`-prefix filter, not just on the
    // constants the scan found. The `declared_ids` floor above counts *every*
    // `pub const … : u16`, so it is satisfied whether or not any of them reach
    // the `missing` check — and `missing` is a filter over what is left, so it
    // reports "every builtin is registered" just as confidently when the
    // prefix matched nothing. Renaming the builtins off the `J` prefix, or
    // scanning a table that no longer uses it, would empty this silently.
    assert!(
        builtins.len() >= 44,
        "expected the `J`-prefixed builtin constants to be found, got {} of {} \
         declared — the check below is a filter over this list, so an empty one \
         reports no missing registrations",
        builtins.len(),
        declared.len()
    );
    let missing: Vec<&&String> = builtins
        .iter()
        .filter(|n| !counts.contains_key(**n))
        .collect();
    assert!(
        missing.is_empty(),
        "declared but never registered: {missing:?}"
    );
}
