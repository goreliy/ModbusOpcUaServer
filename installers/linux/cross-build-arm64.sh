#!/usr/bin/env bash
# Кросс-сборка arm64 (aarch64) НАТИВНО на x86: rustc/cargo/cargo-deb работают
# на полной скорости хоста, arm64-код даёт линкер aarch64-linux-gnu + сисрут.
# Многократно быстрее QEMU-эмуляции. Пакеты пишутся в /work/dist/linux-arm64.
set -euo pipefail
TRIPLE=aarch64-unknown-linux-gnu
export DEBIAN_FRONTEND=noninteractive
export PATH="/tools/bin:$PATH"
export CARGO_INSTALL_ROOT=/tools

echo "=== multiarch + кросс-тулчейн ==="
dpkg --add-architecture arm64
apt-get update -qq
# Хост-инструменты (x86): линкер/pkg-config. arm64 dev-либы: :arm64.
apt-get install -y --no-install-recommends \
    pkg-config crossbuild-essential-arm64 \
    libudev-dev:arm64 >/dev/null
# GUI-зависимости под arm64 — best-effort (headless-шлюзам конфигуратор не нужен).
GUI_LIBS_OK=1
apt-get install -y --no-install-recommends \
    libgl1-mesa-dev:arm64 libxkbcommon-dev:arm64 libwayland-dev:arm64 \
    libgtk-3-dev:arm64 libxcb-render0-dev:arm64 libxcb-shape0-dev:arm64 \
    libxcb-xfixes0-dev:arm64 >/dev/null 2>&1 || { GUI_LIBS_OK=0; echo "WARN: arm64 GUI dev-libs недоступны — GUI не кросс-собираем"; }
echo "хост glibc: $(ldd --version | head -1)"

rustup target add "$TRIPLE"

echo "=== инструменты упаковки (нативно, x86) ==="
command -v cargo-deb >/dev/null 2>&1 || cargo install cargo-deb --locked
command -v cargo-generate-rpm >/dev/null 2>&1 || cargo install cargo-generate-rpm --locked

# Кросс-окружение: линкер, C/C++ компиляторы, pkg-config под arm64-сисрут.
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
export CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc
export CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++
export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig
export PKG_CONFIG_SYSROOT_DIR=/

echo "=== сборка server (arm64) ==="
cargo build --release --target "$TRIPLE" -p app

BUILT_GUI=0
if [ "$GUI_LIBS_OK" = "1" ]; then
  echo "=== сборка gui (arm64, best-effort) ==="
  if cargo build --release --target "$TRIPLE" -p gui; then BUILT_GUI=1; else
    echo "WARN: gui не собрался под arm64 — пакеты будут server-only"
  fi
fi

# cargo-generate-rpm и tar читают target/release/*; положим туда arm64-бинарники,
# чтобы существующие метаданные путей разрешались без правок.
mkdir -p target/release
cp "target/$TRIPLE/release/opc-modbus-server" target/release/
[ "$BUILT_GUI" = "1" ] && cp "target/$TRIPLE/release/opc-modbus-config" target/release/ || rm -f target/release/opc-modbus-config

echo "=== .deb (arm64) ==="
cargo deb -p app --no-build --target "$TRIPLE"

echo "=== .rpm (aarch64) ==="
cargo generate-rpm -p crates/app --target "$TRIPLE"

echo "=== портируемый tar.gz (arm64) ==="
STAGE=/tmp/oms-linux-arm64
rm -rf "$STAGE"; mkdir -p "$STAGE"
cp target/release/opc-modbus-server "$STAGE"/
[ "$BUILT_GUI" = "1" ] && cp target/release/opc-modbus-config "$STAGE"/ || true
cp config.example.json README.md LICENSE.md installers/linux/opc-modbus-server.service "$STAGE"/
tar -C /tmp -czf target/oms-linux-arm64.tar.gz oms-linux-arm64

echo "=== раскладка в /work/dist/linux-arm64 ==="
OUT=${OUT_DIR:-/work/dist/linux-arm64}
mkdir -p "$OUT"
cp "target/$TRIPLE/debian"/*.deb "$OUT"/ 2>/dev/null || cp target/debian/*.deb "$OUT"/ 2>/dev/null || echo "WARN: .deb не найден"
# cargo-generate-rpm с --target пишет в target/<triple>/generate-rpm/.
cp "target/$TRIPLE/generate-rpm"/*.rpm "$OUT"/ 2>/dev/null || cp target/generate-rpm/*.rpm "$OUT"/ 2>/dev/null || echo "WARN: .rpm не найден"
cp target/oms-linux-arm64.tar.gz "$OUT"/

echo "=== проверка архитектуры ==="
file "target/$TRIPLE/release/opc-modbus-server" || true
echo "=== ГОТОВО (gui=$BUILT_GUI) ==="
ls -la "$OUT"
