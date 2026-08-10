//! `cargo run --example voicecheck -- <server> <name> <password> [channel] [seconds]`
//!
//! Joins a voice channel for real — logs in, opens the control connection, starts the audio engine,
//! registers with the relay — and prints what the media path is doing once a second. Then leaves.
//!
//! It exists because the interesting failures in a voice app are all invisible from the window. "I
//! cannot hear anybody" has at least five distinct causes — the UDP port is closed, the relay never
//! learned this client's address, the microphone was refused, the gate is set too high, the device is
//! at a rate that will not open — and they look identical from the inside. This prints the four
//! numbers that separate them:
//!
//! * **media** — whether the relay is answering our keepalives at all. If this is `no`, the UDP port
//!   is not open and nothing else matters.
//! * **out** — packets we have sent. Zero while `level` is moving means the gate is shut.
//! * **in** — packets received. Zero with somebody else talking means the relay is not forwarding to
//!   us, which is an address-binding problem rather than a codec one.
//! * **level** — what the microphone is picking up, after gain and suppression.
//!
//! Run it twice against the same channel from two machines (or two terminals) and both should show
//! `in` climbing while the other one talks.

use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use boa_client::audio::VoiceSession;
use boa_client::net::api;
use boa_proto::{ChannelKind, ClientMsg, ServerMsg};
use futures_util::{SinkExt as _, StreamExt as _};

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        eprintln!(
            "usage: voicecheck <server> <name> <password> [channel] [seconds]\n\
             \n\
             example: voicecheck localhost:8787 ada 'correct horse battery' Lounge 15"
        );
        std::process::exit(2);
    }
    let base = normalise(&args[0]);
    let (name, password) = (&args[1], &args[2]);
    let wanted_channel = args.get(3).cloned();
    let seconds: u64 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(15);

    let client = api::client()?;
    let probe = api::probe(&client, &base).await?;
    println!("server: {} (protocol {}, media UDP {})", probe.name, probe.protocol_version, probe.media_port);

    let session = api::login(&client, &base, name, password).await.context("logging in")?;
    println!("signed in as {}", session.user.name);

    let ws_url = base
        .replace("https://", "wss://")
        .replace("http://", "ws://")
        + "/ws";
    let (socket, _) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .with_context(|| format!("connecting to {ws_url}"))?;
    let (mut sink, mut stream) = socket.split();

    send(
        &mut sink,
        &ClientMsg::Identify {
            token: session.token.clone(),
            protocol_version: boa_proto::PROTOCOL_VERSION,
            agent: "voicecheck".into(),
        },
    )
    .await?;

    // Wait for `Ready`, then pick the channel.
    let channel = loop {
        match next(&mut stream).await? {
            ServerMsg::Ready { channels, .. } => {
                let voice: Vec<_> = channels.iter().filter(|c| c.kind == ChannelKind::Voice).collect();
                if voice.is_empty() {
                    bail!("this server has no voice channels");
                }
                println!(
                    "voice channels: {}",
                    voice.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ")
                );
                let chosen = match &wanted_channel {
                    Some(wanted) => voice
                        .iter()
                        .find(|c| c.name.eq_ignore_ascii_case(wanted))
                        .ok_or_else(|| anyhow!("no voice channel called {wanted:?}"))?,
                    None => voice[0],
                };
                break (chosen.id, chosen.name.clone());
            }
            ServerMsg::Error { message, .. } => bail!("{message}"),
            _ => continue,
        }
    };

    println!("joining {}…", channel.1);
    send(&mut sink, &ClientMsg::JoinVoice { channel: channel.0 }).await?;

    // `VoiceReady` carries the media credentials.
    let (ssrc, key, media_port) = loop {
        match next(&mut stream).await? {
            ServerMsg::VoiceReady { ssrc, key, media_port, .. } => break (ssrc, key, media_port),
            ServerMsg::Error { message, .. } => bail!("{message}"),
            _ => continue,
        }
    };

    let key = boa_proto::SessionKey::from_base64(&key).ok_or_else(|| anyhow!("bad session key"))?;
    let host = base
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split(':')
        .next()
        .unwrap_or("localhost")
        .to_string();
    let relay = tokio::net::lookup_host((host.as_str(), media_port))
        .await?
        .next()
        .ok_or_else(|| anyhow!("{host} resolved to nothing"))?;
    println!("media: ssrc {ssrc}, relay {relay}");

    let settings = boa_client::settings::Settings::load().voice;
    let voice = VoiceSession::start(relay, key, ssrc, channel.0, &settings)
        .context("starting the audio engine")?;

    // Keep the control connection alive while the engine runs, and report once a second.
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    let mut announced = false;

    println!();
    println!("  time  media   out    in  concealed  level  gate");
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let status = voice.status();
                let elapsed = seconds as i64 - (deadline - tokio::time::Instant::now()).as_secs() as i64;
                println!(
                    "  {:>4}s  {:<5}  {:>4}  {:>4}  {:>9}  {:>5.3}  {}",
                    elapsed.max(0),
                    if status.media_ok { "yes" } else { "NO" },
                    status.packets_out,
                    status.packets_in,
                    status.concealed,
                    status.input_level,
                    if status.gate_open { "open" } else { "shut" },
                );
                // Announcing speech is the interface's job in the real client; here it is done so
                // that the *other* end of a two-terminal test lights up.
                if status.speaking != announced {
                    announced = status.speaking;
                    send(&mut sink, &ClientMsg::Speaking { speaking: announced }).await?;
                }
                if tokio::time::Instant::now() >= deadline {
                    break;
                }
            }
            frame = stream.next() => {
                match frame {
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                        if let Ok(ServerMsg::Speaking { user, speaking }) = serde_json::from_str(&text) {
                            println!("        {user} {}", if speaking { "started talking" } else { "stopped" });
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(err)) => bail!("control connection: {err}"),
                    None => bail!("the server closed the connection"),
                }
            }
        }
    }

    let status = voice.status();
    println!();
    println!("verdict:");
    if !status.media_ok {
        println!("  the relay is not answering. UDP {media_port} is not reaching the server —");
        println!("  open it in the firewall; it cannot go through an HTTP reverse proxy.");
    } else if status.packets_out == 0 {
        println!("  the media path works, but nothing was sent. Either the microphone is muted or");
        println!("  the gate never opened — try talking, or lower the threshold in the settings.");
    } else {
        println!("  sent {} packet(s) and received {}.", status.packets_out, status.packets_in);
        if status.packets_in == 0 {
            println!("  Nothing received, which is expected if nobody else is in the channel.");
        }
    }

    drop(voice);
    send(&mut sink, &ClientMsg::LeaveVoice).await?;
    let _ = sink.close().await;
    Ok(())
}

fn normalise(text: &str) -> String {
    let text = text.trim().trim_end_matches('/');
    if text.starts_with("http://") || text.starts_with("https://") {
        text.to_string()
    } else {
        format!("http://{text}")
    }
}

async fn send<S>(sink: &mut S, msg: &ClientMsg) -> Result<()>
where
    S: futures_util::Sink<tokio_tungstenite::tungstenite::Message> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    let text = serde_json::to_string(msg)?;
    sink.send(tokio_tungstenite::tungstenite::Message::Text(text.into())).await?;
    Ok(())
}

async fn next<S>(stream: &mut S) -> Result<ServerMsg>
where
    S: futures_util::Stream<Item = Result<tokio_tungstenite::tungstenite::Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    loop {
        match stream.next().await {
            Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                match serde_json::from_str(&text) {
                    Ok(msg) => return Ok(msg),
                    Err(err) => log::debug!("unparseable frame: {err}"),
                }
            }
            Some(Ok(_)) => continue,
            Some(Err(err)) => bail!("control connection: {err}"),
            None => bail!("the server closed the connection"),
        }
    }
}
