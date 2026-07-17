//! OPC UA Write -> Modbus write-back (phase 4).
//!
//! Per writable tag a [`WritePlan`] is compiled at server start (inverse
//! formula parse errors fail the boot). The `SimpleNodeManager` write
//! callback is synchronous, so the Modbus round-trip is awaited via
//! `block_in_place` — the OPC UA client receives the REAL outcome: `Good`
//! only after the device acknowledged the write.
//!
//! The address-space value is deliberately NOT updated here: confirmation
//! arrives through the normal poll cycle (read-back), the SCADA convention.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use opcua::types::{DataValue, StatusCode, Variant};
use tokio::sync::{mpsc, oneshot};

use gateway_config::ResolvedConfig;
use mb_proto::{ModbusRequest, ProtoError};
use mb_poller::WriteCommand;
use mb_types::{ChannelId, DataType, DeviceId, FunctionCode};
use tags_core::{DecodeMeta, Formula, FormulaError, TypedValue};

/// Queue margin on top of the device-resolved worst-case round-trip (B1):
/// time the command may spend waiting behind poll transactions and other
/// writes before the channel task picks it up.
const WRITE_QUEUE_MARGIN: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum WritePlanError {
    #[error("tag `{tag}`: write_formula: {source}")]
    Formula {
        tag: String,
        #[source]
        source: FormulaError,
    },
    #[error("tag `{tag}`: no write sender for its channel (poller not running?)")]
    NoWriter { tag: String },
}

/// Everything needed to turn one engineering value into one Modbus write.
pub struct WritePlan {
    device: DeviceId,
    address: u16,
    write_fn: WriteFn,
    decode: DecodeMeta,
    inverse: Inverse,
    sender: mpsc::Sender<WriteCommand>,
    /// How long one write may take end to end (B1): the device-resolved
    /// worst case `request_timeout * (1 + max_retries)` plus
    /// [`WRITE_QUEUE_MARGIN`]. The OPC UA callback stops waiting after this,
    /// and the SAME instant is the command's deadline — the poller can never
    /// execute a write whose client already got BadTimeout.
    wait: Duration,
}

/// The concrete Modbus write function this plan emits, resolved from the read
/// function and the optional `write_function` override at build time.
enum WriteFn {
    /// FC05 — one coil.
    SingleCoil,
    /// FC15 — the coil written inside a multi-write frame (some PLCs require it).
    MultipleCoils,
    /// FC06 — one holding register (single-word types only, validated).
    SingleRegister,
    /// FC16 — forced multi-register write (even for a single word).
    MultipleRegisters,
    /// FC06 for a single-word value, FC16 for multi-word — the natural default.
    AutoRegisters,
}

enum Inverse {
    Linear { scale: f64, offset: f64 },
    Expr(Formula),
    None,
}

