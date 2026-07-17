//! Pure, startup-time coalescing (design §5), per `(device, poll_group, area)`.
//!
//! Address-space units are handled per `Area` — coil-space for FC01/02,
//! register-space for FC03/04. Runtime never re-coalesces (adaptive
//! de-coalescing lives in `device.rs`, next stage). Custom and write entries
//! never reach this function (`plan.rs` emits them as singleton transactions).

use mb_types::Area;

use crate::plan::{Field, Transaction};

/// Per-device-resolved caps; the area cap comes from `Area::max_qty()`.
/// `max_gap` (default 0 = never bridge) tolerates small holes.
#[derive(Debug, Clone, Copy)]
pub struct Caps {
    pub max_gap: u16,
}

/// One register entry expanded to `[start, end)` in the area's own units.
/// Bounds are `u32` so an entry ending exactly at the top of the 16-bit
/// address space (`0xFFFF + width = 0x10000`) cannot wrap; `start` always
/// fits `u16` and `end - start` never exceeds `Area::max_qty()` on emit.
#[derive(Debug, Clone)]
pub struct Interval {
    pub start: u32,
    pub end: u32,
    pub field: Field,
}

/// Merge sorted intervals into wire transactions: bridge holes up to
/// `caps.max_gap`, never exceed the PDU cap for this area, and stamp each
/// field's `word_offset` relative to the run start.
pub fn coalesce(area: Area, mut ivals: Vec<Interval>, caps: Caps) -> Vec<Transaction> {
    ivals.sort_by_key(|i| i.start);
    let area_max = u32::from(area.max_qty());
    let max_gap = u32::from(caps.max_gap);
    let mut out = Vec::new();
    let mut run: Option<(u32 /*start*/, u32 /*end*/, Vec<Field>)> = None;

    for iv in ivals {
        match &mut run {
            Some((start, end, fields))
                // Bridge only small holes, never exceed the PDU cap for THIS area.
                if iv.start.saturating_sub(*end) <= max_gap
                    && iv.end.saturating_sub(*start) <= area_max =>
            {
                *end = (*end).max(iv.end);
                let mut f = iv.field;
                f.word_offset = (iv.start - *start) as u16;
                fields.push(f);
            }
            _ => {
                if let Some((s, e, fields)) = run.take() {
                    out.push(emit(area, s, e, fields));
                }
                let mut f0 = iv.field;
                f0.word_offset = 0;
                run = Some((iv.start, iv.end, vec![f0]));
            }
        }
    }
    if let Some((s, e, fields)) = run.take() {
        out.push(emit(area, s, e, fields));
    }
    out
}

