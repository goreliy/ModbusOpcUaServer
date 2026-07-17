//! Resolve step: intern string names into dense ids, assign each channel an
//! **exclusive contiguous `TagId` range** (makes single-writer-per-slot
//! structural), and fully resolve per-device overrides
//! (device override → channel → gateway default).

use std::collections::HashMap;
use std::ops::Range;

use mb_types::{ByteOrder, ChannelId, DataType, DeviceId, FunctionCode, PollGroupId, TagId, WordOrder};

use crate::schema::v1::{
    ConfigV1, GatewaySettings, LoggingConfig, OpcUaConfig, RetryConfig, TransportConfig,
};
use crate::ConfigError;

/// What leaves the crate: identical shape to `ConfigV1` but names replaced by
/// dense ids and every per-device knob fully resolved.
#[derive(Debug)]
pub struct ResolvedConfig {
    pub gateway: GatewaySettings,
    /// OPC UA server settings (verbatim from the schema — nothing to intern).
    pub opcua: OpcUaConfig,
    pub logging: LoggingConfig,
    pub poll_groups: Vec<ResolvedPollGroup>,
    pub channels: Vec<ResolvedChannel>,
    /// Index = `TagId.0`; dense and contiguous from 0.
    pub tag_names: Vec<String>,
    /// Non-fatal findings from validation (e.g. `BusOverload`).
    pub warnings: Vec<ConfigError>,
}

impl ResolvedConfig {
    pub fn tag_count(&self) -> usize {
        self.tag_names.len()
    }

