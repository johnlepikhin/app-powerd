use std::time::Duration;

/// Get user idle time from XScreenSaver extension.
/// Returns None if unavailable.
#[cfg(feature = "x11")]
pub fn get_idle_time() -> Option<Duration> {
    use std::sync::OnceLock;

    use x11rb::connection::Connection;
    use x11rb::protocol::screensaver;
    use x11rb::rust_connection::RustConnection;

    static X11_CONN: OnceLock<Option<(RustConnection, usize)>> = OnceLock::new();

    let conn_opt = X11_CONN.get_or_init(|| {
        RustConnection::connect(None).ok()
    });

    let (conn, screen_num) = conn_opt.as_ref()?;
    let root = conn.setup().roots[*screen_num].root;
    let reply = screensaver::query_info(conn, root).ok()?.reply().ok()?;
    Some(Duration::from_millis(reply.ms_since_user_input as u64))
}

#[cfg(not(feature = "x11"))]
pub fn get_idle_time() -> Option<Duration> {
    None
}

/// Check if user has had recent input (idle time < threshold).
/// Used as a guard: if user is NOT idle, block suspend.
pub fn has_recent_input(threshold: Duration) -> bool {
    match get_idle_time() {
        Some(idle) => idle < threshold,
        None => false,
    }
}
