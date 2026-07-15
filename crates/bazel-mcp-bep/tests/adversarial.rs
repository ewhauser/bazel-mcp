use std::{fs, path::Path};

use bazel_mcp_bep::view::build_event as build_event_view;
use bazel_mcp_bep::{decode_stream_partial, encode_event_id, encode_frame, proto::*};
use buffa::Message;

#[test]
fn checked_adversarial_streams_cover_unknown_truncated_and_out_of_order_events() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    if std::env::var_os("UPDATE_GOLDENS").is_some() {
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("unknown-fields.bep"), unknown_field_stream()).unwrap();
        fs::write(root.join("truncated.bep"), truncated_stream()).unwrap();
        fs::write(root.join("nested-out-of-order.bep"), nested_stream()).unwrap();
    }

    let unknown = decode_stream_partial(
        fs::File::open(root.join("unknown-fields.bep")).unwrap(),
        1024 * 1024,
    );
    assert!(unknown.terminal_error.is_none());
    assert_eq!(unknown.events.len(), 1);
    assert!(matches!(
        unknown.events[0].view().payload,
        Some(build_event_view::Payload::Progress(_))
    ));

    let truncated = decode_stream_partial(
        fs::File::open(root.join("truncated.bep")).unwrap(),
        1024 * 1024,
    );
    assert_eq!(truncated.events.len(), 1);
    assert!(truncated.terminal_error.is_some());

    let nested = decode_stream_partial(
        fs::File::open(root.join("nested-out-of-order.bep")).unwrap(),
        1024 * 1024,
    );
    assert!(nested.terminal_error.is_none());
    assert_eq!(nested.events.len(), 3);
    assert!(matches!(
        nested.events[0].view().payload,
        Some(build_event_view::Payload::Completed(_))
    ));
    assert!(matches!(
        nested.events[2].view().payload,
        Some(build_event_view::Payload::NamedSetOfFiles(_))
    ));
}

fn progress_event(text: &str) -> BuildEvent {
    BuildEvent {
        payload: Some(build_event::Payload::Progress(Box::new(Progress {
            stdout: text.to_owned(),
            stderr: String::new(),
        }))),
        ..Default::default()
    }
}

fn unknown_field_stream() -> Vec<u8> {
    let mut body = progress_event("known payload").encode_to_vec();
    // Field 100, length-delimited, contains data unknown to our pinned subset.
    body.extend_from_slice(&[0xa2, 0x06, 0x03, b'n', b'e', b'w']);
    frame_body(&body)
}

fn truncated_stream() -> Vec<u8> {
    let mut stream = encode_frame(&progress_event("complete"));
    let mut truncated = encode_frame(&progress_event("truncated"));
    truncated.truncate(truncated.len().saturating_sub(3));
    stream.extend(truncated);
    stream
}

fn nested_stream() -> Vec<u8> {
    let named_id = |id: &str| build_event_id::NamedSetOfFilesId { id: id.into() };
    let event_id = |id: &str| {
        encode_event_id(&BuildEventId {
            id: Some(build_event_id::Id::NamedSet(Box::new(named_id(id)))),
        })
    };
    let completed = BuildEvent {
        payload: Some(build_event::Payload::Completed(Box::new(TargetComplete {
            output_group: vec![OutputGroup {
                name: "default".into(),
                file_sets: vec![named_id("root")],
                ..Default::default()
            }],
            ..Default::default()
        }))),
        ..Default::default()
    };
    let child = BuildEvent {
        id: event_id("child"),
        payload: Some(build_event::Payload::NamedSetOfFiles(Box::new(
            NamedSetOfFiles {
                files: vec![File {
                    name: "remote.out".into(),
                    file: Some(file::File::Uri("bytestream://cache/abc/10".into())),
                    length: 10,
                    ..Default::default()
                }],
                file_sets: vec![named_id("root")],
            },
        ))),
        ..Default::default()
    };
    let root = BuildEvent {
        id: event_id("root"),
        payload: Some(build_event::Payload::NamedSetOfFiles(Box::new(
            NamedSetOfFiles {
                files: vec![File {
                    name: "local.out".into(),
                    file: Some(file::File::Uri("file://<WORKSPACE>/local.out".into())),
                    length: 5,
                    ..Default::default()
                }],
                file_sets: vec![named_id("child")],
            },
        ))),
        ..Default::default()
    };
    [completed, child, root]
        .iter()
        .flat_map(encode_frame)
        .collect()
}

fn frame_body(body: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    let mut length = body.len() as u64;
    while length >= 0x80 {
        output.push((length as u8 & 0x7f) | 0x80);
        length >>= 7;
    }
    output.push(length as u8);
    output.extend_from_slice(body);
    output
}
