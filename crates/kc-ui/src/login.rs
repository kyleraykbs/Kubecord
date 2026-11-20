//! The login page (also used to add another account): a token, or an email
//! (or phone number) and password, followed by a two-factor code when the
//! account has one.
//!
//! The steps are a small state machine ([`advance`]) from Discord's answers
//! ([`kc_http::auth::AuthResponse`]) to the next screen and what to say on
//! it, kept free of widgets so every branch is tested. Either way in, only
//! the token is kept: it goes through the same [`Command::Login`] check and
//! is saved exactly as a pasted one. The password and codes are wiped once
//! sent ([`Password`]) and their fields cleared as soon as they are read.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use gtk::prelude::*;
use kc_core::Token;
use kc_http::auth::{AuthClient, AuthResponse, MfaChallenge, MfaMethod, Password, Rejection};
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::session::Command;
use crate::shell::AppState;

// -- the state machine -------------------------------------------------------

/// Where the email-and-password sign-in is.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Step {
    /// Asking for the email (or phone number) and password.
    Credentials,
    /// Asking for a two-factor code. `sms` is the masked phone number once a
    /// code was texted: codes then go to the SMS check.
    Code {
        challenge: MfaChallenge,
        sms: Option<String>,
    },
}

/// What to show beside each field, and under the form.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Feedback {
    pub login: Option<String>,
    pub password: Option<String>,
    pub code: Option<String>,
    pub general: Option<String>,
}

/// After an answer from Discord: signed in, or the step to show.
#[derive(Debug, PartialEq)]
pub(crate) enum Next {
    SignedIn(Token),
    Show(Step, Feedback),
}

/// A request that got no usable answer.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Failure {
    /// Discord wanted a captcha and it was not solved here.
    Captcha,
    /// The request itself failed (no connection, ...).
    Network(String),
}

impl Failure {
    pub(crate) fn of(error: &kc_http::Error) -> Self {
        match error {
            kc_http::Error::Captcha { .. } => Self::Captcha,
            // reqwest's message names the URL, never the body.
            other => Self::Network(other.to_string()),
        }
    }
}

fn general(text: impl Into<String>) -> Feedback {
    Feedback {
        general: Some(text.into()),
        ..Feedback::default()
    }
}

pub(crate) const CAPTCHA_UNSOLVED: &str = "Discord wants a captcha solved before it signs you in. \
     Turn on “Solve Captchas in kylecord” (Settings → Advanced) and try again, or log in with a \
     token instead.";

/// The next step after `step` got `result`.
pub(crate) fn advance(step: &Step, result: Result<AuthResponse, Failure>) -> Next {
    let stay = |feedback: Feedback| Next::Show(step.clone(), feedback);
    let response = match result {
        Ok(response) => response,
        Err(Failure::Captcha) => return stay(general(CAPTCHA_UNSOLVED)),
        Err(Failure::Network(error)) => {
            return stay(general(format!("Could not reach Discord: {error}")));
        }
    };
    match response {
        AuthResponse::Token(token) => Next::SignedIn(token),
        AuthResponse::Mfa(challenge) => {
            if !(challenge.totp || challenge.backup || challenge.sms) {
                return Next::Show(
                    Step::Credentials,
                    general(
                        "This account signs in with a security key, which kylecord cannot use. \
                         Log in with a token instead.",
                    ),
                );
            }
            Next::Show(
                Step::Code {
                    challenge,
                    sms: None,
                },
                Feedback::default(),
            )
        }
        // Captchas are answered before this; one that gets here was not.
        AuthResponse::Captcha { .. } => stay(general(CAPTCHA_UNSOLVED)),
        AuthResponse::Rejected(rejection) => match rejection {
            Rejection::Fields {
                login,
                password,
                code,
            } => match step {
                Step::Credentials => stay(Feedback {
                    login,
                    password,
                    general: code,
                    ..Feedback::default()
                }),
                Step::Code { .. } => stay(Feedback {
                    code: code.or(password).or(login),
                    ..Feedback::default()
                }),
            },
            Rejection::NewLocation => Next::Show(
                Step::Credentials,
                Feedback {
                    login: Some(
                        "Discord saw a login from a new location and emailed you. Approve it \
                         from that email, then log in again."
                            .into(),
                    ),
                    ..Feedback::default()
                },
            ),
            Rejection::Disabled(text) => Next::Show(Step::Credentials, general(text)),
            Rejection::RateLimited { retry_after } => stay(general(format!(
                "Too many attempts. Wait {} and try again.",
                wait_text(retry_after)
            ))),
            Rejection::TicketExpired => Next::Show(
                Step::Credentials,
                general(
                    "The two-factor step took too long and Discord's sign-in ticket expired. \
                     Enter your password again.",
                ),
            ),
            Rejection::InvalidCode => stay(Feedback {
                code: Some("That code is not valid. Check it and try again.".into()),
                ..Feedback::default()
            }),
            Rejection::Other(text) => stay(general(text)),
        },
    }
}

