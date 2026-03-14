use std::fs;
use std::path::Path;
use std::time::Duration;

use tracing::info;

/// Power source status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerSource {
    Battery,
    Ac,
    Unknown,
}

/// Detect current power source from sysfs.
pub fn detect_power_source() -> PowerSource {
    let power_supply = Path::new("/sys/class/power_supply");

    if !power_supply.exists() {
        return PowerSource::Unknown;
    }

    let mut has_battery = false;

    if let Ok(entries) = fs::read_dir(power_supply) {
        for entry in entries.flatten() {
            let path = entry.path();
            let type_file = path.join("type");

            if let Ok(supply_type) = fs::read_to_string(&type_file) {
                let supply_type = supply_type.trim();

                if supply_type == "Mains" {
                    let online_file = path.join("online");
                    if let Ok(online) = fs::read_to_string(&online_file) {
                        if online.trim() == "1" {
                            return PowerSource::Ac;
                        }
                    }
                } else if supply_type == "Battery" {
                    has_battery = true;
                }
            }
        }
    }

    // Only report Battery if an actual battery supply exists
    if has_battery {
        PowerSource::Battery
    } else {
        PowerSource::Unknown
    }
}

/// Spawn a task that periodically checks power source and notifies on changes.
pub fn watch_power_source(
    interval: Duration,
    tx: tokio::sync::mpsc::Sender<PowerSource>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut last = detect_power_source();
        info!(?last, "initial power source");
        let _ = tx.send(last).await;

        loop {
            tokio::time::sleep(interval).await;
            let current = detect_power_source();
            if current != last {
                info!(from = ?last, to = ?current, "power source changed");
                last = current;
                if tx.send(current).await.is_err() {
                    break;
                }
            }
        }
    })
}
