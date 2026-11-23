//! Server Settings: Emoji and Stickers. Upload, rename and delete a
//! server's own; the changes come back on the gateway
//! (GUILD_EMOJIS_UPDATE, GUILD_STICKERS_UPDATE) and the lists follow them.

use std::path::PathBuf;
use std::rc::Rc;

use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use kc_core::Snowflake;
use kc_http::cdn;
use kc_state::Sticker;

use super::{Panel, boxed_list, page_box, placeholder_row, section_title, wrapped};
use crate::shell::AppState;
use crate::video_setup::spawn_local;
use crate::widgets::clear_box;

/// Discord's limits.
const EMOJI_BYTES: usize = 256 * 1024;
const STICKER_BYTES: usize = 512 * 1024;

/// An emoji name Discord takes from a file name: letters, digits and
/// underscores, 2 to 32 of them.
pub(super) fn emoji_name(file_name: &str) -> String {
    let stem = file_name.rsplit_once('.').map_or(file_name, |(stem, _)| stem);
    let mut name: String = stem
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .take(32)
        .collect();
    while name.len() < 2 {
        name.push('_');
    }
    name
}

fn file_name(path: &std::path::Path) -> String {
    path.file_name().map_or_else(|| "file".to_owned(), |name| name.to_string_lossy().into_owned())
}

/// Picks a file of the given types, then hands over its path.
fn pick_file(window: &impl IsA<gtk::Window>, title: &str, types: &[(&str, &str)], picked: impl Fn(PathBuf) + 'static) {
    let dialog = gtk::FileDialog::new();
    dialog.set_title(title);
    let filter = gtk::FileFilter::new();
    filter.set_name(Some(title));
    for (mime, pattern) in types {
        filter.add_mime_type(mime);
        filter.add_pattern(pattern);
    }
    let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);
    dialog.set_filters(Some(&filters));
    dialog.open(Some(window), gtk::gio::Cancellable::NONE, move |result| {
        if let Some(path) = result.ok().and_then(|file| file.path()) {
            picked(path);
        }
    });
}

impl AppState {
    // -- emoji -----------------------------------------------------------

    pub(super) fn emoji_page(self: &Rc<Self>, panel: &Rc<Panel>) -> gtk::Box {
        let page = page_box();
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let title = section_title("Emoji");
        title.set_hexpand(true);
        header.append(&title);
        let upload = gtk::Button::with_label("Upload Emoji");
        upload.add_css_class("suggested-action");
        upload.add_css_class("pill");
        let app = self.clone();
        let target = panel.clone();
        upload.connect_clicked(move |_| app.upload_emoji(&target));
        header.append(&upload);
        page.append(&header);
        page.append(&wrapped(
            "PNG, JPEG, GIF or WebP, up to 256 KB. Names are 2 to 32 letters, numbers and underscores; \
             one is made from the file's name and can be changed after.",
            &["dim-label"],
        ));
        page.append(&panel.emojis);
        self.fill_emojis(panel);
        page
    }

