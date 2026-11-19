//! Application CSS. Colours come from the libadwaita palette so a system
//! theme (including generated ones such as stylix) carries through.

use gtk::gdk;

const CSS: &str = r#"
.kc-rail {
    background-color: @headerbar_shade_color;
    background-color: color-mix(in srgb, @window_bg_color 92%, black);
}

.kc-rail-button {
    padding: 0;
    min-width: 48px;
    min-height: 48px;
    border-radius: 16px;
}

.kc-rail-button avatar {
    border-radius: 16px;
    transition: border-radius 120ms ease;
}

.kc-pill {
    min-width: 4px;
    border-radius: 0 4px 4px 0;
    background-color: @window_fg_color;
}

.kc-folder {
    border-radius: 18px;
    background-color: alpha(@window_fg_color, 0.07);
}

.kc-badge {
    min-width: 16px;
    min-height: 16px;
    padding: 0 4px;
    border-radius: 9px;
    font-size: 0.72em;
    font-weight: 700;
    color: white;
    background-color: @error_bg_color;
}

.kc-channel {
    padding: 4px 8px;
    min-height: 0;
    border-radius: 6px;
    font-weight: normal;
}

.kc-channel.kc-selected {
    background-color: alpha(@window_fg_color, 0.1);
}

.kc-channel.kc-muted {
    opacity: 0.45;
}

.kc-channel.kc-unread label.kc-channel-name {
    font-weight: 700;
}

.kc-channel:not(.kc-unread):not(.kc-selected) label.kc-channel-name {
    opacity: 0.72;
}

.kc-category {
    padding: 2px 4px;
    min-height: 0;
    font-size: 0.8em;
    font-weight: 700;
    letter-spacing: 0.03em;
    opacity: 0.6;
}

.kc-status-dot {
    min-width: 10px;
    min-height: 10px;
    border-radius: 7px;
    box-shadow: 0 0 0 2px @window_bg_color;
}

.kc-status-online { background-color: @success_bg_color; }
.kc-status-idle { background-color: @warning_bg_color; }
.kc-status-dnd { background-color: @error_bg_color; }
.kc-status-offline { background-color: alpha(@window_fg_color, 0.35); }

.kc-messages row {
    padding: 0;
    background: none;
}

.kc-message {
    padding: 1px 16px 1px 16px;
}

.kc-message:hover,
.kc-message.kc-hovered {
    background-color: alpha(@window_fg_color, 0.04);
}

.kc-message.kc-flash {
    background-color: alpha(@accent_bg_color, 0.18);
}

.kc-message.kc-mentioned {
    background-color: alpha(@warning_bg_color, 0.12);
    box-shadow: inset 2px 0 0 @warning_bg_color;
}

.kc-message.kc-first {
    margin-top: 12px;
}

.kc-author {
    font-weight: 700;
}

.kc-gutter-time {
    font-size: 0.7em;
    opacity: 0;
}

.kc-message:hover .kc-gutter-time,
.kc-message.kc-hovered .kc-gutter-time {
    opacity: 0.55;
}

.kc-toolbar {
    padding: 2px;
    border-radius: 8px;
    background-color: @popover_bg_color;
    box-shadow: 0 1px 4px alpha(black, 0.3);
}

.kc-toolbar button {
    min-width: 28px;
    min-height: 28px;
    padding: 0;
}

.kc-reply {
    font-size: 0.88em;
    opacity: 0.75;
}

.kc-tag {
    padding: 0 4px;
    border-radius: 4px;
    font-size: 0.68em;
    font-weight: 700;
    color: white;
    background-color: @accent_bg_color;
}

.kc-embed {
    padding: 8px 12px;
    border-radius: 6px;
    background-color: alpha(@window_fg_color, 0.05);
    border-left: 4px solid alpha(@window_fg_color, 0.25);
}

.kc-media {
    border-radius: 8px;
}

.kc-media-tag {
    padding: 1px 5px;
    border-radius: 4px;
    font-size: 0.7em;
    font-weight: 800;
    color: white;
    background-color: alpha(black, 0.6);
}

.kc-file {
    padding: 8px 12px;
    border-radius: 8px;
    background-color: alpha(@window_fg_color, 0.05);
}

