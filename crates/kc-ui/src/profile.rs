//! The profile popout, after Discord's full profile: a card with the banner
//! and the avatar over it on the left; activity, mutual friends and mutual
//! servers on the right. In a server it switches between the server profile
//! and the global one.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use serde_json::Value;

use kc_core::Snowflake;
use kc_http::cdn;
use kc_state::{Relationship, permissions};

use crate::content::StateResolver;
use crate::members::friendly_date_ms;
use crate::rich;
use crate::shell::AppState;
use crate::video_setup::spawn_local;
use crate::widgets::{avatar_with_status, clear_box, hex_color};

const BANNER_HEIGHT: i32 = 120;
const AVATAR_SIZE: i32 = 92;
const CARD_WIDTH: i32 = 340;

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Server,
    Global,
}

/// One of the two looks: the global profile, or the server's version.
#[derive(Clone, Default)]
struct Look {
    avatar: String,
    banner: Option<String>,
    color: Option<u32>,
    bio: String,
    pronouns: String,
}

struct Model {
    user_id: Snowflake,
    guild: Option<Snowflake>,
    /// They are a member of `guild`, so it has a server profile for them.
    in_guild: bool,
    guild_name: String,
    guild_icon: Option<String>,
    username: String,
    /// The global display name: `global_name`, else the username.
    global_name: String,
    server_nick: Option<String>,
    friend_nick: Option<String>,
    status: String,
    is_me: bool,
    bot: bool,
    relationship: Option<u8>,
    friends_since: Option<u64>,
    roles: Vec<(String, u32)>,
    created_ms: u64,
    joined_ms: Option<u64>,
    /// Whether we may set their nickname in this server.
    can_server_nick: bool,
    global: Look,
    server: Look,
    note: String,
}

impl Model {
    fn look(&self, view: View) -> &Look {
        match view {
            View::Server if self.has_server_profile() => &self.server,
            _ => &self.global,
        }
    }

    fn has_server_profile(&self) -> bool {
        self.guild.is_some() && self.in_guild
    }

    /// The name at the top of the card for `view`.
    fn shown_name(&self, view: View) -> String {
        let server = (view == View::Server)
            .then(|| self.server_nick.clone())
            .flatten();
        server
            .or_else(|| self.friend_nick.clone())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| self.global_name.clone())
    }
}

struct Mutual {
    id: Snowflake,
    name: String,
    detail: Option<String>,
    icon: Option<String>,
    status: Option<String>,
}

struct Profile {
    app: Rc<AppState>,
    dialog: adw::Dialog,
    card: gtk::Box,
    view: Cell<View>,
    model: RefCell<Model>,
}

