//! A minimal read-only JSON value parser.
//!
//! `framediff` needs to read one file we ourselves wrote (`gpu-snapshots/*/draws.json`).
//! The workspace deliberately carries no serde, and pulling a serialization stack into the
//! tree to read one debug artefact is a poor trade — so this is the smallest complete
//! reader that does the job: full JSON grammar, no derive, no dependencies, no writer.
//!
//! It is a PARSER, not a validator: it rejects malformed input with a message and offset,
//! and does not attempt recovery.

use std::collections::BTreeMap;
use std::fmt;

/// A parsed JSON value.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    /// All numbers are kept as `f64`; `as_i64` narrows when the caller wants an integer.
    Number(f64),
    String(String),
    Array(Vec<Json>),
    Object(BTreeMap<String, Json>),
}

impl Json {
    /// Parse a whole document. Trailing non-whitespace is an error.
    pub fn parse(s: &str) -> Result<Json, ParseError> {
        let b = s.as_bytes();
        let mut p = Parser { b, i: 0 };
        p.ws();
        let v = p.value()?;
        p.ws();
        if p.i != b.len() {
            return Err(p.err("trailing data after top-level value"));
        }
        Ok(v)
    }

    /// Field lookup on an object; `None` for any other kind.
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(m) => m.get(key),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Json::Number(n) => Some(*n as i64),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }
}

