//! Parsing the user's favourite GIFs out of Discord's `FrecencyUserSettings`
//! protobuf.
//!
//! `GET /users/@me/settings-proto/2` returns `{"settings": "<base64>"}` whose
//! bytes are a `FrecencyUserSettings` message. The favourites live in its
//! `favorite_gifs` field (number 1), a message whose `gifs` field (number 1)
//! is a `map<string, FavoriteGif>`. A proto map is encoded as repeated
//! entries, each a length-delimited message with the key at field 1 and the
//! value at field 2. A `FavoriteGif` carries `format` (1, enum: 2 = IMAGE,
//! 3 = VIDEO), `src` (2), `width` (3), `height` (4) and `order` (5).
//!
//! Rather than depend on prost and a build step, this walks the wire format
//! by hand, reading only the fields it needs and skipping the rest. Anything
//! it cannot parse is treated as "no favourites" (the caller logs and carries
//! on); it never panics on malformed input.

/// One favourite GIF read from the settings proto.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FavoriteGif {
    pub src: String,
    pub width: u32,
    pub height: u32,
    pub order: u32,
    /// `format` enum: 2 is an image (a real GIF), 3 is a video (an MP4 gifv).
    pub format: u32,
}

/// A protobuf wire type.
const VARINT: u64 = 0;
const LEN: u64 = 2;
const I64: u64 = 1;
const I32: u64 = 5;

/// Reads a base-128 varint at `*pos`, advancing it. Returns `None` on a
/// truncated or overlong varint.
fn read_varint(bytes: &[u8], pos: &mut usize) -> Option<u64> {
    let mut value: u64 = 0;
    let mut shift = 0u32;
    loop {
        let byte = *bytes.get(*pos)?;
        *pos += 1;
        value |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
}

/// Reads a length-delimited field's bytes at `*pos`, advancing past them.
fn read_len<'a>(bytes: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
    let len = read_varint(bytes, pos)? as usize;
    let end = pos.checked_add(len)?;
    let slice = bytes.get(*pos..end)?;
    *pos = end;
    Some(slice)
}

/// Skips a field of the given wire type, advancing `*pos`. Returns `false`
/// if the field is malformed (so the caller stops parsing this message).
fn skip_field(bytes: &[u8], pos: &mut usize, wire: u64) -> bool {
    match wire {
        VARINT => read_varint(bytes, pos).is_some(),
        LEN => read_len(bytes, pos).is_some(),
        I64 => {
            *pos = match pos.checked_add(8) {
                Some(next) if next <= bytes.len() => next,
                _ => return false,
            };
            true
        }
        I32 => {
            *pos = match pos.checked_add(4) {
                Some(next) if next <= bytes.len() => next,
                _ => return false,
            };
            true
        }
        // Group wire types (3, 4) are obsolete and not emitted here.
        _ => false,
    }
}

/// Visits each `(field_number, wire_type, position)` in a message, calling
/// `on_field` which must consume the field's value from `pos` (via the helper
/// readers) and return `true` to continue. Fields `on_field` does not consume
/// are skipped. Stops silently on any malformed field.
fn walk(bytes: &[u8], mut on_field: impl FnMut(u64, u64, &[u8], &mut usize) -> bool) {
    let mut pos = 0;
    while pos < bytes.len() {
        let Some(tag) = read_varint(bytes, &mut pos) else {
            return;
        };
        let field = tag >> 3;
        let wire = tag & 0x7;
        let before = pos;
        if !on_field(field, wire, bytes, &mut pos) {
            return;
        }
        // If the handler did not advance, skip the field itself.
        if pos == before && !skip_field(bytes, &mut pos, wire) {
            return;
        }
    }
}

/// Parses one `FavoriteGif` message.
fn parse_favorite(bytes: &[u8]) -> FavoriteGif {
    let mut gif = FavoriteGif {
        src: String::new(),
        width: 0,
        height: 0,
        order: 0,
        format: 0,
    };
    walk(bytes, |field, wire, data, pos| {
        match (field, wire) {
            (1, VARINT) => gif.format = read_varint(data, pos).unwrap_or(0) as u32,
            (2, LEN) => {
                if let Some(src) = read_len(data, pos) {
                    gif.src = String::from_utf8_lossy(src).into_owned();
                }
            }
            (3, VARINT) => gif.width = read_varint(data, pos).unwrap_or(0) as u32,
            (4, VARINT) => gif.height = read_varint(data, pos).unwrap_or(0) as u32,
            (5, VARINT) => gif.order = read_varint(data, pos).unwrap_or(0) as u32,
            _ => return true, // Skip anything else.
        }
        true
    });
    gif
}

/// Parses one map entry (`{ string key = 1; FavoriteGif value = 2; }`),
/// returning the value's `FavoriteGif` (the key is the GIF id, unused here).
fn parse_map_entry(bytes: &[u8]) -> Option<FavoriteGif> {
    let mut found = None;
    walk(bytes, |field, wire, data, pos| {
        if field == 2
            && wire == LEN
            && let Some(value) = read_len(data, pos)
        {
            found = Some(parse_favorite(value));
        }
        true
    });
    found
}

/// Parses the `FavoriteGifs` message (field 1 `map<string, FavoriteGif>`),
/// collecting its entries ordered by each gif's `order`.
fn parse_favorite_gifs(bytes: &[u8]) -> Vec<FavoriteGif> {
    let mut gifs = Vec::new();
    walk(bytes, |field, wire, data, pos| {
        if field == 1
            && wire == LEN
            && let Some(entry) = read_len(data, pos)
            && let Some(gif) = parse_map_entry(entry)
        {
            gifs.push(gif);
        }
        true
    });
    gifs.sort_by_key(|gif| gif.order);
    gifs
}

