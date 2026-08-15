//! Wayland focus tracking backends.
//!
//! Three backends are supported:
//! - **wlr-foreign-toplevel** (Sway/Hyprland): uses native async Wayland event loop via `AsyncFd`
//! - **KWin scripting** (KDE Plasma): uses a small JavaScript bridge over D-Bus
//! - **GNOME Shell Introspect**: uses blocking `zbus` D-Bus API in `spawn_blocking` because
//!   the `MessageIterator` API doesn't have an async equivalent suitable for our polling model.

mod gnome;
mod kde;
#[cfg(feature = "wayland")]
mod wlr_toplevel;

use tokio::sync::mpsc;

use super::{FocusBackend, FocusEvent};
use crate::error::DesktopError;

/// Wayland focus backend with auto-detection of compositor protocol.
pub struct WaylandBackend {
    inner: WaylandInner,
}

enum WaylandInner {
    #[cfg(feature = "wayland")]
    Wlr(wlr_toplevel::WlrToplevelBackend),
    Kde(kde::KdeKWinBackend),
    Gnome(gnome::GnomeIntrospectBackend),
}

impl WaylandBackend {
    pub fn new() -> Result<Self, DesktopError> {
        // Try wlr-foreign-toplevel first (Sway, Hyprland, wlroots)
        #[cfg(feature = "wayland")]
        {
            match wlr_toplevel::WlrToplevelBackend::new() {
                Ok(backend) => {
                    tracing::info!("using wlr-foreign-toplevel-management protocol");
                    return Ok(Self {
                        inner: WaylandInner::Wlr(backend),
                    });
                }
                Err(e) => {
                    tracing::debug!(error = %e, "wlr-foreign-toplevel not available");
                }
            }
        }

        // Try KDE KWin scripting D-Bus
        match kde::KdeKWinBackend::new() {
            Ok(backend) => {
                tracing::info!("using KDE KWin scripting D-Bus");
                return Ok(Self {
                    inner: WaylandInner::Kde(backend),
                });
            }
            Err(e) => {
                tracing::debug!(error = %e, "KDE KWin scripting not available");
            }
        }

        // Try GNOME Shell Introspect D-Bus
        match gnome::GnomeIntrospectBackend::new() {
            Ok(backend) => {
                tracing::info!("using GNOME Shell Introspect D-Bus");
                Ok(Self {
                    inner: WaylandInner::Gnome(backend),
                })
            }
            Err(e) => {
                tracing::debug!(error = %e, "GNOME Shell Introspect not available");
                Err(DesktopError::WaylandConnection(
                    "no supported Wayland compositor protocol found".into(),
                ))
            }
        }
    }
}

#[async_trait::async_trait]
impl FocusBackend for WaylandBackend {
    async fn run(self: Box<Self>, tx: mpsc::Sender<FocusEvent>) -> Result<(), DesktopError> {
        match self.inner {
            #[cfg(feature = "wayland")]
            WaylandInner::Wlr(backend) => Box::new(backend).run(tx).await,
            WaylandInner::Kde(backend) => Box::new(backend).run(tx).await,
            WaylandInner::Gnome(backend) => Box::new(backend).run(tx).await,
        }
    }
}
