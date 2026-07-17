//! Raw Modbus payload -> [`TypedValue`] decode (and the inverse encode for
//! the phase-4 write path).
//!
//! Conventions (design §4 / phase-2 progress doc):
//! - `RawValue::Registers` holds the register VALUES as tokio-modbus decoded
//!   them (one `u16` per register, already host-order). `ByteOrder::LittleEndian`
//!   swaps the two bytes WITHIN each 16-bit word first (devices that store
//!   low-byte-first inside a register).
//! - `WordOrder::BigEndian` (default) = most-significant word FIRST for
//!   multi-register types; `LittleEndian` = least-significant word first
//!   ("swapped words", typical for many power meters and VFDs).
//! - BCD: each nibble is a decimal digit, words combined per `WordOrder`.
//! - ASCII: 2 bytes per register, high byte first within a word (after the
//!   `ByteOrder` swap); trailing NUL/space trimmed.
//! - `Bit` on a register read selects `bit` (0..16) within the first word;
//!   `Bit` on a coil read takes the first bit. `Bitfield` without a `bit`
//!   index exposes the whole register as `UInt` (mask decoding in formulas);
//!   with `bit` it behaves like `Bit`.

use mb_types::{ByteOrder, DataType, WordOrder};
use mb_poller::RawValue;

use crate::value::TypedValue;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum DecodeError {
    #[error("payload has {got} registers, type needs {need}")]
    TooShort { got: usize, need: usize },
    #[error("empty bit payload")]
    NoBits,
    #[error("bcd digit out of range in word {word:#06x}")]
    BadBcd { word: u16 },
    #[error("bit index {bit} outside 0..16")]
    BadBitIndex { bit: u8 },
    #[error("value not yet read")]
    Absent,
    #[error("cannot encode {what} into {dt:?}")]
    Encode { what: String, dt: DataType },
}

/// Per-tag decode metadata (a slice of `ResolvedRegister`).
#[derive(Debug, Clone, Copy)]
pub struct DecodeMeta {
    pub data_type: DataType,
    pub word_order: WordOrder,
    pub byte_order: ByteOrder,
    pub bit: Option<u8>,
}

/// Apply the per-word byte swap, then arrange words most-significant-first.
fn ordered_words(regs: &[u16], need: usize, meta: DecodeMeta) -> Result<Vec<u16>, DecodeError> {
    if regs.len() < need {
        return Err(DecodeError::TooShort { got: regs.len(), need });
    }
    let mut words: Vec<u16> = regs[..need]
        .iter()
        .map(|w| match meta.byte_order {
            ByteOrder::BigEndian => *w,
            ByteOrder::LittleEndian => w.swap_bytes(),
        })
        .collect();
    if meta.word_order == WordOrder::LittleEndian {
        words.reverse();
    }
    Ok(words)
}

fn combine_u32(w: &[u16]) -> u32 {
    (u32::from(w[0]) << 16) | u32::from(w[1])
}

fn combine_u64(w: &[u16]) -> u64 {
    (u64::from(w[0]) << 48) | (u64::from(w[1]) << 32) | (u64::from(w[2]) << 16) | u64::from(w[3])
}

/// Decode one tag's raw payload into its typed value.
pub fn decode(raw: &RawValue, meta: DecodeMeta) -> Result<TypedValue, DecodeError> {
    match raw {
        RawValue::Absent => Err(DecodeError::Absent),
        RawValue::Bits(bits) => {
            let b = *bits.first().ok_or(DecodeError::NoBits)?;
            Ok(TypedValue::Bool(b))
        }
        RawValue::Raw(bytes) => Ok(TypedValue::Bytes(bytes.to_vec())),
        RawValue::Registers(regs) => decode_registers(regs, meta),
    }
}

