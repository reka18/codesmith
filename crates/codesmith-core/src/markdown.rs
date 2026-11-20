//! Markdown files with YAML frontmatter, as agent files use them: the
//! header's nested maps, lists and scalars, with keys kept in source order
//! (OpenCode's legacy `permission` and `tools` maps are last-match-wins, so
//! the order an author writes keys in is policy).
//!
//! This reads the YAML subset frontmatter needs, not all of YAML: block
//! maps and lists (a list item may be a map), plain, quoted and block (`|`,
//! `>`) scalars, comments, and one-line flow lists and maps (`[a, b]`,
//! `{a: 1}`). As OpenCode's `ConfigMarkdown.sanitize` does, a plain value
//! with an unquoted colon, or a leading `[tag]` followed by text, is read as
//! text rather than failing. Anchors, tags and multi-document streams are
//! not supported; a header using them does not parse.

use serde_json::{Map, Number, Value};

/// A parsed YAML value. Maps keep their keys in source order.
#[derive(Debug, Clone, PartialEq)]
pub enum Yaml {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    List(Vec<Yaml>),
    Map(Vec<(String, Yaml)>),
}

impl Yaml {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Yaml::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&[(String, Yaml)]> {
        match self {
            Yaml::Map(m) => Some(m),
            _ => None,
        }
    }

    /// The value as JSON (map keys lose their order).
    pub fn to_json(&self) -> Value {
        match self {
            Yaml::Null => Value::Null,
            Yaml::Bool(b) => Value::Bool(*b),
            Yaml::Num(n) => {
                if n.fract() == 0.0 && n.abs() < 9.0e15 {
                    Value::Number(Number::from(*n as i64))
                } else {
                    Number::from_f64(*n).map_or(Value::Null, Value::Number)
                }
            }
            Yaml::Str(s) => Value::String(s.clone()),
            Yaml::List(l) => Value::Array(l.iter().map(Yaml::to_json).collect()),
            Yaml::Map(m) => {
                let mut out = Map::new();
                for (k, v) in m {
                    out.insert(k.clone(), v.to_json());
                }
                Value::Object(out)
            }
        }
    }
}

/// A Markdown document: its frontmatter (empty when there is none) and body.
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    pub data: Vec<(String, Yaml)>,
    pub content: String,
}

/// Split `---\n<yaml>\n---\n<body>`. `None` when the header does not parse.
pub fn parse(text: &str) -> Option<Document> {
    let t = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = t
        .strip_prefix("---\n")
        .or_else(|| t.strip_prefix("---\r\n"))
    else {
        return Some(Document {
            data: Vec::new(),
            content: t.to_string(),
        });
    };
    // The closing fence: a line of exactly `---` (an empty header too).
    let (header, body) = if let Some(b) = rest
        .strip_prefix("---\n")
        .or_else(|| rest.strip_prefix("---\r\n"))
        .or_else(|| (rest == "---").then_some(""))
    {
        ("", b)
    } else {
        let mut at = None;
        let mut search = 0;
        while let Some(i) = rest[search..].find("\n---") {
            let start = search + i;
            let after = &rest[start + 4..];
            if after.is_empty() || after.starts_with('\n') || after.starts_with("\r\n") {
                at = Some(start);
                break;
            }
            search = start + 4;
        }
        let start = at?;
        let after = &rest[start + 4..];
        let after = after
            .strip_prefix("\r\n")
            .or_else(|| after.strip_prefix('\n'))
            .unwrap_or(after);
        (&rest[..start], after)
    };
    let data = match parse_yaml(header)? {
        Yaml::Map(m) => m,
        Yaml::Null => Vec::new(),
        _ => return None,
    };
    Some(Document {
        data,
        content: body.to_string(),
    })
}

struct Line<'a> {
    indent: usize,
    text: &'a str,
}

/// Parse a YAML block (the subset described in the module docs).
pub fn parse_yaml(text: &str) -> Option<Yaml> {
    let lines: Vec<Line> = text
        .lines()
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .map(|l| {
            let indent = l.len() - l.trim_start_matches(' ').len();
            Line {
                indent,
                text: &l[indent..],
            }
        })
        .collect();
    let mut p = Parser { lines, at: 0 };
    p.skip_blank();
    if p.at >= p.lines.len() {
        return Some(Yaml::Null);
    }
    let indent = p.lines[p.at].indent;
    let v = p.block(indent)?;
    p.skip_blank();
    (p.at >= p.lines.len()).then_some(v)
}

struct Parser<'a> {
    lines: Vec<Line<'a>>,
    at: usize,
}

fn is_blank(text: &str) -> bool {
    let t = text.trim();
    t.is_empty() || t.starts_with('#')
}

