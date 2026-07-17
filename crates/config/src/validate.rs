//! Semantic validation — every rule of design §7, aggregated into a
//! `Vec<ConfigError>` (never first-fail). `BusOverload` is a WARNING variant:
//! it is reported but does not fail the load.
//!
//! Validation is plain Rust code (the JSON-Schema artifact is deferred).

use std::collections::{HashMap, HashSet};

use mb_types::{Area, DataType, FunctionCode};

use crate::schema::v1::{ChannelConfig, ConfigV1, DeviceConfig, RegisterEntry, TransportConfig};
use crate::ConfigError;

/// Cap on `length` for variable-width register types (ascii/bcd), in registers.
pub const MAX_LENGTH_REGISTERS: u16 = 125;
/// Cap on `custom_response_len`, in bytes (§6 ASCII/Raw bound).
pub const MAX_CUSTOM_RESPONSE_BYTES: u16 = 250;
/// Cap on the parsed `custom_request` payload, in bytes (mirrors the Raw
/// response bound: one PDU minus function code).
pub const MAX_CUSTOM_REQUEST_BYTES: usize = 250;

/// Parse a `custom_request` hex byte string (`"01 a0 ff"`; whitespace
/// optional and ignored) into the raw request payload bytes. Public so
/// `resolve` and the GUI reuse the exact same rules.
pub fn parse_custom_request(text: &str) -> Result<Vec<u8>, String> {
    let hex: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    if hex.is_empty() {
        return Err("no hex digits — omit the field for an empty payload".into());
    }
    if let Some(bad) = hex.chars().find(|c| !c.is_ascii_hexdigit()) {
        return Err(format!("`{bad}` is not a hex digit"));
    }
    if !hex.len().is_multiple_of(2) {
        return Err(format!(
            "odd number of hex digits ({}) — bytes are two digits each",
            hex.len()
        ));
    }
    if hex.len() / 2 > MAX_CUSTOM_REQUEST_BYTES {
        return Err(format!(
            "{} bytes exceeds the cap of {MAX_CUSTOM_REQUEST_BYTES}",
            hex.len() / 2
        ));
    }
    Ok((0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("checked: hex digits"))
        .collect())
}

/// Standard Modbus function codes the gateway handles via dedicated request
/// types; a `Custom` code colliding with one of these sends a malformed frame.
const STANDARD_FUNCTION_CODES: [u8; 8] = [1, 2, 3, 4, 5, 6, 15, 16];

/// Fixed conservative per-transaction turnaround margin for the bus-load
/// estimate (design §7 rule 10's `request_timeout_margin`): stands in for the
/// slave's response latency plus the t3.5 inter-frame silences, which the pure
/// airtime math otherwise models as zero. Real slaves commonly take 1–50 ms to
/// answer; 5 ms errs conservative without drowning the airtime term.
const TURNAROUND_MARGIN_MS: f64 = 5.0;

/// Run every rule; return ALL problems (errors + warnings) found.
pub fn validate(cfg: &ConfigV1) -> Vec<ConfigError> {
    let mut errs = Vec::new();

    validate_opcua(&cfg.opcua, &mut errs);

    // Rule 1: unique channel / device / poll_group ids.
    check_unique("channel", cfg.channels.iter().map(|c| c.id.as_str()), &mut errs);
    check_unique(
        "poll_group",
        cfg.poll_groups.iter().map(|g| g.id.as_str()),
        &mut errs,
    );
    check_unique(
        "device",
        cfg.channels
            .iter()
            .flat_map(|c| c.devices.iter())
            .map(|d| d.id.as_str()),
        &mut errs,
    );

    // Rules 1+2: every tag fed by exactly one RegisterEntry — a tag mapped from
    // two entries (esp. on two channels) would break single-writer-per-slot.
    check_unique(
        "tag",
        cfg.channels
            .iter()
            .flat_map(|c| c.devices.iter())
            .flat_map(|d| d.registers.iter())
            .map(|r| r.tag.as_str()),
        &mut errs,
    );

    // Poll periods must be schedulable: PollWheel would only clamp 0 to 1 ms,
    // i.e. a silent 1000-polls-per-second schedule.
    for g in &cfg.poll_groups {
        if g.period_ms == 0 {
            errs.push(ConfigError::BadPollPeriod { group: g.id.clone() });
        }
    }

    let periods: HashMap<&str, u64> = cfg
        .poll_groups
        .iter()
        .map(|g| (g.id.as_str(), g.period_ms))
        .collect();

    for ch in &cfg.channels {
        validate_transport(ch, &mut errs);

        // Rule 8: max_inflight > 1 is only *schematically* allowed on Tcp —
        // and even there the runtime is sequential for now, so resolve forces
        // 1 and the Tcp case is a WARNING (B3): the field stays in the schema
        // for the future parallel-TCP upgrade.
        if ch.max_inflight > 1 {
            if matches!(ch.transport, TransportConfig::Tcp { .. }) {
                errs.push(ConfigError::MaxInflightUnsupported {
                    channel: ch.id.clone(),
                    value: ch.max_inflight,
                });
            } else {
                errs.push(ConfigError::MaxInflightNotTcp {
                    channel: ch.id.clone(),
                    value: ch.max_inflight,
                });
            }
        }

        let is_stream = matches!(
            ch.transport,
            TransportConfig::Rtu { .. } | TransportConfig::RtuOverTcp { .. }
        );

        for dev in &ch.devices {
            for entry in &dev.registers {
                validate_entry(ch, entry, is_stream, &periods, &mut errs);
            }
        }

        // Rule 10: static bus-load estimate — WARNING, only meaningful where a
        // baud rate exists (serial RTU). A disabled channel is never polled
        // (plan compilation skips it), so its bus cannot overload.
        if ch.enabled {
            if let TransportConfig::Rtu { baud, .. } = ch.transport {
                if baud > 0 {
                    estimate_bus_load(ch, baud, &periods, &mut errs);
                }
            }
        }
    }

    errs
}

fn check_unique<'a>(
    kind: &'static str,
    ids: impl Iterator<Item = &'a str>,
    errs: &mut Vec<ConfigError>,
) {
    let mut seen = HashSet::new();
    let mut reported = HashSet::new();
    for id in ids {
        if !seen.insert(id) && reported.insert(id) {
            errs.push(ConfigError::DuplicateId {
                kind,
                id: id.to_string(),
            });
        }
    }
}

/// Rule 7: transport fields present and sane.
fn validate_transport(ch: &ChannelConfig, errs: &mut Vec<ConfigError>) {
    let mut bad = |reason: String| {
        errs.push(ConfigError::BadTransport {
            channel: ch.id.clone(),
            reason,
        })
    };
    match &ch.transport {
        TransportConfig::Tcp { host, port, .. } | TransportConfig::RtuOverTcp { host, port, .. } => {
            if host.trim().is_empty() {
                bad("host must not be empty".into());
            }
            if *port == 0 {
                bad("port must not be 0".into());
            }
        }
        TransportConfig::Rtu {
            path,
            baud,
            data_bits,
            stop_bits,
            ..
        } => {
            if path.trim().is_empty() {
                bad("serial path must not be empty".into());
            }
            if *baud == 0 {
                bad("baud must not be 0".into());
            }
            if !(5..=8).contains(data_bits) {
                bad(format!("data_bits {data_bits} outside 5..=8"));
            }
            if !(1..=2).contains(stop_bits) {
                bad(format!("stop_bits {stop_bits} outside 1..=2"));
            }
        }
    }
}

