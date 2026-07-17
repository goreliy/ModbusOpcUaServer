#!/usr/bin/env bash
# Упаковка Linux-артефактов БЕЗ пересборки (бинарники уже в target/release
# из полного прогона). Инструменты ставятся в /tools (кешируется volume'ом).
set -euo pipefail
export PATH="/tools/bin:$PATH"
export CARGO_INSTALL_ROOT=/tools

command -v cargo-deb >/dev/null 2>&1 || cargo install cargo-deb --locked
command -v cargo-generate-rpm >/dev/null 2>&1 || cargo install cargo-generate-rpm --locked

echo "=== .deb ==="
cargo deb -p app --no-build

echo "=== .rpm ==="
cargo generate-rpm -p crates/app

echo "=== tar.gz ==="
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