fn wait_text(seconds: f64) -> String {
    let seconds = seconds.ceil().max(1.0) as u64;
    match seconds {
        1 => "a second".to_owned(),
        2..=90 => format!("{seconds} seconds"),
        _ => format!("{} minutes", seconds.div_ceil(60)),
    }
}

/// Checks the email and password before anything is sent.
pub(crate) fn check_credentials(login: &str, password_empty: bool) -> Option<Feedback> {
    let feedback = Feedback {
        login: login
            .trim()
            .is_empty()
            .then(|| "Enter your email or phone number.".to_owned()),
        password: password_empty.then(|| "Enter your password.".to_owned()),
        ..Feedback::default()
    };
    (feedback != Feedback::default()).then_some(feedback)
}

/// Which check a typed code goes to, and the code as Discord wants it:
/// 6 digits for the app (or a text message, once one was sent), or an
/// 8-character backup code (dashes and spaces dropped), as the web client's
/// single code field accepts either.
pub(crate) fn code_method(
    challenge: &MfaChallenge,
    sms_sent: bool,
    text: &str,
) -> Result<(MfaMethod, Password), String> {
    let code: String = text
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    let digits = code.len() == 6 && code.chars().all(|c| c.is_ascii_digit());
    if sms_sent && digits {
        return Ok((MfaMethod::Sms, Password::new(code)));
    }
    if challenge.totp && digits {
        return Ok((MfaMethod::Totp, Password::new(code)));
    }
    if challenge.backup && code.len() == 8 && code.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Ok((MfaMethod::Backup, Password::new(code.to_ascii_lowercase())));
    }
    Err(code_prompt(challenge, sms_sent).1.to_owned())
}

/// The code step's explanation, and what to ask for when a code does not
/// fit.
pub(crate) fn code_prompt(challenge: &MfaChallenge, sms_sent: bool) -> (String, &'static str) {
    if sms_sent {
        return (
            "Enter the 6-digit code Discord texted you.".to_owned(),
            "Enter the 6-digit code from the text message.",
        );
    }
    match (challenge.totp, challenge.backup) {
        (true, true) => (
            "Enter the 6-digit code from your authenticator app, or one of your 8-character \
             backup codes."
                .to_owned(),
            "Enter a 6-digit code or an 8-character backup code.",
        ),
        (true, false) => (
            "Enter the 6-digit code from your authenticator app.".to_owned(),
            "Enter the 6-digit code from your authenticator app.",
        ),
        (false, true) => (
            "Enter one of your 8-character backup codes.".to_owned(),
            "Enter an 8-character backup code.",
        ),
        (false, false) => (
            "Send a code by text message to continue.".to_owned(),
            "Send a code by text message first.",
        ),
    }
}

// -- the page ---------------------------------------------------------------

/// The login page's widgets and the sign-in in progress.
pub(crate) struct LoginPage {
    pub page: adw::ToolbarView,
    pub mode: adw::ToggleGroup,
    pub forms: gtk::Stack,
    pub note: gtk::Label,
    pub login_row: adw::EntryRow,
    pub login_error: gtk::Label,
    pub password_row: adw::PasswordEntryRow,
    pub password_error: gtk::Label,
    pub code_hint: gtk::Label,
    pub code_row: adw::EntryRow,
    pub code_error: gtk::Label,
    pub sms_button: gtk::Button,
    pub back_button: gtk::Button,
    pub token_row: adw::PasswordEntryRow,
    pub label_list: gtk::ListBox,
    pub label_row: adw::EntryRow,
    pub submit: gtk::Button,
    pub submit_label: gtk::Label,
    pub spinner: adw::Spinner,
    pub cancel: gtk::Button,
    pub error: gtk::Label,
    pub step: RefCell<Step>,
    /// The logged-out client of the sign-in in progress (its fingerprint
    /// is kept for the code step).
    pub client: RefCell<Option<AuthClient>>,
    pub busy: Cell<bool>,
    /// Bumped whenever the page is reset (Cancel, a new attempt), so a
    /// request still in flight from a cancelled attempt is dropped instead of
    /// signing the user in after they backed out.
    pub generation: Cell<u64>,
}

