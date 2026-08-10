//! The application: its state, its frame loop, and the chrome around the views.
//!
//! Everything the interface can do is expressed as an [`Action`] rather than performed where the
//! click happens. The reason is ordinary borrow discipline made useful: a message row needs
//! `&State` to draw itself and `&mut Net` to send anything, and the two cannot be held at once
//! through `&mut self`. Returning an action from the drawing code and applying it afterwards makes
//! that a non-problem, and has the side effect that every state change in the app passes through
//! one `match` — which is a good place to look when something happens that should not.
//!
//! The frame loop is lazy where it can be and paced where it must be. egui repaints on input, so a
//! window sitting idle costs nothing; while a call is up or an image is decoding, the loop asks for
//! a repaint a few times a second, which is what the level meter and the speaking rings need.

pub mod chat;
pub mod connect;
pub mod glass;
pub mod icons;
pub mod images;
pub mod settings_view;
pub mod sidebar;
pub mod transfers;
pub mod voice_bar;
pub mod widgets;

use std::time::{Duration, Instant};

use boa_proto::{Attachment, ChannelKind, ClientMsg, Id, ServerMsg};

use crate::audio::devices::Devices;
use crate::audio::VoiceSession;
use crate::net::{Command, Event, Net, Status};
use crate::settings::Settings;
use crate::state::{Pending, State};
use crate::theme;

/// Which pane the content area is showing.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum View {
    /// Not connected: pick a server and sign in.
    Connect,
    Channel(Id),
    Settings,
    /// Watching somebody's screen.
    Watching(Id),
}

/// Something the user asked for, to be applied after drawing.
#[derive(Debug)]
pub enum Action {
    Go(View),
    Probe,
    LogIn,
    Register,
    LogOut,
    OpenChannel(Id),
    LoadOlder(Id),
    SendComposer,
    AttachFiles,
    Typing(Id),
    WantAttachment(Attachment),
    OpenAttachment(Attachment),
    RevealAttachments,
    JoinVoice(Id),
    LeaveVoice,
    ToggleMute,
    ToggleDeafen,
    StartScreen,
    /// One of the things offered by the picker.
    ShareSource(crate::screen::Source),
    CancelSharePicker,
    StopScreen,
    Watch(Id),
    Unwatch(Id),
    CreateChannel(String, ChannelKind),
    /// Offer a file straight to somebody, without the server touching it.
    SendFileDirect(Id),
    AcceptOffer(Id, String),
    DeclineOffer(Id, String),
    CancelTransfer(u64),
    /// The settings were edited and should be saved (and applied to a live call).
    SettingsChanged,
    Notify(Notice),
}

/// What the voice engine reports to the interface, once per frame.
///
/// A snapshot rather than a handle, so drawing code cannot accidentally hold a lock that the audio
/// callback is waiting on — which would be a glitch in the call rather than a slow frame.
#[derive(Clone, Copy, Debug)]
pub struct Meter {
    /// Peak level of the captured signal, 0…1.
    pub input_level: f32,
    /// Whether that signal is currently being transmitted.
    pub gate_open: bool,
    /// Whether we ourselves are counted as speaking.
    pub speaking: bool,
    /// The gate's threshold, as a 0…1 position for the meter.
    pub threshold: f32,
    /// Whether media is reaching the relay. Separate from the control connection because the UDP
    /// port is separate, and a call where chat works and nobody can hear anybody is the commonest
    /// self-hosting mistake.
    pub media_ok: bool,
}

impl Default for Meter {
    fn default() -> Self {
        Meter { input_level: 0.0, gate_open: false, speaking: false, threshold: 0.1, media_ok: false }
    }
}

/// A transient message shown at the bottom of the content area.
#[derive(Clone, Debug)]
pub struct Notice {
    pub text: String,
    pub kind: NoticeKind,
    pub at: Instant,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NoticeKind {
    Info,
    Success,
    Error,
}

impl Notice {
    pub fn info(text: impl Into<String>) -> Self {
        Notice { text: text.into(), kind: NoticeKind::Info, at: Instant::now() }
    }
    pub fn success(text: impl Into<String>) -> Self {
        Notice { text: text.into(), kind: NoticeKind::Success, at: Instant::now() }
    }
    pub fn error(text: impl Into<String>) -> Self {
        Notice { text: text.into(), kind: NoticeKind::Error, at: Instant::now() }
    }

    fn colour(&self) -> egui::Color32 {
        match self.kind {
            NoticeKind::Info => theme::TEXT_DIM,
            NoticeKind::Success => theme::OK,
            NoticeKind::Error => theme::ERROR,
        }
    }

    /// How long a notice stays up.
    ///
    /// Errors last longer than successes, because a success is confirming something the user just
    /// did and an error is telling them something they did not know.
    fn expired(&self) -> bool {
        let life = match self.kind {
            NoticeKind::Error => Duration::from_secs(12),
            _ => Duration::from_secs(5),
        };
        self.at.elapsed() > life
    }
}

pub struct App {
    net: Net,
    settings: Settings,
    state: State,
    status: Status,
    view: View,
    connect: connect::Form,
    images: images::Images,
    devices: Devices,
    meter: Meter,
    composer: String,
    new_channel: String,
    notices: Vec<Notice>,
    /// Attachments already asked for, so a log that redraws sixty times a second does not queue
    /// sixty downloads of the same file.
    requested: std::collections::HashSet<Id>,
    /// When we last told the server we were typing, to keep it to one every few seconds.
    last_typing: Option<Instant>,
    /// Set until the platform backdrop has been installed, which can only happen once there is a
    /// window to install it behind.
    vibrancy_pending: bool,
    /// The app's mark, decoded once from the same PNG the window icon uses.
    ///
    /// The same artwork rather than a second hand-drawn vector: two versions of a logo drift, and
    /// the one people see in the dock should be the one they see in the window.
    logo: Option<egui::TextureHandle>,
    /// The live voice session, if we are in a call. Owned here because the device streams it holds
    /// are not `Send` on every platform and so have to stay on the thread that built them.
    voice: Option<VoiceSession>,
    /// The last value of "am I talking" that the server was told, so the announcement goes out on
    /// the change rather than every frame.
    announced_speaking: bool,
    /// Our own screen share, if one is running.
    share: Option<crate::screen::Share>,
    /// Whose screen we are watching, and the decoder doing it.
    watcher: Option<(Id, crate::screen::Watcher)>,
    /// A decoder for our *own* share, fed straight from the encoder.
    ///
    /// The relay never sends a stream back to whoever sent it, so this is the only way to see what
    /// everybody else is seeing — and seeing it is the point: a share nobody can check is a share that
    /// is silently showing the wrong window.
    preview: Option<crate::screen::Watcher>,
    /// The texture the watched screen is drawn from, with the frame number it came from.
    screen_texture: Option<(egui::TextureHandle, u64)>,
    /// Whose picture that texture holds, so switching between two screens does not show the first
    /// one's last frame under the second one's name.
    texture_for: Option<Id>,
    /// The sources offered while somebody is choosing what to share.
    picking: Option<Vec<crate::screen::Source>>,
    /// What they chose, kept until the server hands back a stream id for it.
    pending_source: Option<crate::screen::Source>,
    /// Direct file transfers: the thread that runs them, and what is in flight.
    transfers: crate::transfer::Transfers,
    active_transfers: Vec<transfers::Active>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::apply(&cc.egui_ctx);
        // Images arrive as bytes and are decoded by this crate, so egui's own image loaders are not
        // installed — they would pull a second copy of the same decoders into the binary.

        let mut settings = Settings::load();
        settings.sanitise();

        // Housekeeping on the local attachment store, once, at startup. Never automatic unless the
        // user asked for a retention period: these files are irreplaceable after three days.
        if settings.local_retention_days > 0 {
            match crate::cache::prune(settings.local_retention_days) {
                Ok(0) => {}
                Ok(n) => log::info!("cache: pruned {n} old attachment(s)"),
                Err(err) => log::warn!("cache: {err:#}"),
            }
        }

