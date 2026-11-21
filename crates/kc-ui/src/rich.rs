//! Discord message content: parser and Pango renderer.
//!
//! [`parse`] turns raw message content into a flat list of styled [`Piece`]s;
//! [`to_markup`] and [`to_plain`] render those pieces for labels and previews.
//!
//! Message content is untrusted: every piece of text is escaped before any
//! markup is added, so the only tags that reach Pango are the ones this module
//! emits. Because the pieces are flat (each carries its complete style) every
//! piece is rendered as a self-contained, balanced run of tags.
//!
//! The parser is a single forward pass followed by a linear delimiter-matching
//! pass. Every look-ahead is either memoised or bounded by a constant, so
//! hostile input (thousands of unmatched markers) cannot cause backtracking.

use kc_core::Snowflake;

/// Longest link target the parser accepts inside `<...>` or `[text](...)`.
///
/// Bounding the look-ahead keeps repeated failed link attempts linear.
const MAX_URL_LEN: usize = 2048;

/// Longest `<...>` entity (mention, emoji, timestamp) including brackets.
const MAX_ENTITY_LEN: usize = 64;

/// Longest custom emoji name Discord allows.
const MAX_EMOJI_NAME_LEN: usize = 32;

/// Formats accepted in `<t:unix:STYLE>`.
const TIMESTAMP_FORMATS: &str = "tTdDfFsSR";

/// Format used when a timestamp has no explicit style.
const DEFAULT_TIMESTAMP_FORMAT: char = 'f';

/// Most emoji an emoji-only message may hold and still render large.
const MAX_JUMBO_EMOJI: usize = 30;

/// Prefix drawn at the start of every block-quoted line.
const QUOTE_PREFIX: &str = "<span alpha=\"50%\">\u{258E} </span>";

/// Wrapper that makes spoiler text unreadable until clicked.
const SPOILER_HIDDEN_OPEN: &str =
    "<a href=\"kubecord://spoiler\"><span foreground=\"#202225\" background=\"#202225\">";
const SPOILER_HIDDEN_CLOSE: &str = "</span></a>";

/// Background for revealed spoilers.
const SPOILER_REVEALED_OPEN: &str = "<span background=\"#808080\" bgalpha=\"25%\">";

/// Background for rendered timestamps.
const TIMESTAMP_OPEN: &str = "<span background=\"#808080\" bgalpha=\"20%\">";

/// Highlight colour behind mentions (Discord "blurple").
const MENTION_COLOUR: u32 = 0x58_65_f2;

/// Stand-in for hidden spoilers in plain text.
const SPOILER_PLACEHOLDER: &str = "\u{25AE}\u{25AE}\u{25AE}\u{25AE}";

/// Formatting applied to a piece of message content.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    /// Inline `code`.
    pub code: bool,
    /// Fenced ``` code block (text keeps its newlines; language tag dropped).
    pub code_block: bool,
    pub spoiler: bool,
    /// Inside a `> ` or `>>> ` block quote.
    pub quote: bool,
    /// 1..=3 for `#`, `##`, `###` headings at line start; 0 otherwise.
    pub heading: u8,
    /// `-# ` subtext line.
    pub subtext: bool,
    /// Target of a bare URL, `<url>`, or `[text](url)` masked link.
    pub link: Option<String>,
}

/// Something a message can mention.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mention {
    /// `<@id>` and `<@!id>`.
    User(Snowflake),
    /// `<@&id>`.
    Role(Snowflake),
    /// `<#id>`.
    Channel(Snowflake),
    /// `@everyone`.
    Everyone,
    /// `@here`.
    Here,
}

/// One run of parsed message content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Piece {
    /// Literal text with a single style.
    Text { text: String, style: Style },
    /// A user, role, channel, `@everyone` or `@here` mention.
    Mention { mention: Mention, style: Style },
    /// Custom emoji `<:name:id>` / `<a:name:id>`.
    Emoji {
        id: Snowflake,
        name: String,
        animated: bool,
    },
    /// `<t:unix>` or `<t:unix:STYLE>`; style defaults to 'f'.
    Timestamp {
        unix: i64,
        format: char,
        style: Style,
    },
}

/// Supplies names for ids while rendering.
pub trait Resolver {
    /// Display name of a user.
    fn user(&self, id: Snowflake) -> Option<String>;
    /// Role name and RGB colour (0 = no colour).
    fn role(&self, id: Snowflake) -> Option<(String, u32)>;
    /// Channel name, without the leading `#`.
    fn channel(&self, id: Snowflake) -> Option<String>;
    /// Human text for a `<t:...>` timestamp.
    fn timestamp(&self, unix: i64, format: char) -> String;
}

/// Parses raw message content into a flat list of styled pieces, in order.
///
/// Implements Discord's markdown dialect: emphasis, spoilers, inline code and
/// fenced code blocks, block quotes, headings, subtext, list bullets, links,
/// mentions, custom emoji and timestamps. Anything malformed or unclosed is
/// kept as literal text. Never panics, and runs in time linear in the input.
pub fn parse(content: &str) -> Vec<Piece> {
    let mut lexer = Lexer::new(content, true);
    lexer.run();
    build(lexer.toks)
}

/// Renders pieces as Pango markup for a `GtkLabel`.
///
/// Each piece is wrapped in its own balanced set of tags, so the result is
/// valid markup for any input. Mentions link to `kubecord://user/ID` and
/// `kubecord://channel/ID`; hidden spoilers link to `kubecord://spoiler`.
/// A piece is never wrapped in more than one `<a>`: a hidden spoiler or an
/// enclosing masked link takes precedence over a mention's own link.
pub fn to_markup(pieces: &[Piece], resolver: &dyn Resolver, reveal_spoilers: bool) -> String {
    let mut writer = MarkupWriter {
        out: String::new(),
        resolver,
        reveal: reveal_spoilers,
        line_start: true,
        pending_break: false,
    };
    for (index, piece) in pieces.iter().enumerate() {
        writer.settle_break(piece);
        match piece {
            Piece::Text { text, style } if style.code_block => writer.code_block(text, style),
            Piece::Text { text, style } => writer.text(text, style),
            Piece::Mention { mention, style } => {
                let inner = writer.mention(mention, style);
                writer.atom(style, &inner);
            }
            Piece::Timestamp {
                unix,
                format,
                style,
            } => {
                let inner = writer.timestamp(*unix, *format, style);
                writer.atom(style, &inner);
            }
            Piece::Emoji { name, .. } => {
                // Emoji carry no style, so borrow the quote state of the line.
                if writer.line_start && quote_follows(&pieces[index + 1..]) {
                    writer.quote_prefix();
                }
                writer.out.push(':');
                escape_text_into(&mut writer.out, name);
                writer.out.push(':');
                writer.line_start = false;
            }
        }
    }
    writer.out
}

/// Plain text for previews and notifications.
///
/// No markup is produced: mentions are resolved to `@Name` / `#name`, emoji
/// become `:name:`, and each run of spoilered content is replaced by "▮▮▮▮"
/// unless `reveal_spoilers` is set.
pub fn to_plain(pieces: &[Piece], resolver: &dyn Resolver, reveal_spoilers: bool) -> String {
    let mut out = String::new();
    let mut in_spoiler = false;
    for piece in pieces {
        let hidden = !reveal_spoilers
            && match piece {
                Piece::Text { style, .. }
                | Piece::Mention { style, .. }
                | Piece::Timestamp { style, .. } => style.spoiler,
                Piece::Emoji { .. } => false,
            };
        if hidden {
            if !in_spoiler {
                out.push_str(SPOILER_PLACEHOLDER);
            }
            in_spoiler = true;
            continue;
        }
        in_spoiler = false;
        match piece {
            Piece::Text { text, .. } => out.push_str(text),
            Piece::Mention { mention, .. } => out.push_str(&mention_label(mention, resolver)),
            Piece::Emoji { name, .. } => {
                out.push(':');
                out.push_str(name);
                out.push(':');
            }
            Piece::Timestamp { unix, format, .. } => {
                out.push_str(&resolver.timestamp(*unix, *format));
            }
        }
    }
    out
}

/// True when the content is nothing but emoji.
///
/// That is: custom emoji pieces and/or text consisting only of unicode emoji
/// and whitespace, with at least one and at most 30 emoji. Discord renders
/// such messages with large emoji. Joiners, variation selectors and skin
/// tones never count as separate emoji, and a pair of regional indicators
/// counts as one flag.
pub fn is_emoji_only(pieces: &[Piece]) -> bool {
    let mut count = 0usize;
    for piece in pieces {
        match piece {
            Piece::Emoji { .. } => count += 1,
            Piece::Text { text, style } if !style.code && !style.code_block => {
                match count_unicode_emoji(text) {
                    Some(found) => count += found,
                    None => return false,
                }
            }
            _ => return false,
        }
    }
    (1..=MAX_JUMBO_EMOJI).contains(&count)
}

/// True when any piece is a custom `Piece::Emoji`.
pub fn has_custom_emoji(pieces: &[Piece]) -> bool {
    pieces
        .iter()
        .any(|piece| matches!(piece, Piece::Emoji { .. }))
}

// ---------------------------------------------------------------------------
// Lexing
// ---------------------------------------------------------------------------

/// Emphasis delimiters. Each kind is matched independently of the others.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Marker {
    /// `**`
    Bold,
    /// `*`
    ItalicStar,
    /// `__`
    Underline,
    /// `_`
    ItalicUnderscore,
    /// `~~`
    Strike,
    /// `||`
    Spoiler,
}

/// Number of [`Marker`] variants.
const MARKER_KINDS: usize = 6;

impl Marker {
    fn index(self) -> usize {
        match self {
            Marker::Bold => 0,
            Marker::ItalicStar => 1,
            Marker::Underline => 2,
            Marker::ItalicUnderscore => 3,
            Marker::Strike => 4,
            Marker::Spoiler => 5,
        }
    }

