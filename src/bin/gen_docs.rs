//! Offline generator for `docs/reference.html` — the full language reference,
//! rendered with the same cyberpunk HUD chrome as `docs/index.html`. Run before
//! publishing GitHub Pages:
//!
//! ```sh
//! cargo run --bin gen-docs
//! ```
//!
//! Source of truth: `javars::reference` (`corpus()`), the exact
//! `(name, chapter, signature, doc, example)` table the editor completion/hover
//! path renders from. The static page and the language server therefore never
//! drift — a name is documented here only if the runtime actually implements it,
//! and each chapter of the corpus names the source table it was read from
//! (`lexer.rs` for the reserved words, `parser.rs` for the contextual ones,
//! `prelude.rs` for the throwables and functional interfaces, `host.rs` for the
//! builtins and the library surface, `compiler.rs` for the rest).

use std::collections::BTreeSet;
use std::fmt::Write as _;

fn main() {
    let corpus = javars::reference::corpus();
    let chapters: BTreeSet<&str> = corpus.iter().map(|(_, c, _, _, _)| *c).collect();

    let page = format!(
        "{head}{body}{foot}",
        head = HEAD,
        body = build_body(corpus),
        foot = FOOT,
    )
    // Stamp the current crate version so the page never falls behind Cargo.toml.
    .replace("__JAVARS_VERSION__", env!("CARGO_PKG_VERSION"));

    let out = "docs/reference.html";
    if let Err(e) = std::fs::write(out, page) {
        eprintln!("gen-docs: cannot write {out}: {e}");
        std::process::exit(1);
    }
    println!(
        "wrote {out} ({} entries, {} chapters)",
        corpus.len(),
        chapters.len()
    );
}

/// A reference-corpus entry: (name, chapter, signature, doc, example).
type CEntry = javars::reference::Entry;

/// Render one `<section>` per chapter (first-seen order), each holding one
/// `<article class="doc-entry">` per name: an anchored heading, the signature in
/// a fenced code block, the description, and — when the entry carries one — a
/// runnable usage example.
fn build_body(corpus: &[CEntry]) -> String {
    let mut chapters: Vec<(&str, Vec<&CEntry>)> = Vec::new();
    for entry in corpus {
        let chapter = entry.1;
        match chapters.iter_mut().find(|(c, _)| *c == chapter) {
            Some((_, entries)) => entries.push(entry),
            None => chapters.push((chapter, vec![entry])),
        }
    }

    let mut out = String::new();
    for (chapter, entries) in &chapters {
        let _ = write!(
            out,
            "\n      <section class=\"tutorial-section\" id=\"ch-{slug}\">\n\
             \x20       <h2>{title}</h2>\n",
            slug = slugify(chapter),
            title = html_escape(chapter),
        );
        for (idx, (name, _chapter, sig, doc, example)) in entries.iter().enumerate() {
            let anchor = format!("doc-{}-{}", slugify(chapter), idx + 1);
            let _ = write!(
                out,
                "        <article class=\"doc-entry\" id=\"{anchor}\">\n\
                 \x20         <h3><a class=\"doc-anchor\" href=\"#{anchor}\">#</a> <code>{name}</code></h3>\n\
                 \x20         <pre><code class=\"lang-java\">{sig}</code></pre>\n\
                 \x20         <p>{doc}</p>\n",
                anchor = anchor,
                name = html_escape(name),
                sig = html_escape(sig),
                doc = prose(doc),
            );
            if !example.is_empty() {
                let _ = writeln!(
                    out,
                    "          <pre><code class=\"lang-java\">{example}</code></pre>",
                    example = html_escape(example),
                );
            }
            out.push_str("        </article>\n");
        }
        out.push_str("      </section>\n");
    }
    out
}

