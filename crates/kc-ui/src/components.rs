//! Message components: the buttons and select menus apps put under their
//! messages, and the layout pieces of messages made of components alone
//! (text, sections, containers, galleries, files, separators). Pressing a
//! button or choosing from a menu sends an interaction to the app.

use std::rc::Rc;

use gtk::prelude::*;
use libadwaita as adw;
use kc_core::Snowflake;
use kc_http::cdn;
use kc_state::{Attachment, Message, State};
use serde_json::Value;

use crate::interactions::{Outcome, Waiting};
use crate::shell::AppState;
use crate::widgets::{hex_color, label};

/// Component types.
mod kind {
    pub const ACTION_ROW: u64 = 1;
    pub const BUTTON: u64 = 2;
    pub const STRING_SELECT: u64 = 3;
    pub const USER_SELECT: u64 = 5;
    pub const ROLE_SELECT: u64 = 6;
    pub const MENTIONABLE_SELECT: u64 = 7;
    pub const CHANNEL_SELECT: u64 = 8;
    pub const SECTION: u64 = 9;
    pub const TEXT_DISPLAY: u64 = 10;
    pub const THUMBNAIL: u64 = 11;
    pub const MEDIA_GALLERY: u64 = 12;
    pub const FILE: u64 = 13;
    pub const SEPARATOR: u64 = 14;
    pub const CONTAINER: u64 = 17;
}

/// Button styles.
mod style {
    pub const PRIMARY: u64 = 1;
    pub const SUCCESS: u64 = 3;
    pub const DANGER: u64 = 4;
    pub const LINK: u64 = 5;
    pub const PREMIUM: u64 = 6;
}

/// Picture size of a section's thumbnail.
const THUMBNAIL: i32 = 85;

/// The message a component belongs to, as interactions need it.
#[derive(Clone)]
struct Owner {
    application_id: Snowflake,
    message_id: Snowflake,
    flags: u64,
}

/// An emoji on a button or menu option: a server emoji's picture, or a
/// standard one (in Discord's art when on).
fn emoji_widget(app: &AppState, emoji: &Value, size: i32) -> Option<gtk::Widget> {
    if let Some(id) = emoji["id"].as_str().and_then(|id| id.parse::<Snowflake>().ok()) {
        let image = gtk::Image::new();
        image.set_pixel_size(size);
        app.images.set_image(
            &image,
            &cdn::emoji(id, emoji["animated"].as_bool().unwrap_or(false), size as u32 * 2),
        );
        return Some(image.upcast());
    }
    let name = emoji["name"].as_str().filter(|name| !name.is_empty())?;
    Some(match crate::discord_emoji::image(name, size) {
        Some(image) => image.upcast(),
        None => gtk::Label::new(Some(name)).upcast(),
    })
}

/// A component's media URL (`{"media": {"url": ...}}` or `{"url": ...}`).
fn media_url(media: &Value) -> Option<String> {
    media["proxy_url"]
        .as_str()
        .or_else(|| media["url"].as_str())
        .map(str::to_owned)
}

/// Reads an unfurled media item as an attachment, to show it the way
/// attachments are shown.
fn media_attachment(media: &Value, name: Option<&str>) -> Option<Attachment> {
    let url = media["url"].as_str()?;
    let filename = name
        .map(str::to_owned)
        .or_else(|| url.split('?').next()?.rsplit('/').next().map(str::to_owned))
        .unwrap_or_else(|| "file".to_owned());
    serde_json::from_value(serde_json::json!({
        "id": "0",
        "filename": filename,
        "url": url,
        "proxy_url": media["proxy_url"].as_str().unwrap_or(url),
        "content_type": media["content_type"],
        "width": media["width"],
        "height": media["height"],
        "size": media["size"].as_u64().unwrap_or(0),
    }))
    .ok()
}

impl AppState {
    /// The components of `message`, or `None` when it has none.
    pub(crate) fn components_widget(
        self: &Rc<Self>,
        message: &Message,
        guild: Option<Snowflake>,
        state: &State,
    ) -> Option<gtk::Widget> {
        if message.components.is_empty() {
            return None;
        }
        let owner = Owner {
            application_id: message
                .application_id
                .or_else(|| message.author.bot.filter(|bot| *bot).map(|_| message.author.id))?,
            message_id: message.id,
            flags: message.flags,
        };
        let column = gtk::Box::new(gtk::Orientation::Vertical, 6);
        column.set_margin_top(4);
        for component in &message.components {
            if let Some(widget) = self.component(component, &owner, guild, state) {
                column.append(&widget);
            }
        }
        Some(column.upcast())
    }

