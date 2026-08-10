//! Sending a file straight to somebody, without the server in the middle.
//!
//! This is the answer to the one thing the three-day attachment rule cannot do: a file that is too
//! big to post, or that should not sit on a shared box at all. It goes machine to machine over
//! [magic-wormhole](https://magic-wormhole.readthedocs.io/), and the server's entire involvement is
//! relaying one short string.
//!
//! **Why that is safe.** A wormhole code is `<number>-<word>-<word>`: a mailbox number and a
//! password. The two clients use it to run a PAKE — each proves it knows the password without
//! sending it — and derive a key no observer can compute, including whoever relayed the code. The
//! code is single-use: the first party to claim the mailbox wins, so a relay that decided to use the
//! code itself would be *noticed*, because the intended recipient's claim would then fail. It is
//! also short-lived, and short *because* the PAKE makes guessing expensive: a wrong guess does not
//! reveal whether it was close, and one wrong guess ends the attempt.
//!
//! **Why it is not the attachment path.** Attachments are convenient: post an image, everybody sees
//! it, three days later the server forgets it. A direct transfer needs both people present at the
//! same time, because there is no server holding the bytes. Different jobs, so both exist.
//!
//! Two rendezvous services are involved and neither sees the file. The **rendezvous server** passes
//! the handshake messages; the **transit relay** forwards the bytes when the two machines cannot
//! reach each other directly, and by then they are encrypted end to end. A self-hosted BoaVoice
//! server can offer its own for both (see `--wormhole-rendezvous`), and the public defaults are used
//! when it does not.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context as _, Result};
use boa_proto::Id;

/// What the interface asks for.
pub enum Command {
    /// Offer a file to somebody. The code comes back as [`Event::Offered`] to be relayed.
    Send { id: u64, to: Id, channel: Id, path: PathBuf },
    /// Accept an offer.
    Receive { id: u64, from: Id, code: String, name: String },
    /// Give up on one, in either direction.
    Cancel { id: u64 },
}

/// What the transfer thread reports.
#[derive(Debug)]
pub enum Event {
    /// A wormhole is open and waiting. The interface relays `code` over the control plane.
    Offered { id: u64, to: Id, channel: Id, code: String, name: String, size: u64 },
    /// Both sides connected, and whether it went directly or through a relay.
    Connected { id: u64, direct: bool },
    Progress { id: u64, done: u64, total: u64 },
    /// Finished. `path` is set on the receiving side.
    Done { id: u64, path: Option<PathBuf> },
    Failed { id: u64, why: String },
}

/// Where the relays are. Empty strings mean "the public defaults".
#[derive(Clone, Default, Debug)]
pub struct Relays {
    pub rendezvous: Option<String>,
    pub transit: Option<String>,
}

/// The interface's handle on the transfer thread.
pub struct Transfers {
    commands: std::sync::mpsc::Sender<(Command, Relays)>,
    events: std::sync::mpsc::Receiver<Event>,
    /// Cancellation flags, one per job, shared with the task doing it.
    cancels: std::sync::Mutex<std::collections::HashMap<u64, Arc<AtomicBool>>>,
    next_id: AtomicU64,
}

