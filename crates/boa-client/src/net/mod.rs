//! The network thread, and the two channels the interface talks to it through.
//!
//! egui redraws on the main thread and must never wait. A tokio runtime lives on its own
//! thread and owns everything that can block: the WebSocket, the HTTP client, the
//! attachment store. Between them are two queues — [`Command`] going down and [`Event`]
//! coming up — and *no shared state at all*. That is the whole design, and it buys three
//! things worth the indirection:
//!
//! * The window cannot be blocked by the network, because there is nothing to block on.
//! * Every state change arrives as an event and is applied in one place, so "why does the
//!   sidebar say that" has one answer to look at rather than a lock to reason about.
//! * The reconnect logic can be a plain loop, because nothing else is reading its state
//!   while it runs.
//!
//! The event queue is a `std::sync::mpsc`, drained once per frame. Each push wakes the
//! window through [`egui::Context::request_repaint`] — without that a message would sit in
//! the queue until the user happened to move the mouse, which on an idle chat window is
//! exactly when messages arrive.

pub mod api;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use boa_proto::{Attachment, ClientMsg, Id, ServerMsg};
use futures_util::{SinkExt as _, StreamExt as _};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use api::{ServerProbe, Session};

/// What the interface asks the network to do.
#[derive(Debug)]
pub enum Command {
    /// Ask a server who it is, before committing to it.
    Probe { base: String },
    Register { base: String, name: String, password: String, display_name: String },
    LogIn { base: String, name: String, password: String },
    /// Open the control connection with a token we already have.
    Connect { base: String, token: String },
    /// Close it and stay closed. Distinct from a dropped connection, which reconnects.
    Disconnect,
    /// Anything on the control plane.
    Send(ClientMsg),
    /// Upload a file, then send a message referencing it.
    ///
    /// One command rather than two, because the intermediate state — an upload that
    /// succeeded and a message that was never sent — is one the interface would otherwise
    /// have to model and nobody would ever test.
    SendWithFiles { channel: Id, content: String, nonce: String, files: Vec<(String, Vec<u8>)> },
    /// Make sure an attachment's bytes are in the local store, downloading if needed.
    WantAttachment(Attachment),
}

/// What the network tells the interface.
#[derive(Debug)]
pub enum Event {
    /// The connection's state changed.
    Status(Status),
    Probed(Result<ServerProbe, String>),
    /// A login or a registration finished.
    Authenticated(Result<Session, String>),
    /// A frame from the server, verbatim.
    Server(ServerMsg),
    /// An attachment's bytes are now in the local store.
    AttachmentReady { attachment: Id, sha256: String },
    /// An attachment is not available and never will be: the server's three days are up
    /// and this machine never had a copy.
    AttachmentGone { attachment: Id },
    /// Something failed in a way worth a line in the status area.
    Trouble(String),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Status {
    Offline,
    Connecting,
    Connected,
    /// Dropped, and trying again in this many seconds.
    ///
    /// Carried in the event rather than counted in the UI so the countdown shown is the
    /// real one — a client that says "retrying in 3s" and then waits eight is worse than
    /// one that says nothing.
    Reconnecting { in_secs: u64 },
    /// Refused for a reason retrying will not fix: a bad token, a version mismatch. The
    /// interface goes back to the connect screen rather than spinning.
    Rejected(String),
}

/// The interface's handle on the network thread.
pub struct Net {
    commands: UnboundedSender<Command>,
    events: std::sync::mpsc::Receiver<Event>,
}

impl Net {
    /// Start the network thread.
    ///
    /// `ctx` is cloned so the thread can wake the window when an event arrives. Cheap —
    /// an `egui::Context` is an `Arc` — and the only thing the two threads share.
    pub fn spawn(ctx: egui::Context) -> Net {
        let (command_tx, command_rx) = unbounded_channel();
        let (event_tx, event_rx) = std::sync::mpsc::channel();

        // Named, because a panic in here is reported by thread name and "unnamed" would
        // not say which of the app's several threads had died.
        let builder = std::thread::Builder::new().name("boa-net".into());
        let spawned = builder.spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                // Two workers is plenty: one WebSocket, occasional HTTP. The default is
                // one per core, which on a 16-core machine is 16 threads asleep.
                .worker_threads(2)
                .enable_all()
                .thread_name("boa-net-worker")
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    log::error!("net: could not start a runtime: {err}");
                    return;
                }
            };
            runtime.block_on(run(command_rx, Sender { tx: event_tx, ctx }));
        });
        if let Err(err) = &spawned {
            // Nothing else can be done about this, and it is worth being loud: the app
            // will run and never connect to anything.
            log::error!("net: could not start the network thread: {err}");
        }

        Net { commands: command_tx, events: event_rx }
    }

    /// Queue a command. Silently dropped if the network thread has died, which is
    /// already logged where it happened.
    pub fn send(&self, command: Command) {
        if self.commands.send(command).is_err() {
            log::error!("net: the network thread is gone");
        }
    }

    /// Convenience for the common case.
    pub fn send_msg(&self, msg: ClientMsg) {
        self.send(Command::Send(msg));
    }

    /// Everything that has arrived since the last frame.
    pub fn drain(&self) -> Vec<Event> {
        self.events.try_iter().collect()
    }
}

