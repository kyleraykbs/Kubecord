//! Discord's captcha challenges. Some requests, friend requests above all,
//! come back asking for an hCaptcha. With the `captcha` feature it is shown
//! here, in a WebKit view whose page counts as discord.com (the only site
//! Discord's captcha runs on), and the request is sent again with the
//! answer. Otherwise, or when the captcha is closed, the caller offers the
//! browser instead.

use std::future::Future;
use std::rc::Rc;

use kc_http::{CaptchaAnswer, Error};

use crate::shell::AppState;
use crate::video_setup::spawn_local;

/// hCaptcha's public test key: it runs on any site and always passes, so
/// the view can be checked without a real challenge.
pub(crate) const TEST_SITEKEY: &str = "10000000-ffff-ffff-ffff-000000000001";

/// Whether this build can show captchas.
pub(crate) const AVAILABLE: bool = cfg!(feature = "captcha");

/// Gets the captcha's answer, or `None` when it was closed unsolved.
pub(crate) type Solved = Box<dyn FnOnce(&Rc<AppState>, Option<String>)>;

impl AppState {
    /// Runs a request Discord may answer with a captcha. With solving in
    /// kubecord on, the captcha is shown (`prompt` says what it is for) and
    /// the request sent again with the answer. `done` gets the final result:
    /// the challenge itself when the captcha is off or was closed.
    pub(crate) fn with_captcha<T, F, Fut>(
        self: &Rc<Self>,
        prompt: &'static str,
        request: F,
        done: impl FnOnce(&Rc<AppState>, kc_http::Result<T>) + 'static,
    ) where
        T: Send + 'static,
        F: Fn(Option<CaptchaAnswer>) -> Fut + 'static,
        Fut: Future<Output = kc_http::Result<T>> + Send + 'static,
    {
        spawn_local(self, request(None), move |app, result| match result {
            Err(Error::Captcha {
                service,
                sitekey,
                rqdata,
                rqtoken,
            }) if AVAILABLE && app.capture.borrow().captcha_in_app => {
                let challenge = Error::Captcha {
                    service,
                    sitekey: sitekey.clone(),
                    rqdata: rqdata.clone(),
                    rqtoken: rqtoken.clone(),
                };
                app.solve_captcha(
                    prompt,
                    &sitekey,
                    rqdata.as_deref(),
                    Box::new(move |app, key| match key {
                        Some(key) => {
                            spawn_local(app, request(Some(CaptchaAnswer { key, rqtoken })), done)
                        }
                        None => done(app, Err(challenge)),
                    }),
                );
            }
            other => done(app, other),
        });
    }

    /// Opens the captcha view with hCaptcha's test key, to check it works.
    pub(crate) fn test_captcha(self: &Rc<Self>) {
        self.solve_captcha(
            "A test captcha: it always passes, and nothing is sent to Discord.",
            TEST_SITEKEY,
            None,
            Box::new(|app, answer| {
                if answer.is_some() {
                    app.set_status("The captcha view works");
                }
            }),
        );
    }

    /// Without the captcha view there is nothing to show; the caller falls
    /// back to the browser.
    #[cfg(not(feature = "captcha"))]
    pub(crate) fn solve_captcha(
        self: &Rc<Self>,
        _prompt: &str,
        _sitekey: &str,
        _rqdata: Option<&str>,
        solved: Solved,
    ) {
        solved(self, None);
    }