/// Render a description: HTML-escape it, then turn each pair of backticks into
/// an inline `<code>` span. The corpus writes identifiers in Markdown backticks
/// because the LSP hover renders Markdown; the HTML page (and the LaTeX the PDF
/// pipeline derives from it) needs real markup instead of literal backticks. An
/// unpaired trailing backtick is left as text.
fn prose(s: &str) -> String {
    let escaped = html_escape(s);
    let parts: Vec<&str> = escaped.split('`').collect();
    // An odd backtick count means the entry is malformed; leave every backtick
    // as text rather than emitting an unbalanced tag.
    if parts.len() % 2 == 0 {
        return escaped;
    }
    let mut out = String::new();
    for (i, part) in parts.iter().enumerate() {
        // Odd-indexed segments are the ones between a pair of backticks.
        if i % 2 == 1 {
            let _ = write!(out, "<code>{part}</code>");
        } else {
            out.push_str(part);
        }
    }
    out
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Lowercase, non-alphanumeric runs collapsed to a single `-`, edges trimmed.
fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

const HEAD: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta name="color-scheme" content="dark light">
  <meta name="description" content="javars — Language reference. Every keyword, operator, type, library method, throwable, functional interface, and runtime builtin the current javars build implements. MIT licensed.">
  <title>javars &mdash; Language Reference</title>
  <link rel="preconnect" href="https://fonts.googleapis.com">
  <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
  <link href="https://fonts.googleapis.com/css2?family=Orbitron:wght@400;600;700;900&family=Share+Tech+Mono&display=swap" rel="stylesheet">
  <link rel="stylesheet" href="hud-static.css">
  <link rel="stylesheet" href="tutorial.css">
  <style>
    .tutorial-main { max-width: 76rem; }
    .section-rule { border:none;border-top:1px dashed var(--border);margin:2rem 0; }
    .hub-scheme-strip { border-bottom:1px dashed var(--border);background:color-mix(in srgb, var(--bg-secondary) 85%, transparent);padding:0.55rem 1.5rem 0.65rem;position:relative; }
    .hub-scheme-strip-inner { max-width:76rem;margin:0 auto;display:flex;align-items:center;gap:0.85rem; }
    .hub-scheme-strip .hud-scheme-label { flex:0 0 auto;font-family:'Orbitron',sans-serif;font-size:9px;font-weight:700;letter-spacing:2px;text-transform:uppercase;color:var(--accent);text-align:left; }
    .hub-scheme-strip .scheme-grid { flex:1 1 auto;display:grid;grid-template-columns:repeat(5,minmax(0,1fr));gap:6px; }
    @media (max-width:720px){ .hub-scheme-strip-inner{flex-direction:column;align-items:stretch}.hub-scheme-strip .scheme-grid{grid-template-columns:repeat(2,minmax(0,1fr))} }
    .docs-build-line { margin:0.35rem 0 0;font-family:'Share Tech Mono',ui-monospace,monospace;font-size:11px;color:var(--text-dim);letter-spacing:0.03em;max-width:52rem;opacity:0.75; }
  </style>
</head>
<body>
  <div class="app tutorial-app" id="docsApp">
    <div class="crt-scanline" id="crtH" aria-hidden="true"></div>
    <div class="crt-scanline-v" id="crtV" aria-hidden="true"></div>

    <header class="tutorial-header">
      <div class="tutorial-header-inner">
        <div>
          <h1 class="tutorial-brand">// JAVARS — LANGUAGE REFERENCE</h1>
          <nav class="tutorial-crumbs" aria-label="Breadcrumb">
            <a href="index.html">Docs</a>
            <span class="sep">/</span>
            <span class="current">Language Reference</span>
            <span class="sep">/</span>
            <a href="https://github.com/MenkeTechnologies/javars" target="_blank" rel="noopener noreferrer">GitHub</a>
          </nav>
          <p class="docs-build-line">javars v__JAVARS_VERSION__ · Java on fusevm · lex/parse → AST → bytecode → Cranelift JIT · no bespoke VM · no JVM · MIT · in active development</p>
        </div>
        <div class="tutorial-toolbar">
          <button type="button" class="btn btn-secondary" id="btnTheme" title="Toggle light/dark">Theme</button>
          <button type="button" class="btn btn-secondary active" id="btnCrt" title="CRT scanline overlay">CRT</button>
          <button type="button" class="btn btn-secondary active" id="btnNeon" title="Neon border pulse">Neon</button>
          <a class="btn btn-secondary" href="index.html">Docs</a>
          <a class="btn btn-secondary" href="https://github.com/MenkeTechnologies/javars" target="_blank" rel="noopener noreferrer">GitHub</a>
        </div>
      </div>
    </header>

    <div class="hub-scheme-strip">
      <div class="hub-scheme-strip-inner">
        <span class="hud-scheme-label">// Color scheme</span>
        <div class="scheme-grid" id="hudSchemeGrid"></div>
      </div>
    </div>

    <main class="tutorial-main">
      <h2 class="tutorial-title"><span class="step-hash">&gt;_</span>LANGUAGE REFERENCE</h2>
      <p class="tutorial-subtitle">Every name the current javars build implements — reserved and contextual keywords, literal forms, declaration types, operators, console IO, the <code>java.lang.String</code> and <code>java.util</code> method surfaces, the static library, the modeled throwables and functional interfaces, synthesized class members, method-reference forms, <code>String.format</code> conversions, and the runtime builtin id space. Each entry carries its signature, a description written from the implementation, and — where one clarifies the behaviour — a runnable example. Where javars computes something other than <code>java</code> does, the entry says so. This page is generated from the reference corpus (<code>src/reference.rs</code>) by the <code>gen-docs</code> binary, and the language server reads the same table, so the page, the editor tooling, and the runtime cannot drift apart.</p>
"#;

const FOOT: &str = r#"
      <section class="tutorial-section">
        <h2>More</h2>
        <ul>
          <li><strong>Docs</strong> — <a href="index.html">index.html</a> (overview, architecture, examples)</li>
          <li><strong>Engineering report</strong> — <a href="report.html">report.html</a> (value model, status, dependencies)</li>
          <li><strong>Source</strong> — <a href="https://github.com/MenkeTechnologies/javars">github.com/MenkeTechnologies/javars</a></li>
        </ul>
      </section>
    </main>

  </div>

  <script src="hud-theme.js"></script>
</body>
</html>
"#;