/// Cap for `retain_last` (phase-2 history rings are rewritten whole per
/// update, so keep them short by contract).
pub const MAX_RETAIN_LAST: u16 = 1000;

fn validate_opcua(opcua: &crate::schema::v1::OpcUaConfig, errs: &mut Vec<ConfigError>) {
    if !opcua.enabled {
        return;
    }
    fn bad(errs: &mut Vec<ConfigError>, reason: impl Into<String>) {
        errs.push(ConfigError::BadOpcUa { reason: reason.into() });
    }
    if opcua.port == 0 {
        bad(errs, "port must be non-zero");
    }
    if opcua.host.trim().is_empty() {
        bad(errs, "host must not be empty");
    }
    // B2 (groundwork): a bind-all host without advertised_host puts
    // 0.0.0.0/:: into the endpoint URLs clients receive from discovery.
    if opcua.advertised_host.is_none() && matches!(opcua.host.trim(), "0.0.0.0" | "::" | "[::]") {
        errs.push(ConfigError::OpcUaNoAdvertisedHost { host: opcua.host.clone() });
    }
    if !opcua.allow_none_security && !opcua.basic256sha256 {
        bad(errs, "all security policies disabled: enable allow_none_security or basic256sha256");
    }
    if !opcua.allow_anonymous && opcua.users.is_empty() {
        bad(errs, "allow_anonymous is false but no users are configured — nobody could connect");
    }
    let mut seen = std::collections::HashSet::new();
    for u in &opcua.users {
        if u.username.trim().is_empty() {
            bad(errs, "user with an empty username");
        } else if !seen.insert(u.username.as_str()) {
            bad(errs, format!("duplicate user `{}`", u.username));
        }
        match (&u.password, &u.password_hash) {
            (Some(_), Some(_)) => bad(
                errs,
                format!("user `{}`: set either password or password_hash, not both", u.username),
            ),
            (None, None) => bad(
                errs,
                format!("user `{}`: one of password / password_hash is required", u.username),
            ),
            (Some(_), None) => errs.push(ConfigError::WeakOpcUaSecurity {
                reason: format!(
                    "user `{}` has a plain-text password — generate a hash with \
                     `opc-modbus-server hash-password` and use password_hash",
                    u.username
                ),
            }),
            (None, Some(h)) if !h.starts_with("$argon2") => bad(
                errs,
                format!("user `{}`: password_hash is not an argon2 PHC string", u.username),
            ),
            (None, Some(_)) => {}
        }
    }
    if opcua.trust_any_client_cert {
        errs.push(ConfigError::WeakOpcUaSecurity {
            reason: "trust_any_client_cert=true accepts ANY client certificate — \
                     commissioning only, disable for production"
                .into(),
        });
    }
}

fn validate_entry(
    ch: &ChannelConfig,
    entry: &RegisterEntry,
    is_stream: bool,
    periods: &HashMap<&str, u64>,
    errs: &mut Vec<ConfigError>,
) {
    // Rule 3: poll_group FK resolves.
    if !periods.contains_key(entry.poll_group.as_str()) {
        errs.push(ConfigError::UnknownPollGroup {
            tag: entry.tag.clone(),
            group: entry.poll_group.clone(),
        });
    }

    // Phase-2 (tags-core) metadata rules.
    if let Some(db) = entry.deadband {
        if !db.is_finite() || db < 0.0 {
            errs.push(ConfigError::BadDeadband {
                tag: entry.tag.clone(),
            });
        }
    }
    if let Some(n) = entry.retain_last {
        if n > MAX_RETAIN_LAST {
            errs.push(ConfigError::RetainLastTooBig {
                tag: entry.tag.clone(),
                value: n,
                max: MAX_RETAIN_LAST,
            });
        }
    }
    // WARNING: formula shadows a customized scale/offset.
    if entry.formula.is_some() && (entry.scale != 1.0 || entry.offset != 0.0) {
        errs.push(ConfigError::ScaleShadowedByFormula {
            tag: entry.tag.clone(),
        });
    }

    // Phase-4 write-back rules.
    if entry.writable {
        validate_writable(entry, errs);
    } else if entry.write_formula.is_some() {
        errs.push(ConfigError::NotWritable {
            tag: entry.tag.clone(),
            reason: "write_formula is set but writable is false".into(),
        });
    }

    // B4: custom_request only makes sense on a Custom function.
    if entry.custom_request.is_some() && !matches!(entry.function, FunctionCode::Custom { .. }) {
        errs.push(ConfigError::BadCustomRequest {
            tag: entry.tag.clone(),
            reason: format!(
                "only valid on a custom function, not {:?}",
                entry.function
            ),
        });
    }

    match entry.function {
        FunctionCode::Custom { code } => {
            // Rule 11: the code must be a valid Modbus request function code.
            // 0 is not a function code; 0x80+ is the exception-response marker,
            // so such a request can never be answered successfully on the wire.
            if code == 0 || code >= 0x80 {
                errs.push(ConfigError::BadCustomFunctionCode {
                    tag: entry.tag.clone(),
                    code,
                });
            } else if STANDARD_FUNCTION_CODES.contains(&code) {
                // WARNING: framed exactly like the standard function but with
                // a payload most slaves will reject as malformed.
                errs.push(ConfigError::CustomCodeCollision {
                    tag: entry.tag.clone(),
                    code,
                });
            }
            // HARD ERROR (finding #17, B6): on serial RTU the response framing
            // for unknown function codes is timing-dependent inside
            // tokio-modbus (custom_response_len is only checked post-hoc, it
            // does not delimit the stream) — a desync poisons the whole bus.
            // RtuOverTcp stays accepted silently — single-segment replies
            // frame reliably in practice.
            if matches!(ch.transport, TransportConfig::Rtu { .. }) {
                errs.push(ConfigError::CustomReadOnSerialRtu {
                    tag: entry.tag.clone(),
                    channel: ch.id.clone(),
                });
            }
            // B4: the optional request payload must be well-formed hex.
            if let Some(text) = &entry.custom_request {
                if let Err(reason) = parse_custom_request(text) {
                    errs.push(ConfigError::BadCustomRequest {
                        tag: entry.tag.clone(),
                        reason,
                    });
                }
            }
            // Rule 6: Custom reads on stream transports require custom_response_len.
            match entry.custom_response_len {
                None if is_stream => errs.push(ConfigError::MissingCustomResponseLen {
                    tag: entry.tag.clone(),
                    channel: ch.id.clone(),
                }),
                // Rule 9: within the Raw cap.
                Some(len) if len == 0 || len > MAX_CUSTOM_RESPONSE_BYTES => {
                    errs.push(ConfigError::BadCustomResponseLen {
                        tag: entry.tag.clone(),
                        len,
                        max: MAX_CUSTOM_RESPONSE_BYTES,
                    })
                }
                _ => {}
            }
        }
        f if f.is_read() => {
            let area = f.read_area().expect("read FC has an area");
            validate_read_entry(entry, area, errs);
        }
        f => {
            // Write FCs never appear as polled register entries.
            errs.push(ConfigError::NonReadFunction {
                tag: entry.tag.clone(),
                function: f,
            });
        }
    }
}