/// The event side, with the repaint wake-up attached so no caller can forget it.
#[derive(Clone)]
struct Sender {
    tx: std::sync::mpsc::Sender<Event>,
    ctx: egui::Context,
}

impl Sender {
    fn send(&self, event: Event) {
        if self.tx.send(event).is_err() {
            return;
        }
        // Without this the event sits in the queue until something else causes a repaint.
        // On an idle chat window that is the difference between a message appearing now
        // and appearing when the mouse next moves.
        self.ctx.request_repaint();
    }

    fn trouble(&self, context: &str, err: anyhow::Error) {
        log::warn!("net: {context}: {err:#}");
        self.send(Event::Trouble(format!("{context}: {err}")));
    }
}

/// The network thread's main loop.
///
/// One live connection at a time, owned by a task with its own outbound queue. Commands
/// that belong to the connection are forwarded to it; commands that do not are handled
/// here. When a connection ends it reports that on a channel and this loop decides whether
/// to try again.
///
/// Two details in here are load-bearing and both were wrong in the obvious version.
/// Connection endings arrive on a *channel* rather than being awaited through the `Link`,
/// because a `select!` branch holding `&mut link` cannot coexist with a branch whose body
/// assigns to it. And the retry moment is an absolute [`tokio::time::Instant`] rather than
/// a fresh `sleep(delay)` per iteration: the loop spins whenever any command arrives, and
/// a relative sleep recreated each time would restart the countdown and could postpone a
/// reconnect indefinitely.
async fn run(mut commands: UnboundedReceiver<Command>, events: Sender) {
    let client = match api::client() {
        Ok(client) => Arc::new(client),
        Err(err) => {
            events.trouble("HTTP", err);
            return;
        }
    };

    let (ends_tx, mut ends) = unbounded_channel::<(u64, End)>();

    // Set while a connection is up: where to send frames.
    let mut link: Option<Link> = None;
    // Where to reconnect to, and how many times in a row it has failed.
    let mut target: Option<(String, String)> = None;
    let mut attempt: u32 = 0;
    // Distinguishes a report from the current connection from one from a connection that
    // has already been replaced — which happens whenever `Connect` is sent twice quickly.
    let mut generation: u64 = 0;
    let mut retry_at: Option<tokio::time::Instant> = None;

    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { return };
                match command {
                    Command::Probe { base } => {
                        let client = client.clone();
                        let events = events.clone();
                        tokio::spawn(async move {
                            let result = api::probe(&client, &base).await.map_err(|e| format!("{e:#}"));
                            events.send(Event::Probed(result));
                        });
                    }
                    Command::Register { base, name, password, display_name } => {
                        let client = client.clone();
                        let events = events.clone();
                        tokio::spawn(async move {
                            let result = api::register(&client, &base, &name, &password, &display_name)
                                .await
                                .map_err(|e| format!("{e:#}"));
                            events.send(Event::Authenticated(result));
                        });
                    }
                    Command::LogIn { base, name, password } => {
                        let client = client.clone();
                        let events = events.clone();
                        tokio::spawn(async move {
                            let result = api::login(&client, &base, &name, &password)
                                .await
                                .map_err(|e| format!("{e:#}"));
                            events.send(Event::Authenticated(result));
                        });
                    }
                    Command::Connect { base, token } => {
                        target = Some((base.clone(), token.clone()));
                        attempt = 1;
                        retry_at = None;
                        generation += 1;
                        events.send(Event::Status(Status::Connecting));
                        link = Some(open(base, token, events.clone(), ends_tx.clone(), generation));
                    }
                    Command::Disconnect => {
                        target = None;
                        attempt = 0;
                        retry_at = None;
                        generation += 1;
                        // Dropping the link drops its outbound sender, which is how the
                        // connection task learns to stop.
                        link = None;
                        events.send(Event::Status(Status::Offline));
                    }
                    Command::Send(msg) => {
                        if let Some(link) = &link {
                            let _ = link.outbound.send(msg);
                        } else {
                            // Deliberately dropped rather than queued. A message typed
                            // while offline that arrives three minutes later, out of
                            // context, is worse than one that visibly failed to send.
                            log::debug!("net: dropped {msg:?} — not connected");
                            events.send(Event::Trouble("not connected".into()));
                        }
                    }
                    Command::SendWithFiles { channel, content, nonce, files } => {
                        let Some((base, token)) = target.clone() else {
                            events.send(Event::Trouble("not connected".into()));
                            continue;
                        };
                        let Some(outbound) = link.as_ref().map(|l| l.outbound.clone()) else {
                            events.send(Event::Trouble("not connected".into()));
                            continue;
                        };
                        let client = client.clone();
                        let events = events.clone();
                        tokio::spawn(async move {
                            let mut ids = Vec::new();
                            for (name, bytes) in files {
                                match api::upload(&client, &base, &token, &name, bytes).await {
                                    Ok(attachment) => ids.push(attachment.id),
                                    Err(err) => {
                                        events.trouble(&format!("uploading {name}"), err);
                                        // Carry on with the rest: one file that would not
                                        // upload should not silently swallow the message
                                        // and the other three.
                                    }
                                }
                            }
                            if content.trim().is_empty() && ids.is_empty() {
                                return;
                            }
                            let _ = outbound.send(ClientMsg::SendMessage {
                                channel,
                                content,
                                nonce,
                                attachments: ids,
                            });
                        });
                    }
                    Command::WantAttachment(attachment) => {
                        let Some((base, token)) = target.clone() else { continue };
                        let client = client.clone();
                        let events = events.clone();
                        tokio::spawn(async move {
                            fetch_attachment(&client, &base, &token, attachment, events).await;
                        });
                    }
                }
            }

            // A connection ended.
            report = ends.recv() => {
                let Some((from, reason)) = report else { return };
                // A report from a connection we have already replaced says nothing about
                // the current one, and acting on it would schedule a reconnect on top of a
                // live link.
                if link.as_ref().map(|l| l.generation) != Some(from) {
                    log::debug!("net: ignoring {reason:?} from a replaced connection");
                    continue;
                }
                link = None;
                if target.is_none() {
                    continue;
                }
                match reason {
                    End::Rejected(why) => {
                        // A bad token or a version mismatch: retrying achieves nothing
                        // except a login loop nobody can interrupt.
                        target = None;
                        attempt = 0;
                        retry_at = None;
                        events.send(Event::Status(Status::Rejected(why)));
                    }
                    End::Dropped(why) => {
                        attempt += 1;
                        let delay = retry_delay(attempt);
                        log::info!("net: connection ended ({why}); retrying in {delay:?}");
                        retry_at = Some(tokio::time::Instant::now() + delay);
                        events.send(Event::Status(Status::Reconnecting { in_secs: delay.as_secs() }));
                    }
                }
            }

            // Time to try again.
            _ = wait_until(retry_at) => {
                retry_at = None;
                if let Some((base, token)) = target.clone() {
                    generation += 1;
                    events.send(Event::Status(Status::Connecting));
                    link = Some(open(base, token, events.clone(), ends_tx.clone(), generation));
                }
            }
        }
    }
}