fn decode_registers(regs: &[u16], meta: DecodeMeta) -> Result<TypedValue, DecodeError> {
    use DataType as DT;
    match meta.data_type {
        DT::Bit => {
            let bit = meta.bit.ok_or(DecodeError::BadBitIndex { bit: 255 })?;
            if bit >= 16 {
                return Err(DecodeError::BadBitIndex { bit });
            }
            let w = ordered_words(regs, 1, meta)?[0];
            Ok(TypedValue::Bool((w >> bit) & 1 == 1))
        }
        DT::Bitfield => {
            let w = ordered_words(regs, 1, meta)?[0];
            match meta.bit {
                Some(bit) if bit >= 16 => Err(DecodeError::BadBitIndex { bit }),
                Some(bit) => Ok(TypedValue::Bool((w >> bit) & 1 == 1)),
                None => Ok(TypedValue::UInt(u64::from(w))),
            }
        }
        DT::U16 => Ok(TypedValue::UInt(u64::from(ordered_words(regs, 1, meta)?[0]))),
        DT::I16 => Ok(TypedValue::Int(i64::from(ordered_words(regs, 1, meta)?[0] as i16))),
        DT::U32 => Ok(TypedValue::UInt(u64::from(combine_u32(&ordered_words(regs, 2, meta)?)))),
        DT::I32 => Ok(TypedValue::Int(i64::from(combine_u32(&ordered_words(regs, 2, meta)?) as i32))),
        DT::U64 => Ok(TypedValue::UInt(combine_u64(&ordered_words(regs, 4, meta)?))),
        DT::I64 => Ok(TypedValue::Int(combine_u64(&ordered_words(regs, 4, meta)?) as i64)),
        DT::F32 => Ok(TypedValue::Float(f64::from(f32::from_bits(combine_u32(
            &ordered_words(regs, 2, meta)?,
        ))))),
        DT::F64 => Ok(TypedValue::Float(f64::from_bits(combine_u64(
            &ordered_words(regs, 4, meta)?,
        )))),
        DT::Bcd => {
            // Variable width: use every register the poller delivered.
            let words = ordered_words(regs, regs.len().max(1), meta)?;
            let mut acc: u64 = 0;
            for w in words {
                for shift in [12u16, 8, 4, 0] {
                    let digit = (w >> shift) & 0xF;
                    if digit > 9 {
                        return Err(DecodeError::BadBcd { word: w });
                    }
                    acc = acc * 10 + u64::from(digit);
                }
            }
            Ok(TypedValue::UInt(acc))
        }
        DT::Ascii => {
            let words = ordered_words(regs, regs.len().max(1), meta)?;
            let mut bytes = Vec::with_capacity(words.len() * 2);
            for w in words {
                bytes.push((w >> 8) as u8);
                bytes.push((w & 0xFF) as u8);
            }
            let s: String = String::from_utf8_lossy(&bytes)
                .trim_end_matches(['\0', ' '])
                .to_string();
            Ok(TypedValue::Text(s))
        }
    }
}

