//! Processes that must never be suspended.
//!
//! Freezing a session daemon does not merely pause it: every client that makes a
//! synchronous D-Bus call to a well-known name it owns blocks on a 25-second
//! timeout, and the name stays claimed so the bus cannot activate a replacement.
//! Freezing a modal dialog hangs whatever script is waiting on its answer —
//! a frozen `pinentry` hangs GPG and the SSH agent.
//!
//! Protection therefore is **not** a guard. Guards are configurable
//! (`GuardAction::Ignore`) and mean "not right now"; this list is unconditional
//! and means "never", and it deliberately outranks the user's own rules.
//!
//! Two tiers:
//!
//! 1. [`BUILTIN_DENY`] — a static list of executable names, matched
//!    case-insensitively with an optional trailing `*`.
//! 2. Owners of well-known names on the session bus, refreshed periodically.
//!    This catches infrastructure the static list has never heard of, which is
//!    the case the incident report could not attribute to any known application.

use std::collections::HashMap;
use std::time::Duration;

use tracing::debug;

use super::ProcessHandle;

/// Executables that are never suspended, whatever the configuration says.
///
/// Entries ending in `*` match by prefix. Matching is done on the basename of
/// the executable, case-insensitively.
pub(crate) const BUILTIN_DENY: &[&str] = &[
    // Session infrastructure: blocking these blocks every client that talks to
    // them over D-Bus.
    "xdg-desktop-portal*",
    "xdg-document-portal",
    "xdg-permission-store",
    "dbus-daemon",
    "dbus-broker",
    "pipewire",
    "pipewire-pulse",
    "wireplumber",
    "pulseaudio",
    "gvfsd*",
    "gnome-keyring-daemon",
    "at-spi-bus-launcher",
    "at-spi2-registryd",
    "ibus-daemon",
    "fcitx5",
    "dconf-service",
    "polkit-*",
    "elogind",
    "systemd*",
    // Modal dialogs: they consume no CPU by definition, but whoever spawned them
    // is blocked until the user answers.
    "zenity",
    "yad",
    "kdialog",
    "xmessage",
    "pinentry*",
    "ssh-askpass*",
    "polkit-gnome-authentication-agent-1",
    "lxpolkit",
    "gcr-prompter",
];

/// Why a process is protected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProtectionReason {
    /// Matched an entry in [`BUILTIN_DENY`].
    Builtin(&'static str),
    /// Owns a well-known name on the session bus.
    DbusNameOwner(String),
    /// The executable could not be identified at all.
    ///
    /// Treated as protected on purpose: if a missing `_NET_WM_PID` or an
    /// unreadable `/proc` entry produced "unknown", allowing the freeze would
    /// turn an identification failure into a way around the deny list.
    Unidentifiable,
}

impl std::fmt::Display for ProtectionReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Builtin(pattern) => write!(f, "built-in deny-list ({pattern})"),
            Self::DbusNameOwner(name) => write!(f, "owns D-Bus name {name}"),
            Self::Unidentifiable => write!(f, "executable could not be identified"),
        }
    }
}

/// Decides which processes must never be suspended.
///
/// Holds no logging state: "warn once per application" is a session-level
/// concern and lives in the engine, which keeps this type a pure function of its
/// inputs and keeps `system` free of any dependency on `state`.
/// `Clone` so the policy can be handed to a blocking task: classifying a whole
/// process tree reads `/proc` per process and must not run on the event loop,
/// and a `'static` closure cannot borrow the engine.
#[derive(Debug, Default, Clone)]
pub(crate) struct ProtectionPolicy {
    /// PID → one well-known name it owns. The name is kept so the operator is
    /// told *why* an application was spared, not merely that it was.
    dbus_owners: HashMap<u32, String>,
}

