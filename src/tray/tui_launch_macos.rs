//! Open `ai-usagebar-tui`, or a provider's login command, in Terminal.app.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

pub fn open() {
    run_in_terminal(&quote(&sibling("ai-usagebar-tui")));
}

/// Run one shell line in a new Terminal window, which stays open afterwards.
pub fn run_in_terminal(line: &str) {
    let tmp = std::env::temp_dir().join(format!(
        "ai-usagebar-{}-{}.sh",
        std::process::id(),
        crate::tray::now_ms()
    ));
    let script = format!(
        "#!/bin/bash\n{line}\necho\nread -p 'Enter to close...'\nrm -f '{}'\n",
        tmp.display()
    );
    if fs::write(&tmp, script).is_err() {
        return;
    }
    let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755));
    let osa = format!(
        "tell application \"Terminal\" to do script \"bash '{}'; true\"\ntell application \"Terminal\" to activate",
        tmp.display()
    );
    let _ = Command::new("osascript").args(["-e", &osa]).spawn();
}

/// `path` as one single-quoted bash word, whatever characters it holds.
pub fn quote(path: &str) -> String {
    format!("'{}'", path.replace('\'', "'\\''"))
}

/// This project's binary `name`: beside the tray, then `~/.cargo/bin`, then `PATH`.
pub fn sibling(name: &str) -> String {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join(name);
        if sibling.is_file() {
            return sibling.to_string_lossy().into_owned();
        }
    }
    if let Ok(home) = crate::cache::home_dir() {
        let cargo = home.join(".cargo").join("bin").join(name);
        if cargo.is_file() {
            return cargo.to_string_lossy().into_owned();
        }
    }
    name.into()
}

#[cfg(test)]
mod tests {
    #[test]
    fn quote_keeps_shell_metacharacters_literal() {
        assert_eq!(super::quote("/A b/x"), "'/A b/x'");
        assert_eq!(super::quote("/it's $(x)"), "'/it'\\''s $(x)'");
    }
}
