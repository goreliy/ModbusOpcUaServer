//! Typed request/response vocabulary + conversion to `tokio_modbus::Request`.

use std::borrow::Cow;

use tokio_modbus::prelude::Request;

#[derive(Debug, Clone)]
pub enum ModbusRequest {
    ReadCoils { addr: u16, qty: u16 },
    ReadDiscreteInputs { addr: u16, qty: u16 },
    ReadHoldingRegisters { addr: u16, qty: u16 },
    ReadInputRegisters { addr: u16, qty: u16 },
    WriteSingleCoil { addr: u16, value: bool },
    WriteSingleRegister { addr: u16, value: u16 },
    WriteMultipleCoils { addr: u16, values: Vec<bool> },
    WriteMultipleRegisters { addr: u16, values: Vec<u16> },
    /// Vendor/raw PDU. `expect_len` = expected response byte count — required by
    /// the poller for Custom reads on stream transports (RTU / RTU-over-TCP),
    /// where the byte stream cannot self-delimit vendor frames (design §3.2).
    Custom {
        code: u8,
        data: Vec<u8>,
        expect_len: Option<u16>,
    },
}

#[derive(Debug, Clone)]
pub enum ModbusResponse {
    Bits(Vec<bool>),     // FC01/02
    Registers(Vec<u16>), // FC03/04
    WriteAck,            // FC05/06/15/16
    Raw(bytes::Bytes),   // Custom
}

impl ModbusRequest {
    pub fn to_tokio_request(&self) -> Request<'static> {
        match self {
            Self::ReadCoils { addr, qty } => Request::ReadCoils(*addr, *qty),
            Self::ReadDiscreteInputs { addr, qty } => Request::ReadDiscreteInputs(*addr, *qty),
            Self::ReadHoldingRegisters { addr, qty } => Request::ReadHoldingRegisters(*addr, *qty),
            Self::ReadInputRegisters { addr, qty } => Request::ReadInputRegisters(*addr, *qty),
            Self::WriteSingleCoil { addr, value } => Request::WriteSingleCoil(*addr, *value),
            Self::WriteSingleRegister { addr, value } => Request::WriteSingleRegister(*addr, *value),
            Self::WriteMultipleCoils { addr, values } => {
                Request::WriteMultipleCoils(*addr, Cow::Owned(values.clone()))
            }
            Self::WriteMultipleRegisters { addr, values } => {
                Request::WriteMultipleRegisters(*addr, Cow::Owned(values.clone()))
            }
            Self::Custom { code, data, .. } => Request::Custom(*code, Cow::Owned(data.clone())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_tokio_request_mapping_table() {
        let cases: Vec<(ModbusRequest, Request<'static>)> = vec![
            (
                ModbusRequest::ReadCoils { addr: 3, qty: 4 },
                Request::ReadCoils(3, 4),
            ),
            (
                ModbusRequest::ReadDiscreteInputs { addr: 10, qty: 8 },
                Request::ReadDiscreteInputs(10, 8),
            ),
            (
                ModbusRequest::ReadHoldingRegisters { addr: 100, qty: 2 },
                Request::ReadHoldingRegisters(100, 2),
            ),
            (
                ModbusRequest::ReadInputRegisters { addr: 0, qty: 125 },
                Request::ReadInputRegisters(0, 125),
            ),
            (
                ModbusRequest::WriteSingleCoil {
                    addr: 7,
                    value: true,
                },
                Request::WriteSingleCoil(7, true),
            ),
            (
                ModbusRequest::WriteSingleRegister {
                    addr: 42,
                    value: 0xBEEF,
                },
                Request::WriteSingleRegister(42, 0xBEEF),
            ),
            (
                ModbusRequest::WriteMultipleCoils {
                    addr: 1,
                    values: vec![true, false, true],
                },
                Request::WriteMultipleCoils(1, Cow::Owned(vec![true, false, true])),
            ),
            (
                ModbusRequest::WriteMultipleRegisters {
                    addr: 200,
                    values: vec![1, 2, 3],
                },
                Request::WriteMultipleRegisters(200, Cow::Owned(vec![1, 2, 3])),
            ),
            (
                ModbusRequest::Custom {
                    code: 65,
                    data: vec![0x01, 0x02],
                    expect_len: Some(16),
                },
                Request::Custom(65, Cow::Owned(vec![0x01, 0x02])),
            ),
        ];
        for (ours, expected) in cases {
            assert_eq!(ours.to_tokio_request(), expected, "for {ours:?}");
        }
    }

    #[test]
    fn expect_len_does_not_leak_into_the_wire_request() {
        // Same wire PDU regardless of expect_len — it is poller-side metadata.
        let a = ModbusRequest::Custom {
            code: 65,
            data: vec![9],
            expect_len: Some(16),
        };
        let b = ModbusRequest::Custom {
            code: 65,
            data: vec![9],
            expect_len: None,
        };
        assert_eq!(a.to_tokio_request(), b.to_tokio_request());
    }
}