    /// The source text of the delimiter, used when it stays unmatched.
    fn literal(self) -> &'static str {
        match self {
            Marker::Bold => "**",
            Marker::ItalicStar => "*",
            Marker::Underline => "__",
            Marker::ItalicUnderscore => "_",
            Marker::Strike => "~~",
            Marker::Spoiler => "||",
        }
    }
}

/// Output of the lexer: content plus style-changing instructions.
#[derive(Debug)]
enum Tok {
    Text(String),
    Marker {
        kind: Marker,
        can_open: bool,
        can_close: bool,
    },
    Code(String),
    CodeBlock(String),
    Mention(Mention),
    Emoji {
        id: Snowflake,
        name: String,
        animated: bool,
    },
    Timestamp {
        unix: i64,
        format: char,
    },
    /// A bare or angle-bracketed URL, shown as itself.
    Url(String),
    /// Start of masked-link text pointing at the given URL.
    LinkOpen(String),
    LinkClose,
    /// Block quote on/off.
    Quote(bool),
    /// Heading level and subtext flag for the current line.
    Line {
        heading: u8,
        subtext: bool,
    },
}

/// Memoised forward substring search.
///
/// Queries arrive with non-decreasing start offsets, so remembering the last
/// answer means the haystack is scanned at most once overall: a failed search
/// stays failed, and a hit is reused until the cursor moves past it.
struct Finder {
    needle: &'static str,
    /// Offset the cached search started from, once one has been made.
    searched_from: Option<usize>,
    found: Option<usize>,
}

impl Finder {
    fn new(needle: &'static str) -> Self {
        Self {
            needle,
            searched_from: None,
            found: None,
        }
    }

    /// Byte offset of the first occurrence of the needle at or after `start`.
    fn find(&mut self, haystack: &str, start: usize) -> Option<usize> {
        if self.searched_from.is_some_and(|from| start >= from) {
            match self.found {
                None => return None,
                Some(position) if position >= start => return Some(position),
                Some(_) => {}
            }
        }
        let found = haystack
            .get(start..)
            .and_then(|tail| tail.find(self.needle))
            .map(|offset| offset + start);
        self.searched_from = Some(start);
        self.found = found;
        found
    }
}

/// Single-pass tokenizer over message content.
struct Lexer<'a> {
    src: &'a str,
    pos: usize,
    toks: Vec<Tok>,
    /// True for whole messages; false for masked-link text, where block
    /// syntax and nested links are not interpreted.
    top_level: bool,
    line_start: bool,
    /// A block quote is active on the current line.
    quoted: bool,
    /// `>>> ` was seen: the quote lasts to the end of the message.
    quote_all: bool,
    /// A heading or subtext is active on the current line.
    line_styled: bool,
    fence: Finder,
    bracket: Finder,
    newline: Finder,
    /// Set once no closing run of exactly 1 (index 0) or 2 (index 1)
    /// backticks exists in the remainder of the input.
    no_tick_run: [bool; 2],
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str, top_level: bool) -> Self {
        Self {
            src,
            pos: 0,
            toks: Vec::new(),
            top_level,
            line_start: top_level,
            quoted: false,
            quote_all: false,
            line_styled: false,
            fence: Finder::new("```"),
            bracket: Finder::new("]"),
            newline: Finder::new("\n"),
            no_tick_run: [false; 2],
        }
    }

    fn run(&mut self) {
        let bytes = self.src.as_bytes();
        while self.pos < bytes.len() {
            if self.line_start {
                self.line_start = false;
                self.line_prefix();
                continue;
            }
            // Every handler below consumes at least one byte.
            match bytes[self.pos] {
                b'\n' => self.line_break(),
                b'\\' => self.escape(),
                b'`' => self.backticks(),
                byte @ (b'*' | b'_' | b'~' | b'|') => self.marker_run(byte),
                b'<' => self.angle(),
                b'[' if self.top_level => self.masked_link(),
                b'h' if self.top_level => self.bare_url(),
                b'@' => self.broadcast_mention(),
                _ => self.plain(),
            }
        }
    }

    /// The unlexed remainder of the input.
    fn rest(&self) -> &'a str {
        self.src.get(self.pos..).unwrap_or("")
    }

    fn push_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(Tok::Text(last)) = self.toks.last_mut() {
            last.push_str(text);
        } else {
            self.toks.push(Tok::Text(text.to_owned()));
        }
    }

    /// Emits the next `len` bytes verbatim. Callers pass ASCII lengths only.
    fn literal(&mut self, len: usize) {
        let end = (self.pos + len).min(self.src.len());
        let text = self.src.get(self.pos..end).unwrap_or("");
        self.push_text(text);
        self.pos = end;
    }

    /// Length of the run of `byte` starting at `start`.
    fn run_len(&self, start: usize, byte: u8) -> usize {
        self.src
            .as_bytes()
            .get(start..)
            .map_or(0, |tail| tail.iter().take_while(|b| **b == byte).count())
    }

    /// Copies text up to the next byte that could start a construct.
    fn plain(&mut self) {
        let bytes = self.src.as_bytes();
        // All special bytes are ASCII, so stopping on one (or at the end)
        // always lands on a character boundary.
        let end = bytes
            .get(self.pos + 1..)
            .and_then(|tail| tail.iter().position(|byte| is_special(*byte)))
            .map_or(bytes.len(), |offset| self.pos + 1 + offset);
        let text = self.src.get(self.pos..end).unwrap_or("");
        self.push_text(text);
        self.pos = end;
    }

    /// Handles block syntax at the start of a line: quotes, headings,
    /// subtext and list bullets. May consume nothing.
    fn line_prefix(&mut self) {
        if !self.quote_all {
            let rest = self.rest();
            let marker_len = if rest.starts_with(">>> ") {
                self.quote_all = true;
                4
            } else if rest.starts_with("> ") {
                2
            } else {
                0
            };
            if marker_len > 0 {
                self.pos += marker_len;
                if !self.quoted {
                    self.quoted = true;
                    self.toks.push(Tok::Quote(true));
                }
            }
        }

        let rest = self.rest();
        if let Some(level) = heading_level(rest) {
            self.pos += usize::from(level) + 1;
            self.line_styled = true;
            self.toks.push(Tok::Line {
                heading: level,
                subtext: false,
            });
            return;
        }
        if rest
            .strip_prefix("-# ")
            .is_some_and(starts_with_line_content)
        {
            self.pos += 3;
            self.line_styled = true;
            self.toks.push(Tok::Line {
                heading: 0,
                subtext: true,
            });
            return;
        }

        // List items stay text, but the bullet is normalised so that
        // `* item` is not mistaken for the start of italics.
        let indent = rest.len() - rest.trim_start_matches(' ').len();
        let item = rest.get(indent..).unwrap_or("");
        let is_bullet = item
            .strip_prefix("- ")
            .or_else(|| item.strip_prefix("* "))
            .is_some_and(starts_with_line_content);
        if is_bullet {
            self.literal(indent);
            self.push_text("\u{2022} ");
            self.pos += 2;
        }
    }

    fn line_break(&mut self) {
        self.pos += 1;
        if self.top_level {
            if self.line_styled {
                self.line_styled = false;
                self.toks.push(Tok::Line {
                    heading: 0,
                    subtext: false,
                });
            }
            if self.quoted && !self.quote_all {
                // The newline between two quoted lines belongs to the quote.
                let rest = self.rest();
                if !rest.starts_with("> ") && !rest.starts_with(">>> ") {
                    self.quoted = false;
                    self.toks.push(Tok::Quote(false));
                }
            }
            self.line_start = true;
        }
        self.push_text("\n");
    }

    /// `\x` emits `x` literally when `x` is ASCII punctuation.
    fn escape(&mut self) {
        let escaped = self
            .src
            .as_bytes()
            .get(self.pos + 1)
            .copied()
            .filter(u8::is_ascii_punctuation);
        match escaped {
            Some(_) => {
                self.pos += 1;
                self.literal(1);
            }
            None => self.literal(1),
        }
    }

    fn backticks(&mut self) {
        let run = self.run_len(self.pos, b'`');
        let consumed = match run {
            1 | 2 => self.inline_code(run),
            _ => self.fenced_code(),
        };
        if !consumed {
            self.literal(run);
        }
    }

    /// Inline code delimited by runs of exactly `run` (1 or 2) backticks.
    fn inline_code(&mut self, run: usize) -> bool {
        let dead = run - 1;
        if self.no_tick_run[dead] {
            return false;
        }
        let start = self.pos + run;
        let Some(close) = self.find_tick_run(start, run) else {
            self.no_tick_run[dead] = true;
            return false;
        };
        let content = self.src.get(start..close).unwrap_or("");
        if content.trim().is_empty() {
            return false;
        }
        // ``code`` may pad its content so it can start or end with a backtick.
        let content = if run == 2 {
            content
                .strip_prefix(' ')
                .and_then(|inner| inner.strip_suffix(' '))
                .unwrap_or(content)
        } else {
            content
        };
        self.toks.push(Tok::Code(content.to_owned()));
        self.pos = close + run;
        true
    }

    /// Offset of the next run of exactly `len` backticks at or after `from`.
    fn find_tick_run(&self, from: usize, len: usize) -> Option<usize> {
        let mut cursor = from;
        loop {
            let offset = self.src.get(cursor..)?.find('`')?;
            let start = cursor + offset;
            let run = self.run_len(start, b'`');
            if run == len {
                return Some(start);
            }
            cursor = start + run;
        }
    }

    /// A ``` fenced block; the fence may open and close on the same line.
    fn fenced_code(&mut self) -> bool {
        let start = self.pos + 3;
        let Some(close) = self.fence.find(self.src, start) else {
            return false;
        };
        let Some(body) = self.src.get(start..close).and_then(code_block_body) else {
            return false;
        };
        self.toks.push(Tok::CodeBlock(body.to_owned()));
        self.pos = close + 3;
        true
    }

    /// Splits a run of `*`, `_`, `~` or `|` into delimiter tokens.
    fn marker_run(&mut self, byte: u8) {
        let start = self.pos;
        let len = self.run_len(start, byte);
        let end = start + len;
        let prev = self
            .src
            .get(..start)
            .and_then(|head| head.chars().next_back());
        let next = self.src.get(end..).and_then(|tail| tail.chars().next());
        self.pos = end;

        let (double, single) = match byte {
            b'*' => (Marker::Bold, Some(Marker::ItalicStar)),
            b'_' => (Marker::Underline, Some(Marker::ItalicUnderscore)),
            b'~' => (Marker::Strike, None),
            _ => (Marker::Spoiler, None),
        };
        for _ in 0..len / 2 {
            self.toks.push(Tok::Marker {
                kind: double,
                can_open: next.is_some(),
                can_close: prev.is_some(),
            });
        }
        if len.is_multiple_of(2) {
            return;
        }
        let Some(kind) = single else {
            // A lone `~` or `|` has no meaning.
            self.push_text(if byte == b'~' { "~" } else { "|" });
            return;
        };
        // Single-character italics must hug their content, and `_` must not
        // sit inside a word (`snake_case_name`).
        let mut can_open = next.is_some_and(|ch| !ch.is_whitespace());
        let mut can_close = prev.is_some_and(|ch| !ch.is_whitespace());
        if kind == Marker::ItalicUnderscore {
            can_open &= !prev.is_some_and(char::is_alphanumeric);
            can_close &= !next.is_some_and(char::is_alphanumeric);
        }
        self.toks.push(Tok::Marker {
            kind,
            can_open,
            can_close,
        });
    }

    /// `<...>`: a mention, custom emoji, timestamp or embed-suppressed URL.
    fn angle(&mut self) {
        let rest = self.rest();
        if let Some((tok, len)) = parse_entity(rest) {
            self.toks.push(tok);
            self.pos += len;
            return;
        }
        if self.top_level
            && let Some((url, len)) = rest.get(1..).and_then(parse_angle_url)
        {
            self.toks.push(Tok::Url(url.to_owned()));
            self.pos += 1 + len;
            return;
        }
        self.literal(1);
    }

    /// `[text](url)`, `[text](<url>)` and `[text](url "title")`.
    fn masked_link(&mut self) {
        let open = self.pos;
        if let Some((close, url, target_len)) = self.masked_link_parts(open) {
            self.toks.push(Tok::LinkOpen(url));
            let label = self.src.get(open + 1..close).unwrap_or("");
            let mut inner = Lexer::new(label, false);
            inner.run();
            self.toks.append(&mut inner.toks);
            self.toks.push(Tok::LinkClose);
            self.pos = close + 1 + target_len;
        } else {
            self.literal(1);
        }
    }

    /// Locates the `]` and parses the target of a masked link opening at
    /// `open`. Returns the offset of `]`, the URL, and the target's length.
    fn masked_link_parts(&mut self, open: usize) -> Option<(usize, String, usize)> {
        let close = self.bracket.find(self.src, open + 1)?;
        if close == open + 1 {
            return None;
        }
        // Link text may not span lines.
        if self
            .newline
            .find(self.src, open + 1)
            .is_some_and(|newline| newline < close)
        {
            return None;
        }
        let (url, target_len) = parse_link_target(self.src.get(close + 1..)?)?;
        Some((close, url.to_owned(), target_len))
    }

    /// A bare `http://` or `https://` URL.
    fn bare_url(&mut self) {
        let rest = self.rest();
        if !is_http_url(rest) {
            self.literal(1);
            return;
        }
        let end = rest
            .find(|ch: char| ch.is_whitespace() || ch == '<')
            .unwrap_or(rest.len());
        let url = trim_url(rest.get(..end).unwrap_or(""));
        if !is_http_url(url) {
            self.literal(1);
            return;
        }
        self.toks.push(Tok::Url(url.to_owned()));
        self.pos += url.len();
    }

    /// `@everyone` and `@here`, when not the prefix of a longer word.
    fn broadcast_mention(&mut self) {
        let rest = self.rest();
        for (keyword, mention) in [("@everyone", Mention::Everyone), ("@here", Mention::Here)] {
            let Some(after) = rest.strip_prefix(keyword) else {
                continue;
            };
            let in_word = after
                .chars()
                .next()
                .is_some_and(|ch| ch.is_alphanumeric() || ch == '_');
            if !in_word {
                self.toks.push(Tok::Mention(mention));
                self.pos += keyword.len();
                return;
            }
        }
        self.literal(1);
    }
}

