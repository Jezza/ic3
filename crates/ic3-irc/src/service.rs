//! Running ic3-irc as a systemd user service: `ic3-irc install`,
//! `uninstall`, `start`, `stop`, `restart`, `enable`, `disable`, `status`
//! and `logs`, thin wrappers around `systemctl --user` and `journalctl`.
//! The service logs to the journal; its level is the config file's `log`.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};

/// The unit's name.
pub const UNIT: &str = "ic3-irc.service";

/// `~/.config/systemd/user/ic3-irc.service` (or under `$XDG_CONFIG_HOME`).
pub fn unit_path() -> anyhow::Result<PathBuf> {
    let base = directories::BaseDirs::new().context("no home directory")?;
    Ok(base.config_dir().join("systemd").join("user").join(UNIT))
}

/// The unit file running `exe` (with `--config config` when given).
pub fn unit_text(exe: &Path, config: Option<&Path>) -> String {
    let mut exec = quote(&exe.display().to_string());
    if let Some(c) = config {
        exec.push_str(" --config ");
        exec.push_str(&quote(&c.display().to_string()));
    }
    exec.push_str(" run");
    format!(
        "[Unit]\n\
         Description=ic3-irc: an IRC server for Microsoft Teams chat\n\
         Wants=network-online.target\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         ExecStart={exec}\n\
         Restart=on-failure\n\
         RestartSec=10\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

/// An `ExecStart` word: quoted when it has a space, quote or backslash, and
/// `%` doubled (systemd's specifier escape).
fn quote(word: &str) -> String {
    let word = word.replace('%', "%%");
    if word.contains([' ', '"', '\\', '\'']) {
        format!("\"{}\"", word.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        word
    }
}

/// Writes the unit for the running binary, then enables and starts it.
pub fn install(config: Option<&Path>) -> anyhow::Result<()> {
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .context("finding this program's path")?;
    if exe.components().any(|c| c.as_os_str() == "target") {
        eprintln!(
            "note: installing {} from a build directory; `cargo install --path crates/ic3-irc` gives a stable path",
            exe.display()
        );
    }
    let config = config
        .map(|c| {
            c.canonicalize()
                .with_context(|| format!("config file {}", c.display()))
        })
        .transpose()?;
    let path = unit_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&path, unit_text(&exe, config.as_deref()))
        .with_context(|| format!("writing {}", path.display()))?;
    println!("wrote {}", path.display());
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", UNIT])?;
    println!(
        "ic3-irc is running and starts at login. Logs: ic3-irc logs -f\n\
         To keep it running while you are logged out: loginctl enable-linger"
    );
    Ok(())
}

/// Stops and disables the service and removes its unit.
pub fn uninstall() -> anyhow::Result<()> {
    let path = unit_path()?;
    if !path.exists() {
        bail!(
            "ic3-irc is not installed ({} does not exist)",
            path.display()
        );
    }
    // Disabling a unit that is already stopped is fine; a failure here
    // should not keep the file around.
    if let Err(e) = systemctl(&["disable", "--now", UNIT]) {
        eprintln!("warning: {e:#}");
    }
    std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    systemctl(&["daemon-reload"])?;
    println!("removed {}", path.display());
    Ok(())
}

pub fn start() -> anyhow::Result<()> {
    systemctl(&["start", UNIT])
}

pub fn stop() -> anyhow::Result<()> {
    systemctl(&["stop", UNIT])
}

pub fn restart() -> anyhow::Result<()> {
    systemctl(&["restart", UNIT])
}

/// Starts it at login from now on (without starting it now).
pub fn enable() -> anyhow::Result<()> {
    systemctl(&["enable", UNIT])
}

/// No longer starts it at login (a running one keeps running).
pub fn disable() -> anyhow::Result<()> {
    systemctl(&["disable", UNIT])
}

/// The service's journal: the last `lines` lines, and new ones as they
/// come with `follow`.
pub fn logs(follow: bool, lines: u32) -> anyhow::Result<()> {
    let mut cmd = Command::new("journalctl");
    cmd.args(["--user", "--unit", UNIT, "--output", "cat", "--lines"])
        .arg(lines.to_string());
    if follow {
        cmd.arg("--follow");
    } else {
        cmd.arg("--no-pager");
    }
    let st = cmd
        .status()
        .context("running journalctl (is this a systemd system?)")?;
    if !st.success() {
        bail!("journalctl failed ({st})");
    }
    Ok(())
}

/// `systemctl --user status`; its exit code says whether it runs.
pub fn status() -> anyhow::Result<i32> {
    let st = Command::new("systemctl")
        .args(["--user", "status", "--no-pager", UNIT])
        .status()
        .context("running systemctl (is this a systemd system?)")?;
    Ok(st.code().unwrap_or(1))
}

fn systemctl(args: &[&str]) -> anyhow::Result<()> {
    let st = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .context("running systemctl (is this a systemd system?)")?;
    if !st.success() {
        bail!("systemctl --user {} failed ({st})", args.join(" "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_runs_the_binary_with_its_config() {
        let u = unit_text(Path::new("/home/a/.cargo/bin/ic3-irc"), None);
        assert!(
            u.contains("\nExecStart=/home/a/.cargo/bin/ic3-irc run\n"),
            "{u}"
        );
        assert!(u.contains("Restart=on-failure") && u.contains("WantedBy=default.target"));
        assert!(
            !u.contains("RUST_LOG"),
            "the config file decides the log level"
        );
        let u = unit_text(
            Path::new("/opt/my apps/ic3-irc"),
            Some(Path::new("/home/a/100%/c.toml")),
        );
        assert!(
            u.contains("ExecStart=\"/opt/my apps/ic3-irc\" --config /home/a/100%%/c.toml run"),
            "{u}"
        );
    }
}
