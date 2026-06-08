// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Platform-specific `zeph://` URI scheme registration (spec-066, TASK-6, TASK-10–13, TASK-14, TASK-16).
//!
//! Entry points:
//! - [`handle_url_scheme_register`] — write OS artefacts.
//! - [`handle_url_scheme_unregister`] — remove OS artefacts.
//! - [`handle_url_scheme_status`] — print registration state; returns `true` when stale/missing.
//! - [`scheme_registration_status`] — machine-readable registration check used by `zeph doctor`.

/// Machine-readable registration state returned by [`scheme_registration_status`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemeStatus {
    /// Registered and the binary path matches the current executable.
    Ok,
    /// No registration artefacts found on disk.
    NotRegistered,
    /// Registration exists but is stale (binary gone or path mismatch).
    Stale(String),
}

/// Return the machine-readable `zeph://` scheme registration status for the given executable.
///
/// This is the same logic as [`handle_url_scheme_status`] but returns a typed value instead of
/// printing, making it suitable for use in `zeph doctor` and scripting scenarios.
///
/// # Examples
///
/// ```no_run
/// use zeph::url_scheme::register::{scheme_registration_status, SchemeStatus};
///
/// let current = std::env::current_exe().ok();
/// match scheme_registration_status(current.as_deref()) {
///     SchemeStatus::Ok => println!("registration is current"),
///     SchemeStatus::NotRegistered => println!("not registered"),
///     SchemeStatus::Stale(reason) => eprintln!("stale: {reason}"),
/// }
/// ```
pub fn scheme_registration_status(current_exe: Option<&std::path::Path>) -> SchemeStatus {
    #[cfg(target_os = "linux")]
    {
        scheme_status_linux(current_exe)
    }

    #[cfg(target_os = "macos")]
    {
        scheme_status_macos(current_exe)
    }

    #[cfg(target_os = "windows")]
    {
        scheme_status_windows(current_exe)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = current_exe;
        SchemeStatus::NotRegistered
    }
}

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
        register_macos(&exe_str)?;
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
pub fn handle_url_scheme_unregister() -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        unregister_linux()?;
    }
    #[cfg(target_os = "macos")]
    {
        unregister_macos()?;
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

/// Print the current `zeph://` scheme registration status to stdout.
///
/// Returns `true` when the registration is stale (registered path does not match the current
/// executable or the registered binary no longer exists on disk).
/// Returns `false` when registration is healthy or when the scheme has never been registered.
///
/// Use this return value to gate `--check` exit codes: a never-registered machine is not an
/// error, but a stale registration indicates the binary has moved and may mislead the OS.
///
/// # Examples
///
/// ```no_run
/// let stale = zeph::url_scheme::register::handle_url_scheme_status();
/// if stale {
///     eprintln!("url-scheme registration is stale; re-run `zeph url-scheme register`");
///     std::process::exit(1);
/// }
/// ```
pub fn handle_url_scheme_status() -> bool {
    let current_exe = std::env::current_exe().ok();

    #[cfg(target_os = "linux")]
    {
        status_linux(current_exe.as_deref())
    }

    #[cfg(target_os = "macos")]
    {
        status_macos(current_exe.as_deref())
    }

    #[cfg(target_os = "windows")]
    {
        status_windows(current_exe.as_deref())
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = current_exe;
        println!("zeph url-scheme status: unsupported platform");
        false
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
fn status_linux(current_exe: Option<&std::path::Path>) -> bool {
    let desktop_path = desktop_file_path();
    if !desktop_path.exists() {
        println!("Status: not registered");
        return false;
    }
    let content = match std::fs::read_to_string(&desktop_path) {
        Ok(c) => c,
        Err(e) => {
            println!("Status: file exists but cannot be read: {e}");
            return true;
        }
    };
    // Parse Exec= line to find the registered binary path.
    let registered_exe = content
        .lines()
        .find(|l| l.starts_with("Exec="))
        .and_then(|l| l.strip_prefix("Exec="))
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or("<unknown>");

    let registered_path = std::path::Path::new(registered_exe);
    let binary_missing = !registered_path.exists();

    if let Some(current) = current_exe {
        let stale = binary_missing || current != registered_path;
        if !stale {
            println!("Status: registered → {registered_exe} (matches current binary)");
        } else if binary_missing {
            println!("Status: registered → {registered_exe} (binary not found on disk, stale)");
        } else {
            println!(
                "Status: stale — registered: {registered_exe}, current: {}",
                current.display()
            );
        }
        stale
    } else {
        println!("Status: registered → {registered_exe}");
        binary_missing
    }
}

#[cfg(target_os = "linux")]
fn scheme_status_linux(current_exe: Option<&std::path::Path>) -> SchemeStatus {
    let desktop_path = desktop_file_path();
    if !desktop_path.exists() {
        return SchemeStatus::NotRegistered;
    }
    let content = match std::fs::read_to_string(&desktop_path) {
        Ok(c) => c,
        Err(e) => return SchemeStatus::Stale(format!("cannot read desktop file: {e}")),
    };
    let registered_exe = content
        .lines()
        .find(|l| l.starts_with("Exec="))
        .and_then(|l| l.strip_prefix("Exec="))
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or("<unknown>");
    let registered_path = std::path::Path::new(registered_exe);
    if !registered_path.exists() {
        return SchemeStatus::Stale(format!(
            "registered binary not found on disk: {registered_exe}"
        ));
    }
    if let Some(current) = current_exe
        && current != registered_path
    {
        return SchemeStatus::Stale(format!(
            "registered: {registered_exe}, current: {}",
            current.display()
        ));
    }
    SchemeStatus::Ok
}

// ── macOS ────────────────────────────────────────────────────────────────────

/// Path to the Zeph.app bundle in ~/Applications.
#[cfg(target_os = "macos")]
fn app_bundle_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_owned());
    std::path::PathBuf::from(home).join("Applications/Zeph.app")
}

/// Escape a string for embedding inside an XML `<string>` element.
#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The Info.plist content with `CFBundleURLTypes` entry for the `zeph://` scheme.
#[cfg(target_os = "macos")]
fn info_plist_content(exe_str: &str) -> String {
    let escaped = xml_escape(exe_str);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>zeph</string>
    <key>CFBundleIdentifier</key>
    <string>com.zeph.url-handler</string>
    <key>CFBundleName</key>
    <string>Zeph</string>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleURLTypes</key>
    <array>
        <dict>
            <key>CFBundleURLName</key>
            <string>Zeph URL</string>
            <key>CFBundleURLSchemes</key>
            <array>
                <string>zeph</string>
            </array>
        </dict>
    </array>
    <key>LSUIElement</key>
    <true/>
    <key>_ZephExePath</key>
    <string>{escaped}</string>
</dict>
</plist>
"#
    )
}

