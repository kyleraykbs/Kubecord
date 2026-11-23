//! Message search, as the web client asks for it.
//!
//! `GET /guilds/{id}/messages/search` (a whole server, or some of its
//! channels) and `GET /channels/{id}/messages/search` (a DM or group DM) take
//! the same filters as query parameters. Their names and values are the web
//! client's (from a capture of each filter, and its filter table): several
//! values of one filter repeat the parameter and match any of them.

use kc_core::Snowflake;

/// Messages returned per page.
pub const PAGE: u32 = 25;

/// What a message contains (`has`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Has {
    Link,
    Embed,
    File,
    Image,
    Video,
    Sound,
    Sticker,
    Poll,
    /// A forwarded message.
    Forward,
}

impl Has {
    pub const ALL: [Has; 9] = [
        Has::Link,
        Has::Embed,
        Has::File,
        Has::Image,
        Has::Video,
        Has::Sound,
        Has::Sticker,
        Has::Poll,
        Has::Forward,
    ];

    /// The parameter's value.
    pub const fn value(self) -> &'static str {
        match self {
            Has::Link => "link",
            Has::Embed => "embed",
            Has::File => "file",
            Has::Image => "image",
            Has::Video => "video",
            Has::Sound => "sound",
            Has::Sticker => "sticker",
            Has::Poll => "poll",
            Has::Forward => "snapshot",
        }
    }

    /// The name shown, and typed after `has:`.
    pub const fn label(self) -> &'static str {
        match self {
            Has::Link => "Link",
            Has::Embed => "Embed",
            Has::File => "File",
            Has::Image => "Image",
            Has::Video => "Video",
            Has::Sound => "Sound",
            Has::Sticker => "Sticker",
            Has::Poll => "Poll",
            Has::Forward => "Forward",
        }
    }

    /// The kind named by `word` (`image`, `forward`, `snapshot`, ...).
    pub fn named(word: &str) -> Option<Has> {
        let word = word.trim().to_ascii_lowercase();
        let word = word.trim_end_matches('s');
        Has::ALL
            .into_iter()
            .find(|has| has.value() == word || has.label().eq_ignore_ascii_case(word))
            .or(match word {
                "attachment" => Some(Has::File),
                "audio" => Some(Has::Sound),
                "picture" | "photo" | "img" => Some(Has::Image),
                "url" => Some(Has::Link),
                _ => None,
            })
    }
}

/// Who wrote a message (`author_type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AuthorType {
    User,
    Bot,
    Webhook,
}

impl AuthorType {
    pub const ALL: [AuthorType; 3] = [AuthorType::User, AuthorType::Bot, AuthorType::Webhook];

    pub const fn value(self) -> &'static str {
        match self {
            AuthorType::User => "user",
            AuthorType::Bot => "bot",
            AuthorType::Webhook => "webhook",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            AuthorType::User => "User",
            AuthorType::Bot => "Bot",
            AuthorType::Webhook => "Webhook",
        }
    }

    pub fn named(word: &str) -> Option<AuthorType> {
        let word = word.trim().to_ascii_lowercase();
        let word = word.trim_end_matches('s');
        AuthorType::ALL
            .into_iter()
            .find(|kind| kind.value() == word)
            .or(match word {
                "person" | "human" | "people" => Some(AuthorType::User),
                "app" => Some(AuthorType::Bot),
                _ => None,
            })
    }
}

/// The order of the results.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sort {
    #[default]
    Newest,
    Oldest,
    Relevant,
}

impl Sort {
    pub const ALL: [Sort; 3] = [Sort::Newest, Sort::Oldest, Sort::Relevant];

    pub const fn label(self) -> &'static str {
        match self {
            Sort::Newest => "Newest",
            Sort::Oldest => "Oldest",
            Sort::Relevant => "Most Relevant",
        }
    }

    const fn params(self) -> &'static str {
        match self {
            Sort::Newest => "sort_by=timestamp&sort_order=desc",
            Sort::Oldest => "sort_by=timestamp&sort_order=asc",
            Sort::Relevant => "sort_by=relevance&sort_order=desc",
        }
    }
}