impl Parser<'_> {
    fn skip_blank(&mut self) {
        while self.at < self.lines.len() && is_blank(self.lines[self.at].text) {
            self.at += 1;
        }
    }

    /// The block starting at the current line, indented `indent`.
    fn block(&mut self, indent: usize) -> Option<Yaml> {
        let first = &self.lines[self.at];
        if first.text == "-" || first.text.starts_with("- ") {
            self.list(indent)
        } else {
            self.map(indent)
        }
    }

    fn list(&mut self, indent: usize) -> Option<Yaml> {
        let mut items = Vec::new();
        loop {
            self.skip_blank();
            let Some(line) = self.lines.get(self.at) else {
                break;
            };
            if line.indent < indent {
                break;
            }
            if line.indent > indent || !(line.text == "-" || line.text.starts_with("- ")) {
                return None;
            }
            let rest = line.text[1..].trim_start_matches(' ');
            let inner = indent + (line.text.len() - rest.len());
            if is_blank(rest) {
                // `-` alone: the item is the indented block that follows.
                self.at += 1;
                self.skip_blank();
                match self.lines.get(self.at) {
                    Some(l) if l.indent > indent => {
                        let i = l.indent;
                        items.push(self.block(i)?);
                    }
                    _ => items.push(Yaml::Null),
                }
            } else if starts_map_entry(rest) || rest.starts_with("- ") {
                // `- key: value` starts a map item; its other keys line up
                // with `key`. Read it as if `key` were on a line of its own.
                let text = self.lines[self.at].text;
                self.lines[self.at] = Line {
                    indent: inner,
                    text: &text[text.len() - rest.len()..],
                };
                items.push(self.block(inner)?);
            } else {
                self.at += 1;
                items.push(self.scalar_value(rest, indent)?);
            }
        }
        Some(Yaml::List(items))
    }

    fn map(&mut self, indent: usize) -> Option<Yaml> {
        let mut entries: Vec<(String, Yaml)> = Vec::new();
        loop {
            self.skip_blank();
            let Some(line) = self.lines.get(self.at) else {
                break;
            };
            if line.indent < indent {
                break;
            }
            if line.indent > indent {
                return None;
            }
            let (key, rest) = split_entry(line.text)?;
            self.at += 1;
            let value = if is_blank(rest) {
                self.skip_blank();
                match self.lines.get(self.at) {
                    // A nested block, or a list at the key's own indent.
                    Some(l)
                        if l.indent > indent
                            || (l.indent == indent
                                && (l.text == "-" || l.text.starts_with("- "))) =>
                    {
                        let i = l.indent;
                        self.block(i)?
                    }
                    _ => Yaml::Null,
                }
            } else {
                self.scalar_value(rest, indent)?
            };
            // A repeated key replaces the earlier value in its first place.
            match entries.iter_mut().find(|(k, _)| *k == key) {
                Some(e) => e.1 = value,
                None => entries.push((key, value)),
            }
        }
        Some(Yaml::Map(entries))
    }

    /// The value after `key:` or `- ` on one line (block scalars read on).
    fn scalar_value(&mut self, rest: &str, indent: usize) -> Option<Yaml> {
        let rest = strip_comment(rest);
        if let Some(style) = rest.chars().next().filter(|c| matches!(c, '|' | '>')) {
            let chomp = rest[1..].trim();
            if chomp.chars().all(|c| matches!(c, '-' | '+' | '0'..='9')) {
                return Some(Yaml::Str(self.block_scalar(style, chomp, indent)));
            }
        }
        // A quoted scalar may continue on the following lines.
        if (rest.starts_with('"') && closing_quote(rest, '"').is_none())
            || (rest.starts_with('\'') && closing_quote(rest, '\'').is_none())
        {
            let mut joined = rest.to_string();
            while let Some(l) = self.lines.get(self.at) {
                if l.indent <= indent && !l.text.is_empty() {
                    break;
                }
                self.at += 1;
                joined.push(' ');
                joined.push_str(l.text.trim());
                if closing_quote(&joined, joined.chars().next().unwrap()).is_some() {
                    break;
                }
            }
            return scalar(&joined);
        }
        // Plain multi-line scalars: indented continuation lines fold in.
        let mut text = rest.to_string();
        while let Some(l) = self.lines.get(self.at) {
            if l.indent <= indent || is_blank(l.text) || l.text.starts_with("- ") {
                break;
            }
            if split_entry(l.text).is_some() {
                break;
            }
            text.push(' ');
            text.push_str(l.text.trim());
            self.at += 1;
        }
        scalar(&text)
    }

    fn block_scalar(&mut self, style: char, chomp: &str, indent: usize) -> String {
        let mut raw: Vec<&str> = Vec::new();
        let mut block_indent = None;
        while let Some(l) = self.lines.get(self.at) {
            if !l.text.is_empty() && l.indent <= indent {
                break;
            }
            if !l.text.is_empty() && block_indent.is_none() {
                block_indent = Some(l.indent);
            }
            raw.push(if l.text.is_empty() { "" } else { l.text });
            self.at += 1;
        }
        let bi = block_indent.unwrap_or(indent + 1);
        // Keep indentation deeper than the block's own.
        let lines: Vec<String> = raw
            .iter()
            .zip(self.lines[self.at - raw.len()..self.at].iter())
            .map(|(t, l)| {
                if t.is_empty() {
                    String::new()
                } else {
                    format!("{}{}", " ".repeat(l.indent.saturating_sub(bi)), t)
                }
            })
            .collect();
        let mut body = if style == '|' {
            lines.join("\n")
        } else {
            let mut out = String::new();
            for (i, l) in lines.iter().enumerate() {
                if i > 0 {
                    out.push(if l.is_empty() || lines[i - 1].is_empty() {
                        '\n'
                    } else {
                        ' '
                    });
                }
                out.push_str(l);
            }
            out
        };
        let trimmed = body.trim_end_matches('\n').len();
        body.truncate(trimmed);
        if chomp.contains('-') {
            body
        } else if chomp.contains('+') {
            let n = raw.iter().rev().take_while(|t| t.is_empty()).count();
            body + &"\n".repeat(n + 1)
        } else if body.is_empty() {
            body
        } else {
            body + "\n"
        }
    }
}