        let net = Net::spawn(cc.egui_ctx.clone());
        let mut app = App {
            net,
            state: State::default(),
            status: Status::Offline,
            view: View::Connect,
            connect: connect::Form {
                url: settings.server_url.clone(),
                name: settings.user_name.clone(),
                ..Default::default()
            },
            images: images::Images::new(),
            devices: Devices::enumerate(),
            meter: Meter::default(),
            composer: String::new(),
            new_channel: String::new(),
            notices: Vec::new(),
            requested: std::collections::HashSet::new(),
            last_typing: None,
            vibrancy_pending: true,
            logo: load_logo(&cc.egui_ctx),
            voice: None,
            announced_speaking: false,
            share: None,
            picking: None,
            pending_source: None,
            watcher: None,
            preview: None,
            screen_texture: None,
            texture_for: None,
            transfers: crate::transfer::Transfers::spawn(cc.egui_ctx.clone()),
            active_transfers: Vec::new(),
            settings,
        };

        // A saved token means straight in, without a login screen. The connect screen appears only
        // if that token turns out to be stale.
        if let (Some(token), false) =
            (app.settings.token.clone(), app.settings.base_url().is_empty())
        {
            app.connect.busy = Some("connecting…");
            app.net.send(Command::Connect { base: app.settings.base_url(), token });
        }

        crate::diagnostics::note(&format!(
            "audio: {} input(s), {} output(s)",
            app.devices.inputs.len(),
            app.devices.outputs.len()
        ));
        app
    }

    fn notify(&mut self, notice: Notice) {
        log::debug!("notice: {}", notice.text);
        self.notices.push(notice);
        // A cap, because a server having a bad minute should not push the content area off the
        // bottom of the window.
        if self.notices.len() > 4 {
            self.notices.remove(0);
        }
    }

    /// Apply everything the network has sent since the last frame.
    fn pump_network(&mut self) {
        for event in self.net.drain() {
            match event {
                Event::Status(status) => {
                    match &status {
                        Status::Connected => {
                            self.connect.busy = None;
                            self.connect.error = None;
                        }
                        Status::Rejected(why) => {
                            // The token is no good. Forget it, or the next launch loops through the
                            // same rejection with no way to reach the login form.
                            self.settings.token = None;
                            self.settings.save_quietly();
                            self.state.reset();
                            self.view = View::Connect;
                            self.connect.busy = None;
                            self.connect.error = Some(why.clone());
                        }
                        Status::Offline => {
                            self.state.reset();
                            self.view = View::Connect;
                            self.connect.busy = None;
                        }
                        Status::Connecting | Status::Reconnecting { .. } => {}
                    }
                    self.status = status;
                }

                Event::Probed(Ok(probe)) => {
                    self.connect.busy = None;
                    self.connect.error = None;
                    self.connect.probe = Some(probe);
                }
                Event::Probed(Err(why)) => {
                    self.connect.busy = None;
                    self.connect.probe = None;
                    self.connect.error = Some(why);
                }

                Event::Authenticated(Ok(session)) => {
                    self.settings.token = Some(session.token.clone());
                    self.settings.user_name = session.user.name.clone();
                    self.settings.server_url = self.connect.url.clone();
                    self.settings.save_quietly();
                    self.connect.busy = Some("connecting…");
                    self.connect.password.clear();
                    self.net.send(Command::Connect {
                        base: self.settings.base_url(),
                        token: session.token,
                    });
                }
                Event::Authenticated(Err(why)) => {
                    self.connect.busy = None;
                    self.connect.error = Some(why);
                }

                Event::Server(msg) => self.handle_server(msg),

                Event::AttachmentReady { attachment, sha256 } => {
                    self.requested.remove(&attachment);
                    // Drop any earlier decode failure for these bytes, so a retry after a
                    // truncated download is not stuck showing the failure.
                    self.images.forget(&sha256);
                }
                Event::AttachmentGone { attachment } => {
                    self.requested.remove(&attachment);
                }
                Event::Trouble(what) => self.notify(Notice::error(what)),
            }
        }
    }

    /// Where the transfer relays are, from what the server offered.
    fn relays(&self) -> crate::transfer::Relays {
        match &self.state.server {
            Some(server) => crate::transfer::Relays {
                rendezvous: server.wormhole_rendezvous.clone(),
                transit: server.wormhole_transit.clone(),
            },
            None => crate::transfer::Relays::default(),
        }
    }

    /// Apply whatever the transfer thread has reported.
    fn pump_transfers(&mut self) {
        use crate::transfer::Event;
        for event in self.transfers.drain() {
            match event {
                Event::Offered { id, to, channel, code, name, size } => {
                    // The wormhole is open; now the recipient needs the code. This is the only part
                    // of a direct transfer the server ever sees.
                    self.net.send_msg(ClientMsg::OfferFile {
                        to,
                        offer: boa_proto::FileOffer { code, name: name.clone(), size, channel },
                    });
                    self.active_transfers.push(transfers::Active::new(id, to, name, true, size));
                }
                Event::Connected { id, direct } => {
                    if let Some(transfer) = self.active_transfers.iter_mut().find(|t| t.id == id) {
                        transfer.direct = Some(direct);
                    }
                }
                Event::Progress { id, done, total } => {
                    if let Some(transfer) = self.active_transfers.iter_mut().find(|t| t.id == id) {
                        transfer.advance(done, total);
                    }
                }
                Event::Done { id, path } => {
                    let finished = self.take_transfer(id);
                    let name = finished.map(|t| t.name).unwrap_or_default();
                    match path {
                        Some(path) => self.notify(Notice::success(format!(
                            "saved {}",
                            path.display()
                        ))),
                        None => self.notify(Notice::success(format!("sent {name}"))),
                    }
                }
                Event::Failed { id, why } => {
                    let finished = self.take_transfer(id);
                    let name = finished.map(|t| t.name).unwrap_or_default();
                    self.notify(Notice::error(format!("{name}: {why}")));
                }
            }
        }
    }

    fn take_transfer(&mut self, id: u64) -> Option<transfers::Active> {
        let at = self.active_transfers.iter().position(|t| t.id == id)?;
        Some(self.active_transfers.remove(at))
    }