impl AppState {
    pub(crate) fn show_profile(self: &Rc<Self>, user_id: Snowflake, _anchor: &gtk::Widget) {
        let Some(model) = self.profile_model(user_id) else {
            return;
        };
        let guild = model.guild;
        let is_me = model.is_me;
        let activities = self.profile_activities(user_id);

        let dialog = adw::Dialog::new();
        dialog.set_title("Profile");
        dialog.add_css_class("kc-profile");
        let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
        card.add_css_class("kc-profile-card");
        card.set_overflow(gtk::Overflow::Hidden);
        card.set_valign(gtk::Align::Start);
        card.set_size_request(CARD_WIDTH, -1);
        let profile = Rc::new(Profile {
            app: self.clone(),
            dialog: dialog.clone(),
            card: card.clone(),
            view: Cell::new(if guild.is_some() {
                View::Server
            } else {
                View::Global
            }),
            model: RefCell::new(model),
        });
        profile.render();

        let card_scroll = gtk::ScrolledWindow::new();
        card_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        card_scroll.set_child(Some(&card));
        card_scroll.set_size_request(CARD_WIDTH, -1);

        // Activity and mutuals; your own profile has no mutuals.
        let stack = adw::ViewStack::new();
        stack.set_vexpand(true);
        if !activities.is_empty() {
            stack.add_titled(
                &scrolled(&activity_list(&activities)),
                Some("activity"),
                "Activity",
            );
        }
        let friends = list_box();
        let servers = list_box();
        if !is_me {
            friends.append(&placeholder("Loading…"));
            servers.append(&placeholder("Loading…"));
            stack.add_titled(&scrolled(&friends), Some("friends"), "Mutual Friends");
            stack.add_titled(&scrolled(&servers), Some("servers"), "Mutual Servers");
        }
        let has_side = !activities.is_empty() || !is_me;
        let switcher = adw::InlineViewSwitcher::new();
        switcher.set_stack(Some(&stack));
        switcher.add_css_class("flat");
        switcher.add_css_class("kc-profile-tabs");
        // Each tab as wide as its title, so none is cut short.
        switcher.set_homogeneous(false);
        switcher.set_halign(gtk::Align::Start);
        let side = gtk::Box::new(gtk::Orientation::Vertical, 10);
        side.set_hexpand(true);
        side.set_margin_top(8);
        side.append(&switcher);
        side.append(&stack);

        let body = gtk::Box::new(gtk::Orientation::Horizontal, 20);
        body.set_margin_top(16);
        body.set_margin_bottom(16);
        body.set_margin_start(16);
        body.set_margin_end(16);
        body.append(&card_scroll);
        if has_side {
            body.append(&side);
        }
        let close = gtk::Button::from_icon_name("window-close-symbolic");
        close.add_css_class("circular");
        close.add_css_class("osd");
        close.set_tooltip_text(Some("Close"));
        close.set_halign(gtk::Align::End);
        close.set_valign(gtk::Align::Start);
        close.set_margin_top(10);
        close.set_margin_end(10);
        let target = dialog.clone();
        close.connect_clicked(move |_| {
            target.close();
        });
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&body));
        overlay.add_overlay(&close);
        dialog.set_child(Some(&overlay));
        dialog.set_content_width(if has_side { 980 } else { CARD_WIDTH + 32 });
        dialog.set_content_height(640);

        // The widgets hold the profile weakly; this keeps it until closed.
        let holder = RefCell::new(Some(profile.clone()));
        dialog.connect_closed(move |_| {
            holder.borrow_mut().take();
        });
        dialog.present(Some(&self.window));

        let Some((_, http, _)) = self.active() else {
            return;
        };
        let weak = Rc::downgrade(&profile);
        spawn_local(
            self,
            async move { http.user_profile(user_id, guild).await },
            move |_, result| {
                let Some(profile) = weak.upgrade() else {
                    return;
                };
                let Ok(data) = result else {
                    clear_box(&friends);
                    clear_box(&servers);
                    friends.append(&placeholder("Could not load the profile"));
                    return;
                };
                profile.apply_profile(&data);
                if !is_me {
                    profile.fill_mutuals(&data, &stack, &friends, &servers);
                }
            },
        );
    }

    /// What the store knows about a user, before the profile request.
    fn profile_model(&self, user_id: Snowflake) -> Option<Model> {
        let guild = self
            .with_session(|session| session.selected_guild.filter(|_| !session.dm_mode))
            .flatten();
        let (_, _, store) = self.active()?;
        store.read(|state| {
            let user = state.users.get(&user_id)?;
            let me = state.my_id();
            let member = guild.and_then(|guild| state.member(guild, user_id));
            let guild_info = guild.and_then(|guild| state.guilds.get(&guild));
            let mut roles: Vec<&kc_state::Role> = member
                .map(|member| {
                    member
                        .roles
                        .iter()
                        .filter_map(|role| state.role(guild?, *role))
                        .collect()
                })
                .unwrap_or_default();
            roles.sort_by_key(|role| std::cmp::Reverse(role.position));
            let relationship = state.relationships.get(&user_id);
            let global_avatar = cdn::user_avatar(user_id, user.avatar.as_deref(), 160);
            let server_avatar = match (guild, member.and_then(|member| member.avatar.as_deref())) {
                (Some(guild), Some(hash)) => cdn::member_avatar(guild, user_id, hash, 160),
                _ => global_avatar.clone(),
            };
            let can_server_nick = match (guild_info, me) {
                (Some(guild_info), Some(me)) => {
                    let my_roles = state
                        .member(guild_info.id, me)
                        .map(|member| member.roles.clone())
                        .unwrap_or_default();
                    let perms = permissions::base(guild_info, me, &my_roles);
                    let top = |roles: &[Snowflake]| {
                        roles
                            .iter()
                            .filter_map(|role| state.role(guild_info.id, *role))
                            .map(|role| role.position)
                            .max()
                            .unwrap_or(0)
                    };
                    if guild_info.owner_id == Some(me) || perms & permissions::ADMINISTRATOR != 0 {
                        true
                    } else if me == user_id {
                        perms & (permissions::CHANGE_NICKNAME | permissions::MANAGE_NICKNAMES) != 0
                    } else {
                        perms & permissions::MANAGE_NICKNAMES != 0
                            && guild_info.owner_id != Some(user_id)
                            && top(&my_roles)
                                > top(member.map(|member| member.roles.as_slice()).unwrap_or(&[]))
                    }
                }
                _ => false,
            };
            Some(Model {
                user_id,
                guild,
                in_guild: member.is_some(),
                guild_name: guild_info
                    .map(|guild| guild.name.clone())
                    .unwrap_or_default(),
                guild_icon: guild_info.and_then(|guild| {
                    guild
                        .icon
                        .as_deref()
                        .map(|hash| cdn::guild_icon(guild.id, hash, 32))
                }),
                username: user.username.clone(),
                global_name: user.display_name().to_owned(),
                server_nick: member.and_then(|member| member.nick.clone()),
                friend_nick: relationship.and_then(|relationship| relationship.nickname.clone()),
                status: state.status_of(user_id).to_owned(),
                is_me: me == Some(user_id),
                bot: user.bot == Some(true),
                relationship: relationship.map(|relationship| relationship.kind),
                friends_since: relationship
                    .filter(|relationship| relationship.kind == Relationship::FRIEND)
                    .and_then(|relationship| relationship.since.as_deref())
                    .and_then(kc_state::time::iso_to_unix_millis),
                roles: roles
                    .into_iter()
                    .map(|role| (role.name.clone(), role.color))
                    .collect(),
                created_ms: user_id.timestamp(),
                joined_ms: member
                    .and_then(|member| member.joined_at.as_deref())
                    .and_then(kc_state::time::iso_to_unix_millis),
                can_server_nick,
                global: Look {
                    avatar: global_avatar,
                    ..Look::default()
                },
                server: Look {
                    avatar: server_avatar,
                    ..Look::default()
                },
                note: String::new(),
            })
        })
    }

    /// The custom status and activities, as (heading, details) pairs.
    fn profile_activities(&self, user_id: Snowflake) -> Vec<(String, String)> {
        let Some((_, _, store)) = self.active() else {
            return Vec::new();
        };
        store.read(|state| {
            let Some(presence) = state.presence(user_id) else {
                return Vec::new();
            };
            let mut out = Vec::new();
            if let Some(custom) = presence.custom_status() {
                out.push(("Custom Status".to_owned(), custom));
            }
            for activity in &presence.activities {
                let kind = activity["type"].as_u64().unwrap_or(0);
                if kind == 4 {
                    continue;
                }
                let Some(name) = activity["name"].as_str() else {
                    continue;
                };
                let verb = match kind {
                    1 => "Streaming",
                    2 => "Listening to",
                    3 => "Watching",
                    5 => "Competing in",
                    _ => "Playing",
                };
                let details = [activity["details"].as_str(), activity["state"].as_str()]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join("\n");
                out.push((format!("{verb} {name}"), details));
            }
            out
        })
    }
}

