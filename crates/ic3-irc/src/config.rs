//! Configuration: a TOML file, environment variables and command-line flags,
//! in rising order of precedence.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use serde::Deserialize;

/// Command line.
#[derive(Debug, Parser)]
#[command(
    name = "ic3-irc",
    version,
    about = "An IRCv3 server for Microsoft Teams chat"
)]
pub struct Cli {
    /// The config file (default: the platform config dir's `ic3-irc/config.toml`).
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,
    /// Where IRC clients connect, e.g. 127.0.0.1:6667; repeat for several.
    #[arg(long)]
    pub listen: Vec<SocketAddr>,
    /// Where the web endpoint (media links, uploads) listens; `off` disables it.
    #[arg(long)]
    pub http_listen: Option<String>,
    /// The base URL clients reach the web endpoint at.
    #[arg(long)]
    pub public_url: Option<String>,
    /// The Azure AD tenant (domain or GUID).
    #[arg(long)]
    pub tenant: Option<String>,
    /// Record every Teams request and push frame (redacted) under this directory.
    #[arg(long)]
    pub capture_dir: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Option<Cmd>,
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum Cmd {
    /// Run the server (the default).
    Run,
    /// Forget the saved Teams session.
    Logout,
    /// Install and start the systemd user service (starts at login).
    Install,
    /// Stop, disable and remove the systemd user service.
    Uninstall,
    /// Start the service.
    Start,
    /// Stop the service.
    Stop,
    /// Restart the service (e.g. after changing the config).
    Restart,
    /// Start the service at login.
    Enable,
    /// Do not start the service at login.
    Disable,
    /// Show whether the service runs, and its latest log lines.
    Status,
    /// Show the service's log.
    Logs {
        /// Keep printing new lines as they come.
        #[arg(short, long)]
        follow: bool,
        /// How many lines to show first.
        #[arg(short = 'n', long, default_value_t = 100)]
        lines: u32,
    },
}

/// The config file's contents; every field is optional.
#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    /// One address, or a list of them.
    pub listen: Option<Listen>,
    pub http_listen: Option<String>,
    pub public_url: Option<String>,
    pub password: Option<String>,
    pub tenant: Option<String>,
    pub server_name: Option<String>,
    /// Group chats idle longer than this are not joined; 0 joins all. The
    /// starting value: `Server`'s `cutoff` changes it at run time.
    pub max_chat_age_days: Option<u64>,
    pub session_path: Option<PathBuf>,
    pub capture_dir: Option<PathBuf>,
    /// The log filter (`RUST_LOG` syntax).
    pub log: Option<String>,
}

/// `listen`: one address or several.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Listen {
    One(SocketAddr),
    Many(Vec<SocketAddr>),
}

impl Listen {
    fn into_vec(self) -> Vec<SocketAddr> {
        match self {
            Listen::One(a) => vec![a],
            Listen::Many(v) => v,
        }
    }
}

/// The resolved configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Every address IRC clients may connect to.
    pub listen: Vec<SocketAddr>,
    pub http_listen: Option<SocketAddr>,
    pub public_url: Option<String>,
    pub password: Option<String>,
    pub tenant: String,
    pub server_name: String,
    pub max_chat_age: Option<Duration>,
    pub session_path: PathBuf,
    pub capture_dir: Option<PathBuf>,
    pub log: String,
}

const DEFAULT_LISTEN: &str = "127.0.0.1:6667";
const DEFAULT_HTTP_LISTEN: &str = "127.0.0.1:6680";

