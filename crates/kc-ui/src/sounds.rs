//! Interface sounds: notifications, joining and leaving, mute and deafen,
//! ringing, streams.
//!
//! The sounds are Discord's own, but they are not part of kylecord: on
//! first need they are downloaded from Discord's public web assets into
//! `~/.cache/kylecord/sounds/` (no token, no account data, just the files
//! every browser loads). Which file is which is not published, so
//! [`discover`] reads the web app's script bundles, where the build maps
//! each sound's name (`./message1.mp3`) to a webpack module that exports
//! its hashed file name (`n.p+"3ed22d14f3c30bc4.mp3"`). The result is kept
//! in `manifest.json`. Until the files are there, or if Discord changes
//! its bundles so the names cannot be found, a short tone made here plays
//! instead. A file dropped in `~/.config/kylecord/sounds/<event>.mp3`
//! (or `.ogg`, `.wav`, `.flac`, `.opus`) replaces any sound.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::LazyLock;

use gtk::glib;
use kc_core::Snowflake;
use kc_media::sound::Sound;
use serde::{Deserialize, Serialize};

use crate::shell::AppState;

/// Every sound kylecord plays.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum SoundEvent {
    Message,
    Mention,
    VoiceJoin,
    VoiceLeave,
    UserJoin,
    UserLeave,
    Mute,
    Unmute,
    Deafen,
    Undeafen,
    IncomingRing,
    OutgoingRing,
    StreamStart,
    StreamStop,
    ViewerJoin,
    ViewerLeave,
    Disconnected,
}

impl SoundEvent {
    pub(crate) const ALL: [SoundEvent; 17] = [
        Self::Message,
        Self::Mention,
        Self::VoiceJoin,
        Self::VoiceLeave,
        Self::UserJoin,
        Self::UserLeave,
        Self::Mute,
        Self::Unmute,
        Self::Deafen,
        Self::Undeafen,
        Self::IncomingRing,
        Self::OutgoingRing,
        Self::StreamStart,
        Self::StreamStop,
        Self::ViewerJoin,
        Self::ViewerLeave,
        Self::Disconnected,
    ];

    /// The name in settings and of an override file.
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Mention => "mention",
            Self::VoiceJoin => "voice_join",
            Self::VoiceLeave => "voice_leave",
            Self::UserJoin => "user_join",
            Self::UserLeave => "user_leave",
            Self::Mute => "mute",
            Self::Unmute => "unmute",
            Self::Deafen => "deafen",
            Self::Undeafen => "undeafen",
            Self::IncomingRing => "incoming_ring",
            Self::OutgoingRing => "outgoing_ring",
            Self::StreamStart => "stream_start",
            Self::StreamStop => "stream_stop",
            Self::ViewerJoin => "viewer_join",
            Self::ViewerLeave => "viewer_leave",
            Self::Disconnected => "disconnected",
        }
    }

    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::Message => "Message",
            Self::Mention => "Mention",
            Self::VoiceJoin => "You Join a Call",
            Self::VoiceLeave => "You Leave a Call",
            Self::UserJoin => "Someone Joins Your Call",
            Self::UserLeave => "Someone Leaves Your Call",
            Self::Mute => "Mute",
            Self::Unmute => "Unmute",
            Self::Deafen => "Deafen",
            Self::Undeafen => "Undeafen",
            Self::IncomingRing => "Incoming Call",
            Self::OutgoingRing => "Outgoing Call",
            Self::StreamStart => "You Go Live",
            Self::StreamStop => "Your Stream Ends",
            Self::ViewerJoin => "Someone Watches Your Stream",
            Self::ViewerLeave => "Someone Stops Watching",
            Self::Disconnected => "Call Disconnected",
        }
    }

    /// The sound's name in Discord's web app.
    pub(crate) fn discord_name(self) -> &'static str {
        match self {
            Self::Message | Self::Mention => "message1",
            Self::VoiceJoin | Self::UserJoin => "user_join",
            Self::VoiceLeave | Self::Disconnected => "disconnect",
            Self::UserLeave => "user_leave",
            Self::Mute => "mute",
            Self::Unmute => "unmute",
            Self::Deafen => "deafen",
            Self::Undeafen => "undeafen",
            Self::IncomingRing => "call_ringing",
            Self::OutgoingRing => "call_calling",
            Self::StreamStart => "stream_started",
            Self::StreamStop => "stream_ended",
            Self::ViewerJoin => "stream_user_joined",
            Self::ViewerLeave => "stream_user_left",
        }
    }

    /// Plays over and over until stopped.
    pub(crate) fn loops(self) -> bool {
        matches!(self, Self::IncomingRing | Self::OutgoingRing)
    }

    /// The built-in stand-in: notes as `(Hz, ms)`, 0 Hz a rest.
    fn tone(self) -> &'static [(f32, u32)] {
        match self {
            Self::Message => &[(880.0, 90), (1320.0, 130)],
            Self::Mention => &[(988.0, 80), (1319.0, 80), (1760.0, 150)],
            Self::VoiceJoin => &[(523.0, 90), (784.0, 150)],
            Self::VoiceLeave => &[(784.0, 90), (523.0, 150)],
            Self::UserJoin => &[(659.0, 80), (988.0, 120)],
            Self::UserLeave => &[(988.0, 80), (659.0, 120)],
            Self::Mute => &[(440.0, 70), (330.0, 90)],
            Self::Unmute => &[(330.0, 70), (440.0, 90)],
            Self::Deafen => &[(392.0, 70), (262.0, 110)],
            Self::Undeafen => &[(262.0, 70), (392.0, 110)],
            Self::IncomingRing => &[(784.0, 180), (0.0, 60), (988.0, 180), (0.0, 1000)],
            Self::OutgoingRing => &[(440.0, 400), (0.0, 1400)],
            Self::StreamStart => &[(523.0, 70), (659.0, 70), (784.0, 130)],
            Self::StreamStop => &[(784.0, 70), (659.0, 70), (523.0, 130)],
            Self::ViewerJoin => &[(1047.0, 90)],
            Self::ViewerLeave => &[(784.0, 90)],
            Self::Disconnected => &[(330.0, 150), (220.0, 260)],
        }
    }
}