impl Profile {
    /// Rebuilds the card for the current view.
    fn render(self: &Rc<Self>) {
        clear_box(&self.card);
        let model = self.model.borrow();
        let view = self.view.get();
        self.card.append(&self.header(&model, view));
        self.card.append(&self.body(&model, view));
    }

    fn header(self: &Rc<Self>, model: &Model, view: View) -> gtk::Overlay {
        let look = model.look(view);
        let banner = gtk::Overlay::new();
        let fill = gtk::DrawingArea::new();
        fill.set_content_height(BANNER_HEIGHT);
        let (red, green, blue) = rgb(look.color.unwrap_or_else(|| fallback_color(model.user_id)));
        fill.set_draw_func(move |_, cairo, _, _| {
            cairo.set_source_rgb(red, green, blue);
            let _ = cairo.paint();
        });
        banner.set_child(Some(&fill));
        if let Some(url) = &look.banner {
            let picture = gtk::Picture::new();
            picture.set_content_fit(gtk::ContentFit::Cover);
            picture.set_can_shrink(true);
            self.app.images.set_picture(&picture, url);
            banner.add_overlay(&picture);
        }
        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        column.append(&banner);
        // Room under the banner for the lower half of the avatar.
        let room = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        room.set_size_request(-1, AVATAR_SIZE / 2 + 8);
        column.append(&room);

        let header = gtk::Overlay::new();
        header.set_child(Some(&column));
        let avatar =
            self.app
                .images
                .avatar(AVATAR_SIZE, &model.shown_name(view), Some(&look.avatar));
        avatar.add_css_class("kc-profile-avatar");
        let ring = avatar_with_status(&avatar, &model.status);
        ring.set_halign(gtk::Align::Start);
        ring.set_valign(gtk::Align::Start);
        ring.set_margin_start(16);
        ring.set_margin_top(BANNER_HEIGHT - AVATAR_SIZE / 2);
        header.add_overlay(&ring);

        // Server or global profile, beside the avatar.
        if model.has_server_profile() {
            let toggles = adw::ToggleGroup::new();
            for (name, title) in [("server", "Server"), ("global", "Global")] {
                let toggle = adw::Toggle::new();
                toggle.set_name(Some(name));
                toggle.set_label(Some(title));
                toggles.add(toggle);
            }
            toggles.set_active_name(Some(if view == View::Server {
                "server"
            } else {
                "global"
            }));
            toggles.set_tooltip_text(Some(&format!(
                "Server: how {} shows them. Global: their profile everywhere else.",
                model.guild_name
            )));
            toggles.set_halign(gtk::Align::End);
            toggles.set_valign(gtk::Align::End);
            toggles.set_margin_end(14);
            toggles.set_margin_bottom(4);
            let weak = Rc::downgrade(self);
            toggles.connect_active_name_notify(move |toggles| {
                let view = if toggles.active_name().as_deref() == Some("global") {
                    View::Global
                } else {
                    View::Server
                };
                // Rebuild after the signal: the card holds this widget.
                let weak = weak.clone();
                glib::idle_add_local_once(move || {
                    if let Some(profile) = weak.upgrade()
                        && profile.view.get() != view
                    {
                        profile.view.set(view);
                        profile.render();
                    }
                });
            });
            header.add_overlay(&toggles);
        }
        header
    }

