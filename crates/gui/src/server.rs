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
    let layer = RingLayer {
        buf: Arc::clone(&buf),
    };
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
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
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
            self.text
                .push_str(&format!(" {}={:?}", field.name(), value));
        }
    }
}

/// Сообщение из фонового потока запуска/останова в UI.
enum Ev {
    Started(Box<RunningStack>, Instant),
    StartFailed(String),
    Stopped,
    Reloaded(Box<RunningStack>, Instant, Result<(), String>),
}

/// Состояние встроенного сервера в окне.
pub enum ServerState {
    Stopped,
    Starting,
    Running {
        stack: Box<RunningStack>,
        since: Instant,
    },
    Stopping,
    Reloading {
        typed: Arc<tags_core::TypedStore>,
    },
}

pub struct EmbeddedServer {
    pub state: ServerState,
    rx: Option<Receiver<Ev>>,
    /// Отдельный рантайм живёт, пока стек запущен (нужен для block_on shutdown).
    rt: Option<Arc<tokio::runtime::Runtime>>,
    pub last_error: Option<String>,
    watch_path: Option<std::path::PathBuf>,
    seen: Option<Vec<u8>>,
    pending: Option<Vec<u8>>,
    last_check: Instant,
    queued_config: Option<ResolvedConfig>,
    external_config: Option<(Vec<u8>, gateway_config::schema::v1::ConfigV1)>,
}

impl Default for EmbeddedServer {
    fn default() -> Self {
        Self {
            state: ServerState::Stopped,
            rx: None,
            rt: None,
            last_error: None,
            watch_path: None,
            seen: None,
            pending: None,
            last_check: Instant::now(),
            queued_config: None,
            external_config: None,
        }
    }
}

impl EmbeddedServer {
    pub fn is_busy(&self) -> bool {
        matches!(
            self.state,
            ServerState::Starting | ServerState::Stopping | ServerState::Reloading { .. }
        )
    }

    pub fn is_running(&self) -> bool {
        matches!(
            self.state,
            ServerState::Running { .. } | ServerState::Reloading { .. }
        )
    }

    /// Живые типизированные значения тегов работающего стека (для графиков).
    pub fn typed(&self) -> Option<Arc<tags_core::TypedStore>> {
        match &self.state {
            ServerState::Running { stack, .. } => Some(stack.typed()),
            ServerState::Reloading { typed } => Some(Arc::clone(typed)),
            _ => None,
        }
    }

    pub fn watch_file(&mut self, path: Option<std::path::PathBuf>) {
        self.seen = path.as_ref().and_then(|p| std::fs::read(p).ok());
        self.watch_path = path;
        self.pending = None;
    }