    fn handle_server(&mut self, msg: ServerMsg) {
        match &msg {
            ServerMsg::Ready { channels, .. } => {
                // Reopen what was open last, if it still exists; otherwise the first text channel.
                let wanted = self.settings.last_channel.map(Id);
                let exists = wanted.filter(|id| channels.iter().any(|c| c.id == *id));
                let open = exists.or_else(|| {
                    channels.iter().find(|c| c.kind == ChannelKind::Text).map(|c| c.id)
                });
                if let Some(open) = open {
                    self.view = View::Channel(open);
                }
            }

            ServerMsg::Error { message, fatal, .. } => {
                // Fatal errors are handled by the network layer, which turns them into `Rejected`.
                // Everything else is a failed request and belongs in the status area.
                if !fatal {
                    self.notify(Notice::error(message.clone()));
                }
            }

            ServerMsg::VoiceState(state) => {
                // The mixer's slots are keyed by stream id and the packets carry no user, so this is
                // where a stream learns whose it is — which is what makes per-person volume work, and
                // what makes a share's sound inherit its sharer's volume.
                if let Some(session) = self.voice.as_ref() {
                    session.attribute(state.ssrc, state.user);
                    if let Some(share) = state.screen {
                        session.attribute(share.ssrc, state.user);
                    }
                    session.set_user_volume(state.user, self.settings.volume_for(state.user));
                }
            }

            ServerMsg::VoiceReady { channel, ssrc, key, media_port } => {
                log::info!("voice: session for channel {channel}, ssrc {ssrc}, media port {media_port}");
                self.start_voice(*channel, *ssrc, key, *media_port);
            }

            ServerMsg::VoiceLeave { user, .. } => {
                let mine = self.state.me.as_ref().is_some_and(|me| me.id == *user);
                if mine {
                    // Dropping the session stops the devices, which is also what turns off the
                    // system's microphone indicator — leaving that on after a call is alarming.
                    self.voice = None;
                    self.meter = Meter::default();
                } else if let (Some(session), Some(state)) =
                    (self.voice.as_ref(), self.state.voice.get(user))
                {
                    // Free their mixer slot, or a later speaker cannot have it.
                    session.forget(state.ssrc);
                }
            }

            ServerMsg::ScreenStart { user, share } => {
                // The stream id is the server's to allocate, so capture starts here rather than when
                // the button was pressed — otherwise the first packets would carry an id nobody is
                // subscribed to.
                if self.state.me.as_ref().is_some_and(|me| me.id == *user) {
                    self.begin_capture(share.ssrc);
                } else if let Some(session) = self.voice.as_ref() {
                    session.attribute(share.ssrc, *user);
                    session.set_user_volume(*user, self.settings.volume_for(*user));
                }
            }

            ServerMsg::ScreenStop { user } => {
                // If we were watching them, stop watching: leaving the view up would show a frozen
                // last frame with no indication that it had stopped.
                if self.state.me.as_ref().is_some_and(|me| me.id == *user) {
                    self.share = None;
                    self.preview = None;
                }
                // Free the mixer slot the share's sound was using, or it holds one for the rest of
                // the call and the next sharer may not get one.
                if let (Some(session), Some(share)) =
                    (self.voice.as_ref(), self.state.voice.get(user).and_then(|s| s.screen))
                {
                    session.forget(share.ssrc);
                }
                if self.watcher.as_ref().is_some_and(|(who, _)| who == user) {
                    self.stop_watching();
                }
                if self.view == View::Watching(*user) {
                    self.view = self
                        .state
                        .my_channel()
                        .map(View::Channel)
                        .unwrap_or(View::Connect);
                }
            }

            ServerMsg::FileOffer { from, offer } => {
                self.notify(Notice::info(format!(
                    "{} wants to send you {}",
                    self.state.label(*from),
                    offer.name
                )));
            }

            _ => {}
        }
        self.state.apply(msg);
    }

    /// Open the media path for a voice session the server has just granted.
    ///
    /// The relay's host is the one the control connection used — the server only sends a *port*,
    /// because it has no reliable way to know which of its addresses this client can reach. Its own
    /// idea of its hostname is routinely wrong behind NAT, a reverse proxy or a VPN, whereas the
    /// address that is already working for TCP demonstrably works.
    fn start_voice(&mut self, channel: Id, ssrc: u32, key: &str, media_port: u16) {
        let Some(key) = boa_proto::SessionKey::from_base64(key) else {
            self.notify(Notice::error("the server sent a voice key this client cannot read"));
            return;
        };
        let Some(host) = host_of(&self.settings.base_url()) else {
            self.notify(Notice::error("cannot work out the server's address for voice"));
            return;
        };

        // Resolution can block briefly, which is acceptable here: it happens once per call, on a
        // name that the control connection has already resolved and that the resolver has cached.
        let relay = match resolve(&host, media_port) {
            Ok(relay) => relay,
            Err(err) => {
                self.notify(Notice::error(format!("voice: {host}:{media_port}: {err}")));
                return;
            }
        };

        // The previous session goes first, so its devices are released before the new ones open —
        // on a machine with one microphone, opening it twice fails.
        self.voice = None;
        match VoiceSession::start(relay, key, ssrc, channel, &self.settings.voice) {
            Ok(session) => {
                self.voice = Some(session);
                self.announced_speaking = false;
                // The volumes this person has already chosen apply to the new session.
                self.apply_user_volumes();
            }
            Err(err) => {
                log::error!("voice: {err:#}");
                self.notify(Notice::error(format!("voice: {err}")));
                // The call is left as far as the server is concerned: appearing in the roster of a
                // call you cannot hear or speak in is worse than not being in it.
                self.net.send_msg(ClientMsg::LeaveVoice);
            }
        }
    }

    /// Start ffmpeg on the stream id the server allocated.
    fn begin_capture(&mut self, ssrc: u32) {
        let Some(session) = self.voice.as_ref() else { return };
        // Whatever was chosen when the button was pressed. Absent means the server announced a share
        // this client did not start, which is not something to guess about.
        let Some(source) = self.pending_source.clone() else {
            log::warn!("screen: a share was announced with no source chosen");
            return;
        };
        let transport = match session.transport() {
            Ok(transport) => transport,
            Err(err) => {
                self.notify(Notice::error(format!("screen: {err}")));
                return;
            }
        };
        let settings = self.settings.screen;
        let width = settings.max_dimension.min(crate::screen::MAX_DIMENSION);
        // The box the picture is fitted into. The real size comes out of the encoder — ffmpeg keeps
        // the display's aspect inside this box — and the watcher reads it from the stream itself, so
        // this is the ceiling rather than a promise.
        let height = (width * 9 / 16).max(2);

        // Three pictures of slack. The preview is the least important consumer of the encoder's
        // output — the people watching come first — so its queue is small and its overflow is
        // dropped.
        let (preview_tx, preview_rx) = std::sync::mpsc::sync_channel(3);
        match crate::screen::Share::start(
            transport,
            ssrc,
            &settings,
            &source,
            width,
            height,
            Some(preview_tx),
        ) {
            Ok(share) => {
                let sound = match (share.audio_device(), &share.audio_problem) {
                    (Some(device), _) => format!(", sound from {device}"),
                    (None, Some(_)) => String::new(),
                    (None, None) => ", no sound".to_string(),
                };
                self.notify(Notice::success(format!(
                    "sharing {} · {} fps · {} Mbit/s{sound}",
                    source.label,
                    settings.fps,
                    settings.kbps as f32 / 1000.0
                )));
                // A missing loopback device is not a failure of the share, so it is a separate line
                // and it says what to install — the fix is five minutes, and silence with no
                // explanation is the outcome worth avoiding.
                if let Some(problem) = share.audio_problem.clone() {
                    self.notify(Notice::error(format!("no desktop sound: {problem}")));
                }
                self.share = Some(share);
                self.preview = Some(crate::screen::Watcher::preview(preview_rx));
                self.texture_for = None;
            }
            Err(err) => {
                self.notify(Notice::error(format!("{err}")));
                // Tell the server it is off again, or everybody else sees a share that sends nothing.
                self.net.send_msg(ClientMsg::StopScreen);
            }
        }
    }

    fn stop_watching(&mut self) {
        if let Some((user, _)) = self.watcher.take() {
            self.net.send_msg(ClientMsg::UnwatchScreen { user });
        }
        if let Some(session) = self.voice.as_ref() {
            session.stop_watching();
        }
        self.screen_texture = None;
    }

    /// Notice a share that is running but producing nothing.
    ///
    /// The failure this catches is the common one on macOS: the capture starts, the screen-recording
    /// permission has not been granted, and nothing comes out. From the outside that is a share
    /// everybody sees as active and nobody can see anything in — worse than no share at all, because
    /// the person sharing has no reason to suspect it.
    fn check_share(&mut self) {
        let Some(share) = self.share.as_ref() else { return };

        // The platform saying why beats anything guessed from a frame count, and it arrives whether or
        // not frames were flowing before — a window that gets closed mid-share ends up here.
        if let Some(trouble) = share.trouble() {
            self.share = None;
            self.preview = None;
            self.net.send_msg(ClientMsg::StopScreen);
            self.notify(Notice::error(format!("the screen share stopped: {trouble}")));
            return;
        }
        // Only for ffmpeg, and that is a correctness point rather than a shortcut. ScreenCaptureKit
        // delivers a frame when the screen *changes*, so a share of a genuinely still window produces
        // nothing for as long as it stays still — and it reports refusals as errors at start-up
        // instead, which is a better signal than a frame count. Guessing here would end somebody's
        // share because they stopped moving.
        if share.native() {
            return;
        }
        // ffmpeg, on the other hand, encodes on a clock: a static screen still produces frames, so
        // nothing at all after four seconds means nothing is being captured.
        if share.started.elapsed() < Duration::from_secs(4) {
            return;
        }
        if share.frames.load(std::sync::atomic::Ordering::Relaxed) > 0 {
            return;
        }
        self.share = None;
        self.preview = None;
        self.net.send_msg(ClientMsg::StopScreen);
        self.notify(Notice::error(
            if cfg!(target_os = "macos") {
                "the screen share captured nothing. macOS needs Privacy & Security → \
                 Screen & System Audio Recording → BoaVoice, and the app has to be restarted after."
            } else {
                "the screen share captured nothing — see last-run.log for what ffmpeg said"
            },
        ));
    }