impl ProtectionPolicy {
    /// Install a freshly collected set of bus owners.
    ///
    /// Also the injection point that lets the second tier be tested without a
    /// session bus, which CI does not have.
    pub fn apply_refresh(&mut self, dbus_owners: HashMap<u32, String>) {
        debug!(
            count = dbus_owners.len(),
            "protection: dbus owner set updated"
        );
        self.dbus_owners = dbus_owners;
    }

    /// Whether an application the user is managing is protected, and why.
    ///
    /// Consults the built-in deny-list only. Owning a well-known bus name is
    /// **not** grounds for exempting an application here: ordinary desktop
    /// programs claim well-known names routinely — every media-capable browser
    /// registers `org.mpris.MediaPlayer2.*`, Telegram claims
    /// `org.telegram.desktop` — and treating that as infrastructure would put
    /// exactly the heaviest applications permanently out of reach, which is the
    /// opposite of what this daemon is for.
    ///
    /// `exe` is the executable basename when known. When it is `None` the
    /// kernel's `comm` is consulted, and if that is unavailable too the process
    /// is reported protected rather than assumed safe.
    pub fn check(&self, pid: u32, exe: Option<&str>) -> Option<ProtectionReason> {
        let name = match exe {
            Some(e) if !e.is_empty() => Some(e.to_string()),
            _ => super::process::comm(pid),
        };

        match name {
            Some(name) => matches_deny_list(&name).map(ProtectionReason::Builtin),
            None => Some(ProtectionReason::Unidentifiable),
        }
    }

    /// Whether a process that merely turned up in someone else's process tree is
    /// protected, and why.
    ///
    /// Here the session-bus tier does apply: a helper or daemon that is not the
    /// application being managed, and that owns a well-known name, is the case
    /// the deny-list cannot enumerate in advance — an unknown service whose
    /// disappearance would block clients on a D-Bus timeout.
    pub fn check_foreign(&self, pid: u32, exe: Option<&str>) -> Option<ProtectionReason> {
        if let Some(bus_name) = self.dbus_owners.get(&pid) {
            return Some(ProtectionReason::DbusNameOwner(bus_name.clone()));
        }
        self.check(pid, exe)
    }

    /// Split a process set into the part that may be suspended and the part that
    /// must not be.
    ///
    /// `own_roots` are the PIDs of the application itself. They are judged by
    /// [`check`](Self::check) alone; everything else in the tree is foreign and
    /// also faces the session-bus tier. Without that distinction an application
    /// that owns a bus name would be excluded from its own suspension, leaving
    /// its children stopped and its main process running.
    ///
    /// Applied to the expanded descendant tree, this is what stops a managed
    /// application from dragging session infrastructure down with it — a browser
    /// or file manager routinely has `gvfsd` or a portal helper somewhere below
    /// it in the process tree.
    pub fn partition(
        &self,
        procs: &[ProcessHandle],
        own_roots: &[u32],
    ) -> (Vec<ProcessHandle>, Vec<(u32, ProtectionReason)>) {
        let mut allowed = Vec::with_capacity(procs.len());
        let mut protected = Vec::new();
        for &handle in procs {
            let verdict = if own_roots.contains(&handle.pid) {
                self.check(handle.pid, None)
            } else {
                self.check_foreign(handle.pid, None)
            };
            match verdict {
                Some(reason) => protected.push((handle.pid, reason)),
                None => allowed.push(handle),
            }
        }
        (allowed, protected)
    }
}

/// Match a basename against [`BUILTIN_DENY`], returning the pattern that matched.
fn matches_deny_list(name: &str) -> Option<&'static str> {
    // Some binaries are installed under a dot-prefixed real name by wrapper
    // scripts (Guix does this: `.gvfsd-real`), so compare against both forms.
    let stripped = name.strip_prefix('.').unwrap_or(name);
    let candidates = [name, stripped, stripped.trim_end_matches("-real")];

    BUILTIN_DENY.iter().copied().find(|pattern| {
        candidates
            .iter()
            .any(|candidate| pattern_matches(pattern, candidate))
    })
}

