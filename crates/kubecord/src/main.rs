fn main() -> glib::ExitCode {
    use_freedesktop_notifications();
    kc_ui::logging::init();
    kc_ui::run()
}

/// Sends desktop notifications straight to the notification daemon
/// (`org.freedesktop.Notifications`, which GNOME, KDE, quickshell, mako, dunst
/// and the rest all provide) instead of letting GLib route them through the
/// xdg portal, whose Notification backend may forward to GNOME Shell even where
/// GNOME Shell is not running -- dropping every notification silently. Inside
/// a Flatpak the portal is the only way out, so it is left alone there, and an
/// explicit `GNOTIFICATION_BACKEND` from the user always wins.
fn use_freedesktop_notifications() {
    let sandboxed = std::path::Path::new("/.flatpak-info").exists();
    if sandboxed || std::env::var_os("GNOTIFICATION_BACKEND").is_some() {
        return;
    }
    // SAFETY: called first thing in `main`, before any thread exists.
    unsafe { std::env::set_var("GNOTIFICATION_BACKEND", "freedesktop") };
}
