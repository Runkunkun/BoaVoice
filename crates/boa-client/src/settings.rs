//! What the client remembers between runs.
//!
//! One JSON file, written atomically, and every field defaulted. The defaults matter
//! more than they look: this file is read on a machine where it may be older than the
//! program, so a missing field must produce a working app rather than a parse error, and
//! a *corrupt* file must produce a working app too — see [`Settings::load`], which throws
//! it away rather than refusing to start. A voice client that will not launch because of
//! one bad byte in a settings file is a voice client somebody cannot use to ask for help.
//!
//! The token is in here, in the clear. That is worth being explicit about rather than
//! pretending otherwise: it is a bearer token for one self-hosted server, stored in the
//! user's own data directory with the user's own permissions. Putting it in the platform
//! keychain would be better and is a real improvement to make; encrypting it with a key
//! stored in the same file, which is the usual middle option, would only look better.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Where the client points and who it is.
///
/// Every field's default is its type's — an empty string, `None`, an empty map, zero — which is why
/// this derives `Default` rather than spelling them out. The two nested structs do *not*: their
/// defaults are real choices and are written out with their reasons.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// `http://host:port` or `https://host`. Stored as the user typed it, normalised on
    /// use — see [`Settings::base_url`].
    pub server_url: String,
    /// The login token, or `None` when signed out.
    pub token: Option<String>,
    /// The account name, only so the connect screen can offer it back.
    pub user_name: String,
    /// The channel that was open last, reopened on the next run.
    pub last_channel: Option<u64>,
    pub voice: VoiceSettings,
    pub screen: ScreenSettings,
    /// Per-person output volume, 0.0…2.0, keyed by user id as a string.
    ///
    /// Keyed by string because JSON object keys are strings and a `HashMap<Id, f32>`
    /// would need a custom deserialiser to say so.
    pub user_volume: HashMap<String, f32>,
    /// How many days to keep downloaded attachments locally. Zero means forever.
    ///
    /// Forever is the default, and it is the whole point of the storage design: the
    /// server drops its copy after three days, so anything this client deletes is gone.
    pub local_retention_days: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceSettings {
    /// Input device name, or `None` for the system default.
    ///
    /// A *name*, not an index: indices are assigned in enumeration order and change when
    /// a headset is plugged in, so a saved index reliably selects the wrong device the
    /// next time hardware moves. A name that has disappeared falls back to the default,
    /// which is the correct behaviour for an unplugged microphone.
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    /// Applied to the captured signal before anything else, 0.0…4.0.
    pub input_gain: f32,
    /// Master output level, 0.0…2.0.
    pub output_volume: f32,
    /// RNNoise-style suppression on the captured signal.
    pub noise_suppression: bool,
    /// Gate threshold in dBFS. Anything quieter is not transmitted.
    ///
    /// Negative, and −45 by default: quiet enough to pass a soft voice, loud enough to
    /// stop a fan. This is separate from the suppressor because they solve different
    /// problems — suppression removes steady noise *from* speech, a gate stops sending
    /// when there is no speech at all, and only the gate saves bandwidth.
    pub gate_threshold_db: f32,
    /// Voice activity has to stay below the threshold for this long before the gate
    /// closes. Without a hang time the gate chops the ends off words.
    pub gate_hang_ms: u32,
    /// Transmit only while a key is held.
    pub push_to_talk: bool,
    /// The key to hold, as egui names it.
    pub push_to_talk_key: String,
    pub muted: bool,
    pub deafened: bool,
    /// Milliseconds of audio the receiver holds before playing, to absorb jitter.
    ///
    /// The one setting with a genuine trade-off in it: every millisecond here is a
    /// millisecond of added delay in the conversation, and too few means every network
    /// hiccup is an audible gap. 60 ms is three packets, which covers ordinary wifi.
    pub jitter_ms: u32,
}

impl Default for VoiceSettings {
    fn default() -> Self {
        VoiceSettings {
            input_device: None,
            output_device: None,
            input_gain: 1.0,
            output_volume: 1.0,
            noise_suppression: true,
            gate_threshold_db: -45.0,
            gate_hang_ms: 300,
            push_to_talk: false,
            push_to_talk_key: "F13".to_string(),
            muted: false,
            deafened: false,
            jitter_ms: 60,
        }
    }
}

/// What a screen share should look like.
///
/// Note what is not here: any notion of a tier, a plan, or a server-side limit. These
/// numbers are the encoder's settings on this machine, they go as high as the hardware
/// and the uplink allow, and the server is told about them only so that a viewer's
/// decoder knows what to expect.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ScreenSettings {
    /// Longest edge the picture may reach, in pixels.
    ///
    /// A *ceiling*, not a choice — and not shown in the settings screen. The share keeps its source's
    /// own resolution up to this; it stays in the file so that somebody with a reason can change it by
    /// hand, in either direction.
    pub max_dimension: u32,
    pub fps: u32,
    pub kbps: u32,
    /// Include the desktop's own audio in the share.
    pub with_audio: bool,
}