/// Sleep until `deadline`, or forever when there is none.
///
/// `select!` polls every branch on every pass, so the "nothing scheduled" case has to be a
/// future that never completes rather than one that returns immediately — the latter would
/// spin the loop at full speed and pin a core.
async fn wait_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// How long to wait before the nth reconnect attempt.
///
/// Exponential, capped at half a minute. The cap matters more than the curve: a laptop
/// that was asleep for an hour should come back within seconds of waking, and an
/// uncapped backoff would have it waiting minutes while showing a window that looks
/// connected.
fn retry_delay(attempt: u32) -> Duration {
    let seconds = match attempt {
        0 | 1 => 1,
        2 => 2,
        3 => 5,
        4 => 10,
        _ => 30,
    };
    Duration::from_secs(seconds)
}

/// A live connection: where to put frames, and which attempt it belongs to.
struct Link {
    outbound: UnboundedSender<ClientMsg>,
    generation: u64,
}

/// Why a connection stopped.
#[derive(Debug)]
enum End {
    /// Something to retry: the network went away, the server restarted.
    Dropped(String),
    /// Something retrying will not fix.
    Rejected(String),
}

/// Start a connection task and hand back its handle.
fn open(
    base: String,
    token: String,
    events: Sender,
    ends: UnboundedSender<(u64, End)>,
    generation: u64,
) -> Link {
    let (outbound_tx, outbound_rx) = unbounded_channel();
    tokio::spawn(async move {
        let end = match connection(&base, &token, outbound_rx, &events).await {
            Ok(end) => end,
            Err(err) => End::Dropped(format!("{err:#}")),
        };
        let _ = ends.send((generation, end));
    });
    Link { outbound: outbound_tx, generation }
}

