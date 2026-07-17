# Phase 1 Design — Modbus Polling Engine & Configuration Model

**Status:** Implementation-ready. **Scope:** Phase 1 only (Modbus transports, config model, scheduler, coalescing, tag cache, error/quality). Numeric decode + formulas are Phase 2 (`tags-core`); this document designs the cache boundary they consume but writes only *raw* words/bits.

---

## 0. Decision Summary

The **SCALE-FIRST** proposal is the backbone: one owning tokio task per physical channel (bounded task count), compile-time coalescing that decouples wire-request count from tag count, a dense-`TagId` flat-slot cache with single-writer-per-slot, and the Exception-vs-comms error asymmetry. Onto it we graft:

- From **EXTENSIBILITY-FIRST**: the `RawValueSink` trait as the poller→consumer seam (so OPC UA / MQTT / `tags-core` plug in without the poller importing them), schema-versioned config, and Custom/raw-PDU as a first-class variant.
- From **SIMPLICITY-FIRST**: the transport as a *plain enum* dispatched at 3 call sites (no `Box<dyn>`, no `async_trait` boxing in the hot loop), one-`Interval`-per-poll-group timers, and the pure unit-testable `build_plan` coalescer.

All three judge panels converged on the same hard corrections, which are now **verified against the vendored `tokio-modbus 0.17.0` source** and baked in below:

| Verified fact (from source) | Consequence for this design |
|---|---|
| `client::rtu::attach_slave<T>(transport, slave)` is generic over any `T: AsyncRead + AsyncWrite + Unpin + Send + 'static` (`src/client/rtu.rs:20`) | **RTU-over-TCP = `rtu::attach_slave(TcpStream, slave)`.** No hand-rolled CRC/framing. Deletes the single riskiest module all three proposals carried. |
| `pub type Result<T> = Result<Result<T, ExceptionCode>, Error>`; `Error = Protocol(ProtocolError) \| Transport(io::Error)` (`src/lib.rs:62`, `src/error.rs:15`) | The prompt's "`…, std::io::Error>`" is **wrong**. Outer error is `tokio_modbus::Error`. `ProtocolError = HeaderMismatch \| FunctionCodeMismatch` must be modeled and classified as a frame-desync (comms) fault. |
| `mod codec;` is private; `ClientCodec`/`calc_crc` are `pub(crate)` (`src/lib.rs:42`) | Cannot reuse the codec. But we don't need to — the generic `attach_slave` gives us the battle-tested `ClientCodec` (CRC + 20-retry resync) *for free* behind `Context`. |
| Custom FC response length falls back to `adu_buf.len() - 3` on an already-buffered frame (`src/codec/rtu.rs:218`) | Custom **reads** over a byte stream (RTU / RTU-over-TCP) cannot self-delimit; config must carry an expected response length. |
| Scaffolded `mb-proto/Cargo.toml` disables `rtu` and omits `tokio-serial` | Must enable `tokio-modbus` features `["rtu","tcp"]` and add `tokio-serial = "5.4"`. |

---

## 1. Crate & Module Layout

Dependency direction is strictly one-way. A new leaf crate **`mb-types`** holds the shared vocabulary enums so `mb-proto` and (later) `tags-core` never depend on the config crate.

```
mb-types  <-  gateway-config
   ^   ^            ^
   |   |            |
   |   +---- mb-proto
   |            ^
   +------------+---- mb-poller   (depends on mb-types, gateway-config, mb-proto)
                        ^
                        +---- tags-core (Phase 2; depends only on the cache boundary + mb-types)
```

### `mb-types/` — shared vocabulary (no serde-required deps beyond derive; no tokio, no I/O)
| File | Responsibility (one line) |
|---|---|
| `src/lib.rs` | Re-exports; the enums below plus the `TagId`/`ChannelId`/`DeviceId`/`PollGroupId` newtypes. |
| `src/datatype.rs` | `DataType`, `WordOrder`, `ByteOrder` + `DataType::register_count()` / `bit-domain` helpers. |
| `src/function.rs` | `FunctionCode` (read+write+`Custom{code}`) with numeric mapping. |
| `src/ids.rs` | Dense newtypes `TagId(u32)`, `ChannelId(u16)`, `DeviceId(u32)`, `PollGroupId(u16)`. |

### `gateway-config/` — serde schema + validation (no tokio, no I/O beyond reading a file)
| File | Responsibility |
|---|---|
| `src/lib.rs` | Re-exports; `pub fn load(path) -> Result<ResolvedConfig, ConfigError>` = read → parse → migrate → validate → intern. |
| `src/schema/mod.rs` | `enum ConfigFile { V1(ConfigV1) }` tagged by `schema_version` (forward-compat root). |
| `src/schema/v1.rs` | `ConfigV1`, `ChannelConfig`, `DeviceConfig`, `PollGroupConfig`, `RegisterEntry`, `RetryConfig`, `BackoffConfig`, transport params, `Parity`. |
| `src/migrate.rs` | `fn migrate(ConfigFile) -> ConfigV1` (identity today; future V1→V2 upgrades). |
| `src/validate.rs` | Semantic checks → aggregated `Vec<ConfigError>` (see §7 for the full rule list, incl. single-writer & bus-budget). |
| `src/resolve.rs` | `ResolvedConfig`: interns names → dense `TagId`/`ChannelId`/…; assigns each channel an **exclusive contiguous `TagId` range** (makes single-writer structural). |
| `src/schema.json` | JSON Schema artifact used by `validate.rs` (jsonschema) *and* shipped for external tooling. |

### `mb-proto/` — transports, framing, typed request/response
| File | Responsibility |
|---|---|
| `src/lib.rs` | Re-exports `Transport`, `ModbusRequest`, `ModbusResponse`, `ProtoError`. |
| `src/error.rs` | `ProtoError` (thiserror) + `flatten()` mapping `tokio_modbus::Result<T>` → `ProtoError` (the *one* flattening site). |
| `src/request.rs` | `ModbusRequest` / `ModbusResponse` enums + `to_tokio_request()` conversion (incl. `Request::Custom`). |
| `src/transport.rs` | `enum Transport { Tcp, Rtu, RtuOverTcp }` owning a `tokio_modbus::client::Context`; `connect()` factory + `request()`. |
| `src/connect.rs` | Opens the byte pipe per transport: `tcp::connect`, `tokio_serial::SerialStream` → `rtu::attach_slave`, `TcpStream` → `rtu::attach_slave`. |

*(No `codec.rs`, no `rtu_over_tcp.rs` hand-rolled framing — deleted per the verified `attach_slave` finding.)*

