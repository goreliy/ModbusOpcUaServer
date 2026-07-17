//! Schema version 1 — serde types matching the JSON wire format.

use mb_types::{ByteOrder, DataType, FunctionCode, WordOrder};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigV1 {
    #[serde(default)]
    pub gateway: GatewaySettings,
    /// The OPC UA server facing the clients (the core of the product).
    #[serde(default)]
    pub opcua: OpcUaConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    /// Named poll groups, referenced by registers.
    pub poll_groups: Vec<PollGroupConfig>,
    pub channels: Vec<ChannelConfig>,
}

/// Application logging. Console output is always on (under systemd it lands
/// in the journal); `dir` additionally enables daily-rotated files — REQUIRED
/// in practice for the Windows service, which has no console.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// tracing env-filter, e.g. "info", "info,mb_poller=debug" or
    /// "info,modbus_traffic=debug" (the raw-frame hex log target).
    /// The `RUST_LOG` environment variable overrides this.
    #[serde(default = "d_log_level")]
    pub level: String,
    /// Log directory; None = console only. Relative paths resolve against
    /// the config file's directory when loaded via [`crate::load`].
    #[serde(default)]
    pub dir: Option<String>,
    /// Rolled file name prefix -> `<prefix>.YYYY-MM-DD`.
    #[serde(default = "d_log_prefix")]
    pub file_prefix: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: d_log_level(),
            dir: None,
            file_prefix: d_log_prefix(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewaySettings {
    #[serde(default)]
    pub instance_name: String,
    #[serde(default)]
    pub default_retry: RetryConfig,
    /// Directory for persistent state (sled: retentive tags, history rings).
    /// Relative paths resolve against the CONFIG FILE's directory when loaded
    /// via [`crate::load`] (a service process's CWD is undefined); `load_str`
    /// leaves them as written.
    #[serde(default = "d_data_dir")]
    pub data_dir: String,
}

impl Default for GatewaySettings {
    fn default() -> Self {
        Self {
            instance_name: String::new(),
            default_retry: RetryConfig::default(),
            data_dir: d_data_dir(),
        }
    }
}

/// The OPC UA server endpoint definition (the product IS an OPC UA server;
/// this section configures how it faces its clients).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpcUaConfig {
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// Bind host. 0.0.0.0 = all interfaces.
    #[serde(default = "d_host_any")]
    pub host: String,
    /// The hostname/IP clients use to reach the server — placed into the
    /// advertised endpoint URLs (discovery). When `None` and `host` is
    /// routable, `host` is used. Effectively required when `host` is a
    /// bind-all address (0.0.0.0 / ::): clients would otherwise receive a
    /// non-connectable endpoint URL (validation warns).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advertised_host: Option<String>,
    #[serde(default = "d_4840")]
    pub port: u16,
    #[serde(default = "d_app_name")]
    pub application_name: String,
    #[serde(default = "d_app_uri")]
    pub application_uri: String,
    /// Offer the plain (SecurityPolicy None) endpoint. Typical for isolated
    /// OT networks and commissioning; disable for hardened installs.
    #[serde(default = "d_true")]
    pub allow_none_security: bool,
    /// Offer Basic256Sha256 Sign / SignAndEncrypt endpoints.
    #[serde(default = "d_true")]
    pub basic256sha256: bool,
    /// Allow anonymous sessions. When false, at least one user is required.
    #[serde(default = "d_true")]
    pub allow_anonymous: bool,
    /// Username/password accounts (see [`OpcUaUser`]).
    #[serde(default)]
    pub users: Vec<OpcUaUser>,
    /// PKI directory (server certificate store). Relative paths resolve
    /// against the config file's directory when loaded via [`crate::load`].
    #[serde(default = "d_pki_dir")]
    pub pki_dir: String,
    /// Accept ANY client certificate on encrypted endpoints without checking
    /// the trust store. Commissioning convenience ONLY (validation warns);
    /// the secure default is false: unknown certs land in `pki/rejected/`
    /// and the operator moves them to `pki/trusted/`.
    #[serde(default)]
    pub trust_any_client_cert: bool,
}