impl Default for ScreenSettings {
    fn default() -> Self {
        // 1080p, 30 frames a second, 6 Mbit/s — and every one of those is a *default* rather than a
        // limit: the settings screen and this file go to 8K, 240 fps and 200 Mbit/s, and nothing on the
        // server has an opinion about any of it.
        //
        // The reason to start here rather than at 4K60 is the other end. Watchers decode in software,
        // and a 4K60 stream costs several times more to decode than to encode — the sender's hardware
        // encoder makes it look free while everybody watching drops frames. 4K keyframes are also close
        // to a megabyte each, and a megabyte arriving in one frame interval is a burst most home links
        // simply discard. Somebody on a LAN who wants 4K60 can have it in two clicks; somebody who never
        // opens the settings gets a share that works.
        ScreenSettings { max_dimension: 1_920, fps: 30, kbps: 6_000, with_audio: true }
    }
}

impl Settings {
    /// Read the settings file, or return defaults.
    ///
    /// A file that will not parse is moved aside and replaced, not refused. The
    /// alternative is an app that cannot start because of a byte it wrote itself, and the
    /// contents are recoverable from the copy if anybody cares.
    pub fn load() -> Self {
        let path = crate::paths::settings_path();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Settings::default(),
            Err(err) => {
                log::warn!("settings: {}: {err}", path.display());
                return Settings::default();
            }
        };
        match serde_json::from_str::<Settings>(&text) {
            Ok(settings) => settings,
            Err(err) => {
                let spoiled = path.with_extension("json.broken");
                log::error!("settings: {err}; keeping the old file as {}", spoiled.display());
                let _ = std::fs::rename(&path, &spoiled);
                Settings::default()
            }
        }
    }

    /// Write the settings file.
    ///
    /// Through a temporary file and a rename, so an interrupted write cannot leave a
    /// half-written settings file — which is the one input this program cannot validate
    /// its way out of, since it is where the validation rules would be stored.
    pub fn save(&self) -> std::io::Result<()> {
        let path = crate::paths::settings_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)
            .map_err(|err| std::io::Error::other(err.to_string()))?;
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, text)?;
        std::fs::rename(&temp, &path)
    }

    /// Save, complaining to the log rather than to the caller.
    ///
    /// Used from the UI, where every settings change would otherwise need error handling
    /// for a failure the user can do nothing about mid-click.
    pub fn save_quietly(&self) {
        if let Err(err) = self.save() {
            log::error!("settings: could not save: {err}");
        }
    }

    /// The server URL with a scheme and no trailing slash.
    ///
    /// People type `boa.example.com`, `boa.example.com:8787` and
    /// `http://boa.example.com/` interchangeably, and all three should work. A bare host
    /// gets `http://`, not `https://`: a self-hosted server on a LAN usually has no
    /// certificate, and defaulting to a scheme that cannot connect would make the
    /// commonest case fail with a TLS error nobody expects.
    pub fn base_url(&self) -> String {
        normalise_url(&self.server_url)
    }

    /// The WebSocket URL for the control plane.
    pub fn ws_url(&self) -> String {
        let base = self.base_url();
        let base = base.strip_prefix("http://").map(|rest| format!("ws://{rest}")).unwrap_or_else(
            || {
                base.strip_prefix("https://")
                    .map(|rest| format!("wss://{rest}"))
                    .unwrap_or_else(|| base.clone())
            },
        );
        format!("{base}/ws")
    }

    /// This person's volume, defaulting to unity.
    pub fn volume_for(&self, user: boa_proto::Id) -> f32 {
        self.user_volume.get(&user.0.to_string()).copied().unwrap_or(1.0).clamp(0.0, 2.0)
    }

    pub fn set_volume_for(&mut self, user: boa_proto::Id, volume: f32) {
        let volume = volume.clamp(0.0, 2.0);
        if (volume - 1.0).abs() < 0.001 {
            // Unity is the default, so storing it would grow the file by one entry per
            // person ever spoken to for no effect.
            self.user_volume.remove(&user.0.to_string());
        } else {
            self.user_volume.insert(user.0.to_string(), volume);
        }
    }

    /// Clamp everything into range.
    ///
    /// Called after loading. A hand-edited file — or one written by an older version with
    /// a different idea of the limits — must not be able to produce a gain of 400 that
    /// deafens somebody on the first frame.
    pub fn sanitise(&mut self) {
        self.voice.input_gain = self.voice.input_gain.clamp(0.0, 4.0);
        self.voice.output_volume = self.voice.output_volume.clamp(0.0, 2.0);
        self.voice.gate_threshold_db = self.voice.gate_threshold_db.clamp(-90.0, 0.0);
        self.voice.gate_hang_ms = self.voice.gate_hang_ms.clamp(0, 2_000);
        self.voice.jitter_ms = self.voice.jitter_ms.clamp(20, 500);
        self.screen.max_dimension = self.screen.max_dimension.clamp(320, 7_680);
        self.screen.fps = self.screen.fps.clamp(1, 240);
        self.screen.kbps = self.screen.kbps.clamp(200, 200_000);
        for volume in self.user_volume.values_mut() {
            *volume = volume.clamp(0.0, 2.0);
        }
    }
}