/// Writable tag name -> plan. Built once at server start.
pub fn build_write_plans(
    cfg: &ResolvedConfig,
    writers: &HashMap<ChannelId, mpsc::Sender<WriteCommand>>,
) -> Result<HashMap<String, WritePlan>, WritePlanError> {
    let mut plans = HashMap::new();
    for ch in cfg.channels.iter().filter(|c| c.enabled) {
        for dev in ch.devices.iter().filter(|d| d.enabled) {
            // B1: the wait budget follows the device-resolved knobs, not a
            // flat constant — a slow device (big timeout x retries) must not
            // time out at the OPC UA layer while the poller is still
            // legitimately working on it.
            let worst_case_ms = dev
                .request_timeout_ms
                .saturating_mul(u64::from(dev.retry.max_retries) + 1);
            let wait = Duration::from_millis(worst_case_ms) + WRITE_QUEUE_MARGIN;
            for reg in dev.registers.iter().filter(|r| r.writable) {
                let name = cfg.tag_name(reg.tag).expect("resolved tag has a name");
                let sender = writers
                    .get(&ch.id)
                    .cloned()
                    .ok_or_else(|| WritePlanError::NoWriter { tag: name.to_string() })?;
                let inverse = match &reg.write_formula {
                    Some(text) => Inverse::Expr(
                        Formula::compile(text, "value", None).map_err(|source| {
                            WritePlanError::Formula { tag: name.to_string(), source }
                        })?,
                    ),
                    None if reg.scale != 1.0 || reg.offset != 0.0 => Inverse::Linear {
                        scale: reg.scale,
                        offset: reg.offset,
                    },
                    None => Inverse::None,
                };
                // Resolve the concrete write FC from the read function and the
                // optional override (validated: coil source -> coil write FC,
                // holding source -> register write FC).
                let write_fn = match (reg.function, reg.write_function) {
                    (FunctionCode::ReadCoils, Some(FunctionCode::WriteMultipleCoils)) => {
                        WriteFn::MultipleCoils
                    }
                    (FunctionCode::ReadCoils, _) => WriteFn::SingleCoil,
                    (_, Some(FunctionCode::WriteSingleRegister)) => WriteFn::SingleRegister,
                    (_, Some(FunctionCode::WriteMultipleRegisters)) => WriteFn::MultipleRegisters,
                    (_, _) => WriteFn::AutoRegisters, // holding registers (validated)
                };
                plans.insert(
                    name.to_string(),
                    WritePlan {
                        device: dev.id,
                        address: reg.address,
                        write_fn,
                        decode: DecodeMeta {
                            data_type: reg.data_type,
                            word_order: reg.word_order,
                            byte_order: reg.byte_order,
                            bit: reg.bit,
                        },
                        inverse,
                        sender,
                        wait,
                    },
                );
            }
        }
    }
    Ok(plans)
}

impl WritePlan {
    /// Execute one OPC UA write. Synchronous by contract of the node-manager
    /// callback; internally drives the async Modbus round-trip.
    pub fn execute(&self, dv: DataValue) -> StatusCode {
        let Some(variant) = dv.value else {
            return StatusCode::BadNothingToDo;
        };
        let req = match self.to_request(&variant) {
            Ok(req) => req,
            Err(status) => return status,
        };

        // B1: ONE instant rules both sides — we stop waiting at `deadline`,
        // and the channel task drops (never executes) any command it dequeues
        // after that same `deadline`. No window remains where the client saw
        // BadTimeout but the device still gets the write later.
        let deadline = Instant::now() + self.wait;
        let (reply_tx, reply_rx) = oneshot::channel();
        let cmd = WriteCommand {
            device: self.device,
            req,
            reply: reply_tx,
            deadline,
        };
        let sender = self.sender.clone();

        // The callback runs on a multi-thread tokio runtime worker; move it
        // to the blocking pool while we drive the round-trip to completion.
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                if sender.send(cmd).await.is_err() {
                    return Err(None); // channel task gone
                }
                let deadline = tokio::time::Instant::from_std(deadline);
                match tokio::time::timeout_at(deadline, reply_rx).await {
                    Err(_elapsed) => Err(Some(ProtoError::Timeout)),
                    Ok(Err(_closed)) => Err(None),
                    Ok(Ok(res)) => Ok(res),
                }
            })
        });

        match result {
            Ok(Ok(_ack)) => StatusCode::Good,
            Ok(Err(e)) => proto_error_status(&e),
            Err(Some(e)) => proto_error_status(&e),
            Err(None) => StatusCode::BadNoCommunication,
        }
    }

    /// Engineering `Variant` -> inverse transform -> encoded Modbus request.
    fn to_request(&self, variant: &Variant) -> Result<ModbusRequest, StatusCode> {
        match self.write_fn {
            WriteFn::SingleCoil | WriteFn::MultipleCoils => {
                let on = match variant {
                    Variant::Boolean(b) => *b,
                    Variant::Byte(v) => *v != 0,
                    Variant::Int16(v) => *v != 0,
                    Variant::Int32(v) => *v != 0,
                    Variant::Int64(v) => *v != 0,
                    Variant::UInt16(v) => *v != 0,
                    Variant::UInt32(v) => *v != 0,
                    Variant::UInt64(v) => *v != 0,
                    _ => return Err(StatusCode::BadTypeMismatch),
                };
                Ok(match self.write_fn {
                    // FC15: the single coil carried in a multi-write frame.
                    WriteFn::MultipleCoils => ModbusRequest::WriteMultipleCoils {
                        addr: self.address,
                        values: vec![on],
                    },
                    // FC05.
                    _ => ModbusRequest::WriteSingleCoil { addr: self.address, value: on },
                })
            }
            WriteFn::SingleRegister | WriteFn::MultipleRegisters | WriteFn::AutoRegisters => {
                let engineering = variant_to_f64(variant).ok_or(StatusCode::BadTypeMismatch)?;
                let raw = match &self.inverse {
                    Inverse::None => engineering,
                    Inverse::Linear { scale, offset } => (engineering - offset) / scale,
                    Inverse::Expr(f) => f
                        .eval("value", engineering)
                        .map_err(|_| StatusCode::BadInvalidArgument)?,
                };
                let typed = raw_to_typed(raw, self.decode.data_type)
                    .ok_or(StatusCode::BadOutOfRange)?;
                let words = tags_core::encode(&typed, self.decode)
                    .map_err(|_| StatusCode::BadTypeMismatch)?;
                Ok(match self.write_fn {
                    // FC06 forced: validation guarantees a single word; guard anyway.
                    WriteFn::SingleRegister => {
                        if words.len() != 1 {
                            return Err(StatusCode::BadTypeMismatch);
                        }
                        ModbusRequest::WriteSingleRegister {
                            addr: self.address,
                            value: words[0],
                        }
                    }
                    // FC16 forced, even for a single word.
                    WriteFn::MultipleRegisters => ModbusRequest::WriteMultipleRegisters {
                        addr: self.address,
                        values: words,
                    },
                    // Natural default: FC06 for one word, FC16 for many.
                    _ => match words.len() {
                        1 => ModbusRequest::WriteSingleRegister {
                            addr: self.address,
                            value: words[0],
                        },
                        _ => ModbusRequest::WriteMultipleRegisters {
                            addr: self.address,
                            values: words,
                        },
                    },
                })
            }
        }
    }
}

