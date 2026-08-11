//! The HTTP surface: register, log in, and move attachment bytes.
//!
//! Everything else is on the WebSocket. What is here is what has to be here:
//!
//! * **Login** happens before there is a connection to authenticate.
//! * **Attachments** are large. A 40 MB image on the control connection would
//!   head-of-line block every voice-state change and every message behind it for as
//!   long as the upload took; over HTTP it is a separate connection that can be slow
//!   without anybody noticing.
//!
//! Both upload and download are authenticated by the same token the WebSocket uses.
//! The download endpoint takes it in a query parameter as well as a header, because an
//! `<img>` tag cannot set headers and the client's image loader is not going to be the
//! only thing that ever wants a URL it can just fetch.

use std::sync::Arc;

use axum::extract::{ConnectInfo, Path, Query, State, WebSocketUpgrade};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use boa_proto::{Attachment, Id};
use serde::{Deserialize, Serialize};

use crate::blobs;
use crate::hub::Hub;

pub fn router(hub: Arc<Hub>) -> Router {
    Router::new()
        .route("/api/info", get(info))
        .route("/api/stats", get(stats))
        .route("/api/register", post(register))
        .route("/api/login", post(login))
        .route("/api/upload", post(upload))
        .route("/attachments/{id}", get(download))
        .route("/ws", get(websocket))
        // The body limit is the upload limit plus room for headers. Set on the router
        // rather than checked in the handler, so a client sending a gigabyte is cut off
        // while it is uploading rather than after the server has buffered it all.
        .layer(axum::extract::DefaultBodyLimit::max(
            (hub.config.max_upload_bytes + 64 * 1024) as usize,
        ))
        .with_state(hub)
}

// --------------------------------------------------------------------------- //
// Errors
// --------------------------------------------------------------------------- //

/// An HTTP failure with a JSON body, so a client has one shape to parse.
struct Failure(StatusCode, String);

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct Body {
            error: String,
        }
        (self.0, Json(Body { error: self.1 })).into_response()
    }
}

fn bad(message: impl Into<String>) -> Failure {
    Failure(StatusCode::BAD_REQUEST, message.into())
}

fn unauthorised(message: impl Into<String>) -> Failure {
    Failure(StatusCode::UNAUTHORIZED, message.into())
}

/// Log the detail, tell the client nothing.
///
/// A database error's text names tables and file paths. That is information about the
/// server, and it goes in the server's log.
fn internal(context: &str, err: anyhow::Error) -> Failure {
    log::error!("{context}: {err:#}");
    Failure(StatusCode::INTERNAL_SERVER_ERROR, "the server could not do that".into())
}

// --------------------------------------------------------------------------- //
// Public information
// --------------------------------------------------------------------------- //

#[derive(Serialize)]
struct Info {
    name: String,
    protocol_version: u16,
    media_port: u16,
    attachment_ttl_secs: u64,
    max_upload_bytes: u64,
    /// Whether a new account can be created right now. The client's connect screen
    /// uses it to offer "register" or only "log in", rather than finding out by making
    /// somebody fill in a form that will be refused.
    registration_open: bool,
}

async fn info(State(hub): State<Arc<Hub>>) -> Result<Json<Info>, Failure> {
    let first_account = hub.db.is_empty().map_err(|err| internal("counting users", err))?;
    let server = hub.config.server_info();
    Ok(Json(Info {
        name: server.name,
        protocol_version: server.protocol_version,
        media_port: server.media_port,
        attachment_ttl_secs: server.attachment_ttl_secs,
        max_upload_bytes: server.max_upload_bytes,
        registration_open: hub.config.registration_allowed(first_account),
    }))
}

/// What the relay has done with the media it has been sent.
///
/// **Why a self-hosted server should tell you this.** When a screen share stutters there are three
/// candidates — the sender's machine, the network, or the relay — and from the outside they look
/// identical. Two numbers settle it: if `received` and `forwarded` track each other, the relay is
/// passing on everything it is given and the loss is on one of the two legs of the wire; if `dropped`
/// climbs, the relay is refusing packets and the reason is in its log. Guessing between those without
/// numbers is how an afternoon disappears.
///
/// Deliberately public and unauthenticated, and deliberately only counters: how many datagrams a box
/// has forwarded says nothing about who was talking or what they said. It is the same class of
/// information as `/api/info`, which is also public because a client has to read it before it can log
/// in.
#[derive(Serialize)]
struct RelayStats {
    /// Datagrams that arrived on the media port, including rubbish.
    received: u64,
    /// Datagrams sent on to somebody. Higher than `received` when several people are watching one
    /// screen — the same datagram goes out once per subscriber.
    forwarded: u64,
    /// Datagrams the relay refused: not from a registered address, the wrong kind for the stream, or
    /// nobody subscribed to them.
    dropped: u64,
    /// Seconds since the process started, so a rate can be worked out from two visits.
    uptime_secs: u64,
}

