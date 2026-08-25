//! Frozen differential-parity replay.
//!
//! `tests/data/parity_expected.txt` holds a curated corpus of Java programs and
//! the stdout each produces, captured byte-for-byte from a real JDK during
//! authoring (the same programs the `parity-fuzz` binary diffs live). This test
//! replays every program through the built `java` frontend and asserts the
//! frozen output, so the parity-critical behaviours — `int`-vs-`double`
//! division, IEEE division by zero, `Double.toString` notation, exponent
//! literals, `+` concatenation, `String` methods, `Math` overload typing,
//! `String.format` — stay locked WITHOUT a JDK installed. CI runs this; the live
//! `parity-fuzz` differential harness is a developer tool.
//!
//! Format: one record per line, `program<TAB>expected`, with `\n` in EITHER
//! field encoded as the two characters backslash-n.
//!
//! The program field is decoded the same way `scripts/capture-parity.sh` decodes
//! it before handing the source to `javac`. It used to be replayed verbatim,
//! which silently limited the corpus to one-line programs: a captured
//! multi-line record was written to `T.java` with the two characters `\n` still
//! in it, `javars` reported a lex error, and the record failed as a
//! "frontend failed" divergence that no frontend change could fix.

use std::process::Command;

/// Run one source program through the built frontend, returning its stdout.
///
/// The JDK's single-file source launcher requires the file to be named for its
/// public class, so each case gets its own directory holding a `T.java`.
fn run(src: &str) -> (String, bool) {
    let dir = std::env::temp_dir().join(format!("javars_parity_{}", fnv1a(src)));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("T.java");
    std::fs::write(&path, src).expect("write program");
    let out = Command::new(env!("CARGO_BIN_EXE_java"))
        .arg(&path)
        .current_dir(&dir)
        .output()
        .expect("spawn java");
    let _ = std::fs::remove_dir_all(&dir);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.success(),
    )
}

fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[test]
fn frozen_corpus_matches_reference_java() {
    let data = include_str!("data/parity_expected.txt");
    let mut n = 0;
    let mut failures = Vec::new();

    for (i, line) in data.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let (prog_enc, expected_enc) = line
            .split_once('\t')
            .unwrap_or_else(|| panic!("line {}: missing TAB separator", i + 1));
        let prog = prog_enc.replace("\\n", "\n");
        let expected = expected_enc.replace("\\n", "\n");

        let (got, ok) = run(&prog);
        if !ok {
            failures.push(format!(
                "line {}: frontend failed\n  program: {prog}",
                i + 1
            ));
        } else if got != expected {
            failures.push(format!(
                "line {}: output mismatch\n  program:  {prog}\n  expected: {:?}\n  got:      {:?}",
                i + 1,
                expected,
                got
            ));
        }
        n += 1;
    }

    // A floor on records *examined*, not merely on the corpus being non-empty.
    // `n > 0` passes on a corpus a bad merge truncated to one line, and passes
    // just as happily if the `include_str!` target is replaced by a stub — the
    // whole test then reports success while checking almost nothing. The real
    // corpus is 622 records and only ever grows (the capture script appends;
    // it never rewrites), so a floor near the current size fails loudly on any
    // truncation while leaving room for the handful a deletion might justify.
    assert!(
        n >= 617,
        "the frozen corpus should hold at least 617 records, found {n} — a \
         truncated corpus still passes every case it kept, so the count is \
         asserted rather than the emptiness"
    );
    assert!(
        failures.is_empty(),
        "{} of {n} frozen parity case(s) diverged from the reference JDK:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}