fn pattern_matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        // Compared as bytes, not as a string slice: `name` comes from
        // `/proc/<pid>/comm`, which any process sets for itself and which may
        // contain non-ASCII. Slicing a `str` at `prefix.len()` panics when that
        // byte index falls inside a multi-byte character, and the length check
        // does not prevent it — it counts bytes too.
        Some(prefix) => {
            let (name, prefix) = (name.as_bytes(), prefix.as_bytes());
            name.len() >= prefix.len() && name[..prefix.len()].eq_ignore_ascii_case(prefix)
        }
        None => name.eq_ignore_ascii_case(pattern),
    }
}

/// Collect the PIDs owning well-known names on the session bus.
///
/// A free function rather than a method because it is executed on a blocking
/// thread: `spawn_blocking` requires a `'static` closure, so it cannot borrow
/// the policy. The caller applies the result via
/// [`ProtectionPolicy::apply_refresh`].
///
/// The whole sweep is bounded by `budget`. A session bus carries hundreds of
/// names and zbus's default per-call timeout is tens of seconds, so an
/// unbounded sweep on a hung bus would stall the daemon for minutes.
pub(crate) fn collect_dbus_owners(budget: Duration) -> Result<HashMap<u32, String>, zbus::Error> {
    let started = std::time::Instant::now();
    let connection = zbus::blocking::Connection::session()?;
    let proxy = zbus::blocking::fdo::DBusProxy::new(&connection)?;

    let mut owners = HashMap::new();
    for name in proxy.list_names()? {
        if started.elapsed() >= budget {
            debug!("protection: dbus sweep hit time budget, using partial result");
            break;
        }
        // Unique names (":1.42") are connections, not claimed services; only a
        // well-known name blocks other clients when its owner is frozen.
        if name.as_str().starts_with(':') {
            continue;
        }
        if let Ok(pid) = proxy.get_connection_unix_process_id(name.inner().clone()) {
            owners.entry(pid).or_insert_with(|| name.to_string());
        }
    }
    Ok(owners)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match_is_case_insensitive() {
        assert!(matches_deny_list("zenity").is_some());
        assert!(matches_deny_list("Zenity").is_some());
        assert!(matches_deny_list("ZENITY").is_some());
    }

    #[test]
    fn prefix_patterns_match() {
        assert!(matches_deny_list("xdg-desktop-portal").is_some());
        assert!(matches_deny_list("xdg-desktop-portal-gtk").is_some());
        assert!(matches_deny_list("pinentry-gtk").is_some());
        assert!(matches_deny_list("PINENTRY-curses").is_some());
        assert!(matches_deny_list("polkit-gnome-authentication-agent-1").is_some());
        assert!(matches_deny_list("systemd-userdbd").is_some());
    }

    /// Guix installs wrapped binaries as `.name-real`; the deny list must still
    /// recognise them, otherwise `gvfsd` on this very machine slips through.
    #[test]
    fn wrapper_prefixed_names_match() {
        assert!(matches_deny_list(".gvfsd-real").is_some());
        assert!(matches_deny_list(".at-spi2-registryd-real").is_some());
    }

    /// A process name is whatever its owner chose via `prctl(PR_SET_NAME)`, so
    /// it need not be ASCII. Matching must not panic when a multi-byte
    /// character straddles the prefix boundary — this runs inside the daemon's
    /// event loop, where a panic strands every process it has suspended.
    #[test]
    fn non_ascii_names_do_not_panic() {
        // "polkit-" is 7 bytes; "polkitЖ" is 6 ASCII bytes + a 2-byte char, so
        // byte index 7 lands inside that character.
        assert!(matches_deny_list("polkitЖ").is_none());
        assert!(matches_deny_list("Ж").is_none());
        assert!(matches_deny_list("gvfsЖd").is_none());
        // A genuinely matching name with a non-ASCII tail must still match.
        assert!(matches_deny_list("polkit-Ж").is_some());
        assert!(matches_deny_list("systemdЖ").is_some());
    }

    #[test]
    fn ordinary_applications_are_not_denied() {
        assert!(matches_deny_list("firefox").is_none());
        assert!(matches_deny_list("Alacritty").is_none());
        assert!(matches_deny_list("telegram-desktop").is_none());
        // Prefix patterns must not match a shorter string.
        assert!(matches_deny_list("xdg").is_none());
    }

    #[test]
    fn unknown_executable_is_protected() {
        let policy = ProtectionPolicy::default();
        // PID 0 never has a /proc entry, so neither exe nor comm resolves.
        assert_eq!(
            policy.check(0, None),
            Some(ProtectionReason::Unidentifiable)
        );
    }

    /// The second tier must work on a name the static list has never heard of —
    /// that is its whole purpose — but only for a foreign process.
    #[test]
    fn foreign_dbus_owner_is_protected_without_a_bus() {
        let mut policy = ProtectionPolicy::default();
        policy.apply_refresh(HashMap::from([(
            4242,
            "org.freedesktop.portal.Desktop".to_string(),
        )]));
        assert_eq!(
            policy.check_foreign(4242, Some("some-unknown-daemon")),
            Some(ProtectionReason::DbusNameOwner(
                "org.freedesktop.portal.Desktop".to_string()
            ))
        );
    }

    /// Owning a bus name must not exempt the application itself.
    ///
    /// Every media-capable browser registers `org.mpris.MediaPlayer2.*` and
    /// Telegram claims `org.telegram.desktop`; treating that as infrastructure
    /// put the heaviest applications permanently beyond management.
    #[test]
    fn an_applications_own_bus_name_does_not_exempt_it() {
        let mut policy = ProtectionPolicy::default();
        policy.apply_refresh(HashMap::from([
            (
                4242,
                "org.mpris.MediaPlayer2.chromium.instance4242".to_string(),
            ),
            (4243, "org.telegram.desktop".to_string()),
        ]));
        assert_eq!(policy.check(4242, Some("chrome")), None);
        assert_eq!(policy.check(4243, Some("telegram-desktop")), None);
    }

    /// The same PID is judged differently depending on whether it is the
    /// application being managed or something that merely turned up below it.
    #[test]
    fn partition_applies_the_bus_tier_only_to_foreign_processes() {
        let mut policy = ProtectionPolicy::default();
        policy.apply_refresh(HashMap::from([
            (100, "org.mpris.MediaPlayer2.chromium".to_string()),
            (200, "org.example.UnknownDaemon".to_string()),
        ]));
        let procs = [
            ProcessHandle {
                pid: 100,
                starttime: 1,
            },
            ProcessHandle {
                pid: 200,
                starttime: 2,
            },
        ];

        // 100 is the application itself, 200 is a stranger in its tree.
        let (allowed, protected) = policy.partition(&procs, &[100]);
        assert_eq!(
            allowed.iter().map(|h| h.pid).collect::<Vec<_>>(),
            vec![100],
            "the application's own process must stay suspendable"
        );
        assert_eq!(protected.len(), 1);
        assert_eq!(protected[0].0, 200);

        // With no roots declared, both are foreign and both are spared.
        let (allowed, protected) = policy.partition(&procs, &[]);
        assert!(allowed.is_empty());
        assert_eq!(protected.len(), 2);
    }

    #[test]
    fn partition_still_honours_the_builtin_list_for_own_processes() {
        let policy = ProtectionPolicy::default();
        // PID 0 resolves to no name at all, so it is unidentifiable and spared
        // even when declared as the application's own.
        let procs = [ProcessHandle {
            pid: 0,
            starttime: 1,
        }];
        let (allowed, protected) = policy.partition(&procs, &[0]);
        assert!(allowed.is_empty());
        assert_eq!(protected.len(), 1);
    }
}
