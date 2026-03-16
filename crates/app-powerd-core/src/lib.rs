#![warn(missing_docs)]

//! Core library for [`app-powerd`](https://github.com/johnlepikhin/app-powerd) —
//! a user-level Linux daemon that saves battery by automatically freezing and
//! throttling unfocused GUI applications via cgroup v2.
//!
//! This crate provides the engine, configuration, desktop focus backends,
//! safety guards, cgroup/systemd system interfaces, IPC protocol, and
//! application state management used by the `app-powerd` binary.
//!
//! # Modules
//!
//! - [`config`] — YAML configuration loading, rules, profiles, matching
//! - [`desktop`] — Focus tracking backends: X11, Wayland (wlr-toplevel), GNOME Shell
//! - [`engine`] — Main event loop coordinating all subsystems
//! - [`guards`] — Safety checks before suspend: audio, camera, fullscreen, input idle
//! - [`system`] — cgroup v2 management, process info, power source detection, systemd D-Bus
//! - [`state`] — Application state machine and registry
//! - [`ipc`] — Unix socket IPC server/client and protocol
//! - [`metrics`] — Atomic counters for freeze/thaw/throttle operations

pub mod config;
pub mod desktop;
pub mod engine;
pub mod error;
pub mod guards;
pub mod ipc;
pub mod metrics;
pub mod state;
pub mod system;
