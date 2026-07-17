//! Конфигуратор сохраняет через `ConfigFile::V1` + to_string_pretty — этот
//! круг (открыть пример → сериализовать как GUI → загрузить сервером) обязан
//! быть без потерь и проходить валидацию.

use gateway_config::ConfigFile;

#[test]
fn example_config_survives_gui_save_cycle() {
    let example = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config.example.json");
    let text = std::fs::read_to_string(example).expect("config.example.json");

    // «Открытие» в GUI.
    let file: ConfigFile = serde_json::from_str(&text).expect("parse");
    let ConfigFile::V1(cfg) = &file;
    let tags_before: usize = cfg
        .channels
        .iter()
        .flat_map(|c| &c.devices)
        .map(|d| d.registers.len())
        .sum();

    // «Сохранение» из GUI.
    let saved = serde_json::to_string_pretty(&file).expect("serialize");
    assert!(saved.contains("\"schema_version\": \"1\""), "версия схемы на месте");

    // Сервер загружает то, что сохранил GUI.
    let resolved = gateway_config::load_str(&saved).expect("server loads GUI-saved config");
    assert_eq!(resolved.tag_count(), tags_before, "теги не потерялись");

    // И второй круг байт-в-байт стабилен (никакого дрейфа форматирования).
    let file2: ConfigFile = serde_json::from_str(&saved).unwrap();
    let saved2 = serde_json::to_string_pretty(&file2).unwrap();
    assert_eq!(saved, saved2, "повторное сохранение идентично");
}
