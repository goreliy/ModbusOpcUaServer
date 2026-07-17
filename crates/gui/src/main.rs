//! `opc-modbus-config` — десктопный конфигуратор OPC Modbus Server (egui).
//!
//! Редактирует `ConfigV1` напрямую (все поля схемы), живая валидация тем же
//! кодом, что и сервер, хэширование паролей и пробное чтение устройства по
//! Modbus прямо из окна (наладка без запуска сервера).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod history;
mod server;
mod testpoll;

use eframe::egui;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([900.0, 600.0])
            .with_title("Конфигуратор OPC Modbus Server"),
        ..Default::default()
    };
    eframe::run_native(
        "opc-modbus-config",
        options,
        Box::new(|cc| {
            app::install_fonts(&cc.egui_ctx);
            let logs = server::install_log_capture();
            Ok(Box::new(app::ConfigApp::new(logs)))
        }),
    )
}
