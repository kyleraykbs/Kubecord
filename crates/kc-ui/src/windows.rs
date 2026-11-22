//! Separate, non-modal windows (image viewers):
//! the main window stays usable while they are open. Each is registered
//! under a key so asking again presents the open one instead of a
//! duplicate, and all of them close with the main window.

use std::rc::Rc;

use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::shell::AppState;

/// One open window and the overlay its failures are shown in.
pub(crate) struct AuxWindow {
    pub window: adw::Window,
    pub toasts: adw::ToastOverlay,
}

impl AppState {
    /// Presents the window open under `key`; false when there is none.
    pub(crate) fn present_window(&self, key: &str) -> bool {
        let window = self
            .aux_windows
            .borrow()
            .get(key)
            .map(|aux| aux.window.clone());
        match window {
            Some(window) => {
                window.present();
                true
            }
            None => false,
        }
    }

    /// The window open under `key`, if any.
    pub(crate) fn window_of(&self, key: &str) -> Option<adw::Window> {
        self.aux_windows
            .borrow()
            .get(key)
            .map(|aux| aux.window.clone())
    }

    /// A new separate window (not modal, not transient: nothing blocks the
    /// main window), registered under `key`. Its content goes inside a
    /// toast overlay, returned with it. Escape closes it.
    pub(crate) fn aux_window(
        self: &Rc<Self>,
        key: &str,
        title: &str,
        size: (i32, i32),
        content: &impl IsA<gtk::Widget>,
    ) -> (adw::Window, adw::ToastOverlay) {
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(content));
        let window = adw::Window::new();
        window.set_title(Some(title));
        window.set_default_size(size.0, size.1);
        window.set_modal(false);
        window.set_application(self.window.application().as_ref());
        window.set_content(Some(&toasts));
        let keys = gtk::EventControllerKey::new();
        let target = window.downgrade();
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape
                && let Some(window) = target.upgrade()
                && window.visible_dialog().is_none()
            {
                window.close();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        window.add_controller(keys);
        // GIFs keep playing while any of the app's windows has the focus.
        let app = Rc::downgrade(self);
        window.connect_is_active_notify(move |_| {
            if let Some(app) = app.upgrade() {
                app.queue_players_update();
            }
            crate::animation::set_window_focused(crate::animation::app_focused());
        });
        let app = Rc::downgrade(self);
        let key_owned = key.to_owned();
        window.connect_close_request(move |closing| {
            if let Some(app) = app.upgrade() {
                let mut windows = app.aux_windows.borrow_mut();
                // Only forget it if the entry is still this window.
                if windows
                    .get(&key_owned)
                    .is_some_and(|aux| &aux.window == closing)
                {
                    windows.remove(&key_owned);
                }
            }
            glib::Propagation::Proceed
        });
        if let Some(old) = self.aux_windows.borrow_mut().insert(
            key.to_owned(),
            AuxWindow {
                window: window.clone(),
                toasts: toasts.clone(),
            },
        ) {
            old.window.close();
        }
        (window, toasts)
    }

    /// Closes every separate window (the main window is closing).
    pub(crate) fn close_aux_windows(&self) {
        let windows: Vec<adw::Window> = self
            .aux_windows
            .borrow_mut()
            .drain()
            .map(|(_, aux)| aux.window)
            .collect();
        for window in windows {
            window.close();
        }
    }

    /// The registered window that has the focus, if one has.
    fn active_aux(&self) -> Option<adw::ToastOverlay> {
        self.aux_windows
            .borrow()
            .values()
            .find(|aux| aux.window.is_active())
            .map(|aux| aux.toasts.clone())
    }

    /// Shows a message in the active separate window, or as a notice in
    /// the main window.
    pub(crate) fn tell(&self, text: &str, seconds: u32) {
        match self.active_aux() {
            Some(toasts) => {
                let toast = adw::Toast::new(text);
                toast.set_timeout(seconds);
                toasts.add_toast(toast);
            }
            None => self.notify(text, None, seconds),
        }
    }
}
