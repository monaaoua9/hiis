//! Just enough JSON for the HTTP API: parse a request body, print a response.
//! Numbers are f64 (ids stay exact up to 2^53, which is plenty for a doc counter).

use std::fmt::{self, Write};

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    // insertion order is kept, duplicate keys resolve to the first on lookup
    Obj(Vec<(String, Json)>),
}

const MAX_DEPTH: usize = 32;

impl Json {
    pub fn parse(src: &str) -> Result<Json, String> {
        let mut p = Parser { b: src.as_bytes(), i: 0 };
        let v = p.value(0)?;
        p.ws();
        if p.i != p.b.len() {
            return Err(p.err("trailing characters"));
        }
        Ok(v)
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(kv) => kv.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(v) => Some(v),
            _ => None,
        }
    }

    pub fn obj<const N: usize>(fields: [(&str, Json); N]) -> Json {
        Json::Obj(fields.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }
}

impl From<&str> for Json {
    fn from(s: &str) -> Json {
        Json::Str(s.to_string())
    }
}

impl From<String> for Json {
    fn from(s: String) -> Json {
        Json::Str(s)
    }
}

impl From<bool> for Json {
    fn from(b: bool) -> Json {
        Json::Bool(b)
    }
}

impl From<u64> for Json {
    fn from(n: u64) -> Json {
        Json::Num(n as f64)
    }
}

impl From<usize> for Json {
    fn from(n: usize) -> Json {
        Json::Num(n as f64)
    }
}

impl From<f32> for Json {
    fn from(n: f32) -> Json {
        Json::Num(n as f64)
    }
}

impl<T: Into<Json>> From<Option<T>> for Json {
    fn from(o: Option<T>) -> Json {
        o.map_or(Json::Null, Into::into)
    }
}

impl<T: Into<Json>> From<Vec<T>> for Json {
    fn from(v: Vec<T>) -> Json {
        Json::Arr(v.into_iter().map(Into::into).collect())
    }
}

impl fmt::Display for Json {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Json::Null => f.write_str("null"),
            Json::Bool(b) => write!(f, "{b}"),
            // JSON has no NaN / infinity
            Json::Num(n) if !n.is_finite() => f.write_str("null"),
            Json::Num(n) if n.fract() == 0.0 && n.abs() < 9.0e15 => write!(f, "{}", *n as i64),
            Json::Num(n) => write!(f, "{n}"),
            Json::Str(s) => write_str(f, s),
            Json::Arr(v) => {
                f.write_char('[')?;
                for (i, x) in v.iter().enumerate() {
                    if i > 0 {
                        f.write_char(',')?;
                    }
                    write!(f, "{x}")?;
                }
                f.write_char(']')
            }
            Json::Obj(kv) => {
                f.write_char('{')?;
                for (i, (k, v)) in kv.iter().enumerate() {
                    if i > 0 {
                        f.write_char(',')?;
                    }
                    write_str(f, k)?;
                    write!(f, ":{v}")?;
                }
                f.write_char('}')
            }
        }
    }
}