/// Phase-4: what can actually be written back to the device.
fn validate_writable(entry: &RegisterEntry, errs: &mut Vec<ConfigError>) {
    let mut not_writable = |reason: &str| {
        errs.push(ConfigError::NotWritable {
            tag: entry.tag.clone(),
            reason: reason.to_string(),
        });
    };
    match entry.function {
        FunctionCode::ReadCoils => {
            // FC05/FC15 write-back. A coil source may only use a coil write FC.
            match entry.write_function {
                None | Some(FunctionCode::WriteSingleCoil)
                | Some(FunctionCode::WriteMultipleCoils) => {}
                Some(FunctionCode::WriteSingleRegister)
                | Some(FunctionCode::WriteMultipleRegisters) => not_writable(
                    "write_function is a register write, but the source is a coil — \
                     use write_single_coil (FC05) or write_multiple_coils (FC15)",
                ),
                Some(other) => not_writable(&format!(
                    "write_function must be a write FC (write_single_coil / \
                     write_multiple_coils), not {other:?}"
                )),
            }
        }
        FunctionCode::ReadHoldingRegisters => {
            // FC06/FC16 write-back; encodable fixed-width numerics only.
            match entry.data_type {
                DataType::Bit | DataType::Bitfield => not_writable(
                    "bit-in-register writes need read-modify-write/FC22 — not supported yet",
                ),
                DataType::Ascii | DataType::Bcd => {
                    not_writable("ascii/bcd write-back is not supported")
                }
                _ => {}
            }
            if entry.formula.is_some() && entry.write_formula.is_none() {
                not_writable(
                    "read `formula` cannot be auto-inverted — set `write_formula` (variable `value`)",
                );
            }
            if entry.write_formula.is_none() && entry.scale == 0.0 {
                not_writable("scale is 0, the linear transform cannot be inverted");
            }
            // A holding source may only use a register write FC. FC06 addresses
            // exactly one register, so multi-word types must use FC16.
            match entry.write_function {
                None | Some(FunctionCode::WriteMultipleRegisters) => {}
                Some(FunctionCode::WriteSingleRegister) => {
                    let words = entry.data_type.register_count().unwrap_or(1);
                    if words != 1 {
                        not_writable(
                            "write_single_register (FC06) writes one register, but the \
                             data type spans several — use write_multiple_registers (FC16)",
                        );
                    }
                }
                Some(FunctionCode::WriteSingleCoil)
                | Some(FunctionCode::WriteMultipleCoils) => not_writable(
                    "write_function is a coil write, but the source is a holding register — \
                     use write_single_register (FC06) or write_multiple_registers (FC16)",
                ),
                Some(other) => not_writable(&format!(
                    "write_function must be a write FC (write_single_register / \
                     write_multiple_registers), not {other:?}"
                )),
            }
        }
        FunctionCode::ReadDiscreteInputs | FunctionCode::ReadInputRegisters => {
            not_writable("discrete inputs / input registers are read-only by Modbus definition");
        }
        _ => not_writable("only holding-register and coil sources can be written"),
    }
}

/// Rules 4 + 5 + 9 for standard read functions.
fn validate_read_entry(entry: &RegisterEntry, area: Area, errs: &mut Vec<ConfigError>) {
    if area.is_bit_domain() {
        // Rule 4: bit-domain types only on FC01/02.
        if entry.data_type != DataType::Bit {
            errs.push(ConfigError::IncompatibleDataType {
                tag: entry.tag.clone(),
                function: entry.function,
                data_type: entry.data_type,
            });
        }
        return; // width is 1 coil; caps trivially hold
    }

    // Word domain (FC03/04): every DataType is representable, but Bit needs a
    // bit index to select within the register.
    if entry.data_type == DataType::Bit && entry.bit.is_none() {
        errs.push(ConfigError::MissingBitIndex {
            tag: entry.tag.clone(),
        });
    }
    if let Some(bit) = entry.bit {
        if bit >= 16 {
            errs.push(ConfigError::BadBitIndex {
                tag: entry.tag.clone(),
                bit,
            });
        }
    }

    // Span in registers: fixed width or explicit `length` for ascii/bcd.
    let span = match entry.data_type.register_count() {
        Some(n) => n,
        None => match entry.length {
            Some(l) => l,
            None => {
                errs.push(ConfigError::MissingLength {
                    tag: entry.tag.clone(),
                    data_type: entry.data_type,
                });
                return;
            }
        },
    };

    if span == 0 {
        errs.push(ConfigError::ZeroLength {
            tag: entry.tag.clone(),
        });
        return;
    }
    // Rules 5 + 9: within the Modbus area cap / ascii cap (identical for registers).
    let cap = area.max_qty().min(MAX_LENGTH_REGISTERS);
    if span > cap {
        errs.push(ConfigError::QtyExceedsCap {
            tag: entry.tag.clone(),
            qty: span,
            max: cap,
        });
    }
    // Address arithmetic must stay inside the 16-bit space.
    if entry.address as u32 + span as u32 > 0x1_0000 {
        errs.push(ConfigError::AddressOverflow {
            tag: entry.tag.clone(),
            address: entry.address,
            qty: span,
        });
    }
}

// ---------------------------------------------------------------------------
// Rule 10: static bus-load estimate for serial RTU channels (WARNING).
// ---------------------------------------------------------------------------

/// Entry span in the area's own units; None = not estimable (invalid entry,
/// already reported by the rules above).
fn entry_span(entry: &RegisterEntry, area: Area) -> Option<u32> {
    if area.is_bit_domain() {
        Some(1)
    } else {
        entry
            .data_type
            .register_count()
            .or(entry.length)
            .map(u32::from)
    }
}

/// Mirror of the runtime coalescer, just enough to count wire transactions:
/// greedy merge of sorted [start, end) intervals bridging holes <= max_gap and
/// capped at the area's max quantity. Returns merged (start, end) runs.
fn merge_runs(mut ivals: Vec<(u32, u32)>, max_gap: u32, area_max: u32) -> Vec<(u32, u32)> {
    ivals.sort_unstable();
    let mut out: Vec<(u32, u32)> = Vec::new();
    for (s, e) in ivals {
        match out.last_mut() {
            Some((rs, re)) if s.saturating_sub(*re) <= max_gap && e.saturating_sub(*rs) <= area_max => {
                *re = (*re).max(e);
            }
            _ => out.push((s, e)),
        }
    }
    out
}

/// Milliseconds of bus airtime for one transaction: 8-byte request +
/// (5 + payload)-byte response, 11 bit-times per char, plus the configured
/// inter-request silent gap and the fixed slave-turnaround margin.
fn txn_ms(payload_bytes: u32, baud: u32, inter_request_delay_ms: u64) -> f64 {
    let chars = 8 + 5 + payload_bytes;
    (chars * 11) as f64 * 1000.0 / baud as f64
        + inter_request_delay_ms as f64
        + TURNAROUND_MARGIN_MS
}

/// Poll-group name + area bucket -> [start, end) intervals awaiting merge.
type AreaBuckets<'a> = HashMap<(&'a str, u8), (Area, Vec<(u32, u32)>)>;