fn field_error() -> gtk::Label {
    let label = gtk::Label::new(None);
    label.add_css_class("error");
    label.add_css_class("caption");
    label.set_wrap(true);
    label.set_xalign(0.0);
    label.set_margin_start(6);
    label.set_visible(false);
    label
}

fn boxed(rows: &[&gtk::Widget]) -> gtk::ListBox {
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    for row in rows {
        list.append(*row);
    }
    list
}

impl LoginPage {
    pub(crate) fn new() -> Self {
        let clamp = adw::Clamp::new();
        clamp.set_maximum_size(440);
        clamp.set_valign(gtk::Align::Center);
        let form = gtk::Box::new(gtk::Orientation::Vertical, 12);
        form.set_margin_start(24);
        form.set_margin_end(24);
        form.set_margin_top(24);
        form.set_margin_bottom(24);

        let title = gtk::Label::new(Some("kylecord"));
        title.add_css_class("title-1");
        let subtitle = gtk::Label::new(Some("Sign in to Discord"));
        subtitle.add_css_class("dim-label");

        let mode = adw::ToggleGroup::new();
        mode.set_homogeneous(true);
        mode.set_can_shrink(false);
        mode.set_margin_top(6);
        mode.add(
            adw::Toggle::builder()
                .name("password")
                .label("Email & Password")
                .build(),
        );
        mode.add(adw::Toggle::builder().name("token").label("Token").build());
        mode.set_active_name(Some("password"));

        // Email and password.
        let login_row = adw::EntryRow::new();
        login_row.set_title("Email or Phone Number");
        login_row.set_input_purpose(gtk::InputPurpose::Email);
        let login_error = field_error();
        let password_row = adw::PasswordEntryRow::new();
        password_row.set_title("Password");
        let password_error = field_error();
        let password_form = gtk::Box::new(gtk::Orientation::Vertical, 6);
        password_form.append(&boxed(&[login_row.upcast_ref()]));
        password_form.append(&login_error);
        password_form.append(&boxed(&[password_row.upcast_ref()]));
        password_form.append(&password_error);

        // Two-factor code.
        let code_title = gtk::Label::new(Some("Two-Factor Authentication"));
        code_title.add_css_class("title-4");
        code_title.set_xalign(0.0);
        let code_hint = gtk::Label::new(None);
        code_hint.set_wrap(true);
        code_hint.set_xalign(0.0);
        code_hint.add_css_class("dim-label");
        let code_row = adw::EntryRow::new();
        code_row.set_title("Code");
        code_row.set_input_purpose(gtk::InputPurpose::FreeForm);
        let code_error = field_error();
        let sms_button = gtk::Button::with_label("Text Me a Code Instead");
        sms_button.add_css_class("flat");
        sms_button.set_halign(gtk::Align::Start);
        let back_button = gtk::Button::from_icon_name("go-previous-symbolic");
        back_button.set_tooltip_text(Some("Back"));
        back_button.add_css_class("flat");
        let code_head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        code_head.append(&back_button);
        code_head.append(&code_title);
        let code_form = gtk::Box::new(gtk::Orientation::Vertical, 6);
        code_form.append(&code_head);
        code_form.append(&code_hint);
        code_form.append(&boxed(&[code_row.upcast_ref()]));
        code_form.append(&code_error);
        code_form.append(&sms_button);

        // Token.
        let token_row = adw::PasswordEntryRow::new();
        token_row.set_title("User Token");
        let token_form = gtk::Box::new(gtk::Orientation::Vertical, 6);
        token_form.append(&boxed(&[token_row.upcast_ref()]));

        let forms = gtk::Stack::new();
        forms.set_vhomogeneous(false);
        forms.set_transition_type(gtk::StackTransitionType::Crossfade);
        forms.add_named(&password_form, Some("password"));
        forms.add_named(&code_form, Some("code"));
        forms.add_named(&token_form, Some("token"));

        let label_row = adw::EntryRow::new();
        label_row.set_title("Label (optional)");
        let label_list = boxed(&[label_row.upcast_ref()]);

        let spinner = adw::Spinner::new();
        spinner.set_visible(false);
        let submit_label = gtk::Label::new(Some("Log In"));
        let submit_content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        submit_content.set_halign(gtk::Align::Center);
        submit_content.append(&spinner);
        submit_content.append(&submit_label);
        let submit = gtk::Button::new();
        submit.set_child(Some(&submit_content));
        submit.add_css_class("suggested-action");
        submit.add_css_class("pill");
        submit.set_halign(gtk::Align::Center);
        submit.set_margin_top(6);
        submit.set_width_request(160);

        let cancel = gtk::Button::with_label("Cancel");
        cancel.add_css_class("flat");
        cancel.set_halign(gtk::Align::Center);
        cancel.set_visible(false);

        let error = gtk::Label::new(None);
        error.add_css_class("error");
        error.set_wrap(true);
        error.set_justify(gtk::Justification::Center);
        error.set_visible(false);

        let note = gtk::Label::new(None);
        note.add_css_class("dim-label");
        note.add_css_class("caption");
        note.set_wrap(true);
        note.set_justify(gtk::Justification::Center);
        note.set_margin_top(6);

        form.append(&title);
        form.append(&subtitle);
        form.append(&mode);
        form.append(&forms);
        form.append(&label_list);
        form.append(&error);
        form.append(&submit);
        form.append(&cancel);
        form.append(&note);
        clamp.set_child(Some(&form));
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_hscrollbar_policy(gtk::PolicyType::Never);
        scroll.set_child(Some(&clamp));

        let page = adw::ToolbarView::new();
        page.add_top_bar(&adw::HeaderBar::new());
        page.set_content(Some(&scroll));

        let login = Self {
            page,
            mode,
            forms,
            note,
            login_row,
            login_error,
            password_row,
            password_error,
            code_hint,
            code_row,
            code_error,
            sms_button,
            back_button,
            token_row,
            label_list,
            label_row,
            submit,
            submit_label,
            spinner,
            cancel,
            error,
            step: RefCell::new(Step::Credentials),
            client: RefCell::new(None),
            busy: Cell::new(false),
            generation: Cell::new(0),
        };
        login.render();
        login
    }

