//! Creating a new server or joining one by invite, from the rail's "+" button.

use std::rc::Rc;

use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::shell::AppState;

impl AppState {
    /// The "Add a Server" dialog: create a new server, or join one by invite.
    pub(crate) fn add_server_dialog(self: &Rc<Self>) {
        let dialog = adw::Dialog::new();
        dialog.set_title("Add a Server");
        dialog.set_content_width(440);

        let header = adw::HeaderBar::new();

        let body = gtk::Box::new(gtk::Orientation::Vertical, 10);
        body.set_margin_top(6);
        body.set_margin_bottom(14);
        body.set_margin_start(14);
        body.set_margin_end(14);

        // -- Create ---------------------------------------------------------
        let create_group = adw::PreferencesGroup::new();
        create_group.set_title("Create a Server");
        create_group.set_description(Some("Make your own server. You'll be the owner."));
        let name = adw::EntryRow::new();
        name.set_title("Server Name");
        create_group.add(&name);
        body.append(&create_group);

        let create = gtk::Button::with_label("Create");
        create.add_css_class("suggested-action");
        create.add_css_class("pill");
        create.set_halign(gtk::Align::Center);
        create.set_sensitive(false);
        body.append(&create);

        // -- Join -----------------------------------------------------------
        let join_group = adw::PreferencesGroup::new();
        join_group.set_title("Join a Server");
        join_group.set_description(Some("Paste an invite link, like discord.gg/abc123."));
        let invite = adw::EntryRow::new();
        invite.set_title("Invite Link");
        join_group.add(&invite);
        body.append(&join_group);

        let join = gtk::Button::with_label("Join");
        join.add_css_class("pill");
        join.set_halign(gtk::Align::Center);
        join.set_sensitive(false);
        body.append(&join);

        // Buttons enable only with input.
        let create_btn = create.clone();
        name.connect_changed(move |entry| {
            create_btn.set_sensitive(!entry.text().trim().is_empty());
        });
        let join_btn = join.clone();
        invite.connect_changed(move |entry| {
            join_btn.set_sensitive(!invite_code(&entry.text()).is_empty());
        });

        let view = adw::ToolbarView::new();
        view.add_top_bar(&header);
        view.set_content(Some(&body));
        dialog.set_child(Some(&view));

        let app = self.clone();
        let parent = dialog.clone();
        let field = name.clone();
        create.connect_clicked(move |_| {
            let name = field.text().trim().to_owned();
            if name.is_empty() {
                return;
            }
            parent.close();
            app.create_server(&name);
        });

        let app = self.clone();
        let parent = dialog.clone();
        join.connect_clicked(move |_| {
            let code = invite_code(&invite.text());
            if code.is_empty() {
                return;
            }
            parent.close();
            app.join_server(&code);
        });

        dialog.present(Some(&self.window));
        name.grab_focus();
    }

    /// Creates a server and, once Discord returns it, opens it.
    fn create_server(self: &Rc<Self>, name: &str) {
        let Some((_, http, _)) = self.active() else {
            return;
        };
        let name = name.to_owned();
        self.spawn(
            async move { http.create_guild(&name, None).await },
            move |app, result| match result {
                Ok(guild_id) => app.select_guild(guild_id),
                Err(error) => app.set_status(&format!("Could not create the server: {error}")),
            },
        );
    }

    /// Accepts an invite and opens the server it points to. As on the
    /// website, the invite is looked up first, then accepted (within the
    /// safety limits).
    pub(crate) fn join_server(self: &Rc<Self>, code: &str) {
        let Some((_, http, _)) = self.active() else {
            return;
        };
        let code = code.to_owned();
        let lookup = code.clone();
        self.spawn(
            async move { http.resolve_invite(&lookup).await },
            move |app, result| match result {
                Ok(invite) => {
                    let invite = Rc::new(invite);
                    app.guard(crate::safety::Action::JoinServer, move |app| app.accept_invite(code, invite));
                }
                Err(error) => app.set_status(&format!("Could not join that server: {error}")),
            },
        );
    }

    fn accept_invite(self: &Rc<Self>, code: String, invite: Rc<kc_http::client::Invite>) {
        let Some((_, http, store)) = self.active() else {
            return;
        };
        let session_id = store.read(|state| state.session_id.clone());
        let invite = (*invite).clone();
        self.with_captcha(
            "Discord wants to make sure you're human before you join this server.",
            move |captcha| {
                let http = http.clone();
                let code = code.clone();
                let session_id = session_id.clone();
                let invite = invite.clone();
                async move { http.accept_invite(&code, session_id.as_deref(), Some(&invite), captcha.as_ref()).await }
            },
            move |app, result| match result {
                // The server also arrives over the gateway; selecting it opens
                // it once its channels are in.
                Ok(Some(guild_id)) => app.select_guild(guild_id),
                Ok(None) => app.set_status("Joined the server."),
                Err(error) if error.is_captcha() => app.captcha_needed(
                    "Discord wants a captcha solved before you join this server.",
                    "https://discord.com/channels/@me",
                ),
                Err(error) => app.set_status(&format!("Could not join that server: {error}")),
            },
        );
    }
}

/// The invite code from whatever the user pasted: a bare code, or a
/// `discord.gg/…` / `discord.com/invite/…` link (with or without scheme or
/// trailing query).
fn invite_code(input: &str) -> String {
    let trimmed = input.trim();
    let without_scheme = trimmed.split("://").last().unwrap_or(trimmed);
    let path = without_scheme
        .split(['?', '#'])
        .next()
        .unwrap_or(without_scheme);
    path.rsplit('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or("")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invite_codes_are_extracted_from_links() {
        assert_eq!(invite_code("abc123"), "abc123");
        assert_eq!(invite_code("  abc123 "), "abc123");
        assert_eq!(invite_code("https://discord.gg/abc123"), "abc123");
        assert_eq!(invite_code("discord.gg/abc123/"), "abc123");
        assert_eq!(invite_code("https://discord.com/invite/abc123"), "abc123");
        assert_eq!(invite_code("http://discordapp.com/invite/xyz?foo=1"), "xyz");
        assert_eq!(invite_code(""), "");
        assert_eq!(invite_code("   "), "");
    }
}
