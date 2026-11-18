//! GTK4/libadwaita interface for kylecord.

pub mod app;
mod chat;
mod composer;
mod content;
mod friends;
mod images;
mod live;
mod markdown;
mod members;
mod menu;
mod player;
mod rail;
mod rich;
mod server;
mod session;
mod settings;
mod settings_panel;
mod shell;
mod sidebar;
mod style;
mod video_setup;
mod voice;
mod widgets;

pub use app::{build_app, run};
