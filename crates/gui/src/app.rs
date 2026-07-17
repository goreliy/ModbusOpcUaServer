//! Окно конфигуратора: навигация, встроенный сервер, формы всех разделов
//! схемы, живая валидация (конфиг + формулы) и служебные диалоги.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};
use gateway_config::schema::v1::*;
use gateway_config::ConfigFile;
use mb_types::{ByteOrder, DataType, FunctionCode, WordOrder};

use crate::server::{EmbeddedServer, LogBuffer, ServerState};
use crate::testpoll::TestPoll;

/// Кириллица гарантированно: подмешиваем системный шрифт первым в стек.
pub fn install_fonts(ctx: &egui::Context) {
    let candidates: &[&str] = if cfg!(windows) {
        &[r"C:\Windows\Fonts\segoeui.ttf", r"C:\Windows\Fonts\arial.ttf"]
    } else {
        &[
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
        ]
    };
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts
                .font_data
                .insert("system".into(), egui::FontData::from_owned(bytes));
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts
                    .families
                    .entry(family)
                    .or_default()
                    .insert(0, "system".into());
            }
            ctx.set_fonts(fonts);
            return;
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Server,
    OpcUa,
    General,
    PollGroups,
    Channel(usize),
    Device(usize, usize),
}

pub struct ConfigApp {
    path: Option<PathBuf>,
    cfg: ConfigV1,
    dirty: bool,
    section: Section,
    selected_register: Option<usize>,
    hash_dialog: Option<HashDialog>,
    test_poll: Option<TestPoll>,
    status: String,
    // Встроенный сервер и его лог.
    server: EmbeddedServer,
    logs: LogBuffer,
    // Кэш обнаруженных COM-портов (F1).
    ports: Vec<String>,
    ports_loaded: bool,
    /// Показывать адреса регистров в 16-ричной системе (карты регистров у
    /// вендоров обычно в hex). Влияет только на отображение/ввод.
    addr_hex: bool,
    /// Живая история значений наблюдаемых тегов (для графиков).
    history: crate::history::History,
    /// Открытые окна графиков по тегам.
    charts: Vec<crate::history::ChartWindow>,
}

#[derive(Default)]
struct HashDialog {
    user_idx: usize,
    password: String,
    result: Option<String>,
}

impl ConfigApp {
    pub fn new(logs: LogBuffer) -> Self {
        Self {
            path: None,
            cfg: template_config(),
            dirty: false,
            section: Section::Server,
            selected_register: None,
            hash_dialog: None,
            test_poll: None,
            status: "Новая конфигурация".into(),
            server: EmbeddedServer::default(),
            logs,
            ports: Vec::new(),
            ports_loaded: false,
            addr_hex: false,
            history: crate::history::History::default(),
            charts: Vec::new(),
        }
    }
}

/// Поле адреса регистра с выбором системы счисления (dec/hex).
fn address_edit(ui: &mut egui::Ui, addr: &mut u16, hex: bool) {
    let mut dv = egui::DragValue::new(addr).range(0..=0xFFFF_u16);
    if hex {
        // 16-ричный ввод/показ, минимум 4 цифры, верхний регистр, с префиксом.
        dv = dv.hexadecimal(4, false, true).prefix("0x");
    }
    ui.add(dv);
}

fn template_config() -> ConfigV1 {
    ConfigV1 {
        gateway: GatewaySettings::default(),
        opcua: OpcUaConfig::default(),
        logging: LoggingConfig {
            dir: Some("./logs".into()),
            ..LoggingConfig::default()
        },
        poll_groups: vec![
            PollGroupConfig { id: "fast".into(), period_ms: 500, priority: 0 },
            PollGroupConfig { id: "slow".into(), period_ms: 5000, priority: 0 },
        ],
        channels: vec![],
    }
}

impl eframe::App for ConfigApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.ports_loaded {
            self.ports = mb_proto::available_serial_ports();
            self.ports_loaded = true;
        }
        if let Some(tp) = &mut self.test_poll {
            tp.pump();
            if tp.running {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
        }
        if self.server.pump() || self.server.is_running() {
            // Пока сервер жив — обновляем метрики/значения раз в секунду.
            ctx.request_repaint_after(Duration::from_secs(1));
        }

        // Наполняем историю значений для открытых графиков (только по тем
        // тегам, чьё окно открыто — это ограничивает объём даже на больших
        // конфигурациях).
        self.record_history();
        // Более частая перерисовка, пока открыт хотя бы один живой график.
        let live_charts = !self.charts.is_empty()
            && self.server.is_running()
            && self.charts.iter().any(|c| c.open && !c.is_paused());
        if live_charts {
            ctx.request_repaint_after(Duration::from_millis(250));
        }

        self.top_bar(ctx);
        self.left_nav(ctx);
        self.validation_panel(ctx);
        self.central(ctx);
        self.hash_dialog_window(ctx);
        self.test_poll_window(ctx);
        self.chart_windows(ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Закрытие окна при работающем сервере — корректный останов.
        self.server.shutdown_blocking();
    }
}

impl ConfigApp {
    // ---------- файл ----------

