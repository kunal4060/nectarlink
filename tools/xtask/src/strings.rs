// SPDX-License-Identifier: GPL-3.0-or-later
//! `strings`: the text the Windows app builds in Rust, for Qt's translation
//! tools. Every `tr!("Context", "Text", ...)` in `desktop/app/src` goes into
//! `desktop/app/i18n/rust_strings.h` as a `QT_TRANSLATE_NOOP`, which `lupdate`
//! reads along with the QML. `--check` fails when the file is out of date.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

const SOURCES: &str = "desktop/app/src";
const OUT: &str = "desktop/app/i18n/rust_strings.h";

pub fn run(check: bool) -> Result<()> {
    let root = crate::repo_root();
    let mut files = Vec::new();
    collect(&root.join(SOURCES), &mut files)?;
    files.sort();
    // (context, text) -> the file it first appears in (no line: a line
    // moving would make the file stale).
    let mut strings: BTreeMap<(String, String), String> = BTreeMap::new();
    for file in &files {
        let source = fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
        let shown = file.strip_prefix(&root).unwrap_or(file).to_string_lossy().replace('\\', "/");
        for (_, context, text) in find(&source).with_context(|| format!("in {shown}"))? {
            // find() already skips #[cfg(test)] modules, so a production
            // context (even one named "Tests") is kept.
            strings.entry((context, text)).or_insert_with(|| shown.clone());
        }
    }
    let content = render(&strings);
    let out = root.join(OUT);
    if check {
        if fs::read_to_string(&out).unwrap_or_default() != content {
            bail!("out of date: {OUT}; run `cargo xtask strings`");
        }
        println!("{OUT} is up to date ({} strings)", strings.len());
    } else {
        if let Some(dir) = out.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        fs::write(&out, content).with_context(|| format!("writing {OUT}"))?;
        println!("wrote {OUT} ({} strings)", strings.len());
    }
    Ok(())
}

fn collect(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            collect(&path, files)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            files.push(path);
        }
    }
    Ok(())
}

/// Every `tr!("Context", "Text"` in `source`: its line, context and text.
/// Calls inside `#[cfg(test)]` modules are the tests' own strings, not
/// the app's. A call whose arguments are not plain or raw literals (a
/// variable, a concatenation) is warned about and skipped: one odd call
/// must not abort the whole run.
fn find(source: &str) -> Result<Vec<(usize, String, String)>> {
    let test_ranges = test_module_ranges(source);
    let mut found = Vec::new();
    let mut rest = source;
    let mut offset = 0;
    while let Some(at) = rest.find("tr!(") {
        // Not a longer name ending in "tr" (e.g. `str!(`).
        let before = source[..offset + at].chars().next_back();
        let start = offset + at + "tr!(".len();
        offset = start;
        rest = &source[start..];
        if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
            continue;
        }
        // Not in a #[cfg(test)] module.
        if test_ranges.iter().any(|(a, b)| start >= *a && start < *b) {
            continue;
        }
        // Not in a comment (a doc example).
        let line_start = source[..start].rfind('\n').map_or(0, |i| i + 1);
        if in_comment(source, line_start, start) {
            continue;
        }
        let line = source[..start].matches('\n').count() + 1;
        let (context, after) = match literal(rest) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("warning: line {line}: skipping tr! call ({e:#})");
                continue;
            }
        };
        let after = match after.trim_start().strip_prefix(',') {
            Some(a) => a,
            None => {
                eprintln!("warning: line {line}: skipping tr! call (tr! needs a text)");
                continue;
            }
        };
        let (text, _) = match literal(after) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("warning: line {line}: skipping tr! call ({e:#})");
                continue;
            }
        };
        found.push((line, context, text));
    }
    Ok(found)
}

