//! GTK4/libadwaita interface for kylecord.

mod account_notice;
pub mod app;
mod captcha;
mod chat;
mod composer;
mod content;
mod friends;
mod images;
mod live;
pub mod logging;
mod markdown;
mod members;
mod menu;
mod ping;
mod player;
mod profile;
mod rail;
mod rich;
mod server;
mod session;
mod settings;
mod settings_panel;
mod shell;
mod sidebar;
mod style;
mod tile_grid;
mod video_setup;
mod voice;
mod widgets;

pub use app::{build_app, run};