/// Where a sound comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    /// A file the user put in the config folder.
    Override(PathBuf),
    /// Discord's own, downloaded.
    Discord(PathBuf),
    /// A tone made here.
    Builtin(PathBuf),
}

impl Source {
    pub(crate) fn path(&self) -> &Path {
        match self {
            Self::Override(path) | Self::Discord(path) | Self::Builtin(path) => path,
        }
    }

    pub(crate) fn describe(&self) -> &'static str {
        match self {
            Self::Override(_) => "Your own file",
            Self::Discord(_) => "Discord's sound",
            Self::Builtin(_) => "Built-in tone",
        }
    }
}

const OVERRIDE_EXTENSIONS: [&str; 5] = ["mp3", "ogg", "wav", "flac", "opus"];

/// `~/.config/kylecord/sounds`: files here replace the sounds.
pub(crate) fn override_dir() -> Option<PathBuf> {
    kc_core::paths::config_dir()
        .ok()
        .map(|dir| dir.join("sounds"))
}

/// `~/.cache/kylecord/sounds`: the downloaded sounds and the tones.
fn cache_dir() -> Option<PathBuf> {
    kc_core::paths::cache_dir()
        .ok()
        .map(|dir| dir.join("sounds"))
}

/// Which file Discord's web app uses for each sound name, as found by
/// [`discover`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Manifest {
    /// Seconds since 1970 when it was found.
    pub discovered: u64,
    /// Sound name (`message1`) → file name (`3ed22d14f3c30bc4.mp3`).
    pub files: BTreeMap<String, String>,
}

impl Manifest {
    fn load(dir: &Path) -> Option<Self> {
        serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).ok()?).ok()
    }

    fn save(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        std::fs::write(
            dir.join("manifest.json"),
            serde_json::to_vec_pretty(self).unwrap_or_default(),
        )
    }
}

/// Picks the file for `event`: an override, else Discord's (when
/// downloaded), else the built-in tone.
pub(crate) fn resolve(
    event: SoundEvent,
    overrides: Option<&Path>,
    cache: &Path,
    manifest: Option<&Manifest>,
) -> Source {
    if let Some(dir) = overrides {
        for extension in OVERRIDE_EXTENSIONS {
            let path = dir.join(format!("{}.{extension}", event.key()));
            if path.is_file() {
                return Source::Override(path);
            }
        }
    }
    if let Some(file) = manifest.and_then(|manifest| manifest.files.get(event.discord_name())) {
        let path = cache.join("discord").join(file);
        if path.is_file() {
            return Source::Discord(path);
        }
    }
    Source::Builtin(cache.join("builtin").join(format!("{}.wav", event.key())))
}

