//! Static reading of Rust source, for the checks that read the test files themselves: the
//! selfcheck's crate-use scan (`code_only`) and the catalogue check's lookup of the Rust
//! tests (`test_names`). Comments and literals are never code.

use std::collections::BTreeSet;

/// The text of Rust source with comments and string and character literals blanked out
/// (newlines kept), so only code is searched.
pub fn code_only(src: &str) -> String {
    let c: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let blank = |out: &mut String, ch: char| out.push(if ch == '\n' { '\n' } else { ' ' });
    let ident = |ch: char| ch.is_alphanumeric() || ch == '_';
    let mut i = 0;
    while i < c.len() {
        let prev_ident = i > 0 && ident(c[i - 1]);
        if c[i] == '/' && c.get(i + 1) == Some(&'/') {
            while i < c.len() && c[i] != '\n' {
                blank(&mut out, c[i]);
                i += 1;
            }
        } else if c[i] == '/' && c.get(i + 1) == Some(&'*') {
            let mut depth = 0;
            while i < c.len() {
                if c[i] == '/' && c.get(i + 1) == Some(&'*') {
                    depth += 1;
                    out.push_str("  ");
                    i += 2;
                } else if c[i] == '*' && c.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    out.push_str("  ");
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    blank(&mut out, c[i]);
                    i += 1;
                }
            }
        } else if !prev_ident && raw_string_start(&c, i).is_some() {
            let (hashes, quote) = raw_string_start(&c, i).unwrap();
            let mut j = quote + 1;
            while j < c.len() && !(c[j] == '"' && (1..=hashes).all(|h| c.get(j + h) == Some(&'#')))
            {
                j += 1;
            }
            let end = (j + 1 + hashes).min(c.len());
            for &ch in &c[i..end] {
                blank(&mut out, ch);
            }
            i = end;
        } else if c[i] == '"' {
            blank(&mut out, c[i]);
            i += 1;
            while i < c.len() && c[i] != '"' {
                if c[i] == '\\' {
                    blank(&mut out, c[i]);
                    i += 1;
                }
                if i < c.len() {
                    blank(&mut out, c[i]);
                    i += 1;
                }
            }
            if i < c.len() {
                blank(&mut out, c[i]);
                i += 1;
            }
        } else if c[i] == '\'' && (c.get(i + 1) == Some(&'\\') || c.get(i + 2) == Some(&'\'')) {
            // A character literal ('x' or an escape); a lifetime has no closing quote.
            let mut j = i + 1;
            if c[j] == '\\' {
                j += 2;
            }
            while j < c.len() && c[j] != '\'' {
                j += 1;
            }
            let end = (j + 1).min(c.len());
            for &ch in &c[i..end] {
                blank(&mut out, ch);
            }
            i = end;
        } else {
            out.push(c[i]);
            i += 1;
        }
    }
    out
}

/// For `r"`, `r#"`, `br"`, `cr#"` and the like at `i`: the number of `#`s and the index of
/// the opening quote.
fn raw_string_start(c: &[char], i: usize) -> Option<(usize, usize)> {
    let mut j = i;
    if matches!(c.get(j), Some('b' | 'c')) {
        j += 1;
    }
    if c.get(j) != Some(&'r') {
        return None;
    }
    j += 1;
    let hashes = c[j..].iter().take_while(|&&ch| ch == '#').count();
    (c.get(j + hashes) == Some(&'"')).then_some((hashes, j + hashes))
}

/// `code`'s tokens: identifiers, `::`, `->`, `=>`, and every other character but whitespace
/// on its own.
fn tokens(code: &str) -> Vec<String> {
    let c: Vec<char> = code.chars().collect();
    let ident = |ch: char| ch.is_alphanumeric() || ch == '_';
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() {
            i += 1;
        } else if ident(ch) && !ch.is_numeric() {
            let start = i;
            while i < c.len() && ident(c[i]) {
                i += 1;
            }
            out.push(c[start..i].iter().collect());
        } else if let Some(two) = c.get(i..i + 2).map(|p| p.iter().collect::<String>())
            && matches!(two.as_str(), "::" | "->" | "=>")
        {
            out.push(two);
            i += 2;
        } else {
            out.push(ch.to_string());
            i += 1;
        }
    }
    out
}

fn is_ident(t: &str) -> bool {
    let mut chars = t.chars();
    chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || c == '_')
}

/// The index just past the bracket that closes the one at `j`.
fn matching(toks: &[String], mut j: usize, open: &str, close: &str) -> usize {
    let mut level = 0;
    while j < toks.len() {
        if toks[j] == open {
            level += 1;
        } else if toks[j] == close {
            level -= 1;
            if level == 0 {
                return j + 1;
            }
        }
        j += 1;
    }
    j
}

/// The libtest names (`module::…::fn`) of the `#[test]` functions in a test file's source:
/// functions with a body, in the file's top level or its inline modules (a function's body
/// is not searched). The same reading as `check_conversion.py`'s `rust_functions`.
pub fn test_names(src: &str) -> BTreeSet<String> {
    let toks = tokens(&code_only(src));
    let mut found = BTreeSet::new();
    let mut mods: Vec<(String, usize)> = Vec::new();
    let mut attrs: Vec<String> = Vec::new();
    let mut depth = 0usize;
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i].as_str();
        if t == "#" && matches!(toks.get(i + 1).map(String::as_str), Some("[" | "!")) {
            let j = if toks[i + 1] == "[" { i + 1 } else { i + 2 };
            let end = matching(&toks, j, "[", "]");
            let inner = if end >= j + 2 {
                &toks[j + 1..end - 1]
            } else {
                &[]
            };
            attrs.push(inner.join(" "));
            i = end;
            continue;
        }
        if t == "mod"
            && toks.get(i + 1).is_some_and(|n| is_ident(n))
            && toks.get(i + 2).is_some_and(|b| b == "{")
        {
            mods.push((toks[i + 1].clone(), depth));
            depth += 1;
            attrs.clear();
            i += 3;
            continue;
        }
        if t == "fn" && toks.get(i + 1).is_some_and(|n| is_ident(n)) {
            let name = toks[i + 1].clone();
            let mut j = i + 2;
            if toks.get(j).is_some_and(|t| t == "<") {
                j = matching(&toks, j, "<", ">");
            }
            while j < toks.len() && toks[j] != "(" {
                j += 1;
            }
            j = matching(&toks, j, "(", ")");
            while j < toks.len() && toks[j] != "{" && toks[j] != ";" {
                j += 1;
            }
            if j >= toks.len() || toks[j] == ";" {
                attrs.clear();
                i = j + 1;
                continue;
            }
            let end = matching(&toks, j, "{", "}");
            if attrs.iter().any(|a| a == "test") {
                let mut path: Vec<&str> = mods.iter().map(|(m, _)| m.as_str()).collect();
                path.push(&name);
                found.insert(path.join("::"));
            }
            attrs.clear();
            i = end;
            continue;
        }
        if t == "{" {
            depth += 1;
        } else if t == "}" {
            depth = depth.saturating_sub(1);
            while mods.last().is_some_and(|(_, d)| *d == depth) {
                mods.pop();
            }
        }
        if t == ";" || t == "{" || t == "}" {
            attrs.clear();
        }
        i += 1;
    }
    found
}
