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

pub enum GuiStack {
    Local(Box<RunningStack>),
    Managed(Arc<svc_host::mcp::Controller>),
    Remote(Arc<crate::remote::Remote>),
    Empty,
}
impl GuiStack {
    pub fn config_revisions(&self) -> (Option<String>, Option<String>) {
        let view = match self {
            Self::Managed(c) => c.view(),
            Self::Remote(c) => c.view(),
            _ => return (None, None),
        };
        (view.saved_revision, view.applied_revision)
    }
    pub fn is_running(&self) -> bool {
        match self {
            Self::Local(_) => true,
            Self::Managed(c) => c.view().running,
            Self::Remote(c) => c.view().running,
            Self::Empty => false,
        }
    }
    pub fn mcp_url(&self) -> Option<String> {
        match self {
            Self::Managed(c) => c.url(),
            Self::Remote(c) => Some(c.url()),
            _ => None,
        }
    }
    pub fn mcp_clients(&self) -> Vec<svc_host::mcp::McpClientView> {
        match self {
            Self::Managed(c) => c.client_views(),
            Self::Remote(c) => c.view().mcp_clients,
            _ => vec![],
        }
    }
    pub fn revision(&self) -> u64 {
        match self {
            Self::Local(s) => s.revision(),
            Self::Managed(c) => c.view().revision,
            Self::Remote(c) => c.view().revision,
            Self::Empty => 0,
        }
    }
    pub fn typed(&self) -> Arc<tags_core::TypedStore> {
        match self {
            Self::Local(s) => s.typed(),
            Self::Managed(c) => c.view().typed.unwrap_or_else(|| {
                Arc::new(tags_core::TypedStore::from_names(std::iter::empty::<&str>()))
            }),
            Self::Remote(c) => c.view().typed.unwrap_or_else(|| {
                Arc::new(tags_core::TypedStore::from_names(std::iter::empty::<&str>()))
            }),
            Self::Empty => Arc::new(tags_core::TypedStore::from_names(std::iter::empty::<&str>())),
        }
    }
    pub fn channel_metrics(&self) -> Vec<(String, Arc<mb_poller::ChannelMetrics>)> {
        match self {
            Self::Local(s) => s.channel_metrics(),
            Self::Managed(c) => c.view().metrics,
            Self::Remote(c) => c.view().metrics,
            Self::Empty => vec![],
        }
    }
    pub fn endpoint_url(&self) -> Option<String> {
        match self {
            Self::Local(s) => s.endpoint_url(),
            Self::Managed(c) => c.view().endpoint,
            Self::Remote(c) => c.view().endpoint,
            Self::Empty => None,
        }
    }
    pub fn trial_remaining(&self) -> Option<std::time::Duration> {
        match self {
            Self::Local(s) => s.trial_remaining(),
            Self::Managed(c) => c.view().trial_remaining,
            Self::Remote(c) => c.view().trial_remaining,
            Self::Empty => None,
        }
    }
    pub fn trial_expired(&self) -> bool {
        self.trial_remaining().is_some_and(|r| r.is_zero())
    }
    pub fn opcua(&self) -> Option<svc_host::mcp::OpcUaView> {
        match self {
            Self::Local(s) => s.opcua().map(|o| svc_host::mcp::OpcUaView {
                sessions: o.session_count(),
                authentications: o.recent_authentications(),
            }),
            Self::Managed(c) => c.view().opcua,
            Self::Remote(c) => c.view().opcua,
            Self::Empty => None,
        }
    }
    pub async fn reload(
        &mut self,
        cfg: ResolvedConfig,
        path: Option<std::path::PathBuf>,
    ) -> Result<(), String> {
        match self {
            Self::Local(s) => s.reload(cfg).await?,
            Self::Managed(c) => return c.reload_file().await,
            Self::Remote(c) => return c.reload().await,
            Self::Empty => return Err("runtime unavailable".into()),
        }
        if let Some(path) = path {
            let c = svc_host::mcp::Controller::new(path).await?;
            if let Self::Local(s) = std::mem::replace(self, Self::Empty) {
                if let Err(e) = c.adopt(*s).await {
                    c.shutdown().await;
                    return Err(e);
                }
                *self = Self::Managed(c);
            }
        }
        Ok(())
    }
    pub async fn shutdown(self) {
        match self {
            Self::Local(s) => s.shutdown().await,
            Self::Managed(c) => c.shutdown().await,
            Self::Remote(c) => c.detach().await,
            Self::Empty => {}
        }
    }
}

