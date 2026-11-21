//! Locating external programs. GUI apps launched from Finder do not inherit
//! the shell `PATH`, so Homebrew and `MacPorts` locations are searched too.

use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Directories searched after `PATH`.
pub const EXTRA_DIRS: [&str; 3] = ["/opt/homebrew/bin", "/usr/local/bin", "/opt/local/bin"];

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// Find `name` in `path_var` (a `PATH`-style list), then in `extra`.
#[must_use]
pub fn find_in(name: &str, path_var: Option<&OsStr>, extra: &[&Path]) -> Option<PathBuf> {
    let from_path: Vec<PathBuf> = path_var
        .map(|p| env::split_paths(p).collect())
        .unwrap_or_default();
    from_path
        .iter()
        .map(PathBuf::as_path)
        .chain(extra.iter().copied())
        .map(|d| d.join(name))
        .find(|c| is_executable(c))
}

/// Find a program on `PATH` or in the usual Homebrew/MacPorts folders.
#[must_use]
pub fn find_tool(name: &str) -> Option<PathBuf> {
    let extra: Vec<&Path> = EXTRA_DIRS.iter().map(Path::new).collect();
    find_in(name, env::var_os("PATH").as_deref(), &extra)
}

/// The user's home directory.
#[must_use]
pub fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn finds_in_path_then_extra() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let exe = b.path().join("tool");
        fs::write(&exe, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            find_in("tool", Some(a.path().as_os_str()), &[b.path()]),
            Some(exe.clone())
        );
        assert_eq!(find_in("tool", Some(b.path().as_os_str()), &[]), Some(exe));
        assert_eq!(find_in("tool", Some(a.path().as_os_str()), &[]), None);
        // Non-executable files are ignored.
        let plain = a.path().join("plain");
        fs::write(&plain, "x").unwrap();
        assert_eq!(find_in("plain", Some(a.path().as_os_str()), &[]), None);
    }
}
