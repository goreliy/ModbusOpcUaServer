//! `mb-types` — shared Modbus vocabulary (leaf crate).
//!
//! Holds the enums and dense-id newtypes shared by `gateway-config`,
//! `mb-proto` and `mb-poller` so none of them need to depend on each other
//! for vocabulary. No tokio, no I/O; serde derive only.

pub mod datatype;
pub mod function;
pub mod ids;

pub use datatype::{ByteOrder, DataType, WordOrder};
pub use function::{Area, FunctionCode};
pub use ids::{ChannelId, DeviceId, PollGroupId, TagId};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{from_str, to_string};

    #[test]
    fn function_code_serde_round_trip() {
        let cases: &[(FunctionCode, &str)] = &[
            (FunctionCode::ReadCoils, r#""read_coils""#),
            (FunctionCode::ReadDiscreteInputs, r#""read_discrete_inputs""#),
            (FunctionCode::ReadHoldingRegisters, r#""read_holding_registers""#),
            (FunctionCode::ReadInputRegisters, r#""read_input_registers""#),
            (FunctionCode::WriteSingleCoil, r#""write_single_coil""#),
            (FunctionCode::WriteSingleRegister, r#""write_single_register""#),
            (FunctionCode::WriteMultipleCoils, r#""write_multiple_coils""#),
            (
                FunctionCode::WriteMultipleRegisters,
                r#""write_multiple_registers""#,
            ),
            (FunctionCode::Custom { code: 0x41 }, r#"{"custom":{"code":65}}"#),
        ];
        for (fc, json) in cases {
            assert_eq!(&to_string(fc).unwrap(), json, "serialize {fc:?}");
            assert_eq!(&from_str::<FunctionCode>(json).unwrap(), fc, "parse {json}");
        }
    }

    #[test]
    fn data_type_serde_lowercase_round_trip() {
        let cases: &[(DataType, &str)] = &[
            (DataType::Bit, r#""bit""#),
            (DataType::U16, r#""u16""#),
            (DataType::I16, r#""i16""#),
            (DataType::U32, r#""u32""#),
            (DataType::I32, r#""i32""#),
            (DataType::U64, r#""u64""#),
            (DataType::I64, r#""i64""#),
            (DataType::F32, r#""f32""#),
            (DataType::F64, r#""f64""#),
            (DataType::Bcd, r#""bcd""#),
            (DataType::Ascii, r#""ascii""#),
            (DataType::Bitfield, r#""bitfield""#),
        ];
        for (dt, json) in cases {
            assert_eq!(&to_string(dt).unwrap(), json, "serialize {dt:?}");
            assert_eq!(&from_str::<DataType>(json).unwrap(), dt, "parse {json}");
        }
    }

    #[test]
    fn unknown_data_type_is_a_hard_error() {
        assert!(from_str::<DataType>(r#""u128""#).is_err());
        assert!(from_str::<DataType>(r#""float""#).is_err());
    }

    #[test]
    fn word_and_byte_order_serde_snake_case() {
        assert_eq!(to_string(&WordOrder::BigEndian).unwrap(), r#""big_endian""#);
        assert_eq!(
            to_string(&WordOrder::LittleEndian).unwrap(),
            r#""little_endian""#
        );
        assert_eq!(
            from_str::<WordOrder>(r#""big_endian""#).unwrap(),
            WordOrder::BigEndian
        );
        assert_eq!(
            from_str::<ByteOrder>(r#""little_endian""#).unwrap(),
            ByteOrder::LittleEndian
        );
        assert_eq!(WordOrder::default(), WordOrder::BigEndian);
        assert_eq!(ByteOrder::default(), ByteOrder::BigEndian);
    }

    #[test]
    fn register_count_table() {
        assert_eq!(DataType::Bit.register_count(), Some(1));
        assert_eq!(DataType::U16.register_count(), Some(1));
        assert_eq!(DataType::I16.register_count(), Some(1));
        assert_eq!(DataType::Bitfield.register_count(), Some(1));
        assert_eq!(DataType::U32.register_count(), Some(2));
        assert_eq!(DataType::I32.register_count(), Some(2));
        assert_eq!(DataType::F32.register_count(), Some(2));
        assert_eq!(DataType::U64.register_count(), Some(4));
        assert_eq!(DataType::I64.register_count(), Some(4));
        assert_eq!(DataType::F64.register_count(), Some(4));
        assert_eq!(DataType::Bcd.register_count(), None);
        assert_eq!(DataType::Ascii.register_count(), None);
    }

    #[test]
    fn area_caps_and_bit_domain() {
        assert!(Area::Coils.is_bit_domain());
        assert!(Area::DiscreteInputs.is_bit_domain());
        assert!(!Area::Holding.is_bit_domain());
        assert!(!Area::Input.is_bit_domain());
        assert_eq!(Area::Coils.max_qty(), 2000);
        assert_eq!(Area::DiscreteInputs.max_qty(), 2000);
        assert_eq!(Area::Holding.max_qty(), 125);
        assert_eq!(Area::Input.max_qty(), 125);
    }

    #[test]
    fn read_area_mapping() {
        assert_eq!(FunctionCode::ReadCoils.read_area(), Some(Area::Coils));
        assert_eq!(
            FunctionCode::ReadDiscreteInputs.read_area(),
            Some(Area::DiscreteInputs)
        );
        assert_eq!(
            FunctionCode::ReadHoldingRegisters.read_area(),
            Some(Area::Holding)
        );
        assert_eq!(
            FunctionCode::ReadInputRegisters.read_area(),
            Some(Area::Input)
        );
        assert_eq!(FunctionCode::WriteSingleCoil.read_area(), None);
        assert_eq!(FunctionCode::Custom { code: 0x41 }.read_area(), None);
        assert!(FunctionCode::ReadCoils.is_read());
        assert!(!FunctionCode::WriteMultipleRegisters.is_read());
        assert!(!FunctionCode::Custom { code: 1 }.is_read());
    }

    #[test]
    fn ids_serde_as_plain_numbers() {
        assert_eq!(to_string(&TagId(7)).unwrap(), "7");
        assert_eq!(from_str::<TagId>("7").unwrap(), TagId(7));
        assert_eq!(from_str::<ChannelId>("2").unwrap(), ChannelId(2));
        assert_eq!(from_str::<DeviceId>("9").unwrap(), DeviceId(9));
        assert_eq!(from_str::<PollGroupId>("1").unwrap(), PollGroupId(1));
    }
}