    fn token_mode(&self) -> bool {
        self.mode.active_name().as_deref() == Some("token")
    }

    /// Shows the current mode and step (not the feedback).
    pub(crate) fn render(&self) {
        let step = self.step.borrow().clone();
        let token = self.token_mode();
        let code = matches!(step, Step::Code { .. }) && !token;
        self.mode.set_visible(!code);
        self.label_list.set_visible(!code);
        self.forms.set_visible_child_name(if token {
            "token"
        } else if code {
            "code"
        } else {
            "password"
        });
        self.submit_label
            .set_label(if code { "Continue" } else { "Log In" });
        if let Step::Code { challenge, sms } = &step {
            self.code_hint
                .set_label(&code_prompt(challenge, sms.is_some()).0);
            self.sms_button.set_visible(challenge.sms);
            self.sms_button.set_label(if sms.is_some() {
                "Text Me Another Code"
            } else {
                "Text Me a Code Instead"
            });
        }
        self.note.set_label(if token {
            "Paste a Discord user token. It is saved on this computer so the account stays \
             signed in."
        } else {
            "Discord may ask for a captcha, or email you to confirm a login from a new place, \
             when you sign in from a third-party client like this one. Your password is sent \
             only to Discord and is never saved: kylecord keeps the resulting token, as when \
             you paste one."
        });
    }

    /// Shows messages beside the fields (and clears the others).
    pub(crate) fn show_feedback(&self, feedback: &Feedback) {
        let set = |label: &gtk::Label, row: Option<&gtk::Widget>, text: &Option<String>| {
            label.set_label(text.as_deref().unwrap_or(""));
            label.set_visible(text.is_some());
            if let Some(row) = row {
                if text.is_some() {
                    row.add_css_class("error");
                } else {
                    row.remove_css_class("error");
                }
            }
        };
        set(
            &self.login_error,
            Some(self.login_row.upcast_ref()),
            &feedback.login,
        );
        set(
            &self.password_error,
            Some(self.password_row.upcast_ref()),
            &feedback.password,
        );
        set(
            &self.code_error,
            Some(self.code_row.upcast_ref()),
            &feedback.code,
        );
        set(&self.error, None, &feedback.general);
    }

    /// Greys the form out while a request is out.
    pub(crate) fn set_busy(&self, busy: bool) {
        self.busy.set(busy);
        self.submit.set_sensitive(!busy);
        self.spinner.set_visible(busy);
        self.forms.set_sensitive(!busy);
        self.mode.set_sensitive(!busy);
        self.label_list.set_sensitive(!busy);
    }

