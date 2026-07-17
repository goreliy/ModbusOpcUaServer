//! The shipped sample config must load, validate cleanly and resolve to a
//! dense, exclusively-partitioned tag space.

use gateway_config::schema::v1::TransportConfig;
use mb_types::{DataType, FunctionCode, PollGroupId, WordOrder};

static SAMPLE: &str = include_str!("../examples/sample-config.json");

#[test]
fn sample_config_loads_validates_and_resolves() {
    let rc = gateway_config::load_str(SAMPLE).expect("sample config must load");

    assert_eq!(rc.gateway.instance_name, "demo-gateway");
    assert!(rc.warnings.is_empty(), "sample must not warn: {:?}", rc.warnings);

    // Structure: rtu (2 devices) + tcp + rtu-over-tcp.
    assert_eq!(rc.channels.len(), 3);
    assert_eq!(rc.poll_groups.len(), 2);
    assert!(matches!(rc.channels[0].transport, TransportConfig::Rtu { baud: 19200, .. }));
    assert!(matches!(rc.channels[1].transport, TransportConfig::Tcp { .. }));
    assert!(matches!(rc.channels[2].transport, TransportConfig::RtuOverTcp { port: 4001, .. }));
    assert_eq!(rc.channels[0].devices.len(), 2, "two devices share the RTU bus");

    // Dense tag space, tiled by exclusive contiguous per-channel ranges.
    assert_eq!(rc.tag_count(), 11);
    let mut next = 0u32;
    for ch in &rc.channels {
        assert_eq!(ch.tag_range.start, next, "channel `{}` range contiguous", ch.name);
        let n: usize = ch.devices.iter().map(|d| d.registers.len()).sum();
        assert_eq!((ch.tag_range.end - ch.tag_range.start) as usize, n);
        next = ch.tag_range.end;
    }
    assert_eq!(next as usize, rc.tag_count());

    // max_inflight: forced to 1 for every transport (B3 — the runtime is
    // sequential; asking for more on TCP is a validation warning).
    assert_eq!(rc.channels[0].max_inflight, 1);
    assert_eq!(rc.channels[1].max_inflight, 1);
    assert_eq!(rc.channels[2].max_inflight, 1);

    // Spot-check resolved entries.
    let meter1 = &rc.channels[0].devices[0];
    let voltage = &meter1.registers[0];
    assert_eq!(rc.tag_name(voltage.tag), Some("meter1.voltage"));
    assert_eq!(voltage.data_type, DataType::F32);
    assert_eq!(voltage.word_order, WordOrder::LittleEndian);
    assert_eq!(voltage.scale, 0.1);
    assert_eq!(voltage.poll_group, PollGroupId(0)); // "fast"

    let serial = &meter1.registers[3];
    assert_eq!(serial.data_type, DataType::Ascii);
    assert_eq!(serial.length, Some(8));
    assert_eq!(serial.poll_group, PollGroupId(1)); // "slow"

    // Device override resolution on the shared bus.
    let meter2 = &rc.channels[0].devices[1];
    assert_eq!(meter1.request_timeout_ms, 500); // channel value
    assert_eq!(meter2.request_timeout_ms, 750); // device override
    assert_eq!(meter1.max_gap, 4); // channel value
    assert_eq!(meter2.max_gap, 0); // device override

    // Retry chain: device -> channel -> gateway default_retry.
    // rtu-bus-1 omits `retry`, so it (and its devices) inherit the gateway level.
    assert_eq!(rc.gateway.default_retry.max_retries, 1);
    assert_eq!(rc.channels[0].retry.max_retries, 1);
    assert_eq!(rc.channels[0].retry.base_backoff_ms, 250);
    assert_eq!(meter1.retry.max_backoff_ms, 10_000);
    // plc-tcp sets a channel-level retry, which wins over the gateway default.
    assert_eq!(rc.channels[1].retry.max_retries, 3);
    assert_eq!(rc.channels[1].devices[0].retry.max_retries, 3);

    // Custom read on the RTU-over-TCP stream carries its response length and
    // the parsed request payload bytes (B4).
    let vendor = &rc.channels[2].devices[0].registers[0];
    assert_eq!(vendor.function, FunctionCode::Custom { code: 65 });
    assert_eq!(vendor.custom_response_len, Some(16));
    assert_eq!(vendor.custom_request, vec![0x01, 0xa0]);

    // B2 groundwork: bind-all host + advertised_host for the endpoint URLs.
    assert_eq!(rc.opcua.host, "0.0.0.0");
    assert_eq!(rc.opcua.advertised_host.as_deref(), Some("192.168.1.5"));
}