/// Give a typed-in server address a scheme, and take off any trailing slash.
fn normalise_url(text: &str) -> String {
    let text = text.trim().trim_end_matches('/');
    if text.is_empty() {
        return String::new();
    }
    if text.starts_with("http://") || text.starts_with("https://") {
        return text.to_string();
    }
    // Somebody who typed a `ws://` URL meant this server; accept it rather than
    // producing `http://ws://…`.
    if let Some(rest) = text.strip_prefix("ws://") {
        return format!("http://{rest}");
    }
    if let Some(rest) = text.strip_prefix("wss://") {
        return format!("https://{rest}");
    }
    format!("http://{text}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use boa_proto::Id;

    #[test]
    fn a_bare_host_gets_http_because_a_self_hosted_box_usually_has_no_certificate() {
        let url = |text: &str| Settings { server_url: text.into(), ..Default::default() };
        assert_eq!(url("boa.example.com").base_url(), "http://boa.example.com");
        assert_eq!(url("192.168.1.10:8787").base_url(), "http://192.168.1.10:8787");
        assert_eq!(url("  http://host/  ").base_url(), "http://host");
        assert_eq!(url("https://host/").base_url(), "https://host");
        assert_eq!(url("").base_url(), "");
    }

    #[test]
    fn the_websocket_url_follows_the_schemes_security() {
        let url = |text: &str| Settings { server_url: text.into(), ..Default::default() };
        assert_eq!(url("host:8787").ws_url(), "ws://host:8787/ws");
        assert_eq!(url("https://host").ws_url(), "wss://host/ws");
        // Somebody who pasted a WebSocket URL meant the same server.
        assert_eq!(url("wss://host").ws_url(), "wss://host/ws");
        assert_eq!(url("ws://host:1/").ws_url(), "ws://host:1/ws");
    }

    #[test]
    fn missing_fields_load_as_defaults_rather_than_as_an_error() {
        // The shape an older version's file might have.
        let settings: Settings = serde_json::from_str(r#"{"server_url":"host"}"#).unwrap();
        assert_eq!(settings.server_url, "host");
        assert!(settings.voice.noise_suppression, "the default, not false");
        assert_eq!(settings.voice.jitter_ms, 60);
        assert_eq!(settings.screen.fps, 30);
        assert_eq!(settings.screen.max_dimension, 1_920);

        // And an entirely empty object is valid.
        let settings: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings.voice.input_gain, 1.0);
    }

    #[test]
    fn a_hand_edited_file_cannot_produce_a_dangerous_gain() {
        let mut settings: Settings = serde_json::from_str(
            r#"{"voice":{"input_gain":400.0,"output_volume":-3.0,"jitter_ms":100000},
                "screen":{"fps":9000,"kbps":1,"max_dimension":2}}"#,
        )
        .unwrap();
        settings.sanitise();
        assert_eq!(settings.voice.input_gain, 4.0);
        assert_eq!(settings.voice.output_volume, 0.0);
        assert_eq!(settings.voice.jitter_ms, 500);
        assert_eq!(settings.screen.fps, 240);
        assert_eq!(settings.screen.kbps, 200);
        assert_eq!(settings.screen.max_dimension, 320);
    }

    #[test]
    fn per_person_volume_stores_only_what_differs_from_unity() {
        let mut settings = Settings::default();
        assert_eq!(settings.volume_for(Id(7)), 1.0);

        settings.set_volume_for(Id(7), 0.5);
        assert_eq!(settings.volume_for(Id(7)), 0.5);
        assert_eq!(settings.user_volume.len(), 1);

        settings.set_volume_for(Id(7), 1.0);
        assert!(settings.user_volume.is_empty(), "unity is the default, not an entry");

        settings.set_volume_for(Id(7), 99.0);
        assert_eq!(settings.volume_for(Id(7)), 2.0, "clamped on the way in");
    }

    #[test]
    fn attachments_are_kept_forever_by_default() {
        // The server keeps its copy for three days; anything this client deletes is gone
        // for good, so the default has to be to keep it.
        assert_eq!(Settings::default().local_retention_days, 0);
    }

    #[test]
    fn a_round_trip_through_json_preserves_everything() {
        let mut before = Settings {
            server_url: "boa.example.com".into(),
            token: Some("t".into()),
            ..Default::default()
        };
        before.set_volume_for(Id(3), 1.5);
        before.screen.kbps = 40_000;

        let text = serde_json::to_string(&before).unwrap();
        let after: Settings = serde_json::from_str(&text).unwrap();
        assert_eq!(after.server_url, before.server_url);
        assert_eq!(after.token, before.token);
        assert_eq!(after.volume_for(Id(3)), 1.5);
        assert_eq!(after.screen.kbps, 40_000);
    }
}