async fn stats(State(hub): State<Arc<Hub>>) -> Json<RelayStats> {
    let (received, forwarded, dropped) = hub.stats.snapshot();
    Json(RelayStats {
        received,
        forwarded,
        dropped,
        uptime_secs: hub.started.elapsed().as_secs(),
    })
}

// --------------------------------------------------------------------------- //
// Accounts
// --------------------------------------------------------------------------- //

#[derive(Deserialize)]
struct Credentials {
    name: String,
    password: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    agent: String,
}

#[derive(Serialize)]
struct Session {
    token: String,
    user: boa_proto::User,
}

async fn register(
    State(hub): State<Arc<Hub>>,
    Json(body): Json<Credentials>,
) -> Result<Json<Session>, Failure> {
    let first_account = hub.db.is_empty().map_err(|err| internal("counting users", err))?;
    if !hub.config.registration_allowed(first_account) {
        return Err(Failure(
            StatusCode::FORBIDDEN,
            "this server is not accepting new accounts".into(),
        ));
    }

    crate::auth::check_name_rules(&body.name).map_err(|err| bad(err.to_string()))?;
    crate::auth::check_password_rules(&body.password).map_err(|err| bad(err.to_string()))?;

    let name = crate::auth::normalise_name(&body.name);
    let display_name = if body.display_name.trim().is_empty() {
        body.name.trim().to_string()
    } else {
        body.display_name.trim().chars().take(48).collect()
    };

    // Argon2 is 60 ms of deliberate work. On a runtime worker that is 60 ms of not
    // forwarding anybody's packets, so it goes to a blocking thread.
    let password = body.password.clone();
    let hash = tokio::task::spawn_blocking(move || crate::auth::hash_password(&password))
        .await
        .map_err(|err| internal("hashing", err.into()))?
        .map_err(|err| internal("hashing", err))?;

    let user = hub
        .db
        .create_user(&name, &display_name, &hash)
        .map_err(|err| bad(err.to_string()))?;

    // A brand new server has nothing in it, and an empty sidebar gives a first user
    // nowhere to click. These two are what everybody makes by hand anyway.
    if first_account {
        for (name, kind) in [("general", boa_proto::ChannelKind::Text), ("Lounge", boa_proto::ChannelKind::Voice)] {
            if let Err(err) = hub.db.create_channel(name, kind) {
                log::warn!("creating the starter channel {name}: {err:#}");
            }
        }
        log::info!("{}: first account on this server", user.name);
    }

    let token = issue_token(&hub, user.id, &body.agent)?;
    Ok(Json(Session { token, user }))
}

async fn login(
    State(hub): State<Arc<Hub>>,
    Json(body): Json<Credentials>,
) -> Result<Json<Session>, Failure> {
    let name = crate::auth::normalise_name(&body.name);
    let found = hub
        .db
        .user_by_name(&name)
        .map_err(|err| internal("looking up a user", err))?;

    // The same answer for "no such user" and "wrong password", and the same amount of
    // work: verifying against a dummy hash when the account does not exist keeps the
    // response time from telling a stranger which names are real.
    let (user, stored) = match found {
        Some(pair) => pair,
        None => (
            boa_proto::User { id: Id::NONE, name: name.clone(), display_name: String::new(), online: false },
            DUMMY_HASH.to_string(),
        ),
    };

    let password = body.password.clone();
    let ok = tokio::task::spawn_blocking(move || crate::auth::verify_password(&password, &stored))
        .await
        .map_err(|err| internal("verifying", err.into()))?;

    if !ok || user.id.is_none() {
        return Err(unauthorised("wrong name or password"));
    }

    let token = issue_token(&hub, user.id, &body.agent)?;
    Ok(Json(Session { token, user }))
}

/// An Argon2 hash of a password nobody has, so a login for a name that does not exist
/// costs the same as one that does.
const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHRzb21lc2FsdA$\
    Q0kKJTQ0hZ8s5xJZ9k7VYnP+P9ZQZ6z6mFhKZ8Xn5Cw";

fn issue_token(hub: &Hub, user: Id, agent: &str) -> Result<String, Failure> {
    let token = crate::auth::new_token();
    let agent: String = agent.chars().take(120).collect();
    hub.db
        .store_token(&token, user, &agent)
        .map_err(|err| internal("storing a token", err))?;
    Ok(token)
}

// --------------------------------------------------------------------------- //
// Attachments
// --------------------------------------------------------------------------- //

#[derive(Deserialize)]
struct UploadParams {
    /// The original file name. Only metadata — blobs are named by their hash.
    name: String,
    /// Bearer token, for callers that would rather not set a header.
    #[serde(default)]
    token: Option<String>,
}

#[derive(Deserialize)]
struct DownloadParams {
    #[serde(default)]
    token: Option<String>,
}

