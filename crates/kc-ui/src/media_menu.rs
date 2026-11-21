//! The right-click menu of images, GIFs and videos: open, copy the link,
//! copy the picture, or save the file. Also the viewer's Save button.

use std::rc::Rc;

use gtk::prelude::*;

use crate::shell::AppState;
use crate::widgets::open_uri;

/// What a piece of media is, for its menu's wording and what it offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MediaType {
    Image,
    Gif,
    Video,
}

impl MediaType {
    fn noun(self) -> &'static str {
        match self {
            MediaType::Image => "Image",
            MediaType::Gif => "GIF",
            MediaType::Video => "Video",
        }
    }
}

/// The menu's subject: the link to open or copy, and the file to save
/// (or copy as a picture), when it has one.
#[derive(Clone, Debug)]
pub(crate) struct MediaLinks {
    pub link: String,
    pub file: Option<String>,
    pub kind: MediaType,
}

/// A file name for a saved URL: its last path segment, query left off.
pub(crate) fn file_name(url: &str, kind: MediaType) -> String {
    let name = url
        .split(['?', '#'])
        .next()
        .and_then(|path| path.rsplit('/').next())
        .filter(|name| !name.is_empty())
        .map(str::to_owned);
    name.unwrap_or_else(|| {
        match kind {
            MediaType::Image => "image.png",
            MediaType::Gif => "animation.gif",
            MediaType::Video => "video.mp4",
        }
        .to_owned()
    })
}

/// The bytes at `url`: a local copy read in place, else downloaded.
async fn bytes_of(url: String) -> Result<Vec<u8>, String> {
    if let Some(path) = crate::images::local_path(&url) {
        return tokio::fs::read(path).await.map_err(|error| error.to_string());
    }
    kc_http::cdn::fetch_to_save(&url).await.map_err(|error| error.to_string())
}