    /// Back to an empty first step: after signing in, or on Cancel.
    pub(crate) fn reset(&self) {
        // Any request in flight from the attempt being reset is now stale.
        self.generation.set(self.generation.get().wrapping_add(1));
        *self.step.borrow_mut() = Step::Credentials;
        self.client.replace(None);
        for row in [
            self.login_row.upcast_ref::<gtk::Editable>(),
            self.password_row.upcast_ref(),
            self.code_row.upcast_ref(),
            self.token_row.upcast_ref(),
            self.label_row.upcast_ref(),
        ] {
            row.set_text("");
        }
        self.set_busy(false);
        self.show_feedback(&Feedback::default());
        self.render();
    }

    fn label(&self) -> Option<String> {
        let text = self.label_row.text();
        let text = text.trim();
        (!text.is_empty()).then(|| text.to_owned())
    }
}

// -- behaviour ----------------------------------------------------------------

impl AppState {
    pub(crate) fn connect_login(self: &Rc<Self>) {
        let login = &self.login;
        let app = self.clone();
        login.submit.connect_clicked(move |_| app.submit_login());
        for row in [
            login.login_row.upcast_ref::<adw::EntryRow>(),
            login.password_row.upcast_ref(),
            login.code_row.upcast_ref(),
            login.token_row.upcast_ref(),
            login.label_row.upcast_ref(),
        ] {
            let app = self.clone();
            row.connect_entry_activated(move |_| app.submit_login());
        }
        let app = self.clone();
        login.mode.connect_active_name_notify(move |_| {
            app.login.show_feedback(&Feedback::default());
            app.login.render();
            app.focus_login();
        });
        let app = self.clone();
        login.back_button.connect_clicked(move |_| {
            *app.login.step.borrow_mut() = Step::Credentials;
            app.login.client.replace(None);
            app.login.code_row.set_text("");
            app.login.show_feedback(&Feedback::default());
            app.login.render();
            app.focus_login();
        });
        let app = self.clone();
        login
            .sms_button
            .connect_clicked(move |_| app.send_login_sms());
        let app = self.clone();
        login.cancel.connect_clicked(move |_| {
            app.login.reset();
            app.stack.set_visible_child_name("chat");
        });
    }

    fn focus_login(&self) {
        let login = &self.login;
        if login.token_mode() {
            login.token_row.grab_focus();
        } else if matches!(*login.step.borrow(), Step::Code { .. }) {
            login.code_row.grab_focus();
        } else if login.login_row.text().is_empty() {
            login.login_row.grab_focus();
        } else {
            login.password_row.grab_focus();
        }
    }

    /// Opens the login page; `token` picks the token form (to replace a
    /// token Discord stopped taking), otherwise the last form used.
    pub(crate) fn show_login_with(self: &Rc<Self>, token: bool) {
        if !self.login.busy.get() {
            self.login.show_feedback(&Feedback::default());
        }
        if token {
            self.login.mode.set_active_name(Some("token"));
        }
        self.login.render();
        self.stack.set_visible_child_name("login");
        // Going back is only possible when an account is already signed in.
        let has_accounts = !self.model.borrow().accounts.is_empty();
        self.login.cancel.set_visible(has_accounts);
        self.focus_login();
    }

    pub(crate) fn show_login(self: &Rc<Self>) {
        self.show_login_with(false);
    }