// -- discovery ---------------------------------------------------------------

const WEB_APP: &str = "https://discord.com/app";
const ASSETS: &str = "https://discord.com/assets/";
/// Scripts read at most while looking for the sounds.
const MAX_SCRIPTS: usize = 40;

static SCRIPT_SRC: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#"src="(/assets/[A-Za-z0-9._-]+\.js)""#).unwrap());
/// `"./message1.mp3":"31742"` (the id may be unquoted).
static SOUND_MODULE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#""\./([a-z0-9_]+)\.mp3"\s*:\s*"?(\d+)"?"#).unwrap());
/// `31742(e,t,n){"use strict";e.exports=n.p+"3ed22d14f3c30bc4.mp3"}`, or
/// the older `31742:function(e,t,n){…}` / `31742:(e,t,n)=>{…}` forms.
static MODULE_FILE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r#"(?:^|[,{])\s*(\d+)\s*(?::\s*(?:function\s*)?)?\(\s*\w+\s*,\s*\w+\s*,\s*\w+\s*\)\s*(?:=>\s*)?\{\s*(?:"use strict";\s*)?\w+\.exports\s*=\s*\w+\.p\s*\+\s*"([0-9a-f]+\.mp3)""#,
    )
    .unwrap()
});

/// The web app's script URLs, the main bundle (`web.*.js`) first.
pub(crate) fn script_urls(html: &str) -> Vec<String> {
    let mut urls: Vec<String> = SCRIPT_SRC
        .captures_iter(html)
        .map(|capture| format!("https://discord.com{}", &capture[1]))
        .collect();
    // Drop every duplicate (not just adjacent ones), keeping first order, so
    // the fetch budget is not spent twice on the same script.
    let mut seen = std::collections::HashSet::new();
    urls.retain(|url| seen.insert(url.clone()));
    urls.sort_by_key(|url| !url.contains("/assets/web."));
    urls
}

/// What one script tells about the sounds.
#[derive(Default)]
pub(crate) struct Clues {
    /// Sound name → module id.
    names: HashMap<String, String>,
    /// Module id → file name.
    files: HashMap<String, String>,
}

impl Clues {
    pub(crate) fn read(&mut self, script: &str) {
        for capture in SOUND_MODULE.captures_iter(script) {
            self.names
                .insert(capture[1].to_owned(), capture[2].to_owned());
        }
        for capture in MODULE_FILE.captures_iter(script) {
            self.files
                .insert(capture[1].to_owned(), capture[2].to_owned());
        }
    }

    /// Sound name → file, for every name whose module was found.
    pub(crate) fn files(&self) -> BTreeMap<String, String> {
        self.names
            .iter()
            .filter_map(|(name, module)| Some((name.clone(), self.files.get(module)?.clone())))
            .collect()
    }

    fn has_all(&self, names: &[&str]) -> bool {
        let files = self.files();
        names.iter().all(|name| files.contains_key(*name))
    }
}

/// The sound names kylecord uses.
fn wanted() -> Vec<&'static str> {
    let mut names: Vec<&str> = SoundEvent::ALL
        .iter()
        .map(|event| event.discord_name())
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// Reads Discord's web app for the sound files' names.
async fn discover() -> Result<Manifest, String> {
    let html = kc_http::cdn::fetch(WEB_APP)
        .await
        .map_err(|error| format!("could not load Discord's web app: {error}"))?;
    let html = String::from_utf8_lossy(&html);
    let scripts = script_urls(&html);
    if scripts.is_empty() {
        return Err("Discord's web app lists no scripts".to_owned());
    }
    let wanted = wanted();
    let mut clues = Clues::default();
    for url in scripts.iter().take(MAX_SCRIPTS) {
        match kc_http::cdn::fetch(url).await {
            Ok(script) => clues.read(&String::from_utf8_lossy(&script)),
            Err(error) => tracing::debug!(%error, url, "web app script"),
        }
        if clues.has_all(&wanted) {
            break;
        }
    }
    let files = clues.files();
    if files.is_empty() {
        return Err("no sounds found in Discord's web app".to_owned());
    }
    Ok(Manifest {
        discovered: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs()),
        files,
    })
}

