//! Links rlottie (Lottie stickers) from the system, found with pkg-config.

fn main() {
    if let Err(error) = pkg_config::Config::new().probe("rlottie") {
        panic!("rlottie not found (install rlottie / librlottie-dev): {error}");
    }
}