fn estimate_bus_load(
    ch: &ChannelConfig,
    baud: u32,
    periods: &HashMap<&str, u64>,
    errs: &mut Vec<ConfigError>,
) {
    // Airtime needed per poll-group cycle, summed over all devices on the bus.
    let mut per_group_ms: HashMap<&str, f64> = HashMap::new();

    for dev in ch.devices.iter().filter(|d| d.enabled) {
        let max_gap = u32::from(effective_max_gap(ch, dev));

        // Bucket read entries by (poll_group, area) — one PDU per (fn, run).
        let mut buckets: AreaBuckets = HashMap::new();
        for entry in &dev.registers {
            if !periods.contains_key(entry.poll_group.as_str()) {
                continue; // broken FK already reported
            }
            match entry.function.read_area() {
                Some(area) => {
                    let Some(span) = entry_span(entry, area) else { continue };
                    let start = u32::from(entry.address);
                    buckets
                        .entry((entry.poll_group.as_str(), area_key(area)))
                        .or_insert_with(|| (area, Vec::new()))
                        .1
                        .push((start, start + span));
                }
                None if matches!(entry.function, FunctionCode::Custom { .. }) => {
                    // Custom never coalesces: one txn per entry per cycle.
                    let payload = u32::from(entry.custom_response_len.unwrap_or(0));
                    *per_group_ms.entry(entry.poll_group.as_str()).or_default() +=
                        txn_ms(payload, baud, ch.inter_request_delay_ms);
                }
                None => {} // write FCs: already rejected, not polled
            }
        }

        for ((group, _), (area, ivals)) in buckets {
            for (s, e) in merge_runs(ivals, max_gap, u32::from(area.max_qty())) {
                let qty = e - s;
                let payload = if area.is_bit_domain() { qty.div_ceil(8) } else { qty * 2 };
                *per_group_ms.entry(group).or_default() +=
                    txn_ms(payload, baud, ch.inter_request_delay_ms);
            }
        }
    }

    // Utilization = sum over groups of (cycle airtime / period).
    let utilization: f64 = per_group_ms
        .iter()
        .filter_map(|(group, ms)| periods.get(group).map(|p| ms / *p.max(&1) as f64))
        .sum();

    if utilization > 1.0 {
        errs.push(ConfigError::BusOverload {
            channel: ch.id.clone(),
            utilization_permille: (utilization * 1000.0).round() as u32,
        });
    }
}

fn effective_max_gap(ch: &ChannelConfig, dev: &DeviceConfig) -> u16 {
    dev.max_gap.unwrap_or(ch.max_gap)
}

