// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Platform-specific `zeph://` URI scheme registration (spec-066, TASK-6, TASK-10–13).
//!
//! Entry points:
//! - [`handle_url_scheme_register`] — write OS artefacts.
//! - [`handle_url_scheme_unregister`] — remove OS artefacts.
//! - [`handle_url_scheme_status`] — check registration state.

/// Register the `zeph://` URI scheme with the host OS.
///
/// On Linux, writes `~/.local/share/applications/zeph-url.desktop` and invokes
/// `xdg-mime` / `update-desktop-database`.  On macOS, prints manual instructions
/// and returns `Ok`.  On Windows, writes `HKCU\Software\Classes\zeph` registry keys.
///
/// # Errors
///
/// Returns an error only when an OS write operation fails unrecoverably (e.g.
/// filesystem permission denied).  Missing optional tools (`xdg-mime`) are handled
/// gracefully and do not cause a non-zero exit.
pub fn handle_url_scheme_register() -> anyhow::Result<()> {
    let exe = std::env::current_exe()
        .map_err(|e| anyhow::anyhow!("cannot determine current executable path: {e}"))?;
    let exe_str = exe.display().to_string();

    #[cfg(target_os = "linux")]
    {
        register_linux(&exe_str)?;
    }
    #[cfg(target_os = "macos")]
    {
        register_macos(&exe_str);
    }
    #[cfg(target_os = "windows")]
    {
        register_windows(&exe_str)?;
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        eprintln!("zeph url-scheme register: unsupported platform");
        eprintln!("Binary path: {exe_str}");
    }
    Ok(())
}

/// Remove the `zeph://` URI scheme registration from the host OS.
///
/// # Errors
///
/// Returns an error only when an OS removal operation fails unrecoverably.
// On macOS and unsupported platforms the body always succeeds; the Result return
// type is kept for a uniform call site across all platforms.
#[allow(clippy::unnecessary_wraps)]
pub fn handle_url_scheme_unregister() -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        unregister_linux()?;
    }
    #[cfg(target_os = "macos")]
    {
        unregister_macos();
    }
    #[cfg(target_os = "windows")]
    {
        unregister_windows()?;
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        eprintln!("zeph url-scheme unregister: unsupported platform");
    }
    Ok(())
}

/// Print the current `zeph://` scheme registration status to stdout and exit 0.
///
/// Reports whether the scheme artefacts exist and whether their registered binary
/// path matches the currently running binary.
pub fn handle_url_scheme_status() {
    let current_exe = std::env::current_exe().ok();

    #[cfg(target_os = "linux")]
    status_linux(current_exe.as_deref());

    #[cfg(target_os = "macos")]
    {
        let _ = current_exe;
        println!("macOS: manual registration only in v1");
        println!("To register, wrap the binary in a .app bundle with URL scheme handler.");
    }

    #[cfg(target_os = "windows")]
    status_windows(current_exe.as_deref());

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = current_exe;
        println!("zeph url-scheme status: unsupported platform");
    }
}

// ── Linux ────────────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn desktop_file_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_owned());
    std::path::PathBuf::from(home).join(".local/share/applications/zeph-url.desktop")
}

#[cfg(target_os = "linux")]
fn register_linux(exe_str: &str) -> anyhow::Result<()> {
    let desktop_path = desktop_file_path();
    let parent = desktop_path.parent().expect("desktop file path has parent");
    std::fs::create_dir_all(parent)?;

    let content = format!(
        "[Desktop Entry]\n\
         Name=Zeph URL Handler\n\
         Exec={exe_str} url-open \"%u\"\n\
         Type=Application\n\
         NoDisplay=true\n\
         MimeType=x-scheme-handler/zeph;\n"
    );
    std::fs::write(&desktop_path, content)?;
    println!("Wrote: {}", desktop_path.display());

    let xdg_result = std::process::Command::new("xdg-mime")
        .args(["default", "zeph-url.desktop", "x-scheme-handler/zeph"])
        .status();
    match xdg_result {
        Ok(s) if s.success() => println!("xdg-mime default: set"),
        Ok(s) => {
            println!(
                "xdg-mime exited with status {s}; run manually:\n  xdg-mime default zeph-url.desktop x-scheme-handler/zeph"
            );
        }
        Err(_) => {
            println!(
                "xdg-mime not found; run manually:\n  xdg-mime default zeph-url.desktop x-scheme-handler/zeph"
            );
        }
    }

    let udd_result = std::process::Command::new("update-desktop-database")
        .arg(parent)
        .status();
    match udd_result {
        Ok(s) if s.success() => println!("update-desktop-database: done"),
        Ok(s) => {
            println!(
                "update-desktop-database exited with status {s}; run manually:\n  update-desktop-database ~/.local/share/applications"
            );
        }
        Err(_) => {
            println!(
                "update-desktop-database not found; run manually:\n  update-desktop-database ~/.local/share/applications"
            );
        }
    }
    println!("Registered zeph:// scheme → {exe_str}");
    Ok(())
}