/// Loads the config: the file (if present), then `IC3_IRC_PASSWORD` and
/// `RUST_LOG`, then the command line.
pub fn load(cli: &Cli, env: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<Config> {
    let dirs = directories::ProjectDirs::from("", "", "ic3-irc");
    let path = cli
        .config
        .clone()
        .or_else(|| dirs.as_ref().map(|d| d.config_dir().join("config.toml")));
    let file = match &path {
        Some(p) if p.exists() => read_file(p)?,
        Some(p) if cli.config.is_some() => {
            anyhow::bail!("config file {} does not exist", p.display())
        }
        _ => FileConfig::default(),
    };
    let data_dir = dirs
        .as_ref()
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    resolve(cli, file, env, &data_dir)
}

fn read_file(p: &Path) -> anyhow::Result<FileConfig> {
    let text = std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing {}", p.display()))
}

fn resolve(
    cli: &Cli,
    f: FileConfig,
    env: &dyn Fn(&str) -> Option<String>,
    data_dir: &Path,
) -> anyhow::Result<Config> {
    let mut listen = if cli.listen.is_empty() {
        f.listen.map(Listen::into_vec).unwrap_or_default()
    } else {
        cli.listen.clone()
    };
    if listen.is_empty() {
        listen.push(DEFAULT_LISTEN.parse().expect("valid default"));
    }
    let http = cli
        .http_listen
        .clone()
        .or(f.http_listen)
        .unwrap_or_else(|| DEFAULT_HTTP_LISTEN.into());
    let http_listen = if http == "off" {
        None
    } else {
        Some(
            http.parse::<SocketAddr>()
                .with_context(|| format!("http_listen: {http:?} is not an address"))?,
        )
    };
    let public_url = http_listen.map(|addr| {
        cli.public_url
            .clone()
            .or(f.public_url.clone())
            .unwrap_or_else(|| format!("http://{addr}"))
            .trim_end_matches('/')
            .to_string()
    });
    let max_chat_age = match f.max_chat_age_days.unwrap_or(60) {
        0 => None,
        days => Some(Duration::from_secs(days * 24 * 3600)),
    };
    Ok(Config {
        listen,
        http_listen,
        public_url,
        password: env("IC3_IRC_PASSWORD")
            .or(f.password)
            .filter(|p| !p.is_empty()),
        tenant: cli
            .tenant
            .clone()
            .or(f.tenant)
            .unwrap_or_else(|| "organizations".into()),
        server_name: f.server_name.unwrap_or_else(|| "ic3-irc".into()),
        max_chat_age,
        session_path: f
            .session_path
            .unwrap_or_else(|| data_dir.join("session.json")),
        capture_dir: cli.capture_dir.clone().or(f.capture_dir),
        log: env("RUST_LOG").or(f.log).unwrap_or_else(|| "info".into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(args: &[&str]) -> Cli {
        Cli::parse_from(std::iter::once("ic3-irc").chain(args.iter().copied()))
    }

    #[test]
    fn defaults() {
        let c = resolve(&cli(&[]), FileConfig::default(), &|_| None, Path::new("/d")).unwrap();
        assert_eq!(c.listen, vec!["127.0.0.1:6667".parse().unwrap()]);
        assert_eq!(c.public_url.as_deref(), Some("http://127.0.0.1:6680"));
        assert_eq!(c.session_path, PathBuf::from("/d/session.json"));
        assert_eq!(c.max_chat_age, Some(Duration::from_secs(60 * 24 * 3600)));
        assert_eq!(c.password, None);
    }

    #[test]
    fn file_then_env_then_flags() {
        let f: FileConfig = toml::from_str(
            "listen = \"0.0.0.0:7000\"\npassword = \"file\"\npublic_url = \"https://irc.example/\"\nmax_chat_age_days = 0\n",
        )
        .unwrap();
        let env = |k: &str| (k == "IC3_IRC_PASSWORD").then(|| "env".to_string());
        let c = resolve(
            &cli(&["--listen", "127.0.0.1:7001"]),
            f,
            &env,
            Path::new("/d"),
        )
        .unwrap();
        assert_eq!(c.listen, vec!["127.0.0.1:7001".parse().unwrap()]);
        assert_eq!(c.password.as_deref(), Some("env"));
        assert_eq!(c.public_url.as_deref(), Some("https://irc.example"));
        assert_eq!(c.max_chat_age, None);
    }

    #[test]
    fn listen_takes_one_address_or_several() {
        let f: FileConfig =
            toml::from_str("listen = [\"127.0.0.1:6667\", \"10.4.5.1:6667\"]").unwrap();
        let c = resolve(&cli(&[]), f, &|_| None, Path::new("/d")).unwrap();
        assert_eq!(c.listen.len(), 2);
        let c = resolve(
            &cli(&["--listen", "127.0.0.1:1", "--listen", "127.0.0.1:2"]),
            FileConfig::default(),
            &|_| None,
            Path::new("/d"),
        )
        .unwrap();
        assert_eq!(c.listen.len(), 2);
    }

    #[test]
    fn http_can_be_turned_off_and_typos_are_errors() {
        let c = resolve(
            &cli(&["--http-listen", "off"]),
            FileConfig::default(),
            &|_| None,
            Path::new("/d"),
        )
        .unwrap();
        assert_eq!((c.http_listen, c.public_url), (None, None));
        assert!(toml::from_str::<FileConfig>("lisen = \"x\"").is_err());
        assert!(
            resolve(
                &cli(&["--http-listen", "nope"]),
                FileConfig::default(),
                &|_| None,
                Path::new("/d")
            )
            .is_err()
        );
    }
}
