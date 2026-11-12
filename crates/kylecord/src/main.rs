fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "kylecord=info,kc_core=info,kc_http=info,kc_gateway=info".into()
            }),
        )
        .init();

    kc_ui::run()
}
