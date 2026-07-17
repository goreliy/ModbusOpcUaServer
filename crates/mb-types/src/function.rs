//! Modbus function codes and the address areas read functions target.

use serde::{Deserialize, Serialize};

/// Read functions only feed the poll loop; writes travel the command path.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FunctionCode {
    ReadCoils,              // 01
    ReadDiscreteInputs,     // 02
    ReadHoldingRegisters,   // 03
    ReadInputRegisters,     // 04
    WriteSingleCoil,        // 05
    WriteSingleRegister,    // 06
    WriteMultipleCoils,     // 15
    WriteMultipleRegisters, // 16
    Custom { code: u8 },    // raw PDU passthrough (vendor functions)
}

impl FunctionCode {
    pub fn is_read(self) -> bool {
        matches!(
            self,
            Self::ReadCoils
                | Self::ReadDiscreteInputs
                | Self::ReadHoldingRegisters
                | Self::ReadInputRegisters
        )
    }

    /// Which address space this FC reads (used to bucket coalescing).
    pub fn read_area(self) -> Option<Area> {
        match self {
            Self::ReadCoils => Some(Area::Coils),
            Self::ReadDiscreteInputs => Some(Area::DiscreteInputs),
            Self::ReadHoldingRegisters => Some(Area::Holding),
            Self::ReadInputRegisters => Some(Area::Input),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Area {
    Coils,
    DiscreteInputs,
    Holding,
    Input,
}

impl Area {
    /// Whether the address unit is a single-bit coil or a 16-bit register.
    pub fn is_bit_domain(self) -> bool {
        matches!(self, Area::Coils | Area::DiscreteInputs)
    }

    /// Modbus PDU response cap in *this area's units*.
    pub fn max_qty(self) -> u16 {
        if self.is_bit_domain() {
            2000
        } else {
            125
        }
    }
}
