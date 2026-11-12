//! Small, safe renderers for message content and metadata.
//!
//! Message content is untrusted: it is escaped before any markup is added, so
//! the only tags that reach Pango are the ones this module emits.

/// A piece of a message once code spans and links have been split out.
enum Piece {
    Text(String),
    Code(String),
    Link { url: String, text: String },
}

/// Renders Discord message content as Pango markup.
///
/// Supports `**bold**`, `*italic*`, `~~strike~~`, `` `code` `` and bare
/// `http(s)://` links. Input is always escaped first, so raw markup, entities
/// and attribute-breaking quotes cannot pass through.
pub fn render_markdown(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 16);
    for piece in tokenize(input) {
        match piece {
            Piece::Text(text) => out.push_str(&render_emphasis(&text)),
            Piece::Code(code) => {
                out.push_str("<tt>");
                out.push_str(&escape_text(&code));
                out.push_str("</tt>");
            }
            Piece::Link { url, text } => {
                out.push_str("<a href=\"");
                out.push_str(&escape_attribute(&url));
                out.push_str("\">");
                out.push_str(&escape_text(&text));
                out.push_str("</a>");
            }
        }
    }
    out
}

/// Formats a Discord ISO8601 timestamp as `HH:MM`.
///
/// Timestamps are parsed lightly: only the wall-clock part is used and no
/// timezone conversion is attempted. Anything unexpected yields an empty
/// string, which the message row treats as "no timestamp".
pub fn format_timestamp(timestamp: &str) -> String {
    let Some((_, time)) = timestamp.split_once('T') else {
        return String::new();
    };
    let mut parts = time.split(':');
    let (Some(hours), Some(minutes)) = (parts.next(), parts.next()) else {
        return String::new();
    };
    let valid = hours.len() == 2
        && minutes.len() >= 2
        && hours.bytes().all(|byte| byte.is_ascii_digit())
        && minutes.bytes().take(2).all(|byte| byte.is_ascii_digit());
    if !valid {
        return String::new();
    }
    format!("{hours}:{}", &minutes[..2])
}

/// Human-readable byte count for attachment rows.
pub fn format_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let bytes_f = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes_f < KIB * KIB {
        format!("{:.1} KiB", bytes_f / KIB)
    } else if bytes_f < KIB * KIB * KIB {
        format!("{:.1} MiB", bytes_f / (KIB * KIB))
    } else {
        format!("{:.1} GiB", bytes_f / (KIB * KIB * KIB))
    }
}

fn tokenize(input: &str) -> Vec<Piece> {
    let mut pieces = Vec::new();
    let mut rest = input;
    loop {
        let Some(start) = rest.find('`') else {
            split_links(rest, &mut pieces);
            break;
        };
        let (before, after) = rest.split_at(start);
        split_links(before, &mut pieces);
        let after = &after[1..];
        match after.find('`') {
            Some(end) => {
                pieces.push(Piece::Code(after[..end].to_owned()));
                rest = &after[end + 1..];
            }
            None => {
                // Unmatched backtick: keep it as literal text.
                let mut literal = String::from("`");
                literal.push_str(after);
                split_links(&literal, &mut pieces);
                break;
            }
        }
    }
    pieces
}

fn split_links(text: &str, pieces: &mut Vec<Piece>) {
    let mut rest = text;
    loop {
        let start = match (rest.find("http://"), rest.find("https://")) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };
        let Some(start) = start else {
            if !rest.is_empty() {
                pieces.push(Piece::Text(rest.to_owned()));
            }
            return;
        };
        if start > 0 {
            pieces.push(Piece::Text(rest[..start].to_owned()));
        }
        let tail = &rest[start..];
        let mut end = tail
            .find(|ch: char| ch.is_whitespace() || matches!(ch, '<' | '>' | '"' | '\''))
            .unwrap_or(tail.len());
        // Keep sentence punctuation out of the link target.
        while end > 0 {
            let last = tail[..end].chars().next_back().expect("non-empty prefix");
            if matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}') {
                end -= last.len_utf8();
            } else {
                break;
            }
        }
        let url = &tail[..end];
        pieces.push(Piece::Link {
            url: url.to_owned(),
            text: url.to_owned(),
        });
        rest = &tail[end..];
    }
}

