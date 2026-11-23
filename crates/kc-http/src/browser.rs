//! The connection Discord sees: Chrome's, not Rust's.
//!
//! Every client here is built on wreq, which speaks TLS through BoringSSL
//! (the library Chrome uses) and is set up with wreq-util's Chrome profile:
//! the TLS ClientHello (ciphers, extensions and their shuffling, GREASE,
//! key shares, ML-DSA signature algorithms, trust anchor IDs) and the HTTP/2
//! connection (settings, window sizes, pseudo-header order, priority) are
//! Chrome's. A rustls client is easy to tell apart from any browser.
//!
//! Only the connection comes from the profile: the request headers are
//! Kubecord's own ([`crate::client::browser_headers`]), which follow the
//! Chrome version in the user agent.

/// The newest Chrome whose connection wreq-util describes. Chrome 152 added
/// signature-algorithm GREASE and the trust anchors extension, and 154 sorts
/// the anchors; nothing later is known to change the connection, so it
/// stands for the Chrome version in the user agent too.
pub const CONNECTION_PROFILE: wreq_util::Profile = wreq_util::Profile::Chrome154;

/// Chrome's connection, without the profile's default headers (requests
/// carry their own).
pub fn emulation() -> wreq_util::Emulation {
    wreq_util::Emulation::builder()
        .profile(CONNECTION_PROFILE)
        .platform(wreq_util::Platform::Windows)
        .headers(false)
        .build()
}

/// What Chrome accepts, in its words (wreq's own default is worded and
/// ordered differently).
pub const ACCEPT_ENCODING: &str = "gzip, deflate, br, zstd";

/// A client builder with Chrome's connection, user agent and
/// `accept-encoding`. Responses are still decompressed: the decoder reads
/// the response's `content-encoding`, whoever set the request's header.
pub fn client_builder() -> wreq::ClientBuilder {
    let mut headers = wreq::header::HeaderMap::new();
    headers.insert(
        wreq::header::ACCEPT_ENCODING,
        wreq::header::HeaderValue::from_static(ACCEPT_ENCODING),
    );
    wreq::Client::builder()
        .emulation(emulation())
        .user_agent(kc_core::super_properties::user_agent())
        .default_headers(headers)
}

/// Opens a WebSocket (the gateway's, a voice server's) the way a page on
/// discord.com does in Chrome: Chrome's TLS, HTTP/1.1 (Chrome's WebSockets
/// do not use HTTP/2 here), and Chrome's handshake headers in its order and
/// capitalisation. Cloudflare refuses (403) a handshake with lowercase
/// header names.
pub async fn websocket(url: &str) -> Result<wreq::ws::WebSocket, wreq::Error> {
    let client = client_builder().build()?;
    let mut order = wreq::header::OrigHeaderMap::new();
    for name in [
        "Host",
        "Connection",
        "Pragma",
        "Cache-Control",
        "User-Agent",
        "Upgrade",
        "Origin",
        "Sec-WebSocket-Version",
        "Accept-Encoding",
        "Accept-Language",
        "Sec-WebSocket-Key",
        "Sec-WebSocket-Extensions",
    ] {
        order.insert(name);
    }
    client
        .websocket(url)
        .version(wreq::Version::HTTP_11)
        .orig_headers(order)
        .header("pragma", "no-cache")
        .header("cache-control", "no-cache")
        .header("origin", "https://discord.com")
        .header("accept-language", "en-US,en;q=0.9")
        // Chrome offers per-message compression. Discord declines it (its
        // gateway compresses with zlib-stream), so nothing arrives deflated.
        .header("sec-websocket-extensions", "permessage-deflate; client_max_window_bits")
        .send()
        .await?
        .into_websocket()
        .await
}
