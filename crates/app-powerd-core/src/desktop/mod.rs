pub(crate) mod window;

#[cfg(feature = "x11")]
pub mod x11;

pub mod wayland;

pub use window::WindowInfo;

use tokio::sync::mpsc;

/// Events emitted by focus backends.
#[derive(Debug, Clone)]
pub enum FocusEvent {
    /// Active window changed.
    FocusChanged(WindowInfo),
    /// A window was closed.
    WindowClosed { window_id: u64 },
}

/// Trait for desktop focus tracking backends.
#[async_trait::async_trait]
pub trait FocusBackend: Send {
    /// Start monitoring and send events to the channel.
    async fn run(
        self: Box<Self>,
        tx: mpsc::Sender<FocusEvent>,
    ) -> Result<(), crate::error::DesktopError>;
}

/// Auto-detect and create the appropriate backend.
pub fn detect_backend() -> Result<Box<dyn FocusBackend>, crate::error::DesktopError> {
    // Check Wayland first
    if std::env::var("WAYLAND_DISPLAY").is_ok() {
        tracing::info!("detected Wayland display");
        match wayland::WaylandBackend::new() {
            Ok(backend) => return Ok(Box::new(backend)),
            Err(e) => tracing::warn!(error = %e, "Wayland backend failed, trying X11"),
        }
    }

    // Then X11
    if std::env::var("DISPLAY").is_ok() {
        #[cfg(feature = "x11")]
        {
            tracing::info!("detected X11 display");
            return Ok(Box::new(x11::X11Backend::new()?));
        }
        #[cfg(not(feature = "x11"))]
        {
            tracing::warn!("X11 detected but x11 feature not enabled. Rebuild with --features x11 to enable X11 support");
        }
    }

    Err(crate::error::DesktopError::NoDisplayServer)
}