/// `key: rest` (or `key:` alone): the key, unquoted, and what follows.
fn split_entry(text: &str) -> Option<(String, &str)> {
    if text.starts_with('"') || text.starts_with('\'') {
        let q = text.chars().next()?;
        let end = closing_quote(text, q)?;
        let key = unquote(&text[..=end])?;
        let after = text[end + 1..].trim_start();
        let rest = after.strip_prefix(':')?;
        if !(rest.is_empty() || rest.starts_with(' ')) {
            return None;
        }
        return Some((key, rest.trim_start()));
    }
    let mut search = 0;
    while let Some(i) = text[search..].find(':') {
        let at = search + i;
        let rest = &text[at + 1..];
        if rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t') {
            let key = text[..at].trim();
            if key.is_empty() || key.starts_with(['-', '#', '[', '{']) {
                return None;
            }
            return Some((key.to_string(), rest.trim_start()));
        }
        search = at + 1;
    }
    None
}

fn starts_map_entry(text: &str) -> bool {
    split_entry(text).is_some()
}

/// Drop a trailing ` # comment` outside quotes.
fn strip_comment(text: &str) -> &str {
    let t = text.trim();
    if t.starts_with('"') || t.starts_with('\'') {
        let q = t.chars().next().unwrap();
        if let Some(end) = closing_quote(t, q) {
            let after = t[end + 1..].trim_start();
            if after.is_empty() || after.starts_with('#') {
                return &t[..=end];
            }
        }
        return t;
    }
    match t.find(" #") {
        Some(i) => t[..i].trim_end(),
        None => t,
    }
}

/// The byte index of the quote closing the one at the start of `s`.
fn closing_quote(s: &str, q: char) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = 1;
    while i < b.len() {
        let c = b[i] as char;
        if q == '"' && c == '\\' {
            i += 2;
            continue;
        }
        if c == q {
            if q == '\'' && b.get(i + 1) == Some(&b'\'') {
                i += 2;
                continue;
            }
            return Some(i);
        }
        i += 1;
    }
    None
}

fn unquote(s: &str) -> Option<String> {
    if let Some(inner) = s.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')) {
        return Some(inner.replace("''", "'"));
    }
    let inner = s.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next()? {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            '0' => out.push('\0'),
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            '/' => out.push('/'),
            ' ' => out.push(' '),
            'u' => {
                let hex: String = chars.by_ref().take(4).collect();
                out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
            }
            other => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    Some(out)
}

/// A one-line scalar, flow list or flow map.
fn scalar(text: &str) -> Option<Yaml> {
    let t = text.trim();
    if t.starts_with('"') || t.starts_with('\'') {
        let q = t.chars().next()?;
        let end = closing_quote(t, q)?;
        if end + 1 == t.len() {
            return Some(Yaml::Str(unquote(t)?));
        }
        // Text after the closing quote: the whole line is text.
        return Some(Yaml::Str(t.to_string()));
    }
    if (t.starts_with('[') && t.ends_with(']')) || (t.starts_with('{') && t.ends_with('}')) {
        if let Some(v) = flow(t) {
            return Some(v);
        }
    }
    Some(plain(t))
}