    pub(super) fn fill_emojis(self: &Rc<Self>, panel: &Rc<Panel>) {
        let guild = panel.guild;
        let Some(emojis) = self
            .with_session(|session| session.store.read(|state| state.guilds.get(&guild).map(|guild| guild.emojis.clone())))
            .flatten()
        else {
            return;
        };
        clear_box(&panel.emojis);
        let (still, animated): (Vec<_>, Vec<_>) = emojis.into_iter().partition(|emoji| !emoji.animated);
        for (heading, emojis) in [("Emoji", still), ("Animated Emoji", animated)] {
            let group = adw::PreferencesGroup::new();
            group.set_title(&format!("{heading} — {}", emojis.len()));
            group.set_margin_top(8);
            let list = boxed_list();
            if emojis.is_empty() {
                placeholder_row(&list, "None yet");
            }
            for emoji in emojis {
                let row = adw::ActionRow::new();
                row.set_title(&glib::markup_escape_text(&format!(":{}:", emoji.name)));
                if !emoji.available {
                    row.set_subtitle("Unavailable (the server lost the boost level for it)");
                }
                let image = gtk::Picture::new();
                image.set_size_request(32, 32);
                image.set_can_shrink(true);
                image.set_valign(gtk::Align::Center);
                self.images.set_picture(&image, &cdn::emoji(emoji.id, emoji.animated, 64));
                row.add_prefix(&image);

                let rename = gtk::Button::from_icon_name("document-edit-symbolic");
                rename.add_css_class("flat");
                rename.set_valign(gtk::Align::Center);
                rename.set_tooltip_text(Some("Rename"));
                let (app, target, id, name) = (self.clone(), panel.clone(), emoji.id, emoji.name.clone());
                rename.connect_clicked(move |_| app.rename_emoji(&target, id, &name));
                row.add_suffix(&rename);

                let delete = gtk::Button::from_icon_name("user-trash-symbolic");
                delete.add_css_class("flat");
                delete.set_valign(gtk::Align::Center);
                delete.set_tooltip_text(Some("Delete"));
                let (app, target, id, name) = (self.clone(), panel.clone(), emoji.id, emoji.name.clone());
                delete.connect_clicked(move |_| app.delete_expression(&target, id, &format!(":{name}:"), false));
                row.add_suffix(&delete);
                list.append(&row);
            }
            group.add(&list);
            panel.emojis.append(&group);
        }
    }

    fn upload_emoji(self: &Rc<Self>, panel: &Rc<Panel>) {
        let (app, panel) = (self.clone(), panel.clone());
        let types = [("image/png", "*.png"), ("image/jpeg", "*.jpg"), ("image/gif", "*.gif"), ("image/webp", "*.webp")];
        pick_file(&self.window, "Emoji Image", &types, move |path| {
            let Some((_, http, _)) = app.active() else {
                return;
            };
            let (guild, panel) = (panel.guild, panel.clone());
            let file = file_name(&path);
            let name = emoji_name(&file);
            spawn_local(
                &app,
                async move {
                    let bytes = tokio::fs::read(&path).await.map_err(|error| error.to_string())?;
                    if bytes.len() > EMOJI_BYTES {
                        return Err("the image is larger than 256 KB".to_owned());
                    }
                    http.create_emoji(guild, &name, &bytes, &file).await.map_err(|error| error.describe())?;
                    Ok(name)
                },
                move |_, result| match result {
                    Ok(name) => panel.toast(&format!("Added :{name}:")),
                    Err(error) => panel.toast(&format!("Could not add the emoji: {error}")),
                },
            );
        });
    }