    fn open(&mut self) {
        let Some(path) = rfd::FileDialog::new().add_filter("JSON", &["json"]).pick_file() else {
            return;
        };
        match std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|text| serde_json::from_str::<ConfigFile>(&text).map_err(|e| e.to_string()))
        {
            Ok(ConfigFile::V1(cfg)) => {
                self.cfg = cfg;
                self.status = format!("Открыт {}", path.display());
                self.path = Some(path);
                self.dirty = false;
                self.section = Section::Server;
                self.selected_register = None;
            }
            Err(e) => self.status = format!("Ошибка открытия: {e}"),
        }
    }

    fn save(&mut self, save_as: bool) {
        let path = if save_as || self.path.is_none() {
            let Some(p) = rfd::FileDialog::new()
                .add_filter("JSON", &["json"])
                .set_file_name("config.json")
                .save_file()
            else {
                return;
            };
            p
        } else {
            self.path.clone().unwrap()
        };
        let file = ConfigFile::V1(self.cfg.clone());
        match serde_json::to_string_pretty(&file)
            .map_err(|e| e.to_string())
            .and_then(|json| std::fs::write(&path, json).map_err(|e| e.to_string()))
        {
            Ok(()) => {
                self.status = format!("Сохранено в {}", path.display());
                self.path = Some(path);
                self.dirty = false;
            }
            Err(e) => self.status = format!("Ошибка сохранения: {e}"),
        }
    }

    // ---------- панели ----------

    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let running = self.server.is_running() || self.server.is_busy();
                ui.add_enabled_ui(!running, |ui| {
                    if ui.button("Новый").clicked() {
                        let logs = self.logs.clone();
                        *self = ConfigApp::new(logs);
                    }
                    if ui.button("Открыть…").clicked() {
                        self.open();
                    }
                });
                if ui.button("Сохранить").clicked() {
                    self.save(false);
                }
                if ui.button("Сохранить как…").clicked() {
                    self.save(true);
                }
                ui.separator();
                ui.label("Адреса:");
                ui.selectable_value(&mut self.addr_hex, false, "Dec");
                ui.selectable_value(&mut self.addr_hex, true, "Hex");
                ui.separator();
                let name = self
                    .path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(без файла)".into());
                let dirty = if self.dirty { " *" } else { "" };
                ui.label(RichText::new(format!("{name}{dirty}")).monospace());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(&self.status);
                });
            });
        });
    }

    fn left_nav(&mut self, ctx: &egui::Context) {
        let current = self.section;
        let mut clicked: Option<Section> = None;
        fn item(
            ui: &mut egui::Ui,
            current: Section,
            clicked: &mut Option<Section>,
            label: String,
            target: Section,
            indent: f32,
        ) {
            ui.horizontal(|ui| {
                ui.add_space(indent);
                if ui.selectable_label(current == target, label).clicked() {
                    *clicked = Some(target);
                }
            });
        }

        egui::SidePanel::left("nav").min_width(230.0).show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.heading("Разделы");
                let srv = if self.server.is_running() { "● Сервер" } else { "○ Сервер" };
                item(ui, current, &mut clicked, srv.into(), Section::Server, 0.0);
                item(ui, current, &mut clicked, "Сервер OPC UA".into(), Section::OpcUa, 0.0);
                item(ui, current, &mut clicked, "Общие и логи".into(), Section::General, 0.0);
                item(ui, current, &mut clicked, "Группы опроса".into(), Section::PollGroups, 0.0);

                ui.separator();
                ui.horizontal(|ui| {
                    ui.heading("Каналы");
                    if ui.button("+").on_hover_text("Добавить канал").clicked() {
                        let n = self.cfg.channels.len() + 1;
                        self.cfg.channels.push(new_channel(&format!("channel-{n}")));
                        clicked = Some(Section::Channel(self.cfg.channels.len() - 1));
                        self.dirty = true;
                    }
                });
                for (ci, ch) in self.cfg.channels.iter().enumerate() {
                    let ch_label = format!("{} {}", if ch.enabled { "▶" } else { "⏸" }, ch.id);
                    item(ui, current, &mut clicked, ch_label, Section::Channel(ci), 8.0);
                    for (di, dev) in ch.devices.iter().enumerate() {
                        let label = format!("{} (unit {})", dev.id, dev.unit_id);
                        item(ui, current, &mut clicked, label, Section::Device(ci, di), 28.0);
                    }
                }
            });
        });

        if let Some(target) = clicked {
            self.section = target;
            self.selected_register = None;
        }
    }

    fn validation_panel(&mut self, ctx: &egui::Context) {
        let mut problems: Vec<(bool, String)> = gateway_config::validate::validate(&self.cfg)
            .iter()
            .map(|e| (e.is_warning(), e.to_string()))
            .collect();
        // B5: формулы компилируются тем же движком, что и на старте сервера —
        // синтаксические ошибки видны сразу, а не только при запуске.
        let regs = self
            .cfg
            .channels
            .iter()
            .flat_map(|c| &c.devices)
            .flat_map(|d| &d.registers)
            .map(|r| (r.tag.as_str(), r.formula.as_deref(), r.write_formula.as_deref()));
        for msg in tags_core::check_config_formulas(regs) {
            problems.push((false, format!("формула: {msg}")));
        }

        egui::TopBottomPanel::bottom("validation")
            .resizable(true)
            .default_height(120.0)
            .show(ctx, |ui| {
                let errors = problems.iter().filter(|(w, _)| !w).count();
                let warns = problems.len() - errors;
                ui.horizontal(|ui| {
                    ui.heading("Проверка");
                    if errors == 0 {
                        ui.colored_label(Color32::from_rgb(0, 160, 0), "✓ ошибок нет");
                    } else {
                        ui.colored_label(Color32::RED, format!("✗ ошибок: {errors}"));
                    }
                    if warns > 0 {
                        ui.colored_label(
                            Color32::from_rgb(200, 140, 0),
                            format!("предупреждений: {warns}"),
                        );
                    }
                });
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (warn, text) in &problems {
                        let (mark, color) = if *warn {
                            ("⚠", Color32::from_rgb(200, 140, 0))
                        } else {
                            ("✗", Color32::RED)
                        };
                        ui.colored_label(color, format!("{mark} {text}"));
                    }
                });
            });
    }

    fn central(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| match self.section {
                Section::Server => self.ui_server(ui),
                Section::OpcUa => self.ui_opcua(ui),
                Section::General => self.ui_general(ui),
                Section::PollGroups => self.ui_poll_groups(ui),
                Section::Channel(ci) => self.ui_channel(ui, ci),
                Section::Device(ci, di) => self.ui_device(ui, ci, di),
            });
        });
    }

    // ---------- раздел: встроенный сервер ----------

    fn ui_server(&mut self, ui: &mut egui::Ui) {
        ui.heading("Сервер");
        // Кнопки мутируют self.server; собираем намерение и применяем после
        // разбора состояния (иначе конфликт заимствования с &self.server.state).
        let mut do_start = false;
        let mut do_stop = false;
        let mut open_chart: Option<String> = None;
        match &self.server.state {
            ServerState::Stopped => {
                ui.label("Остановлен.");
                if let Some(err) = &self.server.last_error {
                    ui.colored_label(Color32::RED, format!("Последняя ошибка: {err}"));
                }
                let hint_unsaved = self.path.is_none() || self.dirty;
                if hint_unsaved {
                    ui.colored_label(
                        Color32::from_rgb(200, 140, 0),
                        "Конфиг не сохранён — относительные пути (./data, ./logs, pki) \
                         будут считаться от текущего каталога.",
                    );
                }
                if ui.button("▶ Запустить сервер").clicked() {
                    do_start = true;
                }
            }
            ServerState::Starting => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Запуск...");
                });
            }
            ServerState::Stopping => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Останов...");
                });
            }
            ServerState::Running { stack, since } => {
                let uptime = since.elapsed().as_secs();
                ui.horizontal(|ui| {
                    ui.colored_label(Color32::from_rgb(0, 160, 0), "● Работает");
                    ui.label(format!("время работы: {}с", uptime));
                    if ui.button("⏹ Остановить").clicked() {
                        do_stop = true;
                    }
                });
                if let Some(url) = stack.endpoint_url() {
                    ui.horizontal(|ui| {
                        ui.label("Эндпоинт:");
                        ui.monospace(url);
                    });
                }
                if let Some(rem) = stack.trial_remaining() {
                    let s = rem.as_secs();
                    ui.colored_label(
                        Color32::from_rgb(200, 140, 0),
                        format!(
                            "Пробная сборка: до автоостанова {:02}:{:02}:{:02}",
                            s / 3600,
                            (s % 3600) / 60,
                            s % 60
                        ),
                    );
                }
                // Клиенты OPC UA.
                if let Some(op) = stack.opcua() {
                    ui.separator();
                    ui.label(RichText::new(format!("Клиентов подключено: {}", op.session_count())).strong());
                    let events = op.recent_authentications();
                    if !events.is_empty() {
                        ui.label("Недавние подключения:");
                        egui::Grid::new("sessions").num_columns(3).striped(true).show(ui, |ui| {
                            ui.label(RichText::new("Пользователь").strong());
                            ui.label(RichText::new("Эндпоинт").strong());
                            ui.label(RichText::new("Политика").strong());
                            ui.end_row();
                            for e in events.iter().rev().take(20) {
                                ui.label(&e.user);
                                ui.label(&e.endpoint_path);
                                ui.label(format!("{} / {}", e.security_policy, e.security_mode));
                                ui.end_row();
                            }
                        });
                    }
                }
                // Метрики каналов.
                let metrics = stack.channel_metrics();
                if !metrics.is_empty() {
                    ui.separator();
                    ui.label(RichText::new("Каналы Modbus").strong());
                    egui::Grid::new("chan-metrics").num_columns(6).striped(true).show(ui, |ui| {
                        for h in ["Канал", "OK", "Ошибки", "Таймауты", "Исключения", "Реконнекты"] {
                            ui.label(RichText::new(h).strong());
                        }
                        ui.end_row();
                        for (name, m) in &metrics {
                            let s = m.snapshot();
                            ui.label(name);
                            ui.label(s.reqs_ok.to_string());
                            ui.label(s.reqs_err.to_string());
                            ui.label(s.timeouts.to_string());
                            ui.label(s.exceptions.to_string());
                            ui.label(s.reconnects.to_string());
                            ui.end_row();
                        }
                    });
                }
                // Живые значения тегов.
                let snaps = {
                    use tags_core::TypedReader;
                    let typed = stack.typed();
                    let mut v = typed.snapshot_all();
                    v.sort_by_key(|(t, _)| t.0);
                    v.into_iter()
                        .filter_map(|(t, s)| typed.name(t).map(|n| (n.to_string(), s)))
                        .collect::<Vec<_>>()
                };
                if !snaps.is_empty() {
                    ui.separator();
                    ui.label(RichText::new(format!("Теги ({})", snaps.len())).strong());
                    egui::ScrollArea::vertical().max_height(260.0).id_salt("tags").show(ui, |ui| {
                        egui::Grid::new("tag-values").num_columns(5).striped(true).show(ui, |ui| {
                            for h in ["Тег", "Значение", "Качество", "Возраст", "График"] {
                                ui.label(RichText::new(h).strong());
                            }
                            ui.end_row();
                            let now = Instant::now();
                            for (name, s) in &snaps {
                                ui.label(name);
                                ui.monospace(format!("{}", s.value));
                                ui.label(quality_label(s.quality));
                                let age = now.saturating_duration_since(s.mono).as_millis();
                                ui.label(format!("{age} мс"));
                                // График доступен только для числовых тегов.
                                if s.value.as_f64().is_some() {
                                    if ui.button("📈").on_hover_text("Открыть график").clicked() {
                                        open_chart = Some(name.clone());
                                    }
                                } else {
                                    ui.label("—");
                                }
                                ui.end_row();
                            }
                        });
                    });
                }
            }
        }

        // Лог сервера.
        ui.separator();
        ui.label(RichText::new("Журнал").strong());
        egui::ScrollArea::vertical().max_height(200.0).id_salt("log").stick_to_bottom(true).show(ui, |ui| {
            let lines = self.logs.lock();
            for line in lines.iter() {
                ui.label(RichText::new(line).monospace().small());
            }
        });

        if let Some(tag) = open_chart {
            self.open_chart(tag);
        }
        if do_start {
            self.start_server();
        }
        if do_stop {
            self.server.stop();
        }
    }

    /// Открыть (или вынести наверх) окно графика для тега.
    fn open_chart(&mut self, tag: String) {
        if let Some(existing) = self.charts.iter_mut().find(|c| c.tag == tag) {
            existing.open = true; // уже открыт — просто оставляем видимым
            return;
        }
        // Единицы измерения тега — для подписи оси, если заданы в конфиге.
        let units = self
            .cfg
            .channels
            .iter()
            .flat_map(|c| c.devices.iter())
            .flat_map(|d| d.registers.iter())
            .find(|r| r.tag == tag)
            .and_then(|r| r.units.clone());
        self.charts.push(crate::history::ChartWindow::new(tag, units));
    }

    /// Записать текущие значения наблюдаемых (открытых в графиках) тегов.
    fn record_history(&mut self) {
        if self.charts.is_empty() {
            return;
        }
        let Some(typed) = self.server.typed() else {
            return;
        };
        use tags_core::TypedReader;
        let now = Instant::now();
        for (tag, snap) in typed.snapshot_all() {
            if let Some(name) = typed.name(tag) {
                if self.charts.iter().any(|c| c.tag == name) {
                    self.history.record(name, &snap, now);
                }
            }
        }
    }

    /// Отрисовать окна графиков; закрытые — убрать и освободить их серии.
    fn chart_windows(&mut self, ctx: &egui::Context) {
        for chart in &mut self.charts {
            chart.show(ctx, &self.history);
        }
        let mut i = 0;
        while i < self.charts.len() {
            if self.charts[i].open {
                i += 1;
            } else {
                let c = self.charts.remove(i);
                self.history.forget(&c.tag);
            }
        }
    }

    /// Собрать конфиг из текущего состояния и запустить стек.
    fn start_server(&mut self) {
        // Сохранённый и не грязный — грузим с диска (сработает перебазирование
        // относительных путей на каталог конфига, B7). Иначе — из памяти.
        let resolved = if let (Some(path), false) = (&self.path, self.dirty) {
            gateway_config::load(path).map_err(|e| e.to_string())
        } else {
            serde_json::to_string(&ConfigFile::V1(self.cfg.clone()))
                .map_err(|e| e.to_string())
                .and_then(|json| gateway_config::load_str(&json).map_err(|e| e.to_string()))
        };
        match resolved {
            Ok(cfg) => self.server.start(cfg),
            Err(e) => self.server.last_error = Some(format!("конфиг: {e}")),
        }
    }

    // ---------- раздел: OPC UA ----------

    fn ui_opcua(&mut self, ui: &mut egui::Ui) {
        ui.heading("Сервер OPC UA");
        let o = &mut self.cfg.opcua;
        let before = format!("{o:?}");
        egui::Grid::new("opcua").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label("Включён");
            ui.checkbox(&mut o.enabled, "");
            ui.end_row();
            ui.label("Адрес привязки (host)");
            ui.text_edit_singleline(&mut o.host);
            ui.end_row();
            ui.label("Рекламируемый адрес");
            opt_string_edit(ui, &mut o.advertised_host, "имя/IP для клиентов (если host=0.0.0.0)");
            ui.end_row();
            ui.label("Порт");
            ui.add(egui::DragValue::new(&mut o.port));
            ui.end_row();
            ui.label("Имя приложения");
            ui.text_edit_singleline(&mut o.application_name);
            ui.end_row();
            ui.label("URI приложения");
            ui.text_edit_singleline(&mut o.application_uri);
            ui.end_row();
            ui.label("Эндпоинт без шифрования");
            ui.checkbox(&mut o.allow_none_security, "для наладки/изолированных сетей");
            ui.end_row();
            ui.label("Basic256Sha256 + Aes256");
            ui.checkbox(&mut o.basic256sha256, "шифрованные эндпоинты");
            ui.end_row();
            ui.label("Анонимный доступ");
            ui.checkbox(&mut o.allow_anonymous, "");
            ui.end_row();
            ui.label("Каталог PKI");
            ui.text_edit_singleline(&mut o.pki_dir);
            ui.end_row();
            ui.label("Доверять любым сертификатам");
            ui.checkbox(&mut o.trust_any_client_cert, "ТОЛЬКО на время пусконаладки");
            ui.end_row();
        });

        ui.separator();
        ui.horizontal(|ui| {
            ui.heading("Пользователи");
            if ui.button("+ Добавить").clicked() {
                o.users.push(OpcUaUser {
                    username: format!("user{}", o.users.len() + 1),
                    password: Some(String::new()),
                    password_hash: None,
                });
            }
        });
        let mut remove: Option<usize> = None;
        let mut open_hash: Option<usize> = None;
        for (i, u) in o.users.iter_mut().enumerate() {
            ui.group(|ui| {
                egui::Grid::new(("user", i)).num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
                    ui.label("Логин");
                    ui.text_edit_singleline(&mut u.username);
                    ui.end_row();
                    let mut hashed = u.password_hash.is_some();
                    ui.label("Хранение пароля");
                    ui.horizontal(|ui| {
                        if ui.radio_value(&mut hashed, false, "открытый (наладка)").changed()
                            || ui.radio_value(&mut hashed, true, "хэш argon2id").changed()
                        {
                            if hashed {
                                u.password = None;
                                u.password_hash = Some(String::new());
                            } else {
                                u.password = Some(String::new());
                                u.password_hash = None;
                            }
                        }
                    });
                    ui.end_row();
                    if let Some(p) = &mut u.password {
                        ui.label("Пароль");
                        ui.text_edit_singleline(p);
                        ui.end_row();
                    }
                    if let Some(h) = &mut u.password_hash {
                        ui.label("password_hash");
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(h).desired_width(360.0));
                            if ui.button("Сгенерировать…").clicked() {
                                open_hash = Some(i);
                            }
                        });
                        ui.end_row();
                    }
                });
                if ui.button("Удалить пользователя").clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            o.users.remove(i);
        }
        if let Some(i) = open_hash {
            self.hash_dialog = Some(HashDialog { user_idx: i, ..Default::default() });
        }
        if before != format!("{:?}", self.cfg.opcua) {
            self.dirty = true;
        }
    }

    fn ui_general(&mut self, ui: &mut egui::Ui) {
        ui.heading("Общие");
        let before = (format!("{:?}", self.cfg.gateway), format!("{:?}", self.cfg.logging));
        egui::Grid::new("gateway").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label("Имя экземпляра");
            ui.text_edit_singleline(&mut self.cfg.gateway.instance_name);
            ui.end_row();
            ui.label("Каталог данных (retentive)");
            ui.text_edit_singleline(&mut self.cfg.gateway.data_dir);
            ui.end_row();
        });
        ui.label("Retry по умолчанию (шлюз):");
        retry_edit(ui, "gw-retry", &mut self.cfg.gateway.default_retry);

        ui.separator();
        ui.heading("Логи");
        egui::Grid::new("logging").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label("Фильтр (level)");
            ui.text_edit_singleline(&mut self.cfg.logging.level);
            ui.end_row();
            ui.label("Каталог файлов логов");
            opt_string_edit(ui, &mut self.cfg.logging.dir, "(пусто — только консоль)");
            ui.end_row();
            ui.label("Префикс файла");
            ui.text_edit_singleline(&mut self.cfg.logging.file_prefix);
            ui.end_row();
        });
        ui.label(
            RichText::new(
                "Подсказка: \"info,modbus_traffic=debug\" + флаг «дамп трафика» на канале — \
                 hex-лог кадров Modbus.",
            )
            .weak(),
        );
        if before != (format!("{:?}", self.cfg.gateway), format!("{:?}", self.cfg.logging)) {
            self.dirty = true;
        }
    }

    fn ui_poll_groups(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Группы опроса");
            if ui.button("+ Добавить").clicked() {
                let n = self.cfg.poll_groups.len() + 1;
                self.cfg.poll_groups.push(PollGroupConfig {
                    id: format!("group-{n}"),
                    period_ms: 1000,
                    priority: 0,
                });
                self.dirty = true;
            }
        });
        let mut remove = None;
        egui::Grid::new("pg").num_columns(4).spacing([12.0, 4.0]).striped(true).show(ui, |ui| {
            ui.label(RichText::new("Имя").strong());
            ui.label(RichText::new("Период, мс").strong());
            ui.label(RichText::new("Приоритет").strong());
            ui.label("");
            ui.end_row();
            for (i, g) in self.cfg.poll_groups.iter_mut().enumerate() {
                if ui.text_edit_singleline(&mut g.id).changed() {
                    self.dirty = true;
                }
                if ui.add(egui::DragValue::new(&mut g.period_ms).range(1..=3_600_000)).changed() {
                    self.dirty = true;
                }
                if ui.add(egui::DragValue::new(&mut g.priority)).changed() {
                    self.dirty = true;
                }
                if ui.button("Удалить").clicked() {
                    remove = Some(i);
                }
                ui.end_row();
            }
        });
        if let Some(i) = remove {
            self.cfg.poll_groups.remove(i);
            self.dirty = true;
        }
    }

    fn ui_channel(&mut self, ui: &mut egui::Ui, ci: usize) {
        let ports = self.ports.clone();
        let mut refresh_ports = false;
        let Some(ch) = self.cfg.channels.get_mut(ci) else {
            ui.label("Канал удалён");
            return;
        };
        let before = format!("{ch:?}");
        ui.horizontal(|ui| {
            ui.heading(format!("Канал: {}", ch.id));
            if ui.button("+ Устройство").clicked() {
                let n = ch.devices.len() + 1;
                ch.devices.push(new_device(&format!("device-{n}"), n as u8));
            }
        });
        egui::Grid::new(("ch", ci)).num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label("Имя (id)");
            ui.text_edit_singleline(&mut ch.id);
            ui.end_row();
            ui.label("Включён");
            ui.checkbox(&mut ch.enabled, "");
            ui.end_row();
            ui.label("Транспорт");
            transport_edit(ui, ci, &mut ch.transport, &ports, &mut refresh_ports);
            ui.end_row();
            ui.label("Таймаут запроса, мс");
            ui.add(egui::DragValue::new(&mut ch.request_timeout_ms).range(50..=60_000));
            ui.end_row();
            ui.label("Пауза между запросами, мс");
            ui.add(egui::DragValue::new(&mut ch.inter_request_delay_ms).range(0..=1000));
            ui.end_row();
            ui.label("Слияние дыр адресов (max_gap)");
            ui.add(egui::DragValue::new(&mut ch.max_gap).range(0..=125));
            ui.end_row();
            ui.label("Оффлайн после N отказов");
            ui.add(egui::DragValue::new(&mut ch.offline_after_failures).range(1..=100));
            ui.end_row();
            ui.label("Дамп трафика (hex)");
            ui.checkbox(&mut ch.log_traffic, "target modbus_traffic (debug)");
            ui.end_row();
        });
        ui.label("Retry канала (переопределяет шлюз):");
        opt_retry_edit(ui, ("ch-retry", ci), &mut ch.retry);

        ui.separator();
        let delete = ui
            .button(RichText::new("Удалить канал").color(Color32::RED))
            .clicked();
        let changed = before != format!("{:?}", self.cfg.channels.get(ci));
        if refresh_ports {
            self.ports = mb_proto::available_serial_ports();
        }
        if delete {
            self.cfg.channels.remove(ci);
            self.section = Section::Server;
            self.dirty = true;
        } else if changed {
            self.dirty = true;
        }
    }

    fn ui_device(&mut self, ui: &mut egui::Ui, ci: usize, di: usize) {
        let poll_groups: Vec<PollGroupConfig> = self.cfg.poll_groups.clone();
        let group_names: Vec<String> = poll_groups.iter().map(|g| g.id.clone()).collect();
        let transport = self.cfg.channels.get(ci).map(|c| c.transport.clone());
        let ch_timeout = self.cfg.channels.get(ci).map(|c| c.request_timeout_ms).unwrap_or(1000);
        // Сводка опроса (F4): сколько запросов Modbus даёт устройство после коалесинга.
        let txn_summary = self.device_txn_count(ci, di);

        let Some(dev) = self.cfg.channels.get_mut(ci).and_then(|c| c.devices.get_mut(di)) else {
            ui.label("Устройство удалено");
            return;
        };
        let before = format!("{dev:?}");
        // Разрешённый таймаут для пробного чтения (B9): устройство → канал.
        let effective_timeout = dev.request_timeout_ms.unwrap_or(ch_timeout);

        ui.horizontal(|ui| {
            ui.heading(format!("Устройство: {}", dev.id));
            if let Some(t) = transport {
                if ui
                    .button("▶ Пробное чтение")
                    .on_hover_text("Подключиться и прочитать все регистры один раз")
                    .clicked()
                {
                    self.test_poll = Some(TestPoll::start(t, dev.clone(), effective_timeout));
                }
            }
        });
        egui::Grid::new(("dev", ci, di)).num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label("Имя (id)");
            ui.text_edit_singleline(&mut dev.id);
            ui.end_row();
            ui.label("Адрес Modbus (unit id)");
            ui.add(egui::DragValue::new(&mut dev.unit_id).range(0..=247));
            ui.end_row();
            ui.label("Включено");
            ui.checkbox(&mut dev.enabled, "");
            ui.end_row();
            ui.label("Таймаут, мс (переопр.)");
            opt_num_edit_u64(ui, &mut dev.request_timeout_ms, 50..=60_000);
            ui.end_row();
            ui.label("Оффлайн после N (переопр.)");
            opt_num_edit_u32(ui, &mut dev.offline_after_failures, 1..=100);
            ui.end_row();
            ui.label("max_gap (переопр.)");
            opt_num_edit_u16(ui, &mut dev.max_gap, 0..=125);
            ui.end_row();
        });
        // B8: retry устройства.
        ui.label("Retry устройства (переопределяет канал):");
        opt_retry_edit(ui, ("dev-retry", ci, di), &mut dev.retry);

        // F4: наглядность опроса.
        ui.separator();
        let summary = match txn_summary {
            Some(m) => format!("Опрос: {} тегов → {} запросов Modbus (после слияния)", dev.registers.len(), m),
            None => format!("Опрос: {} тегов → — (исправьте ошибки конфигурации)", dev.registers.len()),
        };
        ui.label(RichText::new(summary).strong());

        ui.horizontal(|ui| {
            ui.heading(format!("Теги ({})", dev.registers.len()));
            if ui.button("+ Тег").clicked() {
                let n = dev.registers.len() + 1;
                dev.registers.push(new_register(
                    &format!("{}.tag{}", dev.id, n),
                    group_names.first().cloned().unwrap_or_default(),
                ));
                self.selected_register = Some(dev.registers.len() - 1);
            }
        });

        let period_of = |name: &str| poll_groups.iter().find(|g| g.id == name).map(|g| g.period_ms);
        egui::Grid::new(("regs", ci, di)).num_columns(5).striped(true).spacing([10.0, 3.0]).show(ui, |ui| {
            for h in ["Тег", "Функция", "Адрес", "Тип", "Группа"] {
                ui.label(RichText::new(h).strong());
            }
            ui.end_row();
            for (i, r) in dev.registers.iter().enumerate() {
                let selected = self.selected_register == Some(i);
                if ui.selectable_label(selected, &r.tag).clicked() {
                    self.selected_register = Some(i);
                }
                ui.label(function_label(r.function));
                ui.label(fmt_addr(r.address, self.addr_hex));
                ui.label(format!("{:?}", r.data_type).to_lowercase());
                let grp = match period_of(&r.poll_group) {
                    Some(p) => format!("{} ({} мс)", r.poll_group, p),
                    None => format!("{} (?)", r.poll_group),
                };
                ui.label(grp);
                ui.end_row();
            }
        });

        let mut duplicate: Option<usize> = None;
        if let Some(i) = self.selected_register {
            if let Some(r) = dev.registers.get_mut(i) {
                ui.separator();
                match register_form(ui, (ci, di, i), r, &group_names, self.addr_hex) {
                    RegAction::None => {}
                    RegAction::Delete => {
                        dev.registers.remove(i);
                        self.selected_register = None;
                    }
                    RegAction::Duplicate => duplicate = Some(i),
                }
            }
        }
        if let Some(i) = duplicate {
            let mut copy = dev.registers[i].clone();
            copy.tag = format!("{} (копия)", copy.tag);
            dev.registers.insert(i + 1, copy);
            self.selected_register = Some(i + 1);
            self.dirty = true;
        }

        ui.separator();
        let delete_dev = ui
            .button(RichText::new("Удалить устройство").color(Color32::RED))
            .clicked();
        let changed = before
            != format!("{:?}", self.cfg.channels.get(ci).and_then(|c| c.devices.get(di)));
        if delete_dev {
            self.cfg.channels[ci].devices.remove(di);
            self.section = Section::Channel(ci);
            self.dirty = true;
        } else if changed {
            self.dirty = true;
        }
    }

    /// Скомпилировать текущий конфиг и посчитать число Modbus-транзакций
    /// устройства (ci,di) по всем группам. None — если конфиг не грузится.
    fn device_txn_count(&self, ci: usize, di: usize) -> Option<usize> {
        let json = serde_json::to_string(&ConfigFile::V1(self.cfg.clone())).ok()?;
        let resolved = gateway_config::load_str(&json).ok()?;
        let plans = mb_poller::build_all(&resolved);
        let ch = self.cfg.channels.get(ci)?;
        let dev = ch.devices.get(di)?;
        let plan = plans.iter().find(|p| p.name == ch.id)?;
        // Устройства в плане — только enabled, в порядке конфига; матчим по unit.
        let dplan = plan.devices.iter().find(|d| d.unit == dev.unit_id)?;
        Some(dplan.by_group.iter().map(|(_, txns)| txns.len()).sum())
    }

    // ---------- диалоги ----------

    fn hash_dialog_window(&mut self, ctx: &egui::Context) {
        let Some(dlg) = &mut self.hash_dialog else { return };
        let mut open = true;
        let mut apply = false;
        egui::Window::new("Хэш пароля (argon2id)")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label("Пароль:");
                ui.add(egui::TextEdit::singleline(&mut dlg.password).password(true));
                if ui.button("Захэшировать").clicked() {
                    dlg.result = opcua_gateway::hash_password(&dlg.password).ok();
                }
                if let Some(h) = &dlg.result {
                    ui.label(RichText::new(h).monospace().small());
                    if ui.button("Подставить пользователю").clicked() {
                        apply = true;
                    }
                }
            });
        if apply {
            let (idx, hash) = (dlg.user_idx, dlg.result.clone());
            if let (Some(u), Some(h)) = (self.cfg.opcua.users.get_mut(idx), hash) {
                u.password = None;
                u.password_hash = Some(h);
                self.dirty = true;
            }
            self.hash_dialog = None;
            return;
        }
        if !open {
            self.hash_dialog = None;
        }
    }

    fn test_poll_window(&mut self, ctx: &egui::Context) {
        let Some(tp) = &self.test_poll else { return };
        let mut open = true;
        let mut cancel = false;
        egui::Window::new("Пробное чтение")
            .default_width(520.0)
            .open(&mut open)
            .show(ctx, |ui| {
                if tp.running {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Опрос...");
                        if ui.button("Остановить").clicked() {
                            cancel = true;
                        }
                    });
                }
                egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                    for line in &tp.lines {
                        ui.label(RichText::new(line).monospace());
                    }
                });
            });
        if cancel {
            tp.cancel();
        }
        if !open {
            self.test_poll = None; // Drop взведёт отмену фонового потока.
        }
    }
}