/// Bytes at which [`Lexer::plain`] must stop and let the lexer decide.
fn is_special(byte: u8) -> bool {
    matches!(
        byte,
        b'\n' | b'\\' | b'`' | b'*' | b'_' | b'~' | b'|' | b'<' | b'[' | b'h' | b'@'
    )
}

/// True when a line has something on it after its block marker.
fn starts_with_line_content(rest: &str) -> bool {
    rest.chars().next().is_some_and(|ch| ch != '\n')
}

/// Heading level when `line` opens with `# `, `## ` or `### ` plus content.
fn heading_level(line: &str) -> Option<u8> {
    let hashes = line
        .bytes()
        .take(4)
        .take_while(|byte| *byte == b'#')
        .count();
    let level = u8::try_from(hashes)
        .ok()
        .filter(|level| (1..=3).contains(level))?;
    line.get(hashes..)?
        .strip_prefix(' ')
        .is_some_and(starts_with_line_content)
        .then_some(level)
}

/// Extracts the code of a fenced block from the text between its fences,
/// dropping a language tag and surrounding blank lines. `None` when empty.
fn code_block_body(raw: &str) -> Option<&str> {
    let mut body = raw;
    if let Some((first, rest)) = raw.split_once('\n') {
        let is_language = first
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '_' | '.' | '#'));
        if is_language && !rest.trim().is_empty() {
            body = rest;
        }
    }
    let body = body.trim_matches('\n');
    (!body.trim().is_empty()).then_some(body)
}

/// Parses `<@id>`, `<@!id>`, `<@&id>`, `<#id>`, `<:name:id>`, `<a:name:id>`,
/// `<t:unix>` and `<t:unix:STYLE>` at the start of `rest`.
///
/// Returns the token and the number of bytes it spans.
fn parse_entity(rest: &str) -> Option<(Tok, usize)> {
    let close = rest
        .bytes()
        .take(MAX_ENTITY_LEN)
        .position(|byte| byte == b'>')?;
    let inner = rest.get(1..close)?;
    let tok = if let Some(id) = inner.strip_prefix("@&") {
        Tok::Mention(Mention::Role(parse_id(id)?))
    } else if let Some(id) = inner.strip_prefix("@!") {
        Tok::Mention(Mention::User(parse_id(id)?))
    } else if let Some(id) = inner.strip_prefix('@') {
        Tok::Mention(Mention::User(parse_id(id)?))
    } else if let Some(id) = inner.strip_prefix('#') {
        Tok::Mention(Mention::Channel(parse_id(id)?))
    } else if let Some(emoji) = inner.strip_prefix("a:") {
        parse_emoji(emoji, true)?
    } else if let Some(emoji) = inner.strip_prefix(':') {
        parse_emoji(emoji, false)?
    } else {
        parse_timestamp(inner.strip_prefix("t:")?)?
    };
    Some((tok, close + 1))
}

/// A decimal snowflake: 1 to 20 ASCII digits that fit in a `u64`.
fn parse_id(digits: &str) -> Option<Snowflake> {
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u64>().ok().map(Snowflake::new)
}

/// The `name:id` part of a custom emoji.
fn parse_emoji(body: &str, animated: bool) -> Option<Tok> {
    let (name, id) = body.split_once(':')?;
    let valid_name = !name.is_empty()
        && name.len() <= MAX_EMOJI_NAME_LEN
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
    if !valid_name {
        return None;
    }
    Some(Tok::Emoji {
        id: parse_id(id)?,
        name: name.to_owned(),
        animated,
    })
}

/// The `unix` or `unix:STYLE` part of a timestamp.
fn parse_timestamp(body: &str) -> Option<Tok> {
    let (seconds, format) = match body.split_once(':') {
        Some((seconds, format)) => {
            let mut chars = format.chars();
            let format = chars.next().filter(|ch| TIMESTAMP_FORMATS.contains(*ch))?;
            if chars.next().is_some() {
                return None;
            }
            (seconds, format)
        }
        None => (body, DEFAULT_TIMESTAMP_FORMAT),
    };
    let digits = seconds.strip_prefix('-').unwrap_or(seconds);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(Tok::Timestamp {
        unix: seconds.parse::<i64>().ok()?,
        format,
    })
}

/// True for `http://` / `https://` followed by at least one character.
fn is_http_url(text: &str) -> bool {
    ["http://", "https://"]
        .iter()
        .any(|scheme| text.len() > scheme.len() && text.starts_with(scheme))
}

/// Parses `url>` (the text after an opening `<`).
///
/// Returns the URL and the number of bytes consumed including the `>`.
fn parse_angle_url(after_open: &str) -> Option<(&str, usize)> {
    for (offset, ch) in after_open.char_indices().take(MAX_URL_LEN) {
        if ch == '>' {
            let url = after_open.get(..offset)?;
            return is_http_url(url).then_some((url, offset + 1));
        }
        if ch.is_whitespace() || ch == '<' {
            return None;
        }
    }
    None
}

/// Parses the `(url)`, `(<url>)` or `(url "title")` part of a masked link.
///
/// Returns the URL and the number of bytes consumed including both
/// parentheses. Only `http`/`https` targets are accepted.
fn parse_link_target(text: &str) -> Option<(&str, usize)> {
    let after_paren = text.strip_prefix('(')?;
    let (url, url_len) = if let Some(after_angle) = after_paren.strip_prefix('<') {
        let (url, len) = parse_angle_url(after_angle)?;
        (url, len + 1)
    } else {
        let end = unbracketed_target_end(after_paren)?;
        (after_paren.get(..end)?, end)
    };
    if !is_http_url(url) {
        return None;
    }

    let mut rest = after_paren.get(url_len..)?.trim_start_matches(' ');
    if let Some(quote) = rest.chars().next().filter(|ch| matches!(ch, '"' | '\'')) {
        // Optional title: skipped, but it must be closed.
        let body = rest.get(1..)?;
        let end = body
            .char_indices()
            .take(MAX_URL_LEN)
            .find(|(_, ch)| *ch == quote || *ch == '\n')
            .filter(|(_, ch)| *ch == quote)
            .map(|(offset, _)| offset)?;
        rest = body.get(end + 1..)?.trim_start_matches(' ');
    }
    let rest = rest.strip_prefix(')')?;
    Some((url, text.len() - rest.len()))
}

