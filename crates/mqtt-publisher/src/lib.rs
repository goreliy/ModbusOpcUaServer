//! Push-рассылка тегов по MQTT/JSON — прагматичная замена спек-совместимого OPC UA PubSub
//! (см. PLAN.md §5, §11: спек-совместимого безопасного Rust-PubSub на рынке нет).
//!
//! Placeholder — implemented in phase 3 behind a `TagPublisher` trait shared with
//! opcua-gateway, so a spec-compliant backend can be swapped in later without touching callers.
