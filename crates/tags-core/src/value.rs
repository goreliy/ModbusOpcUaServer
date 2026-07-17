//! Typed engineering values produced by the decode → formula pipeline.

use std::fmt;

/// A decoded (and possibly formula-transformed) tag value. Maps 1:1 onto the
/// OPC UA variant types in phase 3 (Bytes -> ByteString, Text -> String).
#[derive(Clone, Debug, PartialEq)]
pub enum TypedValue {
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    Text(String),
    /// Custom/vendor raw payload passed through undecoded.
    Bytes(Vec<u8>),
    /// Never-yet-computed (mirrors `RawValue::Absent`).
    Absent,
}

impl TypedValue {
    /// Numeric view for deadband math and formula input. `Bool` counts as
    /// 0/1; `Text`/`Bytes`/`Absent` are non-numeric.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            TypedValue::Bool(b) => Some(f64::from(u8::from(*b))),
            TypedValue::Int(v) => Some(*v as f64),
            TypedValue::UInt(v) => Some(*v as f64),
            TypedValue::Float(v) => Some(*v),
            TypedValue::Text(_) | TypedValue::Bytes(_) | TypedValue::Absent => None,
        }
    }
}

impl fmt::Display for TypedValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypedValue::Bool(b) => write!(f, "{b}"),
            TypedValue::Int(v) => write!(f, "{v}"),
            TypedValue::UInt(v) => write!(f, "{v}"),
            TypedValue::Float(v) => write!(f, "{v}"),
            TypedValue::Text(s) => write!(f, "{s}"),
            TypedValue::Bytes(b) => write!(f, "{} bytes", b.len()),
            TypedValue::Absent => write!(f, "<absent>"),
        }
    }
}
