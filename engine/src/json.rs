//! Minimal JSON support for the control API: a small `Value` type, a
//! parser for the flat request objects the API accepts, and escaping for
//! responses. This is intentionally small — it is not a general JSON library.
//! Strings, numbers, booleans, null, arrays, and nested objects are parsed;
//! numbers are kept as f64 plus their raw text for integer fields.

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Value>),
    Obj(BTreeMap<String, Value>),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Num(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_arr(&self) -> Option<&Vec<Value>> {
        match self {
            Value::Arr(a) => Some(a),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Obj(m) => m.get(key),
            _ => None,
        }
    }

    pub fn req_str<'a>(&'a self, key: &str) -> Result<&'a str, String> {
        self.get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("missing or non-string field: {}", key))
    }

    pub fn req_num(&self, key: &str) -> Result<f64, String> {
        self.get(key)
            .and_then(Value::as_f64)
            .ok_or_else(|| format!("missing or non-numeric field: {}", key))
    }

    pub fn opt_bool(&self, key: &str, default: bool) -> bool {
        self.get(key).and_then(Value::as_bool).unwrap_or(default)
    }
}

/// Escape a string for inclusion in a JSON string literal (without quotes).
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

pub fn quote(s: &str) -> String {
    format!("\"{}\"", escape(s))
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn ws(&mut self) {
        while self.pos < self.bytes.len() && (self.bytes[self.pos] as char).is_whitespace() {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn expect(&mut self, b: u8) -> Result<(), String> {
        if self.peek() == Some(b) {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!("expected '{}' at {}", b as char, self.pos))
        }
    }

    fn value(&mut self) -> Result<Value, String> {
        self.ws();
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(Value::Str(self.string()?)),
            Some(b't') => self.literal("true", Value::Bool(true)),
            Some(b'f') => self.literal("false", Value::Bool(false)),
            Some(b'n') => self.literal("null", Value::Null),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            _ => Err(format!("unexpected input at {}", self.pos)),
        }
    }

    fn literal(&mut self, word: &str, v: Value) -> Result<Value, String> {
        if self.bytes.len() >= self.pos + word.len()
            && &self.bytes[self.pos..self.pos + word.len()] == word.as_bytes()
        {
            self.pos += word.len();
            Ok(v)
        } else {
            Err(format!("bad literal at {}", self.pos))
        }
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || matches!(c, b'-' | b'+' | b'.' | b'e' | b'E') {
                self.pos += 1;
            } else {
                break;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| "bad number encoding".to_string())?;
        text.parse::<f64>()
            .map(Value::Num)
            .map_err(|_| format!("bad number: {}", text))
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let c = self.peek().ok_or("unterminated string")?;
            self.pos += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let e = self.peek().ok_or("unterminated escape")?;
                    self.pos += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'u' => {
                            if self.pos + 4 > self.bytes.len() {
                                return Err("bad \\u escape".to_string());
                            }
                            let hex = std::str::from_utf8(&self.bytes[self.pos..self.pos + 4])
                                .map_err(|_| "bad \\u escape".to_string())?;
                            let cp = u32::from_str_radix(hex, 16)
                                .map_err(|_| "bad \\u escape".to_string())?;
                            out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                            self.pos += 4;
                        }
                        _ => return Err("bad escape".to_string()),
                    }
                }
                _ => {
                    // Copy the full UTF-8 sequence for this leading byte.
                    let len = utf8_len(c);
                    if self.pos - 1 + len > self.bytes.len() {
                        return Err("bad utf-8 in string".to_string());
                    }
                    let s = std::str::from_utf8(&self.bytes[self.pos - 1..self.pos - 1 + len])
                        .map_err(|_| "bad utf-8 in string".to_string())?;
                    out.push_str(s);
                    self.pos += len - 1;
                }
            }
        }
        Ok(out)
    }

    fn array(&mut self) -> Result<Value, String> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::Arr(items));
        }
        loop {
            items.push(self.value()?);
            self.ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err("expected ',' or ']' in array".to_string()),
            }
        }
        Ok(Value::Arr(items))
    }

    fn object(&mut self) -> Result<Value, String> {
        self.expect(b'{')?;
        let mut map = BTreeMap::new();
        self.ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Obj(map));
        }
        loop {
            self.ws();
            let key = self.string()?;
            self.ws();
            self.expect(b':')?;
            let val = self.value()?;
            map.insert(key, val);
            self.ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err("expected ',' or '}' in object".to_string()),
            }
        }
        Ok(Value::Obj(map))
    }
}

fn utf8_len(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first >> 5 == 0b110 {
        2
    } else if first >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

/// Parse a JSON document; trailing whitespace is allowed, trailing content
/// is rejected.
pub fn parse(input: &str) -> Result<Value, String> {
    let mut p = Parser {
        bytes: input.as_bytes(),
        pos: 0,
    };
    let v = p.value()?;
    p.ws();
    if p.pos != p.bytes.len() {
        return Err("trailing content after JSON value".to_string());
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_request_shapes() {
        let v = parse(r#"{"monitor":1,"x":720.5,"y":1280,"double":true,"keys":["ctrl","l"]}"#)
            .unwrap();
        assert_eq!(v.req_num("monitor").unwrap(), 1.0);
        assert_eq!(v.req_num("x").unwrap(), 720.5);
        assert!(v.opt_bool("double", false));
        assert_eq!(
            v.get("keys").unwrap().as_arr().unwrap().len(),
            2
        );
    }

    #[test]
    fn rejects_trailing_and_bad_input() {
        assert!(parse("{} junk").is_err());
        assert!(parse("{").is_err());
        assert!(parse("").is_err());
    }

    #[test]
    fn escaping_round_trips() {
        let raw = "quote \" backslash \\ newline \n tab \t";
        let q = quote(raw);
        let v = parse(&q).unwrap();
        assert_eq!(v.as_str().unwrap(), raw);
    }

    #[test]
    fn nested_objects_and_unicode() {
        let v = parse(r#"{"a":{"b":[1,2,{"c":"caf\u00e9"}]}}"#).unwrap();
        let c = v
            .get("a")
            .unwrap()
            .get("b")
            .unwrap()
            .as_arr()
            .unwrap()[2]
            .get("c")
            .unwrap()
            .as_str()
            .unwrap();
        assert_eq!(c, "café");
    }
}
