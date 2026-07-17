//! Compile step (design §4): `ResolvedConfig` -> `Vec<ChannelPlan>`.
//!
//! Registers are bucketed per `(device, poll_group, area)` and coalesced at
//! build time; the runtime never re-coalesces (except adaptive de-coalescing
//! in `device.rs`, next stage). Custom entries never coalesce.

use std::time::Duration;

use gateway_config::schema::v1::{RetryConfig, TransportConfig};
use gateway_config::{ResolvedConfig, ResolvedDevice, ResolvedRegister};
use mb_proto::ModbusRequest;
use mb_types::{
    Area, ByteOrder, ChannelId, DataType, DeviceId, FunctionCode, PollGroupId, TagId, WordOrder,
};

use crate::coalesce::{coalesce, Caps, Interval};

#[derive(Debug, Clone)]
pub struct ChannelPlan {
    pub id: ChannelId,
    /// Channel name from the config (log context, traffic-dump tag).
    pub name: String,
    pub transport: TransportConfig,
    pub request_timeout: Duration,
    pub inter_request_delay: Duration,
    /// Resolved: currently always 1 (sequential runtime for all transports).
    pub max_inflight: usize,
    /// Hex-dump raw frames to the `modbus_traffic` tracing target.
    pub log_traffic: bool,
    pub retry: RetryConfig,
    /// Distinct poll groups referenced on this channel, ascending `PollGroupId`.
    /// `DevicePlan::by_group` is index-aligned with this vec.
    pub groups: Vec<(PollGroupId, Duration)>,
    /// Index-aligned with `groups`: the poll group's scheduling priority
    /// (higher first when several groups are due in the same tick).
    pub group_priorities: Vec<i32>,
    pub devices: Vec<DevicePlan>,
}

#[derive(Debug, Clone)]
pub struct DevicePlan {
    pub id: DeviceId,
    pub unit: u8,
    pub offline_after: u32,
    /// Resolved device -> channel -> gateway default (finding #9/#15/#31).
    pub request_timeout: Duration,
    pub retry: RetryConfig,
    /// For the watchdog bulk-Bad sweep.
    pub all_tags: Vec<TagId>,
    /// COALESCED at build time. One entry per `ChannelPlan::groups` element
    /// (same order; empty vec when this device has nothing in that group), so
    /// `(device_idx, group_idx)` from the scheduler indexes directly.
    pub by_group: Vec<(PollGroupId, Vec<Transaction>)>,
}

/// A compiled wire request plus the map back into cache slots.
#[derive(Debug, Clone)]
pub struct Transaction {
    pub req: ModbusRequest,
    pub base: u16,
    /// Scatter targets.
    pub fields: Vec<Field>,
    /// True if it merged >1 entry (used by adaptive de-coalescing).
    pub coalesced: bool,
}

impl Transaction {
    /// The `TagId`s this transaction feeds (quality sweeps on timeout/exception).
    pub fn tags(&self) -> Vec<TagId> {
        self.fields.iter().map(|f| f.tag).collect()
    }
}

#[derive(Debug, Clone)]
pub struct Field {
    pub tag: TagId,
    /// Slice start within the response, in area units.
    pub word_offset: u16,
    /// Span in area units (registers / coils; response bytes for Custom).
    pub word_len: u16,
    pub data_type: DataType,
    pub word_order: WordOrder,
    pub byte_order: ByteOrder,
    pub bit: Option<u8>,
}

