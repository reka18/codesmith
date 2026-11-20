//! Parsing of complete or incomplete JSON, for tool arguments that are still
//! streaming. A port of OpenCode's `protocols/utils/partial-json.ts`, itself
//! adapted from partial-json by the Promplate Dev Team
//! (<https://github.com/promplate/partial-json-parser-js>, MIT License,
//! Copyright (c) 2023 Promplate Dev Team).
//!
//! `NaN` and the infinities have no JSON value; when allowed they parse as
//! `null`.

use serde_json::{Map, Value};

/// Which kinds of value may be cut off. A cut-off value of a kind not
/// allowed is a [`JsonError::Partial`].
pub mod allow {
    /// `"hello \u12` parses as `"hello "`.
    pub const STR: u32 = 0b0_0000_0001;
    /// `123.` parses as `123`.
    pub const NUM: u32 = 0b0_0000_0010;
    /// `[1, 2,` parses as `[1, 2]`.
    pub const ARR: u32 = 0b0_0000_0100;
    /// `{"a": 1, "b":` parses as `{"a": 1}`.
    pub const OBJ: u32 = 0b0_0000_1000;
    /// `nu` parses as `null`.
    pub const NULL: u32 = 0b0_0001_0000;
    /// `tr` and `fa` parse as `true` and `false`.
    pub const BOOL: u32 = 0b0_0010_0000;
    pub const NAN: u32 = 0b0_0100_0000;
    pub const INFINITY: u32 = 0b0_1000_0000;
    pub const NEG_INFINITY: u32 = 0b1_0000_0000;
    pub const INF: u32 = INFINITY | NEG_INFINITY;
    pub const SPECIAL: u32 = NULL | BOOL | INF | NAN;
    pub const ATOM: u32 = STR | NUM | SPECIAL;
    pub const COLLECTION: u32 = ARR | OBJ;
    pub const ALL: u32 = ATOM | COLLECTION;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JsonError {
    #[error("{0} is empty")]
    Empty(String),
    /// The input stops inside a value whose kind may not be cut off.
    #[error("{0}")]
    Partial(String),
    /// The input is not JSON, however it would continue.
    #[error("{0}")]
    Malformed(String),
}

/// Parse complete or incomplete JSON, allowing every kind to be cut off.
pub fn parse(input: &str) -> Result<Value, JsonError> {
    parse_with(input, allow::ALL)
}

/// Parse complete or incomplete JSON; `allowed` says which kinds of value
/// may be cut off (see [`allow`]).
pub fn parse_with(input: &str, allowed: u32) -> Result<Value, JsonError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(JsonError::Empty(input.to_string()));
    }
    if let Ok(v) = serde_json::from_str(trimmed) {
        return Ok(v);
    }
    let repaired = repair(trimmed);
    if repaired != trimmed {
        if let Ok(v) = serde_json::from_str(&repaired) {
            return Ok(v);
        }
    }
    match Parser::new(trimmed, allowed).any() {
        Ok(v) => Ok(v),
        Err(_) if repaired != trimmed => Parser::new(&repaired, allowed).any(),
        Err(e) => Err(e),
    }
}

/// The partial value of streamed tool arguments: what parses so far, or an
/// empty object.
pub fn partial_input(raw: &str) -> Value {
    match parse(raw) {
        Ok(v) => v,
        Err(_) => Value::Object(Map::new()),
    }
}

/// Escape raw control characters and invalid escapes inside strings.
fn repair(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut quoted = false;
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if !quoted {
            out.push(c);
            quoted = c == '"';
        } else if c == '"' {
            out.push(c);
            quoted = false;
        } else if c == '\\' {
            let next = chars.get(i + 1).copied();
            let hex = chars
                .get(i + 2..i + 6)
                .is_some_and(|h| h.iter().all(char::is_ascii_hexdigit));
            if next == Some('u') && hex {
                out.extend(&chars[i..i + 6]);
                i += 5;
            } else if let Some(n) = next.filter(|n| "\"\\/bfnrtu".contains(*n)) {
                out.push('\\');
                out.push(n);
                i += 1;
            } else {
                out.push_str("\\\\");
            }
        } else if (c as u32) <= 0x1f {
            out.push_str(&format!("\\u{:04x}", c as u32));
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
}

struct Parser<'a> {
    src: &'a str,
    s: &'a [u8],
    i: usize,
    allow: u32,
}

fn decode(s: &str) -> Result<Value, String> {
    serde_json::from_str(s).map_err(|e| e.to_string())
}

impl<'a> Parser<'a> {
    fn new(src: &'a str, allow: u32) -> Self {
        Parser {
            src,
            s: src.as_bytes(),
            i: 0,
            allow,
        }
    }

    fn allows(&self, kind: u32) -> bool {
        self.allow & kind != 0
    }

    fn partial(&self, message: &str) -> JsonError {
        JsonError::Partial(format!("{message} at position {}", self.i))
    }

    fn malformed(&self, message: &str) -> JsonError {
        JsonError::Malformed(format!("{message} at position {}", self.i))
    }

    fn at(&self, i: usize) -> Option<u8> {
        self.s.get(i).copied()
    }