/// Parses the user's favourite GIFs out of the decoded `FrecencyUserSettings`
/// proto bytes (field 1 `favorite_gifs`), ordered by the saved order.
pub(crate) fn parse_settings_proto(bytes: &[u8]) -> Vec<FavoriteGif> {
    let mut gifs = Vec::new();
    walk(bytes, |field, wire, data, pos| {
        if field == 1
            && wire == LEN
            && let Some(favorites) = read_len(data, pos)
        {
            gifs = parse_favorite_gifs(favorites);
        }
        true
    });
    gifs
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Appends a protobuf tag (`field << 3 | wire`) as a varint.
    fn tag(out: &mut Vec<u8>, field: u64, wire: u64) {
        varint(out, field << 3 | wire);
    }

    fn varint(out: &mut Vec<u8>, mut value: u64) {
        loop {
            let mut byte = (value & 0x7F) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if value == 0 {
                break;
            }
        }
    }

    /// A length-delimited field: tag, length, then the bytes.
    fn len_field(out: &mut Vec<u8>, field: u64, bytes: &[u8]) {
        tag(out, field, LEN);
        varint(out, bytes.len() as u64);
        out.extend_from_slice(bytes);
    }

    fn varint_field(out: &mut Vec<u8>, field: u64, value: u64) {
        tag(out, field, VARINT);
        varint(out, value);
    }

    /// Encodes one `FavoriteGif`.
    fn favorite(format: u32, src: &str, width: u32, height: u32, order: u32) -> Vec<u8> {
        let mut out = Vec::new();
        varint_field(&mut out, 1, format as u64);
        len_field(&mut out, 2, src.as_bytes());
        varint_field(&mut out, 3, width as u64);
        varint_field(&mut out, 4, height as u64);
        varint_field(&mut out, 5, order as u64);
        out
    }

    /// Encodes a map entry `{ key = 1; value = 2; }`.
    fn map_entry(key: &str, value: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        len_field(&mut out, 1, key.as_bytes());
        len_field(&mut out, 2, value);
        out
    }

    /// Builds a full `FrecencyUserSettings` blob with the given favourites,
    /// base64-encoded as Discord returns it.
    fn settings_blob(entries: &[(&str, Vec<u8>)]) -> String {
        use base64::Engine;
        // FavoriteGifs: repeated map entries at field 1, plus an unrelated
        // bool field 2 to confirm it is skipped.
        let mut favorites = Vec::new();
        for (key, value) in entries {
            len_field(&mut favorites, 1, &map_entry(key, value));
        }
        varint_field(&mut favorites, 2, 1); // hide_tooltip = true
        // FrecencyUserSettings: favorite_gifs at field 1, plus an unrelated
        // length-delimited field 7 to confirm skipping works.
        let mut settings = Vec::new();
        len_field(&mut settings, 1, &favorites);
        len_field(&mut settings, 7, b"ignored");
        base64::engine::general_purpose::STANDARD.encode(settings)
    }

    #[test]
    fn varints_round_trip() {
        for value in [0u64, 1, 127, 128, 300, 16_384, u64::MAX] {
            let mut out = Vec::new();
            varint(&mut out, value);
            let mut pos = 0;
            assert_eq!(read_varint(&out, &mut pos), Some(value));
            assert_eq!(pos, out.len());
        }
        // A truncated varint (continuation bit with no next byte) fails.
        let mut pos = 0;
        assert_eq!(read_varint(&[0x80], &mut pos), None);
    }

    #[test]
    fn parses_favorites_in_order_from_a_canned_blob() {
        // Two image gifs and one video, deliberately out of `order` so the
        // parser's sort is exercised.
        let blob = settings_blob(&[
            (
                "gif-b",
                favorite(2, "https://media.tenor.com/b.gif", 200, 150, 1),
            ),
            (
                "gif-a",
                favorite(2, "https://media.tenor.com/a.gif", 100, 100, 0),
            ),
            (
                "vid-c",
                favorite(3, "https://media.tenor.com/c.mp4", 320, 240, 2),
            ),
        ]);
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(blob)
            .expect("valid base64");
        let gifs = parse_settings_proto(&bytes);
        assert_eq!(gifs.len(), 3);
        // Ordered by `order`: a (0), b (1), c (2).
        assert_eq!(gifs[0].src, "https://media.tenor.com/a.gif");
        assert_eq!(gifs[0].format, 2);
        assert_eq!((gifs[0].width, gifs[0].height), (100, 100));
        assert_eq!(gifs[1].src, "https://media.tenor.com/b.gif");
        assert_eq!(gifs[1].order, 1);
        assert_eq!(gifs[2].src, "https://media.tenor.com/c.mp4");
        assert_eq!(gifs[2].format, 3); // video
    }

    #[test]
    fn empty_or_garbage_yields_no_favorites() {
        assert!(parse_settings_proto(&[]).is_empty());
        // Random bytes must not panic and should yield nothing usable.
        let _ = parse_settings_proto(&[0xFF, 0x00, 0x80, 0x42, 0x13, 0x37]);
        // A settings blob with no favorite_gifs field.
        let mut settings = Vec::new();
        len_field(&mut settings, 7, b"something else");
        assert!(parse_settings_proto(&settings).is_empty());
    }
}
