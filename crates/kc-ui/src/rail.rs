//! The server rail: home, unread direct messages, guilds and guild folders.

use std::rc::Rc;

use gtk::prelude::*;

use kc_core::Snowflake;
use kc_http::cdn;
use kc_state::{Channel, GuildEntry};

use crate::shell::AppState;
use crate::widgets::{badge, clear_box, first_icon, hex_color};

const ICON: i32 = 48;

/// What the rail needs to draw one guild.
#[derive(Hash)]
struct GuildItem {
    id: Snowflake,
    name: String,
    icon: Option<String>,
    unread: bool,
    mentions: u32,
}

#[derive(Hash)]
enum RailEntry {
    Guild(GuildItem),
    Folder {
        id: i64,
        name: Option<String>,
        color: Option<u32>,
        guilds: Vec<GuildItem>,
    },
}

impl AppState {
    pub(crate) fn render_guilds(self: &Rc<Self>) {
        let snapshot = self.with_session(|session| {
            session.store.read(|state| {
                let item = |id: Snowflake| -> Option<GuildItem> {
                    let guild = state.guilds.get(&id)?;
                    let (unread, mentions) = state.guild_unread(id);
                    Some(GuildItem {
                        id,
                        name: if guild.name.is_empty() {
                            "Unavailable server".to_owned()
                        } else {
                            guild.name.clone()
                        },
                        icon: guild
                            .icon
                            .as_deref()
                            .map(|hash| cdn::guild_icon(id, hash, ICON as u32 * 2)),
                        unread,
                        mentions,
                    })
                };
                let entries: Vec<RailEntry> = state
                    .guild_entries()
                    .into_iter()
                    .filter_map(|entry| match entry {
                        GuildEntry::Guild(id) => item(id).map(RailEntry::Guild),
                        GuildEntry::Folder {
                            id,
                            name,
                            color,
                            guild_ids,
                        } => Some(RailEntry::Folder {
                            id,
                            name,
                            color,
                            guilds: guild_ids.into_iter().filter_map(item).collect(),
                        }),
                    })
                    .collect();
                let unread_dms: Vec<(Channel, String, Option<String>, u32)> = state
                    .unread_dms()
                    .into_iter()
                    .take(8)
                    .map(|(channel, count)| {
                        let icon = match channel.dm_recipient() {
                            Some(user) => Some(cdn::user_avatar(
                                user.id,
                                user.avatar.as_deref(),
                                ICON as u32 * 2,
                            )),
                            None => channel
                                .icon
                                .as_deref()
                                .map(|hash| cdn::channel_icon(channel.id, hash, ICON as u32 * 2)),
                        };
                        (channel.clone(), state.channel_title(channel), icon, count)
                    })
                    .collect();
                (entries, unread_dms)
            })
        });
        let Some((entries, unread_dms)) = snapshot else {
            return;
        };
        let (selected_guild, dm_mode, open_folders) = self
            .with_session(|session| {
                (
                    session.selected_guild,
                    session.dm_mode,
                    session.open_folders.clone(),
                )
            })
            .unwrap_or((None, true, Default::default()));

        // The rail is a pure function of this snapshot and the selection, so
        // a rebuild that would draw the same thing (a read-state or presence
        // event that changed nothing visible here) is skipped. Recreating an
        // avatar per server is what made a busy account's sidebar hitch.
        let fingerprint = {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            entries.hash(&mut hasher);
            for (channel, title, icon, count) in &unread_dms {
                channel.id.hash(&mut hasher);
                title.hash(&mut hasher);
                icon.hash(&mut hasher);
                count.hash(&mut hasher);
            }
            selected_guild.hash(&mut hasher);
            dm_mode.hash(&mut hasher);
            let mut folders: Vec<i64> = open_folders.iter().copied().collect();
            folders.sort_unstable();
            folders.hash(&mut hasher);
            hasher.finish()
        };
        if self.rail_fingerprint.get() == Some(fingerprint) {
            return;
        }
        self.rail_fingerprint.set(Some(fingerprint));

        clear_box(&self.guild_box);

        // Home.
        let home = gtk::Button::new();
        home.add_css_class("kc-rail-button");
        home.add_css_class(if dm_mode { "suggested-action" } else { "flat" });
        home.set_icon_name(first_icon(&["user-home-symbolic", "go-home-symbolic"]));
        home.set_tooltip_text(Some("Direct Messages"));
        let app = self.clone();
        home.connect_clicked(move |_| app.open_home());
        self.guild_box.append(&rail_slot(&home, dm_mode, false, 0));

        for (channel, title, icon, count) in unread_dms {
            let button = gtk::Button::new();
            button.add_css_class("kc-rail-button");
            button.add_css_class("flat");
            button.set_child(Some(&self.images.avatar(ICON, &title, icon.as_deref())));
            button.set_tooltip_text(Some(&title));
            let app = self.clone();
            button.connect_clicked(move |_| app.open_channel(channel.clone()));
            self.guild_box
                .append(&rail_slot(&button, false, true, count));
        }

        let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
        separator.set_margin_start(20);
        separator.set_margin_end(20);
        self.guild_box.append(&separator);

        // Forget cached tiles for servers no longer present, so the cache
        // does not grow without bound.
        let present: std::collections::HashSet<Snowflake> = entries
            .iter()
            .flat_map(|entry| match entry {
                RailEntry::Guild(guild) => vec![guild.id],
                RailEntry::Folder { guilds, .. } => guilds.iter().map(|g| g.id).collect(),
            })
            .collect();
        self.guild_slots
            .borrow_mut()
            .retain(|id, _| present.contains(id));

        for entry in entries {
            match entry {
                RailEntry::Guild(guild) => {
                    let selected = !dm_mode && selected_guild == Some(guild.id);
                    self.guild_box
                        .append(&self.cached_guild_slot(&guild, selected));
                }
                RailEntry::Folder {
                    id,
                    name,
                    color,
                    guilds,
                } => {
                    let open = open_folders.contains(&id);
                    let unread = guilds.iter().any(|guild| guild.unread);
                    let mentions: u32 = guilds.iter().map(|guild| guild.mentions).sum();
                    let contains_selected =
                        !dm_mode && guilds.iter().any(|guild| selected_guild == Some(guild.id));

                    let folder = gtk::Button::new();
                    folder.add_css_class("kc-rail-button");
                    folder.add_css_class("flat");
                    let image = gtk::Image::from_icon_name(if open {
                        "folder-open-symbolic"
                    } else {
                        "folder-symbolic"
                    });
                    image.set_pixel_size(24);
                    if let Some(color) = color.filter(|color| *color != 0) {
                        // Tint the glyph with the folder colour.
                        let provider = gtk::CssProvider::new();
                        provider
                            .load_from_string(&format!("image {{ color: {}; }}", hex_color(color)));
                        #[allow(deprecated)]
                        image
                            .style_context()
                            .add_provider(&provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
                    }
                    folder.set_child(Some(&image));
                    let tooltip = name.unwrap_or_else(|| {
                        guilds
                            .iter()
                            .map(|guild| guild.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    });
                    folder.set_tooltip_text(Some(&tooltip));
                    let app = self.clone();
                    folder.connect_clicked(move |_| {
                        app.with_session_mut(|session| {
                            if !session.open_folders.remove(&id) {
                                session.open_folders.insert(id);
                            }
                        });
                        app.render_guilds();
                    });

                    if open {
                        // The folder background is inset from the rail edges;
                        // the pills of its guilds sit just inside it.
                        let group = gtk::Box::new(gtk::Orientation::Vertical, 8);
                        group.add_css_class("kc-folder");
                        group.set_margin_start(4);
                        group.set_margin_end(4);
                        group.append(&rail_slot(&folder, false, false, 0));
                        for guild in &guilds {
                            let selected = !dm_mode && selected_guild == Some(guild.id);
                            group.append(&self.cached_guild_slot(guild, selected));
                        }
                        self.guild_box.append(&group);
                    } else {
                        self.guild_box.append(&rail_slot(
                            &folder,
                            contains_selected,
                            unread,
                            mentions,
                        ));
                    }
                }
            }
        }

        // Add or join a server.
        let add = gtk::Button::new();
        add.add_css_class("kc-rail-button");
        add.add_css_class("flat");
        add.add_css_class("kc-rail-add");
        add.set_icon_name("list-add-symbolic");
        add.set_tooltip_text(Some("Add a Server"));
        let app = self.clone();
        add.connect_clicked(move |_| app.add_server_dialog());
        self.guild_box.append(&rail_slot(&add, false, false, 0));
    }

    /// A guild tile, reused from the cache when its drawn state is unchanged
    /// so a rail rebuild does not recreate every server's avatar. `clear_box`
    /// has already unparented the cached tiles, so a reused one re-appends
    /// cleanly.
    fn cached_guild_slot(self: &Rc<Self>, guild: &GuildItem, selected: bool) -> gtk::Overlay {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        guild.name.hash(&mut hasher);
        guild.icon.hash(&mut hasher);
        guild.unread.hash(&mut hasher);
        guild.mentions.hash(&mut hasher);
        selected.hash(&mut hasher);
        let key = hasher.finish();
        if let Some((cached_key, overlay)) = self.guild_slots.borrow().get(&guild.id)
            && *cached_key == key
        {
            return overlay.clone();
        }
        let overlay = self.guild_slot(guild, selected);
        self.guild_slots
            .borrow_mut()
            .insert(guild.id, (key, overlay.clone()));
        overlay
    }

    fn guild_slot(self: &Rc<Self>, guild: &GuildItem, selected: bool) -> gtk::Overlay {
        let button = gtk::Button::new();
        button.add_css_class("kc-rail-button");
        button.add_css_class("flat");
        button.set_child(Some(&self.images.avatar(
            ICON,
            &guild.name,
            guild.icon.as_deref(),
        )));
        button.set_tooltip_text(Some(&guild.name));
        let app = self.clone();
        let id = guild.id;
        button.connect_clicked(move |_| app.select_guild(id));

        // Right-click: mark the whole server read.
        let menu = gtk::GestureClick::new();
        menu.set_button(gtk::gdk::BUTTON_SECONDARY);
        let app = self.clone();
        let anchor = button.clone();
        menu.connect_pressed(move |_, _, _, _| {
            let popover = gtk::Popover::new();
            popover.set_parent(&anchor);
            let mark = gtk::Button::with_label("Mark As Read");
            mark.add_css_class("flat");
            let app = app.clone();
            let inner = popover.clone();
            mark.connect_clicked(move |_| {
                inner.popdown();
                app.mark_guild_read(id);
            });
            popover.set_child(Some(&mark));
            popover.connect_closed(|popover| popover.unparent());
            popover.popup();
        });
        button.add_controller(menu);

        rail_slot(&button, selected, guild.unread, guild.mentions)
    }

    /// Acknowledges the newest message of every unread channel in a guild.
    pub(crate) fn mark_guild_read(self: &Rc<Self>, guild_id: Snowflake) {
        let Some((_, http, store)) = self.active() else {
            return;
        };
        let acks: Vec<(Snowflake, Snowflake)> = store.read(|state| {
            state
                .channels
                .values()
                .filter(|channel| channel.guild_id == Some(guild_id))
                .filter(|channel| state.unread(channel.id))
                .filter_map(|channel| Some((channel.id, state.last_message_id(channel.id)?)))
                .collect()
        });
        if acks.is_empty() {
            return;
        }
        for (channel, message) in &acks {
            store.mark_read(*channel, *message);
        }
        self.render_guilds();
        self.render_channels();
        self.spawn(async move { http.ack_bulk(&acks).await }, |app, result| {
            if let Err(error) = result {
                app.set_status(&format!("Could not mark as read: {error}"));
            }
        });
    }
}

/// Centres a rail button and decorates it with the selection/unread pill on
/// the left edge and the mention badge in the corner.
fn rail_slot(button: &gtk::Button, selected: bool, unread: bool, mentions: u32) -> gtk::Overlay {
    button.set_halign(gtk::Align::Center);
    let slot = gtk::Overlay::new();
    slot.set_child(Some(button));

    if selected || unread {
        let pill = gtk::Box::new(gtk::Orientation::Vertical, 0);
        pill.add_css_class("kc-pill");
        pill.set_halign(gtk::Align::Start);
        pill.set_valign(gtk::Align::Center);
        pill.set_height_request(if selected { 36 } else { 8 });
        slot.add_overlay(&pill);
    }
    if mentions > 0 {
        let count = badge(mentions);
        count.set_margin_end(8);
        slot.add_overlay(&count);
    }
    slot
}