// ---------- форма тега (контекстная) ----------

enum RegAction {
    None,
    Delete,
    Duplicate,
}

fn register_form(
    ui: &mut egui::Ui,
    id: (usize, usize, usize),
    r: &mut RegisterEntry,
    poll_groups: &[String],
    addr_hex: bool,
) -> RegAction {
    ui.heading(format!("Тег: {}", r.tag));

    let is_coil = matches!(
        r.function,
        FunctionCode::ReadCoils | FunctionCode::ReadDiscreteInputs
    );
    let is_custom = matches!(r.function, FunctionCode::Custom { .. });
    let is_register = matches!(
        r.function,
        FunctionCode::ReadHoldingRegisters | FunctionCode::ReadInputRegisters
    );
    // Битовый домен всегда bit; для катушек тип фиксирован.
    if is_coil {
        r.data_type = DataType::Bit;
    }
    let multiword = matches!(
        r.data_type,
        DataType::U32 | DataType::I32 | DataType::U64 | DataType::I64 | DataType::F32 | DataType::F64
    );
    let numeric = !matches!(r.data_type, DataType::Ascii);
    let bit_selectable = matches!(r.data_type, DataType::Bit | DataType::Bitfield);

    // --- Источник данных ---
    ui.label(RichText::new("Источник данных").strong().underline());
    egui::Grid::new(("src", id)).num_columns(2).spacing([12.0, 5.0]).show(ui, |ui| {
        ui.label("Имя тега");
        ui.text_edit_singleline(&mut r.tag);
        ui.end_row();

        ui.label("Группа опроса");
        egui::ComboBox::from_id_salt(("pg", id))
            .selected_text(&r.poll_group)
            .show_ui(ui, |ui| {
                for g in poll_groups {
                    ui.selectable_value(&mut r.poll_group, g.clone(), g);
                }
            });
        ui.end_row();

        ui.label("Функция Modbus");
        function_combo(ui, id, &mut r.function);
        ui.end_row();

        ui.label("Адрес");
        address_edit(ui, &mut r.address, addr_hex);
        ui.end_row();

        if !is_coil && !is_custom {
            ui.label("Тип данных");
            egui::ComboBox::from_id_salt(("dt", id))
                .selected_text(format!("{:?}", r.data_type).to_lowercase())
                .show_ui(ui, |ui| {
                    for dt in [
                        DataType::U16, DataType::I16, DataType::U32, DataType::I32, DataType::U64,
                        DataType::I64, DataType::F32, DataType::F64, DataType::Bcd, DataType::Ascii,
                        DataType::Bitfield, DataType::Bit,
                    ] {
                        ui.selectable_value(&mut r.data_type, dt, format!("{dt:?}").to_lowercase());
                    }
                });
            ui.end_row();
        }

        if is_register && multiword {
            ui.label("Порядок слов");
            egui::ComboBox::from_id_salt(("wo", id))
                .selected_text(word_order_label(r.word_order))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut r.word_order, WordOrder::BigEndian, "старшее слово первым");
                    ui.selectable_value(&mut r.word_order, WordOrder::LittleEndian, "младшее слово первым (swap)");
                });
            ui.end_row();
            ui.label("Порядок байт в слове");
            egui::ComboBox::from_id_salt(("bo", id))
                .selected_text(byte_order_label(r.byte_order))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut r.byte_order, ByteOrder::BigEndian, "стандартный");
                    ui.selectable_value(&mut r.byte_order, ByteOrder::LittleEndian, "переставлены");
                });
            ui.end_row();
        }

        if is_register && matches!(r.data_type, DataType::Ascii | DataType::Bcd) {
            ui.label("Длина, регистров");
            opt_num_edit_u16(ui, &mut r.length, 1..=125);
            ui.end_row();
        }
        if is_register && bit_selectable {
            ui.label("Бит в регистре (0..15)");
            opt_num_edit_u8(ui, &mut r.bit, 0..=15);
            ui.end_row();
        }
        if is_custom {
            ui.label("Тело запроса (hex)");
            opt_string_edit(ui, &mut r.custom_request, "напр. 01 a0 ff");
            ui.end_row();
            ui.label("Ответ, байт");
            opt_num_edit_u16(ui, &mut r.custom_response_len, 1..=250);
            ui.end_row();
        }
    });

    // Информация о занимаемых регистрах (F2).
    if let Some(span) = register_span(r) {
        let end = r.address.saturating_add(span.saturating_sub(1));
        ui.label(
            RichText::new(format!(
                "Занимает {span} рег.: адреса {}..{}",
                fmt_addr(r.address, addr_hex),
                fmt_addr(end, addr_hex)
            ))
            .weak(),
        );
    }

    // --- Преобразование (только числовые не-custom) ---
    if !is_custom && numeric && !is_coil {
        ui.add_space(6.0);
        ui.label(RichText::new("Преобразование").strong().underline());
        egui::Grid::new(("xform", id)).num_columns(2).spacing([12.0, 5.0]).show(ui, |ui| {
            ui.label("Масштаб (scale)");
            ui.add(egui::DragValue::new(&mut r.scale).speed(0.01));
            ui.end_row();
            ui.label("Смещение (offset)");
            ui.add(egui::DragValue::new(&mut r.offset).speed(0.01));
            ui.end_row();
            ui.label("Формула (raw, tag(\"...\"))");
            opt_string_edit(ui, &mut r.formula, "(пусто — линейная scale/offset)");
            ui.end_row();
            ui.label("Deadband");
            opt_num_edit_f64(ui, &mut r.deadband);
            ui.end_row();
            ui.label("Ед. изм. (units)");
            opt_string_edit(ui, &mut r.units, "");
            ui.end_row();
        });
        formula_help(ui, id);
    } else if is_custom {
        ui.add_space(6.0);
        ui.label(
            RichText::new("Vendor-функция: данные публикуются как байты (ByteString), без преобразования.")
                .weak(),
        );
    }

    // --- Хранение ---
    ui.add_space(6.0);
    ui.label(RichText::new("Хранение").strong().underline());
    egui::Grid::new(("store", id)).num_columns(2).spacing([12.0, 5.0]).show(ui, |ui| {
        ui.label("Retentive (переживает рестарт)");
        ui.checkbox(&mut r.retentive, "");
        ui.end_row();
        ui.label("История, последних N");
        opt_num_edit_u16(ui, &mut r.retain_last, 1..=1000);
        ui.end_row();
    });

    // --- Запись (только holding/coils) ---
    if is_register || matches!(r.function, FunctionCode::ReadCoils) {
        ui.add_space(6.0);
        ui.label(RichText::new("Запись").strong().underline());
        let coil_src = matches!(r.function, FunctionCode::ReadCoils);
        egui::Grid::new(("write", id)).num_columns(2).spacing([12.0, 5.0]).show(ui, |ui| {
            ui.label("Запись из OPC UA");
            ui.checkbox(&mut r.writable, "writable");
            ui.end_row();
            // Функция записи видна всегда (для holding/coil), но активна только
            // при включённой записи — чтобы выбор FC05/06/15/16 был очевиден.
            if coil_src || matches!(r.function, FunctionCode::ReadHoldingRegisters) {
                ui.label("Функция записи");
                ui.add_enabled_ui(r.writable, |ui| {
                    write_function_combo(ui, id, &mut r.write_function, coil_src);
                });
                ui.end_row();
            }
            if r.writable && r.formula.is_some() {
                ui.label("Обратная формула (value)");
                opt_string_edit(ui, &mut r.write_formula, "обязательна при formula");
                ui.end_row();
            }
        });
        if coil_src {
            ui.label(
                RichText::new(
                    "FC05 — одна катушка; FC15 — «запись нескольких» (нужна некоторым PLC).",
                )
                .weak(),
            );
        } else {
            ui.label(
                RichText::new(
                    "FC06 — один регистр (для 1-слового типа); FC16 — «запись нескольких» \
                     (обязательна для u32/i32/f32/… и форсится для одиночного, если требует устройство).",
                )
                .weak(),
            );
        }
    } else {
        r.writable = false;
    }

    ui.add_space(8.0);
    let mut action = RegAction::None;
    ui.horizontal(|ui| {
        if ui.button("Дублировать тег").clicked() {
            action = RegAction::Duplicate;
        }
        if ui.button(RichText::new("Удалить тег").color(Color32::RED)).clicked() {
            action = RegAction::Delete;
        }
    });
    action
}

