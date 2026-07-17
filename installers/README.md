# Дистрибутивы и инсталляторы

**ПРОБНАЯ СБОРКА.** Бинарники собираются с feature `trial`: сервер работает
непрерывно не более **3 часов**, затем корректно останавливается (в консоли,
службе и встроенном сервере GUI). Для полной сборки — убрать `features =
["trial"]` у зависимости `svc-host` в `crates/app/Cargo.toml` и
`crates/gui/Cargo.toml` и пересобрать.

## Windows: готовые файлы

Одна команда пересобирает текущий код со статическим CRT и создаёт финальные
файлы в `dist/windows/`:

```powershell
powershell -ExecutionPolicy Bypass -File scripts/pack-win.ps1
```

В Git публикуются три готовых файла:

- `dist/windows/opc-modbus-server.exe` — сервер;
- `dist/windows/opc-modbus-config.exe` — отдельный GUI-конфигуратор;
- `dist/windows/OPC-Modbus-Server-Setup-0.1.0-trial-win64.exe` — готовый EXE-инсталлятор, внутри которого находятся сервер, GUI-конфигуратор, пример конфигурации и деинсталлятор.

Установщик поднимает UAC, копирует файлы в `Program Files\OPC Modbus Server`,
создаёт конфигурацию в `ProgramData\OPC Modbus Server`, регистрирует службу с
автозапуском и добавляет ярлык конфигуратора.

`target/`, runtime-каталоги `data/`/`pki/` и промежуточные файлы сборки в Git
не добавляются.

### Канонический .msi (WiX Toolset)

`installers/windows/main.wxs` — исходник WiX v3. Нужен установленный WiX
(candle/light) либо `cargo install cargo-wix`. Сборка после
`cargo build --release`:

```
candle -arch x64 installers/windows/main.wxs -o target/main.wixobj
light  target/main.wixobj -o target/OPC-Modbus-Server.msi
```

.msi регистрирует службу (ServiceInstall), кладёт конфиг в ProgramData и ярлык.

## Linux (.deb / .rpm / .tar.gz) через Docker — без тулчейна на хосте

На машине с Docker (Windows/Linux) из корня workspace:

```bash
# полная сборка (тулчейн, системные библиотеки, cargo-deb, cargo-generate-rpm):
MSYS_NO_PATHCONV=1 docker run --rm \
  -v "$PWD:/work" -v oms_target_linux:/work/target -v oms_tools:/tools \
  -w /work rust:1.90-bookworm bash installers/linux/build-in-docker.sh

# только переупаковка (после того как build уже прогнан — секунды):
MSYS_NO_PATHCONV=1 docker run --rm \
  -v "$PWD:/work" -v oms_target_linux:/work/target -v oms_tools:/tools \
  -w /work rust:1.90-bookworm bash installers/linux/pack-only.sh
```

Артефакты появляются в **`dist/linux/`**:
- `opc-modbus-server_0.1.0-1_amd64.deb` — Debian/Ubuntu/**Astra Linux**;
- `opc-modbus-server-0.1.0-1.x86_64.rpm` — RHEL/Rocky/ALT-подобные;
- `oms-linux.tar.gz` — портируемый (оба бинаря + конфиг + unit).

Собрано на glibc **2.36** (Debian 12). Для более старых систем поднимите базовый
образ на `rust:1.90-bullseye` (glibc 2.31) в скрипте.

Проверено: установка `.deb` в чистый `debian:12-slim`, `validate` и `run`
работают, trial-лимит активен.

### Linux arm64 (aarch64) — Astra/Debian/Ubuntu на ARM

Собирается **кросс-компиляцией** (нативный x86-компилятор + линкер
`aarch64-linux-gnu`, а не QEMU-эмуляция — минуты вместо часов). Docker сам
подхватывает arm64-образ по имени тега, поэтому платформу базового образа
фиксируем явно `--platform linux/amd64`:

```bash
MSYS_NO_PATHCONV=1 docker run --rm --platform linux/amd64 \
  -v "$PWD:/work" -v oms_target_cross_arm64:/work/target -v oms_tools_cross:/tools \
  -e CARGO_INSTALL_ROOT=/tools -w /work \
  rust:1.90-bookworm bash installers/linux/cross-build-arm64.sh
```

Артефакты — в **`dist/linux-arm64/`**:
- `opc-modbus-server_0.1.0-1_arm64.deb` (`Architecture: arm64`);
- `opc-modbus-server-0.1.0-1.aarch64.rpm`;
- `oms-linux-arm64.tar.gz` (оба бинаря + конфиг + unit).

Оба бинарника — `ELF … ARM aarch64`, glibc ≥ 2.35 (`Depends: libc6 (>= 2.35)`).
GUI-конфигуратор кросс-собирается вместе с сервером (arm64 GTK/GL dev-либы
ставятся через multiarch `:arm64`); если на будущей машине они окажутся
недоступны, скрипт сам соберёт arm64 в режиме server-only и сообщит.

Пакет ставит бинарники в `/usr/bin`, конфиг в `/etc/opc-modbus-server/`,
systemd-unit в `/usr/lib/systemd/system` (включается автоматически; запуск —
`systemctl start opc-modbus-server`). Метаданные — в
`crates/app/Cargo.toml` (`[package.metadata.deb]` / `[package.metadata.generate-rpm]`).

### GUI-конфигуратор на Linux (X11/Wayland)

`opc-modbus-config` — полноценное desktop-приложение и на Linux (проверено
запуском под Xvfb: окно egui инициализируется и работает). Серверу графика
**не нужна** (headless), а GUI грузит libGL/X11/GTK/xkbcommon через `dlopen`,
поэтому автодетект их не видит — они объявлены как **слабые зависимости**
(`Recommends` в .deb, `Recommends` в .rpm):

- deb: `libgtk-3-0, libgl1, libxkbcommon-x11-0` (GTK3 тянет X11/xcb/cursor/randr);
- rpm: `gtk3, mesa-libGL, libxkbcommon-x11`.

На десктопе `apt`/`dnf` подтянут их сами; на headless-сервере
`apt install --no-install-recommends ./*.deb` поставит только сервер без графики.
Для портируемого `.tar.gz` на «голой» машине под GUI поставьте вручную, напр.
Debian/Ubuntu/Astra:

```bash
sudo apt install libgtk-3-0 libgl1 libxkbcommon-x11-0
```

### Нативно (если тулчейн уже есть на Linux)

```
cargo install cargo-deb cargo-generate-rpm
cargo build --release -p app -p gui
cargo deb -p app --no-build          # -> target/debian/*.deb
cargo generate-rpm -p crates/app     # -> target/generate-rpm/*.rpm
```