.kc-reaction {
    padding: 1px 6px;
    min-height: 24px;
    border-radius: 8px;
    font-weight: normal;
    background-color: alpha(@window_fg_color, 0.06);
}

.kc-reaction:checked {
    background-color: alpha(@accent_bg_color, 0.25);
    box-shadow: inset 0 0 0 1px @accent_bg_color;
}

.kc-codeblock {
    padding: 8px;
    border-radius: 6px;
    background-color: alpha(@window_fg_color, 0.06);
}

.kc-composer {
    border-radius: 10px;
    background-color: alpha(@window_fg_color, 0.06);
}

.kc-composer textview,
.kc-composer textview text {
    background: none;
}

/* The scrollbar's minimum length set the composer's minimum height: a
   one-line message sat at the top of a box a line too tall. A short
   minimum lets the box follow the text; long messages still scroll. */
.kc-composer scrollbar.vertical slider {
    min-height: 12px;
}

.kc-composer scrollbar.vertical,
.kc-composer scrollbar.vertical range,
.kc-composer scrollbar.vertical trough {
    min-height: 0;
    padding-top: 0;
    padding-bottom: 0;
    margin-top: 0;
    margin-bottom: 0;
}

/* Typing unseen: the bar's eye button in the accent colour, so the
   setting is noticeable. */
.kc-typing-hidden {
    color: @accent_color;
}

/* Notices at the top of the window. */
.kc-notice {
    background-color: mix(@window_bg_color, @window_fg_color, 0.16);
    border-radius: 999px;
    padding: 4px 6px 4px 18px;
    margin: 0 12px 6px 12px;
    box-shadow: 0 2px 8px alpha(black, 0.35);
}

.kc-standard-emoji label {
    font-size: 1.45em;
}

/* The incoming call card: a pulsing ring round the caller, and Discord's
   red and green buttons. */
@keyframes kc-ring-pulse {
    from { box-shadow: 0 0 0 0 alpha(@success_bg_color, 0.7); }
    to { box-shadow: 0 0 0 18px alpha(@success_bg_color, 0); }
}

.kc-ringing {
    border-radius: 9999px;
    animation: kc-ring-pulse 1.4s ease-out infinite;
}

.kc-call-decline,
.kc-call-accept {
    min-width: 72px;
    min-height: 44px;
    border-radius: 10px;
    color: white;
}

.kc-call-decline { background-color: @error_bg_color; }
.kc-call-accept { background-color: @success_bg_color; }

.kc-composer-bar {
    padding: 6px 12px;
    border-radius: 10px 10px 0 0;
    font-size: 0.88em;
    background-color: alpha(@window_fg_color, 0.1);
}

.kc-jumbo {
    font-size: 2.6em;
}

.kc-system {
    opacity: 0.75;
}

.kc-divider {
    font-size: 0.75em;
    font-weight: 700;
    opacity: 0.55;
}

.kc-userbar {
    background-color: alpha(black, 0.12);
}

.kc-userbar .kc-status-dot {
    min-width: 12px;
    min-height: 12px;
}

/* Compact controls, as in Discord, so the name gets the room. */
.kc-userbar button.image-button {
    min-width: 28px;
    min-height: 28px;
    padding: 2px;
}

.kc-userbar menubutton > button {
    padding: 2px 4px;
}

/* Right-click context menu. */
.kc-menu {
    padding: 4px;
}

.kc-menu button {
    padding: 5px 10px;
    min-height: 0;
}

.kc-menu button label {
    margin: 0;
}

.kc-menu button.kc-danger label {
    color: @error_color;
}

.kc-menu-sep {
    margin: 4px 6px;
    opacity: 0.5;
}

.kc-guild-menu {
    padding: 2px 8px;
}

.kc-guild-menu image {
    opacity: 0.6;
}

.kc-video {
    border-radius: 8px;
    background-color: black;
    transition: outline-color 120ms ease;
    outline: 3px solid transparent;
}

/* A tile of the call: a person or a stream. */
.kc-tile {
    border-radius: 10px;
    background-color: alpha(@window_fg_color, 0.07);
    outline: 2px solid transparent;
    outline-offset: -2px;
    transition: outline-color 120ms ease;
}