impl Default for OpcUaConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            host: d_host_any(),
            advertised_host: None,
            port: d_4840(),
            application_name: d_app_name(),
            application_uri: d_app_uri(),
            allow_none_security: true,
            basic256sha256: true,
            allow_anonymous: true,
            users: Vec::new(),
            pki_dir: d_pki_dir(),
            trust_any_client_cert: false,
        }
    }
}

/// One username/password account. Exactly ONE of `password` /
/// `password_hash` must be set:
/// - `password` — plain text; convenient for commissioning, validation warns;
/// - `password_hash` — argon2id PHC string, generate with
///   `opc-modbus-server hash-password`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpcUaUser {
    pub username: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelConfig {
    /// Stable key; interned -> ChannelId.
    pub id: String,
    /// DEFAULT TRUE (footgun fix).
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// Tagged enum.
    pub transport: TransportConfig,
    #[serde(default = "d_1000")]
    pub request_timeout_ms: u64,
    /// RS-485 turnaround/silent gap between transactions (t3.5). 0 = none.
    #[serde(default)]
    pub inter_request_delay_ms: u64,
    /// TCP only. RTU/RtuOverTcp are forced to 1 at resolve time.
    #[serde(default = "d_1_usize")]
    pub max_inflight: usize,
    /// Coalescing gap tolerance in this channel's address units. 0 = never bridge holes.
    #[serde(default)]
    pub max_gap: u16,
    /// Hex-dump every raw frame on this channel to the `modbus_traffic`
    /// tracing target (enable with logging.level "...,modbus_traffic=debug").
    /// Field diagnostics without Wireshark; noisy — off by default.
    #[serde(default)]
    pub log_traffic: bool,
    /// Overrides `gateway.default_retry`; omitted = inherit the gateway level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryConfig>,
    #[serde(default = "d_3")]
    pub offline_after_failures: u32,
    pub devices: Vec<DeviceConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TransportConfig {
    Tcp {
        host: String,
        #[serde(default = "d_502")]
        port: u16,
        #[serde(default = "d_5000")]
        connect_timeout_ms: u64,
    },
    RtuOverTcp {
        host: String,
        port: u16,
        #[serde(default = "d_5000")]
        connect_timeout_ms: u64,
    },
    Rtu {
        /// "COM3" | "/dev/ttyUSB0"
        path: String,
        #[serde(default = "d_9600")]
        baud: u32,
        #[serde(default = "d_8")]
        data_bits: u8,
        #[serde(default)]
        parity: Parity,
        #[serde(default = "d_1_u8")]
        stop_bits: u8,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Parity {
    #[default]
    None,
    Even,
    Odd,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceConfig {
    /// Interned -> DeviceId.
    pub id: String,
    /// Modbus slave addr (many share one RTU bus).
    pub unit_id: u8,
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// Overrides channel.
    #[serde(default)]
    pub request_timeout_ms: Option<u64>,
    #[serde(default)]
    pub retry: Option<RetryConfig>,
    #[serde(default)]
    pub offline_after_failures: Option<u32>,
    /// Per-device override: forbid gap-bridging for devices that reject holes.
    #[serde(default)]
    pub max_gap: Option<u16>,
    pub registers: Vec<RegisterEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollGroupConfig {
    /// Interned -> PollGroupId.
    pub id: String,
    /// 200, 5000, ...
    pub period_ms: u64,
    /// Tie-break when several are due.
    #[serde(default)]
    pub priority: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterEntry {
    /// Interned -> TagId; unique across config.
    pub tag: String,
    /// FK -> PollGroupConfig.id.
    pub poll_group: String,
    pub function: FunctionCode,
    pub address: u16,
    pub data_type: DataType,
    #[serde(default)]
    pub word_order: WordOrder,
    #[serde(default)]
    pub byte_order: ByteOrder,
    /// Register/word count for ascii/bcd (variable width).
    #[serde(default)]
    pub length: Option<u16>,
    /// Bit index within the register for `Bit`/`Bitfield` on FC03/04.
    #[serde(default)]
    pub bit: Option<u8>,
    /// REQUIRED response byte count for `Custom` reads (stream cannot self-delimit).
    #[serde(default)]
    pub custom_response_len: Option<u16>,
    /// Request payload for `Custom` reads: a hex byte string like
    /// `"01 a0 ff"` (spaces optional), sent verbatim after the function code.
    /// Only valid on a `Custom` function; None = empty payload.
    #[serde(default)]
    pub custom_request: Option<String>,
    /// Carried for tags-core (Phase 2); the poller ignores these.
    #[serde(default = "d_scale")]
    pub scale: f64,
    #[serde(default)]
    pub offset: f64,
    /// Engineering-value expression over the decoded `raw` value (evalexpr);
    /// may call `tag("other.tag")` for read-at-eval cross-tag access. When
    /// set, `scale`/`offset` are ignored (validation warns if both are
    /// customized). Syntax is checked by tags-core at engine start.
    #[serde(default)]
    pub formula: Option<String>,
    /// Inverse expression for the write path (phase 4): engineering `value`
    /// -> raw units to encode.
    #[serde(default)]
    pub write_formula: Option<String>,
    /// Absolute deadband on the published engineering value: the typed
    /// publish is suppressed while |new - last_published| < deadband
    /// (quality transitions always publish). Numeric tags only.
    #[serde(default)]
    pub deadband: Option<f64>,
    /// Persist the last value across restarts (restored as Uncertain).
    #[serde(default)]
    pub retentive: bool,
    /// Keep a short ring of the last N published values (None/0 = off).
    #[serde(default)]
    pub retain_last: Option<u16>,
    /// Engineering units, surfaced to OPC UA (EUInformation) in phase 3.
    #[serde(default)]
    pub units: Option<String>,
    /// Allow OPC UA clients to write this tag back to the device. Only
    /// holding-register and coil sources can be written; when the read
    /// `formula` is set, a `write_formula` (inverse) is required.
    #[serde(default)]
    pub writable: bool,
    /// Explicit Modbus write function code for a `writable` tag. `None` picks
    /// the natural one: a coil source (`read_coils`) writes with FC05
    /// (`write_single_coil`); a holding source (`read_holding_registers`)
    /// writes with FC06 (`write_single_register`) for a single 16-bit word or
    /// FC16 (`write_multiple_registers`) for multi-word values. Override it to
    /// force a function some devices require: `write_multiple_coils` (FC15) for
    /// a coil, or `write_multiple_registers` (FC16) for a single register.
    /// Only the four write FCs are accepted (validated); it is ignored unless
    /// `writable` is set.
    #[serde(default)]
    pub write_function: Option<FunctionCode>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct RetryConfig {
    /// Per-request, same connection.
    #[serde(default = "d_2")]
    pub max_retries: u32,
    /// Reconnect/probe floor.
    #[serde(default = "d_500")]
    pub base_backoff_ms: u64,
    /// Ceiling.
    #[serde(default = "d_30000")]
    pub max_backoff_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 2,
            base_backoff_ms: 500,
            max_backoff_ms: 30_000,
        }
    }
}

// serde defaults
fn d_true() -> bool {
    true
}
fn d_1_usize() -> usize {
    1
}
fn d_1_u8() -> u8 {
    1
}
fn d_502() -> u16 {
    502
}
fn d_5000() -> u64 {
    5000
}
fn d_9600() -> u32 {
    9600
}
fn d_8() -> u8 {
    8
}
fn d_3() -> u32 {
    3
}
fn d_1000() -> u64 {
    1000
}
fn d_2() -> u32 {
    2
}
fn d_500() -> u64 {
    500
}
fn d_30000() -> u64 {
    30_000
}
fn d_log_level() -> String {
    "info".into()
}
fn d_log_prefix() -> String {
    "opc-modbus-server".into()
}
fn d_data_dir() -> String {
    "./data".into()
}
fn d_host_any() -> String {
    "0.0.0.0".into()
}
fn d_4840() -> u16 {
    4840
}
fn d_app_name() -> String {
    "OPC Modbus Server".into()
}
fn d_app_uri() -> String {
    "urn:opc-modbus-server".into()
}
fn d_pki_dir() -> String {
    "pki".into()
}
fn d_scale() -> f64 {
    1.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::ConfigFile;
    use mb_types::{DataType, FunctionCode, WordOrder};

    #[test]
    fn happy_path_parse_with_defaults() {
        let json = r#"{
            "schema_version": "1",
            "poll_groups": [ { "id": "fast", "period_ms": 200 } ],
            "channels": [
                {
                    "id": "ch1",
                    "transport": { "type": "tcp", "host": "10.0.0.1" },
                    "devices": [
                        {
                            "id": "dev1",
                            "unit_id": 3,
                            "registers": [
                                {
                                    "tag": "t1",
                                    "poll_group": "fast",
                                    "function": "read_holding_registers",
                                    "address": 100,
                                    "data_type": "f32",
                                    "word_order": "little_endian"
                                }
                            ]
                        }
                    ]
                }
            ]
        }"#;
        let ConfigFile::V1(cfg) = serde_json::from_str::<ConfigFile>(json).unwrap();
        assert_eq!(cfg.poll_groups.len(), 1);
        assert_eq!(cfg.poll_groups[0].period_ms, 200);
        assert_eq!(cfg.poll_groups[0].priority, 0); // default

        let ch = &cfg.channels[0];
        assert!(ch.enabled); // default true
        assert_eq!(ch.request_timeout_ms, 1000); // default
        assert_eq!(ch.max_inflight, 1); // default
        assert_eq!(ch.max_gap, 0); // default
        assert!(ch.retry.is_none()); // omitted -> inherit gateway.default_retry
        assert_eq!(ch.offline_after_failures, 3); // default
        match &ch.transport {
            TransportConfig::Tcp {
                host,
                port,
                connect_timeout_ms,
            } => {
                assert_eq!(host, "10.0.0.1");
                assert_eq!(*port, 502); // default
                assert_eq!(*connect_timeout_ms, 5000); // default
            }
            other => panic!("wrong transport: {other:?}"),
        }

        let dev = &ch.devices[0];
        assert_eq!(dev.unit_id, 3);
        assert!(dev.enabled);
        assert!(dev.request_timeout_ms.is_none());
        assert!(dev.max_gap.is_none());

        let reg = &dev.registers[0];
        assert_eq!(reg.function, FunctionCode::ReadHoldingRegisters);
        assert_eq!(reg.address, 100);
        assert_eq!(reg.data_type, DataType::F32);
        assert_eq!(reg.word_order, WordOrder::LittleEndian);
        assert_eq!(reg.scale, 1.0); // default
        assert_eq!(reg.offset, 0.0); // default
    }

    #[test]
    fn unknown_schema_version_is_rejected() {
        let json = r#"{ "schema_version": "99", "poll_groups": [], "channels": [] }"#;
        assert!(serde_json::from_str::<ConfigFile>(json).is_err());
    }

    #[test]
    fn rtu_transport_parses_with_serial_params() {
        let json = r#"{ "type": "rtu", "path": "COM3", "baud": 19200, "parity": "even" }"#;
        match serde_json::from_str::<TransportConfig>(json).unwrap() {
            TransportConfig::Rtu {
                path,
                baud,
                data_bits,
                parity,
                stop_bits,
            } => {
                assert_eq!(path, "COM3");
                assert_eq!(baud, 19200);
                assert_eq!(data_bits, 8); // default
                assert_eq!(parity, Parity::Even);
                assert_eq!(stop_bits, 1); // default
            }
            other => panic!("wrong transport: {other:?}"),
        }
    }
}