    /// Upload a newly decoded screen frame, if there is one.
    ///
    /// Which decoder is read depends on what is on screen: our own share has its own, fed from the
    /// encoder rather than from the network.
    fn poll_screen(&mut self, ctx: &egui::Context) {
        let View::Watching(target) = self.view else { return };
        let mine = self.state.me.as_ref().is_some_and(|me| me.id == target);
        let watcher = if mine {
            self.preview.as_ref()
        } else {
            self.watcher.as_ref().filter(|(who, _)| *who == target).map(|(_, watcher)| watcher)
        };
        let Some(watcher) = watcher else { return };

        // A texture holding somebody else's last frame must not appear under this person's name.
        if self.texture_for != Some(target) {
            self.screen_texture = None;
            self.texture_for = Some(target);
        }

        let seen = self.screen_texture.as_ref().map(|(_, generation)| *generation).unwrap_or(0);
        let Some(frame) = watcher.take_frame(seen) else { return };

        let image = egui::ColorImage::from_rgba_unmultiplied(
            [frame.width, frame.height],
            &frame.rgba,
        );
        match self.screen_texture.as_mut() {
            // Replacing the contents of the existing texture rather than making a new one: a new
            // handle per frame would leave the old one to be freed on the next pass and, at sixty
            // frames a second of 4K, allocate and free 33 MB every 16 ms.
            Some((texture, generation)) if texture.size() == [frame.width, frame.height] => {
                texture.set(image, egui::TextureOptions::LINEAR);
                *generation = frame.generation;
            }
            _ => {
                let texture = ctx.load_texture("screen", image, egui::TextureOptions::LINEAR);
                self.screen_texture = Some((texture, frame.generation));
            }
        }
    }

    fn apply_user_volumes(&self) {
        let Some(session) = self.voice.as_ref() else { return };
        for state in self.state.voice.values() {
            session.set_user_volume(state.user, self.settings.volume_for(state.user));
        }
    }

    /// Read the engine's state into the meter, and announce a change in whether we are talking.
    fn poll_voice(&mut self, ctx: &egui::Context) {
        let Some(session) = self.voice.as_ref() else {
            self.meter = Meter::default();
            return;
        };

        // Push to talk, read from egui's own key state. Only while the window has focus, which is a
        // real limitation and is said so in the settings screen: a global hotkey needs an
        // accessibility permission this app does not ask for.
        if self.settings.voice.push_to_talk {
            let key = egui::Key::from_name(&self.settings.voice.push_to_talk_key);
            let held = key.is_some_and(|key| ctx.input(|i| i.key_down(key)));
            session.set_talk_key(held);
        }

        let status = session.status();
        self.meter = Meter {
            input_level: status.input_level,
            gate_open: status.gate_open,
            speaking: status.speaking,
            threshold: status.threshold,
            media_ok: status.media_ok,
        };

        // One frame per talk spurt, not per audio frame: the control plane carries state changes.
        if status.speaking != self.announced_speaking {
            self.announced_speaking = status.speaking;
            self.net.send_msg(ClientMsg::Speaking { speaking: status.speaking });
        }
    }

    /// Record the microphone and output state, and tell the server if it is listening.
    fn set_voice_flags(&mut self, muted: bool, deafened: bool) {
        self.settings.voice.muted = muted;
        self.settings.voice.deafened = deafened;
        self.settings.save_quietly();
        // The engine first, then the server. In that order because the engine is what actually stops
        // the microphone, and a mute that is announced before it takes effect is a mute that leaked
        // a frame of audio.
        if let Some(session) = self.voice.as_ref() {
            session.set_muted(muted);
            session.set_deafened(deafened);
        }
        if self.state.my_channel().is_some() {
            self.net.send_msg(ClientMsg::UpdateVoiceState { muted, deafened });
        }
    }

    /// Ask for a channel's first page of history, once.
    fn ensure_history(&mut self, channel: Id) {
        let log = self.state.log_mut(channel);
        if log.visited {
            return;
        }
        log.visited = true;
        log.loading = true;
        self.net.send_msg(ClientMsg::History { channel, before: None, limit: 50 });
    }

