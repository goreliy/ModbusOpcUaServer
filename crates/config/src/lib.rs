//! `gateway-config` — configuration schema (JSON via serde), validation and
//! name→dense-id resolution. No tokio; the only I/O is reading the file.
//!
//! Pipeline: read → parse (`schema`) → `migrate` → `validate` → `resolve`.
//! Package is named `gateway-config` to avoid clashing with the popular
//! crates.io `config` crate; import as `gateway_config`.

pub mod migrate;
pub mod resolve;
pub mod schema;
pub mod validate;

use std::path::Path;

use mb_types::{DataType, FunctionCode};

pub use resolve::{
    ResolvedChannel, ResolvedConfig, ResolvedDevice, ResolvedPollGroup, ResolvedRegister,
};
pub use schema::ConfigFile;

/// Everything that can go wrong loading a config. `validate` aggregates ALL
/// problems into `Validation(Vec<..>)` instead of failing on the first one.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("parse: {0}")]
    Parse(#[from] serde_json::Error),

    #[error("duplicate {kind} id `{id}`")]
    DuplicateId { kind: &'static str, id: String },

    #[error("tag `{tag}`: unknown poll_group `{group}`")]
    UnknownPollGroup { tag: String, group: String },

    #[error("poll_group `{group}`: period_ms must be >= 1")]
    BadPollPeriod { group: String },

    #[error("tag `{tag}`: {function:?} is not a read function; register entries are polled reads")]
    NonReadFunction { tag: String, function: FunctionCode },

    #[error("tag `{tag}`: data_type {data_type:?} incompatible with function {function:?}")]
    IncompatibleDataType {
        tag: String,
        function: FunctionCode,
        data_type: DataType,
    },

    #[error("tag `{tag}`: data_type {data_type:?} requires `length`")]
    MissingLength { tag: String, data_type: DataType },

    #[error("tag `{tag}`: `length` must be >= 1")]
    ZeroLength { tag: String },

    #[error("tag `{tag}`: `bit` index required for data_type `bit` on register reads")]
    MissingBitIndex { tag: String },

    #[error("tag `{tag}`: bit index {bit} outside 0..16")]
    BadBitIndex { tag: String, bit: u8 },

    #[error("tag `{tag}`: span of {qty} exceeds cap of {max} for this area")]
    QtyExceedsCap { tag: String, qty: u16, max: u16 },

    #[error("tag `{tag}`: address {address} + span {qty} overflows the 16-bit address space")]
    AddressOverflow { tag: String, address: u16, qty: u16 },

    #[error(
        "tag `{tag}`: custom read on stream transport (channel `{channel}`) requires \
         custom_response_len — the byte stream cannot self-delimit vendor frames"
    )]
    MissingCustomResponseLen { tag: String, channel: String },

    #[error("tag `{tag}`: custom_response_len {len} outside 1..={max} bytes")]
    BadCustomResponseLen { tag: String, len: u16, max: u16 },

    /// HARD ERROR: tokio-modbus frames unknown function codes by "whatever
    /// bytes are currently buffered", which is timing-dependent on a real
    /// serial port (design §3.2 / finding #17) and can desync the whole bus.
    /// RTU-over-TCP is accepted silently: single-segment replies frame
    /// reliably in practice.
    #[error(
        "tag `{tag}`: custom reads are not supported on serial RTU (channel \
         `{channel}`) — framing of unknown function codes cannot be guaranteed \
         by tokio-modbus on a serial stream; use rtu_over_tcp or tcp for \
         vendor functions"
    )]
    CustomReadOnSerialRtu { tag: String, channel: String },

    #[error("tag `{tag}`: custom_request: {reason}")]
    BadCustomRequest { tag: String, reason: String },

    #[error(
        "tag `{tag}`: custom function code {code} is not a valid request code — \
         Modbus function codes are 1..=127 (0x80+ marks exception responses)"
    )]
    BadCustomFunctionCode { tag: String, code: u8 },

    /// WARNING (does not fail the load): the request will be framed exactly
    /// like the standard function, which most slaves will reject as malformed.
    #[error(
        "tag `{tag}`: custom function code {code} collides with a standard \
         Modbus function code — use the dedicated read/write function instead"
    )]
    CustomCodeCollision { tag: String, code: u8 },

    #[error("opcua: {reason}")]
    BadOpcUa { reason: String },

    /// WARNING: with a bind-all `host` and no `advertised_host` the endpoint
    /// URLs advertised in discovery contain 0.0.0.0/:: — clients would
    /// receive a non-connectable address.
    #[error(
        "opcua: host `{host}` binds all interfaces and advertised_host is not \
         set — clients would receive a non-connectable endpoint URL; set \
         advertised_host to the hostname/IP clients use to reach the server"
    )]
    OpcUaNoAdvertisedHost { host: String },

    /// WARNING: plain-text password / trust-all certs are commissioning
    /// conveniences, not production settings.
    #[error("opcua (security): {reason}")]
    WeakOpcUaSecurity { reason: String },

    #[error("tag `{tag}`: writable is not supported here: {reason}")]
    NotWritable { tag: String, reason: String },

    #[error("tag `{tag}`: deadband must be finite and >= 0")]
    BadDeadband { tag: String },

    #[error("tag `{tag}`: retain_last {value} exceeds the cap of {max}")]
    RetainLastTooBig { tag: String, value: u16, max: u16 },

    /// WARNING: `formula` takes precedence — a customized `scale`/`offset`
    /// on the same entry is ignored and probably a config mistake.
    #[error("tag `{tag}`: `formula` is set, so the customized scale/offset are ignored")]
    ScaleShadowedByFormula { tag: String },

    #[error("channel `{channel}`: invalid transport: {reason}")]
    BadTransport { channel: String, reason: String },

    #[error("channel `{channel}`: max_inflight {value} > 1 is only allowed on tcp")]
    MaxInflightNotTcp { channel: String, value: usize },

    /// WARNING: the schema keeps `max_inflight` for the future parallel-TCP
    /// upgrade, but the runtime is sequential — resolve forces the value to 1.
    #[error(
        "channel `{channel}`: max_inflight {value} > 1: parallel TCP polling \
         is not implemented yet — the value is forced to 1"
    )]
    MaxInflightUnsupported { channel: String, value: usize },

    /// WARNING (does not fail the load): the static airtime estimate says this
    /// serial bus cannot physically meet its poll periods.
    #[error(
        "channel `{channel}`: estimated RTU bus utilization {utilization_permille}‰ \
         exceeds the baud budget — poll periods will run permanently overdue"
    )]
    BusOverload {
        channel: String,
        utilization_permille: u32,
    },

    #[error("configuration invalid ({} error(s)): {}", .0.len(), format_list(.0))]
    Validation(Vec<ConfigError>),
}