/// Resolve the caller from an `Authorization: Bearer` header or a `token` parameter.
fn authenticate(hub: &Hub, headers: &HeaderMap, query: Option<&str>) -> Result<Id, Failure> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_string);
    let token = bearer
        .or_else(|| query.map(str::to_string))
        .ok_or_else(|| unauthorised("no token"))?;

    match hub.db.user_for_token(&token) {
        Ok(Some(user)) => Ok(user.id),
        Ok(None) => Err(unauthorised("that token is not valid")),
        Err(err) => Err(internal("checking a token", err)),
    }
}

/// Take an upload and record it. The body is the file, raw.
///
/// Raw rather than multipart: there is exactly one file per request and the name comes
/// in the query string, which makes the whole thing a `PUT`-shaped `POST` that any
/// HTTP client can produce without a form-encoding library.
async fn upload(
    State(hub): State<Arc<Hub>>,
    Query(params): Query<UploadParams>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Attachment>, Failure> {
    let uploader = authenticate(&hub, &headers, params.token.as_deref())?;

    if body.is_empty() {
        return Err(bad("an empty upload"));
    }
    if body.len() as u64 > hub.config.max_upload_bytes {
        return Err(Failure(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("the limit here is {} MB", hub.config.max_upload_bytes / 1024 / 1024),
        ));
    }

    let name = blobs::sanitise_name(&params.name);
    // The extension decides the type, never the client's `Content-Type`. An upload
    // that came back as `text/html` from the server's own origin would be stored
    // cross-site scripting.
    let content_type = blobs::content_type_for(&name);
    let (width, height) = blobs::image_dimensions(&body);

    let sha256 = hub
        .blobs
        .store(&body)
        .await
        .map_err(|err| internal("storing a blob", err))?;

    let attachment = hub
        .db
        .insert_attachment(uploader, &name, body.len() as u64, content_type, width, height, &sha256)
        .map_err(|err| internal("recording an attachment", err))?;

    log::debug!(
        "upload: {} ({} bytes, {content_type}) expires {}",
        attachment.name,
        attachment.size,
        attachment.expires_at
    );
    Ok(Json(attachment))
}

async fn download(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<u64>,
    Query(params): Query<DownloadParams>,
    headers: HeaderMap,
) -> Result<Response, Failure> {
    authenticate(&hub, &headers, params.token.as_deref())?;

    let found = hub
        .db
        .attachment(Id(id))
        .map_err(|err| internal("looking up an attachment", err))?;
    let Some((attachment, blob_deleted)) = found else {
        return Err(Failure(StatusCode::NOT_FOUND, "no such attachment".into()));
    };

    if blob_deleted {
        // 410 rather than 404, and the distinction is the whole storage design: this
        // file *existed*, the server no longer has it, and a client that downloaded it
        // within its three days still does. A 404 would suggest looking again later.
        return Err(Failure(
            StatusCode::GONE,
            format!(
                "that attachment's {} days on the server are up; \
                 whoever had it open still has their own copy",
                hub.config.server_info().attachment_ttl_secs / 86_400
            ),
        ));
    }

    let bytes = match hub.blobs.read(&attachment.sha256).await {
        Ok(bytes) => bytes,
        Err(err) => {
            // The row says the blob is there and it is not. Worth an error rather than
            // a 404: it means the disk was tampered with, or a janitor pass died
            // between removing the file and marking the row.
            log::error!("attachment {id}: {err:#}");
            return Err(Failure(StatusCode::GONE, "the bytes for that attachment are missing".into()));
        }
    };

    let headers = [
        (header::CONTENT_TYPE, attachment.content_type.clone()),
        // Everything is an attachment, even images. The client draws them itself from
        // the bytes; a browser that renders an upload inline does it in the server's
        // origin, which is the one place it must not.
        (
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", attachment.name),
        ),
        // A blob is named by its content and never changes, so it can be cached hard.
        // `immutable` matters here: the file will be *gone* in three days rather than
        // different, and a client that kept it is doing exactly what it should.
        (header::CACHE_CONTROL, "private, max-age=604800, immutable".to_string()),
        (header::ETAG, format!("\"{}\"", attachment.sha256)),
    ];
    Ok((headers, bytes).into_response())
}

// --------------------------------------------------------------------------- //
// The control connection
// --------------------------------------------------------------------------- //

async fn websocket(
    State(hub): State<Arc<Hub>>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    upgrade: WebSocketUpgrade,
) -> Response {
    log::debug!("ws: upgrade from {peer}");
    // Authentication happens in the first frame rather than here — see
    // [`crate::session::identify`]. Doing it at the upgrade would mean the token in a
    // URL, and URLs end up in proxy logs.
    upgrade.on_upgrade(move |socket| crate::session::run(hub, socket))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dummy hash has to actually parse, or the equal-cost path for an unknown
    /// user would take a different (and much faster) route through
    /// `verify_password` and give the timing away after all.
    #[test]
    fn the_dummy_hash_is_a_real_argon2_hash() {
        assert!(
            argon2::password_hash::PasswordHash::new(DUMMY_HASH).is_ok(),
            "the constant must parse as PHC, or an unknown name is measurably faster"
        );
        assert!(!crate::auth::verify_password("anything at all", DUMMY_HASH));
    }
}