    fn component(
        self: &Rc<Self>,
        component: &Value,
        owner: &Owner,
        guild: Option<Snowflake>,
        state: &State,
    ) -> Option<gtk::Widget> {
        match component["type"].as_u64()? {
            kind::ACTION_ROW => {
                let row = adw::WrapBox::new();
                row.set_child_spacing(8);
                row.set_line_spacing(8);
                for child in component["components"].as_array().into_iter().flatten() {
                    if let Some(widget) = self.component(child, owner, guild, state) {
                        row.append(&widget);
                    }
                }
                Some(row.upcast())
            }
            kind::BUTTON => Some(self.component_button(component, owner).upcast()),
            kind::STRING_SELECT
            | kind::USER_SELECT
            | kind::ROLE_SELECT
            | kind::MENTIONABLE_SELECT
            | kind::CHANNEL_SELECT => Some(self.select_menu(component, owner, guild, state).upcast()),
            kind::TEXT_DISPLAY => self.text_widget(component["content"].as_str()?, guild, state),
            kind::SECTION => {
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
                let texts = gtk::Box::new(gtk::Orientation::Vertical, 4);
                texts.set_hexpand(true);
                for child in component["components"].as_array().into_iter().flatten() {
                    if let Some(widget) = self.component(child, owner, guild, state) {
                        texts.append(&widget);
                    }
                }
                row.append(&texts);
                if let Some(accessory) = self.component(&component["accessory"], owner, guild, state) {
                    accessory.set_valign(gtk::Align::Start);
                    row.append(&accessory);
                }
                Some(row.upcast())
            }
            kind::THUMBNAIL => {
                let image = gtk::Image::new();
                image.set_pixel_size(THUMBNAIL);
                image.add_css_class("kc-thumbnail");
                if let Some(url) = media_url(&component["media"]) {
                    self.images.set_image(&image, &url);
                }
                if let Some(text) = component["description"].as_str() {
                    image.set_tooltip_text(Some(text));
                }
                Some(image.upcast())
            }
            kind::MEDIA_GALLERY => {
                let gallery = adw::WrapBox::new();
                gallery.set_child_spacing(4);
                gallery.set_line_spacing(4);
                for item in component["items"].as_array().into_iter().flatten() {
                    if let Some(attachment) = media_attachment(&item["media"], None) {
                        gallery.append(&self.attachment_widget(&attachment));
                    }
                }
                Some(gallery.upcast())
            }
            kind::FILE => {
                let attachment = media_attachment(&component["file"], component["name"].as_str())?;
                Some(self.attachment_widget(&attachment))
            }
            kind::SEPARATOR => {
                let gap = if component["spacing"].as_u64() == Some(2) { 12 } else { 4 };
                if component["divider"].as_bool().unwrap_or(true) {
                    let line = gtk::Separator::new(gtk::Orientation::Horizontal);
                    line.set_margin_top(gap);
                    line.set_margin_bottom(gap);
                    Some(line.upcast())
                } else {
                    let space = gtk::Box::new(gtk::Orientation::Vertical, 0);
                    space.set_height_request(gap * 2);
                    Some(space.upcast())
                }
            }
            kind::CONTAINER => {
                let card = gtk::Box::new(gtk::Orientation::Vertical, 6);
                card.add_css_class("kc-embed");
                if let Some(color) = component["accent_color"].as_u64().filter(|color| *color != 0) {
                    let provider = gtk::CssProvider::new();
                    provider.load_from_string(&format!(
                        "box.kc-embed {{ border-left-color: {}; }}",
                        hex_color(color as u32)
                    ));
                    #[allow(deprecated)]
                    card.style_context()
                        .add_provider(&provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1);
                }
                for child in component["components"].as_array().into_iter().flatten() {
                    if let Some(widget) = self.component(child, owner, guild, state) {
                        card.append(&widget);
                    }
                }
                Some(card.upcast())
            }
            _ => None,
        }
    }