/// Сколько регистров/бит занимает тег (для инфо-строки). None для custom.
fn register_span(r: &RegisterEntry) -> Option<u16> {
    match r.function {
        FunctionCode::ReadCoils | FunctionCode::ReadDiscreteInputs => Some(1),
        FunctionCode::ReadHoldingRegisters | FunctionCode::ReadInputRegisters => {
            Some(r.data_type.register_count().or(r.length).unwrap_or(1))
        }
        _ => None,
    }
}

fn transport_edit(
    ui: &mut egui::Ui,
    ci: usize,
    t: &mut TransportConfig,
    ports: &[String],
    refresh: &mut bool,
) {
    ui.vertical(|ui| {
        let current = match t {
            TransportConfig::Tcp { .. } => "Modbus TCP",
            TransportConfig::Rtu { .. } => "Modbus RTU (COM)",
            TransportConfig::RtuOverTcp { .. } => "RTU поверх TCP",
        };
        egui::ComboBox::from_id_salt(("tr", ci)).selected_text(current).show_ui(ui, |ui| {
            if ui.selectable_label(matches!(t, TransportConfig::Tcp { .. }), "Modbus TCP").clicked() {
                *t = TransportConfig::Tcp { host: "192.168.0.10".into(), port: 502, connect_timeout_ms: 5000 };
            }
            if ui.selectable_label(matches!(t, TransportConfig::Rtu { .. }), "Modbus RTU (COM)").clicked() {
                *t = TransportConfig::Rtu { path: default_serial_path(), baud: 9600, data_bits: 8, parity: Parity::None, stop_bits: 1 };
            }
            if ui.selectable_label(matches!(t, TransportConfig::RtuOverTcp { .. }), "RTU поверх TCP").clicked() {
                *t = TransportConfig::RtuOverTcp { host: "192.168.0.20".into(), port: 4001, connect_timeout_ms: 5000 };
            }
        });
        match t {
            TransportConfig::Tcp { host, port, connect_timeout_ms }
            | TransportConfig::RtuOverTcp { host, port, connect_timeout_ms } => {
                egui::Grid::new(("tr-tcp", ci)).num_columns(2).show(ui, |ui| {
                    ui.label("Хост");
                    ui.text_edit_singleline(host);
                    ui.end_row();
                    ui.label("Порт");
                    ui.add(egui::DragValue::new(port));
                    ui.end_row();
                    ui.label("Таймаут подключения, мс");
                    ui.add(egui::DragValue::new(connect_timeout_ms).range(100..=60_000));
                    ui.end_row();
                });
            }
            TransportConfig::Rtu { path, baud, data_bits, parity, stop_bits } => {
                egui::Grid::new(("tr-rtu", ci)).num_columns(2).show(ui, |ui| {
                    ui.label("Порт");
                    ui.horizontal(|ui| {
                        // F1: список обнаруженных портов + ручной ввод.
                        egui::ComboBox::from_id_salt(("com", ci))
                            .selected_text(if path.is_empty() { "выбрать…" } else { path.as_str() })
                            .show_ui(ui, |ui| {
                                if ports.is_empty() {
                                    ui.label(RichText::new("порты не найдены").weak());
                                }
                                for p in ports {
                                    ui.selectable_value(path, p.clone(), p);
                                }
                            });
                        if ui.button("⟳").on_hover_text("Обновить список портов").clicked() {
                            *refresh = true;
                        }
                        ui.add(egui::TextEdit::singleline(path).desired_width(120.0).hint_text("COM3 / /dev/ttyUSB0"));
                    });
                    ui.end_row();
                    ui.label("Скорость (baud)");
                    egui::ComboBox::from_id_salt(("baud", ci))
                        .selected_text(baud.to_string())
                        .show_ui(ui, |ui| {
                            for b in [1200u32, 2400, 4800, 9600, 19200, 38400, 57600, 115200] {
                                ui.selectable_value(baud, b, b.to_string());
                            }
                        });
                    ui.end_row();
                    ui.label("Биты данных");
                    ui.add(egui::DragValue::new(data_bits).range(5..=8));
                    ui.end_row();
                    ui.label("Чётность");
                    egui::ComboBox::from_id_salt(("par", ci))
                        .selected_text(match parity {
                            Parity::None => "нет",
                            Parity::Even => "чёт",
                            Parity::Odd => "нечёт",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(parity, Parity::None, "нет");
                            ui.selectable_value(parity, Parity::Even, "чёт");
                            ui.selectable_value(parity, Parity::Odd, "нечёт");
                        });
                    ui.end_row();
                    ui.label("Стоп-биты");
                    ui.add(egui::DragValue::new(stop_bits).range(1..=2));
                    ui.end_row();
                });
            }
        }
    });
}