#[cfg(target_os = "linux")]
fn unregister_linux() -> anyhow::Result<()> {
    let desktop_path = desktop_file_path();
    if desktop_path.exists() {
        std::fs::remove_file(&desktop_path)?;
        println!("Removed: {}", desktop_path.display());
    } else {
        println!("Not registered (file not found)");
    }
    let parent = desktop_path.parent().expect("desktop file path has parent");
    let _ = std::process::Command::new("update-desktop-database")
        .arg(parent)
        .status();
    println!("Unregistered zeph:// scheme");
    Ok(())
}

#[cfg(target_os = "linux")]
fn status_linux(current_exe: Option<&std::path::Path>) {
    let desktop_path = desktop_file_path();
    if !desktop_path.exists() {
        println!("Status: not registered");
        return;
    }
    let content = match std::fs::read_to_string(&desktop_path) {
        Ok(c) => c,
        Err(e) => {
            println!("Status: file exists but cannot be read: {e}");
            return;
        }
    };
    // Parse Exec= line to find the registered binary path.
    let registered_exe = content
        .lines()
        .find(|l| l.starts_with("Exec="))
        .and_then(|l| l.strip_prefix("Exec="))
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or("<unknown>");

    if let Some(current) = current_exe {
        if current == std::path::Path::new(registered_exe) {
            println!("Status: registered → {registered_exe} (matches current binary)");
        } else {
            println!(
                "Status: registered → {registered_exe} (current binary: {})",
                current.display()
            );
        }
    } else {
        println!("Status: registered → {registered_exe}");
    }
}

// ── macOS ────────────────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn register_macos(exe_str: &str) {
    println!("macOS: manual registration only in v1");
    println!("Binary path: {exe_str}");
    println!(
        "To register, wrap the binary in a .app bundle with LSHandlerURLScheme = zeph.\n\
         See: https://developer.apple.com/documentation/bundleresources/information_property_list"
    );
}

#[cfg(target_os = "macos")]
fn unregister_macos() {
    println!("macOS: manual unregistration only in v1");
    println!("Remove the .app bundle or CFBundleURLTypes entry from Info.plist.");
}

// ── Windows ──────────────────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
fn register_windows(exe_str: &str) -> anyhow::Result<()> {
    use std::io::Write as _;

    // Use reg.exe to avoid pulling in the winreg crate for this v1 implementation.
    // The Windows approach for v1 is to shell out to reg.exe (always present on Windows).
    let base = "HKCU\\Software\\Classes\\zeph";
    let cmd_key = format!("{base}\\shell\\open\\command");
    let cmd_value = format!("\"{exe_str}\" url-open \"%1\"");

    fn run_reg(args: &[&str]) -> anyhow::Result<()> {
        let status = std::process::Command::new("reg")
            .args(args)
            .status()
            .map_err(|e| anyhow::anyhow!("reg.exe failed: {e}"))?;
        if !status.success() {
            anyhow::bail!("reg.exe exited with status {status}");
        }
        Ok(())
    }

    run_reg(&["add", base, "/ve", "/d", "URL:Zeph Protocol", "/f"])?;
    run_reg(&["add", base, "/v", "URL Protocol", "/d", "", "/f"])?;
    run_reg(&["add", &cmd_key, "/ve", "/d", &cmd_value, "/f"])?;

    println!("Registered zeph:// scheme → {exe_str}");
    Ok(())
}

