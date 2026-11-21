//! Parsing the user's favourite GIFs out of Discord's `FrecencyUserSettings`
//! protobuf.
//!
//! `GET /users/@me/settings-proto/2` returns `{"settings": "<base64>"}` whose
//! bytes are a `FrecencyUserSettings` message. The favourites live in its
//! `favorite_gifs` field (number 2), a message whose `gifs` field (number 1)
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

/// `FavoriteGif.format` for a real `image/gif` (a GIF animated from its bytes).
#[cfg(test)]
pub(crate) const FORMAT_IMAGE: u32 = 2;
/// `FavoriteGif.format` for a video (an MP4 `gifv`).
#[cfg(test)]
pub(crate) const FORMAT_VIDEO: u32 = 3;

/// A protobuf wire type.
const VARINT: u64 = 0;
const LEN: u64 = 2;
const I64: u64 = 1;
const I32: u64 = 5;

/// `favorite_gifs` is field 2 of `FrecencyUserSettings` (field 1 is
/// `versions`), confirmed against a live settings blob. Its inner `gifs` map
/// is field 1 of the `FavoriteGifs` message.
const FAVORITE_GIFS_FIELD: u64 = 2;

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
    // Discord assigns each new favourite a higher `order` than the last, so
    // sorting descending puts the most recently favourited GIF at the top.
    gifs.sort_by(|a, b| b.order.cmp(&a.order));
    gifs
}

/// Parses the user's favourite GIFs out of the decoded `FrecencyUserSettings`
/// proto bytes (field 2 `favorite_gifs`), ordered by the saved order.
pub(crate) fn parse_settings_proto(bytes: &[u8]) -> Vec<FavoriteGif> {
    let mut gifs = Vec::new();
    walk(bytes, |field, wire, data, pos| {
        if field == FAVORITE_GIFS_FIELD
            && wire == LEN
            && let Some(favorites) = read_len(data, pos)
        {
            gifs = parse_favorite_gifs(favorites);
        }
        true
    });
    gifs
}

// -- writing --------------------------------------------------------------
//
// Favouriting or unfavouriting a GIF means rewriting this proto and PATCHing
// it back. The settings blob also holds the user's favourite emoji and
// stickers and all their frecency data, so the rewrite MUST preserve every
// other field byte-for-byte. The approach: split the top-level message into
// its fields as raw (tag, value) byte spans, in order; only the
// `favorite_gifs` field (number 2) is parsed one level deeper (again keeping
// its non-`gifs` fields raw) so just the `gifs` map is changed; then
// everything is re-serialised in the original order. Unknown fields -- at
// either level -- survive untouched.

/// One top-level (or nested) protobuf field, captured as its raw value bytes
/// so it can be re-emitted verbatim. `number`/`wire` are kept to rebuild the
/// tag.
struct RawField {
    number: u64,
    wire: u64,
    value: Vec<u8>,
}