fn function_combo(ui: &mut egui::Ui, id: (usize, usize, usize), f: &mut FunctionCode) {
    egui::ComboBox::from_id_salt(("fc", id)).selected_text(function_label(*f)).show_ui(ui, |ui| {
        for (fc, label) in [
            (FunctionCode::ReadCoils, "01 катушки (coils)"),
            (FunctionCode::ReadDiscreteInputs, "02 дискр. входы"),
            (FunctionCode::ReadHoldingRegisters, "03 holding-регистры"),
            (FunctionCode::ReadInputRegisters, "04 input-регистры"),
        ] {
            ui.selectable_value(f, fc, label);
        }
        let is_custom = matches!(f, FunctionCode::Custom { .. });
        if ui.selectable_label(is_custom, "произвольная (custom)").clicked() && !is_custom {
            *f = FunctionCode::Custom { code: 0x41 };
        }
    });
    if let FunctionCode::Custom { code } = f {
        ui.horizontal(|ui| {
            ui.label("код:");
            ui.add(egui::DragValue::new(code).range(1..=0x7F));
        });
    }
}

/// Выбор функции записи для writable-тега. «Авто» (None) выбирает FC05 для
/// катушки и FC06/FC16 для holding по ширине типа; переопределение форсит
/// конкретный код (напр. FC15 для катушки или FC16 для одиночного регистра).
fn write_function_combo(
    ui: &mut egui::Ui,
    id: (usize, usize, usize),
    wf: &mut Option<FunctionCode>,
    is_coil: bool,
) {
    let label = match wf {
        None if is_coil => "Авто (05)",
        None => "Авто (06/16)",
        Some(FunctionCode::WriteSingleCoil) => "05 одна катушка",
        Some(FunctionCode::WriteMultipleCoils) => "15 неск. катушек",
        Some(FunctionCode::WriteSingleRegister) => "06 один регистр",
        Some(FunctionCode::WriteMultipleRegisters) => "16 неск. регистров",
        Some(_) => "?",
    };
    egui::ComboBox::from_id_salt(("wf", id)).selected_text(label).show_ui(ui, |ui| {
        ui.selectable_value(wf, None, if is_coil { "Авто (05)" } else { "Авто (06/16)" });
        if is_coil {
            ui.selectable_value(wf, Some(FunctionCode::WriteSingleCoil), "05 write_single_coil");
            ui.selectable_value(
                wf,
                Some(FunctionCode::WriteMultipleCoils),
                "15 write_multiple_coils",
            );
        } else {
            ui.selectable_value(
                wf,
                Some(FunctionCode::WriteSingleRegister),
                "06 write_single_register",
            );
            ui.selectable_value(
                wf,
                Some(FunctionCode::WriteMultipleRegisters),
                "16 write_multiple_registers",
            );
        }
    });
}