/// Whether bytes look like an MP3 (ID3 tag or an MPEG frame).
fn looks_like_mp3(bytes: &[u8]) -> bool {
    bytes.starts_with(b"ID3") || (bytes.len() > 1 && bytes[0] == 0xFF && bytes[1] & 0xE0 == 0xE0)
}

/// Downloads every wanted sound the manifest names and the cache lacks.
/// Returns how many of the wanted sounds are present afterwards.
async fn download(dir: &Path, manifest: &Manifest) -> (usize, Vec<String>) {
    let wanted = wanted();
    let folder = dir.join("discord");
    let _ = tokio::fs::create_dir_all(&folder).await;
    let mut present = 0;
    let mut problems = Vec::new();
    for name in &wanted {
        let Some(file) = manifest.files.get(*name) else {
            problems.push(format!("{name}: not found in the web app"));
            continue;
        };
        let path = folder.join(file);
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            present += 1;
            continue;
        }
        match kc_http::cdn::fetch(&format!("{ASSETS}{file}")).await {
            Ok(bytes) if looks_like_mp3(&bytes) => {
                let partial = path.with_extension("part");
                if tokio::fs::write(&partial, &bytes).await.is_ok()
                    && tokio::fs::rename(&partial, &path).await.is_ok()
                {
                    present += 1;
                }
            }
            Ok(_) => problems.push(format!("{name}: not an MP3")),
            Err(error) => problems.push(format!("{name}: {error}")),
        }
    }
    (present, problems)
}

/// Makes sure Discord's sounds are in the cache: reads the manifest (or,
/// with `rediscover` or none saved, finds it again) and downloads what is
/// missing. A download that fails with a saved manifest triggers one
/// fresh discovery, since Discord may have renamed the files.
async fn ensure(dir: PathBuf, rediscover: bool) -> Result<(Manifest, usize, Vec<String>), String> {
    let saved = if rediscover {
        None
    } else {
        Manifest::load(&dir)
    };
    let fresh = saved.is_none();
    let mut manifest = match saved {
        Some(manifest) => manifest,
        None => discover().await?,
    };
    let (mut present, mut problems) = download(&dir, &manifest).await;
    if !fresh && present < wanted().len() {
        manifest = discover().await?;
        (present, problems) = download(&dir, &manifest).await;
    }
    manifest
        .save(&dir)
        .map_err(|error| format!("could not save the sound list: {error}"))?;
    Ok((manifest, present, problems))
}

// -- playing -----------------------------------------------------------------

/// Discord's sounds: where they stand.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum Download {
    /// Not looked at yet this run.
    #[default]
    NotYet,
    Working,
    /// This many of the wanted sounds are present.
    Ready(usize),
    Failed(String),
}

impl Download {
    /// What the Sounds page says about Discord's sounds.
    pub(crate) fn describe(&self) -> String {
        let wanted = wanted().len();
        match self {
            Self::NotYet => "Fetched from discord.com the first time a sound plays.".to_owned(),
            Self::Working => "Getting the sounds from discord.com…".to_owned(),
            Self::Ready(present) if *present == wanted => {
                format!("All {wanted} of Discord's sounds are here.")
            }
            Self::Ready(present) => {
                format!("{present} of {wanted} found; the others play a built-in tone.")
            }
            Self::Failed(why) => format!("Using built-in tones: {why}."),
        }
    }
}

/// Told of download changes; returns false when it is no longer needed.
pub(crate) type DownloadListener = Box<dyn Fn(&Download) -> bool>;

/// The sounds' running state.
#[derive(Default)]
pub(crate) struct Sounds {
    manifest: RefCell<Option<Manifest>>,
    pub(crate) download: RefCell<Download>,
    /// Called when the download state changes (the Sounds page); a
    /// listener returns false once it is no longer needed.
    pub(crate) on_change: RefCell<Vec<DownloadListener>>,
    /// The incoming ring, for a call's channel.
    ring: RefCell<Option<(Snowflake, Sound)>>,
    /// The ring while we call someone.
    calling: RefCell<Option<(Snowflake, Sound)>>,
    /// Bumped per ring, so an old timeout does not stop a newer ring. One per
    /// direction ([0] incoming, [1] outgoing), so a ring in one direction
    /// does not defeat the other's auto-stop backstop.
    ring_serial: [Cell<u64>; 2],
    /// When our outgoing ring started.
    calling_since: Cell<Option<std::time::Instant>>,
    /// The call you are in and the others in it.
    peers: RefCell<Option<(Snowflake, HashSet<Snowflake>)>>,
    /// Who watches each of your streams.
    viewers: RefCell<HashMap<String, HashSet<Snowflake>>>,
}