### `mb-poller/` — scheduler, coalescing, cache
| File | Responsibility |
|---|---|
| `src/lib.rs` | `Poller::spawn(ResolvedConfig, Arc<dyn RawValueSink>) -> PollerHandle`; spawns one task per channel; owns write-command senders + shutdown. |
| `src/plan.rs` | Compile step: `ResolvedConfig` → `Vec<ChannelPlan>` (devices → per-group coalesced `Transaction`s). |
| `src/coalesce.rs` | Pure `coalesce(entries, caps) -> Vec<Transaction>` merge algorithm + unit tests (§5). |
| `src/channel.rs` | `run_channel(...)`: the per-channel task — connect/backoff, sequential RTU / bounded TCP, write interleave, watchdog. |
| `src/schedule.rs` | `PollWheel`: one `Interval` per period on the channel; `next_due()` yields due `(device, group)` batches. |
| `src/device.rs` | `DeviceRuntime`: failure counter, online flag, backoff, de-coalescing state. |
| `src/cache.rs` | `TagCache` (flat `Box<[TagSlot]>`), `RawValueSink`/`CacheReader` traits, `Snapshot`, `Quality`. |
| `src/command.rs` | `WriteCommand` + per-channel `mpsc` write path (interleaves with polls). |
| `src/metrics.rs` | Per-channel/per-device `AtomicU64` counters incl. `bus_busy_ratio`. |

---

## 2. Config Types (`gateway-config` + `mb-types`)

```rust
// ===================== mb-types/src/lib.rs =====================
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct ChannelId(pub u16);
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct DeviceId(pub u32);
#[derive(Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Debug, Serialize, Deserialize)]
pub struct TagId(pub u32);        // dense, contiguous from 0; indexes the flat cache
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct PollGroupId(pub u16);

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
        matches!(self, Self::ReadCoils | Self::ReadDiscreteInputs
                     | Self::ReadHoldingRegisters | Self::ReadInputRegisters)
    }
    /// Which address space this FC reads (used to bucket coalescing).
    pub fn read_area(self) -> Option<Area> {
        match self {
            Self::ReadCoils            => Some(Area::Coils),
            Self::ReadDiscreteInputs   => Some(Area::DiscreteInputs),
            Self::ReadHoldingRegisters => Some(Area::Holding),
            Self::ReadInputRegisters   => Some(Area::Input),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Area { Coils, DiscreteInputs, Holding, Input }
impl Area {
    /// Whether the address unit is a single-bit coil or a 16-bit register.
    pub fn is_bit_domain(self) -> bool { matches!(self, Area::Coils | Area::DiscreteInputs) }
    /// Modbus PDU response cap in *this area's units*.
    pub fn max_qty(self) -> u16 { if self.is_bit_domain() { 2000 } else { 125 } }
}

/// NOTE: NO `#[serde(other)]` catch-all. An unknown data type is a hard load-time
/// error, not a silently-unpollable tag. (Judge must-fix, all three panels.)
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DataType { Bit, U16, I16, U32, I32, U64, I64, F32, F64, Bcd, Ascii, Bitfield }

impl DataType {
    /// Register span for fixed-width numeric types; `None` = caller must supply `length`.
    pub const fn register_count(self) -> Option<u16> {
        match self {
            DataType::Bit | DataType::U16 | DataType::I16 | DataType::Bitfield => Some(1),
            DataType::U32 | DataType::I32 | DataType::F32                      => Some(2),
            DataType::U64 | DataType::I64 | DataType::F64                      => Some(4),
            DataType::Bcd | DataType::Ascii                                    => None, // needs length
        }
    }
}