impl ConfigError {
    /// Warnings are reported on `ResolvedConfig::warnings` instead of failing
    /// the load.
    pub fn is_warning(&self) -> bool {
        matches!(
            self,
            Self::BusOverload { .. }
                | Self::CustomCodeCollision { .. }
                | Self::MaxInflightUnsupported { .. }
                | Self::OpcUaNoAdvertisedHost { .. }
                | Self::ScaleShadowedByFormula { .. }
                | Self::WeakOpcUaSecurity { .. }
        )
    }
}

fn format_list(errs: &[ConfigError]) -> String {
    errs.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

/// Load a config file: read → parse → migrate → validate → resolve.
///
/// Relative `gateway.data_dir`, `logging.dir` and `opcua.pki_dir` are rebased
/// onto the CONFIG FILE's directory (a service process's CWD is undefined
/// under systemd / the Windows SCM); absolute paths pass through untouched.
/// [`load_str`] performs no rebasing (it has no base directory).
pub fn load(path: impl AsRef<Path>) -> Result<ResolvedConfig, ConfigError> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path)?;
    let mut rc = load_str(&text)?;
    if let Some(base) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        rebase_onto(base, &mut rc.gateway.data_dir);
        if let Some(dir) = rc.logging.dir.as_mut() {
            rebase_onto(base, dir);
        }
        rebase_onto(base, &mut rc.opcua.pki_dir);
    }
    Ok(rc)
}

/// Rebase `dir` onto `base` when relative; absolute paths stay untouched.
fn rebase_onto(base: &Path, dir: &mut String) {
    if Path::new(dir.as_str()).is_relative() {
        *dir = base.join(dir.as_str()).to_string_lossy().into_owned();
    }
}