    pub fn reload(&mut self, cfg: ResolvedConfig) {
        if matches!(
            self.state,
            ServerState::Reloading { .. } | ServerState::Starting
        ) {
            self.queued_config = Some(cfg);
            return;
        }
        let state = std::mem::replace(&mut self.state, ServerState::Stopped);
        let ServerState::Running { mut stack, since } = state else {
            self.state = state;
            return;
        };
        let Some(rt) = self.rt.clone() else {
            self.state = ServerState::Running { stack, since };
            return;
        };
        self.state = ServerState::Reloading {
            typed: stack.typed(),
        };
        self.last_error = None;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = rt.block_on(stack.reload(cfg));
            let _ = tx.send(Ev::Reloaded(stack, since, result));
        });
        self.rx = Some(rx);
    }

    pub fn take_external_config(
        &mut self,
    ) -> Option<(Vec<u8>, gateway_config::schema::v1::ConfigV1)> {
        self.external_config.take()
    }

    /// Запустить стек из готового ResolvedConfig (валидация/пути уже применены
    /// вызывающим). Поднятие идёт в фоновом потоке — UI не блокируется.
    pub fn start(&mut self, cfg: ResolvedConfig) {
        if !matches!(self.state, ServerState::Stopped) {
            return;
        }
        self.last_error = None;
        let rt = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
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
        if matches!(self.state, ServerState::Running { .. })
            && self.last_check.elapsed() >= std::time::Duration::from_millis(250)
        {
            self.last_check = Instant::now();
            if let Some(path) = self.watch_path.clone() {
                match std::fs::read(&path) {
                    Ok(bytes) if self.seen.as_ref() != Some(&bytes) => {
                        if self.pending.as_ref() == Some(&bytes) {
                            if let Ok(gateway_config::ConfigFile::V1(cfg)) =
                                serde_json::from_slice(&bytes)
                            {
                                self.external_config = Some((bytes.clone(), cfg));
                            }
                            let candidate = String::from_utf8(bytes.clone())
                                .map_err(|e| e.to_string())
                                .and_then(|text| {
                                    gateway_config::load_str_at(&text, &path)
                                        .map_err(|e| e.to_string())
                                });
                            self.seen = Some(bytes);
                            self.pending = None;
                            match candidate {
                                Ok(cfg) => self.reload(cfg),
                                Err(e) => {
                                    self.last_error =
                                        Some(format!("Конфигурация не применена: {e}"))
                                }
                            }
                        } else {
                            self.pending = Some(bytes);
                        }
                    }
                    Ok(_) => self.pending = None,
                    Err(e) => {
                        self.pending = None;
                        self.last_error = Some(format!("Чтение конфигурации: {e}"));
                    }
                }
            }
        }
        // Пробная сборка: по истечении лимита останавливаем сервер сами.
        if let ServerState::Running { stack, .. } = &self.state {
            if stack.trial_expired() {
                self.last_error =
                    Some("Пробная сборка: истёк лимит 3 часа — сервер остановлен.".into());
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
            Ev::Reloaded(stack, since, result) => {
                self.last_error = result.err();
                self.state = ServerState::Running { stack, since };
            }
        }
        if matches!(self.state, ServerState::Running { .. }) {
            if let Some(cfg) = self.queued_config.take() {
                self.reload(cfg);
            }
        }
        true
    }

    /// Лучшее-усилие корректный останов при закрытии приложения.
    pub fn shutdown_blocking(&mut self) {
        // A prepared/reloaded stack in transit still belongs to this window.
        // Wait for that operation before destroying the runtime.
        if matches!(
            self.state,
            ServerState::Starting | ServerState::Reloading { .. }
        ) {
            if let Some(rx) = self.rx.take() {
                match rx.recv() {
                    Ok(Ev::Started(stack, since)) | Ok(Ev::Reloaded(stack, since, _)) => {
                        self.state = ServerState::Running { stack, since }
                    }
                    _ => self.state = ServerState::Stopped,
                }
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    fn config(dir: &std::path::Path, period: u64) -> ResolvedConfig {
        gateway_config::load_str(&serde_json::json!({
            "schema_version":"1", "gateway":{"data_dir":dir.join("data")},
            "opcua":{"enabled":false}, "poll_groups":[{"id":"p","period_ms":period}],"channels":[]
        }).to_string()).unwrap()
    }
    #[test]
    fn save_during_start_queues_the_latest_configuration_and_close_drains_reload() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = EmbeddedServer::default();
        server.start(config(dir.path(), 100));
        let mut rejected = config(dir.path(), 200);
        rejected.gateway.data_dir = dir.path().join("other").to_string_lossy().to_string();
        server.reload(rejected);
        server.reload(config(dir.path(), 300));
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        loop {
            server.pump();
            if matches!(&server.state,ServerState::Running { stack, .. } if stack.revision() == 2) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "queued save must apply: {:?}",
                server.last_error
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(server.last_error.is_none());
        server.reload(config(dir.path(), 400));
        server.shutdown_blocking();
        assert!(matches!(server.state, ServerState::Stopped));
        assert!(server.rt.is_none());
    }
}