#[cfg(target_os = "macos")]
fn register_macos(exe_str: &str) -> anyhow::Result<()> {
    let bundle = app_bundle_path();
    let macos_dir = bundle.join("Contents/MacOS");
    let plist_path = bundle.join("Contents/Info.plist");
    let symlink_path = macos_dir.join("zeph");

    std::fs::create_dir_all(&macos_dir)?;
    std::fs::write(&plist_path, info_plist_content(exe_str))?;

    // Symlink to the real binary so LaunchServices can find the handler.
    // TODO(#5014): macOS bundle receives zeph:// URLs via Apple Event, not argv;
    // Phase 3 must wire the Apple Event handler through handle_url_open to preserve INV-TRUST.
    if symlink_path.exists() || symlink_path.is_symlink() {
        std::fs::remove_file(&symlink_path)?;
    }
    std::os::unix::fs::symlink(exe_str, &symlink_path)?;

    println!("Wrote: {}", bundle.display());

    // Register with LaunchServices.
    let ls_result = std::process::Command::new("/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister")
        .args(["-f", &bundle.to_string_lossy()])
        .status();
    match ls_result {
        Ok(s) if s.success() => println!("lsregister: registered"),
        Ok(s) => println!("lsregister exited with status {s}; registration may be incomplete"),
        Err(e) => println!("lsregister not found or failed: {e}; registration may be incomplete"),
    }

    println!("Registered zeph:// scheme → {exe_str}");
    Ok(())
}