/// The longest a ring plays if nobody answers or hangs up.
const RING_LIMIT_SECS: u32 = 60;
/// How long Discord's word that nobody is rung is not taken, after we
/// start calling.
const RING_GRACE: std::time::Duration = std::time::Duration::from_secs(4);

impl AppState {
    /// The file to play for `event` now.
    pub(crate) fn sound_source(&self, event: SoundEvent) -> Option<Source> {
        let cache = cache_dir()?;
        if self.sounds.manifest.borrow().is_none() {
            *self.sounds.manifest.borrow_mut() = Manifest::load(&cache);
        }
        let manifest = self.sounds.manifest.borrow();
        Some(resolve(
            event,
            override_dir().as_deref(),
            &cache,
            manifest.as_ref(),
        ))
    }

    /// Plays `event` if sounds and that sound are switched on. Loops are
    /// returned for the caller to stop.
    pub(crate) fn play_sound(self: &Rc<Self>, event: SoundEvent) -> Option<Sound> {
        {
            let prefs = self.capture.borrow();
            if !prefs.sounds_enabled || prefs.sounds_off.iter().any(|key| key == event.key()) {
                return None;
            }
        }
        self.play_sound_anyway(event)
    }

    /// Plays `event` whatever the switches say (the preview buttons).
    pub(crate) fn play_sound_anyway(self: &Rc<Self>, event: SoundEvent) -> Option<Sound> {
        self.fetch_sounds(false);
        let source = self.sound_source(event)?;
        let path = source.path().to_path_buf();
        if let Source::Builtin(path) = &source
            && !path.is_file()
        {
            let wav = kc_media::sound::tone_wav(event.tone(), 0.6);
            let written = path
                .parent()
                .is_some_and(|dir| std::fs::create_dir_all(dir).is_ok())
                && std::fs::write(path, wav).is_ok();
            if !written {
                return None;
            }
        }
        let volume = f64::from(self.capture.borrow().sound_volume);
        if dry_run() {
            eprintln!(
                "demo: sound {} from {} ({})",
                event.key(),
                source.describe(),
                path.display()
            );
            return None;
        }
        match kc_media::sound::play(&path, volume, event.loops()) {
            Ok(sound) => Some(sound),
            Err(error) => {
                tracing::warn!(%error, sound = event.key(), "could not play a sound");
                None
            }
        }
    }

    /// Fetches Discord's sounds in the background, once per run (or again
    /// with `again`, the Refresh button).
    pub(crate) fn fetch_sounds(self: &Rc<Self>, again: bool) {
        let state = self.sounds.download.borrow().clone();
        if state == Download::Working || (!again && state != Download::NotYet) {
            return;
        }
        let Some(dir) = cache_dir() else {
            return;
        };
        if dry_run() || std::env::var_os("KYLECORD_NO_SOUND_DOWNLOAD").is_some() {
            self.set_download(Download::Failed("downloads are switched off".to_owned()));
            return;
        }
        self.set_download(Download::Working);
        self.spawn(ensure(dir, again), |app, result| match result {
            Ok((manifest, present, problems)) => {
                if !problems.is_empty() {
                    tracing::warn!(?problems, "some of Discord's sounds are missing");
                }
                tracing::info!(present, "Discord's sounds ready");
                *app.sounds.manifest.borrow_mut() = Some(manifest);
                app.set_download(Download::Ready(present));
            }
            Err(error) => {
                tracing::warn!(%error, "could not get Discord's sounds; using built-in tones");
                app.set_download(Download::Failed(error));
            }
        });
    }

    fn set_download(&self, state: Download) {
        *self.sounds.download.borrow_mut() = state.clone();
        self.sounds
            .on_change
            .borrow_mut()
            .retain(|listener| listener(&state));
    }

    /// The sound for a message that notifies.
    pub(crate) fn notification_sound(self: &Rc<Self>, mention: bool) {
        self.play_sound(if mention {
            SoundEvent::Mention
        } else {
            SoundEvent::Message
        });
    }

