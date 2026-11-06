//! A static reader for Void Linux (xbps-src) templates.
//!
//! A template is a shell file, but nearly everything Forge needs sits in plain top-level
//! `name=value` lines. This reads those lines and expands the simple `${var}` forms;
//! it never runs anything. Values it cannot work out (command substitutions, unknown
//! variables) carry the [`UNKNOWN`] marker so the converter can tell.

use std::collections::{BTreeMap, BTreeSet};

/// Stands in for a part of a value that can only be known by running the template.
pub const UNKNOWN: char = '\u{0}';

#[derive(Debug, Default)]
pub struct Template {
    pub vars: BTreeMap<String, String>,
    /// Functions the template defines (`do_build`, `post_install`, ...).
    pub funcs: BTreeSet<String>,
}

impl Template {
    pub fn get(&self, k: &str) -> Option<&str> {
        self.vars.get(k).map(String::as_str)
    }
}

pub fn has_unknown(s: &str) -> bool {
    s.contains(UNKNOWN)
}

/// Glob match for `*` and `?` over whole strings.
fn glob(p: &[char], s: &[char]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some('*') => (0..=s.len()).any(|i| glob(&p[1..], &s[i..])),
        Some('?') => !s.is_empty() && glob(&p[1..], &s[1..]),
        Some(c) => s.first() == Some(c) && glob(&p[1..], &s[1..]),
    }
}

/// `${name<op>arg}` for the operators templates actually use.
fn apply(val: &str, op: &str) -> Option<String> {
    let v: Vec<char> = val.chars().collect();
    let pat = |s: &str| -> Vec<char> { s.chars().collect() };
    let join = |c: &[char]| c.iter().collect::<String>();
    if let Some(rest) = op.strip_prefix("//") {
        let (a, b) = rest.split_once('/').unwrap_or((rest, ""));
        return Some(val.replace(a, b));
    }
    if let Some(rest) = op.strip_prefix('/') {
        let (a, b) = rest.split_once('/').unwrap_or((rest, ""));
        return Some(val.replacen(a, b, 1));
    }
    if let Some(p) = op.strip_prefix("%%") {
        let p = pat(p);
        return Some(join(&v[..(0..=v.len()).find(|&i| glob(&p, &v[i..])).unwrap_or(v.len())]));
    }
    if let Some(p) = op.strip_prefix('%') {
        let p = pat(p);
        return Some(join(&v[..(0..=v.len()).rev().find(|&i| glob(&p, &v[i..])).unwrap_or(v.len())]));
    }
    if let Some(p) = op.strip_prefix("##") {
        let p = pat(p);
        return Some(join(&v[(0..=v.len()).rev().find(|&i| glob(&p, &v[..i])).unwrap_or(0)..]));
    }
    if let Some(p) = op.strip_prefix('#') {
        let p = pat(p);
        return Some(join(&v[(0..=v.len()).find(|&i| glob(&p, &v[..i])).unwrap_or(0)..]));
    }
    None
}

