// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[allow(unused_imports)]
use super::*;

#[test]
fn detect_image_mime_jpeg() {
    assert_eq!(detect_image_mime(Some("photo.jpg")), "image/jpeg");
    assert_eq!(detect_image_mime(Some("photo.jpeg")), "image/jpeg");
}

#[test]
fn detect_image_mime_gif() {
    assert_eq!(detect_image_mime(Some("anim.gif")), "image/gif");
}

#[test]
fn detect_image_mime_webp() {
    assert_eq!(detect_image_mime(Some("img.webp")), "image/webp");
}

#[test]
fn detect_image_mime_unknown_defaults_png() {
    assert_eq!(detect_image_mime(Some("file.bmp")), "image/png");
    assert_eq!(detect_image_mime(None), "image/png");
}

#[tokio::test]
async fn resolve_message_extracts_image_attachment() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let agent = Agent::new(provider, channel, registry, None, 5, executor);

    let msg = ChannelMessage {
        text: "look at this".into(),
        attachments: vec![Attachment {
            kind: AttachmentKind::Image,
            data: vec![0u8; 16],
            filename: Some("test.jpg".into()),
        }],
        is_guest_context: false,
        is_from_bot: false,
    };
    let (text, parts) = agent.resolve_message(msg).await;
    assert_eq!(text, "look at this");
    assert_eq!(parts.len(), 1);
    match &parts[0] {
        zeph_llm::provider::MessagePart::Image(img) => {
            assert_eq!(img.mime_type, "image/jpeg");
            assert_eq!(img.data.len(), 16);
        }
        _ => panic!("expected Image part"),
    }
}

#[tokio::test]
async fn resolve_message_drops_oversized_image() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let agent = Agent::new(provider, channel, registry, None, 5, executor);

    let msg = ChannelMessage {
        text: "big image".into(),
        attachments: vec![Attachment {
            kind: AttachmentKind::Image,
            data: vec![0u8; MAX_IMAGE_BYTES + 1],
            filename: Some("huge.png".into()),
        }],
        is_guest_context: false,
        is_from_bot: false,
    };
    let (text, parts) = agent.resolve_message(msg).await;
    assert_eq!(text, "big image");
    assert!(parts.is_empty());
}

#[test]
fn handle_image_command_rejects_path_traversal() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    let result = agent.handle_image_as_string("../../etc/passwd");
    assert!(agent.msg.pending_image_parts.is_empty());
    assert!(result.contains("traversal"));
}

#[test]
fn handle_image_command_missing_file_sends_error() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    let result = agent.handle_image_as_string("nonexistent/image.png");
    assert!(agent.msg.pending_image_parts.is_empty());
    assert!(result.contains("Cannot read image"));
}

#[test]
fn handle_image_command_absolute_path_is_rejected() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    let result = agent.handle_image_as_string("/etc/passwd");
    assert!(agent.msg.pending_image_parts.is_empty());
    assert!(result.contains("path traversal not allowed"));
}

#[test]
fn handle_image_command_parent_dir_traversal_is_rejected() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    let result = agent.handle_image_as_string("../../etc/passwd");
    assert!(agent.msg.pending_image_parts.is_empty());
    assert!(result.contains("path traversal not allowed"));
}

#[test]
fn handle_image_command_loads_valid_file() {
    use std::io::Write as _;
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    // Use a temp dir under cwd so the resulting path can be made relative,
    // which is required by the path-traversal guard.
    let cwd = std::env::current_dir().unwrap();
    let tmp_dir = tempfile::TempDir::new_in(&cwd).unwrap();
    let file_path = tmp_dir.path().join("test.jpg");
    let data = vec![0xFFu8, 0xD8, 0xFF, 0xE0];
    std::fs::File::create(&file_path)
        .unwrap()
        .write_all(&data)
        .unwrap();
    let path = file_path
        .strip_prefix(&cwd)
        .unwrap_or(&file_path)
        .to_str()
        .unwrap()
        .to_owned();

    let result = agent.handle_image_as_string(&path);
    assert_eq!(agent.msg.pending_image_parts.len(), 1);
    match &agent.msg.pending_image_parts[0] {
        zeph_llm::provider::MessagePart::Image(img) => {
            assert_eq!(img.data, data);
            assert_eq!(img.mime_type, "image/jpeg");
        }
        _ => panic!("expected Image part"),
    }
    assert!(result.contains("Image loaded"));
}
