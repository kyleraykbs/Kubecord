use gtk::prelude::*;
use libadwaita as adw;

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "kylecord=info,kc_core=info".into()),
        )
        .init();

    let app = adw::Application::builder()
        .application_id("dev.kyle.kylecord")
        .build();

    app.connect_activate(|app| {
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("kylecord")
            .default_width(1100)
            .default_height(720)
            .build();

        let label = gtk::Label::builder()
            .label("kylecord")
            .css_classes(["title-1"])
            .build();

        window.set_content(Some(&label));
        window.present();
    });

    app.run()
}