    /// The source from `start` to `end`, clamped to the input.
    fn slice(&self, start: usize, end: usize) -> &'a str {
        let end = end.min(self.s.len());
        self.src.get(start.min(end)..end).unwrap_or("")
    }

    fn skip_blank(&mut self) {
        while matches!(self.at(self.i), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.i += 1;
        }
    }

    /// Whether a literal starts here, whole or (when allowed) cut off.
    fn literal(&self, word: &str, kind: u32, min_rest: usize) -> bool {
        let rest = self.slice(self.i, self.s.len());
        rest.starts_with(word)
            || (self.allows(kind)
                && rest.len() > min_rest
                && rest.len() < word.len()
                && word.starts_with(rest))
    }

    fn any(&mut self) -> Result<Value, JsonError> {
        self.skip_blank();
        let Some(c) = self.at(self.i) else {
            return Err(self.partial("Unexpected end of input"));
        };
        match c {
            b'"' => return self.string().map(Value::String),
            b'{' => return self.object(),
            b'[' => return self.array(),
            _ => {}
        }
        let literals: [(&str, u32, usize, Value); 6] = [
            ("null", allow::NULL, 0, Value::Null),
            ("true", allow::BOOL, 0, Value::Bool(true)),
            ("false", allow::BOOL, 0, Value::Bool(false)),
            ("Infinity", allow::INFINITY, 0, Value::Null),
            ("-Infinity", allow::NEG_INFINITY, 1, Value::Null),
            ("NaN", allow::NAN, 0, Value::Null),
        ];
        for (word, kind, min_rest, value) in literals {
            if self.literal(word, kind, min_rest) {
                self.i += word.len();
                return Ok(value);
            }
        }
        self.number()
    }

    fn string(&mut self) -> Result<String, JsonError> {
        let start = self.i;
        let mut escape = false;
        self.i += 1;
        while self.i < self.s.len()
            && (self.s[self.i] != b'"' || (escape && self.s[self.i - 1] == b'\\'))
        {
            escape = if self.s[self.i] == b'\\' {
                !escape
            } else {
                false
            };
            self.i += 1;
        }
        let as_string = |s: &str| -> Result<String, String> {
            serde_json::from_str::<String>(s).map_err(|e| e.to_string())
        };
        if self.at(self.i) == Some(b'"') {
            self.i += 1;
            let raw = self.slice(start, self.i - usize::from(escape));
            return as_string(raw).map_err(|e| self.malformed(&e));
        }
        if self.allows(allow::STR) {
            let body = self.slice(start, self.i - usize::from(escape));
            if let Ok(s) = as_string(&format!("{body}\"")) {
                return Ok(s);
            }
            // Cut inside an escape: drop it.
            let cut = self.src.rfind('\\').filter(|&b| b > start);
            return match cut {
                Some(b) => as_string(&format!("{}\"", self.slice(start, b)))
                    .map_err(|e| self.malformed(&e)),
                None => Err(self.malformed("Unterminated string literal")),
            };
        }
        Err(self.partial("Unterminated string literal"))
    }

    fn object(&mut self) -> Result<Value, JsonError> {
        self.i += 1;
        self.skip_blank();
        let mut object = Map::new();
        let res: Result<(), JsonError> = (|| {
            while self.at(self.i) != Some(b'}') {
                let before = self.i;
                self.skip_blank();
                if self.i >= self.s.len() && self.allows(allow::OBJ) {
                    return Ok(());
                }
                let key = self.string()?;
                self.skip_blank();
                self.i += 1;
                match self.any() {
                    Ok(v) => {
                        object.insert(key, v);
                    }
                    Err(_) if self.allows(allow::OBJ) => return Ok(()),
                    Err(e) => return Err(e),
                }
                self.skip_blank();
                if self.at(self.i) == Some(b',') {
                    self.i += 1;
                }
                if self.i <= before || self.i > self.s.len() {
                    return Err(self.partial("Unexpected end of input"));
                }
            }
            self.i += 1;
            Ok(())
        })();
        match res {
            Ok(()) => Ok(Value::Object(object)),
            Err(_) if self.allows(allow::OBJ) => Ok(Value::Object(object)),
            Err(_) => Err(self.partial("Expected '}' at end of object")),
        }
    }

    fn array(&mut self) -> Result<Value, JsonError> {
        self.i += 1;
        let mut array = Vec::new();
        while self.at(self.i) != Some(b']') {
            let before = self.i;
            match self.any() {
                Ok(v) => array.push(v),
                Err(_) if self.allows(allow::ARR) => return Ok(Value::Array(array)),
                Err(_) => return Err(self.partial("Expected ']' at end of array")),
            }
            self.skip_blank();
            if self.at(self.i) == Some(b',') {
                self.i += 1;
            }
            if self.i <= before || self.i > self.s.len() {
                return match self.allows(allow::ARR) {
                    true => Ok(Value::Array(array)),
                    false => Err(self.partial("Expected ']' at end of array")),
                };
            }
        }
        self.i += 1;
        Ok(Value::Array(array))
    }

    /// A number up to the end of the value, or without a dangling exponent.
    fn number(&mut self) -> Result<Value, JsonError> {
        let before_exponent = |s: &str, start: usize| {
            s.rfind('e')
                .filter(|&e| e > start)
                .and_then(|e| decode(&s[start..e]).ok())
        };
        if self.i == 0 {
            if self.src == "-" {
                return Err(self.malformed("Not sure what '-' is"));
            }
            return match decode(self.src) {
                Ok(v) => Ok(v),
                Err(e) => {
                    let cut = self
                        .allows(allow::NUM)
                        .then(|| {
                            self.src
                                .rfind('e')
                                .and_then(|end| decode(&self.src[..end]).ok())
                        })
                        .flatten();
                    cut.ok_or_else(|| self.malformed(&e))
                }
            };
        }
        let start = self.i;
        if self.at(self.i) == Some(b'-') {
            self.i += 1;
        }
        while self.at(self.i).is_some_and(|c| !b",]}".contains(&c)) {
            self.i += 1;
        }
        if self.i == self.s.len() && !self.allows(allow::NUM) {
            return Err(self.partial("Unterminated number literal"));
        }
        let text = self.slice(start, self.i);
        match decode(text) {
            Ok(v) => Ok(v),
            Err(_) if text == "-" => Err(self.partial("Not sure what '-' is")),
            Err(e) => before_exponent(self.src, start).ok_or_else(|| self.malformed(&e)),
        }
    }
}