/// Same pipeline from an in-memory string (tests, embedded defaults).
pub fn load_str(text: &str) -> Result<ResolvedConfig, ConfigError> {
    let file: ConfigFile = serde_json::from_str(text)?;
    let cfg = migrate::migrate(file);
    let (warnings, errors): (Vec<_>, Vec<_>) = validate::validate(&cfg)
        .into_iter()
        .partition(ConfigError::is_warning);
    if !errors.is_empty() {
        return Err(ConfigError::Validation(errors));
    }
    Ok(resolve::resolve(cfg, warnings))
}

#[cfg(test)]
pub(crate) mod test_util {
    use crate::schema::v1::*;
    use mb_types::{ByteOrder, DataType, FunctionCode, WordOrder};

    pub fn pg(id: &str, period_ms: u64) -> PollGroupConfig {
        PollGroupConfig {
            id: id.into(),
            period_ms,
            priority: 0,
        }
    }

    pub fn entry(tag: &str, function: FunctionCode, address: u16, data_type: DataType) -> RegisterEntry {
        RegisterEntry {
            tag: tag.into(),
            poll_group: "fast".into(),
            function,
            address,
            data_type,
            word_order: WordOrder::default(),
            byte_order: ByteOrder::default(),
            length: None,
            bit: None,
            custom_response_len: None,
            custom_request: None,
            scale: 1.0,
            offset: 0.0,
            formula: None,
            write_formula: None,
            deadband: None,
            retentive: false,
            retain_last: None,
            units: None,
            writable: false,
            write_function: None,
        }
    }

    pub fn entry_custom(tag: &str, code: u8, custom_response_len: Option<u16>) -> RegisterEntry {
        RegisterEntry {
            custom_response_len,
            ..entry(tag, FunctionCode::Custom { code }, 0, DataType::U16)
        }
    }

    pub fn device(id: &str, unit_id: u8, registers: Vec<RegisterEntry>) -> DeviceConfig {
        DeviceConfig {
            id: id.into(),
            unit_id,
            enabled: true,
            request_timeout_ms: None,
            retry: None,
            offline_after_failures: None,
            max_gap: None,
            registers,
        }
    }

    pub fn tcp_channel(id: &str, devices: Vec<DeviceConfig>) -> ChannelConfig {
        ChannelConfig {
            id: id.into(),
            enabled: true,
            transport: TransportConfig::Tcp {
                host: "127.0.0.1".into(),
                port: 502,
                connect_timeout_ms: 5000,
            },
            request_timeout_ms: 1000,
            inter_request_delay_ms: 0,
            max_inflight: 1,
            max_gap: 0,
            log_traffic: false,
            retry: None,
            offline_after_failures: 3,
            devices,
        }
    }

    pub fn rtu_channel(id: &str, baud: u32, devices: Vec<DeviceConfig>) -> ChannelConfig {
        ChannelConfig {
            transport: TransportConfig::Rtu {
                path: "COM3".into(),
                baud,
                data_bits: 8,
                parity: Parity::None,
                stop_bits: 1,
            },
            inter_request_delay_ms: 20,
            ..tcp_channel(id, devices)
        }
    }

    pub fn rtu_over_tcp_channel(id: &str, devices: Vec<DeviceConfig>) -> ChannelConfig {
        ChannelConfig {
            transport: TransportConfig::RtuOverTcp {
                host: "10.0.0.7".into(),
                port: 4001,
                connect_timeout_ms: 5000,
            },
            ..tcp_channel(id, devices)
        }
    }

    /// Default OPC UA section for tests: `advertised_host` is set so the
    /// bind-all default host does not trip the `OpcUaNoAdvertisedHost`
    /// warning in every unrelated test.
    pub fn opcua() -> OpcUaConfig {
        OpcUaConfig {
            advertised_host: Some("192.168.0.2".into()),
            ..OpcUaConfig::default()
        }
    }