    /// A button: link buttons open their page; the others tell the app.
    fn component_button(self: &Rc<Self>, component: &Value, owner: &Owner) -> gtk::Button {
        let button = gtk::Button::new();
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        if let Some(emoji) = emoji_widget(self, &component["emoji"], 18) {
            content.append(&emoji);
        }
        if let Some(text) = component["label"].as_str().filter(|text| !text.is_empty()) {
            content.append(&gtk::Label::new(Some(text)));
        }
        let style = component["style"].as_u64().unwrap_or(2);
        if style == style::LINK {
            let icon = gtk::Image::from_icon_name("adw-external-link-symbolic");
            icon.set_pixel_size(12);
            content.append(&icon);
        }
        button.set_child(Some(&content));
        button.add_css_class("kc-component-button");
        match style {
            style::PRIMARY => button.add_css_class("suggested-action"),
            style::SUCCESS => button.add_css_class("kc-success"),
            style::DANGER => button.add_css_class("destructive-action"),
            _ => {}
        }
        let disabled = component["disabled"].as_bool().unwrap_or(false);
        button.set_sensitive(!disabled && style != style::PREMIUM);
        if style == style::LINK {
            let url = component["url"].as_str().unwrap_or_default().to_owned();
            button.set_tooltip_text(Some(&url));
            let app = self.clone();
            button.connect_clicked(move |button| app.activate_link(&url, button.upcast_ref()));
            return button;
        }
        let custom_id = component["custom_id"].as_str().unwrap_or_default().to_owned();
        let owner = owner.clone();
        let app = self.clone();
        button.connect_clicked(move |button| {
            app.use_component(&owner, kind::BUTTON as u8, &custom_id, None, button.upcast_ref());
        });
        button
    }

    /// A select menu: its options in a popover; one choice sends at once,
    /// several are sent with Done.
    fn select_menu(
        self: &Rc<Self>,
        component: &Value,
        owner: &Owner,
        guild: Option<Snowflake>,
        state: &State,
    ) -> gtk::MenuButton {
        let component_type = component["type"].as_u64().unwrap_or(kind::STRING_SELECT);
        let placeholder = component["placeholder"].as_str().unwrap_or("Make a selection");
        let max = component["max_values"].as_u64().unwrap_or(1).max(1) as usize;
        let min = component["min_values"].as_u64().unwrap_or(1) as usize;
        let custom_id = component["custom_id"].as_str().unwrap_or_default().to_owned();

        // (label, value, description, emoji, chosen) for each choice.
        let choices: Vec<(String, String, String, Value, bool)> = match component_type {
            kind::STRING_SELECT => component["options"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|option| {
                    (
                        option["label"].as_str().unwrap_or_default().to_owned(),
                        option["value"].as_str().unwrap_or_default().to_owned(),
                        option["description"].as_str().unwrap_or_default().to_owned(),
                        option["emoji"].clone(),
                        option["default"].as_bool().unwrap_or(false),
                    )
                })
                .collect(),
            _ => select_choices(component_type, guild, state),
        };

        let menu = gtk::MenuButton::new();
        menu.add_css_class("kc-select-menu");
        let chosen_labels: Vec<&str> = choices
            .iter()
            .filter(|(.., chosen)| *chosen)
            .map(|(label, ..)| label.as_str())
            .collect();
        menu.set_label(&if chosen_labels.is_empty() {
            placeholder.to_owned()
        } else {
            chosen_labels.join(", ")
        });
        menu.set_sensitive(!component["disabled"].as_bool().unwrap_or(false));
        menu.set_width_request(280);

        let popover = gtk::Popover::new();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let list = gtk::ListBox::new();
        list.add_css_class("navigation-sidebar");
        list.set_selection_mode(gtk::SelectionMode::None);
        let mut checks: Vec<(gtk::CheckButton, String)> = Vec::new();
        for (text, value, description, emoji, chosen) in &choices {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row.set_margin_top(2);
            row.set_margin_bottom(2);
            if max > 1 {
                let check = gtk::CheckButton::new();
                check.set_active(*chosen);
                row.append(&check);
                checks.push((check, value.clone()));
            }
            if let Some(emoji) = emoji_widget(self, emoji, 20) {
                row.append(&emoji);
            }
            let words = gtk::Box::new(gtk::Orientation::Vertical, 0);
            words.append(&label(text, &[]));
            if !description.is_empty() {
                words.append(&label(description, &["caption", "dim-label"]));
            }
            row.append(&words);
            list.append(&row);
        }
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_child(Some(&list));
        scroll.set_hscrollbar_policy(gtk::PolicyType::Never);
        scroll.set_propagate_natural_height(true);
        scroll.set_max_content_height(320);
        content.append(&scroll);

        let values: Vec<String> = choices.iter().map(|(_, value, ..)| value.clone()).collect();
        if max > 1 {
            let done = gtk::Button::with_label("Done");
            done.add_css_class("suggested-action");
            let owner = owner.clone();
            let app = self.clone();
            let id = custom_id.clone();
            let popover_ref = popover.downgrade();
            let anchor = menu.downgrade();
            done.connect_clicked(move |_| {
                let picked: Vec<String> = checks
                    .iter()
                    .filter(|(check, _)| check.is_active())
                    .map(|(_, value)| value.clone())
                    .collect();
                if picked.len() < min || picked.len() > max {
                    app.set_status(&format!("Choose between {min} and {max}"));
                    return;
                }
                if let Some(popover) = popover_ref.upgrade() {
                    popover.popdown();
                }
                if let Some(anchor) = anchor.upgrade() {
                    app.use_component(&owner, component_type as u8, &id, Some(picked), anchor.upcast_ref());
                }
            });
            content.append(&done);
        } else {
            list.set_activate_on_single_click(true);
            for row in 0..values.len() {
                if let Some(row) = list.row_at_index(row as i32) {
                    row.set_activatable(true);
                }
            }
            let owner = owner.clone();
            let app = self.clone();
            let popover_ref = popover.downgrade();
            let anchor = menu.downgrade();
            list.connect_row_activated(move |_, row| {
                let Some(value) = values.get(row.index() as usize) else {
                    return;
                };
                if let Some(popover) = popover_ref.upgrade() {
                    popover.popdown();
                }
                if let Some(anchor) = anchor.upgrade() {
                    app.use_component(
                        &owner,
                        component_type as u8,
                        &custom_id,
                        Some(vec![value.clone()]),
                        anchor.upcast_ref(),
                    );
                }
            });
        }
        popover.set_child(Some(&content));
        menu.set_popover(Some(&popover));
        menu
    }

