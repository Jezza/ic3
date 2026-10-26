//! `ic3-irc`: run the IRC server for Teams chat.

use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use ic3_irc::backend::{Backend, Ic3Backend};
use ic3_irc::config::{self, Cli, Cmd, Config};
use ic3_irc::conn::{self, ConnConfig};
use ic3_irc::gateway::{self, GatewayConfig};
use ic3_irc::media::MediaTable;
use ic3_irc::service;
use ic3_teams::TokenStore;
use ic3_teams::logging::{RedactingMakeWriter, filter_directives};
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

/// Inputs queued from connections to the gateway.
const INPUT_QUEUE: usize = 1024;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cfg = config::load(&cli, &|k| std::env::var(k).ok())?;
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(filter_directives(&cfg.log)))
        .with_writer(RedactingMakeWriter::new(std::io::stderr))
        .with_ansi(false)
        .init();
    match cli.command.unwrap_or(Cmd::Run) {
        Cmd::Logout => {
            ic3_teams::FileTokenStore::new(&cfg.session_path)
                .clear()
                .await?;
            tracing::info!(path = %cfg.session_path.display(), "session cleared");
            Ok(())
        }
        Cmd::Run => run(cfg).await,
        Cmd::Install => service::install(cli.config.as_deref()),
        Cmd::Uninstall => service::uninstall(),
        Cmd::Start => service::start(),
        Cmd::Stop => service::stop(),
        Cmd::Restart => service::restart(),
        Cmd::Enable => service::enable(),
        Cmd::Disable => service::disable(),
        Cmd::Status => std::process::exit(service::status()?),
        Cmd::Logs { follow, lines } => service::logs(follow, lines),
    }
}

async fn run(cfg: Config) -> anyhow::Result<()> {
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "ic3-irc starting");
    if let Some(dir) = cfg.session_path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let client = ic3_teams::Client::builder()
        .tenant(&cfg.tenant)
        .token_store(ic3_teams::FileTokenStore::new(&cfg.session_path))
        .capture_dir_opt(cfg.capture_dir.clone())
        .build()?;
    let backend: Arc<dyn Backend> = Arc::new(Ic3Backend::new(client));
    let media = Arc::new(MediaTable::new(cfg.public_url.clone()));

    let mut irc = Vec::new();
    for addr in &cfg.listen {
        let l = TcpListener::bind(addr)
            .await
            .with_context(|| format!("listening on {addr}"))?;
        tracing::info!(%addr, "IRC listening");
        irc.push(l);
    }
    if let Some(addr) = cfg.http_listen {
        let http = TcpListener::bind(addr)
            .await
            .with_context(|| format!("listening on {addr}"))?;
        tracing::info!(%addr, public_url = ?cfg.public_url, "web endpoint listening");
        let app = ic3_irc::http::router(backend.clone(), media.clone(), cfg.password.clone());
        tokio::spawn(async move {
            if let Err(e) = axum::serve(http, app).await {
                tracing::error!(error = %e, "web endpoint stopped");
            }
        });
    }

    let (in_tx, in_rx) = tokio::sync::mpsc::channel(INPUT_QUEUE);
    let gw_cfg = GatewayConfig {
        server_name: cfg.server_name.clone(),
        max_chat_age: cfg.max_chat_age,
        settings_path: cfg.session_path.parent().map(|d| d.join("settings.json")),
        filehost: media.filehost_url(),
        ..GatewayConfig::default()
    };
    let gateway = tokio::spawn(gateway::run(backend, gw_cfg, media, in_rx));
    let conn_cfg = Arc::new(ConnConfig {
        server_name: cfg.server_name.clone(),
        password: cfg.password.clone(),
    });
    let mut listeners = tokio::task::JoinSet::new();
    for l in irc {
        listeners.spawn(conn::serve(l, in_tx.clone(), conn_cfg.clone()));
    }
    drop(in_tx);
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("installing the SIGTERM handler")?;
    tokio::select! {
        Some(r) = listeners.join_next() => {
            r.context("IRC listener panicked")?.context("accepting IRC connections")?;
        }
        _ = tokio::signal::ctrl_c() => tracing::info!("interrupted; shutting down"),
        _ = term.recv() => tracing::info!("terminated; shutting down"),
    }
    gateway.abort();
    Ok(())
}