    pub(crate) fn submit_login(self: &Rc<Self>) {
        let login = &self.login;
        if login.busy.get() {
            return;
        }
        if login.token_mode() {
            let token = login.token_row.text();
            let token = token.trim();
            if token.is_empty() {
                login.show_feedback(&general("Enter a token."));
                return;
            }
            login.show_feedback(&Feedback::default());
            login.set_busy(true);
            let _ = self.cmd_tx.try_send(Command::Login {
                token: Token::new(token),
                label: login.label(),
            });
            return;
        }
        let step = login.step.borrow().clone();
        match step {
            Step::Credentials => {
                let name = login.login_row.text().trim().to_owned();
                let password = Password::new(login.password_row.text().as_str());
                if let Some(feedback) = check_credentials(&name, password.is_empty()) {
                    login.show_feedback(&feedback);
                    return;
                }
                // The field is emptied once read; a failed attempt asks again.
                login.password_row.set_text("");
                let password = Arc::new(password);
                let client = auth_client();
                login.client.replace(Some(client.clone()));
                login.show_feedback(&Feedback::default());
                login.set_busy(true);
                let generation = login.generation.get();
                tracing::info!("signing in with a password");
                self.with_captcha(
                    "Discord wants a captcha before it signs you in.",
                    move |captcha| {
                        let (client, name, password) =
                            (client.clone(), name.clone(), password.clone());
                        async move { client.login(&name, &password, captcha.as_ref()).await }
                    },
                    move |app, result| app.login_answered_if_current(generation, result),
                );
            }
            Step::Code { challenge, sms } => {
                let text = login.code_row.text();
                let (method, code) = match code_method(&challenge, sms.is_some(), &text) {
                    Ok(found) => found,
                    Err(problem) => {
                        login.show_feedback(&Feedback {
                            code: Some(problem),
                            ..Feedback::default()
                        });
                        return;
                    }
                };
                login.code_row.set_text("");
                let Some(client) = login.client.borrow().clone() else {
                    return;
                };
                let code = Arc::new(code);
                let ticket = challenge.ticket.clone();
                login.show_feedback(&Feedback::default());
                login.set_busy(true);
                let generation = login.generation.get();
                tracing::info!(?method, "sending a two-factor code");
                self.with_captcha(
                    "Discord wants a captcha before it signs you in.",
                    move |captcha| {
                        let (client, code, ticket) = (client.clone(), code.clone(), ticket.clone());
                        async move {
                            client
                                .submit_code(method, &code, &ticket, captcha.as_ref())
                                .await
                        }
                    },
                    move |app, result| app.login_answered_if_current(generation, result),
                );
            }
        }
    }

    fn send_login_sms(self: &Rc<Self>) {
        let login = &self.login;
        let Step::Code { challenge, .. } = login.step.borrow().clone() else {
            return;
        };
        let Some(client) = login.client.borrow().clone() else {
            return;
        };
        login.show_feedback(&Feedback::default());
        login.set_busy(true);
        let generation = login.generation.get();
        let ticket = challenge.ticket.clone();
        self.with_captcha(
            "Discord wants a captcha before it texts you a code.",
            move |captcha| {
                let (client, ticket) = (client.clone(), ticket.clone());
                async move { client.send_sms(&ticket, captcha.as_ref()).await }
            },
            move |app, result| {
                // The user cancelled or started over while this was in flight.
                if app.login.generation.get() != generation {
                    return;
                }
                app.login.set_busy(false);
                match result {
                    Ok(Ok(phone)) => {
                        *app.login.step.borrow_mut() = Step::Code {
                            challenge,
                            sms: Some(phone.unwrap_or_default()),
                        };
                        app.login.render();
                        app.focus_login();
                    }
                    Ok(Err(rejection)) => {
                        app.login_answered(Ok(AuthResponse::Rejected(rejection)));
                    }
                    Err(error) => app.login_answered(Err(error)),
                }
            },
        );
    }

    /// Calls [`Self::login_answered`] only if the attempt that produced
    /// `result` has not been cancelled or superseded (its generation still
    /// matches). A stale result is dropped so a cancelled attempt cannot sign
    /// the user in.
    fn login_answered_if_current(
        self: &Rc<Self>,
        generation: u64,
        result: kc_http::Result<AuthResponse>,
    ) {
        if self.login.generation.get() == generation {
            self.login_answered(result);
        }
    }

    /// Discord answered the password or a code (after any captcha).
    pub(crate) fn login_answered(self: &Rc<Self>, result: kc_http::Result<AuthResponse>) {
        let step = self.login.step.borrow().clone();
        let next = advance(&step, result.map_err(|error| Failure::of(&error)));
        match next {
            Next::SignedIn(token) => {
                tracing::info!("password sign-in gave a token");
                // Checked and saved as a pasted token is; the page stays
                // busy until that answers (LoginSuccess / LoginFailed).
                *self.login.step.borrow_mut() = Step::Credentials;
                self.login.client.replace(None);
                let _ = self.cmd_tx.try_send(Command::Login {
                    token,
                    label: self.login.label(),
                });
            }
            Next::Show(next_step, feedback) => {
                tracing::info!(
                    step = if matches!(next_step, Step::Code { .. }) {
                        "code"
                    } else {
                        "password"
                    },
                    "sign-in continues"
                );
                if next_step == Step::Credentials {
                    self.login.client.replace(None);
                }
                *self.login.step.borrow_mut() = next_step;
                self.login.set_busy(false);
                self.login.render();
                self.login.show_feedback(&feedback);
                self.focus_login();
            }
        }
    }

    /// The token (pasted or from the password sign-in) was refused.
    pub(crate) fn login_failed(&self, message: &str) {
        self.login.set_busy(false);
        self.login.render();
        self.login.show_feedback(&general(message));
    }
}