#[cfg(test)]
mod tests {
    //! Ports of OpenCode's `partial-json.test.ts`.
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_complete_json() {
        assert_eq!(
            parse(r#"{"key":"value","items":[1,true,null]}"#).unwrap(),
            json!({"key": "value", "items": [1, true, null]})
        );
        let object = parse(r#"{"__proto__":{"safe":true}}"#).unwrap();
        assert!(object.get("__proto__").is_some());
    }

    #[test]
    fn parses_partial_strings() {
        assert_eq!(parse(r#""hello"#).unwrap(), json!("hello"));
        assert_eq!(parse(r#""hello \u12"#).unwrap(), json!("hello "));
        assert!(matches!(
            parse_with(r#""hello"#, !allow::STR),
            Err(JsonError::Partial(_))
        ));
    }

    #[test]
    fn repairs_invalid_escapes_and_raw_control_characters() {
        assert_eq!(
            parse("{\"path\":\"A\\H\",\"text\":\"first\tsecond\"}").unwrap(),
            json!({"path": "A\\H", "text": "first\tsecond"})
        );
    }

    #[test]
    fn preserves_prototype_keys_in_partial_objects() {
        let object = parse(r#"{"__proto__":{"safe":true}"#).unwrap();
        assert_eq!(object, json!({"__proto__": {"safe": true}}));
    }

    #[test]
    fn controls_partial_collection_values_independently() {
        assert_eq!(parse_with(r#"[""#, allow::ARR).unwrap(), json!([]));
        assert_eq!(
            parse_with(r#"[""#, allow::ARR | allow::STR).unwrap(),
            json!([""])
        );
        assert_eq!(parse_with(r#"{"key":""#, allow::OBJ).unwrap(), json!({}));
        assert_eq!(
            parse_with(r#"{"key":""#, allow::OBJ | allow::STR).unwrap(),
            json!({"key": ""})
        );
    }

    #[test]
    fn parses_partial_literals_and_numbers() {
        assert_eq!(parse_with("nu", allow::NULL).unwrap(), Value::Null);
        assert_eq!(parse_with("tr", allow::BOOL).unwrap(), json!(true));
        assert_eq!(parse_with("fa", allow::BOOL).unwrap(), json!(false));
        assert_eq!(parse_with("1e", allow::NUM).unwrap(), json!(1));
    }

    #[test]
    fn distinguishes_disallowed_partial_values_from_malformed_values() {
        assert!(matches!(
            parse_with("[", allow::STR),
            Err(JsonError::Partial(_))
        ));
        assert!(matches!(
            parse_with("n", !allow::NULL),
            Err(JsonError::Malformed(_))
        ));
    }

    #[test]
    fn rejects_empty_input() {
        assert!(parse("  ").unwrap_err().to_string().contains("is empty"));
    }

    #[test]
    fn streamed_tool_arguments() {
        let raw = r#"{"path":"src/main.rs","content":"fn main() {\n    println!(\"hi"#;
        assert_eq!(
            partial_input(raw),
            json!({"path": "src/main.rs", "content": "fn main() {\n    println!(\"hi"})
        );
        assert_eq!(partial_input(r#"{"a":[1,2"#), json!({"a": [1, 2]}));
        assert_eq!(partial_input(r#"{"a":-"#), json!({}));
        assert_eq!(partial_input(""), json!({}));
        // Every prefix of a document parses without panicking.
        let doc = r#"{"k": [1, -2.5e3, "x\"yé", {"n": null, "t": true}], "é": "ü"}"#;
        for (i, _) in doc.char_indices() {
            let _ = parse(&doc[..i]);
        }
        assert_eq!(parse(doc).unwrap()["é"], json!("ü"));
    }
}
