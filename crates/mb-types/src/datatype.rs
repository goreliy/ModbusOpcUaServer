//! Raw wire data types plus the word/byte ordering pair that makes multi-word
//! decode unambiguous.

use serde::{Deserialize, Serialize};

/// NOTE: NO `#[serde(other)]` catch-all. An unknown data type is a hard load-time
/// error, not a silently-unpollable tag.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DataType {
    Bit,
    U16,
    I16,
    U32,
    I32,
    U64,
    I64,
    F32,
    F64,
    Bcd,
    Ascii,
    Bitfield,
}

impl DataType {
    /// Register span for fixed-width numeric types; `None` = caller must supply `length`.
    pub const fn register_count(self) -> Option<u16> {
        match self {
            DataType::Bit | DataType::U16 | DataType::I16 | DataType::Bitfield => Some(1),
            DataType::U32 | DataType::I32 | DataType::F32 => Some(2),
            DataType::U64 | DataType::I64 | DataType::F64 => Some(4),
            DataType::Bcd | DataType::Ascii => None, // needs length
        }
    }
}

/// Four canonical byte/word orderings, unambiguous for f32/u32 decode.
/// ("big/little endian" alone is under-specified.)
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WordOrder {
    #[default]
    BigEndian, // high word first
    LittleEndian, // low word first
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ByteOrder {
    #[default]
    BigEndian, // within each 16-bit word
    LittleEndian,
}
