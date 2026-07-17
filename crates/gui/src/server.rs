//! Встроенный сервер (F5): запуск/останов полного стека прямо из окна, без
//! отдельного консольного процесса. Собственный многопоточный tokio-рантайм
//! живёт в фоновом потоке; UI общается с ним через std::sync::mpsc и читает
//! наблюдаемый [`svc_host::RunningStack`].

use std::collections::VecDeque;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

use gateway_config::ResolvedConfig;
use svc_host::{start_stack, RunningStack, StackExit};

/// Кольцевой буфер строк лога, который наполняет tracing-слой и читает окно.
pub type LogBuffer = Arc<Mutex<VecDeque<String>>>;
const LOG_CAP: usize = 500;

/// Установить глобальный tracing-подписчик процесса GUI: строки пишутся в
/// кольцевой буфер (его показывает панель «Сервер»). Вызывать один раз.
pub fn install_log_capture() -> LogBuffer {
    let buf: LogBuffer = Arc::new(Mutex::new(VecDeque::with_capacity(LOG_CAP)));
    let layer = RingLayer { buf: Arc::clone(&buf) };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    // try_init: не паникуем, если подписчик уже стоит (напр. в тестах).
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(layer)
        .try_init();
    buf
}

struct RingLayer {
    buf: LogBuffer,
}

impl<S: tracing::Subscriber> Layer<S> for RingLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        let mut visitor = MsgVisitor::default();
        event.record(&mut visitor);
        let meta = event.metadata();
        let line = format!(
            "{:>5} {}: {}",
            meta.level(),
            meta.target(),
            visitor.text.trim()
        );
        let mut b = self.buf.lock();
        if b.len() >= LOG_CAP {
            b.pop_front();
        }
        b.push_back(line);
    }
}

#[derive(Default)]
struct MsgVisitor {
    text: String,
}

impl tracing::field::Visit for MsgVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.text = format!("{value:?}");
        } else {
            self.text.push_str(&format!(" {}={:?}", field.name(), value));
        }
    }
}

/// Сообщение из фонового потока запуска/останова в UI.
enum Ev {
    Started(Box<RunningStack>, Instant),
    StartFailed(String),
    Stopped,
}

/// Состояние встроенного сервера в окне.
pub enum ServerState {
    Stopped,
    Starting,
    Running { stack: Box<RunningStack>, since: Instant },
    Stopping,
}

pub struct EmbeddedServer {
    pub state: ServerState,
    rx: Option<Receiver<Ev>>,
    /// Отдельный рантайм живёт, пока стек запущен (нужен для block_on shutdown).
    rt: Option<Arc<tokio::runtime::Runtime>>,
    pub last_error: Option<String>,
}

impl Default for EmbeddedServer {
    fn default() -> Self {
        Self {
            state: ServerState::Stopped,
            rx: None,
            rt: None,
            last_error: None,
        }
    }
}

impl EmbeddedServer {
    pub fn is_busy(&self) -> bool {
        matches!(self.state, ServerState::Starting | ServerState::Stopping)
    }

    pub fn is_running(&self) -> bool {
        matches!(self.state, ServerState::Running { .. })
    }

    /// Живые типизированные значения тегов работающего стека (для графиков).
    pub fn typed(&self) -> Option<Arc<tags_core::TypedStore>> {
        match &self.state {
            ServerState::Running { stack, .. } => Some(stack.typed()),
            _ => None,
        }
    }

    /// Запустить стек из готового ResolvedConfig (валидация/пути уже применены
    /// вызывающим). Поднятие идёт в фоновом потоке — UI не блокируется.
    pub fn start(&mut self, cfg: ResolvedConfig) {
        if !matches!(self.state, ServerState::Stopped) {
            return;
        }
        self.last_error = None;
        let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
            Ok(rt) => Arc::new(rt),
            Err(e) => {
                self.last_error = Some(format!("не удалось создать рантайм: {e}"));
                return;
            }
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let rt_thread = Arc::clone(&rt);
        std::thread::spawn(move || {
            let ev = rt_thread.block_on(async {
                match start_stack(cfg).await {
                    Ok(stack) => Ev::Started(Box::new(stack), Instant::now()),
                    Err(exit) => Ev::StartFailed(describe_exit(exit)),
                }
            });
            let _ = tx.send(ev);
        });
        self.rt = Some(rt);
        self.rx = Some(rx);
        self.state = ServerState::Starting;
    }

    /// Остановить стек (в фоне, через свой рантайм).
    pub fn stop(&mut self) {
        let stack = match std::mem::replace(&mut self.state, ServerState::Stopping) {
            ServerState::Running { stack, .. } => stack,
            other => {
                self.state = other;
                return;
            }
        };
        let Some(rt) = self.rt.clone() else {
            self.state = ServerState::Stopped;
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            rt.block_on(async move { stack.shutdown().await });
            let _ = tx.send(Ev::Stopped);
        });
        self.rx = Some(rx);
    }

    /// Обработать события фонового потока; true — состояние изменилось.
    pub fn pump(&mut self) -> bool {
        // Пробная сборка: по истечении лимита останавливаем сервер сами.
        if let ServerState::Running { stack, .. } = &self.state {
            if stack.trial_expired() {
                self.last_error = Some("Пробная сборка: истёк лимит 3 часа — сервер остановлен.".into());
                self.stop();
                return true;
            }
        }
        let Some(rx) = &self.rx else { return false };
        let Ok(ev) = rx.try_recv() else { return false };
        self.rx = None;
        match ev {
            Ev::Started(stack, since) => {
                self.state = ServerState::Running { stack, since };
            }
            Ev::StartFailed(msg) => {
                self.last_error = Some(msg);
                self.rt = None;
                self.state = ServerState::Stopped;
            }
            Ev::Stopped => {
                self.rt = None;
                self.state = ServerState::Stopped;
            }
        }
        true
    }

    /// Лучшее-усилие корректный останов при закрытии приложения.
    pub fn shutdown_blocking(&mut self) {
        if let ServerState::Running { stack, .. } =
            std::mem::replace(&mut self.state, ServerState::Stopped)
        {
            if let Some(rt) = self.rt.take() {
                rt.block_on(async move { stack.shutdown().await });
            }
        }
    }
}

fn describe_exit(exit: StackExit) -> String {
    match exit {
        StackExit::Clean => "остановлено".into(),
        StackExit::StorageError => "ошибка хранилища (data_dir / sled)".into(),
        StackExit::EngineError => "движок тегов не стартовал (проверьте формулы)".into(),
        StackExit::OpcUaError => "сервер OPC UA не стартовал (порт/сертификаты)".into(),
    }
}