/// Compile every enabled channel. Disabled channels/devices are excluded from
/// the plan entirely (their `TagId`s stay allocated in the cache and simply
/// remain never-read).
pub fn build_all(cfg: &ResolvedConfig) -> Vec<ChannelPlan> {
    cfg.channels
        .iter()
        .filter(|ch| ch.enabled)
        .map(|ch| {
            let mut group_ids: Vec<PollGroupId> = ch
                .devices
                .iter()
                .filter(|d| d.enabled)
                .flat_map(|d| d.registers.iter().map(|r| r.poll_group))
                .collect();
            group_ids.sort_unstable_by_key(|g| g.0);
            group_ids.dedup();

            let groups = group_ids
                .iter()
                .map(|g| {
                    let pg = &cfg.poll_groups[g.0 as usize];
                    (*g, Duration::from_millis(pg.period_ms))
                })
                .collect();
            let group_priorities = group_ids
                .iter()
                .map(|g| cfg.poll_groups[g.0 as usize].priority)
                .collect();

            let devices = ch
                .devices
                .iter()
                .filter(|d| d.enabled)
                .map(|d| build_device(d, &group_ids))
                .collect();

            ChannelPlan {
                id: ch.id,
                name: ch.name.clone(),
                transport: ch.transport.clone(),
                request_timeout: Duration::from_millis(ch.request_timeout_ms),
                inter_request_delay: Duration::from_millis(ch.inter_request_delay_ms),
                max_inflight: ch.max_inflight,
                log_traffic: ch.log_traffic,
                retry: ch.retry,
                groups,
                group_priorities,
                devices,
            }
        })
        .collect()
}

fn build_device(dev: &ResolvedDevice, group_ids: &[PollGroupId]) -> DevicePlan {
    let all_tags = dev.registers.iter().map(|r| r.tag).collect();
    let by_group = group_ids
        .iter()
        .map(|g| {
            let mut txns: Vec<Transaction> = Vec::new();
            // Bucket per area: one Modbus PDU carries one function, so FC03
            // and FC04 (etc.) never merge.
            for area in [Area::Coils, Area::DiscreteInputs, Area::Holding, Area::Input] {
                let ivals: Vec<Interval> = dev
                    .registers
                    .iter()
                    .filter(|r| r.poll_group == *g && r.function.read_area() == Some(area))
                    .map(|r| to_interval(r, area))
                    .collect();
                if !ivals.is_empty() {
                    txns.extend(coalesce(area, ivals, Caps { max_gap: dev.max_gap }));
                }
            }
            // Custom entries never coalesce: one transaction each.
            for r in dev.registers.iter().filter(|r| r.poll_group == *g) {
                if let FunctionCode::Custom { code } = r.function {
                    txns.push(custom_txn(r, code));
                }
            }
            (*g, txns)
        })
        .collect();

    DevicePlan {
        id: dev.id,
        unit: dev.unit_id,
        offline_after: dev.offline_after_failures,
        request_timeout: Duration::from_millis(dev.request_timeout_ms),
        retry: dev.retry,
        all_tags,
        by_group,
    }
}

/// Expand one entry to `[start, end)` in the area's own units (§5 rule 2):
/// coil-space width 1 for FC01/02, register span via `register_count()` or
/// `length` (ascii/bcd) for FC03/04.
fn to_interval(r: &ResolvedRegister, area: Area) -> Interval {
    let width = if area.is_bit_domain() {
        1
    } else {
        r.data_type.register_count().or(r.length).unwrap_or(1)
    };
    Interval {
        start: u32::from(r.address),
        end: u32::from(r.address) + u32::from(width),
        field: field_of(r, width),
    }
}

fn field_of(r: &ResolvedRegister, width: u16) -> Field {
    Field {
        tag: r.tag,
        word_offset: 0, // finalized by coalesce()
        word_len: width,
        data_type: r.data_type,
        word_order: r.word_order,
        byte_order: r.byte_order,
        bit: r.bit,
    }
}

