#[cfg(feature = "wayland")]
mod wlr_toplevel;
mod gnome;

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
            WaylandInner::Gnome(backend) => Box::new(backend).run(tx).await,
        }
    }

    fn is_fullscreen(&self, window_id: u64) -> bool {
        match &self.inner {
            #[cfg(feature = "wayland")]
            WaylandInner::Wlr(backend) => backend.is_fullscreen(window_id),
            WaylandInner::Gnome(backend) => backend.is_fullscreen(window_id),
        }
    }
}