impl Transfers {
    /// Start the transfer thread.
    ///
    /// Its own thread and its own runtime, separate from the control connection's. A 40 GB transfer
    /// should not share a scheduler with the socket that carries "somebody is typing" — and a
    /// transfer that wedges should not take the chat with it.
    pub fn spawn(ctx: egui::Context) -> Transfers {
        let (command_tx, command_rx) = std::sync::mpsc::channel::<(Command, Relays)>();
        let (event_tx, event_rx) = std::sync::mpsc::channel::<Event>();

        let cancels: Arc<std::sync::Mutex<std::collections::HashMap<u64, Arc<AtomicBool>>>> =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let shared_cancels = cancels.clone();

        let builder = std::thread::Builder::new().name("boa-transfer".into());
        if let Err(err) = builder.spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .thread_name("boa-transfer-worker")
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    log::error!("transfer: no runtime: {err}");
                    return;
                }
            };
            runtime.block_on(async move {
                // A blocking receive on a std channel would block the runtime, so the queue is
                // drained from a blocking thread and handed over.
                let (async_tx, mut async_rx) = tokio::sync::mpsc::unbounded_channel();
                std::thread::Builder::new()
                    .name("boa-transfer-queue".into())
                    .spawn(move || {
                        while let Ok(item) = command_rx.recv() {
                            if async_tx.send(item).is_err() {
                                return;
                            }
                        }
                    })
                    .expect("spawning a thread");

                while let Some((command, relays)) = async_rx.recv().await {
                    let events = Reporter { tx: event_tx.clone(), ctx: ctx.clone() };
                    let cancels = shared_cancels.clone();
                    tokio::spawn(async move {
                        run(command, relays, events, cancels).await;
                    });
                }
            });
        }) {
            log::error!("transfer: could not start the thread: {err}");
        }

        Transfers {
            commands: command_tx,
            events: event_rx,
            cancels: std::sync::Mutex::new(std::collections::HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// A fresh job id.
    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn send(&self, command: Command, relays: &Relays) {
        if self.commands.send((command, relays.clone())).is_err() {
            log::error!("transfer: the transfer thread is gone");
        }
    }

    pub fn drain(&self) -> Vec<Event> {
        self.events.try_iter().collect()
    }

    /// Note a job's cancellation flag, so the interface can stop it later.
    pub fn track(&self, id: u64, flag: Arc<AtomicBool>) {
        if let Ok(mut cancels) = self.cancels.lock() {
            cancels.insert(id, flag);
        }
    }

    pub fn cancel(&self, id: u64) {
        if let Ok(mut cancels) = self.cancels.lock() {
            if let Some(flag) = cancels.remove(&id) {
                flag.store(true, Ordering::Release);
            }
        }
    }
}

/// The event side, with the repaint wake-up attached.
#[derive(Clone)]
struct Reporter {
    tx: std::sync::mpsc::Sender<Event>,
    ctx: egui::Context,
}

impl Reporter {
    fn send(&self, event: Event) {
        if self.tx.send(event).is_ok() {
            self.ctx.request_repaint();
        }
    }
}

type Cancels = Arc<std::sync::Mutex<std::collections::HashMap<u64, Arc<AtomicBool>>>>;

async fn run(command: Command, relays: Relays, events: Reporter, cancels: Cancels) {
    let id = match &command {
        Command::Send { id, .. } | Command::Receive { id, .. } | Command::Cancel { id } => *id,
    };

    // The flag this job watches. Registered before the work starts, so a cancel that arrives
    // immediately is not missed.
    let flag = Arc::new(AtomicBool::new(false));
    if let Ok(mut map) = cancels.lock() {
        if matches!(command, Command::Cancel { .. }) {
            if let Some(existing) = map.remove(&id) {
                existing.store(true, Ordering::Release);
            }
            return;
        }
        map.insert(id, flag.clone());
    }

    let outcome = match command {
        Command::Send { to, channel, path, .. } => {
            send_file(id, to, channel, path, &relays, &events, flag.clone()).await
        }
        Command::Receive { code, name, .. } => {
            receive_file(id, code, name, &relays, &events, flag.clone()).await
        }
        Command::Cancel { .. } => unreachable!("handled above"),
    };

    if let Ok(mut map) = cancels.lock() {
        map.remove(&id);
    }
    match outcome {
        Ok(path) => events.send(Event::Done { id, path }),
        Err(err) => {
            log::warn!("transfer {id}: {err:#}");
            events.send(Event::Failed { id, why: format!("{err}") });
        }
    }
}

/// The app config, pointed at whichever rendezvous server is in use.
fn app_config(relays: &Relays) -> magic_wormhole::AppConfig<magic_wormhole::transfer::AppVersion> {
    let mut config = magic_wormhole::transfer::APP_CONFIG;
    if let Some(rendezvous) = relays.rendezvous.as_deref().filter(|url| !url.trim().is_empty()) {
        config.rendezvous_url = rendezvous.trim().to_string().into();
    }
    config
}

/// The transit relay hints, or none for the public default.
fn relay_hints(relays: &Relays) -> Vec<magic_wormhole::transit::RelayHint> {
    let configured = relays.transit.as_deref().filter(|url| !url.trim().is_empty());
    match configured {
        Some(url) => match url.trim().parse::<url::Url>() {
            Ok(url) => match magic_wormhole::transit::RelayHint::from_urls(None, [url]) {
                Ok(hint) => vec![hint],
                Err(err) => {
                    log::warn!("transfer: bad transit relay: {err}");
                    default_relay_hints()
                }
            },
            Err(err) => {
                log::warn!("transfer: bad transit relay URL: {err}");
                default_relay_hints()
            }
        },
        None => default_relay_hints(),
    }
}

fn default_relay_hints() -> Vec<magic_wormhole::transit::RelayHint> {
    match magic_wormhole::transit::DEFAULT_RELAY_SERVER.parse::<url::Url>() {
        Ok(url) => magic_wormhole::transit::RelayHint::from_urls(None, [url])
            .map(|hint| vec![hint])
            .unwrap_or_default(),
        // No hints at all still works when the two machines can reach each other directly, which on
        // a LAN — the case this feature is most used for — they can.
        Err(_) => Vec::new(),
    }
}

/// A future that finishes when the flag is set.
///
/// magic-wormhole takes cancellation as a future rather than a token, so this bridges the two. A
/// poll every 200 ms rather than a notifier: cancellation is a human pressing a button, and a fifth
/// of a second is not perceptible.
async fn cancelled(flag: Arc<AtomicBool>) {
    while !flag.load(Ordering::Acquire) {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

async fn send_file(
    id: u64,
    to: Id,
    channel: Id,
    path: PathBuf,
    relays: &Relays,
    events: &Reporter,
    flag: Arc<AtomicBool>,
) -> Result<Option<PathBuf>> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow!("that is not a file"))?;
    let size = tokio::fs::metadata(&path)
        .await
        .with_context(|| format!("reading {}", path.display()))?
        .len();

    // Two words, which with the mailbox number is the strength the wordlist is designed around:
    // each word is one of 256, so a code is ~16 bits of entropy — enough because the PAKE allows
    // exactly one guess per mailbox and a wrong one ends the attempt.
    let mailbox = magic_wormhole::MailboxConnection::create(app_config(relays), 2)
        .await
        .context("opening a wormhole")?;
    let code = mailbox.code().to_string();

    // The code goes out *before* waiting for the other side, which is the whole point: the
    // recipient cannot connect until they have it.
    events.send(Event::Offered { id, to, channel, code, name: name.clone(), size });

    let wormhole = magic_wormhole::Wormhole::connect(mailbox)
        .await
        .context("waiting for the other side")?;

    let mut file = async_compat::Compat::new(
        tokio::fs::File::open(&path)
            .await
            .with_context(|| format!("opening {}", path.display()))?,
    );

    let reporter = events.clone();
    let connected = events.clone();
    magic_wormhole::transfer::send_file(
        wormhole,
        relay_hints(relays),
        &mut file,
        name,
        size,
        magic_wormhole::transit::Abilities::ALL,
        move |info| {
            let direct = matches!(info.conn_type, magic_wormhole::transit::ConnectionType::Direct);
            connected.send(Event::Connected { id, direct });
        },
        move |done, total| reporter.send(Event::Progress { id, done, total }),
        cancelled(flag),
    )
    .await
    .context("sending")?;

    Ok(None)
}

async fn receive_file(
    id: u64,
    code: String,
    name: String,
    relays: &Relays,
    events: &Reporter,
    flag: Arc<AtomicBool>,
) -> Result<Option<PathBuf>> {
    let code: magic_wormhole::Code = code.trim().parse().context("that is not a wormhole code")?;
    // `allocate: false` — the sender allocated the mailbox; asking for another one here would open a
    // second, empty wormhole and wait in it forever.
    let mailbox = magic_wormhole::MailboxConnection::connect(app_config(relays), code, false)
        .await
        .context("opening the wormhole")?;
    let wormhole =
        magic_wormhole::Wormhole::connect(mailbox).await.context("connecting to the sender")?;

    let request = magic_wormhole::transfer::request_file(
        wormhole,
        relay_hints(relays),
        magic_wormhole::transit::Abilities::ALL,
        cancelled(flag.clone()),
    )
    .await
    .context("asking for the file")?
    .ok_or_else(|| anyhow!("the sender withdrew the offer"))?;

    let destination = destination_for(&name)?;
    let file = tokio::fs::File::create(&destination)
        .await
        .with_context(|| format!("creating {}", destination.display()))?;
    let mut writer = async_compat::Compat::new(file);

    let reporter = events.clone();
    let connected = events.clone();
    let result = request
        .accept(
            move |info| {
                let direct =
                    matches!(info.conn_type, magic_wormhole::transit::ConnectionType::Direct);
                connected.send(Event::Connected { id, direct });
            },
            move |done, total| reporter.send(Event::Progress { id, done, total }),
            &mut writer,
            cancelled(flag),
        )
        .await;

    if let Err(err) = result {
        // A half-written file is worse than none: it looks like a download that worked, and the
        // content-addressed store is not involved here to catch it.
        let _ = tokio::fs::remove_file(&destination).await;
        return Err(anyhow!("{err}"));
    }
    Ok(Some(destination))
}

/// Where a received file goes: the downloads folder, without overwriting anything.
pub fn destination_for(name: &str) -> Result<PathBuf> {
    let folder = dirs::download_dir()
        .or_else(dirs::home_dir)
        .ok_or_else(|| anyhow!("no downloads folder"))?;
    std::fs::create_dir_all(&folder).ok();

    let safe = sanitise(name);
    let candidate = folder.join(&safe);
    if !candidate.exists() {
        return Ok(candidate);
    }

    // `report.pdf` becomes `report (2).pdf` — the numbering people expect, before the extension
    // rather than after it, so the file still opens in the right program.
    let stem = std::path::Path::new(&safe)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| safe.clone());
    let extension = std::path::Path::new(&safe)
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    for n in 2..1_000 {
        let candidate = folder.join(format!("{stem} ({n}){extension}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(anyhow!("too many files called {safe}"))
}

/// Reduce a sender-supplied name to something safe to create.
///
/// The name comes from another machine, so it is not ours to trust: a path separator or a `..` would
/// let a sender choose where the file lands.
fn sanitise(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control() && *c != ':')
        .take(200)
        .collect();
    let trimmed = cleaned.trim().trim_start_matches('.').to_string();
    if trimmed.is_empty() {
        "received-file".to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_senders_file_name_cannot_choose_where_it_lands() {
        assert_eq!(sanitise("../../etc/passwd"), "passwd");
        assert_eq!(sanitise(r"C:\Windows\System32\evil.dll"), "evil.dll");
        assert_eq!(sanitise("/etc/shadow"), "shadow");
        assert_eq!(sanitise(".bashrc"), "bashrc");
        assert_eq!(sanitise(""), "received-file");
        assert_eq!(sanitise("   "), "received-file");
        assert_eq!(sanitise("holiday photo.jpg"), "holiday photo.jpg");
        assert!(sanitise(&"x".repeat(500)).len() <= 200);
    }

    #[test]
    fn a_received_file_never_overwrites_one_that_is_there() {
        let path = destination_for("boavoice-test-fixture.bin").unwrap();
        assert!(!path.exists(), "the test needs a name nothing uses");
        assert_eq!(path.file_name().unwrap(), "boavoice-test-fixture.bin");

        // With something in the way, the numbering goes before the extension so the file still
        // opens in the right program.
        std::fs::write(&path, b"in the way").unwrap();
        let second = destination_for("boavoice-test-fixture.bin").unwrap();
        assert_eq!(second.file_name().unwrap(), "boavoice-test-fixture (2).bin");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn the_configured_rendezvous_wins_over_the_default() {
        let default = app_config(&Relays::default());
        let mine = app_config(&Relays {
            rendezvous: Some("ws://boa.example.com:4000/v1".into()),
            transit: None,
        });
        assert_ne!(default.rendezvous_url, mine.rendezvous_url);
        assert_eq!(mine.rendezvous_url, "ws://boa.example.com:4000/v1");

        // A blank string is "not configured", not "an empty URL" — which is what a settings field
        // somebody cleared looks like.
        let blank = app_config(&Relays { rendezvous: Some("  ".into()), transit: None });
        assert_eq!(blank.rendezvous_url, default.rendezvous_url);
    }

    #[test]
    fn a_broken_transit_url_falls_back_rather_than_failing() {
        // The alternative is a transfer that cannot start because of a typo in a server's optional
        // configuration, which is a bad trade — the public relay works, and the bytes are encrypted
        // either way.
        let hints = relay_hints(&Relays { rendezvous: None, transit: Some("not a url".into()) });
        assert_eq!(hints.len(), default_relay_hints().len());
    }
}