async fn attach_controller(path: std::path::PathBuf) -> Result<GuiStack, String> {
    let resume = svc_host::mcp::previous_runtime_was_running(&path)
        && !svc_host::mcp::shutdown_was_requested(&path);
    match svc_host::mcp::Controller::new(&path).await {
        Ok(controller) => {
            if resume {
                if let Err(error) = controller.start_server().await {
                    controller.shutdown().await;
                    return Err(error);
                }
            }
            Ok(GuiStack::Managed(controller))
        }
        Err(error) if error == svc_host::mcp::OWNER_BUSY => crate::remote::Remote::connect(path)
            .await
            .map(GuiStack::Remote),
        Err(error) => Err(error),
    }
}

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
    Started(Box<GuiStack>, Instant),
    StartFailed(String),
    Stopped,
    Reloaded(Box<GuiStack>, Instant, Result<(), String>),
}

/// Состояние встроенного сервера в окне.
pub enum ServerState {
    Stopped,
    Starting,
    Running {
        stack: Box<GuiStack>,
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
    auto_attach: bool,
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
            auto_attach: false,
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
        matches!(&self.state,ServerState::Running {stack,..} if stack.is_running())
            || matches!(self.state, ServerState::Reloading { .. })
    }
    pub fn has_mcp(&self) -> bool {
        matches!(&self.state,ServerState::Running{stack,..} if matches!(**stack,GuiStack::Managed(_)|GuiStack::Remote(_)))
    }
    pub fn remote_logs(&self) -> Option<Vec<String>> {
        match &self.state {
            ServerState::Running { stack, .. } => match &**stack {
                GuiStack::Remote(c) => Some(c.logs.read().clone()),
                _ => None,
            },
            _ => None,
        }
    }
    pub fn attach(&mut self, path: std::path::PathBuf) {
        self.attach_runtime(path, false);
    }
    fn attach_runtime(&mut self, path: std::path::PathBuf, resume: bool) {
        self.auto_attach = true;
        self.shutdown_blocking();
        self.watch_file(Some(path.clone()));
        self.last_error = None;
        let rt = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let worker = rt.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let ev = worker.block_on(async {
                match attach_controller(path).await {
                    Ok(stack) => {
                        if resume {
                            let result = match &stack {
                                GuiStack::Managed(c) => c.start_server().await,
                                GuiStack::Remote(c) => c.start().await,
                                _ => unreachable!(),
                            };
                            if let Err(error) = result {
                                stack.shutdown().await;
                                return Ev::StartFailed(error);
                            }
                        }
                        Ev::Started(Box::new(stack), Instant::now())
                    }
                    Err(e) => Ev::StartFailed(e),
                }
            });
            let _ = tx.send(ev);
        });
        self.rt = Some(rt);
        self.rx = Some(rx);
        self.state = ServerState::Starting;
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
        let path = self.watch_path.clone();
        std::thread::spawn(move || {
            let result = rt.block_on(stack.reload(cfg, path));
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
        if matches!(&self.state, ServerState::Running {stack,..} if matches!(&**stack, GuiStack::Managed(c) if c.is_closed()))
        {
            self.shutdown_blocking();
        }
        if let ServerState::Running { stack, .. } = &self.state {
            if let GuiStack::Remote(c) = &**stack {
                if !c.view().running {
                    let c = c.clone();
                    let rt = self.rt.clone().unwrap();
                    let (tx, rx) = std::sync::mpsc::channel();
                    let stack = match std::mem::replace(&mut self.state, ServerState::Starting) {
                        ServerState::Running { stack, .. } => stack,
                        _ => unreachable!(),
                    };
                    std::thread::spawn(move || {
                        let result = rt.block_on(c.start());
                        let _ = tx.send(Ev::Reloaded(stack, Instant::now(), result));
                    });
                    self.rx = Some(rx);
                }
                return;
            }
            if let GuiStack::Managed(c) = &**stack {
                if !c.view().running && !c.is_closed() {
                    let c = Arc::clone(c);
                    let rt = self.rt.clone().unwrap();
                    let (tx, rx) = std::sync::mpsc::channel();
                    let stack = match std::mem::replace(&mut self.state, ServerState::Starting) {
                        ServerState::Running { stack, .. } => stack,
                        _ => unreachable!(),
                    };
                    std::thread::spawn(move || {
                        let result = rt.block_on(c.start_server());
                        let _ = tx.send(Ev::Reloaded(stack, Instant::now(), result));
                    });
                    self.rx = Some(rx);
                    return;
                }
            }
        }
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
        let path = self.watch_path.clone();
        std::thread::spawn(move || {
            let ev = rt_thread.block_on(async {
                if let Some(path) = path {
                    return match attach_controller(path).await {
                        Ok(stack) => {
                            let result = match &stack {
                                GuiStack::Managed(c) => c.start_server().await,
                                GuiStack::Remote(c) => c.start().await,
                                _ => unreachable!(),
                            };
                            match result {
                                Ok(()) => Ev::Started(Box::new(stack), Instant::now()),
                                Err(e) => {
                                    stack.shutdown().await;
                                    Ev::StartFailed(e)
                                }
                            }
                        }
                        Err(e) => Ev::StartFailed(e),
                    };
                }
                match start_stack(cfg).await {
                    Ok(stack) => {
                        Ev::Started(Box::new(GuiStack::Local(Box::new(stack))), Instant::now())
                    }
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
        let (stack, since) = match std::mem::replace(&mut self.state, ServerState::Stopping) {
            ServerState::Running { stack, since } => (stack, since),
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
            if let GuiStack::Remote(c) = &*stack {
                let result = rt.block_on(c.stop());
                let _ = tx.send(Ev::Reloaded(stack, since, result));
                return;
            }
            if let GuiStack::Managed(c) = &*stack {
                rt.block_on(c.stop_server());
                let _ = tx.send(Ev::Reloaded(stack, since, Ok(())));
                return;
            }
            rt.block_on(async move { stack.shutdown().await });
            let _ = tx.send(Ev::Stopped);
        });
        self.rx = Some(rx);
    }

    /// Обработать события фонового потока; true — состояние изменилось.
    pub fn pump(&mut self) -> bool {
        let lost = match &self.state {
            ServerState::Running { stack, .. } => match &**stack {
                GuiStack::Managed(c) => c.is_closed(),
                GuiStack::Remote(c) => !c.is_connected(),
                _ => false,
            },
            _ => false,
        };
        let retry = matches!(&self.state, ServerState::Stopped)
            && self.last_check.elapsed() >= std::time::Duration::from_secs(2);
        if (lost || retry) && self.watch_path.is_some() {
            let path = self.watch_path.clone().unwrap();
            if svc_host::mcp::shutdown_was_requested(&path) {
                self.shutdown_blocking();
                self.auto_attach = false;
                self.last_error=Some("Контроллер завершён командой MCP. Запустите сервер для повторного подключения.".into());
                return true;
            }
            let remembered = matches!(&self.state,ServerState::Running{stack,..} if matches!(&**stack,GuiStack::Remote(c) if c.running_before_disconnect()));
            let resume = svc_host::mcp::previous_runtime_intent(&path).unwrap_or(remembered);
            self.last_check = Instant::now();
            self.attach_runtime(path, resume);
            return true;
        }
        if let ServerState::Running { stack, .. } = &self.state {
            if let GuiStack::Managed(c) = &**stack {
                self.last_error = c.view().error;
            }
            if let GuiStack::Remote(c) = &**stack {
                self.last_error = c.view().error;
            }
        }
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
                                Ok(cfg) => {
                                    if !matches!(&self.state,ServerState::Running{stack,..} if matches!(&**stack,GuiStack::Remote(_)|GuiStack::Managed(_)))
                                    {
                                        self.reload(cfg);
                                    }
                                }
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
    fn gui_attaches_to_mcp_and_closing_gui_preserves_polling() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path,serde_json::json!({"schema_version":"1","gateway":{"data_dir":dir.path().join("data")},"mcp":{"port":0},"opcua":{"enabled":false},"channels":[],"poll_groups":[]}).to_string()).unwrap();
        let owner_rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let owner = owner_rt
            .block_on(svc_host::mcp::Controller::new(&path))
            .unwrap();
        owner_rt.block_on(owner.start_server()).unwrap();
        let mut server = EmbeddedServer::default();
        server.attach(path.clone());
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while !server.has_mcp() {
            server.pump();
            assert!(Instant::now() < deadline, "{:?}", server.last_error);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(server.is_running());
        let remote = match &server.state {
            ServerState::Running { stack, .. } => match &**stack {
                GuiStack::Remote(c) => c.clone(),
                _ => panic!("remote client required"),
            },
            _ => unreachable!(),
        };
        let rt = server.rt.clone().unwrap();
        rt.block_on(remote.stop()).unwrap();
        assert!(!owner.view().running);
        rt.block_on(remote.start()).unwrap();
        assert!(owner.view().running);
        server.shutdown_blocking();
        drop(rt);
        assert!(owner.view().running);
        owner_rt.block_on(owner.shutdown());
    }
    #[test]
    fn gui_owns_selected_controller_without_launching_a_child_server() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path,serde_json::json!({"schema_version":"1","gateway":{"instance_name":"selected","data_dir":dir.path().join("data")},"mcp":{"port":0},"opcua":{"enabled":false},"channels":[],"poll_groups":[]}).to_string()).unwrap();
        let mut server = EmbeddedServer::default();
        server.attach(path.clone());
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while !server.has_mcp() {
            server.pump();
            assert!(Instant::now() < deadline, "{:?}", server.last_error);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            matches!(&server.state,ServerState::Running{stack,..} if matches!(&**stack,GuiStack::Managed(_)))
        );
        assert!(!server.is_running());
        let endpoint: serde_json::Value = serde_json::from_slice(
            &std::fs::read(format!("{}.mcp-endpoint.json", path.display())).unwrap(),
        )
        .unwrap();
        assert_eq!(endpoint["pid"], std::process::id());
        server.shutdown_blocking();
        assert!(!std::path::Path::new(&format!("{}.mcp-endpoint.json", path.display())).exists());
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
