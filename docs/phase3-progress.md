# Фаза 3 — OPC UA сервер + бинарь продукта: ЗАВЕРШЕНА

Продукт (по требованию заказчика) — **OPC UA сервер** с Modbus и формулами; MQTT вторичен и отложен.

## Сделано
- Схема конфига: секция `opcua` (host/port, application_name/uri, allow_none_security,
  basic256sha256, allow_anonymous, users[], pki_dir) + `gateway.data_dir`; валидация
  (порт, полиси, пользователи); ResolvedConfig несёт opcua. 44 теста конфига.
- `opcua-gateway`: сервер на async-opcua 0.18, собирается программно (без server.conf):
  discovery_urls обязательны (иначе Build error); эндпоинты None (опц.) +
  Basic256Sha256 SignAndEncrypt + Aes256-Sha256-RsaPss SignAndEncrypt (Sign-only
  deprecated в стандарте); ANONYMOUS + username/password токены.
  Адресное пространство: Objects/<device>/<tag Variable>, NodeId = ns;s=<имя тега>
  (стабильны между рестартами), DataType-атрибут по правилу типов движка
  (формула/scale → Double, иначе нативный). ВАЖНО: Variable::new с Variant::Empty
  ПАНИКУЕТ — использовать new_data_value с явным DataTypeId.
  Насос значений: TypedBatch → set_values (подписки получают push), Lagged → полный refresh,
  Absent → BadWaitingForInitialData, Good/Uncertain/Bad → Good/UncertainLastUsableValue/BadCommunicationError.
- `app` → бинарь `opc-modbus-server` (clap): `validate --config`, `run --config`;
  порядок запуска poller → engine → opcua, обратный на shutdown (Ctrl+C); sled в
  gateway.data_dir/tags.sled; коды выхода 2=конфиг, 3=хранилище, 4=движок, 5=opcua.
- `config.example.json` в корне репо (TCP-канал + выключенный RTU-канал для примера).
- Тест opcua_e2e: НАСТОЯЩИЙ OPC UA клиент (async-opcua client) через
  connect_to_matching_endpoint + session.read: Modbus сим → поллер → кэш → движок →
  typed store → OPC UA: i16 237 ×0.1 → Double 23.7; u16 25 formula*60 → 1500;
  живая правка регистра → 1800 доезжает до клиента. Прогон ~3.9 c.
- Смоук бинаря: validate ОК (2 канала/2 устройства/6 тегов), run — порт 4842 LISTENING,
  эндпоинты и пользователи в логе, поллер ретраит недоступный ПЛК с backoff.

## Гейты
check чисто; **138 тестов / 0 падений**; clippy --workspace --all-targets -D warnings чисто.

## Дальше (по дорожной карте PLAN.md)
- Фаза 4: путь записи — OPC UA Write → write_formula (обратное) → tags_core::encode →
  WriteCommand{DeviceId} → Modbus (инфраструктура готова: encode есть, WriteCommand по DeviceId есть,
  SimpleNodeManager поддерживает write-коллбеки).
- Фаза 5: безопасность (сертификаты не trust-all, шифрованное хранение паролей).
- Фаза 6: служба Windows/systemd + ротация логов (svc-host, diag-log).
- Фаза 7: GUI-конфигуратор на egui (НЕ Tauri — решение заказчика).
- Фаза 8: инсталляторы (.msi via cargo-wix, .deb via cargo-deb) + CI.
- MQTT (вторично): mqtt-publisher поверх TypedReader/TypedBatch.
