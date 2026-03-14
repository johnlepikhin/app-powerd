# app-powerd

[![Crates.io](https://img.shields.io/crates/v/app-powerd.svg)](https://crates.io/crates/app-powerd)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

User-level Linux daemon that saves battery by automatically managing background GUI applications through cgroup v2
freeze and CPU throttling.

When you switch away from a window, app-powerd detects the focus change, waits a configurable delay, checks safety
guards (audio playback, camera, fullscreen, input idle), and then freezes or throttles the unfocused application. When
you switch back, the app is instantly resumed. All of this runs in userspace — no root required (with systemd transient
scopes or delegated cgroups).

The daemon ships with sensible defaults for 100+ popular Linux applications: terminals and media players are never
touched, browsers get gentle CPU throttling, messengers are frozen with periodic wake-ups for notifications, and static
viewers are frozen aggressively.

## Features

 - **Automatic focus tracking** — X11 (via `x11rb`), Wayland (`wlr-foreign-toplevel` for Sway/Hyprland), GNOME Shell
   (D-Bus Introspect)
 - **cgroup v2 freeze/thaw** — true kernel-level process suspension, zero CPU usage
 - **CPU throttling** — adjustable `cpu.weight`, `cpu.max` quota, and process niceness
 - **Safety guards** — skip suspend if app is playing audio, using camera, fullscreen, or user is idle
 - **Maintenance wake-ups** — periodically thaw frozen apps (e.g., messengers) to receive notifications
 - **Per-app rules** — match by `wm_class`, `app_id`, executable name, or command-line regex
 - **Profiles** — reusable policy templates (`freeze`, `throttle`, `editor`, `browser`, `messenger`, etc.)
 - **Power-aware** — different behavior on AC vs battery
 - **Hot config reload** — `SIGHUP`, CLI command, or automatic inotify watch
 - **IPC interface** — Unix socket with JSON protocol for status, metrics, manual freeze/thaw
 - **Graceful degradation** — auto-detects cgroup capabilities: direct write → systemd transient scopes →
   SIGSTOP/SIGCONT fallback

## Installation

### From crates.io

```bash
cargo install app-powerd
```

### From source

```bash
git clone https://github.com/johnlepikhin/app-powerd.git
cd app-powerd
cargo build --release
cp target/release/app-powerd ~/.local/bin/
```

### Feature flags

| Flag      | Default | Description                                                                   |
| --------- | ------- | ----------------------------------------------------------------------------- |
| `x11`     | Yes     | X11 focus tracking + XScreenSaver idle detection                              |
| `wayland` | No      | Wayland focus tracking via `wlr-foreign-toplevel-management` (Sway, Hyprland) |

```bash
# Wayland-only build
cargo install app-powerd --no-default-features --features wayland

# Both X11 and Wayland
cargo install app-powerd --features wayland
```

GNOME Shell Introspect backend works via D-Bus and is always available regardless of feature flags.

## Quick Start

```bash
# Start the daemon
app-powerd run

# In another terminal, check status
app-powerd status

# List tracked applications
app-powerd list
```

The daemon automatically creates a default config at `~/.config/app-powerd/config.yaml` on first run.

## Usage

```
app-powerd <COMMAND>

Commands:
  run            Start the daemon
  status         Show daemon status
  list           List tracked applications
  stats          Show daemon metrics (freeze/thaw/throttle counters)
  freeze <PID>   Force-freeze a process by PID
  thaw <PID>     Force-thaw a process by PID
  reload-config  Reload configuration from disk
  shutdown       Gracefully stop the daemon
```

### Examples

```bash
# Start with custom config
app-powerd run --config ~/my-config.yaml

# Check what the daemon is managing
app-powerd list
# APP                  STATE        PIDs     TITLE
# ------------------------------------------------------------------------
# firefox              Throttled    1234     GitHub - Mozilla Firefox
# TelegramDesktop      Frozen       5678     Telegram

# View metrics
app-powerd stats

# Manually freeze/thaw a process
app-powerd freeze 1234
app-powerd thaw 1234
```

## Configuration

### Config file

Location: `~/.config/app-powerd/config.yaml` (respects `$XDG_CONFIG_HOME`).

If the file doesn't exist, built-in defaults are used. Reload without restart:

```bash
app-powerd reload-config   # via IPC
kill -HUP $(pidof app-powerd)   # via signal
# also auto-reloads on file change (inotify)
```

### Top-level structure

```yaml
version: 1          # required, must be 1
defaults: { ... }   # global defaults
profiles: { ... }   # named reusable policy templates
rules: [ ... ]      # per-application matching rules
```

All sections except `version` are optional — a minimal config is just `version: 1`.

### `defaults`

| Field                         | Type     | Default   | Description                             |
| ----------------------------- | -------- | --------- | --------------------------------------- |
| `enabled`                     | bool     | `true`    | Master switch for the daemon            |
| `mode.ac`                     | `enable` | `disable` | `disable`                               |
| `mode.battery`                | `enable` | `disable` | `enable`                                |
| `timing.suspend_delay`        | Duration | `"30s"`   | Wait before suspending a background app |
| `timing.resume_grace`         | Duration | `"3s"`    | Grace period after app is focused again |
| `timing.min_suspend`          | Duration | `"5s"`    | Minimum time an app stays suspended     |
| `guards.audio_active`         | `check`  | `ignore`  | `check`                                 |
| `guards.mic_active`           | `check`  | `ignore`  | `check`                                 |
| `guards.camera_active`        | `check`  | `ignore`  | `check`                                 |
| `guards.fullscreen`           | `check`  | `ignore`  | `check`                                 |
| `guards.input_idle`           | Duration | null      | `null`                                  |
| `maintenance_resume.enabled`  | bool     | `false`   | Periodically thaw frozen apps           |
| `maintenance_resume.interval` | Duration | `"30s"`   | How often to thaw                       |
| `maintenance_resume.duration` | Duration | `"1s"`    | How long to keep thawed                 |

Guard action `ignore` can also be written as `skip` (alias).

### `profiles`

Profiles are named, reusable policy templates referenced by rules via `use_profile`.

| Field                | Type                  | Required   | Description                          |
| -------------------- | --------------------- | ---------- | ------------------------------------ |
| `action`             | `freeze`              | `throttle` | `ignore`                             |
| `suspend_delay`      | Duration              | no         | Override default suspend delay       |
| `nice`               | i32 (`-20`..`19`)     | no         | Process niceness (throttle only)     |
| `cpu_weight`         | u32 (`1`–`10000`)     | no         | Cgroup CPU weight (throttle only)    |
| `cpu_quota`          | string (e.g. `"40%"`) | no         | Cgroup CPU quota (throttle only)     |
| `maintenance_resume` | object                | no         | Override maintenance resume settings |
| `guards`             | object                | no         | Override guard settings              |

**Built-in profiles** (from `config/default.yaml`):

| Profile               | Action   | Delay | Notes                                          |
| --------------------- | -------- | ----- | ---------------------------------------------- |
| `ignore`              | ignore   | —     | Never touch the app                            |
| `freeze`              | freeze   | 60s   | Full cgroup freeze                             |
| `freeze-fast`         | freeze   | 20s   | Quick freeze, audio/mic/camera guards disabled |
| `throttle`            | throttle | 30s   | nice=5, cpu_weight=20, cpu_quota=40%           |
| `throttle-aggressive` | throttle | 30s   | nice=19, cpu_weight=1, cpu_quota=5%            |
| `editor`              | throttle | 45s   | nice=5, cpu_weight=50, cpu_quota=50%           |
| `browser`             | throttle | 30s   | nice=5, cpu_weight=20, cpu_quota=40%           |
| `messenger`           | freeze   | 1m    | Maintenance resume every 30s for 3s            |
| `email`               | freeze   | 3m    | Maintenance resume every 5m for 5s             |
| `background-worker`   | throttle | 60s   | nice=10, cpu_weight=10, cpu_quota=25%          |

### `rules`

Rules are evaluated top-to-bottom; **first match wins**. Each rule has three parts:

```yaml
- id: my-rule            # unique identifier (required)
  match:                  # matching criteria
    executable: [foo]
  policy:                 # what to do
    use_profile: throttle
```

#### Match fields

| Field                | Type            | Description                         |
| -------------------- | --------------- | ----------------------------------- |
| `executable`         | list of strings | Process executable name             |
| `wm_class`           | list of strings | X11 WM_CLASS                        |
| `app_id`             | list of strings | Wayland app_id                      |
| `desktop_file`       | list of strings | Desktop file basename               |
| `cmdline_regex`      | regex string    | Matched against `/proc/PID/cmdline` |
| `window_title_regex` | regex string    | Matched against window title        |

**Matching logic:** AND across fields, OR within lists. If both `executable` and `wm_class` are specified, both must
match. An empty `match: {}` creates a catch-all rule.

#### Policy fields

| Field                | Type     | Description                 |
| -------------------- | -------- | --------------------------- |
| `use_profile`        | string   | Reference a named profile   |
| `action`             | `freeze` | `throttle`                  |
| `suspend_delay`      | Duration | Override suspend delay      |
| `nice`               | i32      | Override niceness           |
| `cpu_weight`         | u32      | Override CPU weight         |
| `cpu_quota`          | string   | Override CPU quota          |
| `maintenance_resume` | object   | Override maintenance resume |
| `guards`             | object   | Override guards             |

### Policy resolution order

Each field is resolved independently using this priority chain:

 1. **Rule's direct override** (e.g. `policy.suspend_delay`)
 2. **Profile** (referenced via `use_profile`)
 3. **`defaults`** section
 4. **Hardcoded defaults** (action=freeze, suspend_delay=30s, etc.)

### Duration format

Durations use [humantime](https://docs.rs/humantime) syntax: `"30s"`, `"2m"`, `"1h"`, `"1m30s"`, `"500ms"`.

### Full example

```yaml
version: 1

defaults:
  mode:
    ac: disable
    battery: enable
  timing:
    suspend_delay: "30s"
    resume_grace: "3s"
  guards:
    audio_active: check
    fullscreen: check
    input_idle: "5m"

profiles:
  browser:
    action: throttle
    suspend_delay: "30s"
    nice: 5
    cpu_weight: 20
    cpu_quota: "40%"

  messenger:
    action: freeze
    suspend_delay: "1m"
    maintenance_resume:
      enabled: true
      interval: "30s"
      duration: "3s"

rules:
  - id: terminals
    match:
      executable: [kitty, foot, alacritty, wezterm-gui]
    policy:
      use_profile: ignore

  - id: firefox
    match:
      executable: [firefox, firefox-esr]
    policy:
      use_profile: browser

  - id: telegram
    match:
      wm_class: [TelegramDesktop]
    policy:
      use_profile: messenger

  - id: jetbrains
    match:
      cmdline_regex: "jetbrains|intellij|pycharm"
    policy:
      action: throttle
      cpu_quota: "50%"
```

### Minimal config

```yaml
version: 1
```

Everything works on built-in defaults: freeze background apps after 30s on battery, do nothing on AC.

## How It Works

```
Focus Backend ──→ FocusEvent ──→ Engine
                                   │
                          ┌────────┤
                          │  suspend_delay timer
                          │        │
                          │  Guards check (audio, camera, fullscreen, idle)
                          │        │
                          │  ┌─────┴─────┐
                          │  │           │
                          │  Freeze    Throttle
                          │  (cgroup    (cpu.weight,
                          │   freezer)   cpu.max, nice)
                          │
                          └── Focus returns → instant resume
```

 1. A focus backend (X11/Wayland/GNOME) detects window focus changes
 2. The engine starts a configurable delay timer for the unfocused app
 3. When the timer fires, safety guards are checked (audio, camera, fullscreen, idle)
 4. If all guards pass, the app is frozen (cgroup v2 freezer) or throttled (CPU limits)
 5. When the app regains focus, it is instantly resumed

## Supported Environments

| Environment    | Backend          | Protocol                            |
| -------------- | ---------------- | ----------------------------------- |
| X11            | `X11Backend`     | `_NET_ACTIVE_WINDOW` + XScreenSaver |
| Sway, Hyprland | `WaylandBackend` | `wlr-foreign-toplevel-management`   |
| GNOME Shell    | `GnomeBackend`   | D-Bus `org.gnome.Shell.Introspect`  |

## Cgroup Capabilities

The daemon auto-detects the best available cgroup control method:

| Method               | How it works                               | Requirements                                                  |
| -------------------- | ------------------------------------------ | ------------------------------------------------------------- |
| **DirectWrite**      | Writes directly to cgroup v2 files         | Delegated cgroup subtree (e.g., `systemd-run --user --scope`) |
| **SystemdTransient** | Creates transient systemd scopes via D-Bus | User session with systemd                                     |
| **SignalOnly**       | Falls back to `SIGSTOP`/`SIGCONT`          | Always available                                              |

## systemd Integration

Create `~/.config/systemd/user/app-powerd.service`:

```ini
[Unit]
Description=app-powerd battery-saving daemon
Documentation=https://github.com/johnlepikhin/app-powerd

[Service]
Type=simple
ExecStart=%h/.local/bin/app-powerd run
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
```

```bash
systemctl --user daemon-reload
systemctl --user enable --now app-powerd
```

## Library Usage

The `app-powerd-core` crate exposes the full engine, configuration, desktop backends, guards, and system interfaces as a
library:

```rust
use app_powerd_core::config::load_config;
use app_powerd_core::engine::Engine;
use app_powerd_core::desktop;

let config = load_config("~/.config/app-powerd/config.yaml")?;
let (engine, event_tx) = Engine::new(config, config_path);

// Start a focus backend
let backend = desktop::detect_backend()?;

// Run the engine
engine.run().await;
```

See the [API documentation](https://docs.rs/app-powerd-core) for details.

## License

[MIT](LICENSE)