/// A parse failure, with the byte offset it was detected at.
#[derive(Debug)]
pub struct ParseError {
    pub offset: usize,
    pub msg: &'static str,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at byte {}", self.msg, self.offset)
    }
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn err(&self, msg: &'static str) -> ParseError {
        ParseError {
            offset: self.i,
            msg,
        }
    }

    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn eat(&mut self, c: u8) -> Result<(), ParseError> {
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        } else {
            Err(self.err("expected a specific delimiter"))
        }
    }

    fn lit(&mut self, s: &str) -> bool {
        if self.b[self.i..].starts_with(s.as_bytes()) {
            self.i += s.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self) -> Result<Json, ParseError> {
        match self.peek().ok_or_else(|| self.err("unexpected end"))? {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => Ok(Json::String(self.string()?)),
            b't' if self.lit("true") => Ok(Json::Bool(true)),
            b'f' if self.lit("false") => Ok(Json::Bool(false)),
            b'n' if self.lit("null") => Ok(Json::Null),
            b'-' | b'0'..=b'9' => self.number(),
            _ => Err(self.err("unexpected character at value")),
        }
    }

    fn object(&mut self) -> Result<Json, ParseError> {
        self.eat(b'{')?;
        let mut m = BTreeMap::new();
        self.ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(Json::Object(m));
        }
        loop {
            self.ws();
            let k = self.string()?;
            self.ws();
            self.eat(b':')?;
            self.ws();
            let v = self.value()?;
            m.insert(k, v);
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Json::Object(m));
                }
                _ => return Err(self.err("expected ',' or '}' in object")),
            }
        }
    }

    fn array(&mut self) -> Result<Json, ParseError> {
        self.eat(b'[')?;
        let mut a = Vec::new();
        self.ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Json::Array(a));
        }
        loop {
            self.ws();
            a.push(self.value()?);
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Json::Array(a));
                }
                _ => return Err(self.err("expected ',' or ']' in array")),
            }
        }
    }

    fn string(&mut self) -> Result<String, ParseError> {
        self.eat(b'"')?;
        let mut s = String::new();
        loop {
            let c = self.peek().ok_or_else(|| self.err("unterminated string"))?;
            self.i += 1;
            match c {
                b'"' => return Ok(s),
                b'\\' => {
                    let e = self.peek().ok_or_else(|| self.err("unterminated escape"))?;
                    self.i += 1;
                    match e {
                        b'"' => s.push('"'),
                        b'\\' => s.push('\\'),
                        b'/' => s.push('/'),
                        b'b' => s.push('\u{8}'),
                        b'f' => s.push('\u{c}'),
                        b'n' => s.push('\n'),
                        b'r' => s.push('\r'),
                        b't' => s.push('\t'),
                        b'u' => {
                            let cp = self.hex4()?;
                            // A surrogate pair encodes one non-BMP scalar; a lone surrogate
                            // is replaced rather than aborting the whole file.
                            let ch = if (0xD800..0xDC00).contains(&cp) {
                                let save = self.i;
                                if self.peek() == Some(b'\\') {
                                    self.i += 1;
                                    if self.peek() == Some(b'u') {
                                        self.i += 1;
                                        let lo = self.hex4()?;
                                        if (0xDC00..0xE000).contains(&lo) {
                                            let c = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                            char::from_u32(c).unwrap_or('\u{FFFD}')
                                        } else {
                                            self.i = save;
                                            '\u{FFFD}'
                                        }
                                    } else {
                                        self.i = save;
                                        '\u{FFFD}'
                                    }
                                } else {
                                    '\u{FFFD}'
                                }
                            } else {
                                char::from_u32(cp).unwrap_or('\u{FFFD}')
                            };
                            s.push(ch);
                        }
                        _ => return Err(self.err("unknown string escape")),
                    }
                }
                // Raw UTF-8 bytes pass through; the input came from a Rust String.
                _ => {
                    let start = self.i - 1;
                    let len = utf8_len(c);
                    self.i = start + len;
                    match std::str::from_utf8(&self.b[start..self.i.min(self.b.len())]) {
                        Ok(t) => s.push_str(t),
                        Err(_) => return Err(self.err("invalid UTF-8 in string")),
                    }
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, ParseError> {
        if self.i + 4 > self.b.len() {
            return Err(self.err("truncated \\u escape"));
        }
        let s = std::str::from_utf8(&self.b[self.i..self.i + 4])
            .map_err(|_| self.err("bad \\u escape"))?;
        let v = u32::from_str_radix(s, 16).map_err(|_| self.err("bad \\u escape"))?;
        self.i += 4;
        Ok(v)
    }

    fn number(&mut self) -> Result<Json, ParseError> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        while matches!(
            self.peek(),
            Some(b'0'..=b'9') | Some(b'.') | Some(b'e') | Some(b'E') | Some(b'+') | Some(b'-')
        ) {
            self.i += 1;
        }
        std::str::from_utf8(&self.b[start..self.i])
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .map(Json::Number)
            .ok_or(ParseError {
                offset: start,
                msg: "malformed number",
            })
    }
}

/// Byte length of the UTF-8 sequence a leading byte starts.
fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_shapes_draws_json_uses() {
        let v = Json::parse(
            r#"{"frame": 2143, "draws": [{"ordinal": 0, "kind": "DrawIndexAuto",
               "target": {"base": "0x9afc30000", "width": 320},
               "sampled": [{"binding": 1, "descriptor_honoured": false}],
               "x": null, "y": -1.5e2}]}"#,
        )
        .expect("parse");
        assert_eq!(v.get("frame").and_then(|x| x.as_i64()), Some(2143));
        let d = &v.get("draws").unwrap().as_array().unwrap()[0];
        assert_eq!(
            d.get("kind").and_then(|x| x.as_str()),
            Some("DrawIndexAuto")
        );
        assert_eq!(
            d.get("target")
                .and_then(|t| t.get("base"))
                .and_then(|x| x.as_str()),
            Some("0x9afc30000")
        );
        let s = &d.get("sampled").unwrap().as_array().unwrap()[0];
        assert_eq!(
            s.get("descriptor_honoured").and_then(|x| x.as_bool()),
            Some(false)
        );
        assert_eq!(d.get("x"), Some(&Json::Null));
        assert_eq!(d.get("y").and_then(|x| x.as_i64()), Some(-150));
    }

    #[test]
    fn handles_escapes_and_unicode() {
        let v = Json::parse(r#"{"a": "line\nbreak — dash \"q\""}"#).unwrap();
        assert_eq!(
            v.get("a").and_then(|x| x.as_str()),
            Some("line\nbreak — dash \"q\"")
        );
    }

    #[test]
    fn empty_containers_and_errors() {
        assert_eq!(Json::parse("{}").unwrap(), Json::Object(Default::default()));
        assert_eq!(Json::parse("[]").unwrap(), Json::Array(vec![]));
        assert!(Json::parse("{\"a\": }").is_err());
        assert!(Json::parse("[1,2] junk").is_err());
    }
}