/// Length of an unbracketed link target: it ends at whitespace or at the
/// first `)` that does not close a `(` inside the URL itself.
fn unbracketed_target_end(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, ch) in text.char_indices().take(MAX_URL_LEN) {
        match ch {
            '(' => depth += 1,
            ')' if depth == 0 => return Some(offset),
            ')' => depth -= 1,
            '<' => return None,
            _ if ch.is_whitespace() => return Some(offset),
            _ => {}
        }
    }
    None
}

/// Strips sentence punctuation and markdown delimiters from the end of a
/// bare URL. A `)` is kept while it balances a `(` inside the URL.
fn trim_url(mut url: &str) -> &str {
    let mut opens = url.bytes().filter(|byte| *byte == b'(').count();
    let mut closes = url.bytes().filter(|byte| *byte == b')').count();
    while let Some(last) = url.chars().next_back() {
        let strip = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '>' | '"' | '\'' | '*' | '_' | '~' | '|' => true,
            ')' => closes > opens,
            _ => false,
        };
        if !strip {
            break;
        }
        match last {
            '(' => opens -= 1,
            ')' => closes -= 1,
            _ => {}
        }
        url = url.get(..url.len() - last.len_utf8()).unwrap_or("");
    }
    url
}

// ---------------------------------------------------------------------------
// Delimiter matching and piece building
// ---------------------------------------------------------------------------

/// What a delimiter token turned out to be.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Literal,
    Open,
    Close,
}

/// Pairs up delimiter tokens in one pass.
///
/// Each delimiter kind has at most one pending opener. A closer pairs with it
/// only when some content lies in between; everything unpaired stays literal.
/// Kinds are independent, so overlapping ranges such as `**a *b** c*` simply
/// produce overlapping styles.
fn resolve(toks: &[Tok]) -> Vec<Role> {
    let mut roles = vec![Role::Literal; toks.len()];
    // Pending opener per kind: its token index and the content count then.
    let mut pending: [Option<(usize, usize)>; MARKER_KINDS] = [None; MARKER_KINDS];
    let mut content = 0usize;
    for (index, tok) in toks.iter().enumerate() {
        match tok {
            Tok::Marker {
                kind,
                can_open,
                can_close,
            } => {
                let slot = &mut pending[kind.index()];
                match *slot {
                    Some((opener, seen)) if *can_close && content > seen => {
                        roles[opener] = Role::Open;
                        roles[index] = Role::Close;
                        *slot = None;
                    }
                    None if *can_open => *slot = Some((index, content)),
                    _ => {}
                }
            }
            Tok::Quote(_) | Tok::Line { .. } | Tok::LinkOpen(_) | Tok::LinkClose => {}
            _ => content += 1,
        }
    }
    roles
}

/// Converts tokens into pieces, tracking the style in effect.
fn build(toks: Vec<Tok>) -> Vec<Piece> {
    let roles = resolve(&toks);
    let mut pieces = Vec::new();
    let mut style = Style::default();
    let mut active = [false; MARKER_KINDS];
    for (tok, role) in toks.into_iter().zip(roles) {
        match tok {
            Tok::Text(text) => push_text(&mut pieces, &text, &style),
            Tok::Marker { kind, .. } => match role {
                Role::Literal => push_text(&mut pieces, kind.literal(), &style),
                Role::Open | Role::Close => {
                    active[kind.index()] = role == Role::Open;
                    style.bold = active[Marker::Bold.index()];
                    style.italic = active[Marker::ItalicStar.index()]
                        || active[Marker::ItalicUnderscore.index()];
                    style.underline = active[Marker::Underline.index()];
                    style.strike = active[Marker::Strike.index()];
                    style.spoiler = active[Marker::Spoiler.index()];
                }
            },
            Tok::Code(code) => {
                // Pushed directly: neighbouring code spans stay separate.
                pieces.push(Piece::Text {
                    text: code,
                    style: Style {
                        code: true,
                        ..style.clone()
                    },
                });
            }
            Tok::CodeBlock(code) => {
                pieces.push(Piece::Text {
                    text: code,
                    style: Style {
                        code_block: true,
                        ..style.clone()
                    },
                });
            }
            Tok::Mention(mention) => pieces.push(Piece::Mention {
                mention,
                style: style.clone(),
            }),
            Tok::Emoji { id, name, animated } => pieces.push(Piece::Emoji { id, name, animated }),
            Tok::Timestamp { unix, format } => pieces.push(Piece::Timestamp {
                unix,
                format,
                style: style.clone(),
            }),
            Tok::Url(url) => {
                let link_style = Style {
                    link: Some(url.clone()),
                    ..style.clone()
                };
                push_text(&mut pieces, &url, &link_style);
            }
            Tok::LinkOpen(url) => style.link = Some(url),
            Tok::LinkClose => style.link = None,
            Tok::Quote(on) => style.quote = on,
            Tok::Line { heading, subtext } => {
                style.heading = heading;
                style.subtext = subtext;
            }
        }
    }
    pieces
}

/// Appends text, merging it into the previous piece when the style matches.
fn push_text(pieces: &mut Vec<Piece>, text: &str, style: &Style) {
    if text.is_empty() {
        return;
    }
    if let Some(Piece::Text {
        text: last,
        style: last_style,
    }) = pieces.last_mut()
        && last_style == style
    {
        last.push_str(text);
        return;
    }
    pieces.push(Piece::Text {
        text: text.to_owned(),
        style: style.clone(),
    });
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Accumulates Pango markup while tracking line state for quotes and blocks.
struct MarkupWriter<'a> {
    out: String,
    resolver: &'a dyn Resolver,
    reveal: bool,
    /// Nothing has been written on the current output line yet.
    line_start: bool,
    /// A code block was just written and must be followed by a line break.
    pending_break: bool,
}

impl MarkupWriter<'_> {
    /// Ends the line after a code block unless `next` already does.
    fn settle_break(&mut self, next: &Piece) {
        if !self.pending_break {
            return;
        }
        self.pending_break = false;
        let breaks_itself = matches!(
            next,
            Piece::Text { text, style } if text.starts_with('\n') && !style.code_block
        );
        if !breaks_itself {
            self.out.push('\n');
            self.line_start = true;
        }
    }

    fn quote_prefix(&mut self) {
        self.out.push_str(QUOTE_PREFIX);
        self.line_start = false;
    }

    fn hidden(&self, style: &Style) -> bool {
        style.spoiler && !self.reveal
    }

    /// Writes escaped text. Quoted text is written line by line so that the
    /// quote prefix sits outside the styling of every line.
    fn text(&mut self, text: &str, style: &Style) {
        if !style.quote {
            if !text.is_empty() {
                let mut inner = String::with_capacity(text.len());
                escape_text_into(&mut inner, text);
                self.wrap(style, &inner);
                self.line_start = text.ends_with('\n');
            }
            return;
        }
        for (index, line) in text.split('\n').enumerate() {
            if index > 0 {
                self.out.push('\n');
                self.line_start = true;
            }
            if self.line_start {
                self.quote_prefix();
            }
            if !line.is_empty() {
                let mut inner = String::with_capacity(line.len());
                escape_text_into(&mut inner, line);
                self.wrap(style, &inner);
            }
        }
    }

    /// Writes a fenced code block on lines of its own.
    fn code_block(&mut self, text: &str, style: &Style) {
        if !self.line_start {
            self.out.push('\n');
            self.line_start = true;
        }
        self.text(text, style);
        self.line_start = false;
        self.pending_break = true;
    }

    /// Writes a mention or timestamp whose inner markup is already built.
    fn atom(&mut self, style: &Style, inner: &str) {
        if style.quote && self.line_start {
            self.quote_prefix();
        }
        self.wrap(style, inner);
        self.line_start = false;
    }

    /// Writes `inner` (already valid markup) inside the tags for `style`.
    fn wrap(&mut self, style: &Style, inner: &str) {
        let mut closers: Vec<&'static str> = Vec::new();
        if self.hidden(style) {
            self.out.push_str(SPOILER_HIDDEN_OPEN);
            closers.push(SPOILER_HIDDEN_CLOSE);
        } else if let Some(url) = &style.link {
            self.out.push_str("<a href=\"");
            escape_attribute_into(&mut self.out, url);
            self.out.push_str("\">");
            closers.push("</a>");
        }
        let heading_size = match style.heading {
            1 => Some("x-large"),
            2 => Some("large"),
            3 => Some("medium"),
            _ => None,
        };
        if let Some(size) = heading_size {
            self.out.push_str("<span size=\"");
            self.out.push_str(size);
            self.out.push_str("\" weight=\"bold\">");
            closers.push("</span>");
        }
        if style.subtext {
            self.out.push_str("<span size=\"small\" alpha=\"65%\">");
            closers.push("</span>");
        }
        if style.spoiler && self.reveal {
            self.out.push_str(SPOILER_REVEALED_OPEN);
            closers.push("</span>");
        }
        let simple = [
            (style.bold, "<b>", "</b>"),
            (style.italic, "<i>", "</i>"),
            (style.underline, "<u>", "</u>"),
            (style.strike, "<s>", "</s>"),
            (style.code || style.code_block, "<tt>", "</tt>"),
        ];
        for (enabled, open, close) in simple {
            if enabled {
                self.out.push_str(open);
                closers.push(close);
            }
        }
        self.out.push_str(inner);
        for close in closers.into_iter().rev() {
            self.out.push_str(close);
        }
    }

    /// Inner markup for a mention: highlighted, and linked where possible.
    fn mention(&self, mention: &Mention, style: &Style) -> String {
        let mut label = String::new();
        escape_text_into(&mut label, &mention_label(mention, self.resolver));
        if self.hidden(style) {
            // A highlight would show through the spoiler cover.
            return label;
        }
        // Pango links cannot nest, so an enclosing masked link wins.
        let linkable = style.link.is_none();
        match mention {
            Mention::User(id) => {
                let inner = highlight(MENTION_COLOUR, None, &label);
                link_if(linkable, &format!("kubecord://user/{id}"), inner)
            }
            Mention::Channel(id) => {
                let inner = highlight(MENTION_COLOUR, None, &label);
                link_if(linkable, &format!("kubecord://channel/{id}"), inner)
            }
            Mention::Role(id) => {
                let colour = self
                    .resolver
                    .role(*id)
                    .map(|(_, colour)| colour & 0x00ff_ffff)
                    .filter(|colour| *colour != 0);
                match colour {
                    Some(colour) => highlight(colour, Some(colour), &label),
                    None => highlight(MENTION_COLOUR, None, &label),
                }
            }
            Mention::Everyone | Mention::Here => {
                highlight(MENTION_COLOUR, None, &format!("<b>{label}</b>"))
            }
        }
    }

    /// Inner markup for a timestamp.
    fn timestamp(&self, unix: i64, format: char, style: &Style) -> String {
        let mut label = String::new();
        escape_text_into(&mut label, &self.resolver.timestamp(unix, format));
        if self.hidden(style) {
            return label;
        }
        format!("{TIMESTAMP_OPEN}{label}</span>")
    }
}

