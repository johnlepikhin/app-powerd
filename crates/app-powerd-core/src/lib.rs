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
//! - [`system`] — cgroup v2 management, process identity, protection, power source, systemd D-Bus
//! - [`state`] — Application state machine, registry, and the freeze journal
//! - [`ipc`] — Unix socket IPC server/client and protocol
//! - [`metrics`] — Atomic counters for freeze/thaw/throttle operations
//!
//! # Suspension safety
//!
//! `SIGSTOP` cannot be undone by the process it stops, so the daemon is the only
//! thing that can release what it suspends. Two mechanisms keep that promise:
//!
//! - [`state::FreezeJournal`] records what is suspended *before* the first
//!   signal, so an abrupt exit is recoverable on the next start.
//! - A periodic reconcile sweep inside [`engine::Engine::run`] retires
//!   applications whose processes have exited, so the daemon never signals a PID
//!   that no longer exists.
//!
//! Only the second is automatic: every [`engine::Engine`], however constructed,
//! runs the reconcile sweep. The journal is not — [`engine::Engine::new`] builds
//! an engine with journalling **disabled**, so an embedder that wants suspension
//! to survive a crash must construct the engine with
//! [`engine::Engine::with_journal`], passing a journal loaded from
//! [`state::FreezeJournal::default_path`] (and recovered first, if a previous run
//! left one behind).

pub mod config;
pub mod desktop;
pub mod engine;
pub mod error;
pub mod guards;
pub mod ipc;
pub mod metrics;
pub mod state;
pub mod system;
