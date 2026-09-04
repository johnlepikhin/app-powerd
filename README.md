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
 - **Protected processes** — session infrastructure and modal dialogs are never suspended, overriding
   configuration ([details](#protected-processes))
 - **Crash-safe suspension** — a freeze journal lets a killed daemon's victims be resumed on restart,
   or by `app-powerd thaw-all` with no daemon at all

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
  run              Start the daemon
  status           Show daemon status
  list             List tracked applications
  stats            Show daemon metrics
  freeze <TARGET>  Force-freeze a PID or a tracked application by name
  thaw <TARGET>    Force-thaw a PID or a tracked application by name
  thaw-all         Resume everything app-powerd suspended (works without a daemon)
  reload-config    Reload configuration from disk
  force <MODE>     Override the detected power source (battery|ac|auto)
  shutdown         Gracefully stop the daemon
```

### Examples

```bash
# Start with custom config
app-powerd run --config ~/my-config.yaml

# Check what the daemon is managing
app-powerd list
# APP                  STATE       IN STATE   PROFILE      RULE               PIDS      TITLE
# --------------------------------------------------------------------------------------------
# firefox              THROTTLED   4m12s      browser      firefox            37        GitHub - Mozilla Firefox
# TelegramDesktop      FROZEN      1h03m      messenger    telegram           3         Telegram
# Zenity               PROTECTED   8s         -            built-in protec... 1         Add a new entry

# Check which suspension mechanism is actually in use
app-powerd status
# app-powerd status:
#   enabled:       true
#   power source:  battery (auto)
#   cgroup mode:   SignalOnly
#   cpu control:   unavailable (cpu_weight/cpu_quota are NOT applied)
#   tracked apps:  11
#   protected:     2
#   uptime:        3821s
#   protocol:      v2

# View metrics
app-powerd stats

# Manually freeze/thaw, by PID or by application name
app-powerd freeze 1234
app-powerd thaw TelegramDesktop

# Emergency: resume everything, even with no daemon running
app-powerd thaw-all
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

### Pre-thaw triggers (X11)

The X11 backend additionally watches `_NET_CURRENT_DESKTOP` PropertyNotify
and `_NET_ACTIVE_WINDOW` ClientMessage events on the root window. A frozen
tracked app is *pre-thawed* (resumed and parked in Background with a fresh
suspend timer) when:

 - The user switches to a virtual desktop where the app's window claims to
   live (matching `_NET_WM_DESKTOP`). Catches the common case of switching
   to a workspace that hosts a window currently minimized to tray —
   without this, the app stays SIGSTOP'd and cannot respond to tray-icon
   activation, which would normally route via DBus through the still-frozen
   process.
 - A panel, taskbar or launcher sends an `_NET_ACTIVE_WINDOW` ClientMessage
   targeting one of the app's tracked windows.

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

`app-powerd status` reports the mode in use. On a system without cgroup delegation the daemon warns
**once** at startup and runs in `SignalOnly`, where `nice` still applies but **`cpu_weight` and
`cpu_quota` do not** — profiles that declare them are named in that warning. This is expected on
distributions that do not delegate a cgroup subtree to the user session, and the daemon is designed
to work correctly there; it does not require cgroups.

### Signal mode and recovery

`SIGSTOP` cannot be undone by the process it stops, so a daemon that dies without releasing its
charges would strand them permanently. Two mechanisms prevent that:

- **Freeze journal** — `$XDG_RUNTIME_DIR/app-powerd/frozen.json` records what is suspended, written
  before the first signal and cleared after the last. Each entry stores the PID *and* its start time,
  so a recycled PID is never signalled by mistake. On startup the daemon replays it.
- **`app-powerd thaw-all`** — resumes everything, and works with no daemon running: if nothing holds
  the instance lock it replays the journal directly. This is the recovery path after `kill -9`.

If the daemon is stopped by other means and processes are left stopped, they can be found with:

```bash
ps -eo pid,stat,comm --no-headers | awk '$2 ~ /T/'
```

## Protected processes

Some processes are never suspended, whatever the configuration says. Freezing a session daemon does
not merely pause it: every client making a synchronous D-Bus call to a well-known name it owns blocks
on a 25-second timeout, and the bus cannot activate a replacement while the name is held. Freezing a
modal dialog hangs whatever is waiting for the user's answer — a frozen `pinentry` hangs GPG and the
SSH agent.

The list is compiled in, **takes precedence over your rules**, and cannot be disabled:

```
xdg-desktop-portal*, xdg-document-portal, xdg-permission-store,
dbus-daemon, dbus-broker, pipewire, pipewire-pulse, wireplumber, pulseaudio,
gvfsd*, gnome-keyring-daemon, at-spi-bus-launcher, at-spi2-registryd,
ibus-daemon, fcitx5, dconf-service, polkit-*, elogind, systemd*,
zenity, yad, kdialog, xmessage, pinentry*, ssh-askpass*,
polkit-gnome-authentication-agent-1, lxpolkit, gcr-prompter
```

As a second line of defence, a process that turns up **inside a managed application's process tree**
and owns a well-known name on the session bus is also spared — the unknown helper or daemon the list
above cannot enumerate in advance. This tier deliberately does not apply to the application itself:
ordinary programs claim well-known names all the time (every media-capable browser registers
`org.mpris.MediaPlayer2.*`, Telegram claims `org.telegram.desktop`), and sparing them would put the
heaviest applications on the system permanently beyond management. The check costs a little bus
traffic and can be turned off with `defaults.protection.dbus_check`.

Applications covered by either rule appear as `PROTECTED` in `app-powerd list`, with the reason in
the `RULE` column, and are counted in `app-powerd status`. If a rule of yours appears to be ignored,
this is the first thing to check.

## Logging

The daemon logs to stderr and does not manage a log file itself; colour is used only when a terminal
is attached. Configure rotation externally, for example with logrotate:

```
# ~/.config/logrotate/app-powerd
/home/YOU/.local/state/log/app-powerd.log {
    size 10M
    rotate 3
    copytruncate
    missingok
    notifempty
}
```

Verbosity follows `RUST_LOG` (`RUST_LOG=debug app-powerd run`). Expected conditions — a process that
exited before a signal reached it, a partially applied operation — are logged at `debug`, and
repeated warnings about the same process are suppressed for five minutes; `warns_suppressed` in
`app-powerd stats` counts what was withheld.

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

## Breaking changes in 2.0.0

**Upgrading.** The 1.x daemon does not write a freeze journal, so processes it has already suspended
are unknown to the new binary. Release them before switching:

```bash
# 1. stop the old daemon, then resume whatever it left stopped
ps -eo pid,stat --no-headers | awk '$2 ~ /T/ {print $1}' | xargs -r kill -CONT
# 2. install the new binary and start it
```

**Rolling back to 1.x.** Run `app-powerd thaw-all` with the 2.x binary first, then remove
`$XDG_RUNTIME_DIR/app-powerd/frozen.json` — 1.x neither reads nor cleans it up.

**IPC protocol.** Bumped to v2 and reported in `app-powerd status`. `Freeze`/`Thaw` now take a target
that is either a PID or an application name; the 1.x PID-only wire form is still accepted for one
release, so a 2.x CLI keeps working against a not-yet-restarted 1.x daemon. A request that cannot be
decoded now gets an explicit `protocol mismatch` reply instead of a closed socket.

**Configuration.** Two additive fields, both optional: `defaults.reconcile_interval` (default `30s`)
and `defaults.protection.dbus_check` (default `true`). Existing configs load unchanged. Name matching
in rules is now case-insensitive, which can make a rule match windows it previously missed.

**Library API** (`app-powerd-core`). `AppId` gained a case-folded identity and is no longer a tuple
struct; `AppEntry::pids` returns `Vec<u32>` rather than a slice, and `add_pid` takes and validates a
real PID. Freeze, thaw and throttle return an `ApplyReport` describing each process's outcome instead
of a single `Result`. `Engine::with_journal` is the constructor to use when persistence is wanted;
`Engine::new` keeps the journal disabled.

## License

[MIT](LICENSE)