fn plain(t: &str) -> Yaml {
    match t {
        "" | "~" | "null" | "Null" | "NULL" => return Yaml::Null,
        "true" | "True" | "TRUE" => return Yaml::Bool(true),
        "false" | "False" | "FALSE" => return Yaml::Bool(false),
        _ => {}
    }
    let numeric = t
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E'))
        && t.chars().any(|c| c.is_ascii_digit());
    if numeric {
        if let Ok(n) = t.parse::<f64>() {
            return Yaml::Num(n);
        }
    }
    Yaml::Str(t.to_string())
}

/// `[a, "b", {c: d}]` or `{a: 1, b: [x]}` on one line.
fn flow(t: &str) -> Option<Yaml> {
    let (open, close) = (t.chars().next()?, t.chars().last()?);
    let inner = &t[1..t.len() - 1];
    let mut parts = Vec::new();
    let (mut depth, mut start, mut quote) = (0i32, 0usize, None::<char>);
    for (i, c) in inner.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                '"' | '\'' => quote = Some(c),
                '[' | '{' => depth += 1,
                ']' | '}' => depth -= 1,
                ',' if depth == 0 => {
                    parts.push(&inner[start..i]);
                    start = i + 1;
                }
                _ => {}
            },
        }
    }
    if depth != 0 || quote.is_some() {
        return None;
    }
    let last = &inner[start..];
    if !last.trim().is_empty() || !parts.is_empty() {
        parts.push(last);
    }
    let parts: Vec<&str> = parts.into_iter().map(str::trim).collect();
    if open == '[' && close == ']' {
        return parts
            .iter()
            .map(|p| scalar(p))
            .collect::<Option<Vec<_>>>()
            .map(Yaml::List);
    }
    let mut entries = Vec::new();
    for p in parts {
        let (k, v) = match split_entry(p) {
            Some(kv) => kv,
            None => (p.to_string(), ""),
        };
        entries.push((k, scalar(v)?));
    }
    Some(Yaml::Map(entries))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Yaml {
        Yaml::Str(v.into())
    }

    #[test]
    fn nested_maps_lists_and_scalars() {
        let d = parse(
            "---\nmodel:\n  providerID: example\n  model: chat\nrequest:\n  headers:\n    x-agent: native\n  body:\n    effort: high\npermissions:\n  - action: edit\n    resource: \"*\"\n    effect: deny\ntemperature: 0.5\nsteps: 10\nhidden: true\n---\nBody text.",
        )
        .unwrap();
        assert_eq!(d.content, "Body text.");
        let get = |k: &str| d.data.iter().find(|(x, _)| x == k).unwrap().1.clone();
        assert_eq!(
            get("model"),
            Yaml::Map(vec![
                ("providerID".into(), s("example")),
                ("model".into(), s("chat"))
            ])
        );
        assert_eq!(
            get("permissions").to_json(),
            serde_json::json!([{"action": "edit", "resource": "*", "effect": "deny"}])
        );
        assert_eq!(
            get("request").to_json(),
            serde_json::json!({"headers": {"x-agent": "native"}, "body": {"effort": "high"}})
        );
        assert_eq!(get("temperature"), Yaml::Num(0.5));
        assert_eq!(get("steps").to_json(), serde_json::json!(10));
        assert_eq!(get("hidden"), Yaml::Bool(true));
    }

    #[test]
    fn keys_keep_source_order() {
        let d =
            parse("---\npermission:\n  \"*\": allow\n  bash: ask\n  edit: deny\n---\n").unwrap();
        let m = d.data[0].1.as_map().unwrap();
        let keys: Vec<&str> = m.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["*", "bash", "edit"]);
    }

    #[test]
    fn values_other_agents_accept() {
        let d = parse("---\ndescription: Use this: when needed\nname: [Tag] text\nlist: [a, \"b c\"]\nmap: {a: 1}\nfolded: >\n  one\n  two\nliteral: |\n  line 1\n  line 2\n# comment\nquoted: 'it''s'\n---\n").unwrap();
        let get = |k: &str| d.data.iter().find(|(x, _)| x == k).unwrap().1.clone();
        assert_eq!(get("description"), s("Use this: when needed"));
        assert_eq!(get("name"), s("[Tag] text"));
        assert_eq!(get("list"), Yaml::List(vec![s("a"), s("b c")]));
        assert_eq!(get("map").to_json(), serde_json::json!({"a": 1}));
        assert_eq!(get("folded"), s("one two\n"));
        assert_eq!(get("literal"), s("line 1\nline 2\n"));
        assert_eq!(get("quoted"), s("it's"));
    }

    #[test]
    fn documents_without_or_with_empty_frontmatter() {
        assert_eq!(parse("Just text").unwrap().content, "Just text");
        assert!(parse("Just text").unwrap().data.is_empty());
        let d = parse("---\n---\nBody").unwrap();
        assert!(d.data.is_empty());
        assert_eq!(d.content, "Body");
        assert!(
            parse("---\na: b\n  c: d\n---\n").is_none(),
            "bad indentation"
        );
    }
}