/// Раскрывающаяся справка по синтаксису формул (evalexpr).
fn formula_help(ui: &mut egui::Ui, id: (usize, usize, usize)) {
    egui::CollapsingHeader::new("❓ Справка по формулам")
        .id_salt(("fh", id))
        .show(ui, |ui| {
            ui.label(RichText::new("Прямая формула (поле «Формула») — переменная raw").strong());
            ui.label(
                "raw — декодированное числовое значение регистра (с учётом типа данных и \
                 порядка слов/байт, ДО scale/offset). Результат становится значением тега. \
                 Если формула задана, scale/offset игнорируются.",
            );
            ui.label(
                "tag(\"устройство.тег\") — текущее числовое значение другого тега \
                 (для межтеговых расчётов; если тег недоступен/не число — качество \
                 деградирует, мусор не публикуется).",
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new("Обратная формула (write_formula) — переменная value").strong(),
            );
            ui.label(
                "value — значение, записанное OPC UA-клиентом; формула должна вернуть «сырое» \
                 число для устройства. Обязательна, если задана прямая формула \
                 (автоматически инвертировать её нельзя).",
            );
            ui.add_space(4.0);
            ui.label(RichText::new("Операторы").strong());
            ui.label(
                "+  -  *  /  %(остаток)  ^(степень)   ·   сравнения  == != < <= > >=   ·   \
                 логика  && || !   ·   скобки ( )",
            );
            ui.add_space(4.0);
            ui.label(RichText::new("Функции").strong());
            ui.label("if(условие, то, иначе) · min(a, b, …) · max(a, b, …) · round(x) · floor(x) · ceil(x)");
            ui.label(
                "math::abs · math::sqrt · math::pow(осн, степ) · math::ln · math::log(x, осн) · \
                 math::log2 · math::exp · math::sin / cos / tan …",
            );
            ui.add_space(4.0);
            ui.label(RichText::new("Примеры").strong());
            egui::Grid::new(("fh-ex", id)).num_columns(2).striped(true).show(ui, |ui| {
                for (expr, note) in [
                    ("raw * 0.1", "масштаб 0.1"),
                    ("raw / 10.0 - 273.15", "деци-Кельвины → °C"),
                    ("if(raw > 1000, 1000, raw)", "ограничение сверху"),
                    ("math::sqrt(raw) * 2", "квадратный корень"),
                    ("raw + tag(\"meter.offset\")", "коррекция по другому тегу"),
                    ("value * 10", "обратная к «raw * 0.1» (для записи)"),
                ] {
                    ui.monospace(expr);
                    ui.label(RichText::new(note).weak());
                    ui.end_row();
                }
            });
        });
}