/// One control connection, from handshake to close.
async fn connection(
    base: &str,
    token: &str,
    mut outbound: UnboundedReceiver<ClientMsg>,
    events: &Sender,
) -> Result<End> {
    let url = ws_url(base);
    log::info!("net: connecting to {url}");
    let (socket, _response) = tokio_tungstenite::connect_async(&url)
        .await
        .with_context(|| format!("connecting to {url}"))?;
    let (mut sink, mut stream) = socket.split();

    // Identify first, before anything else is allowed on the wire.
    let identify = ClientMsg::Identify {
        token: token.to_string(),
        protocol_version: boa_proto::PROTOCOL_VERSION,
        agent: api::agent(),
    };
    sink.send(tungstenite_text(&identify)?).await.context("identifying")?;

    // The server's own keepalive is what eventually notices a connection that has died
    // without a close frame — a laptop lid, a NAT timeout. Ours runs in the other
    // direction for the same reason.
    let mut keepalive = tokio::time::interval(Duration::from_secs(20));
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut ready = false;

    loop {
        tokio::select! {
            frame = stream.next() => {
                let Some(frame) = frame else {
                    return Ok(End::Dropped("the server closed the connection".into()));
                };
                let frame = frame.context("reading a frame")?;
                use tokio_tungstenite::tungstenite::Message;
                match frame {
                    Message::Text(text) => {
                        let msg: ServerMsg = match serde_json::from_str(&text) {
                            Ok(msg) => msg,
                            Err(err) => {
                                // A frame this client cannot parse is a protocol problem,
                                // not a reason to hang up: the rest of the session very
                                // probably still works.
                                log::warn!("net: unparseable frame: {err}");
                                continue;
                            }
                        };
                        // A fatal error is the server saying "do not come back with this",
                        // and is the one case that must not turn into a reconnect loop.
                        if let ServerMsg::Error { message, fatal: true, .. } = &msg {
                            return Ok(End::Rejected(message.clone()));
                        }
                        if !ready && matches!(msg, ServerMsg::Ready { .. }) {
                            ready = true;
                            events.send(Event::Status(Status::Connected));
                        }
                        events.send(Event::Server(msg));
                    }
                    Message::Close(frame) => {
                        let why = frame
                            .map(|f| format!("{}: {}", f.code, f.reason))
                            .unwrap_or_else(|| "closed".into());
                        return Ok(End::Dropped(why));
                    }
                    // tungstenite answers pings itself.
                    Message::Ping(_) | Message::Pong(_) => {}
                    Message::Binary(_) | Message::Frame(_) => {}
                }
            }

            msg = outbound.recv() => {
                let Some(msg) = msg else {
                    // The loop above dropped our sender, which means it is replacing this
                    // connection.
                    return Ok(End::Dropped("replaced".into()));
                };
                sink.send(tungstenite_text(&msg)?).await.context("sending a frame")?;
            }

            _ = keepalive.tick() => {
                // An application ping rather than a WebSocket one, because this is also
                // what keeps a NAT mapping alive in the direction the client cares about,
                // and because the reply is something the UI can time.
                let nonce = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                sink.send(tungstenite_text(&ClientMsg::Ping { nonce })?).await.context("keepalive")?;
            }
        }
    }
}