.kc-tile.kc-tile-dark {
    background-color: black;
}

.kc-tile.kc-speaking {
    outline-color: @success_bg_color;
}

.kc-tile-name {
    padding: 2px 8px;
    border-radius: 6px;
    font-size: 0.85em;
    color: white;
    background-color: alpha(black, 0.6);
}

.kc-tile-caption {
    font-size: 0.9em;
    opacity: 0.7;
}

.kc-tile-dark .kc-tile-caption {
    color: white;
}

/* Controls on a tile show while the pointer is over it; on the enlarged
   tile they stay. */
.kc-tile-controls {
    opacity: 0;
    transition: opacity 120ms ease;
}

.kc-tile:hover .kc-tile-controls,
.kc-tile-controls.kc-shown {
    opacity: 1;
}

.kc-tile-controls button,
.kc-members-toggle {
    min-height: 26px;
    min-width: 26px;
    padding: 2px 8px;
    border-radius: 8px;
    color: white;
    background-color: alpha(black, 0.6);
}

.kc-tile-controls button:hover,
.kc-members-toggle:hover {
    background-color: alpha(black, 0.8);
}

.kc-members-toggle {
    border-radius: 999px;
    padding: 2px 10px;
}

.kc-popout {
    background-color: black;
}

.kc-popout:hover .kc-tile-controls {
    opacity: 1;
}

/* What a video is made of, over its picture. */
.kc-video-info {
    padding: 6px 10px;
    border-radius: 8px;
    font-family: monospace;
    font-size: 0.8em;
    color: white;
    background-color: alpha(black, 0.72);
}

/* Profile card: an opaque surface so the ring around the avatar, in the
   same colour, cuts cleanly into the banner. */
.kc-profile-card {
    background-color: mix(@window_bg_color, @window_fg_color, 0.07);
    border-radius: 14px;
}

.kc-profile-avatar {
    border-radius: 9999px;
    box-shadow: 0 0 0 6px mix(@window_bg_color, @window_fg_color, 0.07);
}

.kc-profile-heading {
    font-weight: bold;
    font-size: 0.85em;
    opacity: 0.75;
}

/* A Discord-sized presence dot on the profile avatar, ringed in the card
   colour where it overlaps the avatar's edge. */
.kc-profile-status .kc-status-dot {
    min-width: 22px;
    min-height: 22px;
    border-radius: 16px;
    margin: 0 3px 3px 0;
    box-shadow: 0 0 0 5px mix(@window_bg_color, @window_fg_color, 0.07);
}

.kc-status-bubble {
    background-color: mix(@window_bg_color, @window_fg_color, 0.14);
    border-radius: 10px;
    padding: 5px 10px;
    box-shadow: 0 1px 4px alpha(black, 0.35);
}

.kc-profile-tabs {
    font-size: 0.85em;
}

.kc-profile-row {
    padding: 6px 8px;
    border-radius: 8px;
}

.kc-preview {
    border-radius: 12px;
    background-color: black;
}

.kc-meter block {
    min-width: 4px;
    min-height: 26px;
    margin: 0 1px;
    border-radius: 2px;
}

.kc-meter block.filled {
    background-color: @success_bg_color;
}

.kc-meter block.empty {
    background-color: alpha(currentColor, 0.15);
}

.kc-watch {
    min-height: 0;
    padding: 2px 10px;
    font-size: 0.85em;
}

.kc-voice-row avatar {
    transition: box-shadow 120ms ease;
}

.kc-voice-row .kc-voice-name {
    opacity: 0.6;
}

.kc-voice-row.kc-speaking avatar {
    box-shadow: 0 0 0 2px @success_bg_color;
}

.kc-voice-row.kc-speaking .kc-voice-name {
    opacity: 1;
}

.kc-picker-emoji {
    padding: 2px;
    min-width: 36px;
    min-height: 36px;
}

.kc-profile-banner {
    min-height: 60px;
    border-radius: 8px 8px 0 0;
    background-color: @accent_bg_color;
}
"#;

/// Installs the stylesheet for the default display. Safe to call once GTK
/// has been initialised.
pub fn install() {
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}
