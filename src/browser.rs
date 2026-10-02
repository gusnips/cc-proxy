//! Open a URL in the user's default browser.

use std::process::Command;

pub fn open(url: &str) -> anyhow::Result<()> {
    let mut command = if cfg!(target_os = "macos") {
        let mut command = Command::new("open");
        command.arg(url);
        command
    } else if cfg!(target_os = "windows") {
        let mut command = Command::new("cmd");
        command.args(["/c", "start", "", url]);
        command
    } else {
        let mut command = Command::new("xdg-open");
        command.arg(url);
        command
    };
    let status = command.status()?;
    if !status.success() {
        anyhow::bail!("open command exited with {status}");
    }
    Ok(())
}
