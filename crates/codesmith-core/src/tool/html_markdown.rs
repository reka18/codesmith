//! HTML to Markdown for `webfetch`, ported from OpenCode's
//! packages/core/src/tool/html-markdown.ts (MIT), with a small
//! htmlparser2-style streaming parser in front of it.
//!
//! Everything is iterative with explicit stacks, so hostile nesting cannot
//! overflow the thread stack (the `htmd` crate CodeSmith used before aborted
//! the process on a few thousand nested `<div>`s), and the output is bounded
//! by a byte budget. Active and hidden content (`script`, `style`,
//! `iframe`, `object`, `template`, `head`, `hidden`, `aria-hidden="true"`,
//! closed `<details>` bodies) is left out.
//!
//! Differences from htmlparser2: only common named entities are decoded
//! (all numeric ones are), and `<![CDATA[`/processing instructions are
//! skipped like comments.

pub const MAX_MARKDOWN_BYTES: usize = 5 * 1024 * 1024;
const CONTENT_BYTES: usize = MAX_MARKDOWN_BYTES - 64 * 1024;
const MAX_DEPTH: usize = 10_000;

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

pub type Attrs = Vec<(String, String)>;

pub trait Handler {
    fn open(&mut self, name: &str, attrs: &Attrs);
    fn text(&mut self, text: &str);
    fn close(&mut self, name: &str);
}

fn attr<'a>(attrs: &'a Attrs, name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

const VOID: [&str; 19] = [
    "area", "base", "basefont", "br", "col", "command", "embed", "frame", "hr", "image", "img",
    "input", "keygen", "link", "meta", "param", "source", "track", "wbr",
];

/// Tags whose opening closes these open tags first (htmlparser2's `openImpliesClose`).
fn implies_close(name: &str) -> &'static [&'static str] {
    const P: &[&str] = &["p"];
    const FORM: &[&str] = &[
        "input", "option", "optgroup", "select", "button", "datalist", "textarea",
    ];
    match name {
        "tr" => &["tr", "th", "td"],
        "th" => &["th"],
        "td" => &["thead", "th", "td"],
        "body" => &["head", "link", "script"],
        "li" => &["li"],
        "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "address" | "article" | "aside"
        | "blockquote" | "details" | "div" | "dl" | "fieldset" | "figcaption" | "figure"
        | "footer" | "form" | "header" | "hr" | "main" | "nav" | "ol" | "pre" | "section"
        | "table" | "ul" => P,
        "select" | "input" | "output" | "button" | "datalist" | "textarea" => FORM,
        "option" => &["option"],
        "optgroup" => &["optgroup", "option"],
        "dd" | "dt" => &["dd", "dt"],
        "rt" | "rp" => &["rt", "rp"],
        "tbody" | "tfoot" => &["thead", "tbody"],
        _ => &[],
    }
}

struct Tree<'h, H: Handler> {
    h: &'h mut H,
    stack: Vec<String>,
}

impl<H: Handler> Tree<'_, H> {
    fn open(&mut self, name: String, attrs: Attrs) {
        let closes = implies_close(&name);
        while self
            .stack
            .last()
            .is_some_and(|t| closes.contains(&t.as_str()))
        {
            let t = self.stack.pop().unwrap();
            self.h.close(&t);
        }
        let void = VOID.contains(&name.as_str());
        self.h.open(&name, &attrs);
        if void {
            self.h.close(&name);
        } else {
            self.stack.push(name);
        }
    }

    fn close(&mut self, name: &str) {
        if VOID.contains(&name) {
            if name == "br" {
                self.h.open("br", &Vec::new());
                self.h.close("br");
            }
            return;
        }
        if let Some(pos) = self.stack.iter().rposition(|t| t == name) {
            while self.stack.len() > pos {
                let t = self.stack.pop().unwrap();
                self.h.close(&t);
            }
        } else if name == "p" {
            self.h.open("p", &Vec::new());
            self.h.close("p");
        }
    }
}