#[cfg(target_os = "windows")]
fn unregister_windows() -> anyhow::Result<()> {
    let base = "HKCU\\Software\\Classes\\zeph";
    let status = std::process::Command::new("reg")
        .args(["delete", base, "/f"])
        .status()
        .map_err(|e| anyhow::anyhow!("reg.exe failed: {e}"))?;
    if status.success() {
        println!("Unregistered zeph:// scheme");
    } else {
        println!("reg delete exited with status {status} (key may not exist)");
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn status_windows(current_exe: Option<&std::path::Path>) {
    let key = "HKCU\\Software\\Classes\\zeph\\shell\\open\\command";
    let output = std::process::Command::new("reg")
        .args(["query", key, "/ve"])
        .output();

    match output {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            // Extract the default value from reg.exe output.
            let registered_cmd = text
                .lines()
                .find(|l| l.contains("REG_SZ"))
                .and_then(|l| l.splitn(3, "REG_SZ").nth(1))
                .map(str::trim)
                .unwrap_or("<unknown>");
            let registered_exe = registered_cmd
                .trim_start_matches('"')
                .split('"')
                .next()
                .unwrap_or(registered_cmd);
            if let Some(current) = current_exe {
                println!(
                    "Status: registered → {registered_exe} (current: {})",
                    current.display()
                );
            } else {
                println!("Status: registered → {registered_exe}");
            }
        }
        Ok(_) => println!("Status: not registered"),
        Err(e) => println!("Status: cannot query registry: {e}"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn handle_url_scheme_status_does_not_panic() {
        // Smoke test: status must complete without panicking regardless of registration state.
        super::handle_url_scheme_status();
    }

    #[test]
    fn handle_url_scheme_unregister_is_idempotent() {
        // Unregistering when nothing is registered must not return an error.
        let result = super::handle_url_scheme_unregister();
        assert!(
            result.is_ok(),
            "unregister should not fail when not registered: {result:?}"
        );
    }

    #[cfg(target_os = "linux")]
    mod linux {
        #![allow(unsafe_code)]

        use serial_test::serial;

        fn with_temp_home<F: FnOnce(&std::path::Path)>(f: F) {
            let tmp = tempfile::tempdir().expect("tempdir");
            let prev = std::env::var("HOME").ok();
            // SAFETY: single-threaded under #[serial]; restored unconditionally via catch_unwind.
            unsafe { std::env::set_var("HOME", tmp.path()) };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(tmp.path())));
            match &prev {
                Some(v) => unsafe { std::env::set_var("HOME", v) },
                None => unsafe { std::env::remove_var("HOME") },
            }
            if let Err(e) = result {
                std::panic::resume_unwind(e);
            }
        }

        #[test]
        #[serial]
        fn register_linux_writes_desktop_file() {
            with_temp_home(|home| {
                super::super::register_linux("fake-exe-path")
                    .expect("register_linux should succeed");
                let path = home.join(".local/share/applications/zeph-url.desktop");
                assert!(path.exists(), "desktop file must exist after register");
                let content = std::fs::read_to_string(&path).expect("read desktop file");
                assert!(
                    content.contains("Exec=fake-exe-path url-open \"%u\""),
                    "desktop file must contain correct Exec line; got:\n{content}"
                );
                assert!(
                    content.contains("MimeType=x-scheme-handler/zeph"),
                    "desktop file must declare MimeType; got:\n{content}"
                );
            });
        }

        #[test]
        #[serial]
        fn unregister_linux_removes_desktop_file() {
            with_temp_home(|home| {
                super::super::register_linux("fake-exe-path")
                    .expect("register_linux should succeed");
                let path = home.join(".local/share/applications/zeph-url.desktop");
                assert!(path.exists(), "desktop file must exist before unregister");
                super::super::unregister_linux().expect("unregister_linux should succeed");
                assert!(
                    !path.exists(),
                    "desktop file must be removed after unregister"
                );
            });
        }

        #[test]
        #[serial]
        fn unregister_linux_when_not_registered_is_ok() {
            with_temp_home(|_home| {
                let result = super::super::unregister_linux();
                assert!(
                    result.is_ok(),
                    "unregister_linux must succeed when no file exists: {result:?}"
                );
            });
        }

        #[test]
        #[serial]
        fn status_linux_parses_registered_exe() {
            with_temp_home(|_home| {
                super::super::register_linux("my-test-binary")
                    .expect("register_linux should succeed");
                // status_linux prints to stdout; verify it does not panic in either comparison branch.
                super::super::status_linux(Some(std::path::Path::new("my-test-binary")));
                super::super::status_linux(Some(std::path::Path::new("other-binary")));
                super::super::status_linux(None);
            });
        }

        #[test]
        #[serial]
        fn status_linux_when_not_registered() {
            with_temp_home(|_home| {
                super::super::status_linux(None);
            });
        }
    }
}
