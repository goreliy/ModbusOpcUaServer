# Прогресс фазы 2 — tags-core (формулы, deadband, история)

Обновляется после каждого шага; при обрыве сессии продолжать отсюда.

## Архитектура (решения зафиксированы)
- `tags-core` зависит от `mb-poller` (кэш-граница: CacheReader/ChangeBatch) + `mb-types` + `gateway-config`.
- **Decoder**: RawValue (Bits/Registers/Raw) + DataType/WordOrder/ByteOrder/bit → `TypedValue`
  { Bool, Int(i64), UInt(u64), Float(f64), Text(String), Bytes(Vec<u8>) }.
  Word order: BigEndian = старшее слово первым. Byte order: swap внутри 16-бит слова.
  BCD → UInt; ASCII → Text (2 байта/регистр, trim NUL/space); Custom Raw → Bytes.
  Также ENCODE (typed → регистры) — задел для фазы 4 (запись).
- **Formula** (`evalexpr` v11): переменная `raw` (f64) + функция `tag("имя")` (читает текущее
  типизированное значение другого тега, БЕЗ каскадного пересчёта — read-at-eval).
  Компилируется один раз (Node), парс-ошибки валят старт движка (fail-fast).
  `write_formula` — обратное преобразование для фазы 4.
- **Правило типов**: formula ИЛИ scale≠1 ИЛИ offset≠0 → Float(инженерное значение);
  иначе — нативный тип (UInt/Int/Bool/...). Bool не масштабируется.
- **Deadband** (абсолютный, f64): подавляет публикацию, если |new−last_published| < deadband
  и quality не изменилось. Только для числовых значений.
- **TypedStore**: плоские слоты по TagId (как TagCache), TypedBatch broadcast с
  атомарным seq (тот же паттерн batch_lock), trait TypedReader для фазы 3.
- **Persist (sled)**: ключи по ИМЕНИ тега (стабильны при перенумерации TagId).
  Tree "retentive": name → JSON {value, ts}; при старте retentive-теги
  восстанавливаются с quality=Uncertain. Tree "history": name → JSON-кольцо
  последних retain_last значений (перезапись целиком — ок для коротких колец).
  Flush: авто (sled flush_every_ms) + явный при shutdown.
- **TagEngine**: одна задача; подписка на ChangeBatch; Lagged → snapshot_all ресинк;
  quality-переходы пробрасываются в typed-слои всегда (deadband их не глушит).
- **Схема конфига (v1, обратно совместимо)**: RegisterEntry += formula: Option<String>,
  write_formula: Option<String>, deadband: Option<f64>, retentive: bool (def false),
  retain_last: Option<u16>, units: Option<String>. Валидация в gateway-config:
  deadband конечен и >=0, retain_last <= 1000; синтаксис формул проверяет tags-core
  при старте движка (чтобы не тянуть evalexpr в config-крейт).

## Шаги (последовательно, cargo-гейт после каждого)
1. [x] gateway-config: formula/write_formula/deadband/retentive/retain_last/units в RegisterEntry+ResolvedRegister; правила BadDeadband/RetainLastTooBig (cap 1000)/ScaleShadowedByFormula (warning); 43 теста зелёные
2. [x] tags-core: value.rs (TypedValue) + decode.rs (decode+encode, все DataType × word/byte order, BCD/ASCII/bitfield, round-trip тест) — 9 тестов
3. [x] tags-core: formula.rs — Transform (Linear/Expr), compile+probe-eval (парсер evalexpr ленивый — «raw + » валится только на eval, поэтому пробный прогон с raw=0 и стаб-lookup обязателен), tag() через trait TagLookup, identity сохраняет нативный тип; 16 тестов
4. [x] tags-core: store.rs — TypedStore (плоские слоты, atomically-monotonic TypedBatch, TypedReader, TagLookup: только Good+числовые значения питают формулы; restore→Uncertain); 20 тестов суммарно
5. [x] tags-core: persist.rs — sled, ключи по ИМЕНИ тега, trees retentive/history, StoredValue (все виды), truncate-кольцо, round-trip через reopen; 23 теста суммарно
6. [x] tags-core: engine.rs — TagEngine (fail-fast компиляция формул с именем тега в ошибке,
   restore retentive→Uncertain до старта, Lagged→snapshot_all ресинк, deadband с памятью
   последнего опубликованного, quality-recovery пробивает deadband, персист по имени) —
   5 юнит-тестов + tests/full_chain.rs: sim→poller→cache→engine→typed store, включая
   scale 0.1, formula raw*60, cross-tag tag("a"), Bad-пропагацию при смерти сервера
   и восстановление retentive из sled после перезапуска движка. Прошёл с первого прогона.
7. [x] Полный workspace-гейт: check чисто, **136 тестов / 0 падений**, clippy --all-targets -D warnings чисто.

# ФАЗА 2 ЗАВЕРШЕНА. Следующая — фаза 3: opcua-gateway (адресное пространство из TypedStore,
# subscriptions поверх TypedBatch) + mqtt-publisher (JSON push). Оба сидят на trait TypedReader.