/// Адрес в выбранной системе счисления (для меток/списков).
fn fmt_addr(addr: u16, hex: bool) -> String {
    if hex {
        format!("0x{addr:04X}")
    } else {
        addr.to_string()
    }
}

fn function_label(f: FunctionCode) -> String {
    match f {
        FunctionCode::ReadCoils => "01 coils".into(),
        FunctionCode::ReadDiscreteInputs => "02 discrete".into(),
        FunctionCode::ReadHoldingRegisters => "03 holding".into(),
        FunctionCode::ReadInputRegisters => "04 input".into(),
        FunctionCode::Custom { code } => format!("custom {code:#04x}"),
        other => format!("{other:?}"),
    }
}

fn quality_label(q: mb_poller::Quality) -> RichText {
    match q {
        mb_poller::Quality::Good => RichText::new("Good").color(Color32::from_rgb(0, 160, 0)),
        mb_poller::Quality::Uncertain => RichText::new("Uncertain").color(Color32::from_rgb(200, 140, 0)),
        mb_poller::Quality::Bad => RichText::new("Bad").color(Color32::RED),
    }
}

fn word_order_label(w: WordOrder) -> &'static str {
    match w {
        WordOrder::BigEndian => "старшее слово первым",
        WordOrder::LittleEndian => "младшее слово первым (swap)",
    }
}