/// Expand `$var`, `${var}` and the `${var//a/b}`-style forms against `vars`.
pub fn expand(s: &str, vars: &BTreeMap<String, String>) -> String {
    let c: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < c.len() {
        if c[i] != '$' {
            out.push(c[i]);
            i += 1;
            continue;
        }
        match c.get(i + 1) {
            Some('{') => {
                let Some(end) = c[i + 2..].iter().position(|&x| x == '}') else {
                    out.push(UNKNOWN);
                    return out;
                };
                let body: String = c[i + 2..i + 2 + end].iter().collect();
                let name_len = body.chars().take_while(|x| x.is_ascii_alphanumeric() || *x == '_').count();
                let (name, op) = body.split_at(name_len);
                match vars.get(name) {
                    Some(v) if op.is_empty() => out.push_str(v),
                    Some(v) => match apply(v, op) {
                        Some(r) => out.push_str(&r),
                        None => out.push(UNKNOWN),
                    },
                    None => out.push(UNKNOWN),
                }
                i += end + 3;
            }
            Some('(') => {
                // Command substitution: skip to the matching paren.
                let mut depth = 0;
                let mut j = i + 1;
                while j < c.len() {
                    match c[j] {
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    j += 1;
                }
                out.push(UNKNOWN);
                i = j + 1;
            }
            Some(&x) if x.is_ascii_alphabetic() || x == '_' => {
                let n = c[i + 1..].iter().take_while(|x| x.is_ascii_alphanumeric() || **x == '_').count();
                let name: String = c[i + 1..i + 1 + n].iter().collect();
                match vars.get(&name) {
                    Some(v) => out.push_str(v),
                    None => out.push(UNKNOWN),
                }
                i += 1 + n;
            }
            _ => {
                out.push('$');
                i += 1;
            }
        }
    }
    out
}

/// Read one shell word starting at `chars[*i]`, joining quoted parts; expands unless single-quoted.
/// Returns None if a quote is left open.
fn read_word(chars: &[char], i: &mut usize, vars: &BTreeMap<String, String>) -> Option<String> {
    let mut out = String::new();
    while *i < chars.len() {
        match chars[*i] {
            c if c.is_whitespace() => break,
            '\'' => {
                let end = chars[*i + 1..].iter().position(|&x| x == '\'')?;
                out.extend(&chars[*i + 1..*i + 1 + end]);
                *i += end + 2;
            }
            '"' => {
                let mut j = *i + 1;
                let mut raw = String::new();
                loop {
                    match chars.get(j) {
                        None => return None,
                        Some('"') => break,
                        Some('\\') if matches!(chars.get(j + 1), Some('"' | '\\' | '$' | '`')) => {
                            if chars[j + 1] == '$' {
                                raw.push_str("\\$");
                            } else {
                                raw.push(chars[j + 1]);
                            }
                            j += 2;
                        }
                        Some('\\') if chars.get(j + 1) == Some(&'\n') => j += 2,
                        Some(&x) => {
                            raw.push(x);
                            j += 1;
                        }
                    }
                }
                out.push_str(&expand(&raw.replace("\\$", "\u{1}"), vars).replace('\u{1}', "$"));
                *i = j + 1;
            }
            '\\' if *i + 1 < chars.len() => {
                if chars[*i + 1] != '\n' {
                    out.push(chars[*i + 1]);
                }
                *i += 2;
            }
            '$' => {
                // An unquoted expansion up to the next whitespace or quote.
                let mut j = *i;
                let mut depth = 0;
                while j < chars.len() {
                    let x = chars[j];
                    if x == '{' || x == '(' {
                        depth += 1;
                    } else if x == '}' || x == ')' {
                        depth -= 1;
                    } else if depth <= 0 && j > *i && (x.is_whitespace() || x == '"' || x == '\'' || x == '$') {
                        break;
                    }
                    j += 1;
                }
                let raw: String = chars[*i..j].iter().collect();
                out.push_str(&expand(&raw, vars));
                *i = j;
            }
            c => {
                out.push(c);
                *i += 1;
            }
        }
    }
    Some(out)
}

fn is_func_start(line: &str) -> Option<String> {
    let t = line.trim_end();
    let name_len = t.chars().take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '+')).count();
    let (name, rest) = t.split_at(name_len);
    let rest = rest.trim_start();
    (name_len > 0 && rest.starts_with("()")).then(|| name.to_string())
}

fn assignment(line: &str) -> Option<(String, bool, &str)> {
    let n = line.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').count();
    if n == 0 || line.as_bytes()[0].is_ascii_digit() {
        return None;
    }
    let rest = &line[n..];
    if let Some(v) = rest.strip_prefix("+=") {
        Some((line[..n].to_string(), true, v))
    } else {
        rest.strip_prefix('=').map(|v| (line[..n].to_string(), false, v))
    }
}

fn word_delta(line: &str) -> i32 {
    let mut d = 0;
    for w in line.split(|c: char| c.is_whitespace() || c == ';' || c == '(' || c == ')') {
        match w {
            "if" | "case" | "for" | "while" | "until" | "select" => d += 1,
            "fi" | "esac" | "done" => d -= 1,
            _ => {}
        }
    }
    d
}

