//! Internal helpers shared between `adb` (reverse-tunnel transport) and
//! `wireless` (`adb pair` / `adb connect` setup). Factoring these into one
//! place keeps the Windows-specific subprocess hardening (`CREATE_NO_WINDOW`,
//! null-stdin, scoop-shim resolution) in a single audit surface.

use std::io;
use std::process::{Command, Output, Stdio};

/// `CREATE_NO_WINDOW` from `wincon.h`. We attach this flag to every adb
/// spawn because the Tauri GUI ships with `#![windows_subsystem = "windows"]`
/// (no parent console), so any console child without this flag pops a
/// black `cmd`-like window for a few hundred milliseconds.
#[cfg(windows)]
pub(crate) const CREATE_NO_WINDOW: u32 = 0x08000000;

/// Apply `CREATE_NO_WINDOW` to `cmd` on Windows; no-op elsewhere.
#[cfg(windows)]
pub(crate) fn silent(cmd: &mut Command) -> &mut Command {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(CREATE_NO_WINDOW)
}

#[cfg(not(windows))]
pub(crate) fn silent(cmd: &mut Command) -> &mut Command {
    cmd
}

/// Run `<adb_path> <args...>` synchronously and surface stderr in the
/// error path. Penflow's GUI binary has `windows_subsystem = "windows"`
/// in release, so the parent has no console and therefore no valid
/// stdin handle — explicitly null stdin so shim wrappers that try to
/// inherit it don't fail with `"Shim: Could not start the executable"`.
pub(crate) fn run_adb(adb_path: &str, args: &[&str]) -> io::Result<Output> {
    let mut cmd = Command::new(adb_path);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = silent(&mut cmd).output().map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "failed to invoke `{adb_path} {}`: {e}. Is adb on PATH?",
                args.join(" ")
            ),
        )
    })?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        return Err(io::Error::other(format!(
            "adb {} failed (status {:?}): stderr={stderr}; stdout={stdout}",
            args.join(" "),
            out.status.code()
        )));
    }
    Ok(out)
}

/// Resolve a `Command::new(name)` style target through scoop's shim layer.
///
/// scoop installs binaries under `~/scoop/apps/<name>/current/...` and
/// puts thin trampoline `name.exe` wrappers in `~/scoop/shims/`. Each
/// trampoline reads a sibling `name.shim` text file containing the real
/// target path, then `CreateProcessW`s it. The trampoline runs a PE
/// header peek to decide GUI-vs-console handle wiring; on failure it
/// logs `"Could not determine if target is a GUI app. Assuming console."`
/// and tries to inherit the parent's console handles. A
/// `windows_subsystem = "windows"` parent (Penflow's release build)
/// has no console, the inheritance call returns invalid, and the whole
/// CreateProcess fails with `"Could not create process"`.
///
/// Workaround: when we detect a `.shim` file next to the resolved
/// path, parse the `path = "..."` line out of it and use that
/// directly. The real adb.exe doesn't need a console.
///
/// On non-Windows platforms this is a no-op pass-through.
#[cfg(windows)]
pub(crate) fn resolve_through_shim(cmd: &str) -> String {
    let candidate: std::path::PathBuf = if cmd.contains('\\') || cmd.contains('/') {
        std::path::PathBuf::from(cmd)
    } else {
        let Some(path_var) = std::env::var_os("PATH") else {
            return cmd.to_string();
        };
        let mut found: Option<std::path::PathBuf> = None;
        'outer: for dir in std::env::split_paths(&path_var) {
            for ext in ["", ".exe", ".bat", ".cmd"] {
                let probe = dir.join(format!("{cmd}{ext}"));
                if probe.is_file() {
                    found = Some(probe);
                    break 'outer;
                }
            }
        }
        match found {
            Some(p) => p,
            None => return cmd.to_string(),
        }
    };

    let shim = candidate.with_extension("shim");
    if shim.is_file() {
        if let Ok(content) = std::fs::read_to_string(&shim) {
            for line in content.lines() {
                let line = line.trim();
                if let Some(rest) = line.strip_prefix("path") {
                    let after_eq = rest.trim_start().strip_prefix('=').unwrap_or("").trim();
                    let unquoted = after_eq.trim_matches('"');
                    if !unquoted.is_empty() && std::path::Path::new(unquoted).is_file() {
                        return unquoted.to_string();
                    }
                }
            }
        }
    }

    candidate.to_string_lossy().into_owned()
}

#[cfg(not(windows))]
pub(crate) fn resolve_through_shim(cmd: &str) -> String {
    cmd.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(windows))]
    #[test]
    fn pass_through_on_unix() {
        assert_eq!(resolve_through_shim("adb"), "adb");
    }

    #[cfg(windows)]
    #[test]
    fn parses_scoop_shim_config() {
        let tmp = std::env::temp_dir().join(format!("penflow-shim-test-{}", std::process::id()));
        let shims = tmp.join("shims");
        let real_dir = tmp.join("real");
        std::fs::create_dir_all(&shims).unwrap();
        std::fs::create_dir_all(&real_dir).unwrap();

        let trampoline = shims.join("foo.exe");
        let shim_cfg = shims.join("foo.shim");
        let real_target = real_dir.join("foo.exe");
        std::fs::write(&trampoline, b"trampoline-bytes").unwrap();
        std::fs::write(&real_target, b"real-bytes").unwrap();
        std::fs::write(&shim_cfg, format!("path = \"{}\"\n", real_target.display())).unwrap();

        let saved_path = std::env::var_os("PATH");
        std::env::set_var("PATH", shims.as_os_str());

        let resolved = resolve_through_shim("foo");
        match saved_path {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
        let _ = std::fs::remove_dir_all(&tmp);

        assert_eq!(resolved, real_target.to_string_lossy());
    }
}