    /// Tells the app a button was pressed or menu choices made; `widget`
    /// waits (greyed) until it answers.
    fn use_component(
        self: &Rc<Self>,
        owner: &Owner,
        component_type: u8,
        custom_id: &str,
        values: Option<Vec<String>>,
        widget: &gtk::Widget,
    ) {
        let Some((http, place)) = self.interaction_place(owner.application_id) else {
            self.set_status("Not connected yet: try again in a moment");
            return;
        };
        let nonce = kc_http::HttpClient::new_nonce();
        let body = place.component(
            owner.message_id,
            owner.flags,
            component_type,
            custom_id,
            values,
            &nonce,
        );
        widget.set_sensitive(false);
        let waiting = widget.downgrade();
        self.send_interaction(
            http,
            body,
            nonce,
            Waiting::Outcome(Box::new(move |outcome| {
                if let Some(widget) = waiting.upgrade() {
                    widget.set_sensitive(true);
                }
                if let Outcome::Failed(reason) = outcome {
                    tracing::info!(reason, "an app did not take a button or menu choice");
                }
            })),
        );
    }
}

/// The choices of a user, role, mentionable or channel select menu here.
fn select_choices(
    component_type: u64,
    guild: Option<Snowflake>,
    state: &State,
) -> Vec<(String, String, String, Value, bool)> {
    let mut choices = Vec::new();
    if matches!(component_type, kind::USER_SELECT | kind::MENTIONABLE_SELECT)
        && let Some(guild) = guild
    {
        let mut users: Vec<&kc_state::User> = state
            .guild_members
            .get(&guild)
            .into_iter()
            .flatten()
            .filter_map(|id| state.users.get(id))
            .collect();
        users.sort_by_key(|user| user.username.to_lowercase());
        for user in users {
            choices.push((
                state.display_name(Some(guild), user),
                user.id.to_string(),
                user.username.clone(),
                Value::Null,
                false,
            ));
        }
    }
    if matches!(component_type, kind::ROLE_SELECT | kind::MENTIONABLE_SELECT)
        && let Some(guild) = guild.and_then(|id| state.guilds.get(&id))
    {
        for role in &guild.roles {
            choices.push((format!("@{}", role.name), role.id.to_string(), String::new(), Value::Null, false));
        }
    }
    if component_type == kind::CHANNEL_SELECT
        && let Some(guild) = guild
    {
        for section in state.channel_tree(guild) {
            for channel in section.channels {
                choices.push((
                    format!("#{}", channel.display_name()),
                    channel.id.to_string(),
                    String::new(),
                    Value::Null,
                    false,
                ));
            }
        }
    }
    choices
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gallery_media_reads_as_an_attachment() {
        let media = serde_json::json!({
            "url": "https://cdn.discordapp.com/attachments/1/2/cat.png?ex=1",
            "proxy_url": "https://media.discordapp.net/attachments/1/2/cat.png",
            "width": 640, "height": 480, "content_type": "image/png"
        });
        let attachment = media_attachment(&media, None).unwrap();
        assert_eq!(attachment.filename, "cat.png");
        assert!(attachment.is_image());
        assert_eq!(attachment.width, Some(640));
    }
}