    fn body(self: &Rc<Self>, model: &Model, view: View) -> gtk::Box {
        let look = model.look(view);
        let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
        body.set_margin_start(18);
        body.set_margin_end(18);
        body.set_margin_bottom(18);

        // The name shown, then the original names so they are never lost.
        let shown = model.shown_name(view);
        let title = wrapped(&shown, &["title-2"]);
        title.set_selectable(true);
        body.append(&title);
        // The username line below already covers a display name that is
        // just the username.
        if shown != model.global_name && model.global_name != model.username {
            let global = wrapped(&model.global_name, &["dim-label"]);
            global.set_selectable(true);
            global.set_tooltip_text(Some("Their display name"));
            body.append(&global);
        }
        let mut handle = model.username.clone();
        if !look.pronouns.is_empty() {
            handle.push_str(" • ");
            handle.push_str(&look.pronouns);
        }
        let handle = wrapped(&handle, &["caption"]);
        handle.set_selectable(true);
        body.append(&handle);
        if model.bot {
            let tag = gtk::Label::new(Some("APP"));
            tag.add_css_class("kc-tag");
            tag.set_halign(gtk::Align::Start);
            body.append(&tag);
        }

        body.append(&self.actions(model));

        if !look.bio.is_empty() {
            let bio = gtk::Label::new(None);
            bio.set_xalign(0.0);
            bio.set_wrap(true);
            bio.set_wrap_mode(gtk::pango::WrapMode::WordChar);
            bio.set_selectable(true);
            bio.set_margin_top(6);
            let markup = self.app.active().map(|(_, _, store)| {
                store.read(|state| {
                    rich::to_markup(
                        &rich::parse(&look.bio),
                        &StateResolver {
                            state,
                            guild: model.guild,
                        },
                        true,
                    )
                })
            });
            match markup {
                Some(markup) => bio.set_markup(&markup),
                None => bio.set_label(&look.bio),
            }
            body.append(&bio);
        }

        body.append(&heading("Member Since"));
        let since = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let created = wrapped(&friendly_date_ms(model.created_ms), &[]);
        created.set_tooltip_text(Some("Joined Discord"));
        since.append(&created);
        if let (View::Server, Some(joined)) = (view, model.joined_ms) {
            since.append(&gtk::Label::new(Some("•")));
            let icon = self
                .app
                .images
                .avatar(18, &model.guild_name, model.guild_icon.as_deref());
            since.append(&icon);
            let date = gtk::Label::new(Some(&friendly_date_ms(joined)));
            date.set_tooltip_text(Some(&format!("Joined {}", model.guild_name)));
            since.append(&date);
        }
        body.append(&since);

        if let Some(friends) = model.friends_since {
            body.append(&heading("Friends Since"));
            body.append(&wrapped(&friendly_date_ms(friends), &[]));
        }

        if view == View::Server && !model.roles.is_empty() {
            body.append(&heading("Roles"));
            let wrap = adw::WrapBox::new();
            wrap.set_child_spacing(4);
            wrap.set_line_spacing(4);
            for (role, color) in &model.roles {
                let chip = gtk::Label::new(None);
                chip.add_css_class("kc-reaction");
                let escaped = glib::markup_escape_text(role);
                if *color != 0 {
                    chip.set_markup(&format!(
                        "<span foreground=\"{}\">●</span> {escaped}",
                        hex_color(*color)
                    ));
                } else {
                    chip.set_markup(&escaped);
                }
                wrap.append(&chip);
            }
            body.append(&wrap);
        }

        if !model.is_me {
            body.append(&heading("Note (only visible to you)"));
            let note = gtk::Entry::new();
            note.set_placeholder_text(Some("Click to add a note"));
            note.add_css_class("kc-note");
            note.set_text(&model.note);
            let weak = Rc::downgrade(self);
            let user_id = model.user_id;
            note.connect_activate(move |note| {
                let Some(profile) = weak.upgrade() else {
                    return;
                };
                let text = note.text().to_string();
                profile.model.borrow_mut().note = text.clone();
                let Some((_, http, _)) = profile.app.active() else {
                    return;
                };
                profile.app.spawn(
                    async move { http.set_note(user_id, &text).await },
                    |app, result| match result {
                        Ok(()) => app.set_status("Note saved"),
                        Err(error) => app.set_status(&format!("Could not save note: {error}")),
                    },
                );
            });
            body.append(&note);
        }
        body
    }