/// Byte ranges of `#[cfg(test)] mod … { … }` bodies in `source`.
fn test_module_ranges(source: &str) -> Vec<(usize, usize)> {
    let bytes = source.as_bytes();
    let mut ranges = Vec::new();
    let mut at = 0;
    while let Some(rel) = find_bytes(&bytes[at..], b"#[cfg(test)]") {
        let mut i = at + rel + "#[cfg(test)]".len();
        // Only `mod <name> {` counts: an attribute on a function or a
        // stray mention in a string must not swallow the code after it.
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if &bytes[i..i + 3.min(bytes.len() - i)] != b"mod"
            || bytes.get(i + 3).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
        {
            at = i;
            continue;
        }
        i += 3;
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_')
        {
            i += 1;
        }
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if bytes.get(i) != Some(&b'{') {
            at = i;
            continue;
        }
        let open = i;
        let mut depth = 1;
        i += 1;
        while i < bytes.len() && depth > 0 {
            match bytes[i] {
                b'"' => i = skip_string(bytes, i),
                b'\'' => i = skip_char(bytes, i),
                b'r' if is_raw_open(bytes, i) => i = skip_raw_string(bytes, i),
                b'/' if bytes.get(i + 1) == Some(&b'/') => {
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                }
                b'/' if bytes.get(i + 1) == Some(&b'*') => {
                    i = skip_block_comment(bytes, i);
                }
                b'{' => {
                    depth += 1;
                    i += 1;
                }
                b'}' => {
                    depth -= 1;
                    i += 1;
                }
                _ => i += 1,
            }
        }
        ranges.push((open, i));
        at = i;
    }
    ranges
}

/// Whether the text at `source[line_start..start]` ends inside a comment:
/// a `//` (or `/*`) in a string on the same line, e.g. `https://…`, is
/// not a comment, but a real `//` or an unclosed `/*` before the call is.
fn in_comment(source: &str, line_start: usize, start: usize) -> bool {
    let bytes = source.as_bytes();
    let mut i = line_start;
    let mut block = 0;
    while i < start {
        if block > 0 {
            if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                block += 1;
                i += 2;
            } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                block -= 1;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        match bytes[i] {
            b'"' => i = skip_string(bytes, i),
            b'\'' => i = skip_char(bytes, i),
            b'r' if is_raw_open(bytes, i) => i = skip_raw_string(bytes, i),
            b'/' if bytes.get(i + 1) == Some(&b'/') => return true,
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                block = 1;
                i += 2;
            }
            _ => i += 1,
        }
    }
    block > 0
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Index just past a plain string starting at `bytes[i] == b'"'`.
fn skip_string(bytes: &[u8], mut i: usize) -> usize {
    i += 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    i
}

/// Index just past a char literal starting at `bytes[i] == b'\''`.
fn skip_char(bytes: &[u8], mut i: usize) -> usize {
    i += 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'\'' => return i + 1,
            _ => i += 1,
        }
    }
    i
}

/// Whether `bytes[i]` starts a raw string (`r"…"`, `r#"…"#`, …).
fn is_raw_open(bytes: &[u8], i: usize) -> bool {
    if bytes[i] != b'r' {
        return false;
    }
    let mut j = i + 1;
    while bytes.get(j) == Some(&b'#') {
        j += 1;
    }
    bytes.get(j) == Some(&b'"')
}

/// Index just past a raw string starting at `bytes[i] == b'r'`.
fn skip_raw_string(bytes: &[u8], i: usize) -> usize {
    let mut j = i + 1;
    while bytes.get(j) == Some(&b'#') {
        j += 1;
    }
    let hashes = j - (i + 1);
    let mut k = j + 1; // past the opening quote
    while k < bytes.len() {
        if bytes[k] == b'"' && bytes[k + 1..].iter().take(hashes).all(|b| *b == b'#') {
            return k + 1 + hashes;
        }
        k += 1;
    }
    bytes.len()
}

/// Index just past a (possibly nested) block comment at `bytes[i] == b'/'`.
fn skip_block_comment(bytes: &[u8], mut i: usize) -> usize {
    let mut depth = 1;
    i += 2;
    while i < bytes.len() && depth > 0 {
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
            depth += 1;
            i += 2;
        } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
            depth -= 1;
            i += 2;
        } else {
            i += 1;
        }
    }
    i
}

