//! In-app playback of uploaded video attachments. Clicking a video opens a
//! separate, non-modal window (the main window stays usable) with GTK's built-in
//! video widget: play/pause, a seek bar and volume. The file is downloaded into
//! the shared media cache first (so it reuses the on-disk cache and the download
//! queue), then played locally.

use std::rc::Rc;

use gtk::prelude::*;
use libadwaita as adw;

use crate::shell::AppState;
use crate::widgets::{icon_button, open_uri};

impl AppState {
    /// Opens `url` (a direct video file) in its own player window, or brings an
    /// open player of it forward. `preview_url` is shown (as a still) while the
    /// video downloads.
    pub(crate) fn show_video(self: &Rc<Self>, url: &str, preview_url: &str) {
        let key = format!("video:{url}");
        if self.present_window(&key) {
            return;
        }

        let video = gtk::Video::new();
        video.set_autoplay(true);
        video.set_loop(false);
        video.set_hexpand(true);
        video.set_vexpand(true);

        // A spinner (over the poster) until the download finishes and playback
        // can start, so the window is not just black while it loads.
        let poster = gtk::Picture::new();
        poster.set_content_fit(gtk::ContentFit::Contain);
        self.images.set_picture(&poster, preview_url);
        let spinner = gtk::Spinner::new();
        spinner.set_halign(gtk::Align::Center);
        spinner.set_valign(gtk::Align::Center);
        spinner.set_size_request(48, 48);
        spinner.start();
        let loading = gtk::Overlay::new();
        loading.set_child(Some(&poster));
        loading.add_overlay(&spinner);

        // The player replaces the loading poster once the first bytes are in.
        let stack = gtk::Stack::new();
        stack.add_named(&loading, Some("loading"));
        stack.add_named(&video, Some("video"));
        stack.set_visible_child_name("loading");

        let header = adw::HeaderBar::new();
        let open = icon_button("web-browser-symbolic", "Open in Browser");
        let link = url.to_owned();
        let parent = self.window.clone();
        open.connect_clicked(move |_| open_uri(Some(parent.upcast_ref()), &link));
        header.pack_start(&open);
        let copy = icon_button("edit-copy-symbolic", "Copy Link");
        let link = url.to_owned();
        let app = self.clone();
        copy.connect_clicked(move |button| {
            button.clipboard().set_text(&link);
            app.tell("Link copied", 3);
        });
        header.pack_start(&copy);
        let save = icon_button("document-save-symbolic", "Save Video");
        let link = url.to_owned();
        let app = self.clone();
        save.connect_clicked(move |button| {
            if let Some(window) = button.root().and_downcast::<gtk::Window>() {
                app.save_media(&link, crate::media_menu::MediaType::Video, &window);
            }
        });
        header.pack_start(&save);

        let view = adw::ToolbarView::new();
        view.add_top_bar(&header);
        view.set_content(Some(&stack));

        let size = (
            (f64::from(self.window.width()) * 0.7) as i32,
            (f64::from(self.window.height()) * 0.7) as i32,
        );
        let title = file_title(url);
        let (window, _) = self.aux_window(&key, &title, size, &view);
        window.present();

        // Download into the cache, then play the local file. GTK's media
        // backend (GStreamer) decodes it; we already rely on that backend for
        // inline GIF-style videos.
        let extension = media_extension(url);
        let weak_video = video.downgrade();
        let weak_stack = stack.downgrade();
        let app = self.clone();
        self.images.file(url, &extension, move |path| {
            let (Some(video), Some(stack)) = (weak_video.upgrade(), weak_stack.upgrade()) else {
                return;
            };
            match path {
                Some(path) => {
                    let media = gtk::MediaFile::for_filename(&path);
                    video.set_media_stream(Some(&media));
                    media.play();
                    stack.set_visible_child_name("video");
                }
                None => app.tell("Could not load that video.", 5),
            }
        });
    }
}

/// A short window title from a media URL: its file name, or "Video".
fn file_title(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    match path.rsplit('/').next() {
        Some(name) if !name.is_empty() => name.to_owned(),
        _ => "Video".to_owned(),
    }
}

/// The file extension to cache a video URL under (so the player can tell the
/// format), defaulting to `mp4`.
fn media_extension(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((_, ext))
            if !ext.is_empty()
                && ext.len() <= 5
                && ext.chars().all(|c| c.is_ascii_alphanumeric()) =>
        {
            ext.to_ascii_lowercase()
        }
        _ => "mp4".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_is_the_file_name() {
        assert_eq!(file_title("https://cdn.example.com/a/b/clip.mp4?x=1"), "clip.mp4");
        assert_eq!(file_title("https://cdn.example.com/"), "Video");
    }

    #[test]
    fn extension_defaults_to_mp4() {
        assert_eq!(media_extension("https://x/y/clip.webm?token=abc"), "webm");
        assert_eq!(media_extension("https://x/y/clip.MOV"), "mov");
        assert_eq!(media_extension("https://x/y/noext"), "mp4");
        assert_eq!(media_extension("https://x/y.long.weirdname"), "mp4");
    }
}