    /// Message, friend, nickname and the "more" menu.
    fn actions(self: &Rc<Self>, model: &Model) -> gtk::Box {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.set_margin_top(6);
        let user_id = model.user_id;

        if model.is_me {
            let edit = gtk::Button::with_label("Edit Profile");
            edit.add_css_class("suggested-action");
            let weak = Rc::downgrade(self);
            edit.connect_clicked(move |_| {
                if let Some(profile) = weak.upgrade() {
                    profile.dialog.close();
                    profile.app.edit_profile_dialog();
                }
            });
            row.append(&edit);
        } else {
            let message = gtk::Button::with_label("Message");
            message.add_css_class("suggested-action");
            let weak = Rc::downgrade(self);
            message.connect_clicked(move |_| {
                if let Some(profile) = weak.upgrade() {
                    profile.dialog.close();
                    profile.app.open_dm_with(user_id);
                }
            });
            row.append(&message);
            // Start focus on Message, not a selectable label (which would
            // select its text).
            self.dialog.set_focus(Some(&message));
        }

        // Add, accept or cancel a friend request.
        if !model.is_me && !model.bot {
            let friend = match model.relationship {
                None => Some(("contact-new-symbolic", "Add Friend")),
                Some(Relationship::INCOMING_REQUEST) => {
                    Some(("emblem-ok-symbolic", "Accept Friend Request"))
                }
                Some(Relationship::OUTGOING_REQUEST) => {
                    Some(("process-stop-symbolic", "Cancel Friend Request"))
                }
                _ => None,
            };
            if let Some((icon, tooltip)) = friend {
                let button = gtk::Button::from_icon_name(icon);
                button.set_tooltip_text(Some(tooltip));
                let weak = Rc::downgrade(self);
                let current = model.relationship;
                button.connect_clicked(move |_| {
                    let Some(profile) = weak.upgrade() else {
                        return;
                    };
                    let next = if current == Some(Relationship::OUTGOING_REQUEST) {
                        profile.app.remove_relationship(user_id);
                        None
                    } else {
                        profile.app.befriend_user(user_id);
                        Some(if current == Some(Relationship::INCOMING_REQUEST) {
                            Relationship::FRIEND
                        } else {
                            Relationship::OUTGOING_REQUEST
                        })
                    };
                    profile.model.borrow_mut().relationship = next;
                    profile.rerender();
                });
                row.append(&button);
            }
        }

        // Nicknames: a private one for friends, the server one if allowed.
        let friend = model.relationship == Some(Relationship::FRIEND);
        if friend || model.can_server_nick {
            let button = gtk::Button::from_icon_name("document-edit-symbolic");
            button.set_tooltip_text(Some("Edit Nickname"));
            let weak = Rc::downgrade(self);
            button.connect_clicked(move |_| {
                if let Some(profile) = weak.upgrade() {
                    profile.edit_nicknames();
                }
            });
            row.append(&button);
        }

        let more = gtk::MenuButton::new();
        more.set_icon_name("view-more-symbolic");
        more.set_tooltip_text(Some("More"));
        more.set_popover(Some(&self.more_menu(model)));
        row.append(&more);
        row
    }

    fn more_menu(self: &Rc<Self>, model: &Model) -> gtk::Popover {
        let popover = gtk::Popover::new();
        let items = gtk::Box::new(gtk::Orientation::Vertical, 0);
        items.add_css_class("kc-menu");
        popover.set_child(Some(&items));
        let user_id = model.user_id;
        type Action = Box<dyn Fn(&Rc<Profile>)>;
        let mut entries: Vec<(&str, bool, Action)> = Vec::new();
        if !model.is_me && !model.bot {
            entries.push((
                "Start a Call",
                false,
                Box::new(move |profile| {
                    profile.dialog.close();
                    profile.app.start_call_with(user_id);
                }),
            ));
        }
        if model.relationship == Some(Relationship::FRIEND) {
            entries.push((
                "Remove Friend",
                true,
                Box::new(move |profile| {
                    profile.app.remove_relationship(user_id);
                    profile.model.borrow_mut().relationship = None;
                    profile.rerender();
                }),
            ));
        }
        if !model.is_me {
            if model.relationship == Some(Relationship::BLOCKED) {
                entries.push((
                    "Unblock",
                    false,
                    Box::new(move |profile| {
                        profile.app.remove_relationship(user_id);
                        profile.model.borrow_mut().relationship = None;
                        profile.rerender();
                    }),
                ));
            } else {
                entries.push((
                    "Block",
                    true,
                    Box::new(move |profile| {
                        profile.app.block_user(user_id);
                        profile.model.borrow_mut().relationship = Some(Relationship::BLOCKED);
                        profile.rerender();
                    }),
                ));
            }
        }
        let username = model.username.clone();
        entries.push((
            "Copy Username",
            false,
            Box::new(move |profile| {
                profile.app.window.clipboard().set_text(&username);
                profile.app.set_status("Username copied");
            }),
        ));
        entries.push((
            "Copy User ID",
            false,
            Box::new(move |profile| {
                profile
                    .app
                    .window
                    .clipboard()
                    .set_text(&user_id.to_string());
                profile.app.set_status("User ID copied");
            }),
        ));
        for (title, danger, action) in entries {
            let button = gtk::Button::with_label(title);
            button.add_css_class("flat");
            if danger {
                button.add_css_class("kc-danger");
            }
            if let Some(label) = button.child().and_downcast::<gtk::Label>() {
                label.set_xalign(0.0);
            }
            let weak = Rc::downgrade(self);
            let menu = popover.clone();
            button.connect_clicked(move |_| {
                menu.popdown();
                if let Some(profile) = weak.upgrade() {
                    action(&profile);
                }
            });
            items.append(&button);
        }
        popover
    }