/// The client for a new sign-in. Demo runs (`KYLECORD_DEMO`) get one that
/// points at an address nothing listens on, so nothing reaches Discord.
fn auth_client() -> AuthClient {
    if std::env::var_os("KYLECORD_DEMO").is_some() {
        AuthClient::with_base("https://127.0.0.1:9/api/v9")
    } else {
        AuthClient::new()
    }
}

impl AppState {
    /// `KYLECORD_DEMO=login-<what>`: the login page in one of its states,
    /// offline. `password` / `token` / `add` (with Cancel, as when adding
    /// an account) / `busy` / `errors` (invalid login) / `location` (new
    /// login location) / `code` / `code-sms` / `code-wrong` / `expired`
    /// (ticket expired, back at the password) / `submit` (a real submit
    /// against nowhere: the request fails).
    pub(crate) fn show_login_demo(self: &Rc<Self>, what: &str) {
        let login = &self.login;
        login.reset();
        login
            .mode
            .set_active_name(Some(if what == "token" { "token" } else { "password" }));
        self.show_login();
        let code = || Step::Code {
            challenge: MfaChallenge {
                ticket: "demo".into(),
                totp: true,
                sms: true,
                backup: true,
                webauthn: false,
            },
            sms: None,
        };
        let answer = |step: Step, response: Result<AuthResponse, Failure>| {
            *login.step.borrow_mut() = step.clone();
            if let Next::Show(step, feedback) = advance(&step, response) {
                *login.step.borrow_mut() = step;
                login.render();
                login.show_feedback(&feedback);
            }
        };
        login.login_row.set_text("someone@example.com");
        match what {
            "add" => login.cancel.set_visible(true),
            "busy" => {
                login.password_row.set_text("password");
                login.set_busy(true);
            }
            "errors" => answer(
                Step::Credentials,
                Ok(AuthResponse::Rejected(Rejection::Fields {
                    login: Some("Login or password is invalid.".into()),
                    password: Some("Login or password is invalid.".into()),
                    code: None,
                })),
            ),
            "location" => answer(
                Step::Credentials,
                Ok(AuthResponse::Rejected(Rejection::NewLocation)),
            ),
            "code" => answer(
                Step::Credentials,
                Ok(AuthResponse::Mfa(match code() {
                    Step::Code { challenge, .. } => challenge,
                    Step::Credentials => unreachable!(),
                })),
            ),
            "code-sms" => {
                if let Step::Code { challenge, .. } = code() {
                    *login.step.borrow_mut() = Step::Code {
                        challenge,
                        sms: Some("+*******1234".into()),
                    };
                }
                login.render();
            }
            "code-wrong" => answer(code(), Ok(AuthResponse::Rejected(Rejection::InvalidCode))),
            "expired" => answer(code(), Ok(AuthResponse::Rejected(Rejection::TicketExpired))),
            "submit" => {
                login.password_row.set_text("not a real password");
                self.submit_login();
            }
            _ => {}
        }
        if what == "token" {
            login.token_row.set_text("not-a-real-token");
        }
        self.focus_login();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn challenge(totp: bool, sms: bool, backup: bool) -> MfaChallenge {
        MfaChallenge {
            ticket: "ticket".into(),
            totp,
            sms,
            backup,
            webauthn: false,
        }
    }

    fn code_step() -> Step {
        Step::Code {
            challenge: challenge(true, true, true),
            sms: None,
        }
    }

    fn rejected(rejection: Rejection) -> Result<AuthResponse, Failure> {
        Ok(AuthResponse::Rejected(rejection))
    }

    #[test]
    fn a_token_signs_in_from_either_step() {
        for step in [Step::Credentials, code_step()] {
            assert_eq!(
                advance(&step, Ok(AuthResponse::Token(Token::new("t")))),
                Next::SignedIn(Token::new("t"))
            );
        }
    }

    #[test]
    fn two_factor_moves_to_the_code_step() {
        let next = advance(
            &Step::Credentials,
            Ok(AuthResponse::Mfa(challenge(true, false, true))),
        );
        assert_eq!(
            next,
            Next::Show(
                Step::Code {
                    challenge: challenge(true, false, true),
                    sms: None
                },
                Feedback::default()
            )
        );
        // Only a security key: nothing kylecord can do.
        let mut key_only = challenge(false, false, false);
        key_only.webauthn = true;
        let Next::Show(Step::Credentials, feedback) =
            advance(&Step::Credentials, Ok(AuthResponse::Mfa(key_only)))
        else {
            panic!()
        };
        assert!(feedback.general.unwrap().contains("security key"));
    }

    #[test]
    fn field_errors_land_on_their_fields() {
        let next = advance(
            &Step::Credentials,
            rejected(Rejection::Fields {
                login: Some("Login or password is invalid.".into()),
                password: Some("Login or password is invalid.".into()),
                code: None,
            }),
        );
        assert_eq!(
            next,
            Next::Show(
                Step::Credentials,
                Feedback {
                    login: Some("Login or password is invalid.".into()),
                    password: Some("Login or password is invalid.".into()),
                    ..Feedback::default()
                }
            )
        );
        let Next::Show(step, feedback) = advance(
            &code_step(),
            rejected(Rejection::Fields {
                login: None,
                password: None,
                code: Some("Must be 8 or fewer in length.".into()),
            }),
        ) else {
            panic!()
        };
        assert_eq!(step, code_step());
        assert_eq!(
            feedback.code.as_deref(),
            Some("Must be 8 or fewer in length.")
        );
    }

    #[test]
    fn a_new_location_points_at_the_email() {
        let Next::Show(Step::Credentials, feedback) =
            advance(&Step::Credentials, rejected(Rejection::NewLocation))
        else {
            panic!()
        };
        assert!(feedback.login.unwrap().contains("emailed you"));
        assert_eq!(feedback.general, None);
    }

    #[test]
    fn disabled_and_rate_limited() {
        assert_eq!(
            advance(
                &Step::Credentials,
                rejected(Rejection::Disabled(
                    "This account has been disabled.".into()
                ))
            ),
            Next::Show(
                Step::Credentials,
                general("This account has been disabled.")
            )
        );
        assert_eq!(
            advance(
                &code_step(),
                rejected(Rejection::RateLimited { retry_after: 12.2 })
            ),
            Next::Show(
                code_step(),
                general("Too many attempts. Wait 13 seconds and try again.")
            )
        );
        assert_eq!(wait_text(600.0), "10 minutes");
        assert_eq!(wait_text(0.2), "a second");
    }

    #[test]
    fn an_expired_ticket_goes_back_to_the_password() {
        let Next::Show(Step::Credentials, feedback) =
            advance(&code_step(), rejected(Rejection::TicketExpired))
        else {
            panic!()
        };
        assert!(feedback.general.unwrap().contains("expired"));
    }

    #[test]
    fn a_wrong_code_stays_on_the_code_step() {
        let Next::Show(step, feedback) = advance(&code_step(), rejected(Rejection::InvalidCode))
        else {
            panic!()
        };
        assert_eq!(step, code_step());
        assert!(feedback.code.is_some());
    }

    #[test]
    fn failures_keep_the_step() {
        assert_eq!(
            advance(&code_step(), Err(Failure::Captcha)),
            Next::Show(code_step(), general(CAPTCHA_UNSOLVED))
        );
        let Next::Show(Step::Credentials, feedback) = advance(
            &Step::Credentials,
            Err(Failure::Network("connection refused".into())),
        ) else {
            panic!()
        };
        assert!(feedback.general.unwrap().contains("connection refused"));
        assert_eq!(
            Failure::of(&kc_http::Error::Captcha {
                service: "hcaptcha".into(),
                sitekey: "k".into(),
                rqdata: None,
                rqtoken: None
            }),
            Failure::Captcha
        );
    }

    #[test]
    fn credentials_are_checked_first() {
        assert_eq!(check_credentials("me@example.com", false), None);
        let feedback = check_credentials("  ", true).unwrap();
        assert!(feedback.login.is_some() && feedback.password.is_some());
    }

    #[test]
    fn codes_pick_their_check() {
        let both = challenge(true, true, true);
        let (method, _) = code_method(&both, false, "123 456").unwrap();
        assert_eq!(method, MfaMethod::Totp);
        let (method, code) = code_method(&both, false, "ABCD-EFGH").unwrap();
        assert_eq!(method, MfaMethod::Backup);
        assert!(!format!("{code:?}").contains("abcdefgh"));
        let (method, _) = code_method(&both, true, "123456").unwrap();
        assert_eq!(method, MfaMethod::Sms);
        assert!(code_method(&both, false, "12345").is_err());
        assert!(code_method(&challenge(false, false, true), false, "123456").is_err());
        assert!(code_method(&challenge(true, false, false), false, "abcdefgh").is_err());
    }
}
