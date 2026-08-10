//! What the operator gets to decide, and the defaults for everything they do not.
//!
//! Flags and environment variables, hand-parsed. A self-hosted server is started
//! from a shell script or a systemd unit and never grows a second subcommand, so
//! the whole surface is one struct — and an argument parser would be a dependency
//! that has to be kept current for the rest of the project's life to save thirty
//! lines.
//!
//! Every setting can come from either place, with the flag winning, because those
//! are the two ways a container and a service file respectively want to be
//! configured and picking one annoys half the users.

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;

use anyhow::{bail, Context as _, Result};

/// Default TCP port for the control plane and HTTP.
pub const DEFAULT_PORT: u16 = 8787;

/// Default UDP port for the media plane.
///
/// One above the control port by convention, so an operator writes two adjacent
/// firewall rules. It is a *separate* port, not a second use of the same one, and
/// that is not avoidable: the control plane is HTTP and belongs behind whatever
/// reverse proxy the box already runs, and UDP media cannot go through one.
pub const DEFAULT_MEDIA_PORT: u16 = 8788;

/// Largest upload accepted by default: 64 MiB.
///
/// Chosen against the three-day expiry rather than in isolation. Attachments are a
/// courier service here, not storage — anything genuinely large should go directly
/// between the two machines over the file-transfer path, which has no limit at all
/// because the server never touches the bytes.
pub const DEFAULT_MAX_UPLOAD: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Config {
    /// Where HTTP and the WebSocket listen.
    pub bind: SocketAddr,
    /// Where media arrives. Bound on the same address as `bind`.
    pub media_port: u16,
    /// Database and blobs live under here.
    pub data_dir: PathBuf,
    /// Shown in the client's title bar.
    pub name: String,
    pub max_upload_bytes: u64,
    /// Whether a stranger can create an account.
    ///
    /// Open by default *only until the first account exists* — see
    /// [`Config::registration_allowed`]. A box on the open internet with permanently
    /// open registration is a spam channel, and one that requires an invitation
    /// before anybody at all can log in cannot be set up.
    pub open_registration: bool,
    /// A rendezvous server for direct file transfers, if the operator runs one.
    pub wormhole_rendezvous: Option<String>,
    pub wormhole_transit: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            // Dual-stack by default: `[::]` with the OS's usual v4-mapping accepts
            // both families on one socket, where `0.0.0.0` would quietly refuse
            // every IPv6 client.
            bind: SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), DEFAULT_PORT),
            media_port: DEFAULT_MEDIA_PORT,
            data_dir: PathBuf::from("boavoice-data"),
            name: "BoaVoice".to_string(),
            max_upload_bytes: DEFAULT_MAX_UPLOAD,
            open_registration: true,
            wormhole_rendezvous: None,
            wormhole_transit: None,
        }
    }
}

impl Config {
    pub fn database_path(&self) -> PathBuf {
        self.data_dir.join("boavoice.db")
    }

    pub fn blob_dir(&self) -> PathBuf {
        self.data_dir.join("blobs")
    }

    /// Whether to accept a registration right now.
    ///
    /// `first_account` is whether the database is still empty. The first account is
    /// always allowed: it is how the operator gets in.
    pub fn registration_allowed(&self, first_account: bool) -> bool {
        first_account || self.open_registration
    }

    /// Read flags and the environment. `args` excludes the program name.
    pub fn parse(args: &[String]) -> Result<Self> {
        let mut config = Config::from_env()?;

        let mut i = 0;
        while i < args.len() {
            let arg = args[i].as_str();
            // A value-taking flag needs its value; asking for it in one place means
            // a missing one is reported as such rather than parsed as the next flag.
            let mut value = || -> Result<String> {
                let next = args.get(i + 1).cloned();
                i += 1;
                next.with_context(|| format!("{arg} needs a value"))
            };
            match arg {
                "--bind" => config.bind = parse_bind(&value()?)?,
                "--port" => config.bind.set_port(value()?.parse().context("--port")?),
                "--media-port" => config.media_port = value()?.parse().context("--media-port")?,
                "--data-dir" => config.data_dir = PathBuf::from(value()?),
                "--name" => config.name = value()?,
                "--max-upload-mb" => {
                    let mb: u64 = value()?.parse().context("--max-upload-mb")?;
                    config.max_upload_bytes = mb * 1024 * 1024;
                }
                "--closed-registration" => config.open_registration = false,
                "--open-registration" => config.open_registration = true,
                "--wormhole-rendezvous" => config.wormhole_rendezvous = Some(value()?),
                "--wormhole-transit" => config.wormhole_transit = Some(value()?),
                "--help" | "-h" => {
                    print!("{USAGE}");
                    std::process::exit(0);
                }
                "--version" => {
                    println!("boa-server {}", env!("CARGO_PKG_VERSION"));
                    std::process::exit(0);
                }
                other => bail!("unknown argument {other:?}\n\n{USAGE}"),
            }
            i += 1;
        }

        config.validate()?;
        Ok(config)
    }