#[cfg(target_os = "macos")]
fn unregister_macos() -> anyhow::Result<()> {
    let bundle = app_bundle_path();
    if !bundle.exists() {
        println!("Not registered (bundle not found: {})", bundle.display());
        return Ok(());
    }

    // Unregister from LaunchServices before removing files.
    let ls_result = std::process::Command::new("/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister")
        .args(["-u", &bundle.to_string_lossy()])
        .status();
    match ls_result {
        Ok(s) if s.success() => println!("lsregister: unregistered"),
        Ok(s) => println!("lsregister -u exited with status {s}"),
        Err(e) => println!("lsregister not found or failed: {e}"),
    }

    std::fs::remove_dir_all(&bundle)?;
    println!("Removed: {}", bundle.display());
    println!("Unregistered zeph:// scheme");
    Ok(())
}

#[cfg(target_os = "macos")]
fn status_macos(current_exe: Option<&std::path::Path>) -> bool {
    let bundle = app_bundle_path();
    let plist_path = bundle.join("Contents/Info.plist");

    if !bundle.exists() {
        println!("Status: not registered (bundle not found)");
        return false;
    }

    // Extract the exe path stored in Info.plist via the _ZephExePath key.
    let registered_exe = match std::fs::read_to_string(&plist_path) {
        Ok(content) => extract_zeph_exe_from_plist(&content),
        Err(e) => {
            println!("Status: bundle exists but Info.plist cannot be read: {e}");
            return true;
        }
    };

    let Some(exe_str) = registered_exe else {
        println!("Status: bundle exists but _ZephExePath not found in Info.plist");
        return true;
    };

    let registered_path = std::path::Path::new(&exe_str);
    let binary_missing = !registered_path.exists();

    if let Some(current) = current_exe {
        let stale = binary_missing || current != registered_path;
        if !stale {
            println!("Status: registered → {exe_str} (matches current binary)");
        } else if binary_missing {
            println!("Status: registered → {exe_str} (binary not found on disk, stale)");
        } else {
            println!(
                "Status: stale — registered: {exe_str}, current: {}",
                current.display()
            );
        }
        stale
    } else {
        println!("Status: registered → {exe_str}");
        binary_missing
    }
}

/// Extract the value of `<key>_ZephExePath</key><string>...</string>` from a plist.
///
/// This is a minimal text-based parser — sufficient for the fixed template we write.
/// It is not a general plist parser.
#[cfg(target_os = "macos")]
fn extract_zeph_exe_from_plist(content: &str) -> Option<String> {
    let key_pos = content.find("<key>_ZephExePath</key>")?;
    let after_key = &content[key_pos + "<key>_ZephExePath</key>".len()..];
    let start = after_key.find("<string>")? + "<string>".len();
    let end = after_key[start..].find("</string>")?;
    let raw = after_key[start..start + end].trim();
    // Unescape the XML entities written by xml_escape() at registration time.
    Some(
        raw.replace("&amp;", "&")
            .replace("&lt;", "<")
            .replace("&gt;", ">"),
    )
}