/// The web client's result tabs: each narrows the search further.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Messages,
    Media,
    Links,
    Files,
    Pins,
}

impl Tab {
    pub const ALL: [Tab; 5] = [Tab::Messages, Tab::Media, Tab::Links, Tab::Files, Tab::Pins];

    pub const fn label(self) -> &'static str {
        match self {
            Tab::Messages => "Messages",
            Tab::Media => "Media",
            Tab::Links => "Links",
            Tab::Files => "Files",
            Tab::Pins => "Pins",
        }
    }

    /// What the tab adds to the filters.
    const fn has(self) -> &'static [Has] {
        match self {
            Tab::Media => &[Has::Image, Has::Video],
            Tab::Links => &[Has::Link],
            Tab::Files => &[Has::File],
            Tab::Messages | Tab::Pins => &[],
        }
    }
}

/// Where to search.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// A server (all of it, unless the filters name channels).
    Guild(Snowflake),
    /// A DM or group DM.
    Channel(Snowflake),
}

/// What to search for. Empty filters match everything.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchParams {
    /// Words the message contains.
    pub content: String,
    /// Sent by any of these.
    pub authors: Vec<Snowflake>,
    /// Mentioning any of these.
    pub mentions: Vec<Snowflake>,
    /// Containing any of these.
    pub has: Vec<Has>,
    /// In any of these channels (of a server).
    pub channels: Vec<Snowflake>,
    pub pinned: Option<bool>,
    pub author_types: Vec<AuthorType>,
    /// With a link to any of these sites (`example.com`).
    pub link_hostnames: Vec<String>,
    /// With a file of any of these types (`pdf`).
    pub extensions: Vec<String>,
    /// With a file whose name contains any of these.
    pub filenames: Vec<String>,
    /// Sent after this id (a time, as a snowflake).
    pub min_id: Option<Snowflake>,
    /// Sent before this id.
    pub max_id: Option<Snowflake>,
}

impl SearchParams {
    /// Whether nothing is asked for (a search needs something).
    pub fn is_empty(&self) -> bool {
        *self == SearchParams::default()
    }

    /// The request's path and query for `scope`, a results `tab`, an order
    /// and an offset into the results.
    pub fn path(&self, scope: Scope, tab: Tab, sort: Sort, offset: u32, include_nsfw: bool) -> String {
        let mut query: Vec<String> = Vec::new();
        let mut push = |key: &str, value: &str| query.push(format!("{key}={}", encode(value)));
        let content = self.content.trim();
        if !content.is_empty() {
            push("content", content);
        }
        for author in &self.authors {
            push("author_id", &author.to_string());
        }
        for mention in &self.mentions {
            push("mentions", &mention.to_string());
        }
        // A tab narrows the search to its own kinds: several values of
        // `has` match any of them, so Media within "has: link" must not
        // become "link or image". The tab's kinds stand in for the chosen.
        let has = if tab.has().is_empty() { self.has.as_slice() } else { tab.has() };
        for has in has {
            push("has", has.value());
        }
        for host in &self.link_hostnames {
            push("link_hostname", host);
        }
        for extension in &self.extensions {
            push("attachment_extension", extension.trim_start_matches('.'));
        }
        for name in &self.filenames {
            push("attachment_filename", name);
        }
        if let Some(min) = self.min_id {
            push("min_id", &min.to_string());
        }
        if let Some(max) = self.max_id {
            push("max_id", &max.to_string());
        }
        if matches!(scope, Scope::Guild(_)) {
            for channel in &self.channels {
                push("channel_id", &channel.to_string());
            }
        }
        let pinned = if tab == Tab::Pins { Some(true) } else { self.pinned };
        if let Some(pinned) = pinned {
            push("pinned", if pinned { "true" } else { "false" });
        }
        for kind in &self.author_types {
            push("author_type", kind.value());
        }
        if matches!(scope, Scope::Guild(_)) && include_nsfw {
            push("include_nsfw", "true");
        }
        query.push(sort.params().to_owned());
        query.push(format!("offset={offset}"));
        let base = match scope {
            Scope::Guild(guild) => format!("/guilds/{guild}/messages/search"),
            Scope::Channel(channel) => format!("/channels/{channel}/messages/search"),
        };
        format!("{base}?{}", query.join("&"))
    }
}

