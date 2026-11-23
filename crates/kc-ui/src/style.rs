//! Application CSS. Colours come from the libadwaita palette so a system
//! theme (including generated ones such as stylix) carries through.

use gtk::gdk;

const CSS: &str = r#"
/* A setting declared in Nix, and the same setting changed here for now. */
.kc-nix-badge {
    font-size: 0.75em;
    font-weight: 700;
    padding: 1px 8px;
    border-radius: 999px;
    color: #7ebae4;
    background-color: alpha(#7ebae4, 0.16);
}
.kc-nix-badge.kc-temporary {
    color: @warning_color;
    background-color: alpha(@warning_color, 0.16);
}

/* Near or past the message length limit. */
.kc-length {
    color: @warning_color;
    font-weight: 700;
    padding: 0 6px;
    margin: 0 2px 2px 0;
    border-radius: 6px;
    background-color: alpha(@window_bg_color, 0.85);
}
.kc-length.kc-length-over {
    color: @error_color;
}

/* A file attached to the message being written. */
.kc-upload-card {
    padding: 8px;
}
.kc-upload-preview {
    border-radius: 6px;
    background-color: alpha(currentColor, 0.06);
}

/* The slim bar across the top of the window, with the app's name. */
headerbar.kc-title-bar {
    min-height: 40px;
}
headerbar.kc-title-bar .kc-app-name {
    font-size: 1.15em;
    font-weight: 800;
}
headerbar.kc-title-bar windowcontrols button {
    min-height: 22px;
    min-width: 22px;
    padding: 2px;
}
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

.kc-rail-add {
    color: #3ba55d;
}

.kc-rail-add:hover {
    background: #3ba55d;
    color: #ffffff;
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

.kc-emoji-nav {
    margin-bottom: 2px;
}

.kc-emoji-nav-button {
    padding: 2px 4px;
    min-width: 0;
    min-height: 0;
}

.kc-emoji-nav-button label {
    font-size: 1.1em;
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

/* The call's signal bars, in the status colours, and the Voice Connected
   heading in the online one. */
.kc-voice-connected,
.kc-ping-good { color: @success_bg_color; }
.kc-ping-fair { color: @warning_bg_color; }
.kc-ping-poor { color: @error_bg_color; }

.kc-ping-button {
    min-width: 0;
    min-height: 0;
    padding: 5px 6px;
    border-radius: 6px;
}

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

/* A message jumped to: highlighted, then fading back (see chat::flash). */
.kc-message.kc-flash {
    background-color: alpha(@accent_bg_color, 0.30);
}
.kc-message.kc-flash-out {
    transition: background-color 2000ms ease-out;
}

/* Share mode: the chosen messages, the redacted ones (blurred behind
   stripes), the range's edges and their handles, and the bar. */
.kc-message.kc-share-selected {
    background-color: alpha(@accent_bg_color, 0.20);
}
.kc-message.kc-share-redacted {
    background-color: alpha(@accent_bg_color, 0.10);
    background-image: repeating-linear-gradient(135deg, alpha(currentColor, 0.07) 0px, alpha(currentColor, 0.07) 6px, transparent 6px, transparent 12px);
}
.kc-message.kc-share-redacted > box {
    filter: blur(6px);
    opacity: 0.5;
}
.kc-message.kc-share-first {
    box-shadow: inset 0 2px 0 @accent_bg_color;
}
.kc-message.kc-share-last {
    box-shadow: inset 0 -2px 0 @accent_bg_color;
}
.kc-message.kc-share-first.kc-share-last {
    box-shadow: inset 0 2px 0 @accent_bg_color, inset 0 -2px 0 @accent_bg_color;
}
.kc-share-handle {
    background-color: @accent_bg_color;
    color: @accent_fg_color;
    border-radius: 999px;
    padding: 2px 18px;
    box-shadow: 0 1px 4px alpha(black, 0.4);
}
.kc-share-bar {
    background-color: @popover_bg_color;
    border-radius: 12px;
    padding: 8px 10px;
    box-shadow: 0 2px 10px alpha(black, 0.35);
}
/* A shared picture: the window's colours around the messages. */
.kc-share-render {
    background-color: @window_bg_color;
    padding: 16px 8px;
}
.kc-share-render.kc-no-times .kc-time,
.kc-share-render .kc-gutter-time {
    opacity: 0;
}
.kc-redacted-avatar {
    background-color: alpha(currentColor, 0.25);
    border-radius: 999px;
}
.kc-redacted-note {
    font-weight: bold;
    font-style: italic;
    opacity: 0.75;
}
.kc-redacted-bar {
    background-color: alpha(currentColor, 0.75);
    border-radius: 4px;
}

/* A deleted message the message logger kept. */
.kc-message.kc-deleted {
    background-color: alpha(@error_bg_color, 0.14);
    box-shadow: inset 2px 0 0 @error_bg_color;
}
/* Earlier versions of an edited message, kept by the message logger. */
.kc-old-version {
    opacity: 0.5;
}
.kc-edit-history {
    border-left: 2px solid alpha(currentColor, 0.2);
    padding-left: 6px;
    margin-bottom: 2px;
}

.kc-deleted-note {
    color: @error_color;
}

.kc-message.kc-mentioned {
    background-color: alpha(@warning_bg_color, 0.12);
    box-shadow: inset 2px 0 0 @warning_bg_color;
}

/* A message shown before Discord confirms it reads as half-there. */
.kc-message.kc-sending {
    opacity: 0.55;
}

.kc-send-failed {
    color: @error_bg_color;
}

/* Inline text actions (Retry / Delete) sit flush, like links. */
button.kc-link-button {
    padding: 0 2px;
    min-height: 0;
    min-width: 0;
    background: none;
    box-shadow: none;
    color: @accent_color;
}

button.kc-link-button:hover {
    text-decoration: underline;
    background: none;
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

.kc-invite {
    padding: 12px;
    border-radius: 8px;
    background-color: alpha(@window_fg_color, 0.06);
    margin-top: 4px;
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

/* Code blocks: monospace on a block darker than the chat behind it. */
.kc-codeblock {
    padding: 8px 10px;
    border-radius: 6px;
    background-color: alpha(black, 0.07);
    border: 1px solid alpha(@window_fg_color, 0.08);
}
@media (prefers-color-scheme: dark) {
    .kc-codeblock {
        background-color: alpha(black, 0.30);
    }
}
.kc-code-text {
    font-family: monospace;
    font-size: 0.95em;
}
.kc-code-language {
    font-size: 0.75em;
    font-weight: bold;
    opacity: 0.55;
}

/* A voice channel's chat button: there while the pointer is on the row. */
.kc-channel-chat {
    min-height: 20px;
    min-width: 20px;
    padding: 0;
    opacity: 0;
}

.kc-channel-row:hover .kc-channel-chat,
.kc-channel-chat:focus-visible {
    opacity: 1;
}

.kc-sound {
    padding: 6px 10px;
    border-radius: 8px;
}

.kc-stage-header {
    padding: 14px 16px;
}

.kc-stage-person {
    padding: 6px;
    border-radius: 12px;
}

.kc-stage-person.kc-speaking avatar {
    box-shadow: 0 0 0 3px @success_color;
}

.kc-request-bar {
    padding: 12px 14px;
}

.kc-wave {
    padding: 4px 12px 4px 6px;
    border-radius: 8px;
}

.kc-sticker-choice {
    padding: 4px;
    border-radius: 8px;
}

.kc-voice-message {
    padding: 8px 12px 8px 8px;
    border-radius: 24px;
    background-color: alpha(@window_fg_color, 0.06);
}

.kc-recording-dot {
    min-width: 10px;
    min-height: 10px;
    border-radius: 5px;
    background-color: @error_color;
    animation: kc-recording-pulse 1.2s ease-in-out infinite;
}

@keyframes kc-recording-pulse {
    50% { opacity: 0.3; }
}

.kc-poll {
    padding: 12px;
    border-radius: 10px;
    background-color: alpha(@window_fg_color, 0.05);
}

.kc-poll .kc-poll-answer {
    padding: 8px 10px;
    border-radius: 8px;
}

.kc-poll .kc-poll-answer:disabled {
    opacity: 1;
}

.kc-poll progressbar trough,
.kc-poll progressbar progress {
    min-height: 6px;
    border-radius: 3px;
}

.kc-poll progressbar.kc-poll-winner progress {
    background-color: @success_bg_color;
}

.kc-search-chip {
    border-radius: 999px;
    padding: 0 0 0 10px;
    background-color: alpha(@accent_bg_color, 0.18);
}

.kc-search-chip button {
    min-height: 22px;
    min-width: 22px;
    padding: 0;
}

.kc-search-panel .linked button {
    padding: 4px 6px;
    min-width: 0;
}

.kc-search-result {
    border-radius: 8px;
    padding: 8px;
    background-color: alpha(@window_fg_color, 0.04);
}

.kc-search-result:hover {
    background-color: alpha(@window_fg_color, 0.08);
}

.kc-search-channel {
    font-weight: bold;
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

/* App buttons: Discord's green "success" style; section thumbnails. */
button.kc-success {
    background-color: #248046;
    color: white;
}
button.kc-success:hover {
    background-color: #1a6334;
}
image.kc-thumbnail {
    border-radius: 8px;
}

/* Message text with emoji images (a text view) reads like the labels. */
.kc-rich,
.kc-rich text {
    background: none;
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

/* "Ringing …" above the call while we wait for someone to answer. */
.kc-ringing-banner {
    padding: 6px 14px;
    margin-bottom: 10px;
    border-radius: 999px;
    background-color: alpha(@window_fg_color, 0.08);
}

/* The phone icon shakes in bursts, like a vibrating phone, then rests. */
@keyframes kc-phone-vibrate {
    0%   { -gtk-icon-transform: rotate(0deg); }
    8%   { -gtk-icon-transform: rotate(-16deg); }
    16%  { -gtk-icon-transform: rotate(16deg); }
    24%  { -gtk-icon-transform: rotate(-13deg); }
    32%  { -gtk-icon-transform: rotate(13deg); }
    40%  { -gtk-icon-transform: rotate(-9deg); }
    48%  { -gtk-icon-transform: rotate(9deg); }
    56%  { -gtk-icon-transform: rotate(0deg); }
    100% { -gtk-icon-transform: rotate(0deg); }
}

.kc-ringing-phone {
    color: @success_bg_color;
    animation: kc-phone-vibrate 1.3s ease-in-out infinite;
}

.kc-composer-bar {
    padding: 6px 12px;
    border-radius: 10px 10px 0 0;
    font-size: 0.88em;
    background-color: alpha(@window_fg_color, 0.1);
}

/* When a bar (reply/edit context or the upload tray) sits above the composer,
   square the composer's top corners so the two join into one rounded box. */
.kc-composer.kc-attached {
    border-top-left-radius: 0;
    border-top-right-radius: 0;
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

/* A friend picked in the group DM picker. */
button.kc-chip {
    padding: 2px 8px;
    min-height: 0;
}

button.kc-dm-create {
    min-width: 22px;
    min-height: 22px;
    padding: 0;
    margin-right: 4px;
}

/* A DM row's close button: there only while the row is hovered. */
button.kc-dm-close {
    min-width: 20px;
    min-height: 20px;
    padding: 0;
    opacity: 0;
}

button.kc-dm-close image {
    -gtk-icon-size: 12px;
}

.kc-dm-row:hover button.kc-dm-close,
button.kc-dm-close:focus-visible {
    opacity: 0.7;
}

.kc-dm-row:hover button.kc-dm-close:hover {
    opacity: 1;
}

/* The row stays lit while the pointer is on its ×. */
.kc-dm-row:hover > button.kc-channel:not(.kc-selected) {
    background-color: alpha(currentColor, 0.07);
}

.kc-menu-sep {
    margin: 4px 6px;
    opacity: 0.5;
}

/* The favourite star over a GIF (picker tile or chat message). Hidden until
   the GIF is hovered, so it stays subtle; a favourited star (filled) stays
   faintly visible so you can tell at a glance. */
button.kc-gif-star {
    min-width: 24px;
    min-height: 24px;
    padding: 2px;
    opacity: 0;
    transition: opacity 120ms ease;
}

button.kc-gif-star image {
    -gtk-icon-size: 14px;
}

overlay:hover > button.kc-gif-star,
button.kc-gif-star:focus-visible,
button.kc-gif-star.kc-favorited {
    opacity: 1;
}

button.kc-gif-star.kc-favorited image {
    color: #f0b232;
}

/* A role's colour, before its name. */
.kc-role-dot {
    min-width: 12px;
    min-height: 12px;
    border-radius: 999px;
    background-color: alpha(@window_fg_color, 0.35);
}

/* Server mute and deafen: set by a moderator, shown red as Discord does. */
image.kc-server-muted {
    color: @error_color;
    opacity: 1;
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

/* In the corner the controls appear in: up top until the pointer comes
   over the video, then below the controls. */
.kc-video-info-yields {
    transition: margin 120ms ease;
}

.kc-tile:hover .kc-video-info-yields,
.kc-popout:hover .kc-video-info-yields {
    margin-top: 34px;
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

/* The input sensitivity slider: its track is the level meter drawn
   under it, so the trough itself is see-through. */
.kc-sensitivity trough,
.kc-sensitivity highlight {
    background: none;
    box-shadow: none;
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