/// A Rust string literal at the start of `s` (after whitespace), unescaped,
/// and what follows it. Plain literals accept `\r`, `\0`, `\xHH` and
/// `\u{…}` besides the escapes the old code knew; raw literals
/// (`r"…"` / `r#"…"#`) are returned verbatim.
fn literal(s: &str) -> Result<(String, &str)> {
    let s = s.trim_start();
    if let Some(rest) = s.strip_prefix('r') {
        let hashes = rest.chars().take_while(|c| *c == '#').count();
        if let Some(body) = rest[hashes..].strip_prefix('"') {
            let closer = format!("\"{}\", "#".repeat(hashes));
            let Some(end) = body.find(&closer) else { bail!("unterminated raw string literal") };
            return Ok((body[..end].to_owned(), &body[end + closer.len()..]));
        }
    }
    let body = s.strip_prefix('"').context("not a string literal")?;
    let mut out = String::new();
    let mut chars = body.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => return Ok((out, &body[i + 1..])),
            '\\' => match chars.next().map(|(_, e)| e) {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('0') => out.push('\0'),
                Some('"') => out.push('"'),
                Some('\'') => out.push('\''),
                Some('\\') => out.push('\\'),
                Some('x') => {
                    let h1 = chars.next().map(|(_, e)| e).context("truncated \\x escape")?;
                    let h2 = chars.next().map(|(_, e)| e).context("truncated \\x escape")?;
                    let byte = u8::from_str_radix(&format!("{h1}{h2}"), 16)
                        .context("invalid \\x escape")?;
                    out.push(byte as char);
                }
                Some('u') => {
                    if chars.next().map(|(_, e)| e) != Some('{') {
                        bail!("malformed \\u escape");
                    }
                    let mut hex = String::new();
                    loop {
                        match chars.next().map(|(_, e)| e) {
                            Some('}') => break,
                            Some(c) if c.is_ascii_hexdigit() => hex.push(c),
                            _ => bail!("malformed \\u escape"),
                        }
                    }
                    let code =
                        u32::from_str_radix(&hex, 16).context("invalid \\u escape")?;
                    out.push(char::from_u32(code).context("invalid \\u escape")?);
                }
                Some('\n') => {
                    // A line continuation: skip the next line's indentation.
                    while chars.clone().next().is_some_and(|(_, c)| c.is_whitespace()) {
                        chars.next();
                    }
                }
                other => bail!("unsupported escape {other:?}"),
            },
            c => out.push(c),
        }
    }
    bail!("unterminated string literal")
}

fn c_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // A raw control byte would break the C string (and a \x
            // escape could merge with a following hex digit); octal is
            // always exactly three digits, so it cannot merge.
            c if c.is_control() => out.push_str(&format!("\\{:03o}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn render(strings: &BTreeMap<(String, String), String>) -> String {
    let mut out = String::from(
        "// SPDX-License-Identifier: GPL-3.0-or-later\n\
         // Generated by `cargo xtask strings` from the tr! calls in desktop/app/src. Do not edit.\n\
         // Never compiled: lupdate reads it with the QML, so the text the app builds\n\
         // in Rust is translated like the rest (see desktop/app/src/i18n.rs).\n\
         \n\
         static const char *const nectarlink_rust_strings[] = {\n",
    );
    for ((context, text), at) in strings {
        out.push_str(&format!("    QT_TRANSLATE_NOOP({}, {}), // {at}\n", c_string(context), c_string(text)));
    }
    out.push_str("};\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_tr_calls_with_their_text() {
        let source = r#"
            show_message(tr!("Media", "That player can't do that."));
            let x = tr!("Links", "Sent to %1", name);
            let s = my_str!("not this");
            /// An example: tr!("Docs", "not this either")
            tr!(
                "Mic",
                "Plays into \"%1\"",
                device
            );
        "#;
        let found = find(source).unwrap();
        assert_eq!(
            found,
            vec![
                (2, "Media".into(), "That player can't do that.".into()),
                (3, "Links".into(), "Sent to %1".into()),
                (6, "Mic".into(), "Plays into \"%1\"".into()),
            ]
        );
    }

    #[test]
    fn the_header_escapes_for_c() {
        let mut strings = BTreeMap::new();
        strings.insert(("Ctx".to_owned(), "Say \"hi\"\\".to_owned()), "a.rs".to_owned());
        assert!(render(&strings).contains(r#"QT_TRANSLATE_NOOP("Ctx", "Say \"hi\"\\"), // a.rs"#));
    }
}
