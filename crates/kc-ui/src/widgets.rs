//! Small widget builders shared by the interface modules.

use gtk::prelude::*;
use libadwaita as adw;

/// Whether the active icon theme provides `name`, so controls can fall back to
/// a text label instead of rendering an empty button.
/// Whether the icon theme in use has `name`.
pub fn icon_available(name: &str) -> bool {
    gtk::gdk::Display::default()
        .map(|display| gtk::IconTheme::for_display(&display).has_icon(name))
        .unwrap_or(false)
}

/// Uses Adwaita's icons when the desktop's icon theme is not installed
/// here. In the Flatpak the desktop's theme (Papirus, say) is usually not in
/// the sandbox, and GTK then draws only the few icons in `hicolor`, leaving
/// most buttons blank. A theme that is installed is left alone. Checked now
/// and whenever the theme setting changes.
pub fn keep_icons_drawable() {
    let Some(settings) = gtk::Settings::default() else {
        return;
    };
    let check = |settings: &gtk::Settings| {
        let Some(display) = gtk::gdk::Display::default() else {
            return;
        };
        let name = settings.gtk_icon_theme_name().map(|name| name.to_string()).unwrap_or_default();
        if name.is_empty() || name == "Adwaita" {
            return;
        }
        let installed = gtk::IconTheme::for_display(&display)
            .search_path()
            .iter()
            .any(|dir| dir.join(&name).join("index.theme").exists());
        if !installed {
            tracing::info!(theme = %name, "icon theme not installed here; using Adwaita's icons");
            settings.set_gtk_icon_theme_name(Some("Adwaita"));
        }
    };
    check(&settings);
    settings.connect_gtk_icon_theme_name_notify(check);
}

/// The first icon of `candidates` the theme has, else the first candidate.
pub fn first_icon<'a>(candidates: &[&'a str]) -> &'a str {
    candidates
        .iter()
        .copied()
        .find(|name| icon_available(name))
        .unwrap_or(candidates[0])
}

pub fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::new();
    button.add_css_class("flat");
    if icon_available(icon) {
        button.set_icon_name(icon);
    } else {
        button.set_label(tooltip);
    }
    button.set_tooltip_text(Some(tooltip));
    button
}

pub fn toggle_icon_button(icon: &str, tooltip: &str) -> gtk::ToggleButton {
    let button = gtk::ToggleButton::new();
    button.add_css_class("flat");
    if icon_available(icon) {
        button.set_icon_name(icon);
    } else {
        button.set_label(tooltip);
    }
    button.set_tooltip_text(Some(tooltip));
    button
}

/// A centred placeholder for an empty pane.
pub fn empty_state(icon: &str, title: &str, description: &str) -> adw::StatusPage {
    let page = adw::StatusPage::new();
    page.set_icon_name(Some(icon));
    page.set_title(title);
    if !description.is_empty() {
        page.set_description(Some(description));
    }
    page.add_css_class("compact");
    page.set_vexpand(true);
    page
}

pub fn clear_box(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

pub fn clear_list(list: &gtk::ListBox) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
}

/// Caps `child` at `width` pixels and keeps it at the start edge.
///
/// `AdwClamp` centres its child, and between its tightening threshold and its
/// maximum it eases the child narrower than the space it was given. With the
/// default threshold, a left-aligned card ends up shifted to the right. Equal
/// threshold and maximum turn both behaviours off.
pub fn max_width(child: &impl IsA<gtk::Widget>, width: i32) -> adw::Clamp {
    let clamp = adw::Clamp::new();
    clamp.set_maximum_size(width);
    clamp.set_tightening_threshold(width);
    clamp.set_halign(gtk::Align::Start);
    clamp.set_child(Some(child));
    clamp
}

/// A left-aligned, ellipsised label.
pub fn label(text: &str, classes: &[&str]) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    for class in classes {
        label.add_css_class(class);
    }
    label
}