    fn rename_emoji(self: &Rc<Self>, panel: &Rc<Panel>, id: Snowflake, current: &str) {
        let dialog = adw::AlertDialog::new(Some("Rename Emoji"), None);
        let entry = gtk::Entry::new();
        entry.set_text(current);
        entry.set_max_length(32);
        entry.set_activates_default(true);
        dialog.set_extra_child(Some(&entry));
        dialog.add_responses(&[("cancel", "Cancel"), ("save", "Rename")]);
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("save"));
        dialog.set_close_response("cancel");
        let parent = panel.dialog.clone();
        let (app, panel) = (self.clone(), panel.clone());
        dialog.connect_response(Some("save"), move |_, _| {
            let name = emoji_name(&entry.text());
            let Some((_, http, _)) = app.active() else {
                return;
            };
            let (guild, panel) = (panel.guild, panel.clone());
            spawn_local(
                &app,
                async move { http.rename_emoji(guild, id, &name).await },
                move |_, result| {
                    if let Err(error) = result {
                        panel.fail("Could not rename the emoji", &error);
                    }
                },
            );
        });
        dialog.present(Some(&parent));
    }

    /// Asks, then deletes an emoji (or, with `sticker`, a sticker).
    fn delete_expression(self: &Rc<Self>, panel: &Rc<Panel>, id: Snowflake, name: &str, sticker: bool) {
        let what = if sticker { "Sticker" } else { "Emoji" };
        let dialog = adw::AlertDialog::new(
            Some(&format!("Delete {what}?")),
            Some(&format!("{name} will be gone from the server for everyone.")),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let parent = panel.dialog.clone();
        let (app, panel) = (self.clone(), panel.clone());
        dialog.connect_response(Some("delete"), move |_, _| {
            let Some((_, http, _)) = app.active() else {
                return;
            };
            let (guild, panel) = (panel.guild, panel.clone());
            spawn_local(
                &app,
                async move {
                    if sticker {
                        http.delete_sticker(guild, id).await
                    } else {
                        http.delete_emoji(guild, id).await
                    }
                },
                move |_, result| {
                    if let Err(error) = result {
                        panel.fail(&format!("Could not delete the {}", what.to_lowercase()), &error);
                    }
                },
            );
        });
        dialog.present(Some(&parent));
    }

    // -- stickers --------------------------------------------------------

    pub(super) fn stickers_page(self: &Rc<Self>, panel: &Rc<Panel>) -> gtk::Box {
        let page = page_box();
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let title = section_title("Stickers");
        title.set_hexpand(true);
        header.append(&title);
        let upload = gtk::Button::with_label("Upload Sticker");
        upload.add_css_class("suggested-action");
        upload.add_css_class("pill");
        let app = self.clone();
        let target = panel.clone();
        upload.connect_clicked(move |_| app.sticker_dialog(&target, None));
        header.append(&upload);
        page.append(&header);
        page.append(&wrapped(
            "PNG, APNG, GIF or Lottie JSON, 320 × 320 and up to 512 KB. Each needs a name and the emoji it stands for.",
            &["dim-label"],
        ));
        page.append(&panel.stickers);
        self.fill_stickers(panel);
        page
    }

    pub(super) fn fill_stickers(self: &Rc<Self>, panel: &Rc<Panel>) {
        let guild = panel.guild;
        let Some(stickers) = self
            .with_session(|session| session.store.read(|state| state.guilds.get(&guild).map(|guild| guild.stickers.clone())))
            .flatten()
        else {
            return;
        };
        clear_box(&panel.stickers);
        let list = boxed_list();
        if stickers.is_empty() {
            placeholder_row(&list, "No stickers yet");
        }
        for sticker in stickers {
            let row = adw::ActionRow::new();
            row.set_title(&glib::markup_escape_text(&sticker.name));
            let mut facts = vec![sticker.tags.clone()];
            if !sticker.description.is_empty() {
                facts.push(sticker.description.clone());
            }
            row.set_subtitle(&glib::markup_escape_text(&facts.join(" · ")));
            row.add_prefix(&self.sticker_picture(sticker.id, sticker.format_type, 48, false));

            let edit = gtk::Button::from_icon_name("document-edit-symbolic");
            edit.add_css_class("flat");
            edit.set_valign(gtk::Align::Center);
            edit.set_tooltip_text(Some("Edit"));
            let (app, target, editing) = (self.clone(), panel.clone(), sticker.clone());
            edit.connect_clicked(move |_| app.sticker_dialog(&target, Some(editing.clone())));
            row.add_suffix(&edit);

            let delete = gtk::Button::from_icon_name("user-trash-symbolic");
            delete.add_css_class("flat");
            delete.set_valign(gtk::Align::Center);
            delete.set_tooltip_text(Some("Delete"));
            let (app, target, id, name) = (self.clone(), panel.clone(), sticker.id, sticker.name.clone());
            delete.connect_clicked(move |_| app.delete_expression(&target, id, &name, true));
            row.add_suffix(&delete);
            list.append(&row);
        }
        panel.stickers.append(&list);
    }

    /// Uploads a sticker, or edits `editing`'s name, emoji and description.
    fn sticker_dialog(self: &Rc<Self>, panel: &Rc<Panel>, editing: Option<Sticker>) {
        let dialog = adw::Dialog::new();
        dialog.set_title(if editing.is_some() { "Edit Sticker" } else { "Upload Sticker" });
        dialog.set_content_width(420);
        let page = adw::PreferencesPage::new();
        let group = adw::PreferencesGroup::new();

        let chosen: Rc<std::cell::RefCell<Option<PathBuf>>> = Rc::default();
        let file_row = adw::ActionRow::new();
        if editing.is_none() {
            file_row.set_title("File");
            file_row.set_subtitle("None chosen");
            let browse = gtk::Button::with_label("Browse…");
            browse.set_valign(gtk::Align::Center);
            file_row.add_suffix(&browse);
            let (window, chosen, row) = (self.window.clone(), chosen.clone(), file_row.clone());
            browse.connect_clicked(move |_| {
                let (chosen, row) = (chosen.clone(), row.clone());
                let types = [("image/png", "*.png"), ("image/apng", "*.apng"), ("image/gif", "*.gif"), ("application/json", "*.json")];
                pick_file(&window, "Sticker File", &types, move |path| {
                    row.set_subtitle(&glib::markup_escape_text(&file_name(&path)));
                    chosen.replace(Some(path));
                });
            });
            group.add(&file_row);
        }
        let name = adw::EntryRow::new();
        name.set_title("Name");
        name.set_max_length(30);
        let tags = adw::EntryRow::new();
        tags.set_title("Related Emoji (e.g. 😄 or smile)");
        tags.set_max_length(200);
        let description = adw::EntryRow::new();
        description.set_title("Description (optional)");
        description.set_max_length(100);
        if let Some(sticker) = &editing {
            name.set_text(&sticker.name);
            tags.set_text(&sticker.tags);
            description.set_text(&sticker.description);
        }
        group.add(&name);
        group.add(&tags);
        group.add(&description);
        page.add(&group);

        let save = gtk::Button::with_label(if editing.is_some() { "Save" } else { "Upload" });
        save.add_css_class("suggested-action");
        let cancel = gtk::Button::with_label("Cancel");
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.set_halign(gtk::Align::End);
        buttons.set_margin_top(8);
        buttons.set_margin_bottom(12);
        buttons.set_margin_end(12);
        buttons.append(&cancel);
        buttons.append(&save);
        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&page));
        view.add_bottom_bar(&buttons);
        dialog.set_child(Some(&view));

        let target = dialog.clone();
        cancel.connect_clicked(move |_| {
            target.close();
        });
        let parent = panel.dialog.clone();
        let (app, panel, target) = (self.clone(), panel.clone(), dialog.clone());
        save.connect_clicked(move |_| {
            let (name, tags, description) =
                (name.text().trim().to_owned(), tags.text().trim().to_owned(), description.text().trim().to_owned());
            if name.chars().count() < 2 || tags.is_empty() {
                panel.toast("A sticker needs a name (2 or more characters) and an emoji");
                return;
            }
            let Some((_, http, _)) = app.active() else {
                return;
            };
            let (guild, report) = (panel.guild, panel.clone());
            match &editing {
                Some(sticker) => {
                    let id = sticker.id;
                    spawn_local(
                        &app,
                        async move { http.edit_sticker(guild, id, &name, &description, &tags).await },
                        move |_, result| {
                            if let Err(error) = result {
                                report.fail("Could not save the sticker", &error);
                            }
                        },
                    );
                }
                None => {
                    let Some(path) = chosen.borrow().clone() else {
                        panel.toast("Choose the sticker's file first");
                        return;
                    };
                    spawn_local(
                        &app,
                        async move {
                            let bytes = tokio::fs::read(&path).await.map_err(|error| error.to_string())?;
                            if bytes.len() > STICKER_BYTES {
                                return Err("the file is larger than 512 KB".to_owned());
                            }
                            let file = file_name(&path);
                            http.create_sticker(guild, &name, &description, &tags, bytes, &file)
                                .await
                                .map_err(|error| error.describe())?;
                            Ok(name)
                        },
                        move |_, result| match result {
                            Ok(name) => report.toast(&format!("Added the sticker {name}")),
                            Err(error) => report.toast(&format!("Could not add the sticker: {error}")),
                        },
                    );
                }
            }
            target.close();
        });
        dialog.present(Some(&parent));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emoji_names_come_from_file_names() {
        assert_eq!(emoji_name("party parrot.gif"), "party_parrot");
        assert_eq!(emoji_name("a.png"), "a_");
        assert_eq!(emoji_name("très-cool!.webp"), "tr_s_cool_");
        assert_eq!(emoji_name(&format!("{}.png", "x".repeat(40))).len(), 32);
    }
}
