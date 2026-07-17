#!/usr/bin/env bash
# Собирает Linux-артефакты OPC Modbus Server внутри rust-контейнера:
#   .deb (cargo-deb), .rpm (cargo-generate-rpm), портируемый .tar.gz.
# Ожидает проект в /work, пишет пакеты в /work/dist/linux.
# Feature `trial` уже зашита в app/gui — сборка обычная.
set -euo pipefail

echo "=== системные зависимости ==="
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
# libudev+pkg-config — перечисление COM-портов (serialport); остальное —
# сборка eframe/glow/rfd (GUI) под Linux.
apt-get install -y --no-install-recommends \
    pkg-config libudev-dev \
    libgl1-mesa-dev libxkbcommon-dev libwayland-dev libgtk-3-dev \
    libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev >/dev/null
echo "glibc: $(ldd --version | head -1)"

echo "=== инструменты упаковки ==="
cargo install cargo-deb --locked >/dev/null 2>&1 || cargo install cargo-deb --locked
cargo install cargo-generate-rpm --locked >/dev/null 2>&1 || cargo install cargo-generate-rpm --locked

echo "=== сборка релиза (server + gui) ==="
cargo build --release -p app -p gui

echo "=== .deb ==="
cargo deb -p app --no-build

echo "=== .rpm ==="
cargo generate-rpm -p crates/app

echo "=== портируемый tar.gz ==="
STAGE=/tmp/oms-linux
rm -rf "$STAGE"; mkdir -p "$STAGE"
cp target/release/opc-modbus-server target/release/opc-modbus-config "$STAGE"/
cp config.example.json README.md LICENSE.md installers/linux/opc-modbus-server.service "$STAGE"/
tar -C /tmp -czf target/oms-linux.tar.gz oms-linux

echo "=== раскладка в /work/dist/linux ==="
OUT=${OUT_DIR:-/work/dist/linux}
mkdir -p "$OUT"
cp target/debian/*.deb "$OUT"/ 2>/dev/null || echo "WARN: .deb не найден"
cp target/generate-rpm/*.rpm "$OUT"/ 2>/dev/null || echo "WARN: .rpm не найден"
cp target/oms-linux.tar.gz "$OUT"/

echo "=== ГОТОВО ==="
ls -la "$OUT"