    /// Shows the captcha in a dialog; `solved` gets the answer, or `None`
    /// when the dialog is closed first.
    #[cfg(feature = "captcha")]
    pub(crate) fn solve_captcha(
        self: &Rc<Self>,
        prompt: &str,
        sitekey: &str,
        rqdata: Option<&str>,
        solved: Solved,
    ) {
        use std::cell::RefCell;

        use gtk::prelude::*;
        use libadwaita as adw;
        use libadwaita::prelude::*;
        use webkit6::prelude::*;

        let dialog = adw::Dialog::new();
        dialog.set_title("Captcha");
        // Room for the image challenge, which opens over the checkbox.
        dialog.set_content_width(460);
        dialog.set_content_height(740);

        let note = gtk::Label::new(Some(prompt));
        note.set_wrap(true);
        note.set_xalign(0.0);
        note.set_margin_start(16);
        note.set_margin_end(16);
        note.set_margin_top(4);
        let problem = gtk::Label::new(None);
        problem.set_wrap(true);
        problem.set_xalign(0.0);
        problem.add_css_class("error");
        problem.set_margin_start(16);
        problem.set_margin_end(16);
        problem.set_visible(false);

        // A fresh session that keeps nothing once the dialog is gone.
        let manager = webkit6::UserContentManager::new();
        manager.register_script_message_handler("kubecord", None);
        let session = webkit6::NetworkSession::new_ephemeral();
        let view = webkit6::WebView::builder()
            .user_content_manager(&manager)
            .network_session(&session)
            .build();
        view.set_vexpand(true);
        view.set_background_color(&gtk::gdk::RGBA::new(0.0, 0.0, 0.0, 0.0));
        if let Some(settings) = WebViewExt::settings(&view) {
            // The same browser kubecord tells Discord it is.
            settings.set_user_agent(Some(kc_core::super_properties::user_agent()));
            settings.set_enable_developer_extras(false);
        }

        let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        content.append(&note);
        content.append(&problem);
        content.append(&view);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&content));
        dialog.set_child(Some(&toolbar));

        // The answer is given once: by the captcha, or as None on close.
        let solved = Rc::new(RefCell::new(Some(solved)));
        let finish = {
            let app = self.clone();
            let solved = solved.clone();
            Rc::new(move |answer: Option<String>| {
                if let Some(solved) = solved.borrow_mut().take() {
                    solved(&app, answer);
                }
            })
        };
        let weak_dialog = dialog.downgrade();
        let weak_problem = problem.downgrade();
        let answered = finish.clone();
        manager.connect_script_message_received(Some("kubecord"), move |_, value| {
            let message = value.to_str().to_string();
            if let Some(token) = message.strip_prefix("token:") {
                answered(Some(token.to_owned()));
                if let Some(dialog) = weak_dialog.upgrade() {
                    dialog.close();
                }
            } else if let Some(error) = message.strip_prefix("error:")
                && let Some(problem) = weak_problem.upgrade()
            {
                problem.set_label(&format!(
                    "The captcha reported a problem ({error}). Close this to finish in your browser instead."
                ));
                problem.set_visible(true);
            }
        });
        dialog.connect_closed(move |_| finish(None));

        let dark = adw::StyleManager::default().is_dark();
        view.load_html(
            &captcha_page(sitekey, rqdata, dark),
            Some("https://discord.com/"),
        );
        dialog.present(Some(&self.window));
    }
}

/// The page holding the widget. Its answer goes back through the
/// `kubecord` script message handler.
#[cfg_attr(not(feature = "captcha"), allow(dead_code))]
fn captcha_page(sitekey: &str, rqdata: Option<&str>, dark: bool) -> String {
    // JSON for the script, with `<` escaped so nothing can close the tag.
    let script_value = |json: Result<String, serde_json::Error>| {
        json.unwrap_or_else(|_| "null".to_owned())
            .replace('<', "\\u003c")
    };
    const PAGE: &str = r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<style>
  html, body { margin: 0; height: 100%; background: transparent; }
  body { display: flex; align-items: flex-start; justify-content: center; padding-top: 24px; box-sizing: border-box; }
</style>
<script>
  const SITEKEY = __SITEKEY__;
  const RQDATA = __RQDATA__;
  function send(message) { window.webkit.messageHandlers.kubecord.postMessage(message); }
  function ready() {
    const id = hcaptcha.render('captcha', {
      sitekey: SITEKEY,
      theme: '__THEME__',
      callback: function (token) { send('token:' + token); },
      'expired-callback': function () { send('expired'); },
      'error-callback': function (error) { send('error:' + error); },
    });
    if (RQDATA) { hcaptcha.setData(id, { rqdata: RQDATA }); }
  }
</script>
<script src="https://js.hcaptcha.com/1/api.js?render=explicit&onload=ready&recaptchacompat=off" async defer></script>
</head>
<body><div id="captcha"></div></body>
</html>
"#;
    PAGE.replace("__SITEKEY__", &script_value(serde_json::to_string(sitekey)))
        .replace("__RQDATA__", &script_value(serde_json::to_string(&rqdata)))
        .replace("__THEME__", if dark { "dark" } else { "light" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_embeds_its_values_safely() {
        let page = captcha_page("key\"</script>", Some("data"), true);
        assert!(
            page.contains(r#"const SITEKEY = "key\"\u003c/script>";"#),
            "{page}"
        );
        assert!(page.contains(r#"const RQDATA = "data";"#));
        assert!(page.contains("theme: 'dark'"));
        let page = captcha_page("key", None, false);
        assert!(page.contains("const RQDATA = null;"));
        assert!(page.contains("theme: 'light'"));
    }
}