fn variant_to_f64(v: &Variant) -> Option<f64> {
    match v {
        Variant::Boolean(b) => Some(f64::from(u8::from(*b))),
        Variant::SByte(v) => Some(f64::from(*v)),
        Variant::Byte(v) => Some(f64::from(*v)),
        Variant::Int16(v) => Some(f64::from(*v)),
        Variant::UInt16(v) => Some(f64::from(*v)),
        Variant::Int32(v) => Some(f64::from(*v)),
        Variant::UInt32(v) => Some(f64::from(*v)),
        Variant::Int64(v) => Some(*v as f64),
        Variant::UInt64(v) => Some(*v as f64),
        Variant::Float(v) => Some(f64::from(*v)),
        Variant::Double(v) => Some(*v),
        _ => None,
    }
}

/// Round the inverse-transformed raw to the tag's native register type,
/// rejecting NaN/inf and out-of-range values instead of wrapping.
fn raw_to_typed(raw: f64, dt: DataType) -> Option<TypedValue> {
    if !raw.is_finite() {
        return None;
    }
    let rounded = raw.round();
    match dt {
        DataType::F32 | DataType::F64 => Some(TypedValue::Float(raw)),
        DataType::U16 | DataType::U32 | DataType::U64 => {
            if rounded < 0.0 || rounded > u64::MAX as f64 {
                return None;
            }
            Some(TypedValue::UInt(rounded as u64))
        }
        DataType::I16 | DataType::I32 | DataType::I64 => {
            if rounded < i64::MIN as f64 || rounded > i64::MAX as f64 {
                return None;
            }
            Some(TypedValue::Int(rounded as i64))
        }
        // Coils are handled by WriteKind::Coil; the rest is rejected by
        // validation before we ever get here.
        _ => None,
    }
}

