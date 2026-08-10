//! boa-server — the self-hostable half of BoaVoice.
//!
//! Three things run for the life of the process, and they are separate because they
//! fail differently:
//!
//! * the **HTTP/WebSocket listener**, which is the control plane and the attachment
//!   endpoints,
//! * the **UDP relay**, which forwards voice and video and never touches the database,
//! * the **janitor**, which is what stops the disk filling.
//!
//! If the relay dies, chat keeps working and voice does not, and that is worth saying
//! in the log rather than taking the whole process down — but it is *not* worth
//! pretending is fine, so the process exits when any of the three ends. A server that
//! is half up is the state that gets diagnosed for an hour.

mod auth;
mod blobs;
mod config;
mod db;
mod http;
mod hub;
mod relay;
mod session;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context as _, Result};

use crate::blobs::Blobs;
use crate::config::Config;
use crate::db::Db;
use crate::hub::Hub;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info,boa_server=info"),
    )
    .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let config = Config::parse(&args)?;

    std::fs::create_dir_all(&config.data_dir)
        .with_context(|| format!("creating {}", config.data_dir.display()))?;

    let db = Arc::new(Db::open(&config.database_path())?);
    let blobs = Arc::new(Blobs::open(config.blob_dir())?);

    let held = blobs.total_bytes();
    log::info!(
        "{}: data in {} ({} attachment blob(s), {:.1} MiB), attachments live {} days",
        config.name,
        config.data_dir.display(),
        blobs.list().map(|l| l.len()).unwrap_or(0),
        held as f64 / 1024.0 / 1024.0,
        boa_proto::ATTACHMENT_TTL_SECS / 86_400,
    );
    if db.is_empty()? {
        log::info!(
            "no accounts yet — the first to register gets the starter channels, \
             and may register even with --closed-registration"
        );
    }

    let media_bind = SocketAddr::new(config.bind.ip(), config.media_port);
    let control_bind = config.bind;
    let hub = Arc::new(Hub::new(config, db.clone(), blobs.clone()));
    let stats = Arc::new(relay::Stats::default());

    let listener = tokio::net::TcpListener::bind(control_bind)
        .await
        .with_context(|| format!("binding TCP {control_bind}"))?;
    log::info!("control: listening on http://{}", listener.local_addr()?);

    let app = http::router(hub.clone());
    // `into_make_service_with_connect_info` rather than `into_make_service`: the
    // WebSocket handler logs the peer address, and without this the extractor is
    // missing and every upgrade fails at runtime rather than at compile time.
    let control = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .context("serving HTTP")
    });

    let media = tokio::spawn(relay::run(hub.clone(), media_bind, stats.clone()));
    let janitor = tokio::spawn(blobs::janitor(db.clone(), blobs.clone()));
    let reporter = tokio::spawn(report(hub.clone(), stats.clone()));

    // Ctrl-C is a clean exit, not a crash: the operator asked.
    let outcome = tokio::select! {
        result = control => describe("the control listener", result),
        result = media => describe("the media relay", result),
        _ = janitor => Err(anyhow::anyhow!("the janitor stopped")),
        _ = reporter => Err(anyhow::anyhow!("the reporter stopped")),
        _ = tokio::signal::ctrl_c() => {
            log::info!("interrupted; shutting down");
            Ok(())
        }
    };

    outcome
}

/// Turn a joined task's nested result into one error with a name on it.
fn describe(what: &str, result: Result<Result<()>, tokio::task::JoinError>) -> Result<()> {
    match result {
        Ok(Ok(())) => Err(anyhow::anyhow!("{what} stopped on its own")),
        Ok(Err(err)) => Err(err.context(format!("{what} failed"))),
        Err(err) => Err(anyhow::anyhow!("{what} panicked: {err}")),
    }
}

/// A line every five minutes, so a server left running has some evidence of what it
/// has been doing.
///
/// Quiet on purpose: nothing is printed when nothing has happened, because a log that
/// scrolls when the server is idle is a log nobody reads when it is not.
async fn report(hub: Arc<Hub>, stats: Arc<relay::Stats>) {
    let mut previous = (0, 0, 0);
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(300)).await;
        let now = stats.snapshot();
        if now == previous && hub.online().is_empty() {
            continue;
        }
        previous = now;
        log::info!(
            "{} online, {} in voice with a media address; relay {} in / {} out / {} dropped",
            hub.online().len(),
            hub.media_registered(),
            now.0,
            now.1,
            now.2
        );
    }
}