/// Display text of a mention: `@Name`, `#name`, or a fallback.
fn mention_label(mention: &Mention, resolver: &dyn Resolver) -> String {
    match mention {
        Mention::User(id) => match resolver.user(*id) {
            Some(name) => format!("@{name}"),
            None => "@unknown-user".to_owned(),
        },
        Mention::Role(id) => match resolver.role(*id) {
            Some((name, _)) => format!("@{name}"),
            None => "@unknown-role".to_owned(),
        },
        Mention::Channel(id) => match resolver.channel(*id) {
            Some(name) => format!("#{name}"),
            None => "#unknown-channel".to_owned(),
        },
        Mention::Everyone => "@everyone".to_owned(),
        Mention::Here => "@here".to_owned(),
    }
}

/// Wraps markup in the translucent mention background.
fn highlight(background: u32, foreground: Option<u32>, inner: &str) -> String {
    match foreground {
        Some(foreground) => format!(
            "<span background=\"#{background:06x}\" bgalpha=\"25%\" \
             foreground=\"#{foreground:06x}\">{inner}</span>"
        ),
        None => format!("<span background=\"#{background:06x}\" bgalpha=\"25%\">{inner}</span>"),
    }
}

/// Wraps markup in a link to an internal (trusted) URL when allowed.
fn link_if(linkable: bool, href: &str, inner: String) -> String {
    if linkable {
        format!("<a href=\"{href}\">{inner}</a>")
    } else {
        inner
    }
}

/// True when the rest of the current line continues inside a block quote.
///
/// Used for emoji, which carry no style of their own.
fn quote_follows(rest: &[Piece]) -> bool {
    for piece in rest {
        match piece {
            Piece::Emoji { .. } => {}
            Piece::Text { text, style } => return style.quote && !text.starts_with('\n'),
            Piece::Mention { style, .. } | Piece::Timestamp { style, .. } => return style.quote,
        }
    }
    false
}

/// Counts the emoji in `text`, or `None` if it holds anything that is not
/// an emoji or whitespace.
fn count_unicode_emoji(text: &str) -> Option<usize> {
    let mut count = 0usize;
    // The previous character was a zero-width joiner.
    let mut joined = false;
    // The previous character was the first half of a flag.
    let mut flag_open = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            joined = false;
            flag_open = false;
            continue;
        }
        match u32::from(ch) {
            0x200D => {
                joined = true;
                flag_open = false;
            }
            // Variation selector, keycap and skin tones modify an emoji.
            0xFE0F | 0x20E3 | 0x1F3FB..=0x1F3FF => {}
            0x1F1E6..=0x1F1FF => {
                if !flag_open && !joined {
                    count += 1;
                }
                flag_open = !flag_open;
                joined = false;
            }
            0x1F000..=0x1FAFF | 0x2600..=0x27BF | 0x2190..=0x21FF | 0x2B00..=0x2BFF => {
                if !joined {
                    count += 1;
                }
                joined = false;
                flag_open = false;
            }
            _ => return None,
        }
    }
    Some(count)
}

/// True for characters that are safe to hand to Pango.
///
/// Control characters other than newline and tab are dropped: they have no
/// visible form and a NUL would truncate the markup string.
fn is_renderable(ch: char) -> bool {
    !ch.is_control() || matches!(ch, '\n' | '\t')
}