    /// Re-renders once the current signal has finished.
    fn rerender(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(profile) = weak.upgrade() {
                profile.render();
            }
        });
    }

    /// Friend nickname (private, for friends) and server nickname (if
    /// allowed), in one dialog.
    fn edit_nicknames(self: &Rc<Self>) {
        let model = self.model.borrow();
        let friend = model.relationship == Some(Relationship::FRIEND);
        let dialog = adw::AlertDialog::new(
            Some("Nicknames"),
            Some(&format!(
                "{} is @{}. Leave a field empty to clear it.",
                model.global_name, model.username
            )),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("save", "Save")]);
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("save"));
        dialog.set_close_response("cancel");

        let fields = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let friend_entry = gtk::Entry::new();
        if friend {
            fields.append(&heading("Friend Nickname"));
            let hint = wrapped(
                "Only you see this, wherever they appear.",
                &["caption", "dim-label"],
            );
            fields.append(&hint);
            friend_entry.set_text(model.friend_nick.as_deref().unwrap_or(""));
            friend_entry.set_placeholder_text(Some(&model.global_name));
            fields.append(&friend_entry);
        }
        let server_entry = gtk::Entry::new();
        if model.can_server_nick && model.guild.is_some() {
            fields.append(&heading("Server Nickname"));
            let hint = wrapped(
                &format!("Everyone in {} sees this.", model.guild_name),
                &["caption", "dim-label"],
            );
            fields.append(&hint);
            server_entry.set_text(model.server_nick.as_deref().unwrap_or(""));
            server_entry.set_placeholder_text(Some(&model.global_name));
            fields.append(&server_entry);
        }
        dialog.set_extra_child(Some(&fields));

        let old_friend = model.friend_nick.clone().unwrap_or_default();
        let old_server = model.server_nick.clone().unwrap_or_default();
        let can_server = model.can_server_nick;
        let guild = model.guild;
        let user_id = model.user_id;
        let is_me = model.is_me;
        drop(model);
        let weak = Rc::downgrade(self);
        dialog.connect_response(None, move |_, response| {
            if response != "save" {
                return;
            }
            let Some(profile) = weak.upgrade() else {
                return;
            };
            let Some((_, http, _)) = profile.app.active() else {
                return;
            };
            let friend_nick = friend_entry.text().trim().to_owned();
            if friend && friend_nick != old_friend {
                let http = http.clone();
                let nick = friend_nick.clone();
                let weak = weak.clone();
                spawn_local(
                    &profile.app,
                    async move { http.set_friend_nickname(user_id, &nick).await },
                    move |app, result| match result {
                        Ok(()) => {
                            if let Some(profile) = weak.upgrade() {
                                profile.model.borrow_mut().friend_nick =
                                    (!friend_nick.is_empty()).then_some(friend_nick);
                                profile.render();
                            }
                        }
                        Err(error) => {
                            app.set_status(&format!("Could not set the friend nickname: {error}"))
                        }
                    },
                );
            }
            let server_nick = server_entry.text().trim().to_owned();
            if let (true, Some(guild)) = (can_server, guild)
                && server_nick != old_server
            {
                let nick = server_nick.clone();
                let weak = weak.clone();
                spawn_local(
                    &profile.app,
                    async move { http.set_nickname(guild, user_id, is_me, &nick).await },
                    move |app, result| match result {
                        Ok(()) => {
                            if let Some(profile) = weak.upgrade() {
                                profile.model.borrow_mut().server_nick =
                                    (!server_nick.is_empty()).then_some(server_nick);
                                profile.render();
                            }
                        }
                        Err(error) => {
                            app.set_status(&format!("Could not set the server nickname: {error}"))
                        }
                    },
                );
            }
        });
        dialog.present(Some(&self.dialog));
    }

    /// Takes banners, colours, bios, pronouns and the note from the profile
    /// response, then shows them.
    fn apply_profile(self: &Rc<Self>, data: &Value) {
        {
            let mut model = self.model.borrow_mut();
            let user_id = model.user_id;
            let global = Look {
                avatar: model.global.avatar.clone(),
                banner: text_at(data, "/user_profile/banner")
                    .or_else(|| text_at(data, "/user/banner"))
                    .map(|hash| cdn::user_banner(user_id, hash, 600)),
                color: color_at(data, "/user_profile")
                    .or_else(|| number_at(data, "/user/accent_color")),
                bio: text_at(data, "/user_profile/bio")
                    .or_else(|| text_at(data, "/user/bio"))
                    .unwrap_or("")
                    .to_owned(),
                pronouns: text_at(data, "/user_profile/pronouns")
                    .unwrap_or("")
                    .to_owned(),
            };
            if let Some(guild) = model.guild {
                model.server = Look {
                    avatar: text_at(data, "/guild_member/avatar")
                        .map(|hash| cdn::member_avatar(guild, user_id, hash, 160))
                        .unwrap_or_else(|| model.server.avatar.clone()),
                    banner: text_at(data, "/guild_member_profile/banner")
                        .or_else(|| text_at(data, "/guild_member/banner"))
                        .map(|hash| cdn::member_banner(guild, user_id, hash, 600))
                        .or_else(|| global.banner.clone()),
                    color: color_at(data, "/guild_member_profile").or(global.color),
                    bio: text_at(data, "/guild_member_profile/bio")
                        .map(str::to_owned)
                        .unwrap_or_else(|| global.bio.clone()),
                    pronouns: text_at(data, "/guild_member_profile/pronouns")
                        .map(str::to_owned)
                        .unwrap_or_else(|| global.pronouns.clone()),
                };
                if data
                    .get("guild_member")
                    .is_some_and(|member| !member.is_null())
                {
                    model.in_guild = true;
                }
                if let Some(nick) = text_at(data, "/guild_member/nick") {
                    model.server_nick = Some(nick.to_owned());
                }
                if let Some(joined) = text_at(data, "/guild_member/joined_at")
                    .and_then(kc_state::time::iso_to_unix_millis)
                {
                    model.joined_ms = Some(joined);
                }
            } else {
                model.server = global.clone();
            }
            model.global = global;
            if let Some(note) = text_at(data, "/notes/note").or_else(|| text_at(data, "/note/note"))
            {
                model.note = note.to_owned();
            }
        }
        self.render();
    }

    fn fill_mutuals(
        self: &Rc<Self>,
        data: &Value,
        stack: &adw::ViewStack,
        friends: &gtk::Box,
        servers: &gtk::Box,
    ) {
        let Some((_, _, store)) = self.app.active() else {
            return;
        };
        let mutual_friends: Vec<Mutual> = data["mutual_friends"]
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(|user| {
                        let id = user["id"].as_str()?.parse::<Snowflake>().ok()?;
                        let name = user["global_name"]
                            .as_str()
                            .filter(|name| !name.is_empty())
                            .or_else(|| user["username"].as_str())?
                            .to_owned();
                        let (status, detail) = store.read(|state| {
                            (
                                state.status_of(id).to_owned(),
                                state.presence(id).and_then(|presence| presence.summary()),
                            )
                        });
                        Some(Mutual {
                            id,
                            name,
                            detail,
                            icon: Some(cdn::user_avatar(id, user["avatar"].as_str(), 80)),
                            status: Some(status),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mutual_servers: Vec<Mutual> = data["mutual_guilds"]
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(|entry| {
                        let id = entry["id"].as_str()?.parse::<Snowflake>().ok()?;
                        let (name, icon) = store.read(|state| {
                            let guild = state.guilds.get(&id)?;
                            Some((
                                guild.name.clone(),
                                guild
                                    .icon
                                    .as_deref()
                                    .map(|hash| cdn::guild_icon(id, hash, 80)),
                            ))
                        })?;
                        Some(Mutual {
                            id,
                            name,
                            detail: entry["nick"]
                                .as_str()
                                .map(|nick| format!("Nickname: {nick}")),
                            icon,
                            status: None,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let count = |count: usize, one: &str, many: &str| match count {
            0 => many.to_owned(),
            1 => format!("1 {one}"),
            count => format!("{count} {many}"),
        };
        if let Some(child) = friends.parent().and_then(|parent| parent.parent()) {
            stack.page(&child).set_title(Some(&count(
                mutual_friends.len(),
                "Mutual Friend",
                "Mutual Friends",
            )));
        }
        if let Some(child) = servers.parent().and_then(|parent| parent.parent()) {
            stack.page(&child).set_title(Some(&count(
                mutual_servers.len(),
                "Mutual Server",
                "Mutual Servers",
            )));
        }

        clear_box(friends);
        if mutual_friends.is_empty() {
            friends.append(&placeholder("No friends in common"));
        }
        for friend in mutual_friends {
            let row = self.mutual_row(&friend);
            let weak = Rc::downgrade(self);
            let id = friend.id;
            row.connect_clicked(move |_| {
                if let Some(profile) = weak.upgrade() {
                    profile.dialog.close();
                    let anchor = profile.app.window.clone().upcast::<gtk::Widget>();
                    profile.app.show_profile(id, &anchor);
                }
            });
            self.app.attach_user_menu(&row, friend.id);
            friends.append(&row);
        }
        clear_box(servers);
        if mutual_servers.is_empty() {
            servers.append(&placeholder("No servers in common"));
        }
        for server in mutual_servers {
            let row = self.mutual_row(&server);
            let weak = Rc::downgrade(self);
            let id = server.id;
            row.connect_clicked(move |_| {
                if let Some(profile) = weak.upgrade() {
                    profile.dialog.close();
                    profile.app.select_guild(id);
                }
            });
            servers.append(&row);
        }
    }

    fn mutual_row(&self, mutual: &Mutual) -> gtk::Button {
        let avatar = self
            .app
            .images
            .avatar(40, &mutual.name, mutual.icon.as_deref());
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        match &mutual.status {
            Some(status) => row.append(&avatar_with_status(&avatar, status)),
            None => row.append(&avatar),
        }
        let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
        text.set_valign(gtk::Align::Center);
        let name = gtk::Label::new(Some(&mutual.name));
        name.set_xalign(0.0);
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        text.append(&name);
        if let Some(detail) = &mutual.detail {
            let detail = gtk::Label::new(Some(detail));
            detail.set_xalign(0.0);
            detail.set_ellipsize(gtk::pango::EllipsizeMode::End);
            detail.add_css_class("caption");
            detail.add_css_class("dim-label");
            text.append(&detail);
        }
        row.append(&text);
        let button = gtk::Button::new();
        button.add_css_class("flat");
        button.add_css_class("kc-profile-row");
        button.set_child(Some(&row));
        button
    }
}

// -- pieces ------------------------------------------------------------------

fn activity_list(activities: &[(String, String)]) -> gtk::Box {
    let list = list_box();
    for (title, details) in activities {
        let item = gtk::Box::new(gtk::Orientation::Vertical, 2);
        item.add_css_class("card");
        let inner = gtk::Box::new(gtk::Orientation::Vertical, 2);
        inner.set_margin_top(10);
        inner.set_margin_bottom(10);
        inner.set_margin_start(12);
        inner.set_margin_end(12);
        inner.append(&wrapped(title, &["heading"]));
        if !details.is_empty() {
            inner.append(&wrapped(details, &["dim-label"]));
        }
        item.append(&inner);
        list.append(&item);
    }
    list
}

fn list_box() -> gtk::Box {
    let list = gtk::Box::new(gtk::Orientation::Vertical, 4);
    list.set_valign(gtk::Align::Start);
    list
}

fn scrolled(child: &gtk::Box) -> gtk::ScrolledWindow {
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroll.set_vexpand(true);
    scroll.set_child(Some(child));
    scroll
}

fn placeholder(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.add_css_class("dim-label");
    label.set_margin_top(24);
    label
}

fn heading(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.add_css_class("kc-profile-heading");
    label.set_margin_top(6);
    label
}

fn wrapped(text: &str, classes: &[&str]) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    for class in classes {
        label.add_css_class(class);
    }
    label
}

fn text_at<'a>(data: &'a Value, pointer: &str) -> Option<&'a str> {
    data.pointer(pointer)?
        .as_str()
        .filter(|text| !text.is_empty())
}

fn number_at(data: &Value, pointer: &str) -> Option<u32> {
    data.pointer(pointer)?
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
}

/// A profile's banner colour: the first theme colour, else the accent.
fn color_at(data: &Value, profile: &str) -> Option<u32> {
    number_at(data, &format!("{profile}/theme_colors/0"))
        .or_else(|| number_at(data, &format!("{profile}/accent_color")))
}

fn rgb(color: u32) -> (f64, f64, f64) {
    (
        f64::from((color >> 16) & 0xff) / 255.0,
        f64::from((color >> 8) & 0xff) / 255.0,
        f64::from(color & 0xff) / 255.0,
    )
}

/// A muted colour picked from the user id, for profiles without a banner
/// or colour of their own.
fn fallback_color(user_id: Snowflake) -> u32 {
    const PALETTE: [u32; 6] = [0x3b4a6b, 0x4a3b6b, 0x3b6b5a, 0x6b4a3b, 0x5a3b4a, 0x3b5a6b];
    PALETTE[(user_id.0 % PALETTE.len() as u64) as usize]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn server_profile_falls_back_to_the_global_one() {
        let data = json!({
            "user": { "banner": "abc", "accent_color": 0x112233 },
            "user_profile": { "bio": "global bio", "pronouns": "she/her", "theme_colors": [0x445566, 1] },
            "guild_member_profile": { "bio": "", "pronouns": "they/them" },
            "guild_member": { "nick": "Nick" }
        });
        assert_eq!(text_at(&data, "/guild_member_profile/bio"), None);
        assert_eq!(text_at(&data, "/user_profile/bio"), Some("global bio"));
        assert_eq!(color_at(&data, "/user_profile"), Some(0x445566));
        assert_eq!(color_at(&data, "/guild_member_profile"), None);
        assert_eq!(number_at(&data, "/user/accent_color"), Some(0x112233));
        assert_eq!(rgb(0xff8000), (1.0, 128.0 / 255.0, 0.0));
    }
}
