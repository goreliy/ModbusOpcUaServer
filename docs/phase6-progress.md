# Фаза 6 — служба (Windows/systemd) + логирование

## Решения
- **Файловые логи**: tracing-appender, daily-ротация, non-blocking writer (guard живёт
  до выхода). Секция конфига `logging { level, dir, file_prefix }`; dir=None → только
  консоль. Кривой level → fallback "info" с warning (не валим старт).
- **journald**: НЕ отдельный слой — systemd сам пишет stdout службы в журнал,
  консольный слой и есть интеграция. Windows Event Log — отложено (файл обязателен,
  т.к. у службы нет консоли).
- **Лог трафика Modbus** (PLAN §6): tee-обёртка AsyncRead/AsyncWrite в mb-proto,
  hex-дамп кадров через tracing target="modbus_traffic" (debug), включается
  per-channel флагом `log_traffic` (default false). Для TCP нужно самому владеть
  стримом (tcp::attach_slave вместо tcp::connect) — проверить наличие в 0.17.
- **Windows-служба**: крейт windows-service; подкоманды `service install|uninstall|run`
  (install/uninstall через SCM API, требуют админа); `service run` — SCM-entry,
  вне SCM падает с понятной ошибкой. Общий каркас: run-логика app принимает
  shutdown-future (ctrl_c ИЛИ SCM Stop).
- **systemd**: подкоманда `systemd-unit` печатает готовый unit-файл (Restart=on-failure,
  WantedBy=multi-user.target); сам файл поставит .deb в фазе 8.

## Шаги — ВСЕ ВЫПОЛНЕНЫ, ФАЗА 6 ЗАВЕРШЕНА
1. [x] конфиг: LoggingConfig{level,dir,file_prefix} + ChannelConfig.log_traffic;
       config.example.json дополнен секцией logging
2. [x] mb-proto/src/traffic.rs: Tee (AsyncRead/AsyncWrite, hex-дамп rx/tx в target
       modbus_traffic) + Transport::connect_traced; для TCP теперь свой TcpStream
       (tcp::attach_slave generic — как и rtu) + set_nodelay(true); тест прозрачности
3. [x] diag-log: init(cfg) → LogSetup{guards,filter,filter_error,file}; RUST_LOG >
       config > info; кривой фильтр → fallback info (грабля: EnvFilter::try_new
       ленивый — «слова» парсятся как target, валится только кривой УРОВЕНЬ);
       тест: строка реально попадает в ротируемый файл
4. [x] svc-host: stack.rs run_stack(cfg, shutdown-future) → StackExit (общий для
       консоли и службы; тест clean-shutdown); windows.rs: install (валидирует конфиг
       до регистрации, abs-путь, LocalSystem, auto-start, описание), uninstall
       (stop+delete), run_service (конфиг через env OPC_MODBUS_SERVER_CONFIG,
       т.к. define_windows_service! не передаёт наши argv; вне SCM — понятная
       ошибка); SCM Stop/Shutdown → oneshot → graceful. ServiceStatus не Copy —
       clone(). systemd_unit(): unit с Restart=on-failure, SupplementaryGroups=dialout,
       hardening; юнит-тест на все ключевые строки
5. [x] app: подкоманды run/validate/hash-password/systemd-unit/service{install,uninstall,run};
       на Linux service-ветка выдаёт подсказку про systemd
6. [x] смоук: systemd-unit печатается; `service run` вне SCM → понятная ошибка exit 1;
       файл-лог opc-modbus-server.2026-07-06 реально пишется (без ANSI).
       Гейты: **154 теста / 0 падений**, clippy --all-targets -D warnings чисто.

# ФАЗА 6 ЗАВЕРШЕНА. Дальше: фаза 7 (egui-конфигуратор — НЕ Tauri), фаза 8 (инсталляторы .msi/.deb + CI).