/// Four canonical byte/word orderings, unambiguous for f32/u32 decode.
/// (Simplicity-panel must-fix: "big/little endian" alone is under-specified.)
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WordOrder { #[default] BigEndian, LittleEndian }  // high-word-first / low-word-first
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ByteOrder { #[default] BigEndian, LittleEndian }  // within each 16-bit word
```

```rust
// ===================== gateway-config/src/schema/v1.rs =====================
use serde::{Deserialize, Serialize};
use mb_types::{DataType, FunctionCode, WordOrder, ByteOrder};

/// Versioned on-disk root. Adding V2 later is additive.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "schema_version")]
pub enum ConfigFile {
    #[serde(rename = "1")]
    V1(ConfigV1),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigV1 {
    #[serde(default)] pub gateway: GatewaySettings,
    pub poll_groups: Vec<PollGroupConfig>,   // named, referenced by registers
    pub channels: Vec<ChannelConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GatewaySettings {
    #[serde(default)] pub instance_name: String,
    #[serde(default)] pub default_retry: RetryConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelConfig {
    pub id: String,                                  // stable key; interned -> ChannelId
    #[serde(default = "d_true")] pub enabled: bool,  // DEFAULT TRUE (footgun fix)
    pub transport: TransportConfig,                  // tagged enum
    #[serde(default = "d_1000")] pub request_timeout_ms: u64,
    /// RS-485 turnaround/silent gap between transactions (t3.5). 0 = none.
    #[serde(default)] pub inter_request_delay_ms: u64,
    /// TCP only. RTU/RtuOverTcp are forced to 1 at resolve time.
    #[serde(default = "d_1_usize")] pub max_inflight: usize,
    /// Coalescing gap tolerance in this channel's address units. 0 = never bridge holes.
    #[serde(default)] pub max_gap: u16,
    #[serde(default)] pub retry: RetryConfig,
    #[serde(default = "d_3")] pub offline_after_failures: u32,
    pub devices: Vec<DeviceConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TransportConfig {
    Tcp { host: String, #[serde(default = "d_502")] port: u16,
          #[serde(default = "d_5000")] connect_timeout_ms: u64 },
    RtuOverTcp { host: String, port: u16,
          #[serde(default = "d_5000")] connect_timeout_ms: u64 },
    Rtu { path: String,                          // "COM3" | "/dev/ttyUSB0"
          #[serde(default = "d_9600")] baud: u32,
          #[serde(default = "d_8")]   data_bits: u8,
          #[serde(default)]           parity: Parity,
          #[serde(default = "d_1_u8")] stop_bits: u8 },
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Parity { #[default] None, Even, Odd }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceConfig {
    pub id: String,                                  // interned -> DeviceId
    pub unit_id: u8,                                 // Modbus slave addr (many share one RTU bus)
    #[serde(default = "d_true")] pub enabled: bool,
    #[serde(default)] pub request_timeout_ms: Option<u64>, // overrides channel
    #[serde(default)] pub retry: Option<RetryConfig>,
    #[serde(default)] pub offline_after_failures: Option<u32>,
    /// Per-device override: forbid gap-bridging for devices that reject holes.
    #[serde(default)] pub max_gap: Option<u16>,
    pub registers: Vec<RegisterEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollGroupConfig {
    pub id: String,                                  // interned -> PollGroupId
    pub period_ms: u64,                              // 200, 5000, ...
    #[serde(default)] pub priority: i32,             // tie-break when several are due
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterEntry {
    pub tag: String,                                 // interned -> TagId; unique across config
    pub poll_group: String,                          // FK -> PollGroupConfig.id
    pub function: FunctionCode,
    pub address: u16,
    pub data_type: DataType,
    #[serde(default)] pub word_order: WordOrder,
    #[serde(default)] pub byte_order: ByteOrder,
    /// Register/word count for ascii/bcd (variable width).
    #[serde(default)] pub length: Option<u16>,
    /// Bit index within the register for `Bit`/`Bitfield` on FC03/04.
    #[serde(default)] pub bit: Option<u8>,
    /// REQUIRED response byte count for `Custom` reads (stream cannot self-delimit).
    #[serde(default)] pub custom_response_len: Option<u16>,
    /// Carried for tags-core (Phase 2); the poller ignores these.
    #[serde(default = "d_scale")] pub scale: f64,
    #[serde(default)] pub offset: f64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct RetryConfig {
    #[serde(default = "d_2")]     pub max_retries: u32,      // per-request, same connection
    #[serde(default = "d_500")]   pub base_backoff_ms: u64,  // reconnect/probe floor
    #[serde(default = "d_30000")] pub max_backoff_ms: u64,   // ceiling
}
impl Default for RetryConfig {
    fn default() -> Self { Self { max_retries: 2, base_backoff_ms: 500, max_backoff_ms: 30_000 } }
}

// serde defaults
fn d_true() -> bool { true }  fn d_1_usize() -> usize { 1 } fn d_1_u8() -> u8 { 1 }
fn d_502() -> u16 { 502 }     fn d_5000() -> u64 { 5000 }   fn d_9600() -> u32 { 9600 }
fn d_8() -> u8 { 8 }          fn d_3() -> u32 { 3 }         fn d_1000() -> u64 { 1000 }
fn d_2() -> u32 { 2 }         fn d_500() -> u64 { 500 }     fn d_30000() -> u64 { 30_000 }
fn d_scale() -> f64 { 1.0 }
```

`ResolvedConfig` (from `resolve.rs`) is what leaves the crate: identical shape but names replaced by dense ids, per-channel `TagId` ranges assigned, and per-device timeouts/retries/`max_gap`/`offline_after` fully resolved (device override → channel → gateway default).

---

## 3. Transport Layer (`mb-proto`)

Plain enum, no `Box<dyn>`, no `async_trait` in the hot loop. All three transports collapse onto a single `tokio_modbus::client::Context` because `Context` is transport-generic and RTU is byte-pipe-generic.

```rust
// ===================== mb-proto/src/request.rs =====================
use std::borrow::Cow;
use mb_types::FunctionCode;
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
    /// Vendor/raw PDU. `expect_len` = expected response byte count (see §3.2).
    Custom { code: u8, data: Vec<u8>, expect_len: Option<u16> },
}

#[derive(Debug, Clone)]
pub enum ModbusResponse {
    Bits(Vec<bool>),        // FC01/02
    Registers(Vec<u16>),    // FC03/04
    WriteAck,               // FC05/06/15/16
    Raw(bytes::Bytes),      // Custom
}

impl ModbusRequest {
    pub fn to_tokio_request(&self) -> Request<'static> {
        match self {
            Self::ReadCoils { addr, qty }             => Request::ReadCoils(*addr, *qty),
            Self::ReadDiscreteInputs { addr, qty }    => Request::ReadDiscreteInputs(*addr, *qty),
            Self::ReadHoldingRegisters { addr, qty }  => Request::ReadHoldingRegisters(*addr, *qty),
            Self::ReadInputRegisters { addr, qty }    => Request::ReadInputRegisters(*addr, *qty),
            Self::WriteSingleCoil { addr, value }     => Request::WriteSingleCoil(*addr, *value),
            Self::WriteSingleRegister { addr, value } => Request::WriteSingleRegister(*addr, *value),
            Self::WriteMultipleCoils { addr, values } =>
                Request::WriteMultipleCoils(*addr, Cow::Owned(values.clone())),
            Self::WriteMultipleRegisters { addr, values } =>
                Request::WriteMultipleRegisters(*addr, Cow::Owned(values.clone())),
            Self::Custom { code, data, .. } => Request::Custom(*code, Cow::Owned(data.clone())),
        }
    }
}
```

### 3.1 Error type — modeled against the *real* `tokio_modbus::Error`

```rust
// ===================== mb-proto/src/error.rs =====================
use mb_types::FunctionCode;
use tokio_modbus::ExceptionCode;

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    /// Underlying I/O: connect refused, reset, EOF. Fatal -> reconnect.
    #[error("io: {0}")]        Io(std::io::Error),
    /// Request-level timeout (our tokio::time::timeout). See §6 for stream drain rule.
    #[error("timeout")]        Timeout,
    /// Slave answered with a Modbus exception. Link is HEALTHY. Tag-scoped Bad.
    #[error("modbus exception: {0:?}")] Exception(ExceptionCode),
    /// Frame desync: HeaderMismatch / FunctionCodeMismatch. Fatal -> reconnect.
    #[error("protocol/frame desync: {0}")] Protocol(String),
    /// Transport not connected yet.
    #[error("not connected")]  NotConnected,
}
impl ProtoError {
    /// Fatal = drop the connection and reconnect with backoff.
    /// Non-fatal (Exception, Timeout) = per-request retry on the same connection.
    pub fn is_fatal(&self) -> bool {
        matches!(self, Self::Io(_) | Self::Protocol(_) | Self::NotConnected)
    }
}

/// THE single flattening site. `tokio_modbus::Result<T> = Result<Result<T, ExceptionCode>, Error>`
/// where `Error = Protocol(ProtocolError) | Transport(io::Error)` (verified src/error.rs).
pub fn flatten<T>(r: tokio_modbus::Result<T>) -> Result<T, ProtoError> {
    use tokio_modbus::Error as TmErr;
    match r {
        Ok(Ok(v))               => Ok(v),
        Ok(Err(exc))            => Err(ProtoError::Exception(exc)),      // slave alive, rejected
        Err(TmErr::Transport(e)) => Err(ProtoError::Io(e)),             // socket/serial failure
        Err(TmErr::Protocol(p)) => Err(ProtoError::Protocol(p.to_string())), // frame desync -> fatal
    }
}
```

### 3.2 The transport enum

```rust
// ===================== mb-proto/src/transport.rs =====================
use std::time::Duration;
use tokio_modbus::client::Context;
use tokio_modbus::prelude::{Reader, Writer, SlaveContext, Slave};
use crate::{ModbusRequest, ModbusResponse, ProtoError, error::flatten, connect};
use gateway_config::schema::v1::TransportConfig;

/// One connection. The channel task owns exactly one of these by value, so the
/// borrow checker enforces "one request in flight" on a half-duplex bus for free.
pub enum Transport { Tcp(Context), Rtu(Context), RtuOverTcp(Context) }

impl Transport {
    pub fn kind(&self) -> Kind {
        match self { Self::Tcp(_) => Kind::Tcp, Self::Rtu(_) => Kind::Rtu,
                     Self::RtuOverTcp(_) => Kind::RtuOverTcp }
    }
    pub fn is_half_duplex(&self) -> bool { !matches!(self, Self::Tcp(_)) }

    /// (Re)connect. RTU-over-TCP simply attaches the RTU client to a raw TcpStream —
    /// verified: `rtu::attach_slave<T: AsyncRead+AsyncWrite+Unpin+Send+'static>` (src/client/rtu.rs:20).
    pub async fn connect(cfg: &TransportConfig) -> Result<Self, ProtoError> {
        match cfg {
            TransportConfig::Tcp { host, port, connect_timeout_ms } =>
                Ok(Self::Tcp(connect::tcp(host, *port, *connect_timeout_ms).await?)),
            TransportConfig::Rtu { path, baud, data_bits, parity, stop_bits } =>
                Ok(Self::Rtu(connect::rtu_serial(path, *baud, *data_bits, *parity, *stop_bits).await?)),
            TransportConfig::RtuOverTcp { host, port, connect_timeout_ms } =>
                Ok(Self::RtuOverTcp(connect::rtu_over_tcp(host, *port, *connect_timeout_ms).await?)),
        }
    }

    fn ctx(&mut self) -> &mut Context {
        match self { Self::Tcp(c) | Self::Rtu(c) | Self::RtuOverTcp(c) => c }
    }

    /// Issue one request to `unit`, bounded by `timeout`.
    pub async fn request(&mut self, unit: u8, req: &ModbusRequest, timeout: Duration)
        -> Result<ModbusResponse, ProtoError>
    {
        let ctx = self.ctx();
        ctx.set_slave(Slave(unit));
        let fut = async {
            use ModbusRequest as R;
            match req {
                R::ReadCoils { addr, qty }            => flatten(ctx.read_coils(*addr, *qty).await).map(ModbusResponse::Bits),
                R::ReadDiscreteInputs { addr, qty }   => flatten(ctx.read_discrete_inputs(*addr, *qty).await).map(ModbusResponse::Bits),
                R::ReadHoldingRegisters { addr, qty } => flatten(ctx.read_holding_registers(*addr, *qty).await).map(ModbusResponse::Registers),
                R::ReadInputRegisters { addr, qty }   => flatten(ctx.read_input_registers(*addr, *qty).await).map(ModbusResponse::Registers),
                R::WriteSingleCoil { addr, value }    => flatten(ctx.write_single_coil(*addr, *value).await).map(|_| ModbusResponse::WriteAck),
                R::WriteSingleRegister { addr, value }=> flatten(ctx.write_single_register(*addr, *value).await).map(|_| ModbusResponse::WriteAck),
                R::WriteMultipleCoils { addr, values }=> flatten(ctx.write_multiple_coils(*addr, values).await).map(|_| ModbusResponse::WriteAck),
                R::WriteMultipleRegisters { addr, values } => flatten(ctx.write_multiple_registers(*addr, values).await).map(|_| ModbusResponse::WriteAck),
                R::Custom { .. } => {
                    let resp = flatten(ctx.call(req.to_tokio_request()).await)?;
                    match resp { tokio_modbus::prelude::Response::Custom(_, bytes) => Ok(ModbusResponse::Raw(bytes)),
                                 _ => Err(ProtoError::Protocol("unexpected non-custom response".into())) }
                }
            }
        };
        match tokio::time::timeout(timeout, fut).await {
            Ok(res) => res,
            Err(_)  => Err(ProtoError::Timeout),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind { Tcp, Rtu, RtuOverTcp }
```

```rust
// ===================== mb-proto/src/connect.rs =====================
use std::{net::ToSocketAddrs, time::Duration};
use tokio::net::TcpStream;
use tokio_modbus::client::{Context, tcp, rtu};
use tokio_modbus::prelude::Slave;

pub async fn tcp(host: &str, port: u16, timeout_ms: u64) -> Result<Context, crate::ProtoError> {
    let addr = (host, port).to_socket_addrs().map_err(crate::ProtoError::Io)?
        .next().ok_or_else(|| crate::ProtoError::Protocol("bad host".into()))?;
    let ctx = tokio::time::timeout(Duration::from_millis(timeout_ms), tcp::connect(addr))
        .await.map_err(|_| crate::ProtoError::Timeout)?.map_err(crate::ProtoError::Io)?;
    Ok(ctx)
}

/// RTU-over-TCP: raw RTU frames on a plain TcpStream, NO MBAP — inherits ClientCodec CRC/resync.
pub async fn rtu_over_tcp(host: &str, port: u16, timeout_ms: u64) -> Result<Context, crate::ProtoError> {
    let addr = (host, port).to_socket_addrs().map_err(crate::ProtoError::Io)?
        .next().ok_or_else(|| crate::ProtoError::Protocol("bad host".into()))?;
    let stream = tokio::time::timeout(Duration::from_millis(timeout_ms), TcpStream::connect(addr))
        .await.map_err(|_| crate::ProtoError::Timeout)?.map_err(crate::ProtoError::Io)?;
    Ok(rtu::attach_slave(stream, Slave(0)))     // slave overwritten per-request via set_slave
}

pub async fn rtu_serial(path: &str, baud: u32, data_bits: u8,
                        parity: gateway_config::schema::v1::Parity, stop_bits: u8)
    -> Result<Context, crate::ProtoError>
{
    let builder = tokio_serial::new(path, baud)
        .data_bits(map_data_bits(data_bits))
        .parity(map_parity(parity))
        .stop_bits(map_stop_bits(stop_bits));
    let stream = tokio_serial::SerialStream::open(&builder).map_err(|e| crate::ProtoError::Io(e.into()))?;
    Ok(rtu::attach_slave(stream, Slave(0)))
}
```

**Custom-read framing (§3.2 must-fix).** For `Custom` on RTU / RTU-over-TCP, `get_response_pdu_len` in `tokio-modbus` falls back to "consume whatever is buffered − 3" for unknown function codes (verified `src/codec/rtu.rs:218`), which cannot delimit a fresh stream frame. Therefore `RegisterEntry.custom_response_len` is **required** for Custom reads on stream transports (validated in §7); the poller uses it to enforce a bounded read via the request timeout and to reject short/long frames. Custom *writes* and any Custom on TCP (MBAP is length-prefixed) do not need it. `mb-proto` exposes this as `ModbusRequest::Custom { expect_len }`.

`Cargo.toml` for `mb-proto`: `tokio-modbus = { version = "0.17", default-features = false, features = ["rtu", "tcp"] }`, `tokio-serial = "5.4"`, `tokio = { features = ["net","time","io-util"] }`, `bytes`, `thiserror`, `mb-types`, `gateway-config`.

---

## 4. Scheduler & Channel Model (`mb-poller`)

**One tokio task per physical channel.** N channels → N tasks, independent of the 5000 tags. Ownership (not a lock) provides the half-duplex-RTU "one in flight" invariant.

```rust
// ===================== mb-poller/src/lib.rs =====================
use std::{collections::HashMap, sync::Arc};
use tokio::{sync::{mpsc, watch}, task::JoinHandle};
use mb_types::ChannelId;
use crate::{cache::RawValueSink, command::WriteCommand, plan::ChannelPlan};

pub struct PollerHandle {
    writers: HashMap<ChannelId, mpsc::Sender<WriteCommand>>, // OPC UA -> channel task
    shutdown: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}
impl PollerHandle {
    pub fn writer(&self, ch: ChannelId) -> Option<mpsc::Sender<WriteCommand>> { self.writers.get(&ch).cloned() }
    pub async fn shutdown(self) { let _ = self.shutdown.send(true); for t in self.tasks { let _ = t.await; } }
}

pub struct Poller;
impl Poller {
    pub fn spawn(cfg: gateway_config::ResolvedConfig, sink: Arc<dyn RawValueSink>) -> PollerHandle {
        let plans: Vec<ChannelPlan> = crate::plan::build_all(&cfg);   // compile-time coalescing
        let (sd_tx, sd_rx) = watch::channel(false);
        let mut writers = HashMap::new();
        let mut tasks = Vec::new();
        for plan in plans {
            let (w_tx, w_rx) = mpsc::channel::<WriteCommand>(64);
            writers.insert(plan.id, w_tx);
            let sink = Arc::clone(&sink);
            let sd = sd_rx.clone();
            tasks.push(tokio::spawn(crate::channel::run_channel(plan, sink, w_rx, sd)));
        }
        PollerHandle { writers, shutdown: sd_tx, tasks }
    }
}
```

```rust
// ===================== mb-poller/src/plan.rs =====================
use mb_types::{ChannelId, DeviceId, PollGroupId, TagId, DataType, WordOrder, ByteOrder};
use mb_proto::ModbusRequest;
use std::time::Duration;

pub struct ChannelPlan {
    pub id: ChannelId,
    pub transport: gateway_config::schema::v1::TransportConfig,
    pub request_timeout: Duration,
    pub inter_request_delay: Duration,
    pub max_inflight: usize,           // resolved: 1 for RTU/RtuOverTcp
    pub retry: gateway_config::schema::v1::RetryConfig,
    pub groups: Vec<(PollGroupId, Duration)>,
    pub devices: Vec<DevicePlan>,
}
pub struct DevicePlan {
    pub id: DeviceId,
    pub unit: u8,
    pub offline_after: u32,
    pub all_tags: Vec<TagId>,          // for the watchdog bulk-Bad sweep
    pub by_group: Vec<(PollGroupId, Vec<Transaction>)>, // COALESCED at build time
}
/// A compiled wire request plus the map back into cache slots.
pub struct Transaction {
    pub req: ModbusRequest,
    pub base: u16,
    pub fields: Vec<Field>,            // scatter targets
    pub coalesced: bool,               // true if it merged >1 entry (used by de-coalescing)
}
pub struct Field {
    pub tag: TagId,
    pub word_offset: u16,              // slice start within the response
    pub word_len: u16,
    pub data_type: DataType,
    pub word_order: WordOrder,
    pub byte_order: ByteOrder,
    pub bit: Option<u8>,
}
```

```rust
// ===================== mb-poller/src/channel.rs =====================
use std::{sync::Arc, time::Duration};
use tokio::sync::{mpsc, watch};
use mb_proto::{Transport, ProtoError};
use crate::{cache::RawValueSink, command::WriteCommand, plan::ChannelPlan,
            schedule::PollWheel, device::DeviceRuntime};

pub async fn run_channel(
    plan: ChannelPlan,
    sink: Arc<dyn RawValueSink>,
    mut writes: mpsc::Receiver<WriteCommand>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut rt: Vec<DeviceRuntime> = plan.devices.iter()
        .map(|d| DeviceRuntime::new(d.offline_after, plan.retry)).collect();
    let mut wheel = PollWheel::new(&plan);   // one Interval per period; MissedTickBehavior::Skip

    'reconnect: loop {
        // ---- connect / backoff / whole-channel watchdog ----
        let mut tx = match Transport::connect(&plan.transport).await {
            Ok(t) => t,
            Err(e) => {
                // Whole channel down: mark every tag of every device Bad AND notify subscribers.
                for d in &plan.devices { sink.set_device_quality(&d.all_tags, crate::cache::Quality::Bad); }
                if channel_backoff_or_stop(&plan, &mut shutdown).await { return }
                let _ = &e; continue 'reconnect;
            }
        };
        for r in &mut rt { r.on_connected(); }

        // ---- poll + write loop (sequential for RTU, bounded for TCP) ----
        loop {
            tokio::select! {
                _ = shutdown.changed() => { let _ = tx.ctx_disconnect().await; return; }

                // Writes get priority: serviced at a select! boundary, i.e. only between
                // whole transactions, never mid-coalesced-burst -> no RS-485 frame collision.
                Some(cmd) = writes.recv() => {
                    let dev = &plan.devices[cmd.device_idx];
                    let res = tx.request(dev.unit, &cmd.req, plan.request_timeout).await;
                    if let Err(ref e) = res { if e.is_fatal() { let _ = cmd.reply.send(res); continue 'reconnect; } }
                    let _ = cmd.reply.send(res);
                }

                due = wheel.next_due() => {
                    for (dev_idx, group_idx) in due {   // items run sequentially within a tick
                        if rt[dev_idx].is_offline_and_not_due_to_probe() { continue; } // don't burn a timeout on a dead slave
                        let dev = &plan.devices[dev_idx];
                        for txn in &dev.by_group[group_idx].1 {
                            match poll_with_retry(&mut tx, dev.unit, txn, &plan, &mut rt[dev_idx]).await {
                                Ok(resp) => { rt[dev_idx].on_success(); scatter(&*sink, txn, resp); }
                                Err(e) if e.is_fatal() => {
                                    if rt[dev_idx].on_comm_failure() { // crossed offline threshold
                                        sink.set_device_quality(&dev.all_tags, crate::cache::Quality::Bad);
                                    }
                                    continue 'reconnect;      // drop conn, reconnect w/ backoff
                                }
                                Err(ProtoError::Exception(_)) => {
                                    // Device alive; only THIS txn's tags degrade. On a coalesced
                                    // txn, split-and-remember (see device.rs / §5 de-coalescing).
                                    crate::device::handle_exception(&mut rt[dev_idx], dev, txn, &*sink);
                                }
                                Err(_) => { // Timeout: device may be flaky, not yet offline
                                    if rt[dev_idx].on_comm_failure() {
                                        sink.set_device_quality(&dev.all_tags, crate::cache::Quality::Bad);
                                    } else {
                                        sink.degrade_uncertain(&txn_tags(txn)); // stale-but-maybe-recovering
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
```

**TCP bounded concurrency (correctness fix, all scale/extensibility panels).** A single `tokio_modbus::Context` is `&mut self` per call and **not** `Clone`; you cannot pipeline concurrent requests to different `unit_id`s over one `Context` (`set_slave` + `call` are not atomic). Therefore:

- `max_inflight == 1` (the default, and forced for RTU/RtuOverTcp): the loop above, sequential.
- `max_inflight > 1` on **TCP only**: the channel task opens **N independent `Context`s** (N sockets), one per worker, governed by a `tokio::sync::Semaphore(N)`, and dispatches due transactions across them via `FuturesUnordered`. Each `Context` is used strictly sequentially by its worker. This is the *only* correct way to get parallel TCP reads; it is a localized addition inside `run_channel` and does not change the task-per-channel model.

**Scheduler timing.** `PollWheel` holds one `tokio::time::Interval` per distinct period on the channel (typically 2–4), each set to `MissedTickBehavior::Skip` so an overrun on a slow bus does not accumulate a catch-up burst. `next_due()` `select!`s across the intervals and returns the `(device_idx, group_idx)` batch due now, ordered by group `priority`. Offline devices are skipped except on their probe cadence (`max_backoff`), so a dead slave cannot re-enqueue a 200 ms group and burn a full timeout every tick, starving healthy slaves on the same RTU bus.

**Retry vs backoff (two layers).**
- *Per-request retry* (`max_retries`): `Timeout`/`Exception` on one transaction → retry a couple of times on the same connection inside `poll_with_retry`.
- *Reconnect backoff* (exponential, jittered, capped at `max_backoff_ms`): any fatal error (`Io`/`Protocol`/`NotConnected`) → drop the `Transport`, sleep, `continue 'reconnect`.

---

## 5. Coalescing (`mb-poller/src/coalesce.rs`)

Pure, startup-time, per `(device, poll_group, area)`. Runtime never re-coalesces (except adaptive de-coalescing, below). Address-space units are handled per `Area` — coil-space for FC01/02, register-space for FC03/04.

```rust
pub struct Caps { pub max_gap: u16 }   // per-device-resolved; area max comes from Area::max_qty()

struct Interval { start: u16, end: u16, field: crate::plan::Field } // [start,end) in area units

pub fn coalesce(area: mb_types::Area, mut ivals: Vec<Interval>, caps: Caps)
    -> Vec<crate::plan::Transaction>
{
    ivals.sort_by_key(|i| i.start);
    let area_max = area.max_qty();
    let mut out = Vec::new();
    let mut run: Option<(u16 /*start*/, u16 /*end*/, Vec<crate::plan::Field>)> = None;

    for iv in ivals {
        match &mut run {
            Some((start, end, fields))
                // bridge only small holes, never exceed the PDU cap for THIS area
                if iv.start.saturating_sub(*end) <= caps.max_gap
                && iv.end.saturating_sub(*start) <= area_max =>
            {
                *end = (*end).max(iv.end);
                let mut f = iv.field; f.word_offset = iv.start - *start;
                fields.push(f);
            }
            _ => {
                if let Some((s, e, fields)) = run.take() { out.push(emit(area, s, e, fields)); }
                let f0 = crate::plan::Field { word_offset: 0, ..iv.field };
                run = Some((iv.start, iv.end, vec![f0]));
            }
        }
    }
    if let Some((s, e, fields)) = run.take() { out.push(emit(area, s, e, fields)); }
    out
}

fn emit(area: mb_types::Area, start: u16, end: u16, fields: Vec<crate::plan::Field>)
    -> crate::plan::Transaction
{
    let qty = end - start;
    let req = match area {
        mb_types::Area::Coils          => mb_proto::ModbusRequest::ReadCoils { addr: start, qty },
        mb_types::Area::DiscreteInputs => mb_proto::ModbusRequest::ReadDiscreteInputs { addr: start, qty },
        mb_types::Area::Holding        => mb_proto::ModbusRequest::ReadHoldingRegisters { addr: start, qty },
        mb_types::Area::Input          => mb_proto::ModbusRequest::ReadInputRegisters { addr: start, qty },
    };
    let coalesced = fields.len() > 1;
    crate::plan::Transaction { req, base: start, fields, coalesced }
}
```

**Rules & boundary conditions.**
1. Bucket by `(device, poll_group, area)` — a Modbus PDU carries one function; different areas/periods never merge. FC03 and FC04 stay separate.
2. Expand each entry to an interval in the **area's own units**: register span via `DataType::register_count()` (or `length` for `ascii`/`bcd`); coil-space width 1 for FC01/02 (`Bit`).
3. `max_gap` (per-device-resolved, **default 0** = never bridge) tolerates small holes; a device that rejects undefined addresses sets `max_gap: 0`.
4. Cap runs at `Area::max_qty()` — 125 registers or 2000 coils.
5. Each `Field` carries `word_offset = entry.addr − run.start`, so gap bytes are read-and-discarded without harming decode.
6. **Custom** and **write** entries never coalesce.

**Adaptive de-coalescing (correctness must-fix).** If a *coalesced* transaction returns `Exception::IllegalDataAddress`, `device.rs` splits it into per-field sub-transactions and remembers the split in `DeviceRuntime` (replaces the offending `Transaction` in the live plan for that device). This prevents one bad/absent bridged register from stranding a whole tag group as Bad forever, and it only triggers when gap-bridging is enabled.

At scale: 40 tags across holding regs 100..180 on one device collapse to 1–2 transactions instead of 40. Wire-request count tracks address-space density, not tag count.

---

## 6. Tag Cache (`mb-poller/src/cache.rs`)

Flat `Box<[TagSlot]>` indexed by dense `TagId` — no hashing, no shard lock, cache-friendly, and single-writer-per-slot **by construction** (each channel owns an exclusive `TagId` range, assigned in `resolve.rs`). The poller writes *raw* words/bits; `tags-core` decodes later.

```rust
use std::{sync::Arc, time::{Instant, SystemTime}};
use mb_types::TagId;
use tokio::sync::broadcast;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Quality { Good, Uncertain, Bad }   // -> OPC UA StatusCode later

/// RAW payload before decode. Arc so a read clones a pointer, not the data.
#[derive(Clone, Debug)]
pub enum RawValue {
    Bits(Arc<[bool]>),
    Registers(Arc<[u16]>),
    Raw(bytes::Bytes),
    Absent,                     // never-yet-read
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub value: RawValue,
    pub quality: Quality,
    pub source_ts: SystemTime,  // for OPC UA SourceTimestamp (presentation only)
    pub mono: Instant,          // monotonic; staleness math uses THIS, never SystemTime
    pub seq: u64,               // bumped per write, for change detection
}

/// One slot per tag. parking_lot::RwLock: sub-µs, no poisoning, no async.
/// INVARIANT: the write guard is a pure field assignment and is NEVER held across .await.
/// (Enforced by review + a clippy lint; guard is dropped before any await in scatter().)
pub struct TagSlot { inner: parking_lot::RwLock<Snapshot> }

pub struct TagCache {
    slots: Box<[TagSlot]>,                       // len = tag count, index = TagId.0
    by_name: dashmap::DashMap<Box<str>, TagId>,  // cold path: OPC UA node-id -> TagId at subscribe
    changes: broadcast::Sender<ChangeBatch>,     // coalesced, not per-tag (see below)
}

/// Change notification is BATCHED PER TRANSACTION, not per tag. At 5000 tags x 5 Hz the
/// per-tag firehose (25k events/s) chronically lags subscribers; one batch per coalesced
/// read keeps event volume at ~request-rate. Lossy on Lagged -> subscriber does a full
/// snapshot_all() resync; never back-pressures the poll loop.
#[derive(Clone, Debug)]
pub struct ChangeBatch { pub tags: Arc<[TagId]>, pub seq: u64 }

/// Poller-facing seam. tags-core / OPC UA / MQTT implement or wrap this.
pub trait RawValueSink: Send + Sync {
    /// Write one coalesced read's worth of tags in a single call.
    fn publish_batch(&self, updates: &[(TagId, RawValue)], ts: SystemTime, mono: Instant);
    /// Watchdog: flip many tags Bad, keep last value, AND emit a change batch.
    fn set_device_quality(&self, tags: &[TagId], q: Quality);
    /// Transient single-read miss while device still online.
    fn degrade_uncertain(&self, tags: &[TagId]);
}

/// Consumer-facing seam.
pub trait CacheReader: Send + Sync {
    fn snapshot(&self, tag: TagId) -> Option<Snapshot>;
    fn resolve(&self, name: &str) -> Option<TagId>;
    fn subscribe(&self) -> broadcast::Receiver<ChangeBatch>;
    fn snapshot_all(&self) -> Vec<(TagId, Snapshot)>;
}

impl RawValueSink for TagCache {
    fn publish_batch(&self, updates: &[(TagId, RawValue)], ts: SystemTime, mono: Instant) {
        let mut touched = Vec::with_capacity(updates.len());
        for (tag, val) in updates {
            let mut g = self.slots[tag.0 as usize].inner.write();
            g.value = val.clone(); g.quality = Quality::Good;
            g.source_ts = ts; g.mono = mono; g.seq += 1;   // pure assignment, no await
            touched.push(*tag);                              // guard dropped at loop end
        }
        let _ = self.changes.send(ChangeBatch { tags: touched.into(), seq: 0 });
    }
    fn set_device_quality(&self, tags: &[TagId], q: Quality) {
        for t in tags { let mut g = self.slots[t.0 as usize].inner.write(); g.quality = q; g.mono = Instant::now(); }
        let _ = self.changes.send(ChangeBatch { tags: Arc::from(tags), seq: 0 }); // watchdog MUST notify
    }
    fn degrade_uncertain(&self, tags: &[TagId]) {
        for t in tags { let mut g = self.slots[t.0 as usize].inner.write(); g.quality = Quality::Uncertain; }
        let _ = self.changes.send(ChangeBatch { tags: Arc::from(tags), seq: 0 });
    }
}
```

**Boundary to `tags-core`.** Phase 2 subscribes to `ChangeBatch`, reads `Snapshot.value` via `snapshot()`, applies `DataType`/word+byte order/`scale`/`offset`/formula (metadata it gets from the resolved config, not from the cache), and writes into its own typed store. The poller never imports `tags-core`. Storing raw (not decoded) means a formula edit re-decodes with no re-poll.

**Staleness.** Computed as `now_instant − Snapshot.mono` (monotonic, immune to NTP steps). `source_ts` is a `SystemTime` captured at the same point, used only to present OPC UA `SourceTimestamp`. Last-good value is retained on comms loss (SCADA convention: show last value + Bad quality, never a fake 0).

**ASCII/Raw bound.** `validate.rs` caps `length`/`custom_response_len` (default max 125 registers / 250 bytes) so a `Snapshot` clone on the read path stays cheap.

---

## 7. Error → Quality Mapping & Validation

### Per-request outcome → quality
| Outcome | Device online? | Quality of affected tags | Action |
|---|---|---|---|
| `Ok` | yes | `Good`, value + timestamps updated | reset failure counter |
| `Exception(IllegalDataAddress/Function/Value)` on a *simple* txn | **stays online** (slave answered) | `Bad` (tag-scoped), last value kept | log once; consider disabling the item after repeats |
| `Exception(IllegalDataAddress)` on a *coalesced* txn | stays online | — | **adaptive de-coalesce** (split & remember), retry sub-txns |
| `Exception(ServerDeviceBusy/GatewayTargetDeviceFailedToRespond)` | stays online | last value kept | transient → per-request retry/backoff |
| `Timeout` | maybe | `Uncertain` (below threshold) → `Bad` (at threshold) | increment failure counter; **drain/reset stream on timeout** (see below) |
| `Io`/`Protocol(HeaderMismatch/FunctionCodeMismatch)`/`NotConnected` | comms loss | `Bad` | fatal → drop connection, reconnect w/ backoff |

**Timeout stream-drain rule.** On a byte-stream transport (RTU / RTU-over-TCP), a `tokio::time::timeout` that fires drops the in-flight future mid-exchange; unread reply bytes would desync the next request. `tokio-modbus`'s `ClientCodec` has a 20-retry byte-dropping resync (verified `src/codec/rtu.rs:292`) that recovers on the *next* decode, but to be safe the channel treats a **second consecutive timeout as fatal** (`continue 'reconnect`), forcing a clean reconnect rather than relying on resync indefinitely.

**Offline watchdog (per device, counter-based, no extra task).** `DeviceRuntime { fails, online, backoff, split_overrides }`. `on_comm_failure()` increments `fails`; when `fails >= offline_after && online`, flip `online=false` and return `true` → the channel calls `sink.set_device_quality(&all_tags, Bad)` in one sweep (which **also emits a change batch**, so OPC UA/MQTT learn immediately). `on_success()` resets `fails` and flips back online. Whole-channel connect failure marks every device's tags Bad the same way.

### `validate.rs` rules (aggregated `Vec<ConfigError>`, jsonschema + semantic)
1. Unique channel/device/`poll_group`/tag ids.
2. **Every `TagId` is fed by exactly one `RegisterEntry`** — a tag mapped from two entries (esp. on two channels) is rejected. This is the hard correctness gate that makes single-writer-per-slot safe; treat a gap here as a data-race, not a warning.
3. `poll_group` FK on each register resolves.
4. `function` compatible with `data_type` (bit-domain types only on FC01/02; word types on FC03/04).
5. `qty`/`length` within Modbus area caps (125 reg / 2000 coil).
6. `Custom` reads on RTU/RtuOverTcp **require** `custom_response_len`.
7. Transport fields present (`Rtu` has `path`+`baud`; `Tcp`/`RtuOverTcp` have `host`+`port`).
8. `max_inflight > 1` only allowed on `Tcp`.
9. `length`/`custom_response_len` within the ASCII/Raw cap (§6).
10. **Static bus-load check (warn/refuse).** For each RTU/RtuOverTcp channel, estimate airtime = Σ over coalesced transactions of `(frame_bytes × 11 bits/char / baud + inter_request_delay + request_timeout_margin)`, summed per fastest group period. If required throughput exceeds the baud budget, emit a hard warning (configurable to refuse). This is the difference between a config that can physically meet its periods and one that runs permanently overdue.

### Metrics (`metrics.rs`) — plain `AtomicU64`, single-writer (owning task)
Per channel: `reqs_ok, reqs_err, timeouts, exceptions, protocol_errors, reconnects, bytes_rx, roundtrip_us_ewma, bus_busy_ratio_ppm`. Per device: `reqs_ok, reqs_err, consecutive_failures, online(AtomicBool), last_ok_unix_ms, backoff_ms`. Read via relaxed loads by a future diagnostics endpoint; `bus_busy_ratio` is the operator's saturation signal.

---

## 8. Write Path (`mb-poller/src/command.rs`)

Writes (FC05/06/15/16 + Custom) are a Phase-1 requirement and must serialize with reads on a half-duplex bus. They flow **into** the owning channel task over a per-channel `mpsc`, drained at a `select!` boundary — i.e. only *between* whole transactions, never inside a coalesced read burst, so two frames can never collide on RS-485.

```rust
use tokio::sync::oneshot;
use mb_proto::{ModbusRequest, ModbusResponse, ProtoError};

pub struct WriteCommand {
    pub device_idx: usize,                 // resolved index into the channel's DevicePlan vec
    pub req: ModbusRequest,                // WriteSingle*/WriteMultiple*/Custom
    pub reply: oneshot::Sender<Result<ModbusResponse, ProtoError>>,
}
```

Bounded `mpsc(64)` provides natural backpressure; the OPC UA layer gets a `oneshot` result per write. Writes are serviced with priority (their `select!` arm) so operator control actions aren't delayed behind slow polls, but they still respect the one-in-flight bus invariant.

---

## 9. Open Questions Deferred to Phase 2 (explicitly out of Phase-1 scope)

- **Deadband / on-demand polling** ("poll only what an OPC UA client subscribes to"): the batched `ChangeBatch` and the `RawValueSink` seam are designed so `tags-core` can gate republish per active subscription without the poller changing. Runtime enable/disable of a whole poll group is a **restart-only** operation in Phase 1 (see below).
- **Hot-reload**: Phase 1 is **load-once / restart-only**, decided explicitly. The dense-`TagId` flat-slot cache and any live OPC UA subscriptions bound to `TagId` indices cannot survive a reload that renumbers ids without a stable-id indirection layer; that indirection is a Phase-2 design item. On config change, the supervisor restarts the `Poller`. This is signed off as acceptable for Phase 1 and is what dictates the cache type `tags-core` inherits.
- **Config authoring format**: JSON is the wire format (serde_json). TOML/YAML/CSV-import are cosmetic additions later (serde makes them cheap) and do not affect the model.

---

## 10. Implementation Order (compiles incrementally)

1. **`mb-types`** — enums, newtypes, `register_count`/`Area`. No deps; compiles standalone, fully unit-tested.
2. **`gateway-config`** — `schema/v1.rs` + `ConfigFile` (round-trip a sample JSON in a test), then `validate.rs` (all §7 rules incl. single-writer + bus-budget), then `resolve.rs` (interning + `TagId` range assignment). Depends only on `mb-types`.
3. **`mb-proto` error + request** — `ProtoError`, `flatten()`, `ModbusRequest`/`ModbusResponse`, `to_tokio_request()`. Unit-test `flatten` against constructed `tokio_modbus::Result` values.
4. **`mb-proto` transport** — `connect.rs` (all three via `attach_slave`) + `Transport::request`. Integration-test TCP against a `tokio-modbus` server example and RTU-over-TCP against a serial-tunnel simulator early (highest residual risk is Custom framing, not standard FCs).
5. **`mb-poller` cache** — `TagCache`, `RawValueSink`/`CacheReader`, `Snapshot`. Pure, testable with a fake sink; unblocks the OPC UA/MQTT teams immediately.
6. **`mb-poller` coalesce + plan** — `coalesce()` (pure, table-driven unit tests: gaps, caps, area separation, multi-word spans) then `build_all`.
7. **`mb-poller` device + schedule** — `DeviceRuntime` (retry/backoff/watchdog/de-coalesce state machine, unit-tested) and `PollWheel` (with `tokio::time::pause()` deterministic clock tests).
8. **`mb-poller` channel + command + lib** — `run_channel` wiring it all together against a **mock `Transport`** and paused clock (deterministic scheduler test), then `Poller::spawn`. `command.rs` write path last.
9. **End-to-end** — one RTU channel with 2 devices + one TCP channel, fast/slow groups, against simulators; verify coalescing counts, watchdog Bad propagation, reconnect backoff, and the static bus-load warning.