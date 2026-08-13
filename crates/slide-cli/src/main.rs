mod assets;
mod http;
mod pair;
mod pairing;
mod server;
mod ws;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Deserialize;

#[derive(Parser, Debug)]
#[command(name = "slide", version, about = "Multi-session agent IDE")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Start the slide daemon.
    Serve {
        #[arg(long, default_value_t = 7777)]
        port: u16,
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        /// Deprecated: direct LAN HTTP is insecure and refused. Use
        /// --public-url with an HTTPS reverse proxy instead.
        #[arg(long)]
        lan: bool,
        /// HTTPS origin that proxies to this loopback daemon, for example a
        /// Tailscale Serve URL. Enables `slide pair`.
        #[arg(long)]
        public_url: Option<String>,
        /// Skip auto-opening a browser tab.
        #[arg(long)]
        no_open: bool,
        /// Dev mode: disables the embedded SPA so Vite dev-server at :5173
        /// can serve the UI and proxy /api and /ws back to this daemon.
        #[arg(long)]
        dev: bool,
    },
    /// Launch the browser pointed at the running daemon, with the bootstrap
    /// token embedded. The URL itself is never written to stdout — handy
    /// when the daemon was started with `--no-open`.
    Open,
    /// Create a five-minute, single-use phone pairing QR code.
    Pair,
}

#[derive(Deserialize)]
struct LockFile {
    port: u16,
    bootstrap: String,
}

fn read_lock() -> Result<LockFile> {
    let path = slide_core::config::lock_path();
    let body = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "read {} (is the daemon running? try `slide serve`)",
            path.display(),
        )
    })?;
    serde_json::from_str(&body).context("parse daemon.lock")
}

fn cmd_open() -> Result<()> {
    let lock = read_lock()?;
    let url = format!(
        "http://127.0.0.1:{}/#bootstrap={}",
        lock.port, lock.bootstrap
    );
    opener::open(&url).context("open browser")?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,slide_cli=debug,slide_core=debug".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.cmd.unwrap_or(Cmd::Serve {
        port: 7777,
        bind: "127.0.0.1".into(),
        lan: false,
        public_url: None,
        no_open: false,
        dev: false,
    }) {
        Cmd::Serve {
            port,
            bind,
            lan,
            public_url,
            no_open,
            dev,
        } => {
            if lan {
                anyhow::bail!(
                    "--lan is no longer supported because it exposes credentials over HTTP; use --public-url with an HTTPS reverse proxy"
                );
            }
            let public_url = pair::ensure_phone_pairing_is_safe(&bind, public_url.as_deref())?;
            server::run(&bind, port, !no_open, dev, public_url).await?
        }
        Cmd::Open => cmd_open()?,
        Cmd::Pair => pair::run_pair_cmd()?,
    }
    Ok(())
}

#[cfg(test)]
mod cli_tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn serve_lan_flag_parses() {
        let cli = Cli::try_parse_from(["slide", "serve", "--lan"]).expect("parse");
        match cli.cmd {
            Some(Cmd::Serve { lan, bind, .. }) => {
                assert!(lan);
                assert_eq!(bind, "127.0.0.1");
            }
            other => panic!("expected Serve, got {other:?}"),
        }
    }

    #[test]
    fn serve_public_url_parses() {
        let cli = Cli::try_parse_from([
            "slide",
            "serve",
            "--public-url",
            "https://slide.example.ts.net",
        ])
        .expect("parse");
        match cli.cmd {
            Some(Cmd::Serve { public_url, .. }) => {
                assert_eq!(public_url.as_deref(), Some("https://slide.example.ts.net"));
            }
            other => panic!("expected Serve, got {other:?}"),
        }
    }

    #[test]
    fn pair_subcommand_parses() {
        let cli = Cli::try_parse_from(["slide", "pair"]).expect("parse");
        assert!(matches!(cli.cmd, Some(Cmd::Pair)));
    }

    #[test]
    fn serve_default_bind_is_loopback() {
        let cli = Cli::try_parse_from(["slide", "serve"]).expect("parse");
        match cli.cmd {
            Some(Cmd::Serve { bind, lan, .. }) => {
                assert_eq!(bind, "127.0.0.1");
                assert!(!lan);
            }
            other => panic!("expected Serve, got {other:?}"),
        }
    }
}
