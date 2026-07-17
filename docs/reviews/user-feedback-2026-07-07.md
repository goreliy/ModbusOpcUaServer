# Фидбек заказчика + находки ревью (2026-07-07)

## Фичи
F1. Поиск COM-портов в GUI (выпадающий список + «Обновить», ручной ввод остаётся).
F2. Аккуратная форма тега: контекстные поля (по функции/типу), группировка
    «Источник / Преобразование / Хранение / Запись», вычисляемый диапазон регистров.
F3. Показ подключенных OPC UA клиентов и их параметров.
F4. «Более логичный Modbus-опрос» в GUI: сводка по устройству (N тегов → M
    транзакций после коалесинга), периоды групп видны в контексте.
F5. Запуск сервера ИЗ приложения (без консоли): кнопка старт/стоп, статус,
    метрики каналов, живые значения тегов, клиенты, лог в окне.

## Баги/риски (ревью)
B1 [P1] OPC UA write таймаутится (WRITE_TIMEOUT=10s фикс.), но команда остаётся
    в очереди и выполняется ПОЗЖЕ → клиент ретраит, двойное срабатывание.
    Также 10s < возможного device timeout*(1+retries).
    → WriteCommand несёт deadline; канал отбрасывает просроченные (reply Err);
      таймаут ожидания в write.rs выводить из timeout устройства и retry.
B2 [P1] Эндпоинт рекламирует bind-host (0.0.0.0) в discovery/endpoint URL.
    → advertised_host в схеме (Option); URL-ы из advertised, bind из host;
      предупреждение валидации, если advert получится 0.0.0.0.
B3 [P2] max_inflight>1 принимается для TCP, но рантайм последовательный.
    → резолвить в 1 всегда + WARNING «не реализовано, принудительно 1»
      (упоминание в доке; поле остаётся для будущего).
B4 [P2] Custom PDU не имеет тела запроса (data: Vec::new()).
    → RegisterEntry.custom_request: Option<String> (hex "01 a0 ff"), валидация
      формата, резолв в байты, plan кладёт в ModbusRequest::Custom.data.
B5. Асимметрия валидации: GUI показывает «ошибок нет», а формулы компилируются
    только при старте движка/write-plan.
    → tags-core: pub fn check_formulas(итерация по (tag, formula, write_formula))
      → Vec<String>; GUI мержит в панель проверки.
B6. Custom на serial RTU: expect_len не делимитирует поток (tokio-modbus кадрирует
    unknown FC timing-dependent) — риск рассинхрона шины.
    → УЖЕСТОЧИТЬ: Custom-чтение на serial Rtu = ОШИБКА валидации (было warning);
      RtuOverTcp остаётся warning; TCP полноценно. Задокументировать ограничение.
B7. Относительные ./data ./logs pki зависят от CWD процесса (служба/systemd).
    → gateway_config::load(path): после резолва перебазировать относительные
      data_dir / logging.dir / opcua.pki_dir на КАТАЛОГ КОНФИГА. systemd-unit:
      + WorkingDirectory=каталог конфига (страховка).
B8. GUI не редактирует device.retry (runtime понимает) — добавить; max_inflight
    в GUI не показывать (см. B3).
B9. Пробное чтение ≠ runtime: берёт channel timeout (игнорируя device override),
    поток без отмены/join.
    → использовать device.request_timeout_ms.unwrap_or(channel), AtomicBool-отмена
      (проверка между регистрами), закрытие окна взводит отмену.

---

## Статус реализации (все пункты закрыты)

| # | Пункт | Статус |
|---|---|---|
| B1 | Запись с дедлайном (нет двойного срабатывания после BadTimeout) | ✅ WriteCommand.deadline; wait = device_timeout×(1+retries)+запас; e2e |
| B2 | advertised_host отдельно от bind-host | ✅ схема + разводка в endpoint/discovery URL; e2e opcua_advertised |
| B3 | Честный max_inflight | ✅ резолвится в 1 всегда + warning; поле сохранено |
| B4 | Custom тело запроса (код+байты) | ✅ custom_request (hex) → байты → провод |
| B5 | Формулы в живой валидации GUI | ✅ check_config_formulas в панели проверки |
| B6 | Custom на serial RTU = ошибка | ✅ CustomReadOnSerialRtu теперь hard error |
| B7 | Пути от каталога конфига | ✅ load() перебазирует data_dir/logging.dir/pki_dir; systemd WorkingDirectory |
| B8 | GUI: retry устройства | ✅ opt_retry_edit; max_inflight не показывается (см. B3) |
| B9 | Пробное чтение = рантайму | ✅ таймаут устройства, отмена (AtomicBool), Drop гасит поток |
| F1 | Поиск COM-портов | ✅ mb_proto::available_serial_ports + combo/refresh в RTU-форме |
| F2 | Аккуратная форма тега | ✅ контекстные поля, группы, диапазон регистров, дублирование |
| F3 | Подключённые клиенты | ✅ session_count() + recent_authentications() (пер-сессионные детали недоступны в async-opcua 0.18 — задокументировано) |
| F4 | Логичный опрос в GUI | ✅ «N тегов → M запросов», период группы в списке тегов |
| F5 | Запуск сервера из приложения | ✅ раздел «Сервер»: старт/стоп, uptime, endpoint, клиенты, метрики каналов, живые теги, журнал |