impl AppState {
    /// Gives `widget` the media menu on a right-click.
    pub(crate) fn attach_media_menu(self: &Rc<Self>, widget: &impl IsA<gtk::Widget>, links: MediaLinks) {
        let click = gtk::GestureClick::new();
        click.set_button(gtk::gdk::BUTTON_SECONDARY);
        let app = self.clone();
        let anchor = widget.clone().upcast::<gtk::Widget>().downgrade();
        click.connect_pressed(move |gesture, _, x, y| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            if let Some(anchor) = anchor.upgrade() {
                app.show_media_menu(&anchor, x, y, &links).popup();
            }
        });
        widget.add_controller(click);
    }

    /// The media menu at `(x, y)` of `anchor`, ready to pop up.
    pub(crate) fn show_media_menu(
        self: &Rc<Self>,
        anchor: &gtk::Widget,
        x: f64,
        y: f64,
        links: &MediaLinks,
    ) -> gtk::Popover {
        let popover = gtk::Popover::new();
        // In a window of its own (the viewer), the menu belongs there.
        match anchor.root().and_downcast::<gtk::Window>() {
            Some(window) if window != *self.window.upcast_ref::<gtk::Window>() => {
                let point = anchor
                    .compute_point(&window, &gtk::graphene::Point::new(x as f32, y as f32))
                    .unwrap_or(gtk::graphene::Point::new(x as f32, y as f32));
                popover.set_parent(&window);
                popover.set_has_arrow(false);
                popover.set_halign(gtk::Align::Start);
                popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(point.x() as i32, point.y() as i32, 1, 1)));
                popover.connect_closed(|popover| popover.unparent());
            }
            _ => self.anchor_popover(&popover, anchor, x, y),
        }
        let menu = gtk::Box::new(gtk::Orientation::Vertical, 2);
        menu.add_css_class("kc-menu");
        menu.set_width_request(190);
        let add = |text: &str, action: Box<dyn Fn()>| {
            let item = crate::menu::menu_item(text, false);
            let popover = popover.clone();
            item.connect_clicked(move |_| {
                popover.popdown();
                action();
            });
            menu.append(&item);
        };
        let local = crate::images::local_path(&links.link).is_some();
        let (app, link) = (self.clone(), links.link.clone());
        add(
            if local { "Open File" } else { "Open in Browser" },
            Box::new(move || open_uri(Some(app.window.upcast_ref()), &link)),
        );
        if !local {
            let (app, link) = (self.clone(), links.link.clone());
            add(
                "Copy Link",
                Box::new(move || {
                    app.window.clipboard().set_text(&link);
                    app.tell("Link copied", 3);
                }),
            );
        }
        if let Some(file) = &links.file {
            if links.kind != MediaType::Video {
                let (app, file) = (self.clone(), file.clone());
                add(
                    &format!("Copy {}", if links.kind == MediaType::Gif { "GIF Frame" } else { "Image" }),
                    Box::new(move || app.copy_picture(&file)),
                );
            }
            let (app, file, kind) = (self.clone(), file.clone(), links.kind);
            add(
                &format!("Save {}…", kind.noun()),
                Box::new(move || {
                    let parent: gtk::Window = app.window.clone().upcast();
                    app.save_media(&file, kind, &parent);
                }),
            );
        }
        popover.set_child(Some(&menu));
        popover
    }

    /// Puts the picture at `url` on the clipboard (a GIF's first frame).
    fn copy_picture(self: &Rc<Self>, url: &str) {
        crate::video_setup::spawn_local(self, bytes_of(url.to_owned()), |app, result| {
            let texture = result.and_then(|bytes| {
                gtk::gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes)).map_err(|error| error.to_string())
            });
            match texture {
                Ok(texture) => {
                    app.window.clipboard().set_texture(&texture);
                    app.tell("Image copied", 3);
                }
                Err(error) => app.set_status(&format!("Could not copy the image: {error}")),
            }
        });
    }

    /// Asks where to save the file at `url` (over `parent`), then
    /// downloads it there.
    pub(crate) fn save_media(self: &Rc<Self>, url: &str, kind: MediaType, parent: &gtk::Window) {
        // A dev aid: save there without asking.
        if let Ok(path) = std::env::var("KUBECORD_SAVE_TO") {
            let target = std::path::PathBuf::from(path);
            crate::video_setup::spawn_local(self, bytes_of(url.to_owned()), move |_, bytes| match bytes
                .map_err(|error| error.to_string())
                .and_then(|bytes| std::fs::write(&target, bytes).map_err(|error| error.to_string()))
            {
                Ok(()) => eprintln!("demo: saved {}", target.display()),
                Err(error) => eprintln!("demo: could not save: {error}"),
            });
            return;
        }
        let dialog = gtk::FileDialog::new();
        dialog.set_title(&format!("Save {}", kind.noun()));
        dialog.set_initial_name(Some(&file_name(url, kind)));
        let app = self.clone();
        let url = url.to_owned();
        dialog.save(Some(parent), gtk::gio::Cancellable::NONE, move |result| {
            let Ok(file) = result else {
                return;
            };
            app.set_status(&format!("Saving the {}…", kind.noun().to_lowercase()));
            crate::video_setup::spawn_local(&app, bytes_of(url), move |app, bytes| {
                let bytes = match bytes {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        app.set_status(&format!("Could not save: {error}"));
                        return;
                    }
                };
                let app = app.clone();
                glib::spawn_future_local(async move {
                    let written = file
                        .replace_contents_future(bytes, None, false, gtk::gio::FileCreateFlags::REPLACE_DESTINATION)
                        .await;
                    match written {
                        Ok(_) => app.set_status(&format!("Saved the {}", kind.noun().to_lowercase())),
                        Err((_, error)) => app.set_status(&format!("Could not save: {error}")),
                    }
                });
            });
        });
    }
}

impl AppState {
    /// The demo: the media menu over the viewer's picture, saved as a
    /// picture, then its Save.
    pub(crate) fn demo_media_menu(self: &Rc<Self>, window: &gtk::Window, url: &str) {
        let mut stack: Vec<gtk::Widget> = vec![window.clone().upcast()];
        let mut picture = None;
        while let Some(widget) = stack.pop() {
            if widget.is::<gtk::Picture>() {
                picture = Some(widget);
                break;
            }
            let mut child = widget.first_child();
            while let Some(next) = child {
                child = next.next_sibling();
                stack.push(next);
            }
        }
        let Some(picture) = picture else {
            return;
        };
        let links = MediaLinks {
            link: url.to_owned(),
            file: Some(url.to_owned()),
            kind: MediaType::Image,
        };
        let popover = self.show_media_menu(&picture, 60.0, 60.0, &links);
        popover.set_autohide(false);
        popover.popup();
        if let Ok(path) = std::env::var("KUBECORD_SCREENSHOT") {
            let app = self.clone();
            let url = url.to_owned();
            let window = window.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(700), move || {
                let path = format!("{path}.menu.png");
                if let Err(error) = crate::app::save_window_png(&popover, std::path::Path::new(&path)) {
                    eprintln!("demo: could not save the menu: {error}");
                }
                app.save_media(&url, MediaType::Image, &window);
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_files_are_named_after_the_url() {
        assert_eq!(
            file_name("https://cdn.discordapp.com/attachments/1/2/cat.png?ex=1&hm=2", MediaType::Image),
            "cat.png"
        );
        assert_eq!(file_name("file:///logs/5/9-clip.mp4", MediaType::Video), "9-clip.mp4");
        assert_eq!(file_name("https://example.com/", MediaType::Gif), "animation.gif");
    }
}