fn render_emphasis(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    let mut open: Vec<&'static str> = Vec::new();
    let mut index = 0;
    while index < text.len() {
        let rest = &text[index..];
        if rest.starts_with("**") {
            toggle(&mut out, &mut open, "b");
            index += 2;
            continue;
        }
        if rest.starts_with("~~") {
            toggle(&mut out, &mut open, "s");
            index += 2;
            continue;
        }
        let ch = rest.chars().next().expect("non-empty suffix");
        if ch == '*' {
            toggle(&mut out, &mut open, "i");
            index += ch.len_utf8();
            continue;
        }
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
        index += ch.len_utf8();
    }
    while let Some(tag) = open.pop() {
        out.push_str(close_tag(tag));
    }
    out
}

/// Toggles a formatting tag, closing and reopening inner tags as needed so the
/// output is always properly nested.
fn toggle(out: &mut String, open: &mut Vec<&'static str>, tag: &'static str) {
    if let Some(position) = open.iter().rposition(|active| *active == tag) {
        let mut reopen = Vec::new();
        while open.len() > position {
            let top = open.pop().expect("open tag");
            out.push_str(close_tag(top));
            if top != tag {
                reopen.push(top);
            }
        }
        for inner in reopen.into_iter().rev() {
            out.push_str(open_tag(inner));
            open.push(inner);
        }
    } else {
        out.push_str(open_tag(tag));
        open.push(tag);
    }
}

fn open_tag(tag: &str) -> &'static str {
    match tag {
        "b" => "<b>",
        "i" => "<i>",
        "s" => "<s>",
        _ => "",
    }
}

fn close_tag(tag: &str) -> &'static str {
    match tag {
        "b" => "</b>",
        "i" => "</i>",
        "s" => "</s>",
        _ => "",
    }
}

fn escape_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
}

fn escape_attribute(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_raw_markup() {
        assert_eq!(
            render_markdown("<span>hi</span> & bye"),
            "&lt;span&gt;hi&lt;/span&gt; &amp; bye"
        );
        assert!(!render_markdown("<script>alert(1)</script>").contains("<script>"));
    }

    #[test]
    fn renders_emphasis_and_code() {
        assert_eq!(
            render_markdown("**bold** *italic* ~~gone~~ `code`"),
            "<b>bold</b> <i>italic</i> <s>gone</s> <tt>code</tt>"
        );
    }

    #[test]
    fn code_spans_are_not_formatted() {
        assert_eq!(render_markdown("`**not bold**`"), "<tt>**not bold**</tt>");
        assert_eq!(
            render_markdown("`<tag> & stuff`"),
            "<tt>&lt;tag&gt; &amp; stuff</tt>"
        );
    }

    #[test]
    fn tags_are_always_nested() {
        // Interleaved delimiters must not produce crossing tags.
        assert_eq!(render_markdown("**a *b** c*"), "<b>a <i>b</i></b><i> c</i>");
        // Unbalanced delimiters are closed at the end of the text.
        assert_eq!(render_markdown("**bold"), "<b>bold</b>");
        assert_eq!(render_markdown("~~gone"), "<s>gone</s>");
    }

    #[test]
    fn renders_bare_links() {
        assert_eq!(
            render_markdown("see https://example.com/a?b=1&c=2 now"),
            "see <a href=\"https://example.com/a?b=1&amp;c=2\">https://example.com/a?b=1&amp;c=2</a> now"
        );
        assert_eq!(
            render_markdown("(http://example.com)."),
            "(<a href=\"http://example.com\">http://example.com</a>)."
        );
    }

    #[test]
    fn links_cannot_break_out_of_attributes() {
        let rendered = render_markdown("https://example.com\" onclick=\"evil");
        let href_end = rendered.find("\">").expect("rendered link");
        assert_eq!(&rendered[..href_end], "<a href=\"https://example.com");
        // The quote after the host starts ordinary text, not an attribute.
        assert!(rendered.contains("</a>\" onclick=\"evil"));
    }

    #[test]
    fn formats_timestamps() {
        assert_eq!(
            format_timestamp("2026-11-12T13:45:06.123456+00:00"),
            "13:45"
        );
        assert_eq!(format_timestamp("2026-11-12T09:05:00Z"), "09:05");
        assert_eq!(format_timestamp("not a timestamp"), "");
        assert_eq!(format_timestamp("2026-11-12"), "");
        assert_eq!(format_timestamp("2026-11-12T1:2"), "");
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KiB");
        assert_eq!(format_size(1024 * 1024), "1.0 MiB");
        assert_eq!(format_size(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }
}