/// Percent-encodes a query value the way `encodeURIComponent` does.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dm_search_matches_the_web_clients_request() {
        // From a capture: content, then the order, then the offset.
        let params = SearchParams {
            content: "hello there".into(),
            ..SearchParams::default()
        };
        assert_eq!(
            params.path(Scope::Channel(Snowflake(5)), Tab::Messages, Sort::Newest, 0, true),
            "/channels/5/messages/search?content=hello%20there&sort_by=timestamp&sort_order=desc&offset=0"
        );
        let from = SearchParams {
            authors: vec![Snowflake(7)],
            ..SearchParams::default()
        };
        assert_eq!(
            from.path(Scope::Channel(Snowflake(5)), Tab::Messages, Sort::Newest, 25, true),
            "/channels/5/messages/search?author_id=7&sort_by=timestamp&sort_order=desc&offset=25"
        );
    }

    #[test]
    fn filters_repeat_their_parameter_and_a_server_search_names_channels() {
        let params = SearchParams {
            has: vec![Has::Image, Has::Forward],
            mentions: vec![Snowflake(1), Snowflake(2)],
            channels: vec![Snowflake(30)],
            pinned: Some(false),
            author_types: vec![AuthorType::Bot],
            link_hostnames: vec!["example.com".into()],
            extensions: vec![".pdf".into()],
            filenames: vec!["report".into()],
            min_id: Some(Snowflake(100)),
            max_id: Some(Snowflake(200)),
            ..SearchParams::default()
        };
        let path = params.path(Scope::Guild(Snowflake(9)), Tab::Messages, Sort::Relevant, 0, true);
        assert_eq!(
            path,
            "/guilds/9/messages/search?mentions=1&mentions=2&has=image&has=snapshot&link_hostname=example.com\
             &attachment_extension=pdf&attachment_filename=report&min_id=100&max_id=200&channel_id=30\
             &pinned=false&author_type=bot&include_nsfw=true&sort_by=relevance&sort_order=desc&offset=0"
        );
        // A DM has no channels to name and no NSFW setting.
        let dm = params.path(Scope::Channel(Snowflake(5)), Tab::Messages, Sort::Oldest, 0, true);
        assert!(!dm.contains("channel_id") && !dm.contains("include_nsfw"), "{dm}");
        assert!(dm.ends_with("sort_by=timestamp&sort_order=asc&offset=0"));
    }

    #[test]
    fn tabs_narrow_the_search() {
        let params = SearchParams {
            content: "cat".into(),
            has: vec![Has::Link],
            ..SearchParams::default()
        };
        let media = params.path(Scope::Channel(Snowflake(5)), Tab::Media, Sort::Newest, 0, false);
        assert!(media.contains("has=image&has=video") && !media.contains("has=link"), "{media}");
        let pins = params.path(Scope::Channel(Snowflake(5)), Tab::Pins, Sort::Newest, 0, false);
        assert!(pins.contains("has=link&pinned=true"), "{pins}");
    }

    #[test]
    fn kinds_are_named_loosely() {
        assert_eq!(Has::named("Images"), Some(Has::Image));
        assert_eq!(Has::named("forward"), Some(Has::Forward));
        assert_eq!(Has::named("snapshot"), Some(Has::Forward));
        assert_eq!(Has::named("attachment"), Some(Has::File));
        assert_eq!(Has::named("nothing"), None);
        assert_eq!(AuthorType::named("bots"), Some(AuthorType::Bot));
        assert!(SearchParams::default().is_empty());
    }
}