/// Feed `html` to `h` as open, text and close events, with implied closes
/// applied and every open tag closed by the end.
pub fn parse<H: Handler>(html: &str, h: &mut H) {
    let mut tree = Tree {
        h,
        stack: Vec::new(),
    };
    let b = html.as_bytes();
    let mut i = 0;
    let mut text_start = 0;
    let flush = |tree: &mut Tree<H>, a: usize, z: usize| {
        if z > a {
            tree.h.text(&decode(&html[a..z]));
        }
    };
    while i < b.len() {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        let next = b.get(i + 1).copied();
        if html[i..].starts_with("<!--") {
            flush(&mut tree, text_start, i);
            let end = html[i + 4..]
                .find("-->")
                .map(|e| i + 4 + e + 3)
                .unwrap_or(b.len());
            i = end;
            text_start = i;
        } else if matches!(next, Some(b'!') | Some(b'?')) {
            flush(&mut tree, text_start, i);
            i = html[i..].find('>').map(|e| i + e + 1).unwrap_or(b.len());
            text_start = i;
        } else if next == Some(b'/') && b.get(i + 2).is_some_and(|c| c.is_ascii_alphabetic()) {
            flush(&mut tree, text_start, i);
            let start = i + 2;
            let mut j = start;
            while j < b.len()
                && !matches!(b[j], b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' | b'/' | b'>')
            {
                j += 1;
            }
            let name = html[start..j].to_ascii_lowercase();
            i = html[j..].find('>').map(|e| j + e + 1).unwrap_or(b.len());
            text_start = i;
            tree.close(&name);
        } else if next.is_some_and(|c| c.is_ascii_alphabetic()) {
            flush(&mut tree, text_start, i);
            let start = i + 1;
            let mut j = start;
            while j < b.len()
                && !matches!(b[j], b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' | b'/' | b'>')
            {
                j += 1;
            }
            let name = html[start..j].to_ascii_lowercase();
            let (attrs, end) = attributes(html, j);
            i = end;
            text_start = i;
            tree.open(name.clone(), attrs);
            if matches!(name.as_str(), "script" | "style" | "title") {
                // Raw text up to the matching close tag.
                let lower = html[i..].to_ascii_lowercase();
                let needle = format!("</{name}");
                let mut from = 0;
                let close = loop {
                    match lower[from..].find(&needle) {
                        Some(p) => {
                            let at = from + p + needle.len();
                            if lower.as_bytes().get(at).is_none_or(|c| {
                                matches!(c, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' | b'/' | b'>')
                            }) {
                                break Some(from + p);
                            }
                            from = at;
                        }
                        None => break None,
                    }
                };
                let body_end = close.map(|c| i + c).unwrap_or(b.len());
                if body_end > i {
                    let raw = &html[i..body_end];
                    if name == "title" {
                        tree.h.text(&decode(raw));
                    } else {
                        tree.h.text(raw);
                    }
                }
                i = body_end;
                text_start = i;
            }
        } else {
            i += 1;
        }
    }
    flush(&mut tree, text_start, b.len());
    while let Some(t) = tree.stack.pop() {
        tree.h.close(&t);
    }
}

/// Attributes from `j` to the end of the tag; returns them and where the tag ends.
fn attributes(html: &str, mut j: usize) -> (Attrs, usize) {
    let b = html.as_bytes();
    let mut attrs: Attrs = Vec::new();
    let space = |c: u8| matches!(c, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c');
    loop {
        while j < b.len() && (space(b[j]) || b[j] == b'/') {
            j += 1;
        }
        if j >= b.len() {
            return (attrs, j);
        }
        if b[j] == b'>' {
            return (attrs, j + 1);
        }
        let start = j;
        while j < b.len() && !space(b[j]) && !matches!(b[j], b'=' | b'>' | b'/') {
            j += 1;
        }
        let name = html[start..j].to_ascii_lowercase();
        while j < b.len() && space(b[j]) {
            j += 1;
        }
        let mut value = String::new();
        if j < b.len() && b[j] == b'=' {
            j += 1;
            while j < b.len() && space(b[j]) {
                j += 1;
            }
            if j < b.len() && (b[j] == b'"' || b[j] == b'\'') {
                let q = b[j];
                let vs = j + 1;
                let ve = html[vs..]
                    .find(q as char)
                    .map(|e| vs + e)
                    .unwrap_or(b.len());
                value = decode(&html[vs..ve]);
                j = (ve + 1).min(b.len());
            } else {
                let vs = j;
                while j < b.len() && !space(b[j]) && b[j] != b'>' {
                    j += 1;
                }
                value = decode(&html[vs..j]);
            }
        }
        if !name.is_empty() && !attrs.iter().any(|(k, _)| *k == name) {
            attrs.push((name, value));
        }
    }
}

const ENTITIES: &[(&str, &str)] = &[
    ("amp", "&"),
    ("lt", "<"),
    ("gt", ">"),
    ("quot", "\""),
    ("apos", "'"),
    ("nbsp", "\u{a0}"),
    ("iexcl", "¡"),
    ("cent", "¢"),
    ("pound", "£"),
    ("curren", "¤"),
    ("yen", "¥"),
    ("brvbar", "¦"),
    ("sect", "§"),
    ("uml", "¨"),
    ("copy", "©"),
    ("ordf", "ª"),
    ("laquo", "«"),
    ("not", "¬"),
    ("shy", "\u{ad}"),
    ("reg", "®"),
    ("macr", "¯"),
    ("deg", "°"),
    ("plusmn", "±"),
    ("sup2", "²"),
    ("sup3", "³"),
    ("acute", "´"),
    ("micro", "µ"),
    ("para", "¶"),
    ("middot", "·"),
    ("cedil", "¸"),
    ("sup1", "¹"),
    ("ordm", "º"),
    ("raquo", "»"),
    ("frac14", "¼"),
    ("frac12", "½"),
    ("frac34", "¾"),
    ("iquest", "¿"),
    ("Agrave", "À"),
    ("Aacute", "Á"),
    ("Acirc", "Â"),
    ("Atilde", "Ã"),
    ("Auml", "Ä"),
    ("Aring", "Å"),
    ("AElig", "Æ"),
    ("Ccedil", "Ç"),
    ("Egrave", "È"),
    ("Eacute", "É"),
    ("Ecirc", "Ê"),
    ("Euml", "Ë"),
    ("Igrave", "Ì"),
    ("Iacute", "Í"),
    ("Icirc", "Î"),
    ("Iuml", "Ï"),
    ("ETH", "Ð"),
    ("Ntilde", "Ñ"),
    ("Ograve", "Ò"),
    ("Oacute", "Ó"),
    ("Ocirc", "Ô"),
    ("Otilde", "Õ"),
    ("Ouml", "Ö"),
    ("times", "×"),
    ("Oslash", "Ø"),
    ("Ugrave", "Ù"),
    ("Uacute", "Ú"),
    ("Ucirc", "Û"),
    ("Uuml", "Ü"),
    ("Yacute", "Ý"),
    ("THORN", "Þ"),
    ("szlig", "ß"),
    ("agrave", "à"),
    ("aacute", "á"),
    ("acirc", "â"),
    ("atilde", "ã"),
    ("auml", "ä"),
    ("aring", "å"),
    ("aelig", "æ"),
    ("ccedil", "ç"),
    ("egrave", "è"),
    ("eacute", "é"),
    ("ecirc", "ê"),
    ("euml", "ë"),
    ("igrave", "ì"),
    ("iacute", "í"),
    ("icirc", "î"),
    ("iuml", "ï"),
    ("eth", "ð"),
    ("ntilde", "ñ"),
    ("ograve", "ò"),
    ("oacute", "ó"),
    ("ocirc", "ô"),
    ("otilde", "õ"),
    ("ouml", "ö"),
    ("divide", "÷"),
    ("oslash", "ø"),
    ("ugrave", "ù"),
    ("uacute", "ú"),
    ("ucirc", "û"),
    ("uuml", "ü"),
    ("yacute", "ý"),
    ("thorn", "þ"),
    ("yuml", "ÿ"),
    ("OElig", "Œ"),
    ("oelig", "œ"),
    ("Scaron", "Š"),
    ("scaron", "š"),
    ("Yuml", "Ÿ"),
    ("fnof", "ƒ"),
    ("circ", "ˆ"),
    ("tilde", "˜"),
    ("ensp", "\u{2002}"),
    ("emsp", "\u{2003}"),
    ("thinsp", "\u{2009}"),
    ("zwnj", "\u{200c}"),
    ("zwj", "\u{200d}"),
    ("lrm", "\u{200e}"),
    ("rlm", "\u{200f}"),
    ("ndash", "–"),
    ("mdash", "—"),
    ("lsquo", "‘"),
    ("rsquo", "’"),
    ("sbquo", "‚"),
    ("ldquo", "“"),
    ("rdquo", "”"),
    ("bdquo", "„"),
    ("dagger", "†"),
    ("Dagger", "‡"),
    ("bull", "•"),
    ("hellip", "…"),
    ("permil", "‰"),
    ("prime", "′"),
    ("Prime", "″"),
    ("lsaquo", "‹"),
    ("rsaquo", "›"),
    ("oline", "‾"),
    ("frasl", "⁄"),
    ("euro", "€"),
    ("trade", "™"),
    ("larr", "←"),
    ("uarr", "↑"),
    ("rarr", "→"),
    ("darr", "↓"),
    ("harr", "↔"),
    ("minus", "−"),
    ("le", "≤"),
    ("ge", "≥"),
    ("ne", "≠"),
    ("infin", "∞"),
    ("check", "✓"),
    ("hearts", "♥"),
    ("star", "☆"),
    ("Alpha", "Α"),
    ("alpha", "α"),
    ("beta", "β"),
    ("gamma", "γ"),
    ("delta", "δ"),
    ("lambda", "λ"),
    ("mu", "μ"),
    ("pi", "π"),
    ("sigma", "σ"),
    ("omega", "ω"),
    ("Omega", "Ω"),
    ("NewLine", "\n"),
    ("Tab", "\t"),
    ("lowbar", "_"),
    ("grave", "`"),
    ("lsqb", "["),
    ("rsqb", "]"),
    ("lcub", "{"),
    ("rcub", "}"),
    ("verbar", "|"),
    ("vert", "|"),
    ("ast", "*"),
    ("num", "#"),
    ("dollar", "$"),
    ("percnt", "%"),
    ("excl", "!"),
    ("quest", "?"),
    ("colon", ":"),
    ("semi", ";"),
    ("comma", ","),
    ("period", "."),
    ("sol", "/"),
    ("bsol", "\\"),
    ("equals", "="),
    ("plus", "+"),
    ("hyphen", "‐"),
    ("lpar", "("),
    ("rpar", ")"),
];

/// Decode character references.
pub fn decode(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        rest = &rest[p..];
        let body = &rest[1..];
        if let Some(num) = body.strip_prefix('#') {
            let (hex, digits) = match num.strip_prefix(['x', 'X']) {
                Some(h) => (true, h),
                None => (false, num),
            };
            let len = digits
                .chars()
                .take_while(|c| {
                    if hex {
                        c.is_ascii_hexdigit()
                    } else {
                        c.is_ascii_digit()
                    }
                })
                .count();
            if len > 0 {
                let value = u32::from_str_radix(&digits[..len], if hex { 16 } else { 10 }).ok();
                let ch = value
                    .filter(|v| *v != 0)
                    .and_then(char::from_u32)
                    .unwrap_or('\u{fffd}');
                out.push(ch);
                let mut used = 1 + 1 + usize::from(hex) + len;
                if rest[used..].starts_with(';') {
                    used += 1;
                }
                rest = &rest[used..];
                continue;
            }
        } else {
            let len = body
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .count();
            let name = &body[..len];
            let semi = body[len..].starts_with(';');
            let legacy = matches!(name, "amp" | "lt" | "gt" | "quot" | "nbsp");
            if semi || legacy {
                if let Some((_, v)) = ENTITIES.iter().find(|(k, _)| *k == name) {
                    out.push_str(v);
                    rest = &rest[1 + len + usize::from(semi)..];
                    continue;
                }
            }
        }
        out.push('&');
        rest = &rest[1..];
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------------------
// Converter
// ---------------------------------------------------------------------------

const OMITTED: [&str; 9] = [
    "script", "style", "noscript", "iframe", "object", "embed", "meta", "link", "template",
];
const BLOCKS: [&str; 18] = [
    "address",
    "article",
    "aside",
    "details",
    "dialog",
    "div",
    "dl",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "header",
    "main",
    "nav",
    "p",
    "section",
    "summary",
];

fn heading(name: &str) -> Option<usize> {
    let b = name.as_bytes();
    (b.len() == 2 && b[0] == b'h' && (b'1'..=b'6').contains(&b[1])).then(|| (b[1] - b'0') as usize)
}

fn inline_marker(name: &str) -> Option<&'static str> {
    match name {
        "strong" | "b" => Some("**"),
        "em" | "i" => Some("*"),
        "s" | "strike" | "del" => Some("~~"),
        _ => None,
    }
}

/// The longest prefix of whole characters within `bytes`.
fn slice_bytes(value: &str, bytes: usize) -> &str {
    if value.len() <= bytes {
        return value;
    }
    let mut i = bytes;
    while !value.is_char_boundary(i) {
        i -= 1;
    }
    &value[..i]
}

fn parse_int(s: &str) -> Option<i64> {
    let t = s.trim_start();
    let (neg, digits) = match t.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let n: String = digits.chars().take_while(|c| c.is_ascii_digit()).collect();
    let v: i64 = n.get(..n.len().min(18))?.parse().ok()?;
    Some(if neg { -v } else { v })
}

fn collapse(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.chars() {
        if matches!(c, '\t' | '\r' | '\n' | ' ') {
            space = true;
        } else {
            if space && !out.is_empty() {
                out.push(' ');
            }
            space = false;
            out.push(c);
        }
    }
    out
}

enum Chunk {
    Text(String),
    Raw(String),
}

impl Chunk {
    fn as_str(&self) -> &str {
        match self {
            Chunk::Text(s) | Chunk::Raw(s) => s,
        }
    }
}

struct Link {
    href: String,
    title: Option<String>,
}
struct Code {
    inline: bool,
    text: String,
    language: Option<String>,
}
struct Marker {
    index: usize,
    block: usize,
    leading_space: bool,
    previous: Option<usize>,
}
struct List {
    ordered: bool,
    next: i64,
    previous: Option<usize>,
}
struct Item {
    indent: String,
    previous: Option<usize>,
}
struct Table {
    start: usize,
    rows: Vec<Vec<String>>,
    row: Option<Vec<String>>,
    caption: Option<String>,
    fallback: bool,
    previous: Option<usize>,
}
struct Details {
    open: bool,
    summary: bool,
    previous: Option<usize>,
}

#[derive(Default)]
struct Frame {
    suppressed: bool,
    link: Option<usize>,
    suspended_link: Option<usize>,
    marker: Option<usize>,
    code: Option<usize>,
    link_code: Option<usize>,
    resumed_code: Option<usize>,
    list: Option<usize>,
    item: Option<usize>,
    table: Option<usize>,
    cell: Option<usize>,
    caption: Option<usize>,
    details: Option<usize>,
}

#[derive(Default)]
struct Converter {
    output: Vec<Chunk>,
    stack: Vec<Frame>,
    pending_space: bool,
    pending_indent: String,
    last: Option<char>,
    quote_depth: usize,
    needs_quote_prefix: bool,
    block_count: usize,
    depth: usize,
    stopped: bool,
    output_bytes: usize,
    active_code: Option<usize>,
    active_link: Option<usize>,
    link_open: bool,
    active_marker: Option<usize>,
    active_list: Option<usize>,
    active_item: Option<usize>,
    active_table: Option<usize>,
    active_cell: Option<usize>,
    table_depth: usize,
    fallback_suppressed_depth: usize,
    fallback_omitted_depth: usize,
    active_details: Option<usize>,
    links: Vec<Link>,
    codes: Vec<Code>,
    markers: Vec<Marker>,
    lists: Vec<List>,
    items: Vec<Item>,
    tables: Vec<Table>,
    details: Vec<Details>,
}

impl Converter {
    fn append_limited(&mut self, value: &str, content: bool) -> bool {
        let limit = if content {
            CONTENT_BYTES
        } else {
            MAX_MARKDOWN_BYTES
        };
        if value.is_empty() || self.output_bytes >= limit {
            return false;
        }
        let next = slice_bytes(value, limit - self.output_bytes);
        self.output_bytes += next.len();
        if let Some(c) = next.chars().last() {
            self.last = Some(c);
        }
        self.output.push(Chunk::Text(next.to_string()));
        true
    }

    fn append(&mut self, value: &str) {
        self.append_limited(value, false);
    }

    fn append_raw(&mut self, value: &str) {
        if self.append_limited(value, false) {
            if let Some(Chunk::Text(s)) = self.output.pop() {
                self.output.push(Chunk::Raw(s));
            }
        }
    }

    fn take(&mut self, start: usize) -> String {
        let value: String = self
            .output
            .drain(start.min(self.output.len())..)
            .map(|c| c.as_str().to_string())
            .collect();
        self.output_bytes -= value.len();
        value
    }

    fn quote_prefix(&self) -> String {
        "> ".repeat(self.quote_depth.min(8))
    }

    fn prefix_quote(&mut self) {
        if !self.needs_quote_prefix || self.quote_depth == 0 || self.active_cell.is_some() {
            return;
        }
        let p = self.quote_prefix();
        self.append(&p);
        self.needs_quote_prefix = false;
    }

    fn flush_space(&mut self) {
        if !self.pending_space {
            return;
        }
        if let Some(m) = self.active_marker {
            let index = self.markers[m].index;
            if self.output.len() == index + 1 && self.last != Some(' ') && self.last != Some('\n') {
                if let Chunk::Text(s) = &mut self.output[index] {
                    s.insert(0, ' ');
                }
                self.output_bytes += 1;
                self.markers[m].leading_space = true;
                self.pending_space = false;
                return;
            }
        }
        if self.last.is_some_and(|c| c != '\n' && c != ' ') {
            self.append(" ");
        }
        self.pending_space = false;
    }

    fn inline(&mut self, value: &str, open: bool, content: bool) {
        if open {
            self.flush_space();
        }
        self.prefix_quote();
        if !self.pending_indent.is_empty() {
            let indent = std::mem::take(&mut self.pending_indent);
            self.append(&indent);
        }
        if self.active_link.is_some() && !self.link_open {
            self.append("[");
            self.link_open = true;
        }
        self.append_limited(value, content);
    }

    fn block(&mut self) {
        if let Some(l) = self.active_link {
            if self.link_open {
                self.close_link(l);
                self.link_open = false;
            }
        }
        self.pending_space = false;
        self.append("\n\n");
        self.block_count += 1;
        self.needs_quote_prefix = self.quote_depth > 0;
        self.pending_indent = self
            .active_item
            .map(|i| self.items[i].indent.clone())
            .unwrap_or_default();
    }

    fn suspend_link(&mut self, frame: usize) {
        let Some(l) = self.active_link else {
            return;
        };
        self.stack[frame].suspended_link = Some(l);
        self.block();
        self.active_link = None;
        self.link_open = false;
    }

    fn text(&mut self, value: &str) {
        if let Some(c) = self.active_code {
            self.codes[c].text.push_str(value);
            return;
        }
        let ws = |c: char| matches!(c, '\t' | '\n' | '\x0c' | '\r' | ' ');
        let mut rest = value;
        while !rest.is_empty() {
            let n = rest.find(|c: char| !ws(c)).unwrap_or(rest.len());
            if n > 0 {
                self.pending_space = true;
                rest = &rest[n..];
                continue;
            }
            let n = rest.find(ws).unwrap_or(rest.len());
            let part = &rest[..n];
            rest = &rest[n..];
            let mut escaped = String::with_capacity(part.len() + 4);
            for c in part.chars() {
                if "\\`*_[]<>|~".contains(c) {
                    escaped.push('\\');
                }
                escaped.push(c);
            }
            if escaped.starts_with(['#', '+', '-']) {
                escaped.insert(0, '\\');
            } else {
                let digits = escaped.chars().take_while(|c| c.is_ascii_digit()).count();
                if digits > 0 && escaped[digits..].starts_with('.') {
                    escaped.insert(digits, '\\');
                }
            }
            self.inline(&escaped, true, true);
        }
    }

    fn destination(value: &str) -> String {
        let mut out = String::new();
        let mut space = false;
        for c in value.chars() {
            if matches!(c, '\t' | '\n' | '\r' | ' ') {
                if !space {
                    out.push_str("%20");
                }
                space = true;
                continue;
            }
            space = false;
            if matches!(c, '\\' | '(' | ')') {
                out.push('\\');
            }
            out.push(c);
        }
        out
    }

    fn title(value: Option<&str>) -> String {
        match value {
            Some(v) if !v.is_empty() => {
                let t = collapse(v);
                let mut esc = String::new();
                for c in t.trim().chars() {
                    if c == '\\' || c == '"' {
                        esc.push('\\');
                    }
                    esc.push(c);
                }
                format!(" \"{esc}\"")
            }
            _ => String::new(),
        }
    }

    fn close_link(&mut self, link: usize) {
        let l = &self.links[link];
        let s = format!(
            "]({}{})",
            Self::destination(&l.href),
            Self::title(l.title.as_deref())
        );
        self.append(&s);
    }

    fn finish_code(&mut self, code: usize) {
        let text = self.codes[code].text.clone();
        let inline = self.codes[code].inline;
        if inline && text.is_empty() {
            return;
        }
        let (mut backticks, mut tildes, mut cb, mut ct) = (0, 0, 0, 0);
        for c in text.chars() {
            cb = if c == '`' { cb + 1 } else { 0 };
            ct = if c == '~' { ct + 1 } else { 0 };
            backticks = backticks.max(cb);
            tildes = tildes.max(ct);
        }
        if inline {
            let fence = "`".repeat((backticks + 1).max(1));
            self.flush_space();
            self.prefix_quote();
            let available = CONTENT_BYTES.saturating_sub(self.output_bytes + fence.len() * 2);
            let mut payload = slice_bytes(&text, available).to_string();
            while !payload.is_empty() {
                let edge = payload.starts_with([' ', '`']) || payload.ends_with([' ', '`']);
                let only_spaces = payload.chars().all(|c| c == ' ');
                let padding = if edge && !only_spaces { " " } else { "" };
                let bytes = payload.len();
                if bytes + padding.len() * 2 <= available {
                    self.append_raw(&format!("{fence}{padding}{payload}{padding}{fence}"));
                    return;
                }
                payload = slice_bytes(&payload, bytes - 1).to_string();
            }
            return;
        }
        if self.active_cell.is_some() {
            self.text(&text);
            return;
        }
        let marker = if backticks <= tildes { '`' } else { '~' };
        let count = if marker == '`' { backticks } else { tildes };
        let fence: String = std::iter::repeat_n(marker, (count + 1).max(3)).collect();
        self.block();
        let prefix = format!(
            "{fence}{}\n",
            self.codes[code].language.clone().unwrap_or_default()
        );
        let quote = if self.quote_depth > 0 {
            self.quote_prefix()
        } else {
            String::new()
        };
        let mut payload = text;
        loop {
            let candidate = format!(
                "{prefix}{payload}{}{fence}",
                if payload.ends_with('\n') { "" } else { "\n" }
            );
            let value = if quote.is_empty() {
                candidate
            } else {
                candidate
                    .split('\n')
                    .map(|line| format!("{quote}{line}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            if self.output_bytes + value.len() <= CONTENT_BYTES {
                self.append_raw(&value);
                self.block();
                return;
            }
            if payload.is_empty() {
                return;
            }
            let excess = value.len() - CONTENT_BYTES.saturating_sub(self.output_bytes);
            payload = slice_bytes(&payload, payload.len().saturating_sub(excess)).to_string();
        }
    }

    fn new_link(&mut self, attrs: &Attrs) -> usize {
        self.links.push(Link {
            href: attr(attrs, "href").unwrap_or("").to_string(),
            title: attr(attrs, "title").map(str::to_string),
        });
        self.links.len() - 1
    }

    fn new_code(&mut self, inline: bool) -> usize {
        self.codes.push(Code {
            inline,
            text: String::new(),
            language: None,
        });
        self.codes.len() - 1
    }

    fn code_inline(&self) -> bool {
        self.active_code.is_some_and(|c| self.codes[c].inline)
    }

    fn finish(mut self) -> String {
        let output = std::mem::take(&mut self.output);
        let mut normalized = String::new();
        let mut pending = String::new();
        let spaces = regex::Regex::new(r"[ \t]+\n").unwrap();
        let blanks = regex::Regex::new(r"\n{3,}").unwrap();
        let quote_re = regex::Regex::new(r"^(?:> )+").unwrap();
        let flush = |pending: &mut String, normalized: &mut String| {
            if pending.is_empty() {
                return;
            }
            let t = spaces.replace_all(
                pending,
                |c: &regex::Captures| {
                    if c[0].starts_with("  ") {
                        "  \n"
                    } else {
                        "\n"
                    }
                },
            );
            let t = blanks.replace_all(&t, "\n\n");
            let lines: Vec<&str> = t.split('\n').collect();
            let fixed: Vec<String> = lines
                .iter()
                .enumerate()
                .map(|(i, line)| {
                    if !line.is_empty() {
                        return line.to_string();
                    }
                    let before = i
                        .checked_sub(1)
                        .and_then(|j| quote_re.find(lines[j]))
                        .map(|m| m.len());
                    let after = lines
                        .get(i + 1)
                        .and_then(|l| quote_re.find(l))
                        .map(|m| m.len());
                    match (before, after) {
                        (Some(b), Some(a)) if a != b => {
                            "> ".repeat(a.min(b) / 2).trim_end().to_string()
                        }
                        _ => line.to_string(),
                    }
                })
                .collect();
            normalized.push_str(&fixed.join("\n"));
            pending.clear();
        };
        for chunk in output {
            match chunk {
                Chunk::Raw(r) => {
                    flush(&mut pending, &mut normalized);
                    normalized.push_str(&r);
                }
                Chunk::Text(t) => pending.push_str(&t),
            }
        }
        flush(&mut pending, &mut normalized);
        slice_bytes(normalized.trim(), MAX_MARKDOWN_BYTES).to_string()
    }
}

impl Handler for Converter {
    fn open(&mut self, name: &str, attrs: &Attrs) {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            if self.stack.last().is_some_and(|f| f.suppressed) {
                let visible = self.stack.iter().rposition(|f| !f.suppressed);
                self.fallback_suppressed_depth = visible.map(|v| v + 2).unwrap_or(1);
            }
            self.active_code = None;
            self.stopped = true;
        }
        if self.stopped {
            if OMITTED.contains(&name) {
                self.fallback_omitted_depth += 1;
            } else {
                self.pending_space = true;
            }
            return;
        }
        let mut suppressed =
            self.stack.last().is_some_and(|f| f.suppressed) || OMITTED.contains(&name);
        let hidden = attr(attrs, "hidden").is_some()
            || attr(attrs, "aria-hidden").is_some_and(|v| v.eq_ignore_ascii_case("true"))
            || name == "head";
        let details = self.active_details;
        if hidden
            || details.is_some_and(|d| {
                !self.details[d].open && !self.details[d].summary && name != "summary"
            })
        {
            suppressed = true;
        }
        self.stack.push(Frame {
            suppressed,
            ..Default::default()
        });
        let fi = self.stack.len() - 1;
        if suppressed {
            return;
        }
        if let Some(c) = self.active_code {
            if !self.codes[c].inline {
                if name == "br" {
                    self.codes[c].text.push('\n');
                }
                if name == "code" {
                    if let Some(class) = attr(attrs, "class") {
                        let re = regex::Regex::new(r"(?:language-|lang-)(\S+)").unwrap();
                        self.codes[c].language = re.captures(class).map(|m| m[1].to_string());
                    }
                }
                return;
            }
        }
        if name == "details" {
            self.details.push(Details {
                open: attr(attrs, "open").is_some(),
                summary: false,
                previous: self.active_details,
            });
            let d = self.details.len() - 1;
            self.stack[fi].details = Some(d);
            self.active_details = Some(d);
            self.block();
            return;
        }
        if name == "summary" {
            if let Some(d) = details {
                self.details[d].summary = true;
            }
            self.block();
            return;
        }
        if name == "pre" {
            self.suspend_link(fi);
            let c = self.new_code(false);
            self.stack[fi].code = Some(c);
            self.active_code = Some(c);
            return;
        }
        if name == "code" {
            if self.code_inline() {
                return;
            }
            let c = self.new_code(true);
            self.stack[fi].code = Some(c);
            self.active_code = Some(c);
            return;
        }
        if let Some(level) = heading(name) {
            self.block();
            self.inline(&format!("{} ", "#".repeat(level)), false, false);
            return;
        }
        if BLOCKS.contains(&name) {
            self.suspend_link(fi);
            if name == "p" && self.last == Some(' ') {
                return;
            }
            self.block();
            return;
        }
        if name == "br" {
            self.pending_space = false;
            self.inline("  \n", false, false);
            self.needs_quote_prefix = self.quote_depth > 0;
            return;
        }
        if name == "hr" {
            self.block();
            self.inline("---", false, false);
            self.block();
            return;
        }
        if let Some(marker) = inline_marker(name) {
            self.inline(marker, true, false);
            self.markers.push(Marker {
                index: self.output.len().saturating_sub(1),
                block: self.block_count,
                leading_space: false,
                previous: self.active_marker,
            });
            let m = self.markers.len() - 1;
            self.stack[fi].marker = Some(m);
            self.active_marker = Some(m);
            return;
        }
        if name == "a" {
            if let (Some(link), true) = (self.active_link, self.code_inline()) {
                let code = self.active_code;
                let parent = self
                    .stack
                    .iter()
                    .rposition(|f| f.link == Some(link) && f.link_code == code);
                if let Some(p) = parent {
                    self.finish_code(code.unwrap());
                    if self.link_open {
                        self.close_link(link);
                    }
                    self.active_code = self.stack[p].resumed_code;
                    self.stack[p].link = None;
                    self.stack[p].link_code = None;
                    self.active_link = None;
                    self.link_open = false;
                }
            }
            if self.code_inline() {
                let c = self.active_code.unwrap();
                self.stack[fi].resumed_code = Some(c);
                if !self.codes[c].text.is_empty() {
                    self.finish_code(c);
                }
                self.codes[c].text.clear();
                self.active_code = None;
                let l = self.new_link(attrs);
                self.stack[fi].link = Some(l);
                self.active_link = Some(l);
                self.link_open = true;
                self.inline("[", true, false);
                let lc = self.new_code(true);
                self.stack[fi].link_code = Some(lc);
                self.active_code = Some(lc);
                return;
            }
            if let Some(link) = self.active_link {
                if self.link_open {
                    self.close_link(link);
                }
                if let Some(p) = self.stack.iter().rposition(|f| f.link == Some(link)) {
                    self.stack[p].link = None;
                }
                self.active_link = None;
                self.link_open = false;
            }
            let l = self.new_link(attrs);
            self.stack[fi].link = Some(l);
            self.active_link = Some(l);
            self.link_open = true;
            self.inline("[", true, false);
            return;
        }
        if name == "img" {
            let mut alt = String::new();
            for c in attr(attrs, "alt").unwrap_or("").chars() {
                if c == '\\' || c == ']' {
                    alt.push('\\');
                }
                alt.push(c);
            }
            let close = format!(
                "]({}{})",
                Self::destination(attr(attrs, "src").unwrap_or("")),
                Self::title(attr(attrs, "title"))
            );
            let available = CONTENT_BYTES.saturating_sub(self.output_bytes + 2 + close.len());
            let value = format!("![{}{close}", slice_bytes(&alt, available));
            self.inline(&value, true, false);
            return;
        }
        if name == "blockquote" {
            self.suspend_link(fi);
            self.block();
            self.quote_depth += 1;
            self.needs_quote_prefix = true;
            return;
        }
        if name == "ul" || name == "ol" {
            self.suspend_link(fi);
            let start = parse_int(attr(attrs, "start").unwrap_or("1")).unwrap_or(1);
            self.lists.push(List {
                ordered: name == "ol",
                next: start,
                previous: self.active_list,
            });
            let l = self.lists.len() - 1;
            self.stack[fi].list = Some(l);
            self.active_list = Some(l);
            self.block();
            return;
        }
        if name == "li" {
            self.block();
            let value = attr(attrs, "value").and_then(parse_int);
            let marker = match self.active_list {
                Some(l) if self.lists[l].ordered => {
                    if let Some(v) = value {
                        self.lists[l].next = v;
                    }
                    let n = self.lists[l].next;
                    self.lists[l].next += 1;
                    format!("{n}.")
                }
                _ => "-".to_string(),
            };
            let indent: String = self
                .active_item
                .map(|i| self.items[i].indent.chars().take(24).collect())
                .unwrap_or_default();
            let prefix = format!("{indent}{marker} ");
            self.items.push(Item {
                indent: " ".repeat(prefix.chars().count()),
                previous: self.active_item,
            });
            let it = self.items.len() - 1;
            self.stack[fi].item = Some(it);
            self.active_item = Some(it);
            self.pending_indent.clear();
            self.inline(&prefix, false, false);
            return;
        }
        if name == "table" {
            self.suspend_link(fi);
            self.table_depth += 1;
            if self.table_depth == 1 {
                self.block();
                self.tables.push(Table {
                    start: self.output.len(),
                    rows: Vec::new(),
                    row: None,
                    caption: None,
                    fallback: false,
                    previous: self.active_table,
                });
                let t = self.tables.len() - 1;
                self.stack[fi].table = Some(t);
                self.active_table = Some(t);
            } else {
                self.pending_space = true;
            }
            return;
        }
        if name == "tr" {
            if self.table_depth != 1 {
                self.pending_space = true;
                return;
            }
            if let Some(t) = self.active_table {
                self.tables[t].row = Some(Vec::new());
            }
            return;
        }
        if name == "th" || name == "td" {
            if self.table_depth != 1 {
                self.pending_space = true;
                return;
            }
            let spans = attr(attrs, "colspan").is_some_and(|v| !v.is_empty())
                || attr(attrs, "rowspan").is_some_and(|v| !v.is_empty());
            if spans {
                if let Some(t) = self.active_table {
                    self.tables[t].fallback = true;
                }
            }
            self.stack[fi].cell = Some(self.output.len());
            self.active_cell = Some(self.output.len());
            return;
        }
        if name == "caption" {
            self.stack[fi].caption = Some(self.output.len());
            return;
        }
        if name == "dt" {
            self.block();
            self.inline("**", false, false);
            return;
        }
        if name == "dd" {
            self.inline("\n: ", false, false);
        }
    }

    fn text(&mut self, value: &str) {
        if self.stopped {
            if self.fallback_suppressed_depth == 0 && self.fallback_omitted_depth == 0 {
                Converter::text(self, value);
            }
            return;
        }
        if self.stack.last().is_some_and(|f| f.suppressed) {
            return;
        }
        Converter::text(self, value);
    }

    fn close(&mut self, name: &str) {
        self.depth = self.depth.saturating_sub(1);
        if self.stopped {
            if self.fallback_omitted_depth > 0 && OMITTED.contains(&name) {
                self.fallback_omitted_depth -= 1;
            }
            if self.fallback_suppressed_depth > 0 && self.depth < self.fallback_suppressed_depth {
                self.fallback_suppressed_depth = 0;
            }
            return;
        }
        let Some(frame) = self.stack.pop() else {
            return;
        };
        if frame.suppressed {
            return;
        }
        if let Some(lc) = frame.link_code {
            if self.active_link != frame.link || !self.link_open {
                let t = std::mem::take(&mut self.codes[lc].text);
                let rc = frame.resumed_code.unwrap();
                self.codes[rc].text.push_str(&t);
                self.active_code = Some(rc);
                if self.active_link == frame.link {
                    self.active_link = None;
                }
                self.link_open = false;
                return;
            }
            self.active_code = None;
            self.finish_code(lc);
            if let (Some(l), true) = (frame.link, self.link_open) {
                self.close_link(l);
            }
            self.active_link = None;
            self.link_open = false;
            self.active_code = frame.resumed_code;
            return;
        }
        if self.active_code.is_some_and(|c| !self.codes[c].inline) && frame.code.is_none() {
            return;
        }
        if let Some(c) = frame.code {
            self.active_code = None;
            self.finish_code(c);
            if frame.suspended_link.is_some() {
                self.active_link = frame.suspended_link;
            }
            return;
        }
        if name == "summary" {
            if let Some(d) = self.active_details {
                self.details[d].summary = false;
            }
            return self.block();
        }
        if name == "details" {
            self.active_details = frame.details.and_then(|d| self.details[d].previous);
            return self.block();
        }
        if name == "dt" {
            return self.inline("**", false, false);
        }
        if name == "dd" {
            return self.block();
        }
        if let Some(marker) = inline_marker(name) {
            let trailing = self.pending_space;
            self.pending_space = false;
            if let Some(m) = frame.marker {
                self.active_marker = self.markers[m].previous;
                let (index, block, leading) = (
                    self.markers[m].index,
                    self.markers[m].block,
                    self.markers[m].leading_space,
                );
                if block != self.block_count || self.output.len() == index + 1 {
                    if let Some(chunk) = self.output.get_mut(index) {
                        *chunk = Chunk::Text(String::new());
                    }
                    self.pending_space = trailing || leading;
                    return;
                }
                self.inline(marker, false, false);
                self.pending_space = trailing || leading;
                return;
            }
            self.inline(marker, false, false);
            self.pending_space = trailing;
            return;
        }
        if name == "a" {
            if let Some(l) = frame.link {
                if self.active_link == Some(l) || self.active_link.is_none() {
                    self.active_link = Some(l);
                    if self.link_open || self.last.is_some_and(|c| c != '\n') {
                        self.close_link(l);
                    }
                    self.link_open = false;
                    self.active_link = None;
                }
            }
            return;
        }
        if heading(name).is_some() || BLOCKS.contains(&name) {
            self.block();
            if frame.suspended_link.is_some() {
                self.active_link = frame.suspended_link;
            }
            return;
        }
        if name == "blockquote" {
            self.quote_depth = self.quote_depth.saturating_sub(1);
            self.block();
            if frame.suspended_link.is_some() {
                self.active_link = frame.suspended_link;
            }
            return;
        }
        if name == "li" {
            self.active_item = frame.item.and_then(|i| self.items[i].previous);
            return self.block();
        }
        if name == "ul" || name == "ol" {
            self.active_list = frame.list.and_then(|l| self.lists[l].previous);
            self.block();
            if frame.suspended_link.is_some() {
                self.active_link = frame.suspended_link;
            }
            return;
        }
        if (name == "th" || name == "td") && self.table_depth == 1 {
            self.active_cell = None;
            if let Some(start) = frame.cell {
                let raw = collapse(&self.take(start));
                let raw = raw.trim();
                let mut value = String::with_capacity(raw.len());
                let mut prev = None;
                for c in raw.chars() {
                    if c == '|' && prev != Some('\\') {
                        value.push('\\');
                    }
                    value.push(c);
                    prev = Some(c);
                }
                if let Some(t) = self.active_table {
                    if let Some(row) = self.tables[t].row.as_mut() {
                        row.push(value);
                    }
                }
            }
            return;
        }
        if name == "tr" {
            if self.table_depth != 1 {
                return;
            }
            if let Some(t) = self.active_table {
                if let Some(row) = self.tables[t].row.take() {
                    self.tables[t].rows.push(row);
                }
            }
            self.pending_space = true;
            return;
        }
        if name == "caption" {
            if let (Some(start), Some(t)) = (frame.caption, self.active_table) {
                let c = collapse(&self.take(start)).trim().to_string();
                self.tables[t].caption = Some(c);
                return;
            }
        }
        if name == "table" {
            self.table_depth = self.table_depth.saturating_sub(1);
            if self.table_depth == 0 {
                let table = frame.table;
                self.active_table = table.and_then(|t| self.tables[t].previous);
                if let Some(t) = table {
                    let start = self.tables[t].start;
                    let loose = collapse(&self.take(start)).trim().to_string();
                    let rows = std::mem::take(&mut self.tables[t].rows);
                    let width = rows.first().map(Vec::len).unwrap_or(0);
                    let rectangular = width > 0 && rows.iter().all(|r| r.len() == width);
                    if !loose.is_empty() {
                        self.append(&loose);
                        self.block();
                    }
                    if let Some(caption) = self.tables[t].caption.clone().filter(|c| !c.is_empty())
                    {
                        self.append(&caption);
                        self.block();
                    }
                    if !self.tables[t].fallback && rectangular {
                        let prefix = format!(
                            "{}{}",
                            if self.quote_depth > 0 {
                                self.quote_prefix()
                            } else {
                                String::new()
                            },
                            std::mem::take(&mut self.pending_indent)
                        );
                        self.append(&format!(
                            "{prefix}| {} |\n{prefix}|{}",
                            rows[0].join(" | "),
                            " --- |".repeat(width)
                        ));
                        for row in &rows[1..] {
                            self.append(&format!("\n{prefix}| {} |", row.join(" | ")));
                        }
                    } else {
                        for (i, row) in rows.iter().enumerate() {
                            if i > 0 {
                                self.block();
                            }
                            self.append(&row.join(" | "));
                        }
                    }
                }
                self.block();
                if frame.suspended_link.is_some() {
                    self.active_link = frame.suspended_link;
                }
                return;
            }
            self.pending_space = true;
        }
    }
}

/// Convert an HTML document to Markdown.
pub fn to_markdown(html: &str) -> String {
    let mut c = Converter::default();
    parse(html, &mut c);
    c.finish()
}

/// The visible text of an HTML document: text outside `script`, `style`,
/// `noscript`, `iframe`, `object` and `embed`, with block boundaries as line
/// breaks and runs of blank space collapsed.
pub fn to_text(html: &str) -> String {
    struct Text {
        out: String,
        skip: usize,
    }
    const SKIP: [&str; 6] = ["script", "style", "noscript", "iframe", "object", "embed"];
    const BREAKS: [&str; 30] = [
        "br",
        "p",
        "div",
        "li",
        "tr",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "section",
        "article",
        "header",
        "footer",
        "main",
        "nav",
        "aside",
        "ul",
        "ol",
        "table",
        "blockquote",
        "pre",
        "dl",
        "dt",
        "dd",
        "hr",
        "figure",
        "figcaption",
        "details",
    ];
    impl Handler for Text {
        fn open(&mut self, name: &str, _: &Attrs) {
            if self.skip > 0 || SKIP.contains(&name) {
                self.skip += 1;
            } else if BREAKS.contains(&name) {
                self.out.push('\n');
            }
        }
        fn text(&mut self, text: &str) {
            if self.skip == 0 {
                self.out.push_str(text);
            }
        }
        fn close(&mut self, name: &str) {
            if self.skip > 0 {
                self.skip -= 1;
            } else if BREAKS.contains(&name) {
                self.out.push('\n');
            }
        }
    }
    let mut t = Text {
        out: String::new(),
        skip: 0,
    };
    parse(html, &mut t);
    t.out
        .split('\n')
        .map(|line| collapse(line).replace('\u{a0}', " ").trim().to_string())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    // ---------------------------------------------------------------------
    // Ported from OpenCode's packages/core/test/tool-webfetch.test.ts
    // ("WebFetchTool helpers", MIT) and tool-html-markdown-budget.test.ts.
    // One `oc_webfetch_*` / `oc_html_markdown_budget_*` test per case.
    //
    // Intentional difference: `text` output keeps block boundaries as line
    // breaks ("Hello\nworld wide today"), where OpenCode concatenates text
    // nodes ("Helloworld wide today").
    // ---------------------------------------------------------------------
    use super::*;

    fn md(html: &str) -> String {
        to_markdown(html)
    }

    #[test]
    fn oc_webfetch_ports_text_and_markdown_without_active_content() {
        let html = "<h1>Hello</h1><script>bad()</script><p>world <strong>wide</strong> <product-name>today</product-name></p><style>.bad {}</style>";
        assert_eq!(to_text(html), "Hello\nworld wide today");
        assert_eq!(md(html), "# Hello\n\nworld **wide** today");
    }

    #[test]
    fn oc_webfetch_renders_headings_links_images_breaks_and_rules() {
        let html = r#"<h2>Read <em>this</em></h2><p><a href="https://example.com/a (b)" title="Example">docs</a><br><img src="diagram.png" alt="a ] b"></p><hr><p><del>old</del></p>"#;
        assert_eq!(
            md(html),
            "## Read *this*\n\n[docs](https://example.com/a%20\\(b\\) \"Example\")  \n![a \\] b](diagram.png)\n\n---\n\n~~old~~"
        );
    }

    #[test]
    fn oc_webfetch_preserves_inline_and_preformatted_code() {
        let html = "<p>Use <code>say(`hello`)</code> now.</p><pre><code class=\"language-ts\">const fence = ```\n&amp; stays decoded</code></pre>";
        assert_eq!(
            md(html),
            "Use ``say(`hello`)`` now.\n\n~~~ts\nconst fence = ```\n& stays decoded\n~~~"
        );
    }

    #[test]
    fn oc_webfetch_preserves_inline_code_boundaries() {
        for (content, expected) in [
            ("`x`", "`` `x` ``"),
            ("`x", "`` `x ``"),
            ("x`", "`` x` ``"),
            ("`", "`` ` ``"),
            ("``", "``` `` ```"),
            ("``x`", "``` ``x` ```"),
            ("say(`x`)", "``say(`x`)``"),
            ("a``b`c", "```a``b`c```"),
            ("x", "`x`"),
            (" x ", "`  x  `"),
            (" x", "`  x `"),
            ("x ", "` x  `"),
            ("   ", "`   `"),
            (" ` ", "``  `  ``"),
        ] {
            assert_eq!(
                md(&format!("<p>Use <code>{content}</code>.</p>")),
                format!("Use {expected}."),
                "{content:?}"
            );
        }
    }

    #[test]
    fn oc_webfetch_fits_inline_code_to_its_emitted_boundaries() {
        for (name, content, spare, expected) in [
            ("discarded trailing backtick after ASCII", "x`", 7, "``x``"),
            (
                "discarded trailing backtick after Unicode",
                "😀`",
                10,
                "``😀``",
            ),
            (
                "discarded trailing backtick with spare room",
                "x`",
                9,
                "``x``",
            ),
            ("retained trailing backtick", "x`", 10, "`` x` ``"),
            (
                "new trailing backtick from an internal run",
                "x`y",
                8,
                "``x``",
            ),
            ("leading backtick without padding room", "`x", 8, ""),
            ("leading backtick alone fits", "`x", 9, "`` ` ``"),
            ("leading backtick with payload fits", "`x", 10, "`` `x ``"),
            ("all backticks truncated", "``", 11, "``` ` ```"),
            ("all backticks fit", "``", 12, "``` `` ```"),
            ("mixed internal runs truncated", "a``b`c", 11, "```a```"),
            ("Unicode code point cannot fit", "😀`", 9, ""),
            ("ordinary payload cannot fit", "x", 3, ""),
            ("spaces cannot fit", "   ", 4, ""),
            ("space-only prefix fits", "   ", 5, "` `"),
            ("truncated prefix becomes space-only", " x", 6, "` `"),
            ("discarded trailing space", "x ", 5, "`x`"),
        ] {
            let prefix = "x".repeat(MAX_MARKDOWN_BYTES - 64 * 1024 - spare);
            let html = format!("<p>{prefix}<code>{content}</code></p>");
            assert!(html.len() <= MAX_MARKDOWN_BYTES);
            let out = md(&html);
            assert_eq!(&out[..prefix.len()], prefix, "{name}");
            assert_eq!(&out[prefix.len()..], expected, "{name}");
            assert!(out.len() <= MAX_MARKDOWN_BYTES);
        }
    }

    #[test]
    fn oc_webfetch_keeps_nested_lists_readable() {
        let html = "<ol start=\"3\"><li>alpha<ul><li>nested <strong>item</strong></li></ul></li><li><p>beta first</p><p>beta second</p></li></ol>";
        assert_eq!(
            md(html),
            "3. alpha\n\n   - nested **item**\n\n4. beta first\n\n   beta second"
        );
    }

    #[test]
    fn oc_webfetch_renders_blockquotes_and_tables() {
        let html = "<blockquote><p>quoted <em>text</em></p><ul><li>point</li></ul></blockquote><table><thead><tr><th>Name</th><th>Value</th></tr></thead><tbody><tr><td>one</td><td><code>1</code></td></tr></tbody></table>";
        assert_eq!(
            md(html),
            "> quoted *text*\n\n> - point\n\n| Name | Value |\n| --- | --- |\n| one | `1` |"
        );
    }

    #[test]
    fn oc_webfetch_decodes_entities_and_normalizes_whitespace() {
        let html = "<p>alpha\n  <span>&amp; beta</span> <unknown>caf&eacute;</unknown>&nbsp;gamma 😀</p><p>delta</p>";
        assert_eq!(md(html), "alpha & beta café\u{a0}gamma 😀\n\ndelta");
    }

    #[test]
    fn oc_webfetch_omits_active_and_fallback_content() {
        let html = r#"<p>before <script><b>bad</b></script><style>bad</style><noscript>bad</noscript><iframe>bad</iframe><object>bad</object><embed src="bad"><meta content="bad"><link href="bad"><template>bad</template> after</p>"#;
        assert_eq!(md(html), "before after");
    }

    #[test]
    fn oc_webfetch_is_deterministic_and_bounded_for_malformed_input() {
        let html = format!(
            "<main><p>{}</main></p></unknown>",
            "visible &amp; text ".repeat(4_096)
        );
        let first = md(&html);
        assert_eq!(md(&html), first);
        assert!(first.starts_with("visible & text visible & text"));
        assert!(first.len() <= html.len());
    }

    #[test]
    fn oc_webfetch_defaults_to_the_production_byte_budget() {
        let out = md(&"x".repeat(MAX_MARKDOWN_BYTES));
        assert_eq!(MAX_MARKDOWN_BYTES, 5 * 1024 * 1024);
        assert_eq!(out.len(), MAX_MARKDOWN_BYTES - 64 * 1024);
    }

    #[test]
    fn oc_webfetch_preserves_utf8_boundaries_at_the_content_limit() {
        for ch in ["x", "\u{e9}", "\u{1f600}"] {
            let budget = MAX_MARKDOWN_BYTES - 64 * 1024;
            let fitting = format!("aa{}", ch.repeat((budget - 2) / ch.len()));
            assert_eq!(md(&fitting), fitting);
            let truncated = md(&format!("{fitting}{ch}"));
            assert_eq!(truncated, fitting);
        }
    }

    #[test]
    fn oc_webfetch_bounds_deep_lists_and_fragmented_fences() {
        let lists = format!(
            "{}{}",
            "<ul><li>item".repeat(2_000),
            "</li></ul>".repeat(2_000)
        );
        let quotes = format!(
            "{}{}",
            "<blockquote><p>item".repeat(2_000),
            "</p></blockquote>".repeat(2_000)
        );
        let code = format!("<pre>{}</pre>", "` x ".repeat(4_096));
        assert!(md(&lists).len() < lists.len() * 4);
        assert!(md(&quotes).len() < quotes.len() * 4);
        let _ = md(&code);
        assert_eq!(
            md(&format!(
                "{}safe<script><b>bad</b>&amp;</script><p>tail &amp;</p>",
                "<div>".repeat(20_000)
            )),
            "safe tail &"
        );
    }

    #[test]
    fn oc_webfetch_escapes_prose_that_would_become_structure() {
        assert_eq!(
            md("<p># heading</p><p>1. item</p><p>---</p><p>a | b</p>"),
            "\\# heading\n\n1\\. item\n\n\\---\n\na \\| b"
        );
    }

    #[test]
    fn oc_webfetch_quotes_every_line_of_multiline_blocks() {
        let html = "<blockquote><pre>line  \n\n\nnext</pre><table><tr><td>a|b</td><td>c</td></tr></table></blockquote>";
        assert_eq!(
            md(html),
            "> ```\n> line  \n> \n> \n> next\n> ```\n\n> | a\\|b | c |\n> | --- | --- |"
        );
    }

    #[test]
    fn oc_webfetch_keeps_nested_blockquotes_inside_the_outer_quote() {
        let html =
            "<blockquote><p>outer</p><blockquote><p>inner</p></blockquote><p>end</p></blockquote>";
        assert_eq!(md(html), "> outer\n>\n> > inner\n>\n> end");
    }

    #[test]
    fn oc_webfetch_keeps_whitespace_around_inline_emphasis() {
        assert_eq!(
            md("<p>a<strong> b</strong> c a <em>b </em>c</p>"),
            "a **b** c a *b* c"
        );
        assert_eq!(md("a<strong> </strong>b a<em> </em>b"), "a b a b");
    }

    #[test]
    fn oc_webfetch_formatting_inside_pre_is_code_only() {
        assert_eq!(
            md("<pre><b>x</b><i>y</i><del>z</del></pre>"),
            "```\nxyz\n```"
        );
    }

    #[test]
    fn oc_webfetch_normalizes_multiline_table_cells() {
        let html = "<table><tr><td>x<br>y</td><td><code>a|b</code></td><td><p>first</p><p>second</p></td></tr></table>";
        assert_eq!(
            md(html),
            "| x y | `a\\|b` | first second |\n| --- | --- | --- |"
        );
    }

    #[test]
    fn oc_webfetch_flattens_nested_tables() {
        let html = "<table><tr><th>Parent</th><th>Sibling</th></tr><tr><td>Before<table><tr><th>Key</th><th>Value</th></tr><tr><td>A</td><td>1</td></tr></table>After</td><td>Tail</td></tr></table>";
        assert_eq!(
            md(html),
            "| Parent | Sibling |\n| --- | --- |\n| Before Key Value A 1 After | Tail |"
        );
    }

    #[test]
    fn oc_webfetch_preserves_loose_text_around_malformed_rows() {
        assert_eq!(
            md("<table>before<tr><td>cell</td></tr>after</table>"),
            "before after\n\n| cell |\n| --- |"
        );
        assert_eq!(md("<table>alpha</table>"), "alpha");
    }

    #[test]
    fn oc_webfetch_escapes_tilde_fences_and_drops_empty_emphasis() {
        assert_eq!(
            md("<p>~~~</p><p><strong></strong>content</p><p>~~~</p>"),
            "\\~\\~\\~\n\ncontent\n\n\\~\\~\\~"
        );
    }

    #[test]
    fn oc_webfetch_does_not_confuse_nul_text_with_code() {
        assert_eq!(
            md("<p>before \u{0}0\u{0} after</p><pre>code</pre>"),
            "before \u{0}0\u{0} after\n\n```\ncode\n```"
        );
    }

    #[test]
    fn oc_webfetch_preserves_multiline_inline_code() {
        assert_eq!(
            md("<p><code>first\n\n\nsecond  </code></p>"),
            "` first\n\n\nsecond   `"
        );
    }

    #[test]
    fn oc_webfetch_prefixes_inline_code_at_a_blockquote_line_start() {
        assert_eq!(md("<blockquote><code>x</code> y</blockquote>"), "> `x` y");
    }

    #[test]
    fn oc_webfetch_keeps_links_in_inline_code_with_their_text() {
        let html = "<dl><dt><code>socket = new <a href=\"#constructor\">WebSocket</a>(url)</code><dd>Creates one.</dl>";
        assert_eq!(
            md(html),
            "**` socket = new  `[`WebSocket`](#constructor)`(url)`**\n: Creates one."
        );
        assert_eq!(
            md("<code><a href=\"#x\">x</a></code> after"),
            "[`x`](#x) after"
        );
        assert_eq!(
            md("<dl><dt><code><var>socket</var> = new <code><a href=\"#constructor\">WebSocket</a></code>(<var>url</var>)</code><dd>Creates one.</dl>"),
            "**` socket = new  `[`WebSocket`](#constructor)`(url)`**\n: Creates one."
        );
        assert_eq!(
            md("<code>a<a href=\"/x\">b<a href=\"/y\">c</a>d</a>e</code>"),
            "`a`[`b`](/x)[`c`](/y)`de`"
        );
        assert_eq!(md("<code>a<a href=\"/x\">b</code>c"), "`a`[`b`](/x)c");
        assert_eq!(
            md("<code>a<a href=\"/x\"><div>b</div>c</a>d</code>"),
            "`a`[](/x)\n\n`bcd`"
        );
    }

    #[test]
    fn oc_webfetch_indents_list_continuations_and_numbering() {
        let html = "<ol start=\"0\"><li value=\"4\"><p>first</p><p>continued</p><ul><li><p>nested</p><p>continued nested</p></li></ul></li><li>next</li></ol>";
        assert_eq!(
            md(html),
            "4. first\n\n   continued\n\n   - nested\n\n     continued nested\n\n5. next"
        );
    }

    #[test]
    fn oc_webfetch_renders_block_content_outside_link_syntax() {
        assert_eq!(
            md("<a href=\"/docs\">before<div>block</div>after</a>"),
            "[before](/docs)\n\nblock\n\n[after](/docs)"
        );
    }

    #[test]
    fn oc_webfetch_recovers_nested_anchors() {
        assert_eq!(
            md("<a href=\"/a\">x<a href=\"/b\">y</a>z</a>"),
            "[x](/a)[y](/b)z"
        );
    }

    #[test]
    fn oc_webfetch_keeps_emphasis_whitespace_through_wrappers() {
        assert_eq!(
            md("<p>a<strong><span> bold</span></strong>c</p>"),
            "a **bold** c"
        );
    }

    #[test]
    fn oc_webfetch_flattens_pre_inside_table_cells() {
        let html = "<table><tr><td><pre>a|b\nnext</pre></td><td><code>x|y</code></td></tr></table>";
        assert_eq!(md(html), "| a\\|b next | `x\\|y` |\n| --- | --- |");
    }

    #[test]
    fn oc_webfetch_preserves_unicode_in_inline_constructs() {
        let p = "😀".repeat(16);
        for (html, expected) in [
            (format!("<strong>{p}</strong>"), format!("**{p}**")),
            (
                format!("<a href=\"/docs\">{p}</a>"),
                format!("[{p}](/docs)"),
            ),
            (
                format!("<img src=\"image.png\" alt=\"{p}\">"),
                format!("![{p}](image.png)"),
            ),
            (format!("<code>{p}</code>"), format!("`{p}`")),
        ] {
            assert_eq!(md(&html), expected);
        }
    }

    #[test]
    fn oc_webfetch_preserves_block_content_and_following_lists() {
        let p = "x".repeat(256);
        assert_eq!(
            md(&format!(
                "<table><tr><th>Name</th></tr><tr><td>{p}</td></tr></table>"
            )),
            format!("| Name |\n| --- |\n| {p} |")
        );
        assert_eq!(
            md(&format!("<ul><li>{p}</li></ul><ul><li>next</li></ul>")),
            format!("- {p}\n\n- next")
        );
        assert_eq!(md(&format!("<pre>{p}</pre>")), format!("```\n{p}\n```"));
    }

    #[test]
    fn oc_webfetch_long_delimiter_runs_stay_in_a_closed_fence() {
        let payload = format!("{}{}{}", "`".repeat(32), "~".repeat(32), "x".repeat(64));
        let fence = "`".repeat(33);
        assert_eq!(
            md(&format!("<blockquote><pre>{payload}</pre></blockquote>")),
            format!("> {fence}\n> {payload}\n> {fence}")
        );
    }

    #[test]
    fn oc_webfetch_separates_tables_from_adjacent_content() {
        let html = "intro<table><tr><td>x</td></tr></table>outro<blockquote>quote<table><tr><td>cell</td></tr></table></blockquote><ul><li>item<table><tr><td>cell</td></tr></table></li></ul>";
        assert_eq!(
            md(html),
            "intro\n\n| x |\n| --- |\n\noutro\n\n> quote\n\n> | cell |\n> | --- |\n\n- item\n\n| cell |\n| --- |"
        );
    }

    #[test]
    fn oc_webfetch_closes_multiline_quoted_code_before_prose() {
        let html = format!(
            "<blockquote><pre>{}</pre></blockquote><p>tail</p>",
            "x\n".repeat(16)
        );
        assert_eq!(
            md(&html),
            format!("> ```\n{}> ```\n\ntail", "> x\n".repeat(16))
        );
    }

    #[test]
    fn oc_webfetch_active_content_stays_suppressed_in_depth_fallback() {
        let html = format!(
            "<object>{}LEAK{}</object><p>visible</p>",
            "<div>".repeat(10_001),
            "</div>".repeat(10_001)
        );
        assert_eq!(md(&html), "visible");
    }

    #[test]
    fn oc_webfetch_visible_text_survives_depth_fallback_inside_pre() {
        let html = format!(
            "<pre>{}visible{}</pre><p>after</p>",
            "<i>".repeat(10_001),
            "</i>".repeat(10_001)
        );
        assert_eq!(md(&html), "visible after");
    }

    #[test]
    fn oc_webfetch_resumes_links_around_every_block() {
        let html = "<a href=\"/x\">before<blockquote><p>quote</p></blockquote><ul><li>item</li></ul><pre>code</pre><table><tr><td>cell</td></tr></table>after</a>";
        assert_eq!(
            md(html),
            "[before](/x)\n\n> quote\n\n- item\n\n```\ncode\n```\n\n| cell |\n| --- |\n\n[after](/x)"
        );
    }

    #[test]
    fn oc_webfetch_indents_child_lists_from_the_marker_width() {
        assert_eq!(
            md("<ol start=\"100\"><li>outer<ul><li>inner</li></ul></li></ol>"),
            "100. outer\n\n     - inner"
        );
    }

    #[test]
    fn oc_webfetch_renders_captions_and_definition_lists() {
        let html = "<table><caption>Cache modes</caption><tr><th>Name</th><th>Meaning</th></tr><tr><td>A</td><td>Local</td></tr></table><dl><dt>Cache</dt><dd>A local store</dd><dt>Origin</dt><dd>The remote source</dd></dl>";
        assert_eq!(
            md(html),
            "Cache modes\n\n| Name | Meaning |\n| --- | --- |\n| A | Local |\n\n**Cache**\n: A local store\n\n**Origin**\n: The remote source"
        );
    }

    #[test]
    fn oc_webfetch_falls_back_to_rows_for_table_spans() {
        let html = "<table><tr><th colspan=\"2\">Group</th></tr><tr><td>A</td><td rowspan=\"2\">Shared</td></tr><tr><td>B</td></tr></table>";
        assert_eq!(md(html), "Group\n\nA | Shared\n\nB");
    }

    #[test]
    fn oc_webfetch_suppresses_head_and_hidden_subtrees() {
        let html = "<head><title>noise</title></head><body><p>visible</p><div hidden>hidden</div><div aria-hidden=\"true\">aria</div><div aria-hidden=\"false\">shown</div></body>";
        assert_eq!(md(html), "visible\n\nshown");
    }

    #[test]
    fn oc_webfetch_preserves_pre_breaks_and_normalizes_link_titles() {
        let html = "<pre>first<br>second</pre><p><a href=\"/x\" title=\"line one\n  line two\">link</a></p>";
        assert_eq!(
            md(html),
            "```\nfirst\nsecond\n```\n\n[link](/x \"line one line two\")"
        );
    }

    #[test]
    fn oc_webfetch_renders_details_by_visibility() {
        let html = "<details><summary>Closed</summary><p>secret</p></details><details open><summary>Open</summary><p>visible</p></details>";
        assert_eq!(md(html), "Closed\n\nOpen\n\nvisible");
    }

    #[test]
    fn oc_html_markdown_budget_finishes_bounded_code_conversion() {
        let budget = MAX_MARKDOWN_BYTES - 64 * 1024;
        for (name, count, payload, suffix, quoted) in [
            ("exhausted budget", budget, "x", "", false),
            (
                "one byte short of an empty fence",
                budget - 13,
                "x",
                "",
                false,
            ),
            ("small fitting block", 64, "x", "\n\n```\nx\n```", false),
            (
                "last payload byte fits",
                budget - 15,
                "x",
                "\n\n```\nx\n```",
                false,
            ),
            (
                "only an empty fence fits",
                budget - 14,
                "x",
                "\n\n```\n\n```",
                false,
            ),
            (
                "Unicode payload truncates at a code point",
                budget - 18,
                "😀é",
                "\n\n```\n😀\n```",
                false,
            ),
            (
                "quoted payload fits",
                budget - 23,
                "x",
                "\n\n> ```\n> x\n> ```",
                true,
            ),
            (
                "only an empty quoted fence fits",
                budget - 22,
                "x",
                "\n\n> ```\n> \n> ```",
                true,
            ),
            (
                "one byte short of an empty quoted fence",
                budget - 21,
                "x",
                "",
                true,
            ),
        ] {
            let code = format!("<pre>{payload}</pre>");
            let html = format!(
                "<p>{}</p>{}",
                "x".repeat(count),
                if quoted {
                    format!("<blockquote>{code}</blockquote>")
                } else {
                    code
                }
            );
            assert!(html.len() <= MAX_MARKDOWN_BYTES);
            let start = std::time::Instant::now();
            let out = md(&html);
            assert!(
                start.elapsed() < std::time::Duration::from_secs(3),
                "{name}"
            );
            assert!(out.len() <= MAX_MARKDOWN_BYTES);
            assert_eq!(
                out,
                format!("{}{suffix}", "x".repeat(count.min(budget - 2))),
                "{name}"
            );
        }
    }

    #[test]
    fn deep_nesting_does_not_overflow_the_stack() {
        // htmd recursed per element and aborted the process here.
        let html = format!(
            "{}content{}",
            "<div>".repeat(50_000),
            "</div>".repeat(50_000)
        );
        assert_eq!(md(&html), "content");
        assert_eq!(to_text(&html), "content");
    }
}
