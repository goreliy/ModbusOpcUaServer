//! Пробное чтение устройства (dry-run наладчика): фоновый поток с собственным
//! однопоточным tokio-рантаймом подключается по конфигу канала и читает все
//! настроенные регистры устройства один раз. Результаты стекают в UI через
//! std::sync::mpsc (окно опрашивает их каждый кадр).
//!
//! Соответствие рантайму (B9): используется РАЗРЕШЁННЫЙ таймаут устройства
//! (device.request_timeout_ms или канал), и опрос можно ОТМЕНИТЬ — флаг
//! проверяется между регистрами, закрытие окна взводит его.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use gateway_config::schema::v1::{DeviceConfig, TransportConfig};
use mb_proto::{ModbusRequest, ModbusResponse, Transport};
use mb_types::FunctionCode;

pub enum Msg {
    Line(String),
    Done,
}

pub struct TestPoll {
    rx: Receiver<Msg>,
    cancel: Arc<AtomicBool>,
    pub lines: Vec<String>,
    pub running: bool,
}

impl TestPoll {
    /// `timeout_ms` — уже разрешённый таймаут (устройство или канал).
    pub fn start(transport: TransportConfig, device: DeviceConfig, timeout_ms: u64) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_thread = Arc::clone(&cancel);
        std::thread::spawn(move || run(tx, transport, device, timeout_ms, cancel_thread));
        Self {
            rx,
            cancel,
            lines: Vec::new(),
            running: true,
        }
    }

    /// Взвести отмену (закрытие окна / кнопка «Остановить»).
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Забрать всё, что накопилось; вернуть true, если что-то изменилось.
    pub fn pump(&mut self) -> bool {
        let mut changed = false;
        while let Ok(msg) = self.rx.try_recv() {
            changed = true;
            match msg {
                Msg::Line(l) => self.lines.push(l),
                Msg::Done => self.running = false,
            }
        }
        changed
    }
}

impl Drop for TestPoll {
    fn drop(&mut self) {
        // Окно закрыли, пока опрос шёл — не оставляем поток висеть на шине.
        self.cancel.store(true, Ordering::Relaxed);
    }
}

fn run(
    tx: Sender<Msg>,
    transport: TransportConfig,
    device: DeviceConfig,
    timeout_ms: u64,
    cancel: Arc<AtomicBool>,
) {
    let send = |s: String| {
        let _ = tx.send(Msg::Line(s));
    };
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            send(format!("✗ tokio: {e}"));
            let _ = tx.send(Msg::Done);
            return;
        }
    };

    rt.block_on(async {
        send("Подключение...".into());
        let mut t = match Transport::connect(&transport).await {
            Ok(t) => t,
            Err(e) => {
                send(format!("✗ подключение не удалось: {e}"));
                return;
            }
        };
        send("✓ подключено".into());
        let timeout = Duration::from_millis(timeout_ms.max(100));

        for reg in &device.registers {
            if cancel.load(Ordering::Relaxed) {
                send("⏹ отменено".into());
                break;
            }
            let qty = reg.data_type.register_count().or(reg.length).unwrap_or(1);
            let req = match reg.function {
                FunctionCode::ReadCoils => ModbusRequest::ReadCoils { addr: reg.address, qty: 1 },
                FunctionCode::ReadDiscreteInputs => {
                    ModbusRequest::ReadDiscreteInputs { addr: reg.address, qty: 1 }
                }
                FunctionCode::ReadHoldingRegisters => {
                    ModbusRequest::ReadHoldingRegisters { addr: reg.address, qty }
                }
                FunctionCode::ReadInputRegisters => {
                    ModbusRequest::ReadInputRegisters { addr: reg.address, qty }
                }
                FunctionCode::Custom { code } => ModbusRequest::Custom {
                    code,
                    data: parse_hex(reg.custom_request.as_deref().unwrap_or("")),
                    expect_len: reg.custom_response_len,
                },
                _ => {
                    send(format!("· {}: функция записи — пропущено", reg.tag));
                    continue;
                }
            };
            match t.request(device.unit_id, &req, timeout).await {
                Ok(ModbusResponse::Registers(regs)) => {
                    let words: Vec<String> = regs.iter().map(|w| format!("{w:#06x}")).collect();
                    send(format!("✓ {} @ {}: [{}]", reg.tag, reg.address, words.join(", ")));
                }
                Ok(ModbusResponse::Bits(bits)) => {
                    send(format!("✓ {} @ {}: {:?}", reg.tag, reg.address, bits));
                }
                Ok(ModbusResponse::Raw(bytes)) => {
                    let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
                    send(format!("✓ {} (custom): [{}]", reg.tag, hex.join(" ")));
                }
                Ok(ModbusResponse::WriteAck) => {}
                Err(e) => send(format!("✗ {} @ {}: {e}", reg.tag, reg.address)),
            }
        }
        let _ = t.disconnect().await;
    });
    let _ = tx.send(Msg::Done);
}

/// "01 a0 ff" -> [0x01, 0xa0, 0xff]; мусор и нечётность игнорируем мягко.
fn parse_hex(s: &str) -> Vec<u8> {
    let clean: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    clean
        .as_bytes()
        .chunks(2)
        .filter(|c| c.len() == 2)
        .filter_map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 16).ok())
        .collect()
}
