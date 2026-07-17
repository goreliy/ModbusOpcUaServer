# Фаза 4 — запись OPC UA → Modbus

## Решения
- Синхронный write-callback SimpleNodeManager → `block_in_place` + `block_on(reply)`:
  клиент OPC UA получает РЕАЛЬНЫЙ статус (Good только после ack устройства).
  Требует multi-thread рантайма (app и тесты — multi_thread).
- Значение узла в адресном пространстве НЕ обновляется при записи — подтверждение
  придёт естественным read-back через опрос (SCADA-конвенция; так ведёт себя
  callback-ветка write_node_value в async-opcua).
- Разрешено: `writable: true` только для read_holding_registers (FC16/FC06 write-back)
  и read_coils (FC05). Bit-в-регистре, Ascii/Bcd/Bitfield — отклоняются валидацией
  (read-modify-write/MaskWrite — потом).
- Инверсия: write_formula (переменная `value`) если задана; иначе автоинверсия
  линейной (value-offset)/scale, scale==0+writable → ошибка валидации.
  formula задана + writable ⇒ write_formula ОБЯЗАТЕЛЕН (выражение не автоинвертируется).
- Статусы: Ok→Good, Timeout→BadTimeout, NotConnected→BadNoCommunication,
  Exception→BadDeviceFailure, прочее→BadCommunicationError; неизвестный тег→BadNotWritable,
  кривой Variant→BadTypeMismatch.

## Шаги — ВСЕ ВЫПОЛНЕНЫ, ФАЗА 4 ЗАВЕРШЕНА
1. [x] схема: writable + 7 правил валидации (writable_rules тест); 45 тестов конфига
2. [x] PollerHandle::all_writers()
3. [x] opcua-gateway/src/write.rs: WritePlan (fail-fast компиляция write_formula),
       Inverse::{Linear (автоинверсия), Expr, None}, raw_to_typed с защитой от
       NaN/inf/переполнения, coil-запись из Bool/интов, block_in_place round-trip;
       writable-узлы получают CURRENT_READ|CURRENT_WRITE (+user); 5 юнит-тестов
4. [x] app: poller.all_writers() → spawn
5. [x] e2e: Double 45.3 → scale 0.1 инверсия → регистр 453 (проверен в банке сима),
       Good только после ack; read-back 45.3 через опрос; запись в не-writable
       отклонена (BadNotWritable из validate_node_write по access level), регистр не тронут
6. [x] гейты: 144 теста / 0 падений, clippy --all-targets -D warnings чисто;
       config.example.json дополнен writable-тегом plc1.setpoint

## Дальше: фаза 5 (безопасность), 6 (служба+логи), 7 (egui GUI), 8 (инсталляторы), MQTT (опция)
