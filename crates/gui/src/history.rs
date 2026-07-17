//! Живая история значений тегов и окно графика (egui_plot).
//!
//! Значения наблюдаемых тегов (тех, для которых открыто окно графика)
//! складываются в in-memory кольцевой буфер по каждому тегу. Это не зависит
//! от `retain_last`/sled и даёт плавный real-time без правки серверного стека.

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use eframe::egui::{self, Color32, RichText};
use egui_plot::{Legend, Line, MarkerShape, Plot, PlotPoints, Points};
use mb_poller::Quality;
use tags_core::TypedSnapshot;

/// Максимум точек на тег (≈30 мин при 250 мс, ≈2 ч при 1 с).
const MAX_SAMPLES: usize = 7200;

#[derive(Clone, Copy)]
struct Sample {
    /// Секунды от общего epoch первой записи.
    t: f64,
    v: f64,
    q: Quality,
}

/// Кольцевые буферы значений по каждому наблюдаемому тегу.
#[derive(Default)]
pub struct History {
    epoch: Option<Instant>,
    series: HashMap<String, VecDeque<Sample>>,
}

impl History {
    /// Записать текущее значение тега (числовые — иначе пропуск).
    pub fn record(&mut self, name: &str, snap: &TypedSnapshot, now: Instant) {
        let Some(v) = snap.value.as_f64() else {
            return;
        };
        let epoch = *self.epoch.get_or_insert(now);
        let t = now.saturating_duration_since(epoch).as_secs_f64();
        let ring = self.series.entry(name.to_string()).or_default();
        // Не плодим точки с тем же t (двойной вызов в один тик).
        if ring.back().is_some_and(|s| (s.t - t).abs() < 1e-6) {
            return;
        }
        ring.push_back(Sample { t, v, q: snap.quality });
        if ring.len() > MAX_SAMPLES {
            let drop = ring.len() - MAX_SAMPLES;
            ring.drain(..drop);
        }
    }

    /// Забыть серию тега (окно графика закрыто).
    pub fn forget(&mut self, name: &str) {
        self.series.remove(name);
    }
}

#[derive(Clone, Copy, PartialEq)]
enum TimeWindow {
    S60,
    S300,
    S900,
    All,
}

impl TimeWindow {
    fn secs(self) -> Option<f64> {
        match self {
            TimeWindow::S60 => Some(60.0),
            TimeWindow::S300 => Some(300.0),
            TimeWindow::S900 => Some(900.0),
            TimeWindow::All => None,
        }
    }
    fn label(self) -> &'static str {
        match self {
            TimeWindow::S60 => "1 мин",
            TimeWindow::S300 => "5 мин",
            TimeWindow::S900 => "15 мин",
            TimeWindow::All => "всё",
        }
    }
}

/// Окно графика одного тега.
pub struct ChartWindow {
    pub tag: String,
    pub open: bool,
    units: Option<String>,
    window: TimeWindow,
    paused: bool,
}

impl ChartWindow {
    pub fn new(tag: String, units: Option<String>) -> Self {
        Self {
            tag,
            open: true,
            units,
            window: TimeWindow::S300,
            paused: false,
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    pub fn show(&mut self, ctx: &egui::Context, history: &History) {
        let title = format!("График: {}", self.tag);
        let mut open = self.open;
        egui::Window::new(title)
            .id(egui::Id::new(("chart", &self.tag)))
            .open(&mut open)
            .default_size([580.0, 360.0])
            .min_width(360.0)
            .show(ctx, |ui| self.body(ui, history));
        self.open = open;
    }

    fn body(&mut self, ui: &mut egui::Ui, history: &History) {
        let Some(ring) = history.series.get(&self.tag) else {
            ui.label("Нет данных: запущен ли сервер? Тег числовой?");
            return;
        };
        if ring.is_empty() {
            ui.label("Сбор данных…");
            return;
        }

        // Панель управления.
        ui.horizontal(|ui| {
            ui.label("Окно:");
            for w in [
                TimeWindow::S60,
                TimeWindow::S300,
                TimeWindow::S900,
                TimeWindow::All,
            ] {
                ui.selectable_value(&mut self.window, w, w.label());
            }
            ui.separator();
            ui.checkbox(&mut self.paused, "пауза");
            ui.label(RichText::new("(колесо — зум, перетаскивание — сдвиг, 2× — сброс)").weak());
        });

        // Видимое временное окно.
        let last_t = ring.back().unwrap().t;
        let from = self.window.secs().map_or(f64::NEG_INFINITY, |w| last_t - w);

        // Точки линии + маркеры некачественных значений + статистика.
        let mut pts: Vec<[f64; 2]> = Vec::new();
        let mut uncertain: Vec<[f64; 2]> = Vec::new();
        let mut bad: Vec<[f64; 2]> = Vec::new();
        let (mut mn, mut mx, mut sum, mut n) = (f64::INFINITY, f64::NEG_INFINITY, 0.0_f64, 0u64);
        for s in ring.iter().filter(|s| s.t >= from) {
            pts.push([s.t, s.v]);
            match s.q {
                Quality::Good => {}
                Quality::Uncertain => uncertain.push([s.t, s.v]),
                Quality::Bad => bad.push([s.t, s.v]),
            }
            mn = mn.min(s.v);
            mx = mx.max(s.v);
            sum += s.v;
            n += 1;
        }
        let cur = *ring.back().unwrap();
        let units = self.units.clone().unwrap_or_default();

        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("тек.: {:.4} {}", cur.v, units)).strong());
            if n > 0 {
                ui.separator();
                ui.label(format!("мин {mn:.4}"));
                ui.label(format!("макс {mx:.4}"));
                ui.label(format!("сред {:.4}", sum / n as f64));
                ui.label(format!("точек {n}"));
            }
        });

        // Ось X — время относительно последней точки («-1:05»).
        let anchor = last_t;
        let plot = Plot::new(("plot", &self.tag))
            .legend(Legend::default())
            .height(250.0)
            .allow_zoom(true)
            .allow_drag(true)
            .allow_scroll(true)
            .x_axis_formatter(move |mark, _range| fmt_rel_time(mark.value - anchor))
            .label_formatter(move |_name, p| format!("t {}\nзнач {:.4}", fmt_rel_time(p.x - anchor), p.y));

        plot.show(ui, |plot_ui| {
            plot_ui.line(
                Line::new(PlotPoints::from(pts))
                    .name(&self.tag)
                    .color(Color32::from_rgb(0x3d, 0x9b, 0xff))
                    .width(1.5),
            );
            if !uncertain.is_empty() {
                plot_ui.points(
                    Points::new(PlotPoints::from(uncertain))
                        .name("uncertain")
                        .color(Color32::from_rgb(0xf0, 0xa8, 0x20))
                        .radius(2.5)
                        .shape(MarkerShape::Circle),
                );
            }
            if !bad.is_empty() {
                plot_ui.points(
                    Points::new(PlotPoints::from(bad))
                        .name("bad")
                        .color(Color32::from_rgb(0xe0, 0x40, 0x40))
                        .radius(3.0)
                        .shape(MarkerShape::Diamond),
                );
            }
        });
    }
}

/// Секунды (со знаком, относительно последней точки) -> «-1:05».
fn fmt_rel_time(rel: f64) -> String {
    let neg = rel < -0.5;
    let total = rel.abs().round() as i64;
    format!(
        "{}{}:{:02}",
        if neg { "-" } else { "" },
        total / 60,
        total % 60
    )
}