#[cfg(target_os = "macos")]
fn scheme_status_macos(current_exe: Option<&std::path::Path>) -> SchemeStatus {
    let bundle = app_bundle_path();
    let plist_path = bundle.join("Contents/Info.plist");
    if !bundle.exists() {
        return SchemeStatus::NotRegistered;
    }
    let content = match std::fs::read_to_string(&plist_path) {
        Ok(c) => c,
        Err(e) => return SchemeStatus::Stale(format!("cannot read Info.plist: {e}")),
    };
    let Some(exe_str) = extract_zeph_exe_from_plist(&content) else {
        return SchemeStatus::Stale("_ZephExePath not found in Info.plist".to_owned());
    };
    let registered_path = std::path::Path::new(&exe_str);
    if !registered_path.exists() {
        return SchemeStatus::Stale(format!("registered binary not found on disk: {exe_str}"));
    }
    if let Some(current) = current_exe.filter(|&c| c != registered_path) {
        return SchemeStatus::Stale(format!(
            "registered: {exe_str}, current: {}",
            current.display()
        ));
    }
    SchemeStatus::Ok
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
fn status_windows(current_exe: Option<&std::path::Path>) -> bool {
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
            let registered_path = std::path::Path::new(registered_exe);
            let binary_missing = !registered_path.exists();
            if let Some(current) = current_exe {
                let stale = binary_missing || current != registered_path;
                if !stale {
                    println!("Status: registered → {registered_exe} (matches current binary)");
                } else if binary_missing {
                    println!(
                        "Status: registered → {registered_exe} (binary not found on disk, stale)"
                    );
                } else {
                    println!(
                        "Status: stale — registered: {registered_exe}, current: {}",
                        current.display()
                    );
                }
                stale
            } else {
                println!("Status: registered → {registered_exe}");
                binary_missing
            }
        }
        Ok(_) => {
            println!("Status: not registered");
            false
        }
        Err(e) => {
            println!("Status: cannot query registry: {e}");
            true
        }
    }
}

