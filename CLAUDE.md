# app-powerd

User-level Linux daemon для экономии батареи через автоматическое управление фоновыми GUI-приложениями.

## Quick Start

```bash
cargo build                      # сборка (default: x11)
cargo build --features wayland   # сборка с Wayland
cargo test                       # unit + integration тесты
cargo run --bin app-powerd -- run              # запуск daemon
cargo run --bin app-powerd -- status           # CLI: статус демона
cargo run --bin app-powerd -- list             # CLI: список приложений
```

## Architecture

Cargo workspace с двумя крейтами:

```
crates/
├── app-powerd-core/    # Библиотека: engine, config, desktop, guards, system, ipc, state
└── app-powerd/         # Бинарь: app-powerd (daemon + CLI)
```

### Core модули

| Модуль | Назначение |
|--------|-----------|
| `engine` | Главный event loop, координация всех подсистем |
| `config/` | YAML конфигурация, правила, профили, matching |
| `desktop/` | Focus tracking: X11, Wayland (wlr-toplevel, GNOME Introspect) |
| `guards/` | Проверки перед suspend: audio, camera, fullscreen, input idle |
| `system/` | cgroup v2, freeze/thaw, throttle, process info, power source, systemd D-Bus |
| `state/` | State machine (Active→Background→Throttled/Frozen), AppRegistry |
| `ipc/` | Unix socket IPC: protocol, server, client |
| `metrics` | Atomic counters: frozen/thawed/throttled totals, time_in_frozen/throttled |

### Event Flow

```
FocusBackend → FocusEvent → Engine::event_rx → handle_focus_changed
                                              → handle_suspend_timer (after delay)
                                              → guards check → freeze/throttle
IpcServer    → IpcRequest  → Engine::handle_ipc → IpcResponse
ConfigWatcher/SIGHUP       → EngineEvent::ConfigReloaded
PowerSource                → EngineEvent::PowerSourceChanged
```

### Key Types

- `Engine` — singleton, owns `AppRegistry`, `RulesEngine`, `CgroupManager`
- `AppEntry` — tracked app: state, PIDs, windows, policy, cgroup path, timers
- `AppId` — derived from wm_class > app_id > executable > window_id
- `ResolvedPolicy` — merged result of rule + profile + defaults
- `AppState` — `Active | Background | Throttled | Frozen`
- `FocusBackend` (trait) — implemented by `X11Backend`, `WaylandBackend`

### Feature Flags

| Feature | Description | Dependencies |
|---------|-------------|-------------|
| `x11` (default) | X11 focus tracking + XScreenSaver idle | `x11rb` |
| `wayland` | wlr-foreign-toplevel protocol (Sway/Hyprland) | `wayland-client`, `wayland-protocols-wlr` |

GNOME Shell Introspect backend работает через `zbus` и доступен всегда (без feature flag).

### Cgroup Capabilities (auto-detected)

1. **DirectWrite** — прямая запись в cgroup v2 subtree
2. **SystemdTransient** — transient scopes через `zbus` D-Bus (`StartTransientUnit`)
3. **SignalOnly** — fallback на SIGSTOP/SIGCONT

## Config

Файл: `~/.config/app-powerd/config.yaml` (пример: `config/default.yaml`)

Структура: `version`, `defaults` (mode, timing, guards, maintenance_resume), `profiles`, `rules`.

Reload: `app-powerd reload-config` или SIGHUP или inotify watch.

## Testing

```bash
cargo test                          # все тесты
cargo test --lib                    # только unit
cargo test --test integration       # только integration
```

Integration тесты (`tests/integration.rs`): focus transitions, IPC round-trip, config reload, guards blocking.

## IPC

Unix socket: `$XDG_RUNTIME_DIR/app-powerd.sock`

Protocol: length-prefixed JSON (4 bytes u32 BE + JSON payload).

Commands: `List`, `Status`, `Stats`, `Freeze{pid}`, `Thaw{pid}`, `ReloadConfig`, `Shutdown`.

## Conventions

- Rust edition 2021, `thiserror` для ошибок, `tracing` для логирования
- Async runtime: `tokio` (current_thread)
- Config parsing: `serde` + `serde_yaml`
- D-Bus: `zbus` (blocking API для systemd, blocking+async для GNOME)
- Код на английском, коммиты на английском
