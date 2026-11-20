// json.rs — minimal JSON value type, parser and serializer (std only).
//
// Behaviour mirrors JSON.parse / JSON.stringify for the shapes the forge
// sends and receives: objects keep insertion order, integers serialise
// without a fractional part, strings escape the JSON control set.

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    /// Parse a whole JSON document. Errors are human-readable positions.
    pub fn parse(input: &str) -> Result<Json, String> {
        let mut p = Parser {
            bytes: input.as_bytes(),
            pos: 0,
            depth: 0,
        };
        p.skip_ws();
        let value = p.parse_value()?;
        p.skip_ws();
        if p.pos != p.bytes.len() {
            return Err(format!("trailing characters at byte {}", p.pos));
        }
        Ok(value)
    }

    /// Serialise to compact JSON text.
    pub fn to_string(&self) -> String {
        let mut out = String::new();
        self.write_into(&mut out);
        out
    }

    fn write_into(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Num(n) => out.push_str(&fmt_num(*n)),
            Json::Str(s) => escape_str(s, out),
            Json::Arr(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write_into(out);
                }
                out.push(']');
            }
            Json::Obj(pairs) => {
                out.push('{');
                for (i, (key, value)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    escape_str(key, out);
                    out.push(':');
                    value.write_into(out);
                }
                out.push('}');
            }
        }
    }

    /// Object member lookup (first match wins).
    pub fn get(&self, key: &str) -> Option<&Json> {
        if let Json::Obj(pairs) = self {
            pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v)
        } else {
            None
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        if let Json::Str(s) = self {
            Some(s)
        } else {
            None
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        if let Json::Bool(b) = self {
            Some(*b)
        } else {
            None
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        if let Json::Num(n) = self {
            Some(*n)
        } else {
            None
        }
    }

    /// Integer view of a number: only when it is finite and whole.
    pub fn as_i64(&self) -> Option<i64> {
        if let Json::Num(n) = self {
            if n.is_finite() && n.fract() == 0.0 && *n >= i64::MIN as f64 && *n <= i64::MAX as f64 {
                Some(*n as i64)
            } else {
                None
            }
        } else {
            None
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }
}

/// Format a number the way JSON.stringify does for ordinary magnitudes:
/// whole values print without a fraction; anything else prints shortest
/// round-trip decimal. (Extreme magnitudes >= 1e21 print in full decimal
/// here instead of JS exponent form; the forge never emits those.)
fn fmt_num(n: f64) -> String {
    if n == 0.0 {
        return "0".to_string();
    }
    if !n.is_finite() {
        return "null".to_string();
    }
    if n.fract() == 0.0 && n.abs() < 9_007_199_254_740_992.0 {
        return format!("{}", n as i64);
    }
    format!("{}", n)
}

fn escape_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

const MAX_DEPTH: usize = 512;

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() {
            match self.bytes[self.pos] {
                b' ' | b'\t' | b'\n' | b'\r' => self.pos += 1,
                _ => break,
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn parse_value(&mut self) -> Result<Json, String> {
        if self.depth >= MAX_DEPTH {
            return Err("JSON nesting too deep".to_string());
        }
        match self.peek() {
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
            Some(b'"') => Ok(Json::Str(self.parse_string()?)),
            Some(b't') => self.parse_literal("true", Json::Bool(true)),
            Some(b'f') => self.parse_literal("false", Json::Bool(false)),
            Some(b'n') => self.parse_literal("null", Json::Null),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.parse_number(),
            Some(c) => Err(format!("unexpected character '{}' at byte {}", c as char, self.pos)),
            None => Err("unexpected end of input".to_string()),
        }
    }

    fn parse_literal(&mut self, word: &str, value: Json) -> Result<Json, String> {
        if self.bytes.len() - self.pos < word.len()
            || &self.bytes[self.pos..self.pos + word.len()] != word.as_bytes()
        {
            return Err(format!("invalid literal at byte {}", self.pos));
        }
        self.pos += word.len();
        Ok(value)
    }

    fn parse_number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => {
                self.pos += 1;
            }
            Some(c) if c.is_ascii_digit() => {
                while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                    self.pos += 1;
                }
            }
            _ => return Err(format!("invalid number at byte {}", start)),
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                return Err(format!("invalid number at byte {}", start));
            }
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                return Err(format!("invalid number at byte {}", start));
            }
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| "invalid number".to_string())?;
        text.parse::<f64>()
            .map(Json::Num)
            .map_err(|_| format!("invalid number at byte {}", start))
    }

    fn parse_string(&mut self) -> Result<String, String> {
        // self.bytes[self.pos] == b'"'
        self.pos += 1;
        let mut out = String::new();
        loop {
            let c = match self.peek() {
                Some(c) => c,
                None => return Err("unterminated string".to_string()),
            };
            match c {
                b'"' => {
                    self.pos += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.pos += 1;
                    let esc = self.peek().ok_or("unterminated escape")?;
                    self.pos += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{08}'),
                        b'f' => out.push('\u{0c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let first = self.parse_hex4()?;
                            if (0xD800..0xDC00).contains(&first) {
                                // High surrogate: expect a low surrogate next.
                                if self.peek() == Some(b'\\')
                                    && self.bytes.get(self.pos + 1) == Some(&b'u')
                                {
                                    self.pos += 2;
                                    let second = self.parse_hex4()?;
                                    if (0xDC00..0xE000).contains(&second) {
                                        let combined = 0x10000
                                            + (((first - 0xD800) as u32) << 10)
                                            + (second - 0xDC00) as u32;
                                        out.push(char::from_u32(combined).unwrap_or('\u{FFFD}'));
                                    } else {
                                        out.push('\u{FFFD}');
                                        out.push(char::from_u32(second as u32).unwrap_or('\u{FFFD}'));
                                    }
                                } else {
                                    out.push('\u{FFFD}');
                                }
                            } else if (0xDC00..0xE000).contains(&first) {
                                out.push('\u{FFFD}');
                            } else {
                                out.push(char::from_u32(first as u32).unwrap_or('\u{FFFD}'));
                            }
                        }
                        _ => return Err(format!("invalid escape at byte {}", self.pos - 1)),
                    }
                }
                0x00..=0x1F => {
                    return Err(format!("control character in string at byte {}", self.pos));
                }
                _ => {
                    // Copy one UTF-8 code point verbatim.
                    let rest = &self.bytes[self.pos..];
                    let s = std::str::from_utf8(rest).map_err(|_| "invalid utf-8 in string")?;
                    let ch = s.chars().next().unwrap();
                    out.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
    }

    fn parse_hex4(&mut self) -> Result<u16, String> {
        if self.bytes.len() - self.pos < 4 {
            return Err("truncated \\u escape".to_string());
        }
        let mut value: u16 = 0;
        for i in 0..4 {
            let c = self.bytes[self.pos + i];
            let digit = match c {
                b'0'..=b'9' => (c - b'0') as u16,
                b'a'..=b'f' => (c - b'a' + 10) as u16,
                b'A'..=b'F' => (c - b'A' + 10) as u16,
                _ => return Err(format!("invalid \\u escape at byte {}", self.pos + i)),
            };
            value = (value << 4) | digit;
        }
        self.pos += 4;
        Ok(value)
    }

    fn parse_array(&mut self) -> Result<Json, String> {
        self.depth += 1;
        self.pos += 1; // '['
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(Json::Arr(items));
        }
        loop {
            self.skip_ws();
            items.push(self.parse_value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    self.depth -= 1;
                    return Ok(Json::Arr(items));
                }
                _ => return Err(format!("expected ',' or ']' at byte {}", self.pos)),
            }
        }
    }

    fn parse_object(&mut self) -> Result<Json, String> {
        self.depth += 1;
        self.pos += 1; // '{'
        let mut pairs: Vec<(String, Json)> = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(Json::Obj(pairs));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(format!("expected object key at byte {}", self.pos));
            }
            let key = self.parse_string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(format!("expected ':' at byte {}", self.pos));
            }
            self.pos += 1;
            self.skip_ws();
            let value = self.parse_value()?;
            pairs.push((key, value));
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    self.depth -= 1;
                    return Ok(Json::Obj(pairs));
                }
                _ => return Err(format!("expected ',' or '}}' at byte {}", self.pos)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_complex() {
        let text = r#"{"a":[1,2.5,-3,"x\ny",true,false,null],"b":{"c":"\u0041"}}"#;
        let v = Json::parse(text).unwrap();
        assert_eq!(v.get("a").unwrap().as_str(), None);
        assert_eq!(v.get("b").unwrap().get("c").unwrap().as_str(), Some("A"));
        assert_eq!(v.to_string(), r#"{"a":[1,2.5,-3,"x\ny",true,false,null],"b":{"c":"A"}}"#);
        // Parse the serialised form again: identical value.
        assert_eq!(Json::parse(&v.to_string()).unwrap(), v);
    }

    #[test]
    fn escapes_and_surrogates() {
        let v = Json::parse(r#""\uD83D\uDE00""#).unwrap();
        assert_eq!(v.as_str(), Some("\u{1F600}"));
        assert_eq!(v.to_string(), "\"\u{1F600}\"");
        let v = Json::parse(r#""a\"b\\c\/d\b\f\n\r\t""#).unwrap();
        assert_eq!(v.as_str(), Some("a\"b\\c/d\u{08}\u{0c}\n\r\t"));
        assert_eq!(v.to_string(), r#""a\"b\\c/d\b\f\n\r\t""#);
    }

    #[test]
    fn numbers() {
        assert_eq!(Json::parse("42").unwrap().as_i64(), Some(42));
        assert_eq!(Json::parse("42").unwrap().to_string(), "42");
        assert_eq!(Json::parse("-0").unwrap().to_string(), "0");
        assert_eq!(Json::parse("1.5").unwrap().to_string(), "1.5");
        assert_eq!(Json::parse("1e3").unwrap().to_string(), "1000");
        assert_eq!(Json::parse("0.1").unwrap().to_string(), "0.1");
        assert!(Json::parse("01").is_err());
        assert!(Json::parse("1.").is_err());
        assert!(Json::parse("-").is_err());
        assert!(Json::parse("1e").is_err());
    }

    #[test]
    fn malformed_rejected() {
        for bad in [
            "",
            "{",
            "[1,]",
            "{\"a\":}",
            "{\"a\" 1}",
            "\"unterminated",
            "tru",
            "nulll",
            "[1 2]",
            "{\"a\":1} extra",
            "\"bad\tescape\"",
            "\"\\q\"",
            "\"\\u12\"",
            "\"\\uZZZZ\"",
        ] {
            assert!(Json::parse(bad).is_err(), "should reject: {:?}", bad);
        }
    }

    #[test]
    fn empty_and_literals() {
        assert_eq!(Json::parse("{}").unwrap(), Json::Obj(vec![]));
        assert_eq!(Json::parse("[]").unwrap(), Json::Arr(vec![]));
        assert_eq!(Json::parse("null").unwrap(), Json::Null);
        assert_eq!(Json::parse(" true ").unwrap(), Json::Bool(true));
        assert_eq!(Json::parse("\"\"").unwrap(), Json::Str(String::new()));
    }

    #[test]
    fn helpers() {
        let v = Json::parse("{\"n\":7,\"s\":\"x\",\"b\":true}").unwrap();
        assert_eq!(v.get("n").unwrap().as_i64(), Some(7));
        assert_eq!(v.get("n").unwrap().as_f64(), Some(7.0));
        assert_eq!(v.get("s").unwrap().as_str(), Some("x"));
        assert_eq!(v.get("b").unwrap().as_bool(), Some(true));
        assert_eq!(v.get("missing"), None);
        assert_eq!(v.get("n").unwrap().as_str(), None);
    }
}
