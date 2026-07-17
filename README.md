# OPC Modbus Server

**OPC UA сервер** со встроенным опросчиком Modbus и движком формул. Один бинарь:
опрашивает устройства по Modbus RTU / TCP / RTU-over-TCP, преобразует сырые
регистры в инженерные значения (масштаб, формулы, межтеговые выражения) и
отдаёт их OPC UA клиентам (SCADA/MES) — с подписками, записью назад в
устройства, retentive-памятью и честным качеством данных.

> **Лицензия:** домашнее/некоммерческое использование — **бесплатно**;
> коммерческое — по отдельной платной лицензии, запрос стоимости на
> **gorelyj@gmail.com**. Пробные сборки ограничены 3 часами непрерывной работы.
> Полный текст — [LICENSE.md](LICENSE.md).
>
> **Репозиторий:** https://github.com/goreliy/ModbusOpcUaServer
> **Инсталлятор Windows:** [installers/windows/install.ps1](installers/windows/install.ps1)
> **Серверный бинарь:** [target/release/opc-modbus-server.exe](target/release/opc-modbus-server.exe)
> **GUI-конфигуратор:** [target/release/opc-modbus-config.exe](target/release/opc-modbus-config.exe)

## Быстрый старт

```bash
# проверить конфиг
opc-modbus-server validate --config config.json

# запустить сервер (консоль, Ctrl+C для останова)
opc-modbus-server run --config config.json
```

Шаблон конфигурации — [config.example.json](config.example.json). Клиент
подключается на `opc.tcp://<host>:4840/` (порт настраивается).

## Работа службой

**Windows** (из консоли администратора):

```powershell
opc-modbus-server service install --config C:\ProgramData\opc-modbus-server\config.json
sc start opc-modbus-server        # автозапуск уже включён
opc-modbus-server service uninstall
```

**Linux (systemd, включая Astra):**

```bash
opc-modbus-server systemd-unit --config /etc/opc-modbus-server/config.json \
  | sudo tee /etc/systemd/system/opc-modbus-server.service
sudo systemctl daemon-reload && sudo systemctl enable --now opc-modbus-server
```

## Логи

Секция `logging`: `level` — фильтр tracing (`"info"`,
`"info,mb_poller=debug"`); `dir` — включает файл с дневной ротацией
(`opc-modbus-server.YYYY-MM-DD`) — обязателен для Windows-службы. Консольный
вывод под systemd попадает в journald автоматически. Переменная `RUST_LOG`
перекрывает конфиг.

Диагностика обмена без Wireshark: `"log_traffic": true` на канале +
`"level": "info,modbus_traffic=debug"` — hex-дампы каждого кадра rx/tx.

## Конфигурация (JSON)

| Секция | Что настраивает |
|---|---|
| `gateway` | имя экземпляра, `data_dir` (sled-хранилище retentive-тегов) |
| `opcua` | `host` (адрес привязки), `advertised_host` (адрес для клиентов, если host=0.0.0.0), порт, эндпоинты безопасности, пользователи, PKI |
| `poll_groups` | именованные периоды опроса (200 мс, 5 с, ...) |
| `channels[]` | каналы связи: `tcp` / `rtu` / `rtu_over_tcp` + их устройства |
| `channels[].devices[].registers[]` | теги: адрес, функция, тип, порядок слов/байт, формулы |

Возможности тега: `scale`/`offset` или `formula` (`raw * 60`,
`raw + tag("other.tag")`), `deadband`, `units`, `retentive` (переживает
рестарт), `retain_last` (кольцо истории), `writable` (+`write_formula` —
обратное преобразование для записи); для vendor-функций — `custom_request`
(hex-байты запроса) и `custom_response_len`.

Запись поддерживает функции Modbus **FC05** (`write_single_coil`), **FC06**
(`write_single_register`), **FC15** (`write_multiple_coils`) и **FC16**
(`write_multiple_registers`). По умолчанию код выбирается автоматически
(катушка → FC05; holding-регистр → FC06 для одного слова, FC16 для
многословных типов). Необязательное поле `write_function` форсит конкретный
код, когда устройство этого требует — например `write_multiple_coils` (FC15)
для катушки или `write_multiple_registers` (FC16) для одиночного регистра.

Относительные пути (`data_dir`, `logging.dir`, `opcua.pki_dir`) отсчитываются
от каталога конфиг-файла, а не от текущего каталога процесса — служба и
`systemd` работают корректно независимо от рабочей директории.

## Безопасность

- Эндпоинты: None (для наладки, отключаем `allow_none_security: false`),
  Basic256Sha256 Sign&Encrypt, Aes256-Sha256-RsaPss Sign&Encrypt.
- Пользователи: `password` (плейнтекст — валидатор предупредит) или
  `password_hash` (argon2id):

  ```bash
  echo mySecret | opc-modbus-server hash-password
  # -> $argon2id$v=19$... вставить в "password_hash"
  ```

- Сертификаты клиентов: по умолчанию незнакомый клиентский сертификат
  отклоняется и сохраняется в `pki/rejected/` — перенесите его в
  `pki/trusted/` и переподключитесь. На время пусконаладки можно включить
  `"trust_any_client_cert": true` (валидатор предупредит).

## Качество данных (OPC UA StatusCode)

| Состояние | StatusCode |
|---|---|
| Значение свежее | `Good` |
| Восстановлено из retentive-памяти, ещё не опрошено | `UncertainLastUsableValue` |
| Устройство/канал недоступны | `BadCommunicationError` (значение сохраняется) |
| Ещё ни разу не прочитано | `BadWaitingForInitialData` |

Запись: клиент получает `Good` только после подтверждения устройством
(таймаут → `BadTimeout`, обрыв → `BadNoCommunication` и т.д.).

## Конфигуратор (GUI)

`opc-modbus-config` — десктопный редактор конфигурации (egui, русский
интерфейс): все разделы схемы, живая валидация тем же кодом, что у сервера
(включая синтаксис формул), генерация argon2-хэшей паролей, **поиск
COM-портов**, **пробное чтение устройства по Modbus прямо из окна** и
**раздел «Сервер» — запуск/останов самого сервера, статус, подключённые
клиенты OPC UA, метрики каналов, живые значения тегов и журнал** без отдельного
консольного процесса. В таблице живых значений у каждого числового тега есть
кнопка **📈 — график изменения значения во времени** (egui_plot: зум колесом,
сдвиг перетаскиванием, двойной клик — автосброс, окно 1/5/15 мин или «всё»,
мин/макс/среднее, единицы измерения; некачественные точки — Uncertain/Bad —
подсвечены цветом).

```bash
cargo run -p gui        # или target/release/opc-modbus-config
```

## Сборка из исходников

```bash
cargo build --release -p app -p gui # -> opc-modbus-server + opc-modbus-config
cargo test --workspace              # ~155 тестов
```

Windows 10/11 / Windows Server и любой Linux с systemd (включая Astra Linux).
Дорожная карта и статус фаз — [PLAN.md](PLAN.md), детали реализации — `docs/`.