fn tungstenite_text(msg: &ClientMsg) -> Result<tokio_tungstenite::tungstenite::Message> {
    let text = serde_json::to_string(msg).context("serialising a frame")?;
    Ok(tokio_tungstenite::tungstenite::Message::Text(text.into()))
}

/// Turn an `http(s)://host` base into the WebSocket URL.
fn ws_url(base: &str) -> String {
    let base = base.trim_end_matches('/');
    if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}/ws")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}/ws")
    } else {
        format!("ws://{base}/ws")
    }
}

/// Make sure an attachment is in the local store, then say so.
///
/// The order is the point. The local store is checked *first*, because after three days
/// it is the only copy and asking the server would produce a 410 for a file we have. And
/// a 410 for a file we do *not* have is reported as [`Event::AttachmentGone`] rather than
/// as an error, because it is not one — it is what every attachment does eventually.
async fn fetch_attachment(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    attachment: Attachment,
    events: Sender,
) {
    if crate::cache::have(&attachment.sha256) {
        events.send(Event::AttachmentReady { attachment: attachment.id, sha256: attachment.sha256 });
        return;
    }

    match api::download(client, base, token, attachment.id).await {
        Ok(api::Fetched::Bytes(bytes)) => {
            // Verified against the hash the message carried, not trusted. This is the one
            // place where a wrong file would be silently cached under a name that says it
            // is right, and after the server's copy expires there would be nothing left to
            // compare it against.
            match crate::cache::store(&attachment.sha256, &bytes) {
                Ok(()) => events.send(Event::AttachmentReady {
                    attachment: attachment.id,
                    sha256: attachment.sha256,
                }),
                Err(err) => events.trouble(&format!("saving {}", attachment.name), err),
            }
        }
        Ok(api::Fetched::Expired) => events.send(Event::AttachmentGone { attachment: attachment.id }),
        Err(err) => events.trouble(&format!("downloading {}", attachment.name), err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_websocket_url_keeps_the_schemes_security() {
        assert_eq!(ws_url("http://host:8787"), "ws://host:8787/ws");
        assert_eq!(ws_url("https://host/"), "wss://host/ws");
        assert_eq!(ws_url("host"), "ws://host/ws");
    }

    /// The cap is the part that matters: a laptop that was asleep must come back within
    /// seconds of waking, not minutes.
    #[test]
    fn the_retry_delay_grows_and_then_stops_growing() {
        assert_eq!(retry_delay(1), Duration::from_secs(1));
        assert!(retry_delay(2) > retry_delay(1));
        assert!(retry_delay(4) > retry_delay(3));
        assert_eq!(retry_delay(5), retry_delay(50), "capped");
        assert!(retry_delay(1_000) <= Duration::from_secs(30));
    }

    #[tokio::test]
    async fn a_timer_with_no_deadline_never_fires() {
        // The whole loop depends on this: a branch that returned immediately would spin the
        // select at full speed and pin a core.
        let result = tokio::time::timeout(Duration::from_millis(50), wait_until(None)).await;
        assert!(result.is_err(), "it should still be waiting");

        // And one with a deadline does fire.
        let soon = tokio::time::Instant::now() + Duration::from_millis(10);
        assert!(tokio::time::timeout(Duration::from_secs(1), wait_until(Some(soon))).await.is_ok());
    }
}