    fn from_env() -> Result<Self> {
        let mut config = Config::default();
        if let Ok(value) = std::env::var("BOA_BIND") {
            config.bind = parse_bind(&value)?;
        }
        if let Ok(value) = std::env::var("BOA_PORT") {
            config.bind.set_port(value.parse().context("BOA_PORT")?);
        }
        if let Ok(value) = std::env::var("BOA_MEDIA_PORT") {
            config.media_port = value.parse().context("BOA_MEDIA_PORT")?;
        }
        if let Ok(value) = std::env::var("BOA_DATA_DIR") {
            config.data_dir = PathBuf::from(value);
        }
        if let Ok(value) = std::env::var("BOA_NAME") {
            config.name = value;
        }
        if let Ok(value) = std::env::var("BOA_MAX_UPLOAD_MB") {
            let mb: u64 = value.parse().context("BOA_MAX_UPLOAD_MB")?;
            config.max_upload_bytes = mb * 1024 * 1024;
        }
        if let Ok(value) = std::env::var("BOA_CLOSED_REGISTRATION") {
            config.open_registration = !truthy(&value);
        }
        if let Ok(value) = std::env::var("BOA_WORMHOLE_RENDEZVOUS") {
            config.wormhole_rendezvous = Some(value);
        }
        if let Ok(value) = std::env::var("BOA_WORMHOLE_TRANSIT") {
            config.wormhole_transit = Some(value);
        }
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.media_port == self.bind.port() {
            // They are different protocols on different sockets, so the OS would
            // actually allow this — and then the operator's one firewall rule would
            // open only TCP and voice would silently never connect.
            bail!(
                "the media port ({}) must differ from the control port ({})",
                self.media_port,
                self.bind.port()
            );
        }
        if self.media_port == 0 {
            bail!("the media port cannot be 0: clients are told this number and must be able to reach it");
        }
        if self.name.trim().is_empty() {
            bail!("--name cannot be empty");
        }
        Ok(())
    }

    pub fn server_info(&self) -> boa_proto::ServerInfo {
        boa_proto::ServerInfo {
            name: self.name.clone(),
            media_port: self.media_port,
            protocol_version: boa_proto::PROTOCOL_VERSION,
            attachment_ttl_secs: boa_proto::ATTACHMENT_TTL_SECS,
            max_upload_bytes: self.max_upload_bytes,
            wormhole_rendezvous: self.wormhole_rendezvous.clone(),
            wormhole_transit: self.wormhole_transit.clone(),
        }
    }
}

/// Accept `host:port`, a bare host, or a bare port.
///
/// A bare port is the common case in a container (`--bind 9000`), and a bare host
/// the common case behind a proxy (`--bind 127.0.0.1`). Requiring both every time
/// is the kind of small friction that gets a server started on the wrong interface.
fn parse_bind(text: &str) -> Result<SocketAddr> {
    if let Ok(addr) = text.parse::<SocketAddr>() {
        return Ok(addr);
    }
    if let Ok(port) = text.parse::<u16>() {
        let mut addr = Config::default().bind;
        addr.set_port(port);
        return Ok(addr);
    }
    if let Ok(ip) = text.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, DEFAULT_PORT));
    }
    bail!("{text:?} is not an address, an IP or a port")
}

