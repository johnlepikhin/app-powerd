use std::path::PathBuf;

use tracing::{debug, info, warn};
use zbus::blocking::Connection;
use zbus::zvariant::{OwnedValue, Value};

use super::{cgroup_base_path, sanitize_unit_name};
use crate::error::SystemError;

const SYSTEMD_BUS: &str = "org.freedesktop.systemd1";
const SYSTEMD_PATH: &str = "/org/freedesktop/systemd1";
const SYSTEMD_MANAGER_IFACE: &str = "org.freedesktop.systemd1.Manager";
const DBUS_PROPERTIES_IFACE: &str = "org.freedesktop.DBus.Properties";

/// Manages systemd transient scopes via D-Bus.
pub struct SystemdManager {
    conn: Connection,
}

impl SystemdManager {
    /// Try to connect to the user session bus and verify systemd is available.
    pub fn try_connect() -> Option<Self> {
        let conn = Connection::session().ok()?;

        // Probe systemd version to verify it's reachable
        let reply = conn
            .call_method(
                Some(SYSTEMD_BUS),
                SYSTEMD_PATH,
                Some(DBUS_PROPERTIES_IFACE),
                "Get",
                &(SYSTEMD_MANAGER_IFACE, "Version"),
            )
            .ok()?;

        let version: OwnedValue = reply.body().deserialize().ok()?;
        info!(version = ?version, "connected to systemd user session");

        Some(Self { conn })
    }

    /// Create a transient scope and move PIDs into it.
    /// Returns the expected cgroup path.
    pub fn start_transient_scope(
        &self,
        name: &str,
        pids: &[u32],
        description: &str,
    ) -> Result<PathBuf, SystemError> {
        let scope_name = format!("app-powerd-{}.scope", sanitize_unit_name(name));

        // Build properties array for StartTransientUnit
        // Properties: Description (s), PIDs (au)
        let desc_prop = ("Description", Value::from(description));
        let pids_prop = (
            "PIDs",
            Value::Array(
                pids.iter()
                    .map(|&p| Value::from(p))
                    .collect::<Vec<_>>()
                    .into(),
            ),
        );
        let properties: Vec<(&str, Value<'_>)> = vec![desc_prop, pids_prop];

        // aux: array of (unit_name, array of properties) — empty for us
        let aux: Vec<(&str, Vec<(&str, Value<'_>)>)> = vec![];

        match self.conn.call_method(
            Some(SYSTEMD_BUS),
            SYSTEMD_PATH,
            Some(SYSTEMD_MANAGER_IFACE),
            "StartTransientUnit",
            &(&scope_name, "fail", &properties, &aux),
        ) {
            Ok(_) => {
                let cgroup_path = cgroup_base_path().join(&scope_name);
                debug!(scope = %scope_name, path = %cgroup_path.display(), "transient scope started");
                Ok(cgroup_path)
            }
            Err(e) => {
                warn!(scope = %scope_name, error = %e, "failed to start transient scope");
                Err(SystemError::CgroupError {
                    message: format!("StartTransientUnit failed: {e}"),
                })
            }
        }
    }
}
