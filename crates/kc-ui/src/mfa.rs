//! Discord's two-factor prompt for sensitive actions (deleting a server,
//! giving it away, turning on Community where the server requires 2FA for
//! moderators): the request answers 60003 with a ticket, the code (or the
//! password) is checked at `/mfa/finish`, and the request is sent again
//! with the token that gives.
//!
//! The code and password are only sent to Discord, never kept or logged.

use std::future::Future;
use std::rc::Rc;

use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use kc_http::Error;

use crate::shell::AppState;
use crate::video_setup::spawn_local;

/// The ways to answer we offer, in Discord's names, with what to call them.
const METHODS: [(&str, &str); 3] = [
    ("totp", "Authenticator Code"),
    ("backup", "Backup Code"),
    ("password", "Password"),
];

/// The methods we offer out of what Discord accepts (in our order).
fn offered(accepted: &[String]) -> Vec<(&'static str, &'static str)> {
    METHODS
        .iter()
        .copied()
        .filter(|(method, _)| accepted.iter().any(|accepted| accepted == method))
        .collect()
}

impl AppState {
    /// Runs `request`; if Discord asks for two-factor, asks for the code,
    /// verifies it and runs `request` again with the token.
    pub(crate) fn with_mfa<T, F, Fut>(
        self: &Rc<Self>,
        what: &'static str,
        request: F,
        done: impl FnOnce(&Rc<AppState>, kc_http::Result<T>) + 'static,
    ) where
        T: Send + 'static,
        F: Fn(Option<String>) -> Fut + 'static,
        Fut: Future<Output = kc_http::Result<T>> + Send + 'static,
    {
        spawn_local(self, request(None), move |app, result| match result {
            Err(Error::Mfa { ticket, methods }) if !offered(&methods).is_empty() => {
                app.ask_mfa(what, ticket, methods, request, done);
            }
            other => done(app, other),
        });
    }

    fn ask_mfa<T, F, Fut>(
        self: &Rc<Self>,
        what: &'static str,
        ticket: String,
        methods: Vec<String>,
        request: F,
        done: impl FnOnce(&Rc<AppState>, kc_http::Result<T>) + 'static,
    ) where
        T: Send + 'static,
        F: Fn(Option<String>) -> Fut + 'static,
        Fut: Future<Output = kc_http::Result<T>> + Send + 'static,
    {
        let choices = offered(&methods);
        let dialog = adw::AlertDialog::new(
            Some("Confirm It's You"),
            Some(&format!("Discord wants your two-factor code to {what}.")),
        );
        let fields = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let method = gtk::DropDown::from_strings(&choices.iter().map(|(_, name)| *name).collect::<Vec<_>>());
        method.set_visible(choices.len() > 1);
        fields.append(&method);
        let code = gtk::PasswordEntry::new();
        code.set_show_peek_icon(true);
        code.set_activates_default(true);
        let hint = {
            let choices = choices.clone();
            move |index: u32| match choices.get(index as usize).map(|(method, _)| *method) {
                Some("password") => "Your password",
                Some("backup") => "An 8-character backup code",
                _ => "The 6-digit code from your authenticator",
            }
        };
        code.set_placeholder_text(Some(hint(0)));
        {
            let code = code.clone();
            let hint = hint.clone();
            method.connect_selected_notify(move |method| code.set_placeholder_text(Some(hint(method.selected()))));
        }
        fields.append(&code);
        dialog.set_extra_child(Some(&fields));
        dialog.add_responses(&[("cancel", "Cancel"), ("confirm", "Confirm")]);
        dialog.set_response_appearance("confirm", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("confirm"));
        dialog.set_close_response("cancel");

        let done = Rc::new(std::cell::RefCell::new(Some(done)));
        let request = Rc::new(request);
        let app = self.clone();
        let focus = code.clone();
        dialog.connect_response(None, move |_, response| {
            let finish = done.borrow_mut().take();
            let Some(finish) = finish else {
                return;
            };
            if response != "confirm" {
                finish(&app, Err(Error::Mfa { ticket: ticket.clone(), methods: methods.clone() }));
                return;
            }
            let Some((_, http, _)) = app.active() else {
                return;
            };
            let kind = choices.get(method.selected() as usize).map_or("totp", |(method, _)| *method);
            let secret = code.text().trim().replace(' ', "");
            let ticket = ticket.clone();
            let request = request.clone();
            spawn_local(
                &app,
                async move { http.verify_mfa(&ticket, kind, &secret).await },
                move |app, verified| match verified {
                    Ok(token) => spawn_local(app, request(Some(token)), finish),
                    Err(error) => {
                        app.set_status(&format!("That code didn't work: {}", error.describe()));
                        finish(app, Err(error));
                    }
                },
            );
        });
        dialog.present(Some(&self.window));
        focus.grab_focus();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_methods_discord_accepts_are_offered() {
        let accepted = vec!["webauthn".to_owned(), "password".to_owned(), "totp".to_owned()];
        assert_eq!(offered(&accepted), vec![("totp", "Authenticator Code"), ("password", "Password")]);
        assert!(offered(&["webauthn".to_owned()]).is_empty());
    }
}