    pub fn tag_name(&self, tag: TagId) -> Option<&str> {
        self.tag_names.get(tag.0 as usize).map(String::as_str)
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedPollGroup {
    pub id: PollGroupId,
    pub name: String,
    pub period_ms: u64,
    pub priority: i32,
}

#[derive(Debug, Clone)]
pub struct ResolvedChannel {
    pub id: ChannelId,
    pub name: String,
    pub enabled: bool,
    pub transport: TransportConfig,
    pub request_timeout_ms: u64,
    pub inter_request_delay_ms: u64,
    /// Forced to 1 for ALL transports: parallel TCP polling is not
    /// implemented yet (validation warns when a Tcp channel asks for more).
    /// The schema field remains for the future upgrade.
    pub max_inflight: usize,
    pub retry: RetryConfig,
    pub offline_after_failures: u32,
    pub max_gap: u16,
    pub log_traffic: bool,
    /// Exclusive contiguous `TagId` range owned by this channel's task.
    pub tag_range: Range<u32>,
    pub devices: Vec<ResolvedDevice>,
}

#[derive(Debug, Clone)]
pub struct ResolvedDevice {
    pub id: DeviceId,
    pub name: String,
    pub channel: ChannelId,
    pub unit_id: u8,
    pub enabled: bool,
    // Fully resolved: device override → channel value (→ gateway default).
    pub request_timeout_ms: u64,
    pub retry: RetryConfig,
    pub offline_after_failures: u32,
    pub max_gap: u16,
    pub registers: Vec<ResolvedRegister>,
}

#[derive(Debug, Clone)]
pub struct ResolvedRegister {
    pub tag: TagId,
    pub poll_group: PollGroupId,
    pub function: FunctionCode,
    pub address: u16,
    pub data_type: DataType,
    pub word_order: WordOrder,
    pub byte_order: ByteOrder,
    pub length: Option<u16>,
    pub bit: Option<u8>,
    pub custom_response_len: Option<u16>,
    /// Parsed `custom_request` payload bytes; empty when the entry has none.
    pub custom_request: Vec<u8>,
    pub scale: f64,
    pub offset: f64,
    // Phase 2 (tags-core) metadata; the poller ignores everything below.
    pub formula: Option<String>,
    pub write_formula: Option<String>,
    pub deadband: Option<f64>,
    pub retentive: bool,
    pub retain_last: Option<u16>,
    pub units: Option<String>,
    pub writable: bool,
    /// Explicit write FC override (None = natural choice). See schema docs.
    pub write_function: Option<FunctionCode>,
}

/// Intern a *validated* `ConfigV1`. Must run after `validate::validate`
/// returned no hard errors — FK lookups assume referential integrity.
pub fn resolve(cfg: ConfigV1, warnings: Vec<ConfigError>) -> ResolvedConfig {
    let poll_groups: Vec<ResolvedPollGroup> = cfg
        .poll_groups
        .iter()
        .enumerate()
        .map(|(i, g)| ResolvedPollGroup {
            id: PollGroupId(i as u16),
            name: g.id.clone(),
            period_ms: g.period_ms,
            priority: g.priority,
        })
        .collect();
    let group_by_name: HashMap<&str, PollGroupId> = cfg
        .poll_groups
        .iter()
        .enumerate()
        .map(|(i, g)| (g.id.as_str(), PollGroupId(i as u16)))
        .collect();

    let mut tag_names: Vec<String> = Vec::new();
    let mut channels: Vec<ResolvedChannel> = Vec::with_capacity(cfg.channels.len());
    let mut next_device: u32 = 0;

    for (ci, ch) in cfg.channels.iter().enumerate() {
        let channel_id = ChannelId(ci as u16);
        let range_start = tag_names.len() as u32;
        // Middle level of the retry chain: channel override → gateway default.
        let ch_retry = ch.retry.unwrap_or(cfg.gateway.default_retry);

        let devices: Vec<ResolvedDevice> = ch
            .devices
            .iter()
            .map(|dev| {
                let device_id = DeviceId(next_device);
                next_device += 1;
                let registers = dev
                    .registers
                    .iter()
                    .map(|r| {
                        let tag = TagId(tag_names.len() as u32);
                        tag_names.push(r.tag.clone());
                        ResolvedRegister {
                            tag,
                            poll_group: *group_by_name
                                .get(r.poll_group.as_str())
                                .expect("validated: poll_group FK resolves"),
                            function: r.function,
                            address: r.address,
                            data_type: r.data_type,
                            word_order: r.word_order,
                            byte_order: r.byte_order,
                            length: r.length,
                            bit: r.bit,
                            custom_response_len: r.custom_response_len,
                            custom_request: r
                                .custom_request
                                .as_deref()
                                .map(|s| {
                                    crate::validate::parse_custom_request(s)
                                        .expect("validated: custom_request parses")
                                })
                                .unwrap_or_default(),
                            scale: r.scale,
                            offset: r.offset,
                            formula: r.formula.clone(),
                            write_formula: r.write_formula.clone(),
                            deadband: r.deadband,
                            retentive: r.retentive,
                            retain_last: r.retain_last,
                            units: r.units.clone(),
                            writable: r.writable,
                            write_function: r.write_function,
                        }
                    })
                    .collect();
                ResolvedDevice {
                    id: device_id,
                    name: dev.id.clone(),
                    channel: channel_id,
                    unit_id: dev.unit_id,
                    enabled: dev.enabled && ch.enabled,
                    request_timeout_ms: dev.request_timeout_ms.unwrap_or(ch.request_timeout_ms),
                    retry: dev.retry.unwrap_or(ch_retry),
                    offline_after_failures: dev
                        .offline_after_failures
                        .unwrap_or(ch.offline_after_failures),
                    max_gap: dev.max_gap.unwrap_or(ch.max_gap),
                    registers,
                }
            })
            .collect();

        channels.push(ResolvedChannel {
            id: channel_id,
            name: ch.id.clone(),
            enabled: ch.enabled,
            transport: ch.transport.clone(),
            request_timeout_ms: ch.request_timeout_ms,
            inter_request_delay_ms: ch.inter_request_delay_ms,
            // B3: exactly one request in flight for EVERY transport — the
            // sequential runtime is the honest truth; validation already
            // warned when a Tcp channel asked for more.
            max_inflight: 1,
            retry: ch_retry,
            offline_after_failures: ch.offline_after_failures,
            max_gap: ch.max_gap,
            log_traffic: ch.log_traffic,
            tag_range: range_start..tag_names.len() as u32,
            devices,
        });
    }

    ResolvedConfig {
        gateway: cfg.gateway,
        opcua: cfg.opcua,
        logging: cfg.logging,
        poll_groups,
        channels,
        tag_names,
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::*;
    use mb_types::{DataType as DT, FunctionCode as FC};

    fn two_channel_config() -> ConfigV1 {
        let mut dev1 = device(
            "d1",
            1,
            vec![
                entry("a.one", FC::ReadHoldingRegisters, 0, DT::U16),
                entry("a.two", FC::ReadHoldingRegisters, 10, DT::F32),
            ],
        );
        dev1.request_timeout_ms = Some(250);
        dev1.max_gap = Some(8);
        dev1.offline_after_failures = Some(10);
        let dev2 = device("d2", 2, vec![entry("a.three", FC::ReadCoils, 0, DT::Bit)]);
        let mut rtu = rtu_channel("bus1", 19200, vec![dev1, dev2]);
        rtu.max_inflight = 1;
        rtu.max_gap = 2;

        let mut tcp = tcp_channel(
            "plc",
            vec![device("d3", 1, vec![entry("b.one", FC::ReadInputRegisters, 5, DT::I16)])],
        );
        tcp.max_inflight = 4;

        cfg(vec![pg("fast", 200), pg("slow", 2000)], vec![rtu, tcp])
    }

    #[test]
    fn ids_are_dense_and_contiguous() {
        let rc = resolve(two_channel_config(), vec![]);

        assert_eq!(rc.tag_count(), 4);
        assert_eq!(rc.tag_names, vec!["a.one", "a.two", "a.three", "b.one"]);

        // Channels and poll groups numbered by declaration order.
        assert_eq!(rc.channels[0].id, ChannelId(0));
        assert_eq!(rc.channels[1].id, ChannelId(1));
        assert_eq!(rc.poll_groups[0].id, PollGroupId(0));
        assert_eq!(rc.poll_groups[1].id, PollGroupId(1));
        assert_eq!(rc.poll_groups[1].period_ms, 2000);

        // Device ids dense across the whole config.
        let dev_ids: Vec<u32> = rc
            .channels
            .iter()
            .flat_map(|c| c.devices.iter())
            .map(|d| d.id.0)
            .collect();
        assert_eq!(dev_ids, vec![0, 1, 2]);

        // Tag ids climb densely in declaration order.
        let tags: Vec<u32> = rc
            .channels
            .iter()
            .flat_map(|c| c.devices.iter())
            .flat_map(|d| d.registers.iter())
            .map(|r| r.tag.0)
            .collect();
        assert_eq!(tags, vec![0, 1, 2, 3]);
    }

    #[test]
    fn per_channel_tag_ranges_are_exclusive_and_contiguous() {
        let rc = resolve(two_channel_config(), vec![]);
        assert_eq!(rc.channels[0].tag_range, 0..3);
        assert_eq!(rc.channels[1].tag_range, 3..4);
        // Ranges tile the whole tag space with no overlap and no holes.
        assert_eq!(rc.channels[0].tag_range.end, rc.channels[1].tag_range.start);
        assert_eq!(rc.channels[1].tag_range.end as usize, rc.tag_count());
        // Every register's tag falls inside its channel's range.
        for ch in &rc.channels {
            for dev in &ch.devices {
                for reg in &dev.registers {
                    assert!(ch.tag_range.contains(&reg.tag.0));
                }
            }
        }
    }

    #[test]
    fn device_overrides_resolve_with_channel_fallback() {
        let rc = resolve(two_channel_config(), vec![]);
        let bus = &rc.channels[0];
        let (d1, d2) = (&bus.devices[0], &bus.devices[1]);

        // d1 overrides; d2 inherits channel values.
        assert_eq!(d1.request_timeout_ms, 250);
        assert_eq!(d2.request_timeout_ms, 1000);
        assert_eq!(d1.max_gap, 8);
        assert_eq!(d2.max_gap, 2);
        assert_eq!(d1.offline_after_failures, 10);
        assert_eq!(d2.offline_after_failures, 3);
        assert_eq!(d2.retry.max_retries, bus.retry.max_retries);
    }

    #[test]
    fn retry_resolves_device_then_channel_then_gateway() {
        use crate::schema::v1::RetryConfig;

        let mut c = two_channel_config();
        c.gateway.default_retry = RetryConfig {
            max_retries: 7,
            base_backoff_ms: 111,
            max_backoff_ms: 9_000,
        };
        // Channel 0 omits retry -> gateway default; its d1 overrides at the
        // device level; d2 inherits through the channel.
        c.channels[0].retry = None;
        c.channels[0].devices[0].retry = Some(RetryConfig {
            max_retries: 0,
            base_backoff_ms: 50,
            max_backoff_ms: 200,
        });
        // Channel 1 sets an explicit channel-level retry.
        c.channels[1].retry = Some(RetryConfig {
            max_retries: 4,
            base_backoff_ms: 300,
            max_backoff_ms: 3_000,
        });

        let rc = resolve(c, vec![]);
        let bus = &rc.channels[0];
        assert_eq!(bus.retry.max_retries, 7); // gateway default
        assert_eq!(bus.retry.base_backoff_ms, 111);
        assert_eq!(bus.devices[0].retry.max_retries, 0); // device override
        assert_eq!(bus.devices[1].retry.max_retries, 7); // inherits gateway via channel

        let plc = &rc.channels[1];
        assert_eq!(plc.retry.max_retries, 4); // channel override
        assert_eq!(plc.devices[0].retry.max_backoff_ms, 3_000);
    }

    #[test]
    fn max_inflight_forced_to_1_on_all_transports() {
        // B3: the runtime is sequential everywhere — resolve clamps RTU (would
        // be a validation error) AND TCP (validation warning) alike.
        let mut c = two_channel_config();
        c.channels[0].max_inflight = 8;
        let rc = resolve(c, vec![]);
        assert_eq!(rc.channels[0].max_inflight, 1); // RTU
        assert_eq!(rc.channels[1].max_inflight, 1); // TCP (asked for 4)
    }

    #[test]
    fn custom_request_resolves_to_bytes() {
        let mut c = two_channel_config();
        let mut e = entry("vendor.blob", FC::Custom { code: 0x41 }, 0, DT::U16);
        e.custom_response_len = Some(8);
        e.custom_request = Some("01 A0 ff".into());
        c.channels[1].devices[0].registers.push(e);
        let rc = resolve(c, vec![]);
        let regs = &rc.channels[1].devices[0].registers;
        assert_eq!(regs[1].custom_request, vec![0x01, 0xa0, 0xff]);
        assert!(regs[0].custom_request.is_empty(), "no payload -> empty vec");
    }

    #[test]
    fn poll_group_names_intern_to_ids() {
        let mut c = two_channel_config();
        c.channels[1].devices[0].registers[0].poll_group = "slow".into();
        let rc = resolve(c, vec![]);
        assert_eq!(rc.channels[1].devices[0].registers[0].poll_group, PollGroupId(1));
        assert_eq!(rc.channels[0].devices[0].registers[0].poll_group, PollGroupId(0));
    }
}
