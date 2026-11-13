mod auth;
mod channels;
mod db;
mod encode;
mod import;
mod rdp;
mod session;
mod web;

use std::net::{IpAddr, SocketAddr};

use anyhow::{Context as _, bail};
use tracing_subscriber::EnvFilter;

/// Settings read from the environment (see `deploy/rust-portal.env.example`).
#[derive(Clone, Debug)]
pub struct Config {
    /// Address the HTTP server listens on; nginx proxies to it.
    pub listen: SocketAddr,
    /// Public IP advertised to browsers as the WebRTC candidate.
    pub public_ip: IpAddr,
    /// Inclusive UDP port range for WebRTC media, one port per session.
    pub udp_ports: (u16, u16),
    pub db_url: String,
    /// Upper bound for the video bitrate, in bits per second.
    pub max_bitrate: u32,
}

impl Config {
    fn from_env() -> anyhow::Result<Self> {
        let var = |name: &str, default: &str| std::env::var(name).unwrap_or_else(|_| default.to_owned());
        let (lo, hi) = var("UDP_PORTS", "50000-50100")
            .split_once('-')
            .map(|(a, b)| (a.to_owned(), b.to_owned()))
            .context("UDP_PORTS must look like 50000-50100")?;
        Ok(Self {
            listen: var("LISTEN", "127.0.0.1:8090").parse().context("LISTEN")?,
            public_ip: std::env::var("PUBLIC_IP")
                .context("PUBLIC_IP must be set")?
                .parse()
                .context("PUBLIC_IP")?,
            udp_ports: (lo.parse()?, hi.parse()?),
            db_url: var("DATABASE_URL", "sqlite://portal.db"),
            max_bitrate: var("MAX_BITRATE", "6000000").parse().context("MAX_BITRATE")?,
        })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("rust_portal=info,str0m=warn")),
        )
        .init();
    str0m::crypto::from_feature_flags().install_process_default();

    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("import") => {
            let pool = db::open(&std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite://portal.db".into())).await?;
            let guac = std::env::var("GUAC_DB_URL").context("GUAC_DB_URL must point to the Guacamole Postgres")?;
            import::run(&pool, &guac).await
        }
        Some("users") => {
            let pool = db::open(&std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite://portal.db".into())).await?;
            println!("{:<20} {:<6} {:<10} {:<9} computers", "username", "admin", "2fa", "status");
            for u in db::list_users(&pool).await? {
                let twofa = if u.totp_disabled { "off" } else if u.totp_enrolled { "on" } else { "setup" };
                let n = u.connection_ids.as_deref().map_or(0, |s| s.split(',').count());
                let status = if u.disabled { "disabled" } else { "active" };
                println!("{:<20} {:<6} {:<10} {:<9} {n}", u.username, u.is_admin, twofa, status);
            }
            Ok(())
        }
        Some("probe") => {
            let pool = db::open(&std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite://portal.db".into())).await?;
            let id: i64 = args.get(2).context("usage: rust-portal probe <connection id>")?.parse()?;
            rdp::probe(&pool, id).await
        }
        Some("serve") | None => {
            let config = Config::from_env()?;
            let pool = db::open(&config.db_url).await?;
            web::serve(config, pool).await
        }
        Some(other) => bail!("unknown command {other:?} (expected serve, import, users or probe)"),
    }
}