    /// Starts the incoming ring for `channel_id` (stopped by
    /// [`AppState::stop_ring`], or after a minute).
    pub(crate) fn start_ring(self: &Rc<Self>, channel_id: Snowflake, outgoing: bool) {
        let slot = if outgoing {
            &self.sounds.calling
        } else {
            &self.sounds.ring
        };
        if slot
            .borrow()
            .as_ref()
            .is_some_and(|(ringing, _)| *ringing == channel_id)
        {
            return;
        }
        self.stop_ring(outgoing);
        let event = if outgoing {
            SoundEvent::OutgoingRing
        } else {
            SoundEvent::IncomingRing
        };
        let Some(sound) = self.play_sound(event) else {
            return;
        };
        *slot.borrow_mut() = Some((channel_id, sound));
        if outgoing {
            self.sounds
                .calling_since
                .set(Some(std::time::Instant::now()));
        }
        let serial = self.sounds.ring_serial[outgoing as usize].get() + 1;
        self.sounds.ring_serial[outgoing as usize].set(serial);
        let app = Rc::downgrade(self);
        glib::timeout_add_seconds_local_once(RING_LIMIT_SECS, move || {
            if let Some(app) = app.upgrade()
                && app.sounds.ring_serial[outgoing as usize].get() == serial
            {
                app.stop_ring(outgoing);
            }
        });
    }

    pub(crate) fn stop_ring(&self, outgoing: bool) {
        let slot = if outgoing {
            &self.sounds.calling
        } else {
            &self.sounds.ring
        };
        if let Some((_, sound)) = slot.borrow_mut().take() {
            sound.stop();
        }
    }

    /// Discord says nobody is rung in our call any more. The first
    /// moments are ignored: the call is created before we ask Discord to
    /// ring the others, and its first update may list nobody.
    pub(crate) fn call_stopped_ringing(&self) {
        let started = self.sounds.calling_since.get();
        if started.is_some_and(|started| started.elapsed() > RING_GRACE) {
            self.stop_ring(true);
        }
    }

    /// The channel we are ringing others in, if any.
    pub(crate) fn calling_channel(&self) -> Option<Snowflake> {
        self.sounds
            .calling
            .borrow()
            .as_ref()
            .map(|(channel, _)| *channel)
    }
}

/// Demo runs print the sounds instead of playing them, and download
/// nothing.
fn dry_run() -> bool {
    std::env::var_os("KYLECORD_DEMO").is_some()
}

/// Who arrived and who went, between two looks at a group of people.
pub(crate) fn arrivals(before: &HashSet<Snowflake>, after: &HashSet<Snowflake>) -> (usize, usize) {
    (
        after.difference(before).count(),
        before.difference(after).count(),
    )
}

impl AppState {
    /// The people (other than you) in the call you are in, from the store.
    fn call_peers(&self, channel_id: Snowflake) -> HashSet<Snowflake> {
        self.with_session(|session| {
            let me = session.user.as_ref().map(|user| user.id);
            session.store.read(|state| {
                state
                    .voice_states
                    .values()
                    .filter(|voice| voice.channel_id == Some(channel_id))
                    .map(|voice| voice.user_id)
                    .filter(|user| Some(*user) != me)
                    .collect()
            })
        })
        .unwrap_or_default()
    }

    /// You joined `channel_id` (or were moved there): its people are
    /// known from now on, without a sound each.
    pub(crate) fn sounds_call_joined(self: &Rc<Self>, channel_id: Snowflake, fresh: bool) {
        if fresh {
            self.play_sound(SoundEvent::VoiceJoin);
        }
        let peers = self.call_peers(channel_id);
        *self.sounds.peers.borrow_mut() = Some((channel_id, peers));
    }

    pub(crate) fn sounds_call_left(self: &Rc<Self>, lost: bool) {
        self.sounds.peers.borrow_mut().take();
        self.stop_ring(true);
        self.play_sound(if lost {
            SoundEvent::Disconnected
        } else {
            SoundEvent::VoiceLeave
        });
    }

    /// Voice states changed: someone may have joined or left your call.
    pub(crate) fn sounds_voice_changed(self: &Rc<Self>) {
        let Some(channel_id) = self
            .sounds
            .peers
            .borrow()
            .as_ref()
            .map(|(channel, _)| *channel)
        else {
            return;
        };
        let now = self.call_peers(channel_id);
        let (joined, left) = {
            let mut peers = self.sounds.peers.borrow_mut();
            let Some((_, before)) = peers.as_mut() else {
                return;
            };
            let change = arrivals(before, &now);
            *before = now.clone();
            change
        };
        if joined > 0 {
            // Someone answered: stop ringing them.
            if self.calling_channel() == Some(channel_id) {
                self.stop_ring(true);
            }
            self.play_sound(SoundEvent::UserJoin);
        } else if left > 0 {
            self.play_sound(SoundEvent::UserLeave);
        }
    }