fn truthy(value: &str) -> bool {
    matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

const USAGE: &str = "\
boa-server — a self-hosted BoaVoice server

USAGE:
    boa-server [OPTIONS]

OPTIONS:
    --bind <addr>              host:port, host, or port     (default [::]:8787)
    --port <port>              just the control port
    --media-port <port>        UDP port for voice and video (default 8788)
    --data-dir <path>          database and attachments     (default ./boavoice-data)
    --name <name>              what clients call this server
    --max-upload-mb <n>        largest attachment           (default 64)
    --closed-registration      only the first account may register
    --open-registration        anybody may register         (the default)
    --wormhole-rendezvous <url>  rendezvous server offered to clients for
                                 direct file transfers
    --wormhole-transit <url>     transit relay for the same
    -h, --help                 this
    --version                  version

Every option can also be given as an environment variable: BOA_BIND, BOA_PORT,
BOA_MEDIA_PORT, BOA_DATA_DIR, BOA_NAME, BOA_MAX_UPLOAD_MB,
BOA_CLOSED_REGISTRATION, BOA_WORMHOLE_RENDEZVOUS, BOA_WORMHOLE_TRANSIT.
Flags win over the environment.

Open both ports: TCP for chat and control, UDP for voice and screen sharing. The
UDP port cannot go through an HTTP reverse proxy.
";

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Config> {
        Config::parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn the_default_binds_dual_stack() {
        let config = Config::default();
        assert!(config.bind.is_ipv6(), "0.0.0.0 would refuse every IPv6 client");
        assert_eq!(config.bind.port(), DEFAULT_PORT);
        assert_eq!(config.media_port, DEFAULT_MEDIA_PORT);
    }

    #[test]
    fn bind_accepts_an_address_a_host_or_a_bare_port() {
        assert_eq!(parse_bind("127.0.0.1:9000").unwrap().port(), 9000);
        assert_eq!(parse_bind("9000").unwrap().port(), 9000);
        assert!(parse_bind("9000").unwrap().is_ipv6(), "a bare port keeps the default host");
        assert_eq!(parse_bind("127.0.0.1").unwrap().port(), DEFAULT_PORT);
        assert!(parse_bind("what").is_err());
    }

    #[test]
    fn flags_win_over_the_environment() {
        // Not testing the environment itself — that would need process-wide state
        // and would race with every other test — but the merge order is visible in
        // the code path: `from_env` first, flags applied over it.
        let config = parse(&["--port", "1234", "--name", "Home"]).unwrap();
        assert_eq!(config.bind.port(), 1234);
        assert_eq!(config.name, "Home");
    }

    #[test]
    fn a_flag_missing_its_value_is_reported_as_such() {
        let err = parse(&["--port"]).unwrap_err();
        assert!(err.to_string().contains("--port needs a value"), "{err}");
        let err = parse(&["--nonsense"]).unwrap_err();
        assert!(err.to_string().contains("unknown argument"), "{err}");
    }

    /// The one configuration mistake that produces a working-looking server with
    /// no voice: both planes on the same number, one firewall rule, TCP only.
    #[test]
    fn the_two_ports_must_differ() {
        let err = parse(&["--port", "8788"]).unwrap_err();
        assert!(err.to_string().contains("must differ"), "{err}");
        assert!(parse(&["--media-port", "0"]).is_err());
    }

    #[test]
    fn registration_is_always_allowed_for_the_first_account() {
        let closed = parse(&["--closed-registration"]).unwrap();
        assert!(closed.registration_allowed(true), "the operator has to be able to get in");
        assert!(!closed.registration_allowed(false));

        let open = Config::default();
        assert!(open.registration_allowed(true));
        assert!(open.registration_allowed(false));
    }

    #[test]
    fn paths_hang_off_the_data_directory() {
        let config = parse(&["--data-dir", "/srv/boa"]).unwrap();
        assert_eq!(config.database_path(), PathBuf::from("/srv/boa/boavoice.db"));
        assert_eq!(config.blob_dir(), PathBuf::from("/srv/boa/blobs"));
    }

    #[test]
    fn upload_limits_are_given_in_megabytes() {
        assert_eq!(parse(&["--max-upload-mb", "8"]).unwrap().max_upload_bytes, 8 * 1024 * 1024);
    }

    #[test]
    fn server_info_reports_what_the_client_cannot_guess() {
        let config = parse(&["--media-port", "9999", "--max-upload-mb", "8"]).unwrap();
        let info = config.server_info();
        assert_eq!(info.media_port, 9999);
        assert_eq!(info.max_upload_bytes, 8 * 1024 * 1024);
        assert_eq!(info.attachment_ttl_secs, boa_proto::ATTACHMENT_TTL_SECS);
        assert_eq!(info.protocol_version, boa_proto::PROTOCOL_VERSION);
    }
}