fn byte_order_label(b: ByteOrder) -> &'static str {
    match b {
        ByteOrder::BigEndian => "стандартный",
        ByteOrder::LittleEndian => "переставлены",
    }
}

fn retry_edit(ui: &mut egui::Ui, id: impl std::hash::Hash, r: &mut RetryConfig) {
    ui.push_id(id, |ui| {
        ui.horizontal(|ui| {
            ui.label("повторов:");
            ui.add(egui::DragValue::new(&mut r.max_retries).range(0..=10));
            ui.label("пауза от, мс:");
            ui.add(egui::DragValue::new(&mut r.base_backoff_ms).range(50..=60_000));
            ui.label("до, мс:");
            ui.add(egui::DragValue::new(&mut r.max_backoff_ms).range(100..=600_000));
        });
    });
}

fn opt_retry_edit(ui: &mut egui::Ui, id: impl std::hash::Hash + Copy, r: &mut Option<RetryConfig>) {
    let mut set = r.is_some();
    ui.horizontal(|ui| {
        if ui.checkbox(&mut set, "переопределить").changed() {
            *r = if set { Some(RetryConfig::default()) } else { None };
        }
    });
    if let Some(rc) = r {
        retry_edit(ui, id, rc);
    }
}

fn opt_string_edit(ui: &mut egui::Ui, v: &mut Option<String>, hint: &str) {
    let mut buf = v.clone().unwrap_or_default();
    let resp = ui.add(egui::TextEdit::singleline(&mut buf).hint_text(hint).desired_width(320.0));
    if resp.changed() {
        *v = if buf.trim().is_empty() { None } else { Some(buf) };
    }
}

macro_rules! opt_num_edit {
    ($name:ident, $ty:ty) => {
        fn $name(ui: &mut egui::Ui, v: &mut Option<$ty>, range: std::ops::RangeInclusive<$ty>) {
            let mut set = v.is_some();
            ui.horizontal(|ui| {
                if ui.checkbox(&mut set, "").changed() {
                    *v = if set { Some(*range.start()) } else { None };
                }
                if let Some(x) = v {
                    ui.add(egui::DragValue::new(x).range(range));
                } else {
                    ui.label(RichText::new("(не задано)").weak());
                }
            });
        }
    };
}
opt_num_edit!(opt_num_edit_u64, u64);
opt_num_edit!(opt_num_edit_u32, u32);
opt_num_edit!(opt_num_edit_u16, u16);
opt_num_edit!(opt_num_edit_u8, u8);

fn opt_num_edit_f64(ui: &mut egui::Ui, v: &mut Option<f64>) {
    let mut set = v.is_some();
    ui.horizontal(|ui| {
        if ui.checkbox(&mut set, "").changed() {
            *v = if set { Some(0.0) } else { None };
        }
        if let Some(x) = v {
            ui.add(egui::DragValue::new(x).speed(0.01));
        } else {
            ui.label(RichText::new("(не задан)").weak());
        }
    });
}

fn default_serial_path() -> String {
    if cfg!(windows) { "COM3".into() } else { "/dev/ttyUSB0".into() }
}

fn new_channel(id: &str) -> ChannelConfig {
    ChannelConfig {
        id: id.into(),
        enabled: true,
        transport: TransportConfig::Tcp { host: "192.168.0.10".into(), port: 502, connect_timeout_ms: 5000 },
        request_timeout_ms: 1000,
        inter_request_delay_ms: 0,
        max_inflight: 1,
        max_gap: 0,
        log_traffic: false,
        retry: None,
        offline_after_failures: 3,
        devices: vec![],
    }
}

fn new_device(id: &str, unit: u8) -> DeviceConfig {
    DeviceConfig {
        id: id.into(),
        unit_id: unit,
        enabled: true,
        request_timeout_ms: None,
        retry: None,
        offline_after_failures: None,
        max_gap: None,
        registers: vec![],
    }
}

fn new_register(tag: &str, poll_group: String) -> RegisterEntry {
    RegisterEntry {
        tag: tag.into(),
        poll_group,
        function: FunctionCode::ReadHoldingRegisters,
        address: 0,
        data_type: DataType::U16,
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

