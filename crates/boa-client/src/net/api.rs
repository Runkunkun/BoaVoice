//! The server's HTTP endpoints: probe, register, log in, and move attachment bytes.
//!
//! All async, all on the network thread's runtime. Nothing here touches the UI or the
//! filesystem beyond the attachment store — the results go back as
//! [`crate::net::Event`]s, so a slow server delays a spinner and never the window.

use anyhow::{anyhow, bail, Context as _, Result};
use boa_proto::Attachment;
use serde::{Deserialize, Serialize};

/// What `/api/info` says about a server before anybody has logged in.
///
/// Fetched by the connect screen so it can name the server, offer registration only when
/// the server would accept it, and — the useful one — refuse a version mismatch with a
/// clear sentence instead of letting the WebSocket be closed a second later for reasons
/// the user cannot see.
#[derive(Clone, Debug, Deserialize)]
pub struct ServerProbe {
    pub name: String,
    pub protocol_version: u16,
    pub media_port: u16,
    pub attachment_ttl_secs: u64,
    pub max_upload_bytes: u64,
    pub registration_open: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Session {
    pub token: String,
    pub user: boa_proto::User,
}

#[derive(Serialize)]
struct Credentials<'a> {
    name: &'a str,
    password: &'a str,
    display_name: &'a str,
    agent: &'a str,
}

/// The string this build reports as its agent, for the server's log.
pub fn agent() -> String {
    format!("boa-client {} {}", env!("CARGO_PKG_VERSION"), std::env::consts::OS)
}

/// A client configured the way every call here wants it.
///
/// Built once per process rather than per request: a fresh `Client` means a fresh
/// connection pool and a fresh TLS handshake, which turns a 2 ms attachment fetch into a
/// 200 ms one on a remote server — and the chat log fetches a lot of attachments.
pub fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        // Long enough for a slow upload over a bad connection, short enough that a
        // server that has gone away does not leave a request outstanding forever.
        .timeout(std::time::Duration::from_secs(120))
        .connect_timeout(std::time::Duration::from_secs(10))
        .user_agent(agent())
        .build()
        .context("building an HTTP client")
}

/// Read the JSON error body a failed request came with.
///
/// The server answers failures with `{"error": "..."}`, and that sentence is written for
/// a person. Falling back to the status code alone loses "a password needs at least 12
/// characters" and replaces it with "400".
async fn failure(response: reqwest::Response) -> anyhow::Error {
    let status = response.status();
    #[derive(Deserialize)]
    struct Body {
        error: String,
    }
    match response.json::<Body>().await {
        Ok(body) => anyhow!("{}", body.error),
        Err(_) => anyhow!("the server answered {status}"),
    }
}

pub async fn probe(client: &reqwest::Client, base: &str) -> Result<ServerProbe> {
    let response = client
        .get(format!("{base}/api/info"))
        .send()
        .await
        .with_context(|| format!("reaching {base}"))?;
    if !response.status().is_success() {
        return Err(failure(response).await);
    }
    let probe: ServerProbe = response.json().await.context("reading the server's reply")?;

    if probe.protocol_version != boa_proto::PROTOCOL_VERSION {
        // Refused here rather than at the WebSocket, where the same mismatch appears as a
        // connection that closes immediately for no visible reason.
        bail!(
            "{} speaks protocol {} and this client speaks {} — one of the two needs updating",
            probe.name,
            probe.protocol_version,
            boa_proto::PROTOCOL_VERSION
        );
    }
    Ok(probe)
}

pub async fn register(
    client: &reqwest::Client,
    base: &str,
    name: &str,
    password: &str,
    display_name: &str,
) -> Result<Session> {
    post_credentials(client, &format!("{base}/api/register"), name, password, display_name).await
}

pub async fn login(
    client: &reqwest::Client,
    base: &str,
    name: &str,
    password: &str,
) -> Result<Session> {
    post_credentials(client, &format!("{base}/api/login"), name, password, "").await
}

async fn post_credentials(
    client: &reqwest::Client,
    url: &str,
    name: &str,
    password: &str,
    display_name: &str,
) -> Result<Session> {
    let agent = agent();
    let response = client
        .post(url)
        .json(&Credentials { name, password, display_name, agent: &agent })
        .send()
        .await
        .context("sending credentials")?;
    if !response.status().is_success() {
        return Err(failure(response).await);
    }
    response.json().await.context("reading the server's reply")
}

/// Upload one file and get back the attachment record to reference in a message.
pub async fn upload(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    name: &str,
    bytes: Vec<u8>,
) -> Result<Attachment> {
    // The name goes in the query string, percent-encoded by `query`, and the body is the
    // file with nothing wrapped around it.
    let response = client
        .post(format!("{base}/api/upload"))
        .query(&[("name", name)])
        .bearer_auth(token)
        .body(bytes)
        .send()
        .await
        .context("uploading")?;
    if !response.status().is_success() {
        return Err(failure(response).await);
    }
    response.json().await.context("reading the upload's reply")
}

/// What happened when we asked for an attachment's bytes.
pub enum Fetched {
    /// Here they are.
    Bytes(Vec<u8>),
    /// The server's three days are up. Not an error: it is the expected end of every
    /// attachment's life on the server, and the caller's job is to fall back to the local
    /// copy or to say plainly that there is none.
    Expired,
}

pub async fn download(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    attachment: boa_proto::Id,
) -> Result<Fetched> {
    let response = client
        .get(format!("{base}/attachments/{attachment}"))
        .bearer_auth(token)
        .send()
        .await
        .context("fetching an attachment")?;

    // 410 Gone is the documented end of an attachment's server life, and is the one status
    // that must not be reported as a failure — see the storage design in the README.
    if response.status() == reqwest::StatusCode::GONE {
        return Ok(Fetched::Expired);
    }
    if !response.status().is_success() {
        return Err(failure(response).await);
    }
    Ok(Fetched::Bytes(response.bytes().await.context("reading attachment bytes")?.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_agent_names_the_build_and_the_platform() {
        let agent = agent();
        assert!(agent.starts_with("boa-client "), "{agent}");
        assert!(agent.contains(std::env::consts::OS), "{agent}");
    }

    #[test]
    fn a_client_can_be_built() {
        // Not much of a test on its own, but it catches the case where a feature flag
        // change leaves reqwest without a TLS backend — which otherwise shows up as
        // every HTTPS server being unreachable at runtime.
        assert!(client().is_ok());
    }
}
