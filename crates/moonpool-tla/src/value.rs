//! TLA+ values and how they are written as TLA+ text.

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// A value that TLC can read: what an event field or a trace definition holds.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Value {
    /// `TRUE` or `FALSE`.
    Bool(bool),
    /// An integer. TLC holds integers in 32 bits, so a value outside
    /// `i32::MIN + 1 ..= i32::MAX` is refused when the trace is written.
    Int(i64),
    /// A string, written between double quotes.
    Str(String),
    /// A sequence (a tuple), written `<<a, b>>`.
    Seq(Vec<Value>),
    /// A set, written `{a, b}`. The elements are written sorted and once.
    Set(Vec<Value>),
    /// A record, written `[k1 |-> a, k2 |-> b]`. A record with no field
    /// is written `<<>>`: TLA+ has no empty record.
    Record(BTreeMap<String, Value>),
}

/// Why a value cannot be written as TLA+ text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueError {
    /// TLC holds integers in 32 bits.
    IntOutOfRange(i64),
    /// A record field or a definition is not a TLA+ identifier.
    BadIdentifier(String),
}

impl std::fmt::Display for ValueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IntOutOfRange(v) => write!(
                f,
                "integer {v} does not fit the 32 bits of a TLC integer; record it as a string"
            ),
            Self::BadIdentifier(name) => write!(f, "`{name}` is not a TLA+ identifier"),
        }
    }
}

impl std::error::Error for ValueError {}

impl Value {
    /// A set of strings.
    pub fn str_set<S: Into<String>>(items: impl IntoIterator<Item = S>) -> Self {
        Self::Set(items.into_iter().map(|s| Self::Str(s.into())).collect())
    }

    /// A record from `(field, value)` pairs.
    pub fn record<K: Into<String>>(fields: impl IntoIterator<Item = (K, Value)>) -> Self {
        Self::Record(fields.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }

    /// Append this value to `out` as TLA+ text.
    ///
    /// # Errors
    ///
    /// Returns the first integer that does not fit TLC's 32 bits, or the
    /// first record field that is not a TLA+ identifier.
    pub fn write_tla(&self, out: &mut String) -> Result<(), ValueError> {
        match self {
            Self::Bool(true) => out.push_str("TRUE"),
            Self::Bool(false) => out.push_str("FALSE"),
            Self::Int(v) => {
                // `i32::MIN` is refused too: TLC reads `-2147483648` as the
                // minus of a literal that does not fit 32 bits.
                if i32::try_from(*v).is_err() || *v == i64::from(i32::MIN) {
                    return Err(ValueError::IntOutOfRange(*v));
                }
                // `write!` to a String cannot fail.
                let _ = write!(out, "{v}");
            }
            Self::Str(s) => write_string(s, out),
            Self::Seq(items) => write_list(items.iter(), "<<", ">>", out)?,
            Self::Set(items) => {
                let mut sorted: Vec<&Value> = items.iter().collect();
                sorted.sort();
                sorted.dedup();
                write_list(sorted.into_iter(), "{", "}", out)?;
            }
            Self::Record(fields) if fields.is_empty() => out.push_str("<<>>"),
            Self::Record(fields) => {
                out.push('[');
                for (i, (name, value)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    check_identifier(name)?;
                    out.push_str(name);
                    out.push_str(" |-> ");
                    value.write_tla(out)?;
                }
                out.push(']');
            }
        }
        Ok(())
    }
}

fn write_list<'a>(
    items: impl Iterator<Item = &'a Value>,
    open: &str,
    close: &str,
    out: &mut String,
) -> Result<(), ValueError> {
    out.push_str(open);
    for (i, item) in items.enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        item.write_tla(out)?;
    }
    out.push_str(close);
    Ok(())
}

/// A TLA+ string: `\` and `"` are escaped, a control character becomes a
/// space (TLA+ strings have no escape for it).
fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A TLA+ identifier: ASCII letters, digits and `_`, with at least one letter.
pub(crate) fn check_identifier(name: &str) -> Result<(), ValueError> {
    let well_formed = name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && name.chars().any(|c| c.is_ascii_alphabetic());
    if well_formed {
        Ok(())
    } else {
        Err(ValueError::BadIdentifier(name.to_owned()))
    }
}

impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Self::Int(v)
    }
}

impl From<i32> for Value {
    fn from(v: i32) -> Self {
        Self::Int(i64::from(v))
    }
}

impl From<u32> for Value {
    fn from(v: u32) -> Self {
        Self::Int(i64::from(v))
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Self::Str(v.to_owned())
    }
}

impl From<String> for Value {
    fn from(v: String) -> Self {
        Self::Str(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tla(v: &Value) -> String {
        let mut out = String::new();
        v.write_tla(&mut out).expect("the value is writable");
        out
    }

    #[test]
    fn scalars_are_written_as_tla() {
        assert_eq!(tla(&Value::Bool(true)), "TRUE");
        assert_eq!(tla(&Value::Bool(false)), "FALSE");
        assert_eq!(tla(&Value::Int(-7)), "-7");
        assert_eq!(tla(&Value::from("a1")), "\"a1\"");
    }

    #[test]
    fn a_string_escapes_quotes_and_backslashes() {
        assert_eq!(tla(&Value::from("a\"b\\c\nd")), "\"a\\\"b\\\\c d\"");
    }

    #[test]
    fn a_set_is_sorted_and_has_no_duplicate() {
        assert_eq!(tla(&Value::str_set(["b", "a", "b"])), "{\"a\", \"b\"}");
    }

    #[test]
    fn a_record_nests_sequences() {
        let v = Value::record([
            ("slot", Value::Int(3)),
            ("votes", Value::Seq(vec![Value::Int(1), Value::Int(2)])),
        ]);
        assert_eq!(tla(&v), "[slot |-> 3, votes |-> <<1, 2>>]");
        assert_eq!(tla(&Value::Record(BTreeMap::new())), "<<>>");
    }

    #[test]
    fn an_integer_beyond_32_bits_is_refused() {
        let mut out = String::new();
        let big = i64::from(i32::MAX) + 1;
        assert_eq!(
            Value::Int(big).write_tla(&mut out),
            Err(ValueError::IntOutOfRange(big))
        );
        let min = i64::from(i32::MIN);
        assert_eq!(
            Value::Int(min).write_tla(&mut out),
            Err(ValueError::IntOutOfRange(min))
        );
        assert_eq!(Value::Int(min + 1).write_tla(&mut out), Ok(()));
    }

    #[test]
    fn a_field_that_is_not_an_identifier_is_refused() {
        let mut out = String::new();
        let v = Value::record([("http.status", Value::Int(200))]);
        assert_eq!(
            v.write_tla(&mut out),
            Err(ValueError::BadIdentifier("http.status".to_owned()))
        );
        assert!(check_identifier("123").is_err());
        assert!(check_identifier("a_1").is_ok());
    }
}