/// Escapes text for use as markup character data.
fn escape_text_into(out: &mut String, input: &str) {
    for ch in input.chars().filter(|ch| is_renderable(*ch)) {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
}

/// Escapes text for use inside a double-quoted markup attribute.
fn escape_attribute_into(out: &mut String, input: &str) {
    for ch in input.chars().filter(|ch| is_renderable(*ch)) {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    /// Generous bound for the pathological-input tests in a debug build.
    const QUICK: Duration = Duration::from_secs(2);

    struct Names;

    impl Resolver for Names {
        fn user(&self, id: Snowflake) -> Option<String> {
            match id.get() {
                1 => Some("Alice".to_owned()),
                9 => Some("<b>&\"evil\"".to_owned()),
                _ => None,
            }
        }

        fn role(&self, id: Snowflake) -> Option<(String, u32)> {
            match id.get() {
                2 => Some(("Mods".to_owned(), 0x00ff_8000)),
                3 => Some(("Plain".to_owned(), 0)),
                _ => None,
            }
        }

        fn channel(&self, id: Snowflake) -> Option<String> {
            (id.get() == 4).then(|| "general".to_owned())
        }

        fn timestamp(&self, unix: i64, format: char) -> String {
            format!("<{unix}&{format}>")
        }
    }

    fn plain() -> Style {
        Style::default()
    }

    fn text(text: &str, style: Style) -> Piece {
        Piece::Text {
            text: text.to_owned(),
            style,
        }
    }

    fn link(url: &str) -> Style {
        Style {
            link: Some(url.to_owned()),
            ..plain()
        }
    }

    fn markup(content: &str) -> String {
        to_markup(&parse(content), &Names, false)
    }

    /// Asserts that `content` parses to a single unstyled text piece.
    fn assert_literal(content: &str) {
        assert_eq!(parse(content), vec![text(content, plain())], "{content:?}");
    }

    /// Checks that markup is well formed: only known tags, properly nested,
    /// no nested links, quoted attributes, and only known entities.
    fn check_markup(markup: &str) -> Result<(), String> {
        const TAGS: [&str; 7] = ["a", "b", "i", "u", "s", "tt", "span"];
        const ENTITIES: [&str; 5] = ["&amp;", "&lt;", "&gt;", "&quot;", "&apos;"];
        let mut stack: Vec<&str> = Vec::new();
        let mut rest = markup;
        while let Some(position) = rest.find(['<', '&']) {
            let tail = &rest[position..];
            if let Some(after) = tail.strip_prefix('&') {
                if !ENTITIES.iter().any(|entity| tail.starts_with(entity)) {
                    return Err(format!("bare ampersand at {tail:.20?}"));
                }
                rest = after;
                continue;
            }
            let end = tail
                .find('>')
                .ok_or_else(|| format!("unterminated tag at {tail:.20?}"))?;
            let tag = &tail[1..end];
            if let Some(name) = tag.strip_prefix('/') {
                match stack.pop() {
                    Some(open) if open == name => {}
                    other => return Err(format!("</{name}> closes {other:?}")),
                }
            } else {
                let name = tag.split(' ').next().unwrap_or("");
                if !TAGS.contains(&name) {
                    return Err(format!("unknown tag <{tag}>"));
                }
                if name == "a" && stack.contains(&"a") {
                    return Err("nested link".to_owned());
                }
                if tag.contains('<') || !tag.matches('"').count().is_multiple_of(2) {
                    return Err(format!("malformed attributes in <{tag}>"));
                }
                stack.push(name);
            }
            rest = &tail[end + 1..];
        }
        if markup.matches('>').count() != markup.matches('<').count() {
            return Err("stray '>'".to_owned());
        }
        match stack.last() {
            Some(open) => Err(format!("unclosed <{open}>")),
            None => Ok(()),
        }
    }

    #[test]
    fn empty_input_has_no_pieces() {
        assert!(parse("").is_empty());
        assert_eq!(markup(""), "");
        assert_eq!(to_plain(&[], &Names, false), "");
    }

    #[test]
    fn plain_text_is_one_piece() {
        assert_literal("just some words, nothing else");
        assert_literal("héllo wörld ✓ 日本語");
    }

    #[test]
    fn parses_basic_emphasis() {
        assert_eq!(
            parse("**bold**"),
            vec![text(
                "bold",
                Style {
                    bold: true,
                    ..plain()
                }
            )]
        );
        assert_eq!(
            parse("*italic* and _italic_"),
            vec![
                text(
                    "italic",
                    Style {
                        italic: true,
                        ..plain()
                    }
                ),
                text(" and ", plain()),
                text(
                    "italic",
                    Style {
                        italic: true,
                        ..plain()
                    }
                ),
            ]
        );
        assert_eq!(
            parse("__under__ ~~gone~~"),
            vec![
                text(
                    "under",
                    Style {
                        underline: true,
                        ..plain()
                    }
                ),
                text(" ", plain()),
                text(
                    "gone",
                    Style {
                        strike: true,
                        ..plain()
                    }
                ),
            ]
        );
    }

    #[test]
    fn parses_bold_italic() {
        let both = Style {
            bold: true,
            italic: true,
            ..plain()
        };
        assert_eq!(parse("***both***"), vec![text("both", both.clone())]);
        assert_eq!(
            parse("___both___"),
            vec![text(
                "both",
                Style {
                    underline: true,
                    italic: true,
                    ..plain()
                }
            )]
        );
        assert_eq!(
            parse("**bold *and italic***"),
            vec![
                text(
                    "bold ",
                    Style {
                        bold: true,
                        ..plain()
                    }
                ),
                text("and italic", both),
            ]
        );
    }

    #[test]
    fn styles_nest_and_combine() {
        assert_eq!(
            parse("~~__**x**__~~ ||*y*||"),
            vec![
                text(
                    "x",
                    Style {
                        bold: true,
                        underline: true,
                        strike: true,
                        ..plain()
                    }
                ),
                text(" ", plain()),
                text(
                    "y",
                    Style {
                        italic: true,
                        spoiler: true,
                        ..plain()
                    }
                ),
            ]
        );
        // Emphasis may span lines.
        assert_eq!(
            parse("**a\nb**"),
            vec![text(
                "a\nb",
                Style {
                    bold: true,
                    ..plain()
                }
            )]
        );
    }

    #[test]
    fn overlapping_styles_stay_flat() {
        assert_eq!(
            parse("**a *b** c*"),
            vec![
                text(
                    "a ",
                    Style {
                        bold: true,
                        ..plain()
                    }
                ),
                text(
                    "b",
                    Style {
                        bold: true,
                        italic: true,
                        ..plain()
                    }
                ),
                text(
                    " c",
                    Style {
                        italic: true,
                        ..plain()
                    }
                ),
            ]
        );
        assert_eq!(check_markup(&markup("**a *b** c*")), Ok(()));
    }

    #[test]
    fn unclosed_markers_stay_literal() {
        for content in [
            "**bold",
            "*italic",
            "_italic",
            "__under",
            "~~strike",
            "||spoiler",
            "`code",
            "``code",
            "```code",
            "a ** b",
            "a * b * c",
            "****",
            "~",
            "|",
            "a | b || c",
            "2 * 3 = 6",
        ] {
            assert_literal(content);
        }
    }

    #[test]
    fn underscores_inside_words_are_literal() {
        assert_literal("snake_case_name");
        assert_literal("a_b and c_d");
        assert_literal(":thumbs_up:");
        // The escaped arm of a shrug is literal, and the other never closes.
        assert_literal_result(r"¯\_(ツ)_/¯", "¯_(ツ)_/¯");
        assert_eq!(
            parse("_snake_case_"),
            vec![text(
                "snake_case",
                Style {
                    italic: true,
                    ..plain()
                }
            )]
        );
    }

    #[test]
    fn italic_stars_must_hug_their_content() {
        assert_literal("x * not italic*");
        assert_literal("a * b* c");
        assert_eq!(
            parse("a *b* c"),
            vec![
                text("a ", plain()),
                text(
                    "b",
                    Style {
                        italic: true,
                        ..plain()
                    }
                ),
                text(" c", plain()),
            ]
        );
    }

    #[test]
    fn parses_spoilers() {
        assert_eq!(
            parse("||secret|| and || spaced ||"),
            vec![
                text(
                    "secret",
                    Style {
                        spoiler: true,
                        ..plain()
                    }
                ),
                text(" and ", plain()),
                text(
                    " spaced ",
                    Style {
                        spoiler: true,
                        ..plain()
                    }
                ),
            ]
        );
    }

    #[test]
    fn backslash_escapes_emit_literals() {
        assert_literal_result(
            r"\*a\* \_b\_ \~\~c\~\~ \|\|d\|\| \`e\` \\ \<@1\> \# \-",
            "*a* _b_ ~~c~~ ||d|| `e` \\ <@1> # -",
        );
        // A backslash before anything else is kept.
        assert_literal(r"C:\path\to and a trailing \");
        // Escaped block markers do not start blocks.
        assert_literal_result(r"\# not a heading", "# not a heading");
        assert_literal_result(r"\- not a bullet", "- not a bullet");
        assert_literal_result(r"\> not a quote", "> not a quote");
    }

    fn assert_literal_result(content: &str, expected: &str) {
        assert_eq!(parse(content), vec![text(expected, plain())], "{content:?}");
    }

    #[test]
    fn parses_inline_code() {
        let code = Style {
            code: true,
            ..plain()
        };
        assert_eq!(
            parse("run `cargo test` now"),
            vec![
                text("run ", plain()),
                text("cargo test", code.clone()),
                text(" now", plain()),
            ]
        );
        assert_eq!(
            parse("``code with ` inside``"),
            vec![text("code with ` inside", code.clone())]
        );
        assert_eq!(parse("`` `tick` ``"), vec![text("`tick`", code.clone())]);
        assert_eq!(
            parse("**`bold code`**"),
            vec![text("bold code", Style { bold: true, ..code })]
        );
    }

    #[test]
    fn nothing_is_interpreted_inside_code() {
        let code = Style {
            code: true,
            ..plain()
        };
        let content = "**b** _i_ <@1> <:e:2> <t:3> @everyone https://x.com [a](https://y.com) \\*";
        assert_eq!(parse(&format!("`{content}`")), vec![text(content, code)]);
        assert_eq!(
            parse(&format!("```\n{content}\n```")),
            vec![text(
                content,
                Style {
                    code_block: true,
                    ..plain()
                }
            )]
        );
    }

    #[test]
    fn parses_fenced_code_blocks() {
        let block = Style {
            code_block: true,
            ..plain()
        };
        assert_eq!(
            parse("```rust\nfn main() {}\n```"),
            vec![text("fn main() {}", block.clone())]
        );
        assert_eq!(
            parse("```\nline one\n  line two\n```"),
            vec![text("line one\n  line two", block.clone())]
        );
        assert_eq!(parse("```text```"), vec![text("text", block.clone())]);
        // A first line that is not a language tag is part of the code.
        assert_eq!(
            parse("```let x = 1;\nlet y = 2;```"),
            vec![text("let x = 1;\nlet y = 2;", block.clone())]
        );
        assert_eq!(
            parse("before\n```\ncode\n```\nafter"),
            vec![
                text("before\n", plain()),
                text("code", block),
                text("\nafter", plain()),
            ]
        );
    }

    #[test]
    fn unclosed_or_empty_fences_stay_literal() {
        assert_literal("```rust\nfn main() {}");
        assert_literal("``````");
        assert_literal("```\n```");
    }

    #[test]
    fn parses_block_quotes() {
        let quote = Style {
            quote: true,
            ..plain()
        };
        assert_eq!(
            parse("> quoted **b**\nplain"),
            vec![
                text("quoted ", quote.clone()),
                text(
                    "b",
                    Style {
                        bold: true,
                        ..quote.clone()
                    }
                ),
                text("\nplain", plain()),
            ]
        );
        assert_eq!(
            parse("> one\n> two\nthree"),
            vec![text("one\ntwo", quote.clone()), text("\nthree", plain())]
        );
        assert_eq!(
            parse("intro\n>>> all\nof this"),
            vec![text("intro\n", plain()), text("all\nof this", quote)]
        );
        assert_literal(">not a quote");
        assert_literal("a > b");
    }

    #[test]
    fn parses_headings() {
        for (content, level) in [("# Title", 1), ("## Title", 2), ("### Title", 3)] {
            assert_eq!(
                parse(content),
                vec![text(
                    "Title",
                    Style {
                        heading: level,
                        ..plain()
                    }
                )]
            );
        }
        assert_eq!(
            parse("# Big *deal*\nbody"),
            vec![
                text(
                    "Big ",
                    Style {
                        heading: 1,
                        ..plain()
                    }
                ),
                text(
                    "deal",
                    Style {
                        heading: 1,
                        italic: true,
                        ..plain()
                    }
                ),
                text("\nbody", plain()),
            ]
        );
        assert_literal("#hashtag");
        assert_literal("#### too deep");
        assert_literal("not # at start");
        assert_literal("# ");
    }

    #[test]
    fn parses_subtext() {
        assert_eq!(
            parse("-# fine print\nnormal"),
            vec![
                text(
                    "fine print",
                    Style {
                        subtext: true,
                        ..plain()
                    }
                ),
                text("\nnormal", plain()),
            ]
        );
        assert_literal("-#nospace");
    }

    #[test]
    fn list_bullets_are_normalised() {
        assert_eq!(
            parse("- one\n* two\n  - nested\n1. three"),
            vec![text("• one\n• two\n  • nested\n1. three", plain())]
        );
        assert_eq!(
            parse("* item with *italic*"),
            vec![
                text("• item with ", plain()),
                text(
                    "italic",
                    Style {
                        italic: true,
                        ..plain()
                    }
                ),
            ]
        );
        assert_literal("-not a bullet");
        assert_literal("a - b");
    }

    #[test]
    fn parses_bare_urls() {
        assert_eq!(
            parse("see https://example.com/a?b=1&c=2, ok"),
            vec![
                text("see ", plain()),
                text(
                    "https://example.com/a?b=1&c=2",
                    link("https://example.com/a?b=1&c=2")
                ),
                text(", ok", plain()),
            ]
        );
        assert_eq!(
            parse("http://example.com/snake_case_path."),
            vec![
                text(
                    "http://example.com/snake_case_path",
                    link("http://example.com/snake_case_path")
                ),
                text(".", plain()),
            ]
        );
        assert_literal("https://");
        assert_literal("ftp://example.com and httpx://nope");
    }

    #[test]
    fn urls_keep_balanced_parentheses() {
        let wiki = "https://en.wikipedia.org/wiki/Rust_(programming_language)";
        assert_eq!(parse(wiki), vec![text(wiki, link(wiki))]);
        assert_eq!(
            parse(&format!("({wiki})!")),
            vec![
                text("(", plain()),
                text(wiki, link(wiki)),
                text(")!", plain())
            ]
        );
        assert_eq!(
            parse("(https://example.com)"),
            vec![
                text("(", plain()),
                text("https://example.com", link("https://example.com")),
                text(")", plain()),
            ]
        );
    }

    #[test]
    fn urls_inside_emphasis_and_spoilers() {
        let url = "https://example.com";
        assert_eq!(
            parse("||https://example.com||"),
            vec![text(
                url,
                Style {
                    spoiler: true,
                    ..link(url)
                }
            )]
        );
        assert_eq!(
            parse("**https://example.com**"),
            vec![text(
                url,
                Style {
                    bold: true,
                    ..link(url)
                }
            )]
        );
    }

    #[test]
    fn parses_angle_bracket_urls() {
        assert_eq!(
            parse("<https://example.com/x_y_z>"),
            vec![text(
                "https://example.com/x_y_z",
                link("https://example.com/x_y_z")
            )]
        );
        assert_literal("<ftp://example.com>");
        assert_literal("<not a link>");
    }

    #[test]
    fn parses_masked_links() {
        let url = "https://example.com";
        for content in [
            "[click](https://example.com)",
            "[click](<https://example.com>)",
            "[click](https://example.com \"a title\")",
            "[click](https://example.com 'a title')",
        ] {
            assert_eq!(parse(content), vec![text("click", link(url))], "{content}");
        }
        let wiki = "https://en.wikipedia.org/wiki/Rust_(programming_language)";
        assert_eq!(
            parse(&format!("see [Rust]({wiki})!")),
            vec![
                text("see ", plain()),
                text("Rust", link(wiki)),
                text("!", plain()),
            ]
        );
        assert_eq!(
            parse("[**bold** text](https://example.com)"),
            vec![
                text(
                    "bold",
                    Style {
                        bold: true,
                        ..link(url)
                    }
                ),
                text(" text", link(url)),
            ]
        );
    }

    #[test]
    fn non_http_masked_links_stay_literal() {
        assert_literal("[x](javascript:alert(1))");
        assert_literal("[x](ftp://example.com)");
        assert_literal("[x](file:///etc/passwd)");
        assert_literal("[x]()");
        assert_literal("[x] (not a link)");
        assert_literal("[](nothing)");
        assert_literal("[unclosed](");
        // The text stays literal but the bare URL is still a link.
        assert_eq!(
            parse("[x](https://example.com"),
            vec![
                text("[x](", plain()),
                text("https://example.com", link("https://example.com")),
            ]
        );
    }

    #[test]
    fn parses_mentions() {
        let mention = |mention| Piece::Mention {
            mention,
            style: plain(),
        };
        assert_eq!(
            parse("<@123><@!456><@&789><#42>"),
            vec![
                mention(Mention::User(Snowflake::new(123))),
                mention(Mention::User(Snowflake::new(456))),
                mention(Mention::Role(Snowflake::new(789))),
                mention(Mention::Channel(Snowflake::new(42))),
            ]
        );
        assert_eq!(
            parse("hi @everyone and @here!"),
            vec![
                text("hi ", plain()),
                mention(Mention::Everyone),
                text(" and ", plain()),
                mention(Mention::Here),
                text("!", plain()),
            ]
        );
        assert_eq!(
            parse("**<@1>**"),
            vec![Piece::Mention {
                mention: Mention::User(Snowflake::new(1)),
                style: Style {
                    bold: true,
                    ..plain()
                },
            }]
        );
    }

    #[test]
    fn malformed_mentions_stay_literal() {
        for content in [
            "<@abc>",
            "<@>",
            "<@&>",
            "<@!12x>",
            "<#general>",
            "<@123",
            "<@99999999999999999999999>",
            "<:name:>",
            "<:name:abc>",
            "<::123>",
            "<:bad name:123>",
            "<a:name>",
            "<b:name:123>",
            "<t:>",
            "<t:abc>",
            "<t:123:Q>",
            "<t:123:RR>",
            "<t:123:>",
            "<>",
            "<",
            "@heretic and @everyone_else",
            "me@example.com",
        ] {
            assert_literal(content);
        }
    }

    #[test]
    fn parses_custom_emoji() {
        assert_eq!(
            parse("<:blob_cat:123> <a:party:456>"),
            vec![
                Piece::Emoji {
                    id: Snowflake::new(123),
                    name: "blob_cat".to_owned(),
                    animated: false,
                },
                text(" ", plain()),
                Piece::Emoji {
                    id: Snowflake::new(456),
                    name: "party".to_owned(),
                    animated: true,
                },
            ]
        );
    }

    #[test]
    fn parses_timestamps() {
        assert_eq!(
            parse("<t:1700000000>"),
            vec![Piece::Timestamp {
                unix: 1_700_000_000,
                format: 'f',
                style: plain(),
            }]
        );
        for format in TIMESTAMP_FORMATS.chars() {
            assert_eq!(
                parse(&format!("<t:1700000000:{format}>")),
                vec![Piece::Timestamp {
                    unix: 1_700_000_000,
                    format,
                    style: plain(),
                }]
            );
        }
        assert_eq!(
            parse("<t:-5:R>"),
            vec![Piece::Timestamp {
                unix: -5,
                format: 'R',
                style: plain(),
            }]
        );
    }

    #[test]
    fn adjacent_text_is_merged_and_newlines_kept() {
        assert_eq!(
            parse("a\\*b < c\n\nd @ e [f] h"),
            vec![text("a*b < c\n\nd @ e [f] h", plain())]
        );
    }

    #[test]
    fn markup_for_inline_styles() {
        assert_eq!(
            markup("**b** *i* __u__ ~~s~~ `c`"),
            "<b>b</b> <i>i</i> <u>u</u> <s>s</s> <tt>c</tt>"
        );
        assert_eq!(markup("***x***"), "<b><i>x</i></b>");
    }

    #[test]
    fn markup_escapes_text() {
        assert_eq!(
            markup("<span>hi</span> & \"bye\""),
            "&lt;span&gt;hi&lt;/span&gt; &amp; \"bye\""
        );
        assert_eq!(
            markup("`<tt> & </tt>`"),
            "<tt>&lt;tt&gt; &amp; &lt;/tt&gt;</tt>"
        );
        assert!(!markup("<script>alert(1)</script>").contains("<script>"));
        // Control characters never reach Pango.
        assert_eq!(markup("a\0b\u{7}c\td"), "abc\td");
    }

    #[test]
    fn markup_escapes_link_targets() {
        assert_eq!(
            markup("https://example.com/?a=1&b=2"),
            "<a href=\"https://example.com/?a=1&amp;b=2\">https://example.com/?a=1&amp;b=2</a>"
        );
        // Quotes cannot break out of the attribute.
        let rendered = markup("[x](<https://example.com/\"onclick=\"evil'&>)");
        assert_eq!(
            rendered,
            "<a href=\"https://example.com/&quot;onclick=&quot;evil&apos;&amp;\">x</a>"
        );
        assert_eq!(check_markup(&rendered), Ok(()));
        let rendered = markup("https://example.com/\"onclick=\"evil <b>");
        assert!(rendered.starts_with("<a href=\"https://example.com/&quot;onclick=&quot;evil\">"));
        assert_eq!(check_markup(&rendered), Ok(()));
    }

    #[test]
    fn markup_for_headings_and_subtext() {
        assert_eq!(
            markup("# One\n## Two\n### Three"),
            "<span size=\"x-large\" weight=\"bold\">One</span>\n\
             <span size=\"large\" weight=\"bold\">Two</span>\n\
             <span size=\"medium\" weight=\"bold\">Three</span>"
        );
        assert_eq!(
            markup("-# small"),
            "<span size=\"small\" alpha=\"65%\">small</span>"
        );
    }

    #[test]
    fn markup_prefixes_quoted_lines() {
        let prefix = "<span alpha=\"50%\">\u{258E} </span>";
        assert_eq!(
            markup("> one **two**\n> three\nfour"),
            format!("{prefix}one <b>two</b>\n{prefix}three\nfour")
        );
        assert_eq!(
            markup(">>> a\n\n<@1> b"),
            format!(
                "{prefix}a\n{prefix}\n{prefix}<a href=\"kubecord://user/1\">\
                 <span background=\"#5865f2\" bgalpha=\"25%\">@Alice</span></a> b"
            )
        );
        assert_eq!(markup("> <:wave:5> hi"), format!("{prefix}:wave: hi"));
    }

    #[test]
    fn markup_puts_code_blocks_on_their_own_lines() {
        assert_eq!(markup("a ```x y\nz``` b"), "a \n<tt>x y\nz</tt>\n b");
        assert_eq!(
            markup("before\n```rust\nlet a = 1 < 2;\n```\nafter"),
            "before\n<tt>let a = 1 &lt; 2;</tt>\nafter"
        );
        assert_eq!(markup("```one``````two```"), "<tt>one</tt>\n<tt>two</tt>");
        let prefix = "<span alpha=\"50%\">\u{258E} </span>";
        assert_eq!(
            markup(">>> ```\na\nb\n```"),
            format!("{prefix}<tt>a</tt>\n{prefix}<tt>b</tt>")
        );
    }

    #[test]
    fn markup_for_mentions() {
        assert_eq!(
            markup("<@1>"),
            "<a href=\"kubecord://user/1\">\
             <span background=\"#5865f2\" bgalpha=\"25%\">@Alice</span></a>"
        );
        assert_eq!(
            markup("<#4>"),
            "<a href=\"kubecord://channel/4\">\
             <span background=\"#5865f2\" bgalpha=\"25%\">#general</span></a>"
        );
        assert_eq!(
            markup("<@&2>"),
            "<span background=\"#ff8000\" bgalpha=\"25%\" foreground=\"#ff8000\">@Mods</span>"
        );
        assert_eq!(
            markup("<@&3>"),
            "<span background=\"#5865f2\" bgalpha=\"25%\">@Plain</span>"
        );
        assert_eq!(
            markup("@everyone @here"),
            "<span background=\"#5865f2\" bgalpha=\"25%\"><b>@everyone</b></span> \
             <span background=\"#5865f2\" bgalpha=\"25%\"><b>@here</b></span>"
        );
    }

    #[test]
    fn mentions_fall_back_to_placeholder_names() {
        assert!(markup("<@77>").contains(">@unknown-user<"));
        assert!(markup("<@77>").contains("kubecord://user/77"));
        assert!(markup("<@&77>").contains(">@unknown-role<"));
        assert!(markup("<#77>").contains(">#unknown-channel<"));
        assert_eq!(
            to_plain(&parse("<@77> <@&77> <#77>"), &Names, false),
            "@unknown-user @unknown-role #unknown-channel"
        );
    }

    #[test]
    fn resolved_names_are_escaped() {
        let rendered = markup("<@9>");
        assert!(rendered.contains("@&lt;b&gt;&amp;\"evil\""));
        assert_eq!(check_markup(&rendered), Ok(()));
    }

    #[test]
    fn markup_for_emoji_and_timestamps() {
        assert_eq!(markup("<:blob:1><a:spin:2>"), ":blob::spin:");
        assert_eq!(
            markup("<t:60:R>"),
            "<span background=\"#808080\" bgalpha=\"20%\">&lt;60&amp;R&gt;</span>"
        );
    }

    #[test]
    fn spoilers_are_hidden_until_revealed() {
        let pieces = parse("a ||secret <b>|| z");
        assert_eq!(
            to_markup(&pieces, &Names, false),
            "a <a href=\"kubecord://spoiler\">\
             <span foreground=\"#202225\" background=\"#202225\">secret &lt;b&gt;</span></a> z"
        );
        assert_eq!(
            to_markup(&pieces, &Names, true),
            "a <span background=\"#808080\" bgalpha=\"25%\">secret &lt;b&gt;</span> z"
        );
    }

    #[test]
    fn hidden_spoilers_never_nest_links() {
        let pieces = parse("||<@1> https://example.com [x](https://example.com) <t:1>||");
        let hidden = to_markup(&pieces, &Names, false);
        assert_eq!(check_markup(&hidden), Ok(()));
        assert!(!hidden.contains("kubecord://user"));
        assert!(!hidden.contains("https://example.com\""));
        assert!(!hidden.contains("bgalpha"));
        let revealed = to_markup(&pieces, &Names, true);
        assert_eq!(check_markup(&revealed), Ok(()));
        assert!(revealed.contains("kubecord://user/1"));
        // A mention inside a masked link keeps only the outer link.
        let masked = markup("[go <@1>](https://example.com)");
        assert_eq!(check_markup(&masked), Ok(()));
        assert!(!masked.contains("kubecord://user"));
    }

    #[test]
    fn plain_text_rendering() {
        let pieces = parse(
            "**hi** <@1> in <#4> <@&2> @everyone <:blob:7> <t:5:R> \
             [site](https://example.com) `code`",
        );
        assert_eq!(
            to_plain(&pieces, &Names, false),
            "hi @Alice in #general @Mods @everyone :blob: <5&R> site code"
        );
    }

    #[test]
    fn plain_text_hides_spoilers() {
        let pieces = parse("a ||one **two** <@1>|| b ||c||");
        assert_eq!(
            to_plain(&pieces, &Names, false),
            "a \u{25AE}\u{25AE}\u{25AE}\u{25AE} b \u{25AE}\u{25AE}\u{25AE}\u{25AE}"
        );
        assert_eq!(to_plain(&pieces, &Names, true), "a one two @Alice b c");
    }

    #[test]
    fn detects_emoji_only_messages() {
        assert!(is_emoji_only(&parse("👍 👍🏽")));
        assert!(is_emoji_only(&parse("<:blob:1>")));
        assert!(is_emoji_only(&parse("<:blob:1> <a:spin:2>\n🎉")));
        assert!(is_emoji_only(&parse("🇩🇪 ❤️ 👨‍👩‍👧")));
        assert!(!is_emoji_only(&parse("ok 👍")));
        assert!(!is_emoji_only(&parse("👍 1")));
        assert!(!is_emoji_only(&parse("")));
        assert!(!is_emoji_only(&parse("   ")));
        assert!(!is_emoji_only(&parse("<@1> 👍")));
        assert!(!is_emoji_only(&parse("`👍`")));
    }

    #[test]
    fn emoji_only_counts_sequences_once_and_caps_at_thirty() {
        assert_eq!(count_unicode_emoji("👍🏽"), Some(1));
        assert_eq!(count_unicode_emoji("👨‍👩‍👧"), Some(1));
        assert_eq!(count_unicode_emoji("🇩🇪🇫🇷"), Some(2));
        assert_eq!(count_unicode_emoji("❤️"), Some(1));
        assert_eq!(count_unicode_emoji("a"), None);
        assert!(is_emoji_only(&parse(&"👍".repeat(30))));
        assert!(!is_emoji_only(&parse(&"👍".repeat(31))));
        assert!(!is_emoji_only(&parse(&"<:a:1>".repeat(31))));
    }

    #[test]
    fn detects_custom_emoji() {
        assert!(has_custom_emoji(&parse("hi <:blob:1>")));
        assert!(!has_custom_emoji(&parse("hi 👍 :blob:")));
    }

    #[test]
    fn pathological_asterisks_finish_quickly() {
        let started = Instant::now();
        let content = "*".repeat(10_000);
        let pieces = parse(&content);
        assert_eq!(pieces, vec![text(&content, plain())]);
        assert_eq!(check_markup(&to_markup(&pieces, &Names, false)), Ok(()));

        let spaced = "*a ".repeat(10_000);
        assert_eq!(check_markup(&markup(&spaced)), Ok(()));
        let mixed = "**_~~||`".repeat(5_000);
        assert_eq!(check_markup(&markup(&mixed)), Ok(()));
        assert!(started.elapsed() < QUICK, "took {:?}", started.elapsed());
    }

    #[test]
    fn pathological_brackets_finish_quickly() {
        let started = Instant::now();
        let content = "[".repeat(5_000);
        assert_eq!(parse(&content), vec![text(&content, plain())]);

        for unit in [
            "[a](",
            "[a](http://x",
            "[a](<http://x",
            "[a](http://x \"t",
            "<http://x",
            "<@1",
            "[a]",
            "`a``",
            "```a",
        ] {
            let content = unit.repeat(5_000);
            assert_eq!(check_markup(&markup(&content)), Ok(()), "{unit}");
        }
        let closed_late = format!("{}](https://example.com)", "[".repeat(5_000));
        assert_eq!(check_markup(&markup(&closed_late)), Ok(()));
        assert!(started.elapsed() < QUICK, "took {:?}", started.elapsed());
    }

    #[test]
    fn markup_is_balanced_for_nasty_input() {
        let nasty = [
            "",
            " ",
            "\n",
            "\n\n\n",
            "*",
            "**",
            "***",
            "****a****",
            "**a*b**c*",
            "*a**b*c**",
            "__a_b__c_",
            "~~a||b~~c||",
            "||a**b||c**",
            "**a `b** c`",
            "`a **b` c**",
            "```a **b``` c**",
            "> **a\n> b**\nc",
            ">>> # h\n-# s\n- l\n```\nc\n```\n||s||",
            "# **h\nb**",
            "# ```a\nb``` c",
            "-# ||x\ny||",
            "> ",
            ">>> ",
            "> \n> \n",
            "[a](https://x.com) [b](https://y.com",
            "[**a](https://x.com)**",
            "[a [b](https://x.com)](https://y.com)",
            "[<@1> ||x||](https://x.com)",
            "[`code`](https://x.com \"t\")",
            "||[a](https://x.com)|| ||<@1>|| ||<#4>|| ||<@&2>|| ||@here|| ||<t:1>||",
            "**<@1> <@&2> <#4> @everyone <t:1:R> <:e:1>**",
            "<<<>>> <@<@1>> <<:a:1>> <t:<t:1>>",
            "https://x.com/<b>\"'&;</b>",
            "<https://x.com/\"'&>",
            "&amp; &lt; &gt; &quot; &#x41; &",
            "</b></i></span></a><a href=\"x\">",
            "\\",
            "\\\\\\*\\",
            "\u{0}\u{1}\u{1b}[31m\u{7f}",
            "\u{202e}rtl \u{200d}\u{fe0f} 👨‍👩‍👧 🇩🇪 e\u{301}",
            "* * *\n- - -\n_ _ _\n| | |\n~ ~ ~\n` ` `",
            "```\n```\n```\n```",
            "``` ``` ``` `` `` ` `",
            "> ```\n> code\n> ```",
            "@everyone@here@everyone",
            "a\r\nb\r\n> c\r\n# d\r\n",
        ];
        assert!(nasty.len() >= 40);
        for content in nasty {
            let pieces = parse(content);
            for reveal in [false, true] {
                let rendered = to_markup(&pieces, &Names, reveal);
                assert_eq!(
                    check_markup(&rendered),
                    Ok(()),
                    "{content:?} -> {rendered:?}"
                );
            }
            // Plain rendering must not panic either.
            let _ = to_plain(&pieces, &Names, false);
            let _ = is_emoji_only(&pieces);
        }
    }

    #[test]
    fn every_prefix_of_a_busy_message_renders_balanced() {
        let content = "> **a** _b_ ||c|| `d` ```e\nf``` <@1> <:g:2> <t:3:R> \
                       [h](https://i.jk \"l\") https://m.no/(p) é👍\n# q\n-# r\n- s";
        for (end, _) in content.char_indices() {
            let rendered = markup(&content[..end]);
            assert_eq!(check_markup(&rendered), Ok(()), "{:?}", &content[..end]);
        }
    }

    #[test]
    fn style_fields_set_by_hand_render_safely() {
        // Pieces need not come from `parse`.
        let pieces = vec![
            text(
                "x<\ny",
                Style {
                    bold: true,
                    italic: true,
                    underline: true,
                    strike: true,
                    code: true,
                    code_block: true,
                    spoiler: true,
                    quote: true,
                    heading: 9,
                    subtext: true,
                    link: Some("not\"a<url>".to_owned()),
                },
            ),
            text("", plain()),
            Piece::Emoji {
                id: Snowflake::new(1),
                name: "<b>".to_owned(),
                animated: false,
            },
        ];
        for reveal in [false, true] {
            assert_eq!(check_markup(&to_markup(&pieces, &Names, reveal)), Ok(()));
        }
    }
}