#[cfg(target_os = "windows")]
fn scheme_status_windows(current_exe: Option<&std::path::Path>) -> SchemeStatus {
    let key = "HKCU\\Software\\Classes\\zeph\\shell\\open\\command";
    let output = std::process::Command::new("reg")
        .args(["query", key, "/ve"])
        .output();

    match output {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
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
            let registered_path = std::path::Path::new(registered_exe);
            if !registered_path.exists() {
                return SchemeStatus::Stale(format!(
                    "registered binary not found on disk: {registered_exe}"
                ));
            }
            if let Some(current) = current_exe {
                if current != registered_path {
                    return SchemeStatus::Stale(format!(
                        "registered: {registered_exe}, current: {}",
                        current.display()
                    ));
                }
            }
            SchemeStatus::Ok
        }
        Ok(_) => SchemeStatus::NotRegistered,
        Err(e) => SchemeStatus::Stale(format!("cannot query registry: {e}")),
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

        #[test]
        #[serial]
        fn scheme_status_linux_not_registered_when_no_file() {
            with_temp_home(|_home| {
                let status = super::super::scheme_status_linux(None);
                assert_eq!(status, super::super::SchemeStatus::NotRegistered);
            });
        }

        #[test]
        #[serial]
        fn scheme_status_linux_ok_when_registered_and_current() {
            with_temp_home(|_home| {
                // Use the current test binary as the "registered" exe so the path exists.
                let current = std::env::current_exe().expect("current_exe");
                let exe_str = current.to_string_lossy().to_string();
                super::super::register_linux(&exe_str).expect("register_linux");
                let status = super::super::scheme_status_linux(Some(&current));
                assert_eq!(status, super::super::SchemeStatus::Ok);
            });
        }

        #[test]
        #[serial]
        fn scheme_status_linux_stale_when_binary_path_differs() {
            with_temp_home(|_home| {
                let current = std::env::current_exe().expect("current_exe");
                super::super::register_linux("/nonexistent/other-binary").expect("register_linux");
                let status = super::super::scheme_status_linux(Some(&current));
                assert!(
                    matches!(status, super::super::SchemeStatus::Stale(_)),
                    "expected Stale, got {status:?}"
                );
            });
        }
    }

    #[cfg(target_os = "macos")]
    mod macos {
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
        fn extract_zeph_exe_from_plist_parses_correctly() {
            let plist = super::super::info_plist_content("/usr/local/bin/zeph");
            let result = super::super::extract_zeph_exe_from_plist(&plist);
            assert_eq!(result.as_deref(), Some("/usr/local/bin/zeph"));
        }

        #[test]
        fn extract_zeph_exe_from_plist_returns_none_when_key_absent() {
            let result = super::super::extract_zeph_exe_from_plist("<plist></plist>");
            assert!(result.is_none());
        }

        #[test]
        fn extract_zeph_exe_from_plist_round_trips_path_with_ampersand() {
            let path = "/home/user/a&b/zeph";
            let plist = super::super::info_plist_content(path);
            let result = super::super::extract_zeph_exe_from_plist(&plist);
            assert_eq!(result.as_deref(), Some(path));
        }

        #[test]
        #[serial]
        fn register_macos_writes_bundle() {
            with_temp_home(|home| {
                let current = std::env::current_exe().expect("current_exe");
                let exe_str = current.to_string_lossy().to_string();
                super::super::register_macos(&exe_str).expect("register_macos should succeed");

                let bundle = home.join("Applications/Zeph.app");
                assert!(bundle.exists(), "bundle must exist after register");

                let plist = bundle.join("Contents/Info.plist");
                assert!(plist.exists(), "Info.plist must exist");

                let content = std::fs::read_to_string(&plist).expect("read plist");
                assert!(
                    content.contains("<string>zeph</string>"),
                    "plist must contain scheme"
                );
                assert!(content.contains(&exe_str), "plist must contain exe path");

                let symlink = bundle.join("Contents/MacOS/zeph");
                assert!(
                    symlink.exists() || symlink.is_symlink(),
                    "symlink must exist"
                );
            });
        }

        #[test]
        #[serial]
        fn unregister_macos_removes_bundle() {
            with_temp_home(|home| {
                let current = std::env::current_exe().expect("current_exe");
                let exe_str = current.to_string_lossy().to_string();
                super::super::register_macos(&exe_str).expect("register_macos");

                let bundle = home.join("Applications/Zeph.app");
                assert!(bundle.exists(), "bundle must exist before unregister");

                super::super::unregister_macos().expect("unregister_macos should succeed");
                assert!(!bundle.exists(), "bundle must be removed after unregister");
            });
        }

        #[test]
        #[serial]
        fn unregister_macos_when_not_registered_is_ok() {
            with_temp_home(|_home| {
                let result = super::super::unregister_macos();
                assert!(
                    result.is_ok(),
                    "unregister when no bundle must succeed: {result:?}"
                );
            });
        }

        #[test]
        #[serial]
        fn scheme_status_macos_not_registered_when_no_bundle() {
            with_temp_home(|_home| {
                let status = super::super::scheme_status_macos(None);
                assert_eq!(status, super::super::SchemeStatus::NotRegistered);
            });
        }

        #[test]
        #[serial]
        fn scheme_status_macos_ok_when_registered_and_current() {
            with_temp_home(|_home| {
                let current = std::env::current_exe().expect("current_exe");
                let exe_str = current.to_string_lossy().to_string();
                super::super::register_macos(&exe_str).expect("register_macos");
                let status = super::super::scheme_status_macos(Some(&current));
                assert_eq!(status, super::super::SchemeStatus::Ok);
            });
        }

        #[test]
        #[serial]
        fn scheme_status_macos_stale_when_binary_missing() {
            with_temp_home(|_home| {
                super::super::register_macos("/nonexistent/missing-binary")
                    .expect("register_macos");
                let status = super::super::scheme_status_macos(None);
                assert!(
                    matches!(status, super::super::SchemeStatus::Stale(_)),
                    "expected Stale for missing binary, got {status:?}"
                );
            });
        }
    }
}