    pub fn cfg(poll_groups: Vec<PollGroupConfig>, channels: Vec<ChannelConfig>) -> ConfigV1 {
        ConfigV1 {
            gateway: GatewaySettings::default(),
            opcua: opcua(),
            logging: LoggingConfig::default(),
            poll_groups,
            channels,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::*;
    use mb_types::{DataType as DT, FunctionCode as FC};

    #[test]
    fn load_str_happy_path() {
        let json = r#"{
            "schema_version": "1",
            "opcua": { "advertised_host": "10.0.0.2" },
            "poll_groups": [ { "id": "fast", "period_ms": 200 } ],
            "channels": [ {
                "id": "ch1",
                "transport": { "type": "tcp", "host": "10.0.0.1" },
                "devices": [ {
                    "id": "dev1", "unit_id": 1,
                    "registers": [ {
                        "tag": "t1", "poll_group": "fast",
                        "function": "read_holding_registers",
                        "address": 0, "data_type": "u16"
                    } ]
                } ]
            } ]
        }"#;
        let rc = load_str(json).unwrap();
        assert_eq!(rc.tag_count(), 1);
        assert_eq!(rc.tag_name(mb_types::TagId(0)), Some("t1"));
        assert!(rc.warnings.is_empty());
    }

    #[test]
    fn load_rebases_relative_dirs_onto_config_parent() {
        let dir = tempfile::tempdir().unwrap();
        let abs_logs = dir.path().join("elsewhere").join("logs");

        let mut c = cfg(vec![pg("fast", 200)], vec![]);
        c.gateway.data_dir = "./data".into();
        c.logging.dir = Some(abs_logs.to_string_lossy().into_owned()); // absolute: untouched
        c.opcua.pki_dir = "pki".into();
        let json = serde_json::to_string(&ConfigFile::V1(c)).unwrap();

        let cfg_path = dir.path().join("config.json");
        std::fs::write(&cfg_path, &json).unwrap();

        // load(): relative dirs anchor to the config file's directory.
        let rc = load(&cfg_path).unwrap();
        assert_eq!(Path::new(&rc.gateway.data_dir), dir.path().join("data"));
        assert_eq!(Path::new(&rc.opcua.pki_dir), dir.path().join("pki"));
        assert_eq!(
            Path::new(rc.logging.dir.as_deref().unwrap()),
            abs_logs,
            "absolute paths must pass through untouched"
        );

        // load_str(): no base dir, nothing is rebased.
        let rc = load_str(&json).unwrap();
        assert_eq!(rc.gateway.data_dir, "./data");
        assert_eq!(rc.opcua.pki_dir, "pki");
    }

    #[test]
    fn load_str_aggregates_all_validation_errors() {
        // Two independent problems: duplicate tag AND unknown poll group.
        let mut bad = entry("dup", FC::ReadHoldingRegisters, 5, DT::U16);
        bad.poll_group = "nope".into();
        let c = cfg(
            vec![pg("fast", 200)],
            vec![tcp_channel(
                "ch1",
                vec![device(
                    "d1",
                    1,
                    vec![entry("dup", FC::ReadHoldingRegisters, 0, DT::U16), bad],
                )],
            )],
        );
        let json = serde_json::to_string(&ConfigFile::V1(c)).unwrap();
        match load_str(&json) {
            Err(ConfigError::Validation(errs)) => {
                assert_eq!(errs.len(), 2, "all problems reported: {errs:?}");
                assert!(errs.iter().any(|e| matches!(e, ConfigError::DuplicateId { kind: "tag", .. })));
                assert!(errs.iter().any(|e| matches!(e, ConfigError::UnknownPollGroup { .. })));
            }
            other => panic!("expected Validation error, got {other:?}"),
        }
    }

    #[test]
    fn bus_overload_warning_does_not_fail_the_load() {
        let regs: Vec<_> = (0..5u16)
            .map(|i| entry(&format!("t{i}"), FC::ReadHoldingRegisters, i * 1000, DT::U16))
            .collect();
        let c = cfg(
            vec![pg("fast", 50)],
            vec![rtu_channel("bus1", 1200, vec![device("d1", 1, regs)])],
        );
        let json = serde_json::to_string(&ConfigFile::V1(c)).unwrap();
        let rc = load_str(&json).expect("warning must not fail the load");
        assert_eq!(rc.warnings.len(), 1);
        assert!(matches!(rc.warnings[0], ConfigError::BusOverload { .. }));
    }

    #[test]
    fn malformed_json_is_a_parse_error() {
        assert!(matches!(load_str("{ not json"), Err(ConfigError::Parse(_))));
    }
}