    fn apply(&mut self, action: Action) {
        match action {
            Action::Go(view) => self.view = view,

            Action::Probe => {
                let base = crate::settings::Settings {
                    server_url: self.connect.url.clone(),
                    ..Default::default()
                }
                .base_url();
                if base.is_empty() {
                    self.connect.error = Some("type a server address first".into());
                    return;
                }
                self.connect.busy = Some("looking up the server…");
                self.connect.error = None;
                self.net.send(Command::Probe { base });
            }

            Action::LogIn | Action::Register => {
                let base = crate::settings::Settings {
                    server_url: self.connect.url.clone(),
                    ..Default::default()
                }
                .base_url();
                self.connect.busy = Some("signing in…");
                self.connect.error = None;
                let name = self.connect.name.trim().to_string();
                let password = self.connect.password.clone();
                self.net.send(match action {
                    Action::Register => Command::Register {
                        base,
                        name,
                        password,
                        display_name: self.connect.display_name.trim().to_string(),
                    },
                    _ => Command::LogIn { base, name, password },
                });
            }

            Action::LogOut => {
                self.settings.token = None;
                self.settings.save_quietly();
                self.net.send(Command::Disconnect);
                self.state.reset();
                self.view = View::Connect;
                self.connect.probe = None;
                self.connect.busy = None;
            }

            Action::OpenChannel(channel) => {
                self.view = View::Channel(channel);
                self.settings.last_channel = Some(channel.0);
                self.settings.save_quietly();
                self.ensure_history(channel);
            }

            Action::LoadOlder(channel) => {
                let before = self.state.log(channel).and_then(|log| log.messages.first()).map(|m| m.id);
                let log = self.state.log_mut(channel);
                if log.loading || log.complete {
                    return;
                }
                log.loading = true;
                self.net.send_msg(ClientMsg::History { channel, before, limit: 50 });
            }

            Action::SendComposer => {
                let View::Channel(channel) = self.view else { return };
                let content = self.composer.trim().to_string();
                if content.is_empty() {
                    return;
                }
                self.composer.clear();
                let nonce = new_nonce();
                self.state.add_pending(Pending {
                    nonce: nonce.clone(),
                    channel,
                    content: content.clone(),
                    attachment_names: vec![],
                    sent_at: Instant::now(),
                });
                self.net.send_msg(ClientMsg::SendMessage {
                    channel,
                    content,
                    nonce,
                    attachments: vec![],
                });
            }

            Action::AttachFiles => {
                let View::Channel(channel) = self.view else { return };
                let Some(paths) = rfd::FileDialog::new().pick_files() else { return };
                let limit = self
                    .state
                    .server
                    .as_ref()
                    .map(|s| s.max_upload_bytes)
                    .unwrap_or(64 * 1024 * 1024);

                let mut files = Vec::new();
                let mut names = Vec::new();
                for path in paths {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "attachment".into());
                    match std::fs::read(&path) {
                        Ok(bytes) if bytes.len() as u64 > limit => {
                            // Refused here rather than after uploading 200 MB to be told no. The
                            // suggestion is the point: the direct transfer has no limit at all,
                            // because the server never touches the bytes.
                            self.notify(Notice::error(format!(
                                "{name} is {} — this server takes {} in a message. Send it directly instead.",
                                widgets::bytes(bytes.len() as u64),
                                widgets::bytes(limit)
                            )));
                        }
                        Ok(bytes) => {
                            names.push(name.clone());
                            files.push((name, bytes));
                        }
                        Err(err) => self.notify(Notice::error(format!("{name}: {err}"))),
                    }
                }
                if files.is_empty() {
                    return;
                }

                let content = self.composer.trim().to_string();
                self.composer.clear();
                let nonce = new_nonce();
                self.state.add_pending(Pending {
                    nonce: nonce.clone(),
                    channel,
                    content: content.clone(),
                    attachment_names: names,
                    sent_at: Instant::now(),
                });
                self.net.send(Command::SendWithFiles { channel, content, nonce, files });
            }

            Action::Typing(channel) => {
                // Throttled here as well as on the server, so a fast typist does not put a frame
                // on the wire per keystroke for the server to then discard.
                let fresh =
                    self.last_typing.is_none_or(|at| at.elapsed() > Duration::from_secs(3));
                if fresh {
                    self.last_typing = Some(Instant::now());
                    self.net.send_msg(ClientMsg::Typing { channel });
                }
            }

            Action::WantAttachment(attachment) => {
                if self.requested.insert(attachment.id) {
                    self.net.send(Command::WantAttachment(attachment));
                }
            }

            Action::OpenAttachment(attachment) => match open_attachment(&attachment) {
                Ok(()) => {}
                Err(err) => self.notify(Notice::error(format!("{}: {err}", attachment.name))),
            },

            Action::RevealAttachments => {
                let dir = crate::paths::attachment_dir();
                let _ = std::fs::create_dir_all(&dir);
                if let Err(err) = reveal(&dir) {
                    self.notify(Notice::error(format!("{err}")));
                }
            }

            Action::JoinVoice(channel) => {
                self.view = View::Channel(channel);
                self.ensure_history(channel);
                self.net.send_msg(ClientMsg::JoinVoice { channel });
                // The server starts everybody unmuted; whoever was muted before joining meant it.
                self.net.send_msg(ClientMsg::UpdateVoiceState {
                    muted: self.settings.voice.muted,
                    deafened: self.settings.voice.deafened,
                });
            }

            Action::LeaveVoice => {
                // Locally first: the devices should stop the moment the button is pressed, not when
                // the server's acknowledgement comes back over a connection that might be slow.
                self.share = None;
                self.preview = None;
                self.watcher = None;
                self.screen_texture = None;
                self.voice = None;
                self.meter = Meter::default();
                self.net.send_msg(ClientMsg::LeaveVoice);
            }

            // Muting is a property of the person, not of a call: somebody who joins muted meant to
            // join muted, and a mute button that only works once you are already in a conversation
            // is a button that is missing exactly when it is wanted. So both toggles work offline,
            // change the saved setting, and are only *sent* when there is a session to send them
            // to — `JoinVoice` re-sends them on arrival.
            Action::ToggleMute => {
                // Deafened implies muted, so the mute button's job while deafened is to undo both —
                // otherwise it appears to do nothing.
                let (muted, deafened) = if self.settings.voice.deafened {
                    (false, false)
                } else {
                    (!self.settings.voice.muted, false)
                };
                self.set_voice_flags(muted, deafened);
            }

            Action::ToggleDeafen => {
                let deafened = !self.settings.voice.deafened;
                // Turning the sound off mutes as well; turning it back on restores the microphone
                // state that was saved, rather than unmuting somebody who was muted before.
                let muted = if deafened { true } else { self.settings.voice.muted };
                self.set_voice_flags(muted, deafened);
            }

            Action::StartScreen => {
                if self.state.my_channel().is_none() {
                    self.notify(Notice::error("join a voice channel first"));
                    return;
                }
                if !crate::screen::ffmpeg_available() {
                    // Said before anything is announced, rather than after a share that sends
                    // nothing — and it names the places that were searched, because the commonest
                    // cause is not a missing ffmpeg but an app that cannot see the one installed.
                    self.notify(Notice::error(crate::screen::ffmpeg::advice()));
                    return;
                }
                // The app asks for the permission itself rather than leaving it to whatever ffmpeg
                // triggers — and it has to happen *before* anything is announced, because macOS will
                // not grant it to a process that is already running.
                match crate::platform::request_screen_access() {
                    crate::platform::ScreenAccess::Granted
                    | crate::platform::ScreenAccess::Unknown => {}
                    crate::platform::ScreenAccess::AskedForIt => {
                        self.notify(Notice::info(
                            "allow BoaVoice under Screen & System Audio Recording, then restart it — \
                             macOS only grants this to a process that starts afterwards",
                        ));
                        return;
                    }
                }

                // What to share is a choice, and it is asked even when there is one answer. Starting
                // silently on the only screen is friendlier by one click and worse in every other
                // way: nobody can see *what* is about to be shared, which for a screen is the one
                // thing worth being sure of before it goes out.
                let found = crate::screen::sources();
                if found.is_empty() {
                    self.notify(Notice::error("nothing to share was found"));
                    return;
                }
                self.picking = Some(found);
            }

            Action::ShareSource(source) => {
                self.picking = None;
                self.pending_source = Some(source);
                let screen = self.settings.screen;
                // The announced size is a ceiling, not a promise: the picture keeps the source's own
                // resolution and the far side reads the real dimensions out of the stream itself. The
                // 16:9 height is only what the box is shaped like.
                self.net.send_msg(ClientMsg::StartScreen(boa_proto::control::ScreenRequest {
                    width: screen.max_dimension,
                    height: screen.max_dimension * 9 / 16,
                    fps: screen.fps,
                    kbps: screen.kbps,
                    with_audio: screen.with_audio,
                }));
            }

            Action::CancelSharePicker => self.picking = None,

            Action::StopScreen => {
                // Locally first: ffmpeg should stop, and the platform's recording indicator go out,
                // the moment the button is pressed.
                self.share = None;
                self.preview = None;
                self.net.send_msg(ClientMsg::StopScreen);
            }

            Action::Watch(user) => {
                let Some(share) = self.state.voice.get(&user).and_then(|state| state.screen) else {
                    self.notify(Notice::error("they are not sharing a screen"));
                    return;
                };
                let Some(session) = self.voice.as_ref() else {
                    self.notify(Notice::error("join the call first"));
                    return;
                };
                // The decoder is started before the subscription is sent, so the first keyframe the
                // relay forwards has somewhere to go.
                let watcher = session.watch(share.ssrc);
                self.screen_texture = None;
                self.watcher = Some((user, watcher));
                self.net.send_msg(ClientMsg::WatchScreen { user });
                self.view = View::Watching(user);
            }

            Action::Unwatch(_) => {
                self.stop_watching();
                self.view = self.state.my_channel().map(View::Channel).unwrap_or(View::Connect);
            }

            Action::CreateChannel(name, kind) => {
                self.net.send_msg(ClientMsg::CreateChannel { name, kind })
            }

            Action::SettingsChanged => {
                self.settings.save_quietly();
                if let Some(session) = self.voice.as_ref() {
                    // Levels, the gate and the buffer apply live. A changed *device* does not: that
                    // needs the stream rebuilt, which means rejoining, and doing it silently
                    // mid-sentence would be worse than waiting for the next call.
                    session.apply(&self.settings.voice);
                }
                self.apply_user_volumes();
            }

            Action::SendFileDirect(to) => {
                let Some(path) = rfd::FileDialog::new().pick_file() else { return };
                let relays = self.relays();
                let channel = match self.view {
                    View::Channel(channel) => channel,
                    _ => self.state.first_text_channel().unwrap_or(Id::NONE),
                };
                let id = self.transfers.next_id();
                self.notify(Notice::info(format!(
                    "opening a wormhole to {} — they will be asked to accept",
                    self.state.label(to)
                )));
                self.transfers.send(
                    crate::transfer::Command::Send { id, to, channel, path },
                    &relays,
                );
            }

            Action::AcceptOffer(from, code) => {
                let Some((_, offer)) =
                    self.state.offers.iter().find(|(who, o)| *who == from && o.code == code).cloned()
                else {
                    return;
                };
                let relays = self.relays();
                let id = self.transfers.next_id();
                self.active_transfers.push(transfers::Active::new(
                    id,
                    from,
                    offer.name.clone(),
                    false,
                    offer.size,
                ));
                self.transfers.send(
                    crate::transfer::Command::Receive {
                        id,
                        from,
                        code: offer.code.clone(),
                        name: offer.name.clone(),
                    },
                    &relays,
                );
                // Taken off the list now: the offer has been acted on, and a second accept would
                // open a second wormhole with a code that is already claimed.
                self.state.offers.retain(|(who, o)| !(*who == from && o.code == code));
            }

            Action::DeclineOffer(from, code) => {
                self.state.offers.retain(|(who, o)| !(*who == from && o.code == code));
                // The sender is told, so their wormhole stops waiting rather than timing out.
                self.net.send_msg(ClientMsg::CancelFileOffer { to: from, code });
            }

            Action::CancelTransfer(id) => {
                self.transfers.cancel(id);
                self.take_transfer(id);
            }

            Action::Notify(notice) => self.notify(notice),
        }
    }
}