/// A red count badge; hidden when the count is zero.
pub fn badge(count: u32) -> gtk::Label {
    let text = if count > 99 {
        "99+".to_owned()
    } else {
        count.to_string()
    };
    let badge = gtk::Label::new(Some(&text));
    badge.add_css_class("kc-badge");
    badge.set_halign(gtk::Align::End);
    badge.set_valign(gtk::Align::End);
    badge.set_visible(count > 0);
    badge
}

/// The coloured presence dot shown on avatars.
pub fn status_dot(status: &str) -> gtk::Box {
    let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    dot.add_css_class("kc-status-dot");
    dot.add_css_class(match status {
        "online" => "kc-status-online",
        "idle" => "kc-status-idle",
        "dnd" => "kc-status-dnd",
        _ => "kc-status-offline",
    });
    dot.set_halign(gtk::Align::End);
    dot.set_valign(gtk::Align::End);
    dot.set_tooltip_text(Some(status_label(status)));
    dot
}

pub fn status_label(status: &str) -> &'static str {
    match status {
        "online" => "Online",
        "idle" => "Idle",
        "dnd" => "Do Not Disturb",
        "invisible" => "Invisible",
        _ => "Offline",
    }
}

/// Wraps an avatar with a presence dot in its corner.
pub fn avatar_with_status(avatar: &adw::Avatar, status: &str) -> gtk::Overlay {
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(avatar));
    overlay.add_overlay(&status_dot(status));
    overlay.set_valign(gtk::Align::Center);
    overlay
}

/// Collapses whitespace and shortens text for one-line previews.
pub fn snippet(text: &str, max_chars: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > max_chars {
        let mut shortened: String = collapsed.chars().take(max_chars).collect();
        shortened.push('…');
        shortened
    } else {
        collapsed
    }
}

/// `#rrggbb` for a Discord RGB integer.
pub fn hex_color(color: u32) -> String {
    format!("#{:06x}", color & 0xff_ffff)
}

/// Opens a URL with the desktop's default handler.
pub fn open_uri(parent: Option<&gtk::Window>, uri: &str) {
    gtk::UriLauncher::new(uri).launch(parent, gtk::gio::Cancellable::NONE, |_| {});
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippets_collapse_and_truncate() {
        assert_eq!(snippet("a\n  b   c", 20), "a b c");
        assert_eq!(snippet("abcdef", 3), "abc…");
    }

    #[test]
    fn colours_format_as_hex() {
        assert_eq!(hex_color(0xff8800), "#ff8800");
        assert_eq!(hex_color(255), "#0000ff");
    }
}

/// A status under a name: its emoji (a server emoji as a small image, a
/// Unicode one as text) then its text, ellipsised. `classes` style the text.
pub fn status_line_widget(
    images: &crate::images::ImageCache,
    line: &kc_state::StatusLine,
    classes: &[&str],
) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    if let Some(emoji) = &line.emoji {
        row.append(&status_emoji_widget(images, emoji, 16));
    }
    if !line.text.is_empty() {
        let text = label(&line.text, classes);
        text.set_hexpand(true);
        row.append(&text);
    }
    let plain = line.plain();
    row.set_tooltip_text((!plain.is_empty()).then_some(plain.as_str()));
    row
}

/// One status emoji at `size` pixels: an image for a server emoji, text for a
/// Unicode one.
pub fn status_emoji_widget(
    images: &crate::images::ImageCache,
    emoji: &kc_state::StatusEmoji,
    size: i32,
) -> gtk::Widget {
    match emoji {
        kc_state::StatusEmoji::Unicode(text) => {
            if let Some(image) = crate::discord_emoji::image(text, size) {
                return image.upcast();
            }
            let glyph = gtk::Label::new(Some(text));
            glyph.set_valign(gtk::Align::Center);
            glyph.upcast()
        }
        kc_state::StatusEmoji::Custom { id, name, animated } => {
            let image = gtk::Image::new();
            image.set_pixel_size(size);
            image.set_valign(gtk::Align::Center);
            image.set_tooltip_text(Some(&format!(":{name}:")));
            images.set_image(
                &image,
                &kc_http::cdn::emoji(*id, *animated, (size * 2) as u32),
            );
            image.upcast()
        }
    }
}