fn emit(area: Area, start: u32, end: u32, fields: Vec<Field>) -> Transaction {
    let addr = start as u16;
    let qty = (end - start) as u16;
    let req = match area {
        Area::Coils => mb_proto::ModbusRequest::ReadCoils { addr, qty },
        Area::DiscreteInputs => mb_proto::ModbusRequest::ReadDiscreteInputs { addr, qty },
        Area::Holding => mb_proto::ModbusRequest::ReadHoldingRegisters { addr, qty },
        Area::Input => mb_proto::ModbusRequest::ReadInputRegisters { addr, qty },
    };
    let coalesced = fields.len() > 1;
    Transaction {
        req,
        base: addr,
        fields,
        coalesced,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mb_proto::ModbusRequest;
    use mb_types::{ByteOrder, DataType, TagId, WordOrder};

    fn field(tag: u32, word_len: u16) -> Field {
        Field {
            tag: TagId(tag),
            word_offset: 0,
            word_len,
            data_type: DataType::U16,
            word_order: WordOrder::default(),
            byte_order: ByteOrder::default(),
            bit: None,
        }
    }

    fn iv(start: u32, width: u16, tag: u32) -> Interval {
        Interval {
            start,
            end: start + u32::from(width),
            field: field(tag, width),
        }
    }

    fn addr_qty(req: &ModbusRequest) -> (u16, u16) {
        match req {
            ModbusRequest::ReadCoils { addr, qty }
            | ModbusRequest::ReadDiscreteInputs { addr, qty }
            | ModbusRequest::ReadHoldingRegisters { addr, qty }
            | ModbusRequest::ReadInputRegisters { addr, qty } => (*addr, *qty),
            other => panic!("unexpected request {other:?}"),
        }
    }

    struct Case {
        name: &'static str,
        area: Area,
        max_gap: u16,
        /// `(start, width)`; the field's tag = entry index.
        entries: Vec<(u32, u16)>,
        /// Per expected transaction: `(addr, qty, word_offsets)`.
        expected: Vec<(u16, u16, Vec<u16>)>,
    }

    #[test]
    fn coalesce_table() {
        let cases = vec![
            Case {
                name: "adjacent registers merge",
                area: Area::Holding,
                max_gap: 0,
                entries: vec![(100, 1), (101, 1)],
                expected: vec![(100, 2, vec![0, 1])],
            },
            Case {
                name: "gap <= max_gap bridges the hole",
                area: Area::Holding,
                max_gap: 2,
                entries: vec![(100, 1), (103, 1)],
                expected: vec![(100, 4, vec![0, 3])],
            },
            Case {
                name: "gap > max_gap splits",
                area: Area::Holding,
                max_gap: 1,
                entries: vec![(100, 1), (103, 1)],
                expected: vec![(100, 1, vec![0]), (103, 1, vec![0])],
            },
            Case {
                name: "register area cap 125 splits an adjacent run",
                area: Area::Holding,
                max_gap: 0,
                entries: vec![(0, 100), (100, 100)],
                expected: vec![(0, 100, vec![0]), (100, 100, vec![0])],
            },
            Case {
                name: "coil area cap allows a full 2000-bit run",
                area: Area::Coils,
                max_gap: 2000,
                entries: vec![(0, 1), (1999, 1)],
                expected: vec![(0, 2000, vec![0, 1999])],
            },
            Case {
                name: "coil area cap 2000 splits at 2001",
                area: Area::Coils,
                max_gap: 2000,
                entries: vec![(0, 1), (2000, 1)],
                expected: vec![(0, 1, vec![0]), (2000, 1, vec![0])],
            },
            Case {
                name: "multi-word f64 span merges with its neighbour",
                area: Area::Holding,
                max_gap: 0,
                entries: vec![(10, 4), (14, 2)],
                expected: vec![(10, 6, vec![0, 4])],
            },
            Case {
                name: "unsorted input is sorted before merging",
                area: Area::Holding,
                max_gap: 0,
                entries: vec![(101, 1), (100, 1)],
                expected: vec![(100, 2, vec![0, 1])],
            },
            Case {
                name: "two bitfield tags on the same register overlap into one read",
                area: Area::Holding,
                max_gap: 0,
                entries: vec![(5, 1), (5, 1)],
                expected: vec![(5, 1, vec![0, 0])],
            },
            Case {
                name: "word_offset stays run-relative across a bridged gap",
                area: Area::Input,
                max_gap: 3,
                entries: vec![(20, 2), (25, 1), (26, 2)],
                expected: vec![(20, 8, vec![0, 5, 6])],
            },
        ];

        for c in cases {
            let ivals = c
                .entries
                .iter()
                .enumerate()
                .map(|(i, (s, w))| iv(*s, *w, i as u32))
                .collect();
            let txns = coalesce(c.area, ivals, Caps { max_gap: c.max_gap });
            assert_eq!(txns.len(), c.expected.len(), "{}: transaction count", c.name);
            for (t, (addr, qty, offsets)) in txns.iter().zip(&c.expected) {
                assert_eq!(addr_qty(&t.req), (*addr, *qty), "{}: addr/qty", c.name);
                assert_eq!(t.base, *addr, "{}: base", c.name);
                let got: Vec<u16> = t.fields.iter().map(|f| f.word_offset).collect();
                assert_eq!(&got, offsets, "{}: word offsets", c.name);
                assert_eq!(
                    t.coalesced,
                    t.fields.len() > 1,
                    "{}: coalesced flag",
                    c.name
                );
            }
        }
    }

    #[test]
    fn emit_uses_the_area_specific_request_variant() {
        for (area, want) in [
            (Area::Coils, "ReadCoils"),
            (Area::DiscreteInputs, "ReadDiscreteInputs"),
            (Area::Holding, "ReadHoldingRegisters"),
            (Area::Input, "ReadInputRegisters"),
        ] {
            let txns = coalesce(area, vec![iv(1, 1, 0)], Caps { max_gap: 0 });
            let got = format!("{:?}", txns[0].req);
            assert!(got.starts_with(want), "{area:?}: got {got}");
        }
    }

    #[test]
    fn top_of_address_space_does_not_wrap() {
        // 0xFFFF + width 1 = 0x10000: must stay one clean read at 0xFFFF.
        let txns = coalesce(Area::Holding, vec![iv(0xFFFF, 1, 0)], Caps { max_gap: 0 });
        assert_eq!(txns.len(), 1);
        assert_eq!(addr_qty(&txns[0].req), (0xFFFF, 1));
    }
}
