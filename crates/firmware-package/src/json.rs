//! The format's JSON rules, checked strictly: UTF-8 with no byte-order mark, one object, no member
//! name twice in any object (unknown members included), whole numbers only (no sign, fraction or
//! exponent) and bounded nesting.
//!
//! serde's derive neither notices a repeated unknown member nor tells `1` from `1.0` in every case,
//! so this reads the text into a [`Value`] first, checking every rule on the way, and only then hands
//! it to the typed reader.

use std::cell::Cell;
use std::collections::BTreeSet;
use std::fmt;

use serde::de::{DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};

/// The UTF-8 byte-order mark.
pub const BOM: &[u8; 3] = b"\xef\xbb\xbf";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JsonProblem {
    ByteOrderMark,
    NotUtf8,
    /// Not JSON at all, or something after the value. The text is serde_json's description.
    Syntax(String),
    NotAnObject,
    /// The repeated member name.
    RepeatedMember(String),
    /// A number with a sign, a fraction or an exponent, or too large for 64 bits.
    NotAWholeNumber,
    TooDeep,
}

impl fmt::Display for JsonProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ByteOrderMark => f.write_str("it starts with a byte-order mark"),
            Self::NotUtf8 => f.write_str("it isn't UTF-8"),
            Self::Syntax(e) => write!(f, "it isn't valid JSON ({e})"),
            Self::NotAnObject => f.write_str("it isn't a JSON object"),
            Self::RepeatedMember(name) => write!(f, "the member name {name:?} appears twice in one object"),
            Self::NotAWholeNumber => f.write_str(
                "a number has a sign, fraction or exponent, or is too large; numbers must be whole",
            ),
            Self::TooDeep => f.write_str("it nests too deeply"),
        }
    }
}

/// Reads `bytes` as one JSON object under the rules above, nesting at most `max_depth` levels (the
/// object itself is level one; objects and arrays count, other values don't).
pub fn parse_object(bytes: &[u8], max_depth: usize) -> Result<Map<String, Value>, JsonProblem> {
    if bytes.starts_with(BOM) {
        return Err(JsonProblem::ByteOrderMark);
    }
    std::str::from_utf8(bytes).map_err(|_| JsonProblem::NotUtf8)?;
    let problem = Cell::new(None);
    let mut de = serde_json::Deserializer::from_slice(bytes);
    let parsed = Strict {
        depth: 0,
        max_depth,
        problem: &problem,
    }
    .deserialize(&mut de)
    .and_then(|v| de.end().map(|()| v));
    match parsed {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(JsonProblem::NotAnObject),
        Err(e) => Err(problem
            .take()
            .unwrap_or_else(|| JsonProblem::Syntax(e.to_string()))),
    }
}

/// Builds a [`Value`], refusing anything the rules forbid. The first problem found is kept in
/// `problem`, so the caller can report it exactly rather than as serde_json's text.
#[derive(Clone, Copy)]
struct Strict<'a> {
    depth: usize,
    max_depth: usize,
    problem: &'a Cell<Option<JsonProblem>>,
}

impl Strict<'_> {
    fn fail<E: serde::de::Error>(&self, p: JsonProblem) -> E {
        let e = E::custom(&p);
        self.problem.set(Some(p));
        e
    }

    fn deeper<E: serde::de::Error>(&self) -> Result<Self, E> {
        if self.depth + 1 > self.max_depth {
            return Err(self.fail(JsonProblem::TooDeep));
        }
        Ok(Self {
            depth: self.depth + 1,
            ..*self
        })
    }
}