/// Encode a typed value back into register words (phase-4 write path). The
/// exact inverse of [`decode`] for the fixed-width numeric types; ASCII/BCD
/// writes are out of scope for now (no known write use case).
pub fn encode(value: &TypedValue, meta: DecodeMeta) -> Result<Vec<u16>, DecodeError> {
    use DataType as DT;
    let unsupported = |what: &str| DecodeError::Encode {
        what: what.to_string(),
        dt: meta.data_type,
    };

    let to_u64 = |v: &TypedValue| -> Result<u64, DecodeError> {
        match v {
            TypedValue::UInt(u) => Ok(*u),
            TypedValue::Int(i) if *i >= 0 => Ok(*i as u64),
            TypedValue::Float(f) if f.fract() == 0.0 && *f >= 0.0 => Ok(*f as u64),
            other => Err(unsupported(&format!("{other:?}"))),
        }
    };
    let to_i64 = |v: &TypedValue| -> Result<i64, DecodeError> {
        match v {
            TypedValue::Int(i) => Ok(*i),
            TypedValue::UInt(u) => i64::try_from(*u).map_err(|_| unsupported("u64 overflow")),
            TypedValue::Float(f) if f.fract() == 0.0 => Ok(*f as i64),
            other => Err(unsupported(&format!("{other:?}"))),
        }
    };

    let words_msf: Vec<u16> = match meta.data_type {
        DT::U16 => vec![u16::try_from(to_u64(value)?).map_err(|_| unsupported("overflow"))?],
        DT::I16 => {
            let v = to_i64(value)?;
            let v = i16::try_from(v).map_err(|_| unsupported("overflow"))?;
            vec![v as u16]
        }
        DT::U32 => {
            let v = u32::try_from(to_u64(value)?).map_err(|_| unsupported("overflow"))?;
            vec![(v >> 16) as u16, v as u16]
        }
        DT::I32 => {
            let v = i32::try_from(to_i64(value)?).map_err(|_| unsupported("overflow"))?;
            let v = v as u32;
            vec![(v >> 16) as u16, v as u16]
        }
        DT::U64 => {
            let v = to_u64(value)?;
            vec![(v >> 48) as u16, (v >> 32) as u16, (v >> 16) as u16, v as u16]
        }
        DT::I64 => {
            let v = to_i64(value)? as u64;
            vec![(v >> 48) as u16, (v >> 32) as u16, (v >> 16) as u16, v as u16]
        }
        DT::F32 => {
            let f = value.as_f64().ok_or_else(|| unsupported("non-numeric"))?;
            let v = (f as f32).to_bits();
            vec![(v >> 16) as u16, v as u16]
        }
        DT::F64 => {
            let f = value.as_f64().ok_or_else(|| unsupported("non-numeric"))?;
            let v = f.to_bits();
            vec![(v >> 48) as u16, (v >> 32) as u16, (v >> 16) as u16, v as u16]
        }
        DT::Bit | DT::Bitfield | DT::Bcd | DT::Ascii => {
            return Err(unsupported("write of this data_type"));
        }
    };

    // Inverse of ordered_words: word order first, then per-word byte swap.
    let mut words = words_msf;
    if meta.word_order == WordOrder::LittleEndian {
        words.reverse();
    }
    if meta.byte_order == ByteOrder::LittleEndian {
        for w in &mut words {
            *w = w.swap_bytes();
        }
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn meta(dt: DataType) -> DecodeMeta {
        DecodeMeta {
            data_type: dt,
            word_order: WordOrder::BigEndian,
            byte_order: ByteOrder::BigEndian,
            bit: None,
        }
    }

    fn regs(vals: &[u16]) -> RawValue {
        RawValue::Registers(Arc::from(vals))
    }

    #[test]
    fn scalar_types_decode_big_endian() {
        assert_eq!(decode(&regs(&[1500]), meta(DataType::U16)), Ok(TypedValue::UInt(1500)));
        assert_eq!(
            decode(&regs(&[0xFFFF]), meta(DataType::I16)),
            Ok(TypedValue::Int(-1))
        );
        assert_eq!(
            decode(&regs(&[0x0001, 0x86A0]), meta(DataType::U32)),
            Ok(TypedValue::UInt(100_000))
        );
        assert_eq!(
            decode(&regs(&[0xFFFF, 0xFFFE]), meta(DataType::I32)),
            Ok(TypedValue::Int(-2))
        );
        assert_eq!(
            decode(&regs(&[0, 0, 0, 42]), meta(DataType::U64)),
            Ok(TypedValue::UInt(42))
        );
    }

    #[test]
    fn f32_and_f64_round_trip_via_bits() {
        // 12.34_f32 == 0x4145_70A4
        assert_eq!(
            decode(&regs(&[0x4145, 0x70A4]), meta(DataType::F32)),
            Ok(TypedValue::Float(f64::from(12.34_f32)))
        );
        let bits = 12.34_f64.to_bits();
        let words = [
            (bits >> 48) as u16,
            (bits >> 32) as u16,
            (bits >> 16) as u16,
            bits as u16,
        ];
        assert_eq!(decode(&regs(&words), meta(DataType::F64)), Ok(TypedValue::Float(12.34)));
    }

    #[test]
    fn word_order_little_endian_swaps_words() {
        // Same f32 with the words swapped ("swapped words" devices).
        let m = DecodeMeta {
            word_order: WordOrder::LittleEndian,
            ..meta(DataType::F32)
        };
        assert_eq!(
            decode(&regs(&[0x70A4, 0x4145]), m),
            Ok(TypedValue::Float(f64::from(12.34_f32)))
        );
    }

    #[test]
    fn byte_order_little_endian_swaps_within_words() {
        let m = DecodeMeta {
            byte_order: ByteOrder::LittleEndian,
            ..meta(DataType::U16)
        };
        // Device stored 0x1234 as 0x3412.
        assert_eq!(decode(&regs(&[0x3412]), m), Ok(TypedValue::UInt(0x1234)));

        // Combined with word swap for a u32.
        let m = DecodeMeta {
            word_order: WordOrder::LittleEndian,
            byte_order: ByteOrder::LittleEndian,
            ..meta(DataType::U32)
        };
        // Logical value 0x12345678: MSW=0x1234 LSW=0x5678; device sends
        // low-word-first with bytes swapped in each: [0x7856, 0x3412].
        assert_eq!(decode(&regs(&[0x7856, 0x3412]), m), Ok(TypedValue::UInt(0x1234_5678)));
    }

    #[test]
    fn bit_and_bitfield_selection() {
        let m = DecodeMeta { bit: Some(0), ..meta(DataType::Bit) };
        assert_eq!(decode(&regs(&[0b0000_0001]), m), Ok(TypedValue::Bool(true)));
        let m = DecodeMeta { bit: Some(1), ..meta(DataType::Bit) };
        assert_eq!(decode(&regs(&[0b0000_0001]), m), Ok(TypedValue::Bool(false)));

        // Bitfield without index = whole register as UInt.
        assert_eq!(
            decode(&regs(&[0b1010]), meta(DataType::Bitfield)),
            Ok(TypedValue::UInt(0b1010))
        );
        // Bitfield with index = that bit.
        let m = DecodeMeta { bit: Some(3), ..meta(DataType::Bitfield) };
        assert_eq!(decode(&regs(&[0b1010]), m), Ok(TypedValue::Bool(true)));

        // Coil payloads: first bit wins regardless of data_type.
        assert_eq!(
            decode(&RawValue::Bits(Arc::from([true].as_slice())), meta(DataType::Bit)),
            Ok(TypedValue::Bool(true))
        );
    }

    #[test]
    fn bcd_and_ascii_decode() {
        // BCD 1234 in one register.
        assert_eq!(decode(&regs(&[0x1234]), meta(DataType::Bcd)), Ok(TypedValue::UInt(1234)));
        // Two registers: 12345678.
        assert_eq!(
            decode(&regs(&[0x1234, 0x5678]), meta(DataType::Bcd)),
            Ok(TypedValue::UInt(12_345_678))
        );
        // Invalid nibble.
        assert_eq!(
            decode(&regs(&[0x12AF]), meta(DataType::Bcd)),
            Err(DecodeError::BadBcd { word: 0x12AF })
        );

        // ASCII "AB", "C" + NUL padding.
        assert_eq!(
            decode(&regs(&[0x4142, 0x4300]), meta(DataType::Ascii)),
            Ok(TypedValue::Text("ABC".into()))
        );
    }

    #[test]
    fn short_payload_is_an_error_not_a_panic() {
        assert_eq!(
            decode(&regs(&[0x0001]), meta(DataType::U32)),
            Err(DecodeError::TooShort { got: 1, need: 2 })
        );
        assert_eq!(decode(&RawValue::Absent, meta(DataType::U16)), Err(DecodeError::Absent));
    }

    #[test]
    fn encode_inverts_decode_for_numeric_types() {
        for (dt, val, regs_in) in [
            (DataType::U16, TypedValue::UInt(1500), vec![1500u16]),
            (DataType::I16, TypedValue::Int(-1), vec![0xFFFF]),
            (DataType::U32, TypedValue::UInt(100_000), vec![0x0001, 0x86A0]),
            (DataType::I32, TypedValue::Int(-2), vec![0xFFFF, 0xFFFE]),
            (DataType::F32, TypedValue::Float(f64::from(12.34_f32)), vec![0x4145, 0x70A4]),
        ] {
            for word_order in [WordOrder::BigEndian, WordOrder::LittleEndian] {
                for byte_order in [ByteOrder::BigEndian, ByteOrder::LittleEndian] {
                    let m = DecodeMeta { data_type: dt, word_order, byte_order, bit: None };
                    // encode(decode(x)) == x for the canonical big-endian regs
                    // transformed into this ordering.
                    let mut expected = regs_in.clone();
                    if word_order == WordOrder::LittleEndian {
                        expected.reverse();
                    }
                    if byte_order == ByteOrder::LittleEndian {
                        for w in &mut expected {
                            *w = w.swap_bytes();
                        }
                    }
                    let encoded = encode(&val, m).expect("encode");
                    assert_eq!(encoded, expected, "{dt:?} {word_order:?} {byte_order:?}");
                    assert_eq!(
                        decode(&RawValue::Registers(encoded.into()), m),
                        Ok(val.clone()),
                        "round-trip {dt:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn encode_rejects_unwritable_types_and_overflow() {
        assert!(encode(&TypedValue::UInt(70_000), meta(DataType::U16)).is_err());
        assert!(encode(&TypedValue::Text("x".into()), meta(DataType::U16)).is_err());
        assert!(encode(&TypedValue::UInt(1), meta(DataType::Ascii)).is_err());
        assert!(encode(&TypedValue::UInt(1), meta(DataType::Bcd)).is_err());
    }
}
