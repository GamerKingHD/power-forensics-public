//! Minimal JSON parser (std-only). Our sessions are JSONL; the analysis
//! engine reads them back without new dependencies. Supports the full
//! JSON grammar (nesting capped for safety), precise-ish errors.

#[derive(Debug, Clone, PartialEq)]
pub enum JVal {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<JVal>),
    Obj(Vec<(String, JVal)>),
}

impl JVal {
    pub fn get(&self, key: &str) -> Option<&JVal> {
        match self {
            JVal::Obj(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn num(&self) -> Option<f64> {
        match self {
            JVal::Num(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            JVal::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn arr(&self) -> Option<&[JVal]> {
        match self {
            JVal::Arr(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            JVal::Bool(b) => Some(*b),
            _ => None,
        }
    }
}

struct Parser<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(s: &'a str) -> Self {
        Parser {
            b: s.as_bytes(),
            pos: 0,
        }
    }

    fn err<T>(&self, msg: &str) -> Result<T, String> {
        Err(format!(
            "JSON error at byte {pos}: {msg}",
            pos = self.pos.min(self.b.len())
        ))
    }

    fn ws(&mut self) {
        while matches!(self.b.get(self.pos), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.pos).copied()
    }

    fn expect(&mut self, c: u8, what: &str) -> Result<(), String> {
        if self.peek() == Some(c) {
            self.pos += 1;
            Ok(())
        } else {
            self.err(what)
        }
    }

    fn value(&mut self, depth: usize) -> Result<JVal, String> {
        if depth > 64 {
            return self.err("nesting too deep");
        }
        self.ws();
        match self.peek() {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => Ok(JVal::Str(self.string()?)),
            Some(b't') => self.literal("true", JVal::Bool(true)),
            Some(b'f') => self.literal("false", JVal::Bool(false)),
            Some(b'n') => self.literal("null", JVal::Null),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            _ => self.err("expected value"),
        }
    }

    fn literal(&mut self, word: &str, val: JVal) -> Result<JVal, String> {
        if self.b.get(self.pos..self.pos + word.len()) == Some(word.as_bytes()) {
            self.pos += word.len();
            Ok(val)
        } else {
            self.err("bad literal")
        }
    }

    fn number(&mut self) -> Result<JVal, String> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        let token = std::str::from_utf8(&self.b[start..self.pos])
            .map_err(|_| "bad number encoding".to_string())?;
        token
            .parse::<f64>()
            .map(JVal::Num)
            .map_err(|_| format!("bad number: {token}"))
    }

    fn hex4(&mut self) -> Result<u32, String> {
        if self.pos + 4 > self.b.len() {
            return self.err("truncated \\u escape");
        }
        let s = std::str::from_utf8(&self.b[self.pos..self.pos + 4])
            .map_err(|_| "bad \\u escape".to_string())?;
        self.pos += 4;
        u32::from_str_radix(s, 16).map_err(|_| "bad \\u escape".to_string())
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"', "expected string")?;
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return self.err("unterminated string"),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    match self.peek() {
                        Some(b'"') => out.push('"'),
                        Some(b'\\') => out.push('\\'),
                        Some(b'/') => out.push('/'),
                        Some(b'b') => out.push('\u{08}'),
                        Some(b'f') => out.push('\u{0C}'),
                        Some(b'n') => out.push('\n'),
                        Some(b'r') => out.push('\r'),
                        Some(b't') => out.push('\t'),
                        Some(b'u') => {
                            self.pos += 1;
                            let hi = self.hex4()?;
                            // Surrogate pair for non-BMP characters.
                            if (0xD800..0xDC00).contains(&hi)
                                && self.b.get(self.pos..self.pos + 2) == Some(b"\\u".as_ref())
                            {
                                self.pos += 2;
                                let lo = self.hex4()?;
                                if (0xDC00..0xE000).contains(&lo) {
                                    let cp = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                                    out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                                    continue;
                                } else {
                                    out.push('\u{FFFD}');
                                    continue;
                                }
                            }
                            out.push(char::from_u32(hi).unwrap_or('\u{FFFD}'));
                            continue;
                        }
                        _ => return self.err("bad escape"),
                    }
                    self.pos += 1;
                }
                Some(_) => {
                    // Raw UTF-8: consume one char.
                    let s = std::str::from_utf8(&self.b[self.pos..])
                        .map_err(|_| "bad utf8".to_string())?;
                    let c = s.chars().next().ok_or("unterminated string".to_string())?;
                    // Raw control characters are invalid JSON.
                    if (c as u32) < 0x20 {
                        return self.err("unescaped control character");
                    }
                    out.push(c);
                    self.pos += c.len_utf8();
                }
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<JVal, String> {
        self.expect(b'[', "expected [")?;
        let mut items = Vec::new();
        self.ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(JVal::Arr(items));
        }
        loop {
            items.push(self.value(depth + 1)?);
            self.ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    return Ok(JVal::Arr(items));
                }
                _ => return self.err("expected , or ]"),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<JVal, String> {
        self.expect(b'{', "expected {")?;
        let mut pairs = Vec::new();
        self.ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(JVal::Obj(pairs));
        }
        loop {
            self.ws();
            if self.peek() != Some(b'"') {
                return self.err("expected string key");
            }
            let key = self.string()?;
            self.ws();
            self.expect(b':', "expected :")?;
            let val = self.value(depth + 1)?;
            pairs.push((key, val));
            self.ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(JVal::Obj(pairs));
                }
                _ => return self.err("expected , or }"),
            }
        }
    }
}

/// Parse one complete JSON value; trailing garbage is an error.
pub fn parse(text: &str) -> Result<JVal, String> {
    let mut p = Parser::new(text);
    let v = p.value(0)?;
    p.ws();
    if p.pos != p.b.len() {
        return p.err("trailing data");
    }
    Ok(v)
}

/// Compact serializer (roundtrips `parse`; used for archive manifests).
pub fn to_json(v: &JVal) -> String {
    match v {
        JVal::Null => "null".to_string(),
        JVal::Bool(true) => "true".to_string(),
        JVal::Bool(false) => "false".to_string(),
        JVal::Num(n) => {
            if n.is_finite() {
                format!("{n:?}")
            } else {
                "null".to_string()
            }
        }
        JVal::Str(s) => {
            let mut o = String::with_capacity(s.len() + 2);
            o.push('"');
            for c in s.chars() {
                match c {
                    '"' => o.push_str("\\\""),
                    '\\' => o.push_str("\\\\"),
                    '\n' => o.push_str("\\n"),
                    '\r' => o.push_str("\\r"),
                    '\t' => o.push_str("\\t"),
                    c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
                    c => o.push(c),
                }
            }
            o.push('"');
            o
        }
        JVal::Arr(items) => {
            let mut o = String::from("[");
            for (i, x) in items.iter().enumerate() {
                if i > 0 {
                    o.push(',');
                }
                o.push_str(&to_json(x));
            }
            o.push(']');
            o
        }
        JVal::Obj(pairs) => {
            let mut o = String::from("{");
            for (i, (k, x)) in pairs.iter().enumerate() {
                if i > 0 {
                    o.push(',');
                }
                o.push_str(&to_json(&JVal::Str(k.clone())));
                o.push(':');
                o.push_str(&to_json(x));
            }
            o.push('}');
            o
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitives() {
        assert_eq!(parse("null").unwrap(), JVal::Null);
        assert_eq!(parse("true").unwrap(), JVal::Bool(true));
        assert_eq!(parse("  -12.5e3  ").unwrap(), JVal::Num(-12500.0));
        assert_eq!(parse("\"hi\"").unwrap(), JVal::Str("hi".to_string()));
    }

    #[test]
    fn escapes_and_unicode() {
        assert_eq!(
            parse("\"a\\\"b\\\\c\\n\"").unwrap(),
            JVal::Str("a\"b\\c\n".to_string())
        );
        assert_eq!(parse("\"\\u00e9\"").unwrap(), JVal::Str("é".to_string()));
        assert_eq!(
            parse("\"\\uD83D\\uDE00\"").unwrap(),
            JVal::Str("😀".to_string())
        );
        assert_eq!(
            parse("\"Dub\\u0103\"").unwrap(),
            JVal::Str("Dubă".to_string())
        );
    }

    #[test]
    fn nested_session_like() {
        let v = parse("{\"a\":{\"v\":6.4,\"p\":\"measured\"},\"b\":[1,null,{\"c\":[]}]}").unwrap();
        assert_eq!(v.get("a").unwrap().get("v").unwrap().num(), Some(6.4));
        assert_eq!(v.get("b").unwrap().arr().unwrap().len(), 3);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("").is_err());
        assert!(parse("{").is_err());
        assert!(parse("{\"a\":}").is_err());
        assert!(parse("[1,]").is_err());
        assert!(parse("{\"a\":1} x").is_err());
        assert!(parse("\"\\x\"").is_err());
        assert!(parse("\"bad\n\"").is_err());
        // Deep nesting is capped, not a stack overflow.
        let deep = "[".repeat(100) + &"]".repeat(100);
        assert!(parse(&deep).is_err());
    }

    #[test]
    fn accessors() {
        let v = parse("{\"s\":\"x\",\"n\":2,\"b\":false,\"a\":[]}").unwrap();
        assert_eq!(v.get("s").unwrap().as_str(), Some("x"));
        assert_eq!(v.get("n").unwrap().num(), Some(2.0));
        assert_eq!(v.get("b").unwrap().as_bool(), Some(false));
        assert!(v.get("missing").is_none());
        assert!(v.get("s").unwrap().num().is_none());
    }

    #[test]
    fn serializer_roundtrip() {
        for text in [
            "{\"a\":1,\"b\":[1.5,null,true,\"x\\u00e9\"]}",
            "{\"nested\":{\"x\":[]},\"empty\":\"\"}",
        ] {
            let v = parse(text).unwrap();
            let back = parse(&to_json(&v)).unwrap();
            assert_eq!(v, back);
        }
        assert_eq!(to_json(&JVal::Num(f64::NAN)), "null");
    }
}