fn custom_txn(r: &ResolvedRegister, code: u8) -> Transaction {
    Transaction {
        // B4: the PDU is the function code followed by the (possibly empty)
        // `custom_request` payload bytes. `expect_len` bounds the reply.
        req: ModbusRequest::Custom {
            code,
            data: r.custom_request.clone(),
            expect_len: r.custom_response_len,
        },
        base: r.address,
        fields: vec![Field {
            word_len: r.custom_response_len.unwrap_or(0), // bytes for Custom
            ..field_of(r, 0)
        }],
        coalesced: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rc(json: &str) -> ResolvedConfig {
        gateway_config::load_str(json).expect("test config must load")
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

    #[test]
    fn multiword_span_merges_with_correct_offsets_and_channel_knobs() {
        let cfg = rc(r#"{
            "schema_version": "1",
            "poll_groups": [ { "id": "fast", "period_ms": 200 } ],
            "channels": [ {
                "id": "bus", "max_gap": 2,
                "transport": { "type": "rtu_over_tcp", "host": "10.0.0.7", "port": 4001 },
                "request_timeout_ms": 750,
                "inter_request_delay_ms": 20,
                "devices": [ {
                    "id": "d1", "unit_id": 5,
                    "registers": [
                        { "tag": "f",  "poll_group": "fast", "function": "read_holding_registers", "address": 100, "data_type": "f32" },
                        { "tag": "a",  "poll_group": "fast", "function": "read_holding_registers", "address": 102, "data_type": "u16" },
                        { "tag": "b",  "poll_group": "fast", "function": "read_holding_registers", "address": 105, "data_type": "u16" }
                    ]
                } ]
            } ]
        }"#);
        let plans = build_all(&cfg);
        assert_eq!(plans.len(), 1);
        let p = &plans[0];
        assert_eq!(p.id, ChannelId(0));
        assert_eq!(p.request_timeout, Duration::from_millis(750));
        assert_eq!(p.inter_request_delay, Duration::from_millis(20));
        assert_eq!(p.max_inflight, 1, "forced to 1 on RtuOverTcp");
        assert_eq!(p.groups, vec![(PollGroupId(0), Duration::from_millis(200))]);

        let d = &p.devices[0];
        assert_eq!(d.unit, 5);
        assert_eq!(d.all_tags, vec![TagId(0), TagId(1), TagId(2)]);
        let txns = &d.by_group[0].1;
        assert_eq!(txns.len(), 1, "f32@100 + u16@102 + gap2 + u16@105 -> one txn");
        let t = &txns[0];
        assert_eq!(addr_qty(&t.req), (100, 6));
        assert_eq!(t.base, 100);
        assert!(t.coalesced);
        let offsets: Vec<u16> = t.fields.iter().map(|f| f.word_offset).collect();
        assert_eq!(offsets, vec![0, 2, 5]);
        let lens: Vec<u16> = t.fields.iter().map(|f| f.word_len).collect();
        assert_eq!(lens, vec![2, 1, 1]);
    }

    #[test]
    fn device_timeout_and_retry_overrides_reach_the_plan() {
        // #9/#15/#31: the resolved per-device knobs must survive plan compile.
        let cfg = rc(r#"{
            "schema_version": "1",
            "poll_groups": [ { "id": "fast", "period_ms": 200 } ],
            "channels": [ {
                "id": "bus",
                "transport": { "type": "tcp", "host": "h" },
                "request_timeout_ms": 500,
                "retry": { "max_retries": 2 },
                "devices": [
                    { "id": "slow-meter", "unit_id": 1,
                      "request_timeout_ms": 2000,
                      "retry": { "max_retries": 0 },
                      "registers": [
                        { "tag": "m1", "poll_group": "fast", "function": "read_holding_registers", "address": 0, "data_type": "u16" }
                    ] },
                    { "id": "plain", "unit_id": 2, "registers": [
                        { "tag": "p1", "poll_group": "fast", "function": "read_holding_registers", "address": 0, "data_type": "u16" }
                    ] }
                ]
            } ]
        }"#);
        let p = &build_all(&cfg)[0];
        assert_eq!(p.devices[0].request_timeout, Duration::from_millis(2000));
        assert_eq!(p.devices[0].retry.max_retries, 0);
        assert_eq!(p.devices[1].request_timeout, Duration::from_millis(500), "inherits channel");
        assert_eq!(p.devices[1].retry.max_retries, 2, "inherits channel");
    }

    #[test]
    fn fc03_and_fc04_never_merge() {
        let cfg = rc(r#"{
            "schema_version": "1",
            "poll_groups": [ { "id": "fast", "period_ms": 200 } ],
            "channels": [ {
                "id": "c", "max_gap": 10,
                "transport": { "type": "tcp", "host": "h" },
                "devices": [ {
                    "id": "d", "unit_id": 1,
                    "registers": [
                        { "tag": "h1", "poll_group": "fast", "function": "read_holding_registers", "address": 0, "data_type": "u16" },
                        { "tag": "i1", "poll_group": "fast", "function": "read_input_registers",   "address": 1, "data_type": "u16" }
                    ]
                } ]
            } ]
        }"#);
        let txns = &build_all(&cfg)[0].devices[0].by_group[0].1;
        assert_eq!(txns.len(), 2, "adjacent but different areas must not merge");
        assert!(matches!(txns[0].req, ModbusRequest::ReadHoldingRegisters { addr: 0, qty: 1 }));
        assert!(matches!(txns[1].req, ModbusRequest::ReadInputRegisters { addr: 1, qty: 1 }));
        assert!(!txns[0].coalesced && !txns[1].coalesced);
    }

    #[test]
    fn custom_entries_never_coalesce_and_carry_expect_len() {
        let cfg = rc(r#"{
            "schema_version": "1",
            "poll_groups": [ { "id": "fast", "period_ms": 200 } ],
            "channels": [ {
                "id": "c", "max_gap": 50,
                "transport": { "type": "rtu_over_tcp", "host": "h", "port": 4001 },
                "devices": [ {
                    "id": "d", "unit_id": 1,
                    "registers": [
                        { "tag": "h1", "poll_group": "fast", "function": "read_holding_registers", "address": 0, "data_type": "u16" },
                        { "tag": "h2", "poll_group": "fast", "function": "read_holding_registers", "address": 1, "data_type": "u16" },
                        { "tag": "x1", "poll_group": "fast", "function": { "custom": { "code": 65 } }, "address": 0, "data_type": "u16", "custom_request": "01 A0 ff", "custom_response_len": 8 },
                        { "tag": "x2", "poll_group": "fast", "function": { "custom": { "code": 66 } }, "address": 1, "data_type": "u16", "custom_response_len": 8 }
                    ]
                } ]
            } ]
        }"#);
        let txns = &build_all(&cfg)[0].devices[0].by_group[0].1;
        // One merged holding read + two Custom singletons, despite max_gap 50.
        assert_eq!(txns.len(), 3);
        assert!(matches!(txns[0].req, ModbusRequest::ReadHoldingRegisters { addr: 0, qty: 2 }));
        // B4: x1 carries its parsed custom_request payload; x2 has none.
        for (t, (code, payload)) in txns[1..]
            .iter()
            .zip([(65u8, vec![0x01u8, 0xa0, 0xff]), (66, vec![])])
        {
            assert!(!t.coalesced);
            assert_eq!(t.fields.len(), 1);
            match &t.req {
                ModbusRequest::Custom { code: c, data, expect_len } => {
                    assert_eq!(*c, code);
                    assert_eq!(*data, payload, "plan must carry the custom_request bytes");
                    assert_eq!(*expect_len, Some(8));
                }
                other => panic!("expected Custom, got {other:?}"),
            }
        }
    }

    #[test]
    fn groups_align_with_by_group_and_disabled_things_are_excluded() {
        let cfg = rc(r#"{
            "schema_version": "1",
            "poll_groups": [
                { "id": "fast", "period_ms": 200 },
                { "id": "slow", "period_ms": 2000 }
            ],
            "channels": [ {
                "id": "c1",
                "transport": { "type": "tcp", "host": "h" },
                "devices": [
                    { "id": "devA", "unit_id": 1, "registers": [
                        { "tag": "a1", "poll_group": "slow", "function": "read_holding_registers", "address": 0, "data_type": "u16" }
                    ] },
                    { "id": "devB", "unit_id": 2, "registers": [
                        { "tag": "b1", "poll_group": "fast", "function": "read_coils", "address": 3, "data_type": "bit" },
                        { "tag": "b2", "poll_group": "slow", "function": "read_holding_registers", "address": 10, "data_type": "u16" }
                    ] },
                    { "id": "devC", "unit_id": 3, "enabled": false, "registers": [
                        { "tag": "c1t", "poll_group": "fast", "function": "read_holding_registers", "address": 0, "data_type": "u16" }
                    ] }
                ]
            }, {
                "id": "off", "enabled": false,
                "transport": { "type": "tcp", "host": "h2" },
                "devices": [ { "id": "devD", "unit_id": 1, "registers": [
                    { "tag": "d1t", "poll_group": "fast", "function": "read_holding_registers", "address": 0, "data_type": "u16" }
                ] } ]
            } ]
        }"#);
        let plans = build_all(&cfg);
        assert_eq!(plans.len(), 1, "disabled channel excluded");
        let p = &plans[0];
        assert_eq!(
            p.groups,
            vec![
                (PollGroupId(0), Duration::from_millis(200)),
                (PollGroupId(1), Duration::from_millis(2000)),
            ]
        );
        assert_eq!(p.devices.len(), 2, "disabled device excluded");

        // by_group is index-aligned with plan.groups for every device.
        for d in &p.devices {
            assert_eq!(d.by_group.len(), p.groups.len());
            for (i, (g, _)) in p.groups.iter().enumerate() {
                assert_eq!(d.by_group[i].0, *g);
            }
        }
        // devA has nothing in "fast" -> empty slot, one slow txn.
        let dev_a = &p.devices[0];
        assert!(dev_a.by_group[0].1.is_empty());
        assert_eq!(dev_a.by_group[1].1.len(), 1);
        // devB: coil read (width 1) in fast, holding in slow.
        let dev_b = &p.devices[1];
        assert!(matches!(dev_b.by_group[0].1[0].req, ModbusRequest::ReadCoils { addr: 3, qty: 1 }));
        assert!(matches!(dev_b.by_group[1].1[0].req, ModbusRequest::ReadHoldingRegisters { addr: 10, qty: 1 }));
    }

    #[test]
    fn device_max_gap_override_controls_bridging() {
        let cfg = rc(r#"{
            "schema_version": "1",
            "poll_groups": [ { "id": "fast", "period_ms": 200 } ],
            "channels": [ {
                "id": "c", "max_gap": 5,
                "transport": { "type": "tcp", "host": "h" },
                "devices": [
                    { "id": "bridges", "unit_id": 1, "registers": [
                        { "tag": "p1", "poll_group": "fast", "function": "read_holding_registers", "address": 0, "data_type": "u16" },
                        { "tag": "p2", "poll_group": "fast", "function": "read_holding_registers", "address": 4, "data_type": "u16" }
                    ] },
                    { "id": "strict", "unit_id": 2, "max_gap": 0, "registers": [
                        { "tag": "q1", "poll_group": "fast", "function": "read_holding_registers", "address": 0, "data_type": "u16" },
                        { "tag": "q2", "poll_group": "fast", "function": "read_holding_registers", "address": 4, "data_type": "u16" }
                    ] }
                ]
            } ]
        }"#);
        let plans = build_all(&cfg);
        assert_eq!(plans[0].devices[0].by_group[0].1.len(), 1, "channel max_gap=5 bridges");
        assert_eq!(plans[0].devices[1].by_group[0].1.len(), 2, "device max_gap=0 forbids holes");
    }
}
