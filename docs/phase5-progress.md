# Фаза 5 — безопасность OPC UA

## Решения
- **Сертификаты клиентов**: `trust_client_certs` больше не намертво true. Новый флаг
  `opcua.trust_any_client_cert` (default **false** — secure by default). Рабочий процесс
  штатный для OPC UA: первый коннект шифрованного клиента → его серт падает в
  `pki/rejected/` → оператор переносит в `pki/trusted/`. Для наладки флаг можно
  временно включить (валидация выдаёт warning).
- **Пароли**: `users[]` теперь `password` (плейнтекст, warning при валидации) ИЛИ
  `password_hash` (argon2id PHC-строка) — ровно одно из двух (ошибка иначе).
  Проверка — кастомный AuthManager (async-opcua `with_authenticator`), плейнтекст
  сравнивается constant-time. Хэш генерирует `opc-modbus-server hash-password`.
- Анонимный доступ — как раньше, по флагу allow_anonymous, через тот же AuthManager.
- Пер-пользовательские права на запись — НЕ в этой фазе (write callback не знает
  сессию; задокументировано как ограничение).

## Шаги
1. [x] схема: trust_any_client_cert (def false); OpcUaUser.password → Option +
       password_hash (argon2 PHC, префикс проверяется); валидация: ровно одно из
       двух, WeakOpcUaSecurity-warning на плейнтекст и на trust_any=true; 45 тестов
2. [x] opcua-gateway/src/auth.rs: GatewayAuthenticator — AuthManager требует
       ТРИ метода: anonymous, username (+ обязательный user_token_policies —
       скопирован паттерн DefaultAuthenticator: anonymous policy если endpoint
       содержит ANONYMOUS, username policy если token id совпадает с юзером);
       плейнтекст — constant-time, хэш — argon2 verify, кривой хэш fail-closed;
       builder: with_authenticator + trust_client_certs(cfg) + user tokens с
       ПУСТЫМ паролем (секрет не попадает в конфиг сервера). 9 юнит-тестов
3. [x] app: hash-password читает пароль из stdin (не из argv — история шелла)
4. [x] e2e opcua_auth.rs зелёный (24 c). Грабли, на которых висли:
       - UserTokenPolicy::username_password() НЕ существует в 0.18 — собирать вручную;
       - wait_for_connection() возвращает bool (false = retry-бюджет исчерпан);
       - disconnect() на НЕподключившейся сессии ВИСНЕТ НАВСЕГДА — для отклонённых
         сессий только event_loop_handle.abort() + drop(session);
       - connect_to_matching_endpoint возвращает Err(opcua::types::Error), не StatusCode;
       - SessionEventLoop генерик по Connector (в тестах — generic-хелпер).
5. [x] гейты: **149 тестов / 0 падений**, clippy чисто; PLAN.md фаза 5 → ✅;
       config.example.json переведён на password_hash (валидируется без warnings);
       README.md написан (запуск, конфиг, безопасность, сертификаты, качество данных).

# ФАЗА 5 ЗАВЕРШЕНА. Дальше: фаза 6 (служба Windows/systemd + логи), 7 (egui GUI), 8 (инсталляторы).