impl eframe::App for App {
    /// Fully transparent, and that is not the same as "the window tint".
    ///
    /// The tint is painted by each panel's frame. Painting it *here as well* stacks two layers of
    /// the same 70%-opaque colour, which comes out at 93% and hides the platform's blur behind a
    /// wall that merely looks like a dark theme. This is the surface underneath all of it, and it
    /// has to let the compositor through.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;

        if self.vibrancy_pending {
            // Only once there is a window. Doing it in `new` is too early: winit has not made its
            // view the content view yet, and the check inside would roll the whole thing back.
            crate::platform::install_vibrancy(frame);
            self.vibrancy_pending = false;
        }

        self.pump_network();
        self.pump_transfers();
        self.poll_voice(ctx);
        self.poll_screen(ctx);
        self.check_share();
        self.images.collect(ctx);
        self.state.expire();
        self.notices.retain(|notice| !notice.expired());

        let mut actions: Vec<Action> = Vec::new();

        egui::Panel::top("title")
            .exact_size(38.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::NONE.fill(theme::WINDOW))
            .show(ui, |ui| {
                title_bar(ui, &self.state, &self.status, &self.view, self.logo.as_ref());
            });

        // Notices go in their own strip, above the voice bar and below everything else. They used to
        // be drawn at the end of the content area — which in the channel view is *after* the log and
        // the composer have taken the whole height, so they were laid out past the bottom edge and
        // never seen. A failed action that explains itself off-screen is indistinguishable from an
        // action that did nothing, which is exactly how this was found.
        if !self.notices.is_empty() {
            let lines = self.notices.len().min(4) as f32;
            egui::Panel::bottom("notices")
                .exact_size(lines * 17.0 + 8.0)
                .resizable(false)
                .show_separator_line(false)
                .frame(egui::Frame::NONE.inner_margin(egui::Margin::symmetric(14, 2)))
                .show(ui, |ui| {
                    for notice in self.notices.iter().rev() {
                        ui.label(
                            egui::RichText::new(&notice.text).size(11.0).color(notice.colour()),
                        );
                    }
                });
        }

        let connected = self.state.me.is_some();

        if connected {
            egui::Panel::bottom("voice")
                .exact_size(voice_bar::HEIGHT)
                .resizable(false)
                .show_separator_line(false)
                .frame(egui::Frame::NONE.inner_margin(egui::Margin::symmetric(10, 6)))
                .show(ui, |ui| {
                    // The glass sheet covers the margin too, or the panel would appear to float
                    // inside a transparent frame of its own.
                    glass::panel(ui, ui.max_rect().expand2(egui::vec2(10.0, 6.0)));
                    actions.extend(voice_bar::show(
                        ui,
                        &self.state,
                        &self.status,
                        &self.meter,
                        &self.settings.voice,
                    ));
                });

            egui::Panel::left("sidebar")
                .exact_size(sidebar::WIDTH)
                .resizable(false)
                .show_separator_line(false)
                .frame(egui::Frame::NONE.inner_margin(egui::Margin::symmetric(10, 8)))
                .show(ui, |ui| {
                    glass::panel(ui, ui.max_rect().expand2(egui::vec2(10.0, 8.0)));
                    let open = match &self.view {
                        View::Channel(id) => Some(*id),
                        _ => None,
                    };
                    actions.extend(sidebar::show(ui, &self.state, open, &mut self.new_channel));
                });
        }

        egui::CentralPanel::no_frame()
            .frame(egui::Frame::NONE.fill(theme::WINDOW).inner_margin(egui::Margin::symmetric(12, 6)))
            .show(ui, |ui| {
                match self.view.clone() {
                    View::Connect => {
                        actions.extend(connect::show(ui, &mut self.connect, self.logo.as_ref()));
                    }
                    View::Settings => {
                        actions.extend(settings_view::show(
                            ui,
                            &mut self.settings,
                            &self.state,
                            &self.devices,
                            &self.meter,
                            self.images.loaded(),
                        ));
                    }
                    View::Watching(user) => {
                        let mine = self.state.me.as_ref().is_some_and(|me| me.id == user);
                        actions.extend(watching(
                            ui,
                            &self.state,
                            user,
                            self.screen_texture.as_ref().map(|(texture, _)| texture),
                            self.watcher.as_ref().map(|(_, watcher)| watcher),
                            mine.then_some(self.share.as_ref()).flatten(),
                        ));
                    }
                    View::Channel(channel) => {
                        // Whatever channel is on screen gets its first page, however it came to be
                        // on screen — clicked, restored from the last session, or picked by `Ready`.
                        // Guarded by `visited`, so this is one request rather than one per frame.
                        self.ensure_history(channel);
                        // The composer sits at the bottom of the content area rather than in its own
                        // panel, so the message log scrolls under nothing.
                        // The transfer strip takes what it needs above the composer, and takes
                        // nothing when there is nothing in flight.
                        let strip = 4.0 + 44.0
                            * (self.state.offers.iter().filter(|(_, o)| o.channel == channel).count()
                                + self.active_transfers.len()) as f32;
                        let composer_height = 62.0 + strip;
                        let log_height = (ui.available_height() - composer_height).max(80.0);
                        let mut view = chat::View {
                            state: &self.state,
                            images: &mut self.images,
                            channel,
                            now: boa_proto::now_millis(),
                        };
                        ui.allocate_ui(egui::vec2(ui.available_width(), log_height), |ui| {
                            actions.extend(chat::log(ui, &mut view));
                        });
                        actions.extend(transfers::show(
                            ui,
                            &self.state,
                            &self.active_transfers,
                            channel,
                        ));
                        let can_send = matches!(self.status, Status::Connected);
                        actions.extend(chat::composer(
                            ui,
                            &self.state,
                            channel,
                            &mut self.composer,
                            can_send,
                        ));
                    }
                }

            });

        if let Some(offered) = self.picking.clone() {
            actions.extend(share_picker(ctx, &offered));
        }

        for action in actions {
            self.apply(action);
        }