fn proto_error_status(e: &ProtoError) -> StatusCode {
    match e {
        ProtoError::Timeout => StatusCode::BadTimeout,
        ProtoError::NotConnected => StatusCode::BadNoCommunication,
        ProtoError::Exception(_) => StatusCode::BadDeviceFailure,
        _ => StatusCode::BadCommunicationError,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mb_types::{ByteOrder, WordOrder};

    fn plan(dt: DataType, inverse: Inverse) -> WritePlan {
        let (tx, _rx) = mpsc::channel(1);
        WritePlan {
            device: DeviceId(0),
            address: 10,
            write_fn: WriteFn::AutoRegisters,
            decode: DecodeMeta {
                data_type: dt,
                word_order: WordOrder::BigEndian,
                byte_order: ByteOrder::BigEndian,
                bit: None,
            },
            inverse,
            sender: tx,
            wait: Duration::from_secs(10),
        }
    }

    #[test]
    fn linear_inverse_and_encode() {
        // temp degC 23.7 with scale 0.1 -> raw 237.
        let p = plan(DataType::I16, Inverse::Linear { scale: 0.1, offset: 0.0 });
        match p.to_request(&Variant::Double(23.7)).unwrap() {
            ModbusRequest::WriteSingleRegister { addr, value } => {
                assert_eq!((addr, value), (10, 237));
            }
            other => panic!("expected single-register write, got {other:?}"),
        }
    }

    #[test]
    fn expr_inverse_and_multiword_encode() {
        let f = Formula::compile("value / 60", "value", None).unwrap();
        let p = plan(DataType::U32, Inverse::Expr(f));
        match p.to_request(&Variant::Double(90000.0)).unwrap() {
            ModbusRequest::WriteMultipleRegisters { addr, values } => {
                assert_eq!(addr, 10);
                assert_eq!(values, vec![0x0000, 1500]);
            }
            other => panic!("expected multi-register write, got {other:?}"),
        }
    }

    #[test]
    fn coil_write_accepts_bool_and_ints() {
        let (tx, _rx) = mpsc::channel(1);
        let p = WritePlan {
            device: DeviceId(0),
            address: 3,
            write_fn: WriteFn::SingleCoil,
            decode: DecodeMeta {
                data_type: DataType::Bit,
                word_order: WordOrder::BigEndian,
                byte_order: ByteOrder::BigEndian,
                bit: None,
            },
            inverse: Inverse::None,
            sender: tx,
            wait: Duration::from_secs(10),
        };
        assert!(matches!(
            p.to_request(&Variant::Boolean(true)).unwrap(),
            ModbusRequest::WriteSingleCoil { addr: 3, value: true }
        ));
        assert!(matches!(
            p.to_request(&Variant::UInt16(0)).unwrap(),
            ModbusRequest::WriteSingleCoil { value: false, .. }
        ));
        assert!(matches!(
            p.to_request(&Variant::String("x".into())),
            Err(StatusCode::BadTypeMismatch)
        ));
    }

    /// FC15: a coil forced into a multi-write frame carries one coil value.
    #[test]
    fn coil_write_multiple_forces_fc15() {
        let (tx, _rx) = mpsc::channel(1);
        let p = WritePlan {
            device: DeviceId(0),
            address: 7,
            write_fn: WriteFn::MultipleCoils,
            decode: DecodeMeta {
                data_type: DataType::Bit,
                word_order: WordOrder::BigEndian,
                byte_order: ByteOrder::BigEndian,
                bit: None,
            },
            inverse: Inverse::None,
            sender: tx,
            wait: Duration::from_secs(10),
        };
        match p.to_request(&Variant::Boolean(true)).unwrap() {
            ModbusRequest::WriteMultipleCoils { addr, values } => {
                assert_eq!(addr, 7);
                assert_eq!(values, vec![true]);
            }
            other => panic!("expected FC15 multi-coil write, got {other:?}"),
        }
    }

    /// FC16 forced on a single-word type: FC16 frame even though FC06 would fit.
    #[test]
    fn single_word_forced_to_fc16() {
        let mut p = plan(DataType::U16, Inverse::None);
        p.write_fn = WriteFn::MultipleRegisters;
        match p.to_request(&Variant::Double(42.0)).unwrap() {
            ModbusRequest::WriteMultipleRegisters { addr, values } => {
                assert_eq!((addr, values), (10, vec![42]));
            }
            other => panic!("expected forced FC16, got {other:?}"),
        }
    }

    /// FC06 forced stays a single-register write for a single-word type.
    #[test]
    fn single_word_forced_to_fc06() {
        let mut p = plan(DataType::U16, Inverse::None);
        p.write_fn = WriteFn::SingleRegister;
        assert!(matches!(
            p.to_request(&Variant::Double(42.0)).unwrap(),
            ModbusRequest::WriteSingleRegister { addr: 10, value: 42 }
        ));
    }

    #[test]
    fn range_and_type_guards() {
        let p = plan(DataType::U16, Inverse::None);
        // 70000 encodes over u16 -> encode() overflow -> BadTypeMismatch.
        assert!(matches!(p.to_request(&Variant::Double(70000.0)), Err(StatusCode::BadTypeMismatch)));
        // Negative into unsigned -> BadOutOfRange at raw_to_typed.
        assert!(matches!(p.to_request(&Variant::Double(-5.0)), Err(StatusCode::BadOutOfRange)));
        // NaN -> BadOutOfRange.
        assert!(matches!(p.to_request(&Variant::Double(f64::NAN)), Err(StatusCode::BadOutOfRange)));
        // Non-numeric -> BadTypeMismatch.
        assert!(matches!(p.to_request(&Variant::String("x".into())), Err(StatusCode::BadTypeMismatch)));
    }

    #[test]
    fn build_plans_compiles_inverse_fail_fast() {
        let cfg = gateway_config::load_str(
            r#"{
            "schema_version": "1",
            "poll_groups": [ { "id": "fast", "period_ms": 100 } ],
            "channels": [ { "id": "c", "transport": { "type": "tcp", "host": "h" },
                "devices": [ { "id": "d", "unit_id": 1, "registers": [
                    { "tag": "w", "poll_group": "fast", "function": "read_holding_registers",
                      "address": 0, "data_type": "u16", "writable": true,
                      "formula": "raw * 60", "write_formula": "value +" }
                ] } ] } ]
        }"#,
        )
        .expect("config loads (formula syntax is checked by tags-core, not validate)");
        let mut writers = HashMap::new();
        let (tx, _rx) = mpsc::channel(1);
        writers.insert(ChannelId(0), tx);
        let err = match build_write_plans(&cfg, &writers) {
            Err(e) => e,
            Ok(_) => panic!("bad write_formula must fail plan build"),
        };
        assert!(matches!(err, WritePlanError::Formula { tag, .. } if tag == "w"));
    }

    #[test]
    fn wait_budget_follows_device_timeout_and_retries() {
        // B1: wait = device request_timeout * (1 + max_retries) + 5s margin,
        // per device — NOT a flat constant. Device `slow` overrides both
        // knobs; `plain` inherits the channel values.
        let cfg = gateway_config::load_str(
            r#"{
            "schema_version": "1",
            "poll_groups": [ { "id": "fast", "period_ms": 100 } ],
            "channels": [ { "id": "c", "transport": { "type": "tcp", "host": "h" },
                "request_timeout_ms": 500,
                "retry": { "max_retries": 2 },
                "devices": [
                    { "id": "slow", "unit_id": 1,
                      "request_timeout_ms": 2000, "retry": { "max_retries": 3 },
                      "registers": [
                        { "tag": "s", "poll_group": "fast", "function": "read_holding_registers",
                          "address": 0, "data_type": "u16", "writable": true }
                    ] },
                    { "id": "plain", "unit_id": 2, "registers": [
                        { "tag": "p", "poll_group": "fast", "function": "read_holding_registers",
                          "address": 0, "data_type": "u16", "writable": true }
                    ] }
                ] } ]
        }"#,
        )
        .expect("config loads");
        let mut writers = HashMap::new();
        let (tx, _rx) = mpsc::channel(1);
        writers.insert(ChannelId(0), tx);
        let plans = build_write_plans(&cfg, &writers).expect("plans build");
        assert_eq!(
            plans["s"].wait,
            Duration::from_millis(2000 * 4) + WRITE_QUEUE_MARGIN
        );
        assert_eq!(
            plans["p"].wait,
            Duration::from_millis(500 * 3) + WRITE_QUEUE_MARGIN
        );
    }
}
