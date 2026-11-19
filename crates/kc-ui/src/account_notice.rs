//! What to tell someone when Discord itself stops an account: it ended the
//! session and no longer takes the saved token, or it wants the account
//! verified before it does anything more.

use std::rc::Rc;

use libadwaita as adw;
use libadwaita::prelude::*;

use crate::session::AccountId;
use crate::shell::AppState;
use crate::widgets::open_uri;

const LOGIN_URL: &str = "https://discord.com/login";
const APP_URL: &str = "https://discord.com/app";

/// Why Discord signed an account out.
fn signed_out_text(name: &str) -> String {
    format!(
        "Discord ended this session and no longer accepts the saved sign-in for {name}. \
         This is not a ban. Discord does it when it wants to confirm an account is yours \
         (a failed captcha can set it off) and after a password change."
    )
}

/// How to get back in.
const SIGNED_OUT_STEPS: &str = "1. Open Discord in your browser and sign in.\n\
     2. If it says Verification Required, choose Re-verify by Email and follow the link \
     in the email.\n\
     3. Come back and sign in here with the new token. The old one no longer works.";

/// What Discord is asking for, from a gateway `required_action`.
fn required_action_text(action: &str) -> String {
    let action = action.to_ascii_uppercase();
    let how = match (action.contains("EMAIL"), action.contains("PHONE")) {
        (true, true) => "verify it by email or phone",
        (true, false) => "verify it by email",
        (false, true) => "verify it by phone",
        _ if action.contains("CAPTCHA") => "solve a captcha",
        _ if action.contains("AGREEMENT") || action.contains("TOS") => "accept its updated terms",
        _ => "confirm it is yours",
    };
    format!(
        "Discord wants you to {how} before this account can do anything more. Until \
         then, sending messages, adding friends and joining calls will fail.\n\n\
         Open Discord in your browser and follow what it asks. This is not a ban, and \
         kylecord carries on by itself once Discord is satisfied."
    )
}

/// Wider than an alert, which stays at 372 px whatever it is asked: these
/// dialogs say several sentences.
const WIDTH: i32 = 560;

/// A button along the bottom of a notice: its label, whether it is the
/// main one, and what it does (the dialog closes either way).
type NoticeButton = (&'static str, bool, Box<dyn Fn()>);

/// A notice dialog: a title, a centred message, optional left-aligned
/// steps, and a row of buttons.
fn notice_dialog(
    title: &str,
    message: &str,
    steps: Option<&str>,
    buttons: Vec<NoticeButton>,
) -> adw::Dialog {
    let dialog = adw::Dialog::new();
    dialog.set_title(title);
    dialog.set_content_width(WIDTH);

    let body = gtk::Box::new(gtk::Orientation::Vertical, 14);
    body.set_margin_top(28);
    body.set_margin_bottom(24);
    body.set_margin_start(28);
    body.set_margin_end(28);

    let heading = gtk::Label::new(Some(title));
    heading.add_css_class("title-2");
    heading.set_wrap(true);
    body.append(&heading);

    let text = gtk::Label::new(Some(message));
    text.set_wrap(true);
    text.set_justify(gtk::Justification::Center);
    body.append(&text);

    if let Some(steps) = steps {
        let label = gtk::Label::new(Some(steps));
        label.set_wrap(true);
        label.set_xalign(0.0);
        label.set_margin_top(4);
        body.append(&label);
    }

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    row.set_homogeneous(true);
    row.set_margin_top(10);
    let mut main = None;
    for (label, suggested, action) in buttons {
        let button = gtk::Button::with_label(label);
        button.add_css_class("pill");
        if suggested {
            button.add_css_class("suggested-action");
            main = Some(button.clone());
        }
        let target = dialog.clone();
        button.connect_clicked(move |_| {
            target.close();
            action();
        });
        row.append(&button);
    }
    body.append(&row);
    dialog.set_child(Some(&body));
    if let Some(main) = main {
        dialog.set_default_widget(Some(&main));
        dialog.set_focus(Some(&main));
    }
    dialog
}

impl AppState {
    /// Explains that Discord signed `account` out, with a way to the website
    /// and to signing in again.
    pub(crate) fn show_signed_out(self: &Rc<Self>, account: AccountId) {
        let name = {
            let model = self.model.borrow();
            model
                .sessions
                .get(&account)
                .map(|session| session.account.clone())
        }
        .map(|account| self.account_name(&account))
        .unwrap_or_else(|| "this account".to_owned());

        let (web, token) = (self.clone(), self.clone());
        let dialog = notice_dialog(
            "Discord Signed You Out",
            &signed_out_text(&name),
            Some(SIGNED_OUT_STEPS),
            vec![
                ("Not Now", false, Box::new(|| {})),
                (
                    "Enter New Token",
                    false,
                    Box::new(move || token.show_login()),
                ),
                (
                    "Open Discord",
                    true,
                    Box::new(move || {
                        open_uri(Some(web.window.upcast_ref()), LOGIN_URL);
                        // The new token is wanted next.
                        web.show_login();
                    }),
                ),
            ],
        );
        dialog.present(Some(&self.window));
    }

    /// Explains that Discord wants the account verified (it still accepts
    /// the token), with a way to the website.
    pub(crate) fn show_required_action(self: &Rc<Self>, action: &str) {
        let app = self.clone();
        let dialog = notice_dialog(
            "Verification Required",
            &required_action_text(action),
            None,
            vec![
                ("Not Now", false, Box::new(|| {})),
                (
                    "Open Discord",
                    true,
                    Box::new(move || open_uri(Some(app.window.upcast_ref()), APP_URL)),
                ),
            ],
        );
        dialog.present(Some(&self.window));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_required_action_says_what_discord_wants() {
        let text = |action| required_action_text(action);
        assert!(text("REQUIRE_REVERIFIED_EMAIL").contains("verify it by email before"));
        assert!(text("REQUIRE_VERIFIED_PHONE").contains("verify it by phone before"));
        assert!(text("REQUIRE_VERIFIED_EMAIL_OR_REVERIFIED_PHONE").contains("by email or phone"));
        assert!(text("REQUIRE_CAPTCHA").contains("solve a captcha"));
        assert!(text("TOS_UPDATE_ACKNOWLEDGMENT").contains("updated terms"));
        assert!(text("SOMETHING_NEW").contains("confirm it is yours"));
    }

    #[test]
    fn being_signed_out_is_explained_with_the_account_named() {
        let text = signed_out_text("someone");
        assert!(text.contains("sign-in for someone"));
        assert!(text.contains("not a ban"));
        assert!(SIGNED_OUT_STEPS.contains("Re-verify by Email"));
    }
}
