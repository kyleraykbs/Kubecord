//! GTK4/libadwaita interface for kylecord.

mod account_notice;
pub mod app;
mod captcha;
mod channel_settings;
mod chat;
mod composer;
mod content;
mod demo;
mod friends;
mod images;
mod live;
pub mod logging;
mod markdown;
mod members;
mod menu;
mod moderation;
mod ping;
mod player;
mod profile;
mod rail;
mod rich;
mod server;
mod server_settings;
mod session;
mod settings;
mod settings_panel;
mod shell;
mod sidebar;
mod style;
mod tile_grid;
mod video_info;
mod video_setup;
mod voice;
mod widgets;

pub use app::{build_app, run};