/// Appends a varint.
fn write_varint(out: &mut Vec<u8>, mut value: u64) {
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

/// Appends a field tag (`number << 3 | wire`).
fn write_tag(out: &mut Vec<u8>, number: u64, wire: u64) {
    write_varint(out, number << 3 | wire);
}

/// Appends a varint field.
fn write_varint_field(out: &mut Vec<u8>, number: u64, value: u64) {
    write_tag(out, number, VARINT);
    write_varint(out, value);
}

/// Appends a length-delimited field (tag, length, bytes).
fn write_len_field(out: &mut Vec<u8>, number: u64, bytes: &[u8]) {
    write_tag(out, number, LEN);
    write_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

/// Appends a field, reproducing its wire form from the captured value bytes.
fn write_field(out: &mut Vec<u8>, field: &RawField) {
    write_tag(out, field.number, field.wire);
    // A length-delimited field needs its length prefix re-emitted; VARINT/I64/
    // I32 captured their raw bytes, which already carry the exact encoding.
    if field.wire == LEN {
        write_varint(out, field.value.len() as u64);
    }
    out.extend_from_slice(&field.value);
}

/// Splits a message into its fields in order, each with its raw value bytes.
/// Returns `None` if the bytes are not a well-formed stream of fields (so the
/// caller refuses to rewrite rather than risk dropping data).
fn split_fields(bytes: &[u8]) -> Option<Vec<RawField>> {
    let mut fields = Vec::new();
    let mut pos = 0;
    while pos < bytes.len() {
        let tag = read_varint(bytes, &mut pos)?;
        let number = tag >> 3;
        let wire = tag & 0x7;
        let start = pos;
        match wire {
            VARINT => {
                read_varint(bytes, &mut pos)?;
            }
            LEN => {
                read_len(bytes, &mut pos)?;
            }
            I64 => pos = pos.checked_add(8).filter(|end| *end <= bytes.len())?,
            I32 => pos = pos.checked_add(4).filter(|end| *end <= bytes.len())?,
            // Groups (3, 4) are obsolete and Discord never emits them.
            _ => return None,
        }
        // For a length-delimited field, keep only the payload (not its length
        // prefix); `write_field` re-emits the prefix. For the rest, keep the
        // encoded value bytes as-is.
        let value = if wire == LEN {
            let len_end = pos;
            let payload_start = len_end - read_len_payload_len(bytes, start)?;
            bytes.get(payload_start..len_end)?.to_vec()
        } else {
            bytes.get(start..pos)?.to_vec()
        };
        fields.push(RawField {
            number,
            wire,
            value,
        });
    }
    Some(fields)
}

/// The payload length of the length-delimited field whose length varint starts
/// at `start`. Used by `split_fields` to find where the payload begins.
fn read_len_payload_len(bytes: &[u8], start: usize) -> Option<usize> {
    let mut pos = start;
    Some(read_varint(bytes, &mut pos)? as usize)
}

/// Re-serialises an ordered list of fields.
fn join_fields(fields: &[RawField]) -> Vec<u8> {
    let mut out = Vec::new();
    for field in fields {
        write_field(&mut out, field);
    }
    out
}

/// Encodes one `FavoriteGif` value message.
fn encode_favorite(gif: &FavoriteGif) -> Vec<u8> {
    let mut out = Vec::new();
    write_varint_field(&mut out, 1, u64::from(gif.format));
    write_len_field(&mut out, 2, gif.src.as_bytes());
    write_varint_field(&mut out, 3, u64::from(gif.width));
    write_varint_field(&mut out, 4, u64::from(gif.height));
    write_varint_field(&mut out, 5, u64::from(gif.order));
    out
}

/// Encodes one `gifs` map entry (`{ string key = 1; FavoriteGif value = 2; }`).
fn encode_map_entry(key: &str, value: &FavoriteGif) -> Vec<u8> {
    let mut out = Vec::new();
    write_len_field(&mut out, 1, key.as_bytes());
    write_len_field(&mut out, 2, &encode_favorite(value));
    out
}

/// Reads a map entry's key (field 1 string). Used to match an existing
/// favourite by its URL when unfavouriting, and to find the highest `order`.
fn map_entry_key(bytes: &[u8]) -> Option<String> {
    let mut key = None;
    walk(bytes, |field, wire, data, pos| {
        if field == 1
            && wire == LEN
            && let Some(bytes) = read_len(data, pos)
        {
            key = Some(String::from_utf8_lossy(bytes).into_owned());
        }
        true
    });
    key
}

/// How the `gifs` map of a `FavoriteGifs` message should change.
enum Change<'a> {
    /// Add (or replace) the favourite stored under `url`.
    Add {
        url: &'a str,
        src: &'a str,
        format: u32,
        width: u32,
        height: u32,
    },
    /// Remove the favourite stored under `url`.
    Remove { url: &'a str },
}

/// Rewrites the `favorite_gifs` (`FavoriteGifs`) message, applying `change` to
/// its `gifs` map (field 1) while keeping every other field (e.g. a
/// `hide_tooltip`) verbatim. Returns `None` if the message is malformed.
fn rewrite_favorite_gifs(bytes: &[u8], change: &Change) -> Option<Vec<u8>> {
    let mut fields = split_fields(bytes)?;
    // The map entries are the field-1 length-delimited fields; collect their
    // keys to find the highest order (for an Add) and whether the url exists.
    let mut max_order = None;
    for field in &fields {
        if field.number == 1 && field.wire == LEN {
            let order = parse_map_entry(&field.value).map(|gif| gif.order);
            if let Some(order) = order {
                max_order = Some(max_order.map_or(order, |current: u32| current.max(order)));
            }
        }
    }
    match change {
        Change::Remove { url } => {
            fields.retain(|field| {
                !(field.number == 1
                    && field.wire == LEN
                    && map_entry_key(&field.value).as_deref() == Some(url))
            });
        }
        Change::Add {
            url,
            src,
            format,
            width,
            height,
        } => {
            // Drop any existing entry for this url first, so favouriting twice
            // does not duplicate it (matches a map's set semantics).
            fields.retain(|field| {
                !(field.number == 1
                    && field.wire == LEN
                    && map_entry_key(&field.value).as_deref() == Some(*url))
            });
            let order = max_order.map_or(0, |order| order + 1);
            let entry = encode_map_entry(
                url,
                &FavoriteGif {
                    src: (*src).to_owned(),
                    width: *width,
                    height: *height,
                    order,
                    format: *format,
                },
            );
            // Append the new map entry as the last field-1 entry. Insert it
            // before any trailing non-map fields so map entries stay grouped,
            // though proto does not require this.
            let insert_at = fields
                .iter()
                .rposition(|field| field.number == 1 && field.wire == LEN)
                .map_or(0, |index| index + 1);
            fields.insert(
                insert_at,
                RawField {
                    number: 1,
                    wire: LEN,
                    value: entry,
                },
            );
        }
    }
    Some(join_fields(&fields))
}

/// Applies `change` to the decoded `FrecencyUserSettings` proto bytes,
/// returning the new bytes. Preserves every top-level field verbatim, touching
/// only `favorite_gifs` (field 1). If there is no `favorite_gifs` field yet and
/// the change is an Add, one is created. Returns `None` if the proto is
/// malformed (the caller then refuses to PATCH, rather than risk clobbering the
/// account's settings with a partial blob).
fn apply_change(bytes: &[u8], change: &Change) -> Option<Vec<u8>> {
    let mut fields = split_fields(bytes)?;
    if let Some(favorite_gifs) = fields
        .iter_mut()
        .find(|field| field.number == FAVORITE_GIFS_FIELD && field.wire == LEN)
    {
        favorite_gifs.value = rewrite_favorite_gifs(&favorite_gifs.value, change)?;
    } else if let Change::Add { .. } = change {
        // No favourites yet: build a fresh FavoriteGifs holding just this one,
        // placed first (field order is not significant to proto).
        let value = rewrite_favorite_gifs(&[], change)?;
        fields.insert(
            0,
            RawField {
                number: FAVORITE_GIFS_FIELD,
                wire: LEN,
                value,
            },
        );
    }
    // A Remove against a blob with no favorite_gifs is a no-op (re-emits the
    // original fields unchanged).
    Some(join_fields(&fields))
}

/// Favourites the GIF at `url` (the media URL used as the map key), returning
/// the new `FrecencyUserSettings` proto bytes. Preserves all other settings.
pub(crate) fn add_favorite(
    bytes: &[u8],
    url: &str,
    src: &str,
    format: u32,
    width: u32,
    height: u32,
) -> Option<Vec<u8>> {
    apply_change(
        bytes,
        &Change::Add {
            url,
            src,
            format,
            width,
            height,
        },
    )
}

/// Unfavourites the GIF stored under `url`, returning the new proto bytes.
/// Preserves all other settings.
pub(crate) fn remove_favorite(bytes: &[u8], url: &str) -> Option<Vec<u8>> {
    apply_change(bytes, &Change::Remove { url })
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
        // FrecencyUserSettings: favorite_gifs at field 2, plus an unrelated
        // length-delimited field 7 to confirm skipping works.
        let mut settings = Vec::new();
        len_field(&mut settings, 2, &favorites);
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
        // Ordered by `order` descending (newest first): c (2), b (1), a (0).
        assert_eq!(gifs[0].src, "https://media.tenor.com/c.mp4");
        assert_eq!(gifs[0].format, 3); // video
        assert_eq!((gifs[0].width, gifs[0].height), (320, 240));
        assert_eq!(gifs[1].src, "https://media.tenor.com/b.gif");
        assert_eq!(gifs[1].order, 1);
        assert_eq!(gifs[2].src, "https://media.tenor.com/a.gif");
        assert_eq!(gifs[2].format, 2); // image
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

    // -- round-trip safety ------------------------------------------------
    //
    // These tests are the guarantee that favouriting/unfavouriting never drops
    // any other setting from the account's shared FrecencyUserSettings proto.

    /// Returns the raw bytes (including tag) of the first top-level field with
    /// the given number, or `None`. Used to assert other settings survive
    /// byte-for-byte.
    fn raw_top_field(bytes: &[u8], number: u64) -> Option<Vec<u8>> {
        let mut pos = 0;
        while pos < bytes.len() {
            let tag_start = pos;
            let tag = read_varint(bytes, &mut pos).unwrap();
            let field = tag >> 3;
            let wire = tag & 0x7;
            match wire {
                VARINT => {
                    read_varint(bytes, &mut pos).unwrap();
                }
                LEN => {
                    read_len(bytes, &mut pos).unwrap();
                }
                I64 => pos += 8,
                I32 => pos += 4,
                _ => panic!("unexpected wire type"),
            }
            if field == number {
                return Some(bytes[tag_start..pos].to_vec());
            }
        }
        None
    }

    /// Builds a FrecencyUserSettings-shaped blob with:
    /// - `favorite_gifs` (field 2) holding two gifs AND an extra unknown field
    ///   (a length-delimited field 9) inside it, to prove nested unknown fields
    ///   survive;
    /// - an unknown top-level varint field 5 and an unknown top-level
    ///   length-delimited field 9 (arbitrary bytes) surrounding it, to prove
    ///   top-level unknown fields survive;
    /// - a known-but-unrelated top-level field 7 (e.g. a sibling settings
    ///   message).
    fn settings_with_extras() -> Vec<u8> {
        let mut favorites = Vec::new();
        len_field(
            &mut favorites,
            1,
            &map_entry(
                "https://media.tenor.com/a.gif",
                &favorite(FORMAT_IMAGE, "https://media.tenor.com/a.gif", 100, 100, 0),
            ),
        );
        len_field(
            &mut favorites,
            1,
            &map_entry(
                "https://media.tenor.com/b.mp4",
                &favorite(FORMAT_VIDEO, "https://media.tenor.com/b.mp4", 320, 240, 1),
            ),
        );
        // An unknown field inside FavoriteGifs (e.g. a future bool/message).
        len_field(&mut favorites, 9, b"nested-unknown-bytes");

        let mut settings = Vec::new();
        varint_field(&mut settings, 5, 123_456); // unknown top-level varint
        len_field(&mut settings, 2, &favorites); // favorite_gifs (field 2)
        len_field(&mut settings, 7, b"favorite-stickers-ish"); // unrelated
        len_field(&mut settings, 9, &[0x00, 0xFF, 0x13, 0x37, 0x80]); // unknown LEN
        settings
    }

    #[test]
    fn favouriting_adds_an_entry_and_preserves_everything_else() {
        let original = settings_with_extras();
        // The other fields we must keep byte-for-byte.
        let field5 = raw_top_field(&original, 5).unwrap();
        let field7 = raw_top_field(&original, 7).unwrap();
        let field9 = raw_top_field(&original, 9).unwrap();
        // The nested unknown field inside favorite_gifs.
        let fav_gifs = {
            let fields = split_fields(&original).unwrap();
            fields
                .into_iter()
                .find(|f| f.number == FAVORITE_GIFS_FIELD && f.wire == LEN)
                .unwrap()
                .value
        };
        let nested_unknown = raw_top_field(&fav_gifs, 9).unwrap();

        // Favourite a new image gif.
        let new_url = "https://media.tenor.com/c.gif";
        let updated = add_favorite(&original, new_url, new_url, FORMAT_IMAGE, 200, 150)
            .expect("well-formed proto");

        // (a) favorite_gifs now has three gifs, with the new one's key, src,
        //     format and order (= previous max 1, plus 1 = 2).
        let gifs = parse_settings_proto(&updated);
        assert_eq!(gifs.len(), 3, "a third favourite was added");
        let added = gifs.iter().find(|g| g.order == 2).expect("order is max+1");
        assert_eq!(added.src, new_url);
        assert_eq!(added.format, FORMAT_IMAGE);
        assert_eq!((added.width, added.height), (200, 150));
        // The two originals are still present, unchanged.
        assert!(
            gifs.iter()
                .any(|g| g.src == "https://media.tenor.com/a.gif")
        );
        assert!(
            gifs.iter()
                .any(|g| g.src == "https://media.tenor.com/b.mp4" && g.format == FORMAT_VIDEO)
        );

        // (b) every OTHER top-level field is byte-identical.
        assert_eq!(raw_top_field(&updated, 5).unwrap(), field5);
        assert_eq!(raw_top_field(&updated, 7).unwrap(), field7);
        assert_eq!(raw_top_field(&updated, 9).unwrap(), field9);

        // (c) favorite_gifs's own unknown field survived.
        let new_fav_gifs = {
            let fields = split_fields(&updated).unwrap();
            fields
                .into_iter()
                .find(|f| f.number == FAVORITE_GIFS_FIELD && f.wire == LEN)
                .unwrap()
                .value
        };
        assert_eq!(raw_top_field(&new_fav_gifs, 9).unwrap(), nested_unknown);

        // Now unfavourite the same gif → back to two, others still identical.
        let reverted = remove_favorite(&updated, new_url).expect("well-formed proto");
        let gifs = parse_settings_proto(&reverted);
        assert_eq!(gifs.len(), 2, "the added favourite was removed");
        assert!(!gifs.iter().any(|g| g.src == new_url));
        assert!(
            gifs.iter()
                .any(|g| g.src == "https://media.tenor.com/a.gif")
        );
        assert!(
            gifs.iter()
                .any(|g| g.src == "https://media.tenor.com/b.mp4")
        );

        assert_eq!(raw_top_field(&reverted, 5).unwrap(), field5);
        assert_eq!(raw_top_field(&reverted, 7).unwrap(), field7);
        assert_eq!(raw_top_field(&reverted, 9).unwrap(), field9);
        let reverted_fav_gifs = {
            let fields = split_fields(&reverted).unwrap();
            fields
                .into_iter()
                .find(|f| f.number == FAVORITE_GIFS_FIELD && f.wire == LEN)
                .unwrap()
                .value
        };
        assert_eq!(
            raw_top_field(&reverted_fav_gifs, 9).unwrap(),
            nested_unknown
        );
    }

    #[test]
    fn favouriting_the_same_url_twice_does_not_duplicate() {
        let original = settings_with_extras();
        let url = "https://media.tenor.com/dup.gif";
        let once = add_favorite(&original, url, url, FORMAT_IMAGE, 10, 10).unwrap();
        let twice = add_favorite(&once, url, url, FORMAT_IMAGE, 10, 10).unwrap();
        let count = parse_settings_proto(&twice)
            .iter()
            .filter(|g| g.src == url)
            .count();
        assert_eq!(count, 1, "re-favouriting replaces, not duplicates");
    }

    #[test]
    fn favouriting_into_an_empty_proto_creates_favorite_gifs() {
        // A blob with only unrelated fields and no favorite_gifs yet.
        let mut original = Vec::new();
        varint_field(&mut original, 5, 42);
        len_field(&mut original, 7, b"other-settings");
        let field5 = raw_top_field(&original, 5).unwrap();
        let field7 = raw_top_field(&original, 7).unwrap();

        let url = "https://media.tenor.com/first.gif";
        let updated = add_favorite(&original, url, url, FORMAT_IMAGE, 50, 50).unwrap();
        let gifs = parse_settings_proto(&updated);
        assert_eq!(gifs.len(), 1);
        assert_eq!(gifs[0].src, url);
        assert_eq!(gifs[0].order, 0, "first favourite gets order 0");
        // The unrelated fields survive.
        assert_eq!(raw_top_field(&updated, 5).unwrap(), field5);
        assert_eq!(raw_top_field(&updated, 7).unwrap(), field7);
    }

    #[test]
    fn unfavouriting_a_missing_url_is_a_noop() {
        let original = settings_with_extras();
        let updated = remove_favorite(&original, "https://media.tenor.com/absent.gif").unwrap();
        // Byte-for-byte identical: nothing matched, nothing changed.
        assert_eq!(updated, original);
    }

    #[test]
    fn malformed_proto_refuses_to_rewrite() {
        // A truncated length-delimited field: split_fields fails, so no write.
        let mut bad = Vec::new();
        tag(&mut bad, 1, LEN);
        varint(&mut bad, 100); // claims 100 bytes that are not there
        bad.extend_from_slice(b"short");
        assert!(add_favorite(&bad, "u", "u", FORMAT_IMAGE, 1, 1).is_none());
        assert!(remove_favorite(&bad, "u").is_none());
    }
}