        // Paced repaints, and only when something is actually moving: a call needs the level meter
        // and the speaking rings, and a decode in flight needs the frame that draws it. Idle, the
        // window costs nothing.
        let busy = !self.active_transfers.is_empty()
            || self.watcher.is_some()
            || self.share.is_some()
            || self.state.my_channel().is_some()
            || !self.notices.is_empty()
            || !self.state.pending.is_empty()
            || matches!(self.status, Status::Connecting | Status::Reconnecting { .. });
        if busy {
            ctx.request_repaint_after(Duration::from_millis(80));
        }
    }
}

/// The window's own chrome: the mark, where we are, and how the connection is.
fn title_bar(
    ui: &mut egui::Ui,
    state: &State,
    status: &Status,
    view: &View,
    logo: Option<&egui::TextureHandle>,
) {
    let bar = ui.max_rect();
    // Room for the window controls the platform draws over our content. Only on macOS, where the
    // title bar is hidden and the traffic lights float above the app's own drawing.
    let left = bar.left() + if cfg!(target_os = "macos") { 78.0 } else { 12.0 };

    let mark = egui::Rect::from_center_size(egui::pos2(left + 10.0, bar.center().y), egui::Vec2::splat(20.0));
    match logo {
        Some(logo) => ui.painter().image(logo.id(), mark, FULL_TEXTURE, egui::Color32::WHITE),
        // The mark is decoration; a build whose icon would not decode still gets a title bar.
        None => ui.painter().rect_filled(mark, theme::R_CHIP, theme::ACCENT),
    };

    let name = state.server.as_ref().map(|s| s.name.as_str()).unwrap_or("BoaVoice");
    ui.painter().text(
        egui::pos2(left + 26.0, bar.center().y),
        egui::Align2::LEFT_CENTER,
        name,
        egui::FontId::proportional(13.0),
        theme::TEXT,
    );

    if let View::Channel(channel) = view {
        if let Some(channel) = state.channel(*channel) {
            ui.painter().text(
                bar.center(),
                egui::Align2::CENTER_CENTER,
                format!("#{}", channel.name),
                egui::FontId::proportional(12.5),
                theme::TEXT_DIM,
            );
        }
    }

    // A dot rather than words: the bar is 38 points tall and the voice bar already carries the
    // sentence.
    let (colour, tooltip) = match status {
        Status::Connected => (theme::OK, "connected"),
        Status::Connecting => (theme::WARN, "connecting"),
        Status::Reconnecting { .. } => (theme::WARN, "reconnecting"),
        Status::Offline => (theme::TEXT_FAINT, "offline"),
        Status::Rejected(_) => (theme::ERROR, "signed out"),
    };
    let dot = egui::pos2(bar.right() - 14.0, bar.center().y);
    ui.painter().circle_filled(dot, 4.0, colour);
    // A hover target over the dot rather than a widget: the bar is painted absolutely, so
    // there is no layout slot to hang a tooltip off.
    ui.interact(
        egui::Rect::from_center_size(dot, egui::Vec2::splat(18.0)),
        ui.id().with("connection-dot"),
        egui::Sense::hover(),
    )
    .on_hover_text(tooltip);
}

/// The screen-watching view.
fn watching(
    ui: &mut egui::Ui,
    state: &State,
    user: Id,
    texture: Option<&egui::TextureHandle>,
    watcher: Option<&crate::screen::Watcher>,
    own_share: Option<&crate::screen::Share>,
) -> Option<Action> {
    use std::sync::atomic::Ordering;

    let mut action = None;
    let label = state.label(user);

    // Your own share, opened from the sidebar. It can never show a picture, and the reason is a
    // design decision rather than a fault: the relay does not send a stream back to the person who
    // sent it — otherwise everybody would see and hear themselves. So this shows what the *sender*
    // is doing instead, which is what somebody testing alone actually needs to know.
    if let Some(share) = own_share {
        let frames = share.frames.load(Ordering::Relaxed);
        let packets = share.packets.load(Ordering::Relaxed);
        let audio = share.audio_packets();

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Your own screen").size(14.0).color(theme::TEXT));
            ui.label(
                egui::RichText::new(format!(
                    "{frames} frames · {packets} packets{}",
                    if audio > 0 { format!(" · {audio} of sound") } else { String::new() }
                ))
                .size(10.5)
                .color(if frames == 0 { theme::WARN } else { theme::TEXT_FAINT }),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::icon_button(ui, icons::close, 26.0, "Close").clicked() {
                    action = Some(Action::Unwatch(user));
                }
            });
        });

        let rect = ui.available_rect_before_wrap();
        // The preview: decoded from what the encoder produced, so it is what the others receive rather
        // than a second look at the desktop. A share pointed at the wrong window looks wrong *here*.
        if let Some(texture) = texture {
            let size = texture.size_vec2();
            let scale = (rect.width() / size.x).min(rect.height() / size.y);
            let drawn = egui::Rect::from_center_size(rect.center(), size * scale);
            ui.painter().rect_filled(rect, theme::R_PANEL, theme::mocha::CRUST);
            ui.painter().image(texture.id(), drawn, FULL_TEXTURE, egui::Color32::WHITE);
            return action;
        }

        glass::well(ui, rect, theme::R_PANEL);
        let centre = rect.center();
        let line = |offset: f32, text: String, colour: egui::Color32, size: f32| {
            ui.painter().text(
                egui::pos2(centre.x, centre.y + offset),
                egui::Align2::CENTER_CENTER,
                text,
                egui::FontId::proportional(size),
                colour,
            );
        };
        let (text, colour) = if frames == 0 {
            ("nothing captured yet — waiting for the first frame".to_string(), theme::WARN)
        } else {
            ("decoding your own stream…".to_string(), theme::TEXT_FAINT)
        };
        line(-8.0, text, colour, 12.0);
        line(
            12.0,
            "This is decoded from what is going out, not a second look at the desktop.".to_string(),
            theme::TEXT_FAINT,
            10.5,
        );
        if let Some(device) = share.audio_device() {
            line(36.0, format!("sound from {device}"), theme::TEXT_FAINT, 10.5);
        } else if let Some(problem) = &share.audio_problem {
            line(36.0, format!("no sound: {problem}"), theme::TEXT_FAINT, 10.5);
        }
        // Which engine, because it is the difference between "nothing to install" and "ffmpeg is
        // capturing this", and because it is the first thing worth knowing when a share looks wrong.
        line(
            56.0,
            format!(
                "{} · {}×{}",
                if share.native() { "ScreenCaptureKit" } else { "ffmpeg" },
                share.width,
                share.height
            ),
            theme::TEXT_FAINT,
            10.5,
        );
        if let Some(trouble) = share.trouble() {
            line(76.0, trouble, theme::WARN, 10.5);
        }
        return action;
    }

    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(format!("{label}'s screen")).size(14.0).color(theme::TEXT));
        if let Some(watcher) = watcher {
            let frames = watcher.frames.load(Ordering::Relaxed);
            let dropped = watcher.dropped.load(Ordering::Relaxed);
            // Whether the share *claims* sound is worth showing next to the frame count: a share
            // with no sound and a share whose sound is not arriving look identical otherwise.
            let sound = match state.voice.get(&user).and_then(|s| s.screen) {
                Some(share) if share.with_audio => " · with sound",
                Some(_) => " · no sound",
                None => "",
            };
            ui.label(
                egui::RichText::new(format!("{frames} frames, {dropped} dropped{sound}"))
                    .size(10.5)
                    .color(theme::TEXT_FAINT),
            );
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if widgets::icon_button(ui, icons::close, 26.0, "Stop watching").clicked() {
                action = Some(Action::Unwatch(user));
            }
        });
    });

    let rect = ui.available_rect_before_wrap();
    match texture {
        Some(texture) => {
            // Fitted, letterboxed, and centred. Filling the panel instead would either stretch the
            // picture or crop it, and somebody is watching this to read what is on it.
            let size = texture.size_vec2();
            let scale = (rect.width() / size.x).min(rect.height() / size.y);
            let drawn = egui::Rect::from_center_size(rect.center(), size * scale);
            // The letterbox is painted rather than left transparent: a screen share showing the
            // desktop through the gaps is confusing about which pixels belong to whom.
            ui.painter().rect_filled(rect, theme::R_PANEL, theme::mocha::CRUST);
            ui.painter().image(texture.id(), drawn, FULL_TEXTURE, egui::Color32::WHITE);
        }
        None => {
            glass::well(ui, rect, theme::R_PANEL);
            let share = state.voice.get(&user).and_then(|state| state.screen);
            let arrived = watcher.map(|w| w.frames.load(Ordering::Relaxed)).unwrap_or(0);
            let stalled = watcher.is_some_and(|w| w.since.elapsed() > Duration::from_secs(6));
            let waiting = watcher.is_some_and(|w| !w.started.load(Ordering::Relaxed));
            let text = match (share, waiting, arrived == 0 && stalled) {
                // "Nothing yet" has three quite different causes and only one of them is worth
                // waiting for, so each gets its own sentence — and after six seconds of nothing the
                // honest answer is that the packets are not arriving, not that a keyframe is due.
                (Some(_), _, true) => format!(
                    "no video is arriving after six seconds — UDP {} is not getting through, on \
                     their side or on yours",
                    state.server.as_ref().map(|s| s.media_port).unwrap_or(0)
                ),
                (Some(share), true, false) => format!(
                    "{}×{} at {} fps — waiting for a keyframe (up to two seconds)",
                    share.width, share.height, share.fps
                ),
                (Some(_), false, _) => "decoding…".to_string(),
                (None, _, _) => format!("{label} is not sharing a screen"),
            };
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                text,
                egui::FontId::proportional(12.0),
                theme::TEXT_FAINT,
            );
        }
    }

    action
}

