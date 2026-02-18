use std::time::Duration;

use expectrl::{Expect, Regex, session::OsSession, spawn};

fn spawn_tui() -> OsSession {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = std::path::Path::new(manifest_dir)
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let cmd = format!(
        "cargo run --manifest-path {}/Cargo.toml -- --tui",
        workspace_root.display()
    );
    let mut session = spawn(cmd).expect("failed to spawn TUI");
    session.set_expect_timeout(Some(Duration::from_secs(30)));
    session
}

#[test]
#[ignore] // requires full build + config
fn tui_shows_splash_on_startup() {
    let mut session = spawn_tui();
    session
        .expect(Regex("Type a message"))
        .expect("splash hint should appear");
    // Send Ctrl+C (0x03) to exit
    session.send("\x03").expect("send ctrl-c");
}

#[test]
#[ignore]
fn tui_quit_with_q() {
    let mut session = spawn_tui();
    session
        .expect(Regex("Type a message"))
        .expect("wait for splash");

    // Esc -> Normal mode, then 'q' to quit
    session.send("\x1b").expect("send esc");
    std::thread::sleep(Duration::from_millis(500));
    session.send("q").expect("send q");
    std::thread::sleep(Duration::from_millis(500));

    assert!(
        !session.get_process().is_alive().unwrap_or(true),
        "process should have exited"
    );
}

#[test]
#[ignore]
fn tui_ctrl_c_exits() {
    let mut session = spawn_tui();
    session
        .expect(Regex("Type a message"))
        .expect("wait for splash");

    session.send("\x03").expect("send ctrl-c");
    std::thread::sleep(Duration::from_millis(500));

    assert!(
        !session.get_process().is_alive().unwrap_or(true),
        "process should have exited on ctrl-c"
    );
}
