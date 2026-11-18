//! GTK4/libadwaita interface for kylecord.

pub mod app;
mod markdown;
mod session;
mod settings;

pub use app::{build_app, run};