impl<'de> DeserializeSeed<'de> for Strict<'_> {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Strict<'_> {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }

    fn visit_unit<E: serde::de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Value, E> {
        Ok(Value::String(v.to_owned()))
    }

    fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Value, E> {
        Ok(Value::String(v))
    }

    // serde_json calls this only for a non-negative integer written without fraction or exponent
    // that fits in 64 bits. `-0`, `1.0`, `1e2` and larger integers all arrive as `f64`, and negative
    // integers as `i64`.
    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Value, E> {
        Ok(Value::Number(Number::from(v)))
    }

    fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Value, E> {
        Err(self.fail(JsonProblem::NotAWholeNumber))
    }

    fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Value, E> {
        Err(self.fail(JsonProblem::NotAWholeNumber))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let inner = self.deeper()?;
        let mut out = Vec::new();
        while let Some(v) = seq.next_element_seed(inner)? {
            out.push(v);
        }
        Ok(Value::Array(out))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let inner = self.deeper()?;
        let mut seen = BTreeSet::new();
        let mut out = Map::new();
        // Keys are compared after unescaping, so `"a"` and `"a"` are the same name.
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key.clone()) {
                return Err(self.fail(JsonProblem::RepeatedMember(key)));
            }
            let v = map.next_value_seed(inner)?;
            out.insert(key, v);
        }
        Ok(Value::Object(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Map<String, Value>, JsonProblem> {
        parse_object(s.as_bytes(), 4)
    }

    #[test]
    fn accepts_plain_objects() {
        let m = parse(r#" {"a": 1, "b": [true, null, "x"], "c": {"d": 0}} "#).unwrap();
        assert_eq!(m["a"], 1);
        assert_eq!(m["c"]["d"], 0);
        assert!(parse("{}").is_ok());
        assert_eq!(parse(r#"{"n": 18446744073709551615}"#).unwrap()["n"], u64::MAX);
    }

    #[test]
    fn refuses_repeated_members_at_every_level() {
        assert_eq!(
            parse(r#"{"a":1,"a":1}"#),
            Err(JsonProblem::RepeatedMember("a".into()))
        );
        assert_eq!(
            parse(r#"{"a":{"x":1,"x":2}}"#),
            Err(JsonProblem::RepeatedMember("x".into()))
        );
        assert_eq!(
            parse(r#"{"a":[{"x":1,"y":2,"x":2}]}"#),
            Err(JsonProblem::RepeatedMember("x".into()))
        );
        // The same name written two ways.
        assert_eq!(
            parse(r#"{"a":1,"a":2}"#),
            Err(JsonProblem::RepeatedMember("a".into()))
        );
    }

    #[test]
    fn refuses_numbers_that_are_not_plain_whole_numbers() {
        for n in [
            "-1",
            "-0",
            "1.0",
            "1.5",
            "1e2",
            "1E2",
            "0.0",
            "18446744073709551616",
            "-9223372036854775809",
        ] {
            assert_eq!(
                parse(&format!(r#"{{"n": {n}}}"#)),
                Err(JsonProblem::NotAWholeNumber),
                "{n}"
            );
        }
        // A plus sign and leading zeros aren't JSON at all.
        assert!(matches!(parse(r#"{"n": +1}"#), Err(JsonProblem::Syntax(_))));
        assert!(matches!(parse(r#"{"n": 01}"#), Err(JsonProblem::Syntax(_))));
        // Inside an unknown nested member too.
        assert_eq!(parse(r#"{"x": [{"n": 2.5}]}"#), Err(JsonProblem::NotAWholeNumber));
    }

    #[test]
    fn depth_counts_the_object_and_every_container() {
        assert!(parse(r#"{"a":{"b":[{"c":1}]}}"#).is_ok());
        assert_eq!(parse(r#"{"a":{"b":[{"c":[]}]}}"#), Err(JsonProblem::TooDeep));
        assert_eq!(parse(r#"{"a":{"b":[[{}]]}}"#), Err(JsonProblem::TooDeep));
        assert_eq!(parse_object(br#"{"a":{}}"#, 1), Err(JsonProblem::TooDeep));
        // Very deep input stops at the limit, long before serde_json's own recursion limit.
        let deep = format!("{{\"a\":{}1{}}}", "[".repeat(10_000), "]".repeat(10_000));
        assert_eq!(parse(&deep), Err(JsonProblem::TooDeep));
    }

    #[test]
    fn refuses_a_bom_bad_utf8_and_non_objects() {
        assert_eq!(parse("\u{feff}{}"), Err(JsonProblem::ByteOrderMark));
        assert_eq!(parse_object(b"{\"a\":\"\xff\"}", 4), Err(JsonProblem::NotUtf8));
        assert_eq!(parse("[]"), Err(JsonProblem::NotAnObject));
        assert_eq!(parse("1"), Err(JsonProblem::NotAnObject));
        assert!(matches!(parse("{} {}"), Err(JsonProblem::Syntax(_))));
        assert!(matches!(parse("{"), Err(JsonProblem::Syntax(_))));
        assert!(matches!(parse(""), Err(JsonProblem::Syntax(_))));
    }
}
