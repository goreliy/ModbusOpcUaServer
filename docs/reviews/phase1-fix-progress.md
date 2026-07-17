# Прогресс исправления находок ревью фазы 1

Источник находок: [phase1-findings.md](phase1-findings.md) (33 шт.).
Файл обновляется после каждого шага — при обрыве сессии продолжать отсюда.

Состояние workspace на момент записи: `cargo check` чисто, **100 тестов, 0 падений**.

## Сделано (22/33)

### mb-proto (агент fix:mb-proto, завершён)
- #33 ✔ ProtoError::Protocol → структурный { kind: ProtocolKind, detail }, отдельный fatal-вариант Resolve для DNS
- #32 ✔ async DNS (tokio::net::lookup_host) внутри существующего connect-таймаута
- #17 ✔ прагматично: TODO(phase2) на настоящий фрейминг Custom; warning в validate.rs (CustomReadOnSerialRtu); post-hoc expect_len оставлен
- (доп. из #6/#12) ✔ транспортная валидация длины ответа в release: регистры len==qty, биты len>=qty с усечением; несоответствие → Protocol{UnexpectedResponse} (fatal). Тесты с «лживым» слейвом.

### gateway-config (агент fix:gateway-config, завершён)
- #10/#20 ✔ трёхуровневый retry: device → channel (теперь Option) → gateway.default_retry; sample-config + тесты цепочки
- #21 ✔ Custom code == 0 || >= 0x80 отклоняется; коллизия со стандартными кодами — warning
- #22 ✔ period_ms == 0 отклоняется
- #23 ✔ TURNAROUND_MARGIN_MS (5 мс) в оценке шины; disabled-каналы пропускаются

### mb-poller channel.rs/device.rs (агент fix:core-runtime, завершён)
Связная half-duplex политика, все 13 находок подтверждены и починены:
- #1/#2 (CRITICAL) ✔ состояние DeviceRuntime переживает реконнекты (on_connected удалён); вотчдог достижим; e2e half_duplex_e2e с молчащим юнитом
- #3 ✔ проба уже-оффлайн устройства не апгрейдит Bad → Uncertain
- #4/#12 ✔ Timeout не ретраится на том же соединении на half-duplex (+ транспортная проверка длины из mb-proto)
- #5/#13 ✔ записи участвуют в drain-правиле (consec_timeouts, metrics.timeouts, реконнект по таймауту записи)
- #7 ✔ backoff сбрасывается на base только после первого УСПЕШНОГО запроса (healthy-флаг)
- #8 ✔ ожидающие записи обслуживаются на границах транзакций (drain_pending_writes)
- #18 ✔ проба оффлайн-устройства — одиночный запрос без ретраев
- #19 ✔ Exception = признак жизни: on_success() перед handle_exception (и для записей тоже)
- #26 ✔ inter_request_gap() после каждого отправленного запроса, включая ветку timeout/continue
- #27 ✔ fail_pending_writes(): очередь записей отвечает Err(NotConnected) при разрыве; backoff_or_stop fail-fast для новых
Новые тесты: fails_survive_reconnect..., probe_backoff_survives_reconnect, exception_reply_revives_offline_device, timeout_is_not_retryable_on_half_duplex, probe_of_offline_device_is_single_shot + tests/half_duplex_e2e.rs (2 e2e).

## Осталось (11/33) — кластер ApiCache + тесты

Порядок выполнения (последовательно, по одному, с cargo check после каждого):

1. ✔ СДЕЛАНО #9/#15/#31: DevicePlan несёт request_timeout+retry; poll_with_retry/service_write/DeviceRuntime::new используют девайсные значения; plan.retry остался только для канального reconnect-backoff. Тест device_timeout_and_retry_overrides_reach_the_plan. 32 lib-теста зелёные.
2. ✔ СДЕЛАНО #6: scatter() сверяет len ответа с qty (регистры ==, биты >= после транспортного усечения); несоответствие → ВСЕ теги txn Bad, ничего не публикуется. Тест scatter_rejects_wrong_total_length_entirely. 33 lib-теста.
3. ✔ СДЕЛАНО #14: WriteCommand.device: DeviceId; dev_index map в run_channel; unknown/disabled → явная ошибка. Все 3 call-site (demo + 2 e2e) обновлены.
4. ✔ СДЕЛАНО #11: `*shutdown.borrow()` проверяется на каждой границе транзакции и устройства в run_tick; запрос в полёте не отменяется.
5. ✔ СДЕЛАНО #24: batch_lock (parking_lot::Mutex) вокруг reserve+send — seq монотонен в порядке доставки.
6. ✔ СДЕЛАНО #25: set_device_quality больше не трогает mono; doc Snapshot.mono/seq уточнены (seq = value-only, не дедупить по нему quality-переходы).
7. ✔ СДЕЛАНО #28: swept-флаг — sweep Bad один раз на период разрыва, сбрасывается при успешном connect (Instant::now() из цикла ушёл вместе с фиксом #25).
8. ✔ СДЕЛАНО #29: док-коммент PollerHandle исправлен (drop = останов задач).
9. ✔ СДЕЛАНО #30: spawn/spawn_with_cache принимают &ResolvedConfig; 5 call-site обновлены.
10. ✔ СДЕЛАНО #16: (a) half-duplex e2e — есть с core-этапа; (b) tests/de_coalesce_e2e.rs — split происходит, ok-теги снова Good и обновляются, дыра Bad, reconnects=0; (c) custom_function_round_trips_over_tcp в mb-proto — эхо Custom PDU + короткий ответ (контракт expect_len — за scatter, проверен юнитом).
11. ✔ СДЕЛАНО финальный гейт: check чисто, **104 теста / 0 падений**, clippy --all-targets -D warnings чисто, e2e_demo живой (coalescing 30 req vs 84, запись прошла, чистый shutdown).

## ИТОГ: все 33 находки закрыты (32 починены, #17 — прагматично: warning + TODO(phase2) на настоящий фрейминг Custom на serial RTU)

## Дальше
- ✔ СДЕЛАНО нагрузочный тест: tests/load.rs (ignored, ~10 c, запуск `cargo test -p mb-poller --test load -- --ignored --nocapture`).
  Результат: 20 каналов × 10 устройств × 25 тегов = 200 устройств / 5000 тегов, период 200 мс,
  min/avg обновлений на тег = 50/50 из ~50 возможных (100% удержание периода), коалесинг ровно
  25 тегов/запрос, 10020 запросов OK, 0 таймаутов, 0 реконнектов, 0 не-Good тегов,
  shutdown 20 каналов за ~0.9 мс.
- ✔ СДЕЛАНО PLAN.md: фаза 1 → ✅ с зафиксированными цифрами.

# ФАЗА 1 ПОЛНОСТЬЮ ЗАВЕРШЕНА (код + ревью + нагрузка). Следующая — фаза 2: tags-core (формулы, deadband, sled-буфер истории).