    /// A stream's state changed: for your own, someone may have started or
    /// stopped watching.
    pub(crate) fn sounds_stream_changed(self: &Rc<Self>, key: &str) {
        let me = self
            .with_session(|session| session.user.as_ref().map(|user| user.id))
            .flatten();
        let Some(me) = me else {
            return;
        };
        if key.rsplit(':').next() != Some(me.to_string().as_str()) {
            return;
        }
        let viewers: Option<HashSet<Snowflake>> = self
            .with_session(|session| {
                session.store.read(|state| {
                    state
                        .streams
                        .get(key)
                        .map(|stream| stream.viewer_ids.iter().copied().collect())
                })
            })
            .flatten();
        let Some(viewers) = viewers else {
            // The stream ended.
            self.sounds.viewers.borrow_mut().remove(key);
            return;
        };
        let before = self
            .sounds
            .viewers
            .borrow_mut()
            .insert(key.to_owned(), viewers.clone());
        let Some(before) = before else {
            return;
        };
        let (joined, left) = arrivals(&before, &viewers);
        if joined > 0 {
            self.play_sound(SoundEvent::ViewerJoin);
        } else if left > 0 {
            self.play_sound(SoundEvent::ViewerLeave);
        }
    }

    /// Whether your status is Do Not Disturb (no ringing then).
    pub(crate) fn do_not_disturb(&self) -> bool {
        self.with_session(|session| {
            session
                .store
                .read(|state| state.status.as_deref() == Some("dnd"))
        })
        .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Against the real discord.com (public files, no account): run with
    /// `cargo test -p kc-ui discord_sounds -- --ignored`.
    #[test]
    #[ignore = "downloads from discord.com"]
    fn discord_sounds_are_found_and_downloaded() {
        let dir =
            std::env::temp_dir().join(format!("kylecord-discord-sounds-{}", std::process::id()));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (manifest, present, problems) = runtime.block_on(ensure(dir.clone(), true)).unwrap();
        eprintln!(
            "found {} names; {present} wanted present; problems {problems:?}",
            manifest.files.len()
        );
        assert_eq!(present, wanted().len(), "{problems:?}");
        for name in wanted() {
            let file = &manifest.files[name];
            let bytes = std::fs::read(dir.join("discord").join(file)).unwrap();
            assert!(looks_like_mp3(&bytes), "{name}");
        }
        // They decode and play (into a fakesink, not the speakers).
        gstreamer::init().unwrap();
        let sink = gstreamer::ElementFactory::make("fakesink")
            .property("sync", false)
            .build()
            .unwrap();
        let path = dir.join("discord").join(&manifest.files["message1"]);
        let sound = kc_media::sound::play_on(&path, 0.5, false, sink).unwrap();
        let started = std::time::Instant::now();
        while !sound.finished() && started.elapsed() < std::time::Duration::from_secs(5) {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(sound.played());
        // A second run downloads nothing new and keeps the manifest.
        let again = runtime.block_on(ensure(dir.clone(), false)).unwrap();
        assert_eq!(again.0.files, manifest.files);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn arrivals_count_both_ways() {
        let before: HashSet<Snowflake> = [Snowflake(1), Snowflake(2)].into();
        let after: HashSet<Snowflake> = [Snowflake(2), Snowflake(3), Snowflake(4)].into();
        assert_eq!(arrivals(&before, &after), (2, 1));
        assert_eq!(arrivals(&after, &after), (0, 0));
    }

    #[test]
    fn every_event_has_a_distinct_key_and_a_tone() {
        let mut keys: Vec<&str> = SoundEvent::ALL.iter().map(|event| event.key()).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), SoundEvent::ALL.len());
        for event in SoundEvent::ALL {
            assert!(!event.tone().is_empty());
            assert!(!event.title().is_empty());
        }
        assert!(SoundEvent::IncomingRing.loops() && !SoundEvent::Message.loops());
    }

    /// Shaped like the web app's main bundle (2026-11): the context map of
    /// sound names to module ids, and the modules exporting the files.
    const BUNDLE: &str = r#"var a={"./mention1.mp3":"369135","./message1.mp3":"31742","./mute.mp3":645951,"./call_ringing.mp3":"159944"};
        ,31742(e,t,n){"use strict";e.exports=n.p+"3ed22d14f3c30bc4.mp3"},87636(e,t,n){"use strict";e.exports=n.p+"00b48ea9fc787786.mp3"}
        ,645951:function(e,t,n){e.exports=n.p+"7d8709c9f90d5d1e.mp3"},159944:(e,t,n)=>{"use strict";e.exports=n.p+"aaaa1111bbbb2222.mp3"}"#;

    #[test]
    fn sound_files_are_found_in_the_bundle() {
        let mut clues = Clues::default();
        clues.read(BUNDLE);
        let files = clues.files();
        assert_eq!(files["message1"], "3ed22d14f3c30bc4.mp3");
        assert_eq!(files["mute"], "7d8709c9f90d5d1e.mp3");
        assert_eq!(files["call_ringing"], "aaaa1111bbbb2222.mp3");
        // A name whose module is in no script read so far.
        assert!(!files.contains_key("mention1"));
        assert!(clues.has_all(&["message1", "mute"]));
        assert!(!clues.has_all(&["message1", "mention1"]));
    }

    #[test]
    fn names_and_files_may_come_from_different_scripts() {
        let mut clues = Clues::default();
        clues.read(r#"{"./deafen.mp3":"12"}"#);
        assert!(clues.files().is_empty());
        clues.read(r#",12(e,t,n){"use strict";e.exports=n.p+"0123456789abcdef.mp3"}"#);
        assert_eq!(clues.files()["deafen"], "0123456789abcdef.mp3");
    }

    #[test]
    fn the_main_bundle_is_read_first() {
        let html = r#"<script defer src="/assets/123.aa.js"></script>
            <script defer src="/assets/web.f53eaa9877e51f2f.js"></script>
            <link href="/assets/x.css"><script src="/assets/9.bb.js"></script>"#;
        assert_eq!(
            script_urls(html),
            [
                "https://discord.com/assets/web.f53eaa9877e51f2f.js",
                "https://discord.com/assets/123.aa.js",
                "https://discord.com/assets/9.bb.js"
            ]
        );
    }

    #[test]
    fn mp3s_are_recognised() {
        assert!(looks_like_mp3(b"ID3\x04rest"));
        assert!(looks_like_mp3(&[0xFF, 0xFB, 0x90]));
        assert!(!looks_like_mp3(b"<!DOCTYPE html>"));
    }

    #[test]
    fn overrides_then_discord_then_the_tone() {
        let root = std::env::temp_dir().join(format!("kylecord-sounds-{}", std::process::id()));
        let overrides = root.join("config");
        let cache = root.join("cache");
        std::fs::create_dir_all(cache.join("discord")).unwrap();
        std::fs::create_dir_all(&overrides).unwrap();
        let manifest = Manifest {
            discovered: 1,
            files: BTreeMap::from([("mute".to_owned(), "abc.mp3".to_owned())]),
        };
        // Named but not downloaded: the tone.
        assert_eq!(
            resolve(SoundEvent::Mute, Some(&overrides), &cache, Some(&manifest)),
            Source::Builtin(cache.join("builtin/mute.wav"))
        );
        std::fs::write(cache.join("discord/abc.mp3"), b"ID3").unwrap();
        assert_eq!(
            resolve(SoundEvent::Mute, Some(&overrides), &cache, Some(&manifest)),
            Source::Discord(cache.join("discord/abc.mp3"))
        );
        std::fs::write(overrides.join("mute.ogg"), b"x").unwrap();
        assert_eq!(
            resolve(SoundEvent::Mute, Some(&overrides), &cache, Some(&manifest)),
            Source::Override(overrides.join("mute.ogg"))
        );
        // Another event is untouched by that override.
        assert!(matches!(
            resolve(
                SoundEvent::Unmute,
                Some(&overrides),
                &cache,
                Some(&manifest)
            ),
            Source::Builtin(_)
        ));
        let saved = root.join("saved");
        manifest.save(&saved).unwrap();
        assert_eq!(Manifest::load(&saved), Some(manifest));
        let _ = std::fs::remove_dir_all(root);
    }
}