pub fn parse(text: &str) -> Template {
    let mut t = Template::default();
    let lines: Vec<&str> = text.lines().collect();
    let mut n = 0;
    let mut depth = 0i32;
    while n < lines.len() {
        let line = lines[n];
        n += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if depth > 0 {
            depth += word_delta(trimmed);
            continue;
        }
        if let Some(name) = is_func_start(line) {
            t.funcs.insert(name);
            // Skip the body: it ends at a `}` in column 0 (or on the same line).
            if !(trimmed.ends_with('}') && trimmed.matches('{').count() == trimmed.matches('}').count()) {
                while n < lines.len() && !lines[n].starts_with('}') {
                    n += 1;
                }
                n += 1;
            }
            continue;
        }
        let first = trimmed.split_whitespace().next().unwrap_or("");
        if matches!(first, "if" | "case" | "for" | "while" | "until") {
            depth = word_delta(trimmed).max(0);
            continue;
        }
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let Some((name, append, value)) = assignment(trimmed) else { continue };
        // The value may run over several lines when a quote stays open.
        let mut buf = value.to_string();
        let word = loop {
            let chars: Vec<char> = buf.chars().collect();
            let mut i = 0;
            match read_word(&chars, &mut i, &t.vars) {
                Some(w) => break w,
                None if n < lines.len() => {
                    buf.push('\n');
                    buf.push_str(lines[n]);
                    n += 1;
                }
                None => break String::new(),
            }
        };
        if append {
            let e = t.vars.entry(name).or_default();
            if !e.is_empty() {
                e.push(' ');
            }
            e.push_str(&word);
        } else {
            t.vars.insert(name, word);
        }
    }
    t
}

/// Split a value into shell words (quotes respected).
pub fn words(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        // Expansion was done already; a stray `$` is literal here.
        let mut w = String::new();
        while i < chars.len() && !chars[i].is_whitespace() {
            match chars[i] {
                '\'' | '"' => {
                    let q = chars[i];
                    let end = chars[i + 1..].iter().position(|&x| x == q).map(|p| i + 1 + p).unwrap_or(chars.len());
                    w.extend(&chars[i + 1..end]);
                    i = end + 1;
                }
                '\\' if i + 1 < chars.len() => {
                    w.push(chars[i + 1]);
                    i += 2;
                }
                c => {
                    w.push(c);
                    i += 1;
                }
            }
        }
        out.push(w);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_plain_assignments_and_expands_variables() {
        let t = parse("pkgname=foo\nversion=1.2.3\ndistfiles=\"https://x/${pkgname}/v${version}.tar.gz\"\nshort_desc='A tool'\n");
        assert_eq!(t.get("distfiles"), Some("https://x/foo/v1.2.3.tar.gz"));
        assert_eq!(t.get("short_desc"), Some("A tool"));
    }

    #[test]
    fn multiline_quoted_values_and_appends() {
        let t = parse("makedepends=\"a b\n c d\"\nmakedepends+=\" e\"\nchecksum=abc\n");
        assert_eq!(words(t.get("makedepends").unwrap()), ["a", "b", "c", "d", "e"]);
        assert_eq!(t.get("checksum"), Some("abc"));
    }

    #[test]
    fn substitution_forms() {
        let t = parse("version=1.2.3\na=${version//./_}\nb=${version%.*}\nc=${version#*.}\nd=${version%%.*}\n");
        assert_eq!(t.get("a"), Some("1_2_3"));
        assert_eq!(t.get("b"), Some("1.2"));
        assert_eq!(t.get("c"), Some("2.3"));
        assert_eq!(t.get("d"), Some("1"));
    }

    #[test]
    fn dynamic_values_are_marked_unknown() {
        let t = parse("version=1\nx=\"$(date)\"\ny=\"${XBPS_X}/a\"\n");
        assert!(has_unknown(t.get("x").unwrap()));
        assert!(has_unknown(t.get("y").unwrap()));
    }

    #[test]
    fn functions_and_conditionals_are_skipped_but_functions_noted() {
        let t = parse(
            "pkgname=a\ncase \"$X\" in\n\tx86_64) makedepends+=\" y\" ;;\nesac\nif [ -n \"$Z\" ]; then\n  version=9\nfi\nversion=1\ndo_build() {\n\tversion=7\n}\npost_install() {\n\ttrue\n}\n",
        );
        assert_eq!(t.get("version"), Some("1"));
        assert!(t.get("makedepends").is_none());
        assert!(t.funcs.contains("do_build") && t.funcs.contains("post_install"));
    }

    #[test]
    fn words_respect_quotes() {
        assert_eq!(words("-DA=ON \"-DB=x y\" --c='d e'"), ["-DA=ON", "-DB=x y", "--c=d e"]);
    }
}