/// Ask what to share.
///
/// A floating window rather than a settings page, because this is a question asked at the moment of
/// pressing the button and answered once — the answer is not a preference worth keeping. Screens come
/// first, then windows, which is the order people look for them in.
fn share_picker(ctx: &egui::Context, offered: &[crate::screen::Source]) -> Option<Action> {
    let mut action = None;
    let mut open = true;
    egui::Window::new("Share a screen")
        .collapsible(false)
        .resizable(false)
        .open(&mut open)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.set_min_width(280.0);
            ui.label(
                egui::RichText::new("Everyone in the call can ask to watch it.")
                    .size(10.5)
                    .color(theme::TEXT_FAINT),
            );
            ui.add_space(6.0);

            let (screens, windows): (Vec<_>, Vec<_>) =
                offered.iter().partition(|source| !source.window);

            for source in &screens {
                if widgets::pill_button(ui, &source.label, true).clicked() {
                    action = Some(Action::ShareSource((*source).clone()));
                }
                ui.add_space(2.0);
            }
            if !windows.is_empty() {
                ui.add_space(6.0);
                widgets::section(ui, "Windows");
                egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                    for source in &windows {
                        if widgets::pill_button(ui, &source.label, false).clicked() {
                            action = Some(Action::ShareSource((*source).clone()));
                        }
                        ui.add_space(2.0);
                    }
                });
            } else if cfg!(target_os = "macos") {
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(
                        "A single window is not offered on macOS yet: ffmpeg can capture a display \
                         and nothing smaller, and one window needs ScreenCaptureKit.",
                    )
                    .size(10.0)
                    .color(theme::TEXT_FAINT),
                );
            }
        });
    if !open {
        return Some(Action::CancelSharePicker);
    }
    action
}

/// The whole of a texture, for `Painter::image`.
pub const FULL_TEXTURE: egui::Rect =
    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));

/// Decode the embedded icon into a texture.
///
/// The same bytes `main.rs` hands to the window manager. Decoded once at startup on the interface
/// thread, which is 512×512 of PNG — a millisecond, before the first frame.
fn load_logo(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    const LOGO: &[u8] = include_bytes!("../../../../packaging/icon-512.png");
    let image = image::load_from_memory(LOGO)
        .inspect_err(|err| log::warn!("logo: {err}"))
        .ok()?;
    let rgba = image.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Some(ctx.load_texture(
        "boa-logo",
        egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()),
        egui::TextureOptions::LINEAR,
    ))
}

/// The host part of an `http(s)://host[:port]` base URL.
fn host_of(base: &str) -> Option<String> {
    let rest = base.strip_prefix("http://").or_else(|| base.strip_prefix("https://"))?;
    let authority = rest.split('/').next()?;
    // An IPv6 literal is bracketed, and its colons are not a port separator.
    if let Some(end) = authority.strip_prefix('[').and_then(|rest| rest.find(']').map(|i| i + 1)) {
        return Some(authority[..end + 1].to_string());
    }
    Some(authority.split(':').next()?.to_string())
}

/// Resolve a host and port to one address.
fn resolve(host: &str, port: u16) -> std::io::Result<std::net::SocketAddr> {
    use std::net::ToSocketAddrs as _;
    // The brackets round an IPv6 literal have to come off before it will parse as a host.
    let host = host.trim_start_matches('[').trim_end_matches(']');
    (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::other(format!("{host} resolved to nothing")))
}

/// A fresh message nonce.
///
/// Random rather than a counter, because a counter restarts at zero when the app does — and the
/// server's duplicate guard is `(author, nonce)`, so a restarted counter would make the first
/// message of a new session collide with the first of the last one.
fn new_nonce() -> String {
    use rand::Rng as _;
    let value: u128 = rand::rng().random();
    format!("{value:032x}")
}

/// Copy an attachment out of the content-addressed store under its real name, and open it.
///
/// The copy is the point: the store names files by hash and with no extension, which every
/// operating system needs in order to know what to open a file with.
fn open_attachment(attachment: &Attachment) -> anyhow::Result<()> {
    let bytes = crate::cache::read(&attachment.sha256)?;
    let name = attachment.name.replace(['/', '\\'], "_");
    let path = std::env::temp_dir().join(format!("boavoice-{}-{}", &attachment.sha256[..8], name));
    std::fs::write(&path, bytes)?;
    reveal_file(&path)
}

fn reveal(dir: &std::path::Path) -> anyhow::Result<()> {
    reveal_file(dir)
}

/// Hand a path to the desktop.
fn reveal_file(path: &std::path::Path) -> anyhow::Result<()> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    std::process::Command::new(program)
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|err| anyhow::anyhow!("could not open {}: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonces_are_random_so_a_restart_cannot_collide_with_the_last_session() {
        let a = new_nonce();
        let b = new_nonce();
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn the_relays_host_comes_from_the_url_the_control_plane_used() {
        assert_eq!(host_of("http://boa.example.com:8787").as_deref(), Some("boa.example.com"));
        assert_eq!(host_of("https://boa.example.com").as_deref(), Some("boa.example.com"));
        assert_eq!(host_of("http://192.168.1.10:8787").as_deref(), Some("192.168.1.10"));
        // An IPv6 literal's colons are not a port separator.
        assert_eq!(host_of("http://[fd00::1]:8787").as_deref(), Some("[fd00::1]"));
        assert_eq!(host_of("boa.example.com"), None, "a URL without a scheme is not one");
    }

    #[test]
    fn resolving_strips_the_brackets_from_an_ipv6_literal() {
        assert_eq!(resolve("127.0.0.1", 1).unwrap().port(), 1);
        assert!(resolve("[::1]", 2).is_ok(), "brackets must not reach the resolver");
        assert!(resolve("not a host at all", 3).is_err());
    }

    #[test]
    fn errors_stay_up_longer_than_successes() {
        let mut error = Notice::error("something broke");
        let mut ok = Notice::success("done");
        // Age both by eight seconds: the success has had its time, the error has not.
        error.at = Instant::now() - Duration::from_secs(8);
        ok.at = Instant::now() - Duration::from_secs(8);
        assert!(!error.expired(), "an error is telling you something you did not know");
        assert!(ok.expired());
    }
}
