//! Named fields (title, body, ...) on top of the plain-text engine.
//!
//! A fielded document is stored as `\x1e name \x1f value` records. Indexing adds, next
//! to the ordinary terms of the whole text, one extra posting per token under `field:term`.
//! So an unfielded query behaves exactly as before (it sees all the text), and `title:fox`
//! only looks at the title terms. The document length that BM25 normalises by is the
//! whole-document one, not a per-field length; that is the price of not touching the segment
//! format. A colon can't occur inside an analysed term, which keeps the two kinds apart.

use crate::analysis::{Analyzer, Token};
use crate::error::{Error, Result};
use crate::index::builder::{Grouped, SegmentBuilder};
use crate::json::Json;

pub const REC: char = '\u{1e}';
pub const SEP: char = '\u{1f}';

// phrases and proximity must not stretch from one field into the next
const FIELD_GAP: u32 = 50;

pub fn valid_name(name: &str) -> bool {
    let mut it = name.chars();
    it.next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() <= 32
        && it.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

pub fn is_fielded(raw: &str) -> bool {
    raw.starts_with(REC)
}

/// True for the dictionary entries that belong to a field.
pub fn is_field_term(term: &str) -> bool {
    term.contains(':')
}

pub fn encode(fields: &[(&str, &str)]) -> Result<String> {
    if fields.is_empty() {
        return Err(Error::Invalid("a document needs at least one field".into()));
    }
    let mut out = String::new();
    for (name, value) in fields {
        if !valid_name(name) {
            return Err(Error::Invalid(format!(
                "bad field name {name:?} (lowercase letters, digits, _; max 32)"
            )));
        }
        if value.contains([REC, SEP]) {
            return Err(Error::Invalid("field values can't contain control characters 0x1e / 0x1f".into()));
        }
        out.push(REC);
        out.push_str(name);
        out.push(SEP);
        out.push_str(value);
    }
    Ok(out)
}

pub fn decode(raw: &str) -> Option<Vec<(&str, &str)>> {
    let body = raw.strip_prefix(REC)?;
    Some(body.split(REC).filter_map(|rec| rec.split_once(SEP)).collect())
}

pub fn to_json(fields: &[(String, String)]) -> Json {
    Json::Obj(fields.iter().map(|(k, v)| (k.clone(), Json::Str(v.clone()))).collect())
}

/// `{"title": "...", "body": "..."}` -> pairs, in the order given.
pub fn from_json(j: &Json) -> Result<Vec<(String, String)>> {
    let Json::Obj(kv) = j else { return Err(Error::Invalid("\"fields\" must be an object".into())) };
    kv.iter()
        .map(|(k, v)| match v {
            Json::Str(s) => Ok((k.clone(), s.clone())),
            _ => Err(Error::Invalid(format!("field {k:?} must be a string"))),
        })
        .collect()
}

/// What a reader sees: plain text as it is, a fielded document as its values one per line.
pub fn plain(raw: &str) -> String {
    match decode(raw) {
        None => raw.to_string(),
        Some(f) => f.iter().map(|(_, v)| *v).collect::<Vec<_>>().join("\n"),
    }
}

pub fn index(an: &Analyzer, raw: &str) -> Grouped {
    let Some(fields) = decode(raw) else {
        return SegmentBuilder::group(an.analyze(raw));
    };

    let mut tokens: Vec<Token> = Vec::new();
    let mut extra: Vec<Token> = Vec::new();
    let mut base = 0;
    for (name, value) in fields {
        let part = an.analyze(value);
        let next = part.last().map_or(base, |t| base + t.position + 1 + FIELD_GAP);
        for t in part {
            let position = base + t.position;
            extra.push(Token { term: format!("{name}:{}", t.term), position, start: t.start, end: t.end });
            tokens.push(Token { position, ..t });
        }
        base = next;
    }
    let n = tokens.len() as u32;
    tokens.extend(extra);
    SegmentBuilder::group_counted(tokens, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(valid_name("title"));
        assert!(valid_name("a_1"));
        for bad in ["", "Title", "1a", "a-b", "a:b", "é", &"x".repeat(33)] {
            assert!(!valid_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn roundtrip() {
        let raw =
            encode(&[("title", "Hello, world"), ("body", "line one\nline two"), ("title", "again")]).unwrap();
        assert!(is_fielded(&raw));
        let f = decode(&raw).unwrap();
        assert_eq!(f, [("title", "Hello, world"), ("body", "line one\nline two"), ("title", "again")]);
        assert_eq!(plain(&raw), "Hello, world\nline one\nline two\nagain");
        assert_eq!(plain("just text"), "just text");
        assert!(decode("just text").is_none());
    }

    #[test]
    fn rejects_bad_input() {
        assert!(encode(&[]).is_err());
        assert!(encode(&[("Bad", "x")]).is_err());
        assert!(encode(&[("ok", "a\u{1f}b")]).is_err());
        assert!(encode(&[("ok", "a\u{1e}b")]).is_err());
    }

    #[test]
    fn empty_values_survive() {
        let raw = encode(&[("a", ""), ("b", "x")]).unwrap();
        assert_eq!(decode(&raw).unwrap(), [("a", ""), ("b", "x")]);
    }
}