fn area_key(area: Area) -> u8 {
    match area {
        Area::Coils => 0,
        Area::DiscreteInputs => 1,
        Area::Holding => 2,
        Area::Input => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::*;
    use mb_types::{DataType as DT, FunctionCode as FC};

    fn assert_single<F: Fn(&ConfigError) -> bool>(errs: &[ConfigError], pred: F, what: &str) {
        assert_eq!(errs.len(), 1, "{what}: expected exactly one error, got {errs:?}");
        assert!(pred(&errs[0]), "{what}: wrong error: {:?}", errs[0]);
    }

    #[test]
    fn valid_config_has_no_errors() {
        let cfg = cfg(
            vec![pg("fast", 200)],
            vec![tcp_channel(
                "ch1",
                vec![device(
                    "d1",
                    1,
                    vec![entry("t1", FC::ReadHoldingRegisters, 0, DT::U16)],
                )],
            )],
        );
        assert!(validate(&cfg).is_empty());
    }

    #[test]
    fn duplicate_channel_id() {
        let cfg = cfg(
            vec![pg("fast", 200)],
            vec![
                tcp_channel("ch1", vec![device("d1", 1, vec![])]),
                tcp_channel("ch1", vec![device("d2", 2, vec![])]),
            ],
        );
        assert_single(
            &validate(&cfg),
            |e| matches!(e, ConfigError::DuplicateId { kind: "channel", id } if id == "ch1"),
            "dup channel",
        );
    }

    #[test]
    fn duplicate_device_id_across_channels() {
        let cfg = cfg(
            vec![pg("fast", 200)],
            vec![
                tcp_channel("ch1", vec![device("d1", 1, vec![])]),
                tcp_channel("ch2", vec![device("d1", 2, vec![])]),
            ],
        );
        assert_single(
            &validate(&cfg),
            |e| matches!(e, ConfigError::DuplicateId { kind: "device", id } if id == "d1"),
            "dup device",
        );
    }

    #[test]
    fn duplicate_poll_group_id() {
        let cfg = cfg(vec![pg("fast", 200), pg("fast", 500)], vec![]);
        assert_single(
            &validate(&cfg),
            |e| matches!(e, ConfigError::DuplicateId { kind: "poll_group", id } if id == "fast"),
            "dup group",
        );
    }

    #[test]
    fn tag_fed_by_two_entries_is_rejected() {
        // Same tag from two entries on two different channels: data race on the slot.
        let cfg = cfg(
            vec![pg("fast", 200)],
            vec![
                tcp_channel(
                    "ch1",
                    vec![device("d1", 1, vec![entry("t1", FC::ReadHoldingRegisters, 0, DT::U16)])],
                ),
                tcp_channel(
                    "ch2",
                    vec![device("d2", 1, vec![entry("t1", FC::ReadInputRegisters, 5, DT::U16)])],
                ),
            ],
        );
        assert_single(
            &validate(&cfg),
            |e| matches!(e, ConfigError::DuplicateId { kind: "tag", id } if id == "t1"),
            "dup tag",
        );
    }

    #[test]
    fn unknown_poll_group_fk() {
        let mut e = entry("t1", FC::ReadHoldingRegisters, 0, DT::U16);
        e.poll_group = "nope".into();
        let cfg = cfg(vec![pg("fast", 200)], vec![tcp_channel("ch1", vec![device("d1", 1, vec![e])])]);
        assert_single(
            &validate(&cfg),
            |e| matches!(e, ConfigError::UnknownPollGroup { tag, group } if tag == "t1" && group == "nope"),
            "bad fk",
        );
    }

    #[test]
    fn word_type_on_bit_function_is_incompatible() {
        let cfg = cfg(
            vec![pg("fast", 200)],
            vec![tcp_channel(
                "ch1",
                vec![device("d1", 1, vec![entry("t1", FC::ReadCoils, 0, DT::U16)])],
            )],
        );
        assert_single(
            &validate(&cfg),
            |e| matches!(e, ConfigError::IncompatibleDataType { tag, .. } if tag == "t1"),
            "u16 on coils",
        );
    }

    #[test]
    fn bit_on_register_read_requires_bit_index() {
        let missing = cfg(
            vec![pg("fast", 200)],
            vec![tcp_channel(
                "ch1",
                vec![device("d1", 1, vec![entry("t1", FC::ReadHoldingRegisters, 0, DT::Bit)])],
            )],
        );
        assert_single(
            &validate(&missing),
            |e| matches!(e, ConfigError::MissingBitIndex { tag } if tag == "t1"),
            "bit w/o index",
        );

        // With a valid bit index it passes; with bit >= 16 it fails.
        let mut ok = entry("t1", FC::ReadHoldingRegisters, 0, DT::Bit);
        ok.bit = Some(15);
        let good = cfg(vec![pg("fast", 200)], vec![tcp_channel("ch1", vec![device("d1", 1, vec![ok])])]);
        assert!(validate(&good).is_empty());

        let mut bad = entry("t1", FC::ReadHoldingRegisters, 0, DT::Bit);
        bad.bit = Some(16);
        let broken = cfg(vec![pg("fast", 200)], vec![tcp_channel("ch1", vec![device("d1", 1, vec![bad])])]);
        assert_single(
            &validate(&broken),
            |e| matches!(e, ConfigError::BadBitIndex { bit: 16, .. }),
            "bit index 16",
        );
    }

    #[test]
    fn bit_on_coils_is_fine_without_index() {
        let cfg = cfg(
            vec![pg("fast", 200)],
            vec![tcp_channel(
                "ch1",
                vec![device("d1", 1, vec![entry("t1", FC::ReadDiscreteInputs, 7, DT::Bit)])],
            )],
        );
        assert!(validate(&cfg).is_empty());
    }

    #[test]
    fn write_function_in_register_entry_is_rejected() {
        let cfg = cfg(
            vec![pg("fast", 200)],
            vec![tcp_channel(
                "ch1",
                vec![device("d1", 1, vec![entry("t1", FC::WriteSingleRegister, 0, DT::U16)])],
            )],
        );
        assert_single(
            &validate(&cfg),
            |e| matches!(e, ConfigError::NonReadFunction { tag, .. } if tag == "t1"),
            "write fc",
        );
    }

    #[test]
    fn ascii_without_length_is_rejected() {
        let cfg = cfg(
            vec![pg("fast", 200)],
            vec![tcp_channel(
                "ch1",
                vec![device("d1", 1, vec![entry("t1", FC::ReadHoldingRegisters, 0, DT::Ascii)])],
            )],
        );
        assert_single(
            &validate(&cfg),
            |e| matches!(e, ConfigError::MissingLength { tag, .. } if tag == "t1"),
            "ascii w/o length",
        );
    }

    #[test]
    fn ascii_length_over_cap_is_rejected() {
        let mut e = entry("t1", FC::ReadHoldingRegisters, 0, DT::Ascii);
        e.length = Some(200);
        let cfg = cfg(vec![pg("fast", 200)], vec![tcp_channel("ch1", vec![device("d1", 1, vec![e])])]);
        assert_single(
            &validate(&cfg),
            |e| matches!(e, ConfigError::QtyExceedsCap { qty: 200, max: 125, .. }),
            "ascii cap",
        );
    }

    #[test]
    fn zero_length_is_rejected() {
        let mut e = entry("t1", FC::ReadHoldingRegisters, 0, DT::Bcd);
        e.length = Some(0);
        let cfg = cfg(vec![pg("fast", 200)], vec![tcp_channel("ch1", vec![device("d1", 1, vec![e])])]);
        assert_single(
            &validate(&cfg),
            |e| matches!(e, ConfigError::ZeroLength { tag } if tag == "t1"),
            "zero length",
        );
    }

    #[test]
    fn address_overflow_is_rejected() {
        let cfg = cfg(
            vec![pg("fast", 200)],
            vec![tcp_channel(
                "ch1",
                vec![device("d1", 1, vec![entry("t1", FC::ReadHoldingRegisters, 65533, DT::U64)])],
            )],
        );
        assert_single(
            &validate(&cfg),
            |e| matches!(e, ConfigError::AddressOverflow { address: 65533, qty: 4, .. }),
            "addr overflow",
        );
    }

    #[test]
    fn custom_read_on_stream_requires_response_len() {
        let e = entry_custom("t1", 0x41, None);
        let on_rtu = cfg(
            vec![pg("fast", 200)],
            vec![rtu_channel("bus1", 9600, vec![device("d1", 1, vec![e])])],
        );
        // Serial RTU also always gets the framing HARD ERROR (B6).
        let errs = validate(&on_rtu);
        assert_eq!(errs.len(), 2, "missing-len error + serial-rtu error: {errs:?}");
        assert!(errs.iter().any(
            |e| matches!(e, ConfigError::MissingCustomResponseLen { tag, channel } if tag == "t1" && channel == "bus1")
        ));
        assert!(errs.iter().any(
            |e| matches!(e, ConfigError::CustomReadOnSerialRtu { .. }) && !e.is_warning()
        ));

        // Same on RtuOverTcp.
        let e = entry_custom("t1", 0x41, None);
        let over_tcp = cfg(
            vec![pg("fast", 200)],
            vec![rtu_over_tcp_channel("gw1", vec![device("d1", 1, vec![e])])],
        );
        assert_single(
            &validate(&over_tcp),
            |e| matches!(e, ConfigError::MissingCustomResponseLen { .. }),
            "custom on rtu-over-tcp",
        );

        // On plain TCP (MBAP length-prefixed) it is not required.
        let e = entry_custom("t1", 0x41, None);
        let plain_tcp = cfg(
            vec![pg("fast", 200)],
            vec![tcp_channel("ch1", vec![device("d1", 1, vec![e])])],
        );
        assert!(validate(&plain_tcp).is_empty());
    }

    #[test]
    fn custom_response_len_over_cap_is_rejected() {
        // RtuOverTcp is a stream too, but has no serial-rtu error — this
        // isolates the cap rule.
        let e = entry_custom("t1", 0x41, Some(500));
        let cfg = cfg(
            vec![pg("fast", 5000)],
            vec![rtu_over_tcp_channel("gw1", vec![device("d1", 1, vec![e])])],
        );
        assert_single(
            &validate(&cfg),
            |e| matches!(e, ConfigError::BadCustomResponseLen { len: 500, max: 250, .. }),
            "custom cap",
        );
    }

    #[test]
    fn custom_read_on_serial_rtu_is_a_hard_error_but_rtu_over_tcp_passes() {
        // B6: even a fully valid Custom entry is REJECTED on serial RTU —
        // tokio-modbus cannot guarantee framing of unknown function codes on
        // a serial stream (finding #17).
        let e = entry_custom("t1", 0x41, Some(8));
        let on_rtu = cfg(
            vec![pg("fast", 200)],
            vec![rtu_channel("bus1", 9600, vec![device("d1", 1, vec![e])])],
        );
        let errs = validate(&on_rtu);
        assert_single(
            &errs,
            |e| matches!(e, ConfigError::CustomReadOnSerialRtu { tag, channel } if tag == "t1" && channel == "bus1"),
            "custom on serial rtu",
        );
        assert!(!errs[0].is_warning(), "must be a hard error (was a warning pre-B6)");

        // Same entry on RtuOverTcp: accepted silently (single-segment replies
        // frame reliably in practice).
        let e = entry_custom("t1", 0x41, Some(8));
        let over_tcp = cfg(
            vec![pg("fast", 200)],
            vec![rtu_over_tcp_channel("gw1", vec![device("d1", 1, vec![e])])],
        );
        assert!(validate(&over_tcp).is_empty());
    }

    #[test]
    fn custom_code_zero_or_exception_range_is_rejected() {
        for code in [0u8, 0x80, 131, 255] {
            let e = entry_custom("t1", code, Some(4));
            let cfg = cfg(
                vec![pg("fast", 200)],
                vec![tcp_channel("ch1", vec![device("d1", 1, vec![e])])],
            );
            assert_single(
                &validate(&cfg),
                |e| matches!(e, ConfigError::BadCustomFunctionCode { tag, code: c } if tag == "t1" && *c == code),
                &format!("custom code {code}"),
            );
        }
    }

    #[test]
    fn custom_code_colliding_with_standard_warns() {
        for code in [1u8, 2, 3, 4, 5, 6, 15, 16] {
            let e = entry_custom("t1", code, Some(4));
            let cfg = cfg(
                vec![pg("fast", 200)],
                vec![tcp_channel("ch1", vec![device("d1", 1, vec![e])])],
            );
            let errs = validate(&cfg);
            assert_single(
                &errs,
                |e| matches!(e, ConfigError::CustomCodeCollision { tag, code: c } if tag == "t1" && *c == code),
                &format!("collision {code}"),
            );
            assert!(errs[0].is_warning(), "collision must be a warning, not an error");
        }

        // A genuinely vendor-specific code passes without noise.
        let e = entry_custom("t1", 0x41, Some(4));
        let cfg = cfg(
            vec![pg("fast", 200)],
            vec![tcp_channel("ch1", vec![device("d1", 1, vec![e])])],
        );
        assert!(validate(&cfg).is_empty());
    }

    #[test]
    fn zero_poll_period_is_rejected() {
        let cfg = cfg(
            vec![pg("fast", 0)],
            vec![tcp_channel(
                "ch1",
                vec![device("d1", 1, vec![entry("t1", FC::ReadHoldingRegisters, 0, DT::U16)])],
            )],
        );
        assert_single(
            &validate(&cfg),
            |e| matches!(e, ConfigError::BadPollPeriod { group } if group == "fast"),
            "period 0",
        );
    }

    fn one_entry_cfg(e: RegisterEntry) -> crate::schema::v1::ConfigV1 {
        cfg(vec![pg("fast", 200)], vec![tcp_channel("ch1", vec![device("d1", 1, vec![e])])])
    }

    #[test]
    fn opcua_rules() {
        use crate::schema::v1::{OpcUaConfig, OpcUaUser};
        let base = cfg(
            vec![pg("fast", 200)],
            vec![tcp_channel(
                "ch1",
                vec![device("d1", 1, vec![entry("t1", FC::ReadHoldingRegisters, 0, DT::U16)])],
            )],
        );

        // Defaults are valid.
        assert!(validate(&base).is_empty());

        // No anonymous + no users = nobody can connect.
        let mut c = base.clone();
        c.opcua = OpcUaConfig { allow_anonymous: false, ..opcua() };
        assert_single(
            &validate(&c),
            |e| matches!(e, ConfigError::BadOpcUa { .. }),
            "anonymous off without users",
        );

        // Duplicate usernames (plaintext passwords also warn — filter those).
        let mut c = base.clone();
        c.opcua.users = vec![
            OpcUaUser { username: "op".into(), password: Some("1".into()), password_hash: None },
            OpcUaUser { username: "op".into(), password: Some("2".into()), password_hash: None },
        ];
        let errs = validate(&c);
        let hard: Vec<_> = errs.iter().filter(|e| !e.is_warning()).collect();
        assert_eq!(hard.len(), 1, "duplicate users: {errs:?}");
        assert!(matches!(hard[0], ConfigError::BadOpcUa { reason } if reason.contains("duplicate")));
        assert_eq!(
            errs.iter().filter(|e| e.is_warning()).count(),
            2,
            "each plaintext password warns"
        );

        // password XOR password_hash.
        let mut c = base.clone();
        c.opcua.users = vec![OpcUaUser {
            username: "both".into(),
            password: Some("x".into()),
            password_hash: Some("$argon2id$v=19$stub".into()),
        }];
        assert_single(
            &validate(&c),
            |e| matches!(e, ConfigError::BadOpcUa { reason } if reason.contains("not both")),
            "both set",
        );
        let mut c = base.clone();
        c.opcua.users = vec![OpcUaUser { username: "none".into(), password: None, password_hash: None }];
        assert_single(
            &validate(&c),
            |e| matches!(e, ConfigError::BadOpcUa { reason } if reason.contains("required")),
            "neither set",
        );

        // Non-argon2 hash rejected; argon2 hash passes silently.
        let mut c = base.clone();
        c.opcua.users = vec![OpcUaUser {
            username: "h".into(),
            password: None,
            password_hash: Some("plainmd5".into()),
        }];
        assert_single(
            &validate(&c),
            |e| matches!(e, ConfigError::BadOpcUa { reason } if reason.contains("argon2")),
            "bad hash format",
        );
        let mut c = base.clone();
        c.opcua.users = vec![OpcUaUser {
            username: "h".into(),
            password: None,
            password_hash: Some("$argon2id$v=19$m=19456,t=2,p=1$abc$def".into()),
        }];
        assert!(validate(&c).is_empty(), "argon2 hash user is clean");

        // trust_any_client_cert warns.
        let mut c = base.clone();
        c.opcua.trust_any_client_cert = true;
        let errs = validate(&c);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].is_warning(), "trust-any is a warning: {errs:?}");

        // Port 0 / all policies off.
        let mut c = base.clone();
        c.opcua.port = 0;
        c.opcua.allow_none_security = false;
        c.opcua.basic256sha256 = false;
        assert_eq!(validate(&c).len(), 2, "port + policies");

        // Disabled section skips all checks (incl. the advertised-host one:
        // OpcUaConfig::default() binds 0.0.0.0 without advertised_host).
        let mut c = base;
        c.opcua =
            OpcUaConfig { enabled: false, port: 0, allow_anonymous: false, ..OpcUaConfig::default() };
        assert!(validate(&c).is_empty());
    }

    #[test]
    fn bind_all_host_without_advertised_host_warns() {
        let base = cfg(vec![pg("fast", 200)], vec![]);

        // B2 groundwork: 0.0.0.0 / :: without advertised_host -> WARNING.
        for host in ["0.0.0.0", "::", "[::]"] {
            let mut c = base.clone();
            c.opcua.host = host.into();
            c.opcua.advertised_host = None;
            let errs = validate(&c);
            assert_single(
                &errs,
                |e| matches!(e, ConfigError::OpcUaNoAdvertisedHost { host: h } if h == host),
                &format!("bind-all `{host}` without advertised_host"),
            );
            assert!(errs[0].is_warning(), "must be a warning, not an error");
        }

        // Bind-all WITH advertised_host: silent.
        let mut c = base.clone();
        c.opcua.host = "0.0.0.0".into();
        c.opcua.advertised_host = Some("192.168.1.10".into());
        assert!(validate(&c).is_empty());

        // Routable host without advertised_host: silent (host is used).
        let mut c = base;
        c.opcua.host = "192.168.1.10".into();
        c.opcua.advertised_host = None;
        assert!(validate(&c).is_empty());
    }

    #[test]
    fn custom_request_parses_hex_edge_cases() {
        // Spaces optional, case-insensitive.
        assert_eq!(parse_custom_request("01 a0 ff").unwrap(), vec![0x01, 0xa0, 0xff]);
        assert_eq!(parse_custom_request("01A0FF").unwrap(), vec![0x01, 0xa0, 0xff]);
        assert_eq!(parse_custom_request("  00 ").unwrap(), vec![0x00]);
        assert_eq!(parse_custom_request("0 1a 0f f").unwrap(), vec![0x01, 0xa0, 0xff]);
        // Cap: 250 bytes ok, 251 rejected.
        assert_eq!(parse_custom_request(&"ab".repeat(250)).unwrap().len(), 250);
        assert!(parse_custom_request(&"ab".repeat(251)).is_err());
        // Malformed: empty / whitespace-only / odd digits / non-hex.
        assert!(parse_custom_request("").is_err());
        assert!(parse_custom_request("   ").is_err());
        assert!(parse_custom_request("abc").is_err());
        assert!(parse_custom_request("0x01").is_err());
        assert!(parse_custom_request("01,02").is_err());
    }

    #[test]
    fn custom_request_rules() {
        // Well-formed payload on a Custom function: clean.
        let mut e = entry_custom("t1", 0x41, Some(8));
        e.custom_request = Some("01 a0 ff".into());
        assert!(validate(&one_entry_cfg(e)).is_empty());

        // Malformed hex: rejected.
        let mut e = entry_custom("t1", 0x41, Some(8));
        e.custom_request = Some("zz".into());
        let errs = validate(&one_entry_cfg(e));
        assert_single(
            &errs,
            |e| matches!(e, ConfigError::BadCustomRequest { tag, .. } if tag == "t1"),
            "bad hex",
        );
        assert!(!errs[0].is_warning());

        // Set on a non-Custom function: rejected.
        let mut e = entry("t1", FC::ReadHoldingRegisters, 0, DT::U16);
        e.custom_request = Some("01".into());
        assert_single(
            &validate(&one_entry_cfg(e)),
            |e| matches!(e, ConfigError::BadCustomRequest { tag, reason } if tag == "t1" && reason.contains("custom function")),
            "custom_request off Custom",
        );
    }

    #[test]
    fn writable_rules() {
        // Holding + plain numeric: OK.
        let mut e = entry("t", FC::ReadHoldingRegisters, 0, DT::U16);
        e.writable = true;
        assert!(validate(&one_entry_cfg(e)).is_empty());

        // Coil: OK.
        let mut e = entry("t", FC::ReadCoils, 0, DT::Bit);
        e.writable = true;
        assert!(validate(&one_entry_cfg(e)).is_empty());

        // Input register: read-only.
        let mut e = entry("t", FC::ReadInputRegisters, 0, DT::U16);
        e.writable = true;
        assert_single(
            &validate(&one_entry_cfg(e)),
            |e| matches!(e, ConfigError::NotWritable { .. }),
            "input registers read-only",
        );

        // Bit-in-register: not supported.
        let mut e = entry("t", FC::ReadHoldingRegisters, 0, DT::Bit);
        e.bit = Some(3);
        e.writable = true;
        assert_single(
            &validate(&one_entry_cfg(e)),
            |e| matches!(e, ConfigError::NotWritable { .. }),
            "bit-in-register",
        );

        // formula without write_formula: rejected.
        let mut e = entry("t", FC::ReadHoldingRegisters, 0, DT::U16);
        e.writable = true;
        e.formula = Some("raw * 60".into());
        assert_single(
            &validate(&one_entry_cfg(e)),
            |e| matches!(e, ConfigError::NotWritable { reason, .. } if reason.contains("write_formula")),
            "formula needs inverse",
        );

        // formula + write_formula: OK.
        let mut e = entry("t", FC::ReadHoldingRegisters, 0, DT::U16);
        e.writable = true;
        e.formula = Some("raw * 60".into());
        e.write_formula = Some("value / 60".into());
        assert!(validate(&one_entry_cfg(e)).is_empty());

        // scale 0 cannot invert.
        let mut e = entry("t", FC::ReadHoldingRegisters, 0, DT::U16);
        e.writable = true;
        e.scale = 0.0;
        let errs = validate(&one_entry_cfg(e));
        assert!(
            errs.iter().any(|e| matches!(e, ConfigError::NotWritable { reason, .. } if reason.contains("scale"))),
            "scale 0: {errs:?}"
        );

        // write_formula without writable: rejected.
        let mut e = entry("t", FC::ReadHoldingRegisters, 0, DT::U16);
        e.write_formula = Some("value".into());
        assert_single(
            &validate(&one_entry_cfg(e)),
            |e| matches!(e, ConfigError::NotWritable { .. }),
            "write_formula without writable",
        );
    }

    #[test]
    fn write_function_override_rules() {
        // Holding + FC16 forced: OK.
        let mut e = entry("t", FC::ReadHoldingRegisters, 0, DT::U16);
        e.writable = true;
        e.write_function = Some(FC::WriteMultipleRegisters);
        assert!(validate(&one_entry_cfg(e)).is_empty(), "holding + FC16 forced is valid");

        // Coil + FC15 forced: OK.
        let mut e = entry("t", FC::ReadCoils, 0, DT::Bit);
        e.writable = true;
        e.write_function = Some(FC::WriteMultipleCoils);
        assert!(validate(&one_entry_cfg(e)).is_empty(), "coil + FC15 forced is valid");

        // Coil source + register write FC: rejected.
        let mut e = entry("t", FC::ReadCoils, 0, DT::Bit);
        e.writable = true;
        e.write_function = Some(FC::WriteSingleRegister);
        assert_single(
            &validate(&one_entry_cfg(e)),
            |e| matches!(e, ConfigError::NotWritable { reason, .. } if reason.contains("register write")),
            "coil source cannot use a register write FC",
        );

        // Multi-word holding + FC06 (single register): rejected.
        let mut e = entry("t", FC::ReadHoldingRegisters, 0, DT::U32);
        e.writable = true;
        e.write_function = Some(FC::WriteSingleRegister);
        assert_single(
            &validate(&one_entry_cfg(e)),
            |e| matches!(e, ConfigError::NotWritable { reason, .. } if reason.contains("write_single_register")),
            "multi-word type cannot use FC06",
        );

        // Holding source + coil write FC: rejected.
        let mut e = entry("t", FC::ReadHoldingRegisters, 0, DT::U16);
        e.writable = true;
        e.write_function = Some(FC::WriteSingleCoil);
        assert_single(
            &validate(&one_entry_cfg(e)),
            |e| matches!(e, ConfigError::NotWritable { reason, .. } if reason.contains("coil write")),
            "holding source cannot use a coil write FC",
        );
    }

    #[test]
    fn bad_deadband_is_rejected() {
        for db in [-0.5, f64::NAN, f64::INFINITY] {
            let mut e = entry("t1", FC::ReadHoldingRegisters, 0, DT::U16);
            e.deadband = Some(db);
            assert_single(
                &validate(&one_entry_cfg(e)),
                |e| matches!(e, ConfigError::BadDeadband { tag } if tag == "t1"),
                "deadband must be finite and >= 0",
            );
        }
        // 0.0 is legal (deadband off).
        let mut e = entry("t1", FC::ReadHoldingRegisters, 0, DT::U16);
        e.deadband = Some(0.0);
        assert!(validate(&one_entry_cfg(e)).is_empty());
    }

    #[test]
    fn retain_last_cap_is_enforced() {
        let mut e = entry("t1", FC::ReadHoldingRegisters, 0, DT::U16);
        e.retain_last = Some(MAX_RETAIN_LAST + 1);
        assert_single(
            &validate(&one_entry_cfg(e)),
            |e| matches!(e, ConfigError::RetainLastTooBig { tag, .. } if tag == "t1"),
            "retain_last over the cap",
        );
    }

    #[test]
    fn formula_shadowing_scale_warns() {
        let mut e = entry("t1", FC::ReadHoldingRegisters, 0, DT::U16);
        e.formula = Some("raw * 0.1".into());
        e.scale = 0.1; // customized alongside formula -> warning
        let errs = validate(&one_entry_cfg(e));
        assert_single(
            &errs,
            |e| matches!(e, ConfigError::ScaleShadowedByFormula { tag } if tag == "t1"),
            "formula + customized scale",
        );
        assert!(errs[0].is_warning(), "must be a warning, not a hard error");

        // formula with DEFAULT scale/offset does not warn.
        let mut e = entry("t1", FC::ReadHoldingRegisters, 0, DT::U16);
        e.formula = Some("raw * 0.1".into());
        assert!(validate(&one_entry_cfg(e)).is_empty());
    }

    #[test]
    fn turnaround_margin_counts_toward_bus_load() {
        // 100 scattered single-register reads at 115200 baud in a 200 ms
        // period: pure airtime is ~1.4 ms/txn (~72% — passes), but the 5 ms
        // turnaround margin per transaction pushes it far over budget.
        let regs: Vec<_> = (0..100u16)
            .map(|i| entry(&format!("t{i}"), FC::ReadHoldingRegisters, i * 100, DT::U16))
            .collect();
        let mut ch = rtu_channel("bus1", 115_200, vec![device("d1", 1, regs)]);
        ch.inter_request_delay_ms = 0;
        let cfg = cfg(vec![pg("fast", 200)], vec![ch]);
        assert_single(
            &validate(&cfg),
            |e| matches!(e, ConfigError::BusOverload { channel, .. } if channel == "bus1"),
            "turnaround-dominated overload",
        );
    }

    #[test]
    fn disabled_channel_skips_bus_load_estimate() {
        // Same hopeless layout as bus_overload_emits_warning_not_error, but
        // the channel is disabled: it is never polled, so no warning.
        let regs: Vec<_> = (0..5u16)
            .map(|i| entry(&format!("t{i}"), FC::ReadHoldingRegisters, i * 1000, DT::U16))
            .collect();
        let mut ch = rtu_channel("bus1", 1200, vec![device("d1", 1, regs)]);
        ch.enabled = false;
        let cfg = cfg(vec![pg("fast", 50)], vec![ch]);
        assert!(validate(&cfg).is_empty());
    }

    #[test]
    fn transport_sanity_errors() {
        // Empty TCP host.
        let mut ch = tcp_channel("ch1", vec![]);
        ch.transport = crate::schema::v1::TransportConfig::Tcp {
            host: "".into(),
            port: 502,
            connect_timeout_ms: 5000,
        };
        assert_single(
            &validate(&cfg(vec![pg("fast", 200)], vec![ch])),
            |e| matches!(e, ConfigError::BadTransport { .. }),
            "empty host",
        );

        // Port 0 on RtuOverTcp.
        let mut ch = rtu_over_tcp_channel("gw1", vec![]);
        ch.transport = crate::schema::v1::TransportConfig::RtuOverTcp {
            host: "10.0.0.1".into(),
            port: 0,
            connect_timeout_ms: 5000,
        };
        assert_single(
            &validate(&cfg(vec![pg("fast", 200)], vec![ch])),
            |e| matches!(e, ConfigError::BadTransport { .. }),
            "port 0",
        );

        // Empty serial path, baud 0 and bad data_bits: three findings, aggregated.
        let mut ch = rtu_channel("bus1", 9600, vec![]);
        ch.transport = crate::schema::v1::TransportConfig::Rtu {
            path: "".into(),
            baud: 0,
            data_bits: 9,
            parity: crate::schema::v1::Parity::None,
            stop_bits: 1,
        };
        let errs = validate(&cfg(vec![pg("fast", 200)], vec![ch]));
        assert_eq!(errs.len(), 3, "aggregated, not first-fail: {errs:?}");
        assert!(errs.iter().all(|e| matches!(e, ConfigError::BadTransport { .. })));
    }

    #[test]
    fn max_inflight_gt_1_errors_off_tcp_and_warns_on_tcp() {
        let mut ch = rtu_channel("bus1", 9600, vec![]);
        ch.max_inflight = 4;
        let errs = validate(&cfg(vec![pg("fast", 200)], vec![ch]));
        assert_single(
            &errs,
            |e| matches!(e, ConfigError::MaxInflightNotTcp { value: 4, .. }),
            "inflight on rtu",
        );
        assert!(!errs[0].is_warning(), "non-TCP stays a hard error");

        // B3: even on TCP the runtime is sequential — accepted, but WARN that
        // the value is forced to 1.
        let mut ch = tcp_channel("ch1", vec![]);
        ch.max_inflight = 4;
        let errs = validate(&cfg(vec![pg("fast", 200)], vec![ch]));
        assert_single(
            &errs,
            |e| matches!(e, ConfigError::MaxInflightUnsupported { value: 4, .. }),
            "inflight on tcp",
        );
        assert!(errs[0].is_warning(), "TCP over-ask is a warning, not an error");

        // max_inflight 1 is always silent.
        let ch = tcp_channel("ch1", vec![]);
        assert!(validate(&cfg(vec![pg("fast", 200)], vec![ch])).is_empty());
    }

    #[test]
    fn bus_overload_emits_warning_not_error() {
        // 1200 baud, 50 ms period, 5 scattered single-register reads with
        // max_gap 0 => ~137 ms of airtime per txn, hopelessly over budget.
        let regs: Vec<_> = (0..5u16)
            .map(|i| entry(&format!("t{i}"), FC::ReadHoldingRegisters, i * 1000, DT::U16))
            .collect();
        let cfg = cfg(
            vec![pg("fast", 50)],
            vec![rtu_channel("bus1", 1200, vec![device("d1", 1, regs)])],
        );
        let errs = validate(&cfg);
        assert_single(
            &errs,
            |e| matches!(e, ConfigError::BusOverload { channel, utilization_permille } if channel == "bus1" && *utilization_permille > 1000),
            "bus overload",
        );
        assert!(errs[0].is_warning(), "BusOverload must be a warning");
    }

    #[test]
    fn coalescable_layout_stays_under_budget() {
        // Same register count but contiguous + bridgeable at sane baud: no warning.
        let regs: Vec<_> = (0..5u16)
            .map(|i| entry(&format!("t{i}"), FC::ReadHoldingRegisters, 100 + i * 2, DT::U32))
            .collect();
        let mut ch = rtu_channel("bus1", 19200, vec![device("d1", 1, regs)]);
        ch.max_gap = 4;
        let cfg = cfg(vec![pg("fast", 250)], vec![ch]);
        assert!(validate(&cfg).is_empty());
    }

    #[test]
    fn merge_runs_bridges_gaps_and_respects_caps() {
        // Two entries 4 apart bridge with max_gap 4; a far one stays separate.
        let runs = merge_runs(vec![(100, 102), (106, 108), (400, 401)], 4, 125);
        assert_eq!(runs, vec![(100, 108), (400, 401)]);
        // max_gap 0: nothing bridges.
        let runs = merge_runs(vec![(100, 102), (106, 108)], 0, 125);
        assert_eq!(runs.len(), 2);
        // Cap: a run may not exceed area_max even if contiguous.
        let runs = merge_runs(vec![(0, 100), (100, 200)], 0, 125);
        assert_eq!(runs.len(), 2);
    }
}