fn write_str(f: &mut fmt::Formatter<'_>, s: &str) -> fmt::Result {
    f.write_char('"')?;
    for c in s.chars() {
        match c {
            '"' => f.write_str("\\\"")?,
            '\\' => f.write_str("\\\\")?,
            '\n' => f.write_str("\\n")?,
            '\r' => f.write_str("\\r")?,
            '\t' => f.write_str("\\t")?,
            c if (c as u32) < 0x20 => write!(f, "\\u{:04x}", c as u32)?,
            c => f.write_char(c)?,
        }
    }
    f.write_char('"')
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn err(&self, what: &str) -> String {
        format!("{what} at byte {}", self.i)
    }

    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn eat(&mut self, lit: &str) -> bool {
        if self.b[self.i..].starts_with(lit.as_bytes()) {
            self.i += lit.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, String> {
        if depth > MAX_DEPTH {
            return Err(self.err("nested too deeply"));
        }
        self.ws();
        let Some(&c) = self.b.get(self.i) else { return Err(self.err("unexpected end")) };
        match c {
            b'{' => self.object(depth),
            b'[' => self.array(depth),
            b'"' => self.string().map(Json::Str),
            b't' if self.eat("true") => Ok(Json::Bool(true)),
            b'f' if self.eat("false") => Ok(Json::Bool(false)),
            b'n' if self.eat("null") => Ok(Json::Null),
            b'-' | b'0'..=b'9' => self.number(),
            _ => Err(self.err("unexpected character")),
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, String> {
        self.i += 1;
        let mut kv = Vec::new();
        self.ws();
        if self.b.get(self.i) == Some(&b'}') {
            self.i += 1;
            return Ok(Json::Obj(kv));
        }
        loop {
            self.ws();
            if self.b.get(self.i) != Some(&b'"') {
                return Err(self.err("expected a string key"));
            }
            let k = self.string()?;
            self.ws();
            if self.b.get(self.i) != Some(&b':') {
                return Err(self.err("expected ':'"));
            }
            self.i += 1;
            kv.push((k, self.value(depth + 1)?));
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Json::Obj(kv));
                }
                _ => return Err(self.err("expected ',' or '}'")),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Json, String> {
        self.i += 1;
        let mut v = Vec::new();
        self.ws();
        if self.b.get(self.i) == Some(&b']') {
            self.i += 1;
            return Ok(Json::Arr(v));
        }
        loop {
            v.push(self.value(depth + 1)?);
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Json::Arr(v));
                }
                _ => return Err(self.err("expected ',' or ']'")),
            }
        }
    }

    fn number(&mut self) -> Result<Json, String> {
        let s = self.i;
        while self.i < self.b.len()
            && matches!(self.b[self.i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
        {
            self.i += 1;
        }
        let text = std::str::from_utf8(&self.b[s..self.i]).unwrap();
        // rust accepts "+1" and "1." which JSON doesn't; a leading zero before a digit is out too
        let digits = text.strip_prefix('-').unwrap_or(text);
        let bad = text.starts_with('+')
            || text.ends_with('.')
            || digits.starts_with('.')
            || (digits.len() > 1 && digits.starts_with('0') && digits.as_bytes()[1].is_ascii_digit());
        match text.parse::<f64>() {
            Ok(n) if !bad => Ok(Json::Num(n)),
            _ => Err(self.err("bad number")),
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let h = self.b.get(self.i..self.i + 4).and_then(|h| std::str::from_utf8(h).ok());
        let n = h.and_then(|h| u32::from_str_radix(h, 16).ok()).ok_or_else(|| self.err("bad \\u escape"))?;
        self.i += 4;
        Ok(n)
    }

    fn string(&mut self) -> Result<String, String> {
        self.i += 1;
        let mut out = String::new();
        loop {
            // copy the plain run in one go; the source is a &str so it is valid utf-8
            let run = self.b[self.i..].iter().position(|&c| c == b'"' || c == b'\\' || c < 0x20);
            let Some(run) = run else { return Err(self.err("unterminated string")) };
            out.push_str(std::str::from_utf8(&self.b[self.i..self.i + run]).unwrap());
            self.i += run;

            match self.b[self.i] {
                b'"' => {
                    self.i += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.i += 1;
                    let Some(&e) = self.b.get(self.i) else { return Err(self.err("unterminated string")) };
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let cp = if (0xD800..0xDC00).contains(&hi) {
                                if !self.eat("\\u") {
                                    return Err(self.err("lone surrogate"));
                                }
                                let lo = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&lo) {
                                    return Err(self.err("bad surrogate pair"));
                                }
                                0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                            } else {
                                hi
                            };
                            out.push(char::from_u32(cp).ok_or_else(|| self.err("bad code point"))?);
                        }
                        _ => return Err(self.err("bad escape")),
                    }
                }
                _ => return Err(self.err("control character in string")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Json {
        Json::parse(s).unwrap()
    }

    #[test]
    fn scalars_and_containers() {
        assert_eq!(p(" null "), Json::Null);
        assert_eq!(p("true"), Json::Bool(true));
        assert_eq!(p("-12.5e1"), Json::Num(-125.0));
        assert_eq!(p("[1, [2], {}]").as_array().unwrap().len(), 3);
        let o = p(r#"{"a": 1, "b": {"c": "d"}, "a": 2}"#);
        assert_eq!(o.get("a"), Some(&Json::Num(1.0)));
        assert_eq!(o.get("b").unwrap().get("c").unwrap().as_str(), Some("d"));
        assert!(o.get("zz").is_none());
    }

    #[test]
    fn string_escapes() {
        assert_eq!(p(r#""a\nb\t\"q\" \\ \/""#), Json::Str("a\nb\t\"q\" \\ /".into()));
        assert_eq!(p(r#""\u0041\u00e9""#), Json::Str("Aé".into()));
        // surrogate pair -> one char
        assert_eq!(p(r#""\ud83d\ude00""#), Json::Str("😀".into()));
        assert_eq!(p("\"مرحبا\""), Json::Str("مرحبا".into()));
    }

    #[test]
    fn rejects_malformed_input() {
        for bad in [
            "",
            "{",
            "[1,",
            "[1 2]",
            "{\"a\"}",
            "{a:1}",
            "tru",
            "01",
            "1.",
            "+1",
            ".5",
            "\"abc",
            "\"\\x\"",
            "\"\\ud800\"",
            "\"\\ud800\\u0041\"",
            "\"\\udc00\"",
            "[1] x",
            "\"tab\there\"",
            "--1",
            "1e",
        ] {
            assert!(Json::parse(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn depth_is_capped() {
        let ok = format!("{}1{}", "[".repeat(30), "]".repeat(30));
        assert!(Json::parse(&ok).is_ok());
        let deep = format!("{}1{}", "[".repeat(5000), "]".repeat(5000));
        assert!(Json::parse(&deep).is_err());
    }

    #[test]
    fn printing_roundtrips() {
        let v = Json::obj([
            ("text", "line1\nline2 \"q\" \u{1} é".into()),
            ("n", 3u64.into()),
            ("f", 0.5f32.into()),
            ("none", Option::<u64>::None.into()),
            ("list", vec![1u64, 2, 3].into()),
            ("bad", Json::Num(f64::NAN)),
        ]);
        let s = v.to_string();
        assert_eq!(
            s,
            r#"{"text":"line1\nline2 \"q\" \u0001 é","n":3,"f":0.5,"none":null,"list":[1,2,3],"bad":null}"#
        );
        let back = Json::parse(&s).unwrap();
        assert_eq!(back.get("text").unwrap().as_str(), v.get("text").unwrap().as_str());
    }

    #[test]
    fn big_integers_print_without_exponent() {
        assert_eq!(Json::Num(1_234_567_890_123.0).to_string(), "1234567890123");
        assert_eq!(Json::Num(-7.0).to_string(), "-7");
    }
}
