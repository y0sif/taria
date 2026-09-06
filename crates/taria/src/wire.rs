//! Message contract for the taria transport.
//!
//! Apps and the bridge exchange these messages over a Unix domain socket as
//! newline-delimited JSON: each message is one JSON object serialized on a
//! single line, terminated by `\n`. `serde_json` never emits raw newlines
//! inside a compact object, so `serde_json::to_string` plus a trailing `\n`
//! is a valid frame. This module defines only the types; transports own the
//! actual I/O.
//!
//! Handshake: the first message an app sends after connecting is
//! [`AppToBridge::Hello`], carrying its label and
//! [`PROTOCOL_VERSION`](crate::PROTOCOL_VERSION) so the bridge can reject
//! incompatible peers. After that the app streams [`AppToBridge::Snapshot`]
//! messages, and the bridge sends [`BridgeToApp::Input`] whenever an agent
//! submits input. Every input carries an [`InputId`], and the app answers it
//! with an [`AppToBridge::Ack`] naming the same id, so the bridge can tell an
//! input the app acted on from one it never saw.
//!
//! # Compatibility
//!
//! Version 1 is the frozen format. Within version 1, changes must be additive,
//! and additive changes are safe because of two properties both peers already
//! rely on: a reader skips any line it cannot parse rather than dropping the
//! connection, and serde ignores unknown fields. So a new optional field on an
//! existing message, or an entirely new message variant, reaches an old peer as
//! something it quietly ignores while the rest of the stream keeps working.
//!
//! Anything else needs a [`PROTOCOL_VERSION`](crate::PROTOCOL_VERSION) bump:
//! removing a field, renaming one, making an optional field required, or
//! changing what an existing field or variant means. The last is the dangerous
//! one, because an old peer parses the message successfully and acts on the old
//! meaning.

use serde::{Deserialize, Serialize};

use crate::{AgentInput, Snapshot};

/// Identifier the bridge assigns to one input so the app's acknowledgement can
/// be matched back to it.
///
/// Unique within a single connection; a reconnect starts over.
pub type InputId = u64;

/// Message sent from a TUI app to the bridge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum AppToBridge {
    /// Handshake, sent once as the app's first message.
    Hello {
        /// Human-readable app name shown to agents (typically the binary name).
        app_label: String,
        /// The app's [`PROTOCOL_VERSION`](crate::PROTOCOL_VERSION).
        protocol_version: u32,
    },
    /// A newly published state of the app's semantic tree.
    Snapshot(Snapshot),
    /// What became of one [`BridgeToApp::Input`].
    ///
    /// Sent for every input received, so an agent waiting on an effect can tell
    /// "the app ignored this" from "the app has not got to it yet".
    Ack {
        /// The [`InputId`] of the input being answered.
        id: InputId,
        status: InputStatus,
    },
}

/// Fate of one input, reported back by the app.
///
/// One input may be acked more than once, and the last ack wins. An app acks
/// [`Delivered`](Self::Delivered) the moment its event loop dequeues the
/// input, before it knows what it will do with it, and may follow up with
/// [`Ignored`](Self::Ignored) once it turns out to have done nothing. Acks for
/// one input reach the bridge in the order the app sent them, so the newest
/// one received is the app's current answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputStatus {
    /// The app's event loop dequeued the input.
    Delivered,
    /// Dropped before reaching the app: its input queue was full.
    Dropped,
    /// The app looked at the input and deliberately did nothing (for
    /// example an act blocked by a modal dialog, or an unknown node id).
    Ignored,
}

/// Message sent from the bridge to a TUI app.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum BridgeToApp {
    /// Agent input to apply against the latest snapshot.
    Input {
        /// Identifier the app echoes in its [`AppToBridge::Ack`].
        id: InputId,
        input: AgentInput,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Action, Node, NodeId, PROTOCOL_VERSION, Role};

    fn roundtrip_app(msg: &AppToBridge) -> AppToBridge {
        let json = serde_json::to_string(msg).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    fn roundtrip_bridge(msg: &BridgeToApp) -> BridgeToApp {
        let json = serde_json::to_string(msg).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn hello_roundtrips_with_type_tag() {
        let msg = AppToBridge::Hello {
            app_label: "demo-app".into(),
            protocol_version: PROTOCOL_VERSION,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"hello""#), "json: {json}");
        assert_eq!(roundtrip_app(&msg), msg);
    }

    #[test]
    fn snapshot_message_roundtrips_with_type_tag() {
        let root = Node::new("root", Role::App)
            .label("demo")
            .focused(true)
            .child(
                Node::new("btn", Role::Button)
                    .label("Save")
                    .action(Action::Activate),
            );
        let msg = AppToBridge::Snapshot(Snapshot::new(7, root));
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"snapshot""#), "json: {json}");
        assert_eq!(roundtrip_app(&msg), msg);
    }

    #[test]
    fn input_messages_roundtrip_with_id() {
        let inputs = [
            AgentInput::Act {
                node: NodeId("btn".into()),
                action: Action::Custom("archive".into()),
                value: Some("v".into()),
            },
            AgentInput::Key {
                key: "ctrl+c".into(),
            },
            AgentInput::Text {
                text: "buy milk".into(),
            },
        ];
        for (id, input) in inputs.into_iter().enumerate() {
            let msg = BridgeToApp::Input {
                id: id as InputId,
                input,
            };
            let json = serde_json::to_string(&msg).unwrap();
            assert!(json.contains(r#""type":"input""#), "json: {json}");
            assert_eq!(roundtrip_bridge(&msg), msg);
        }
    }

    #[test]
    fn input_message_json_shape_is_stable() {
        let msg = BridgeToApp::Input {
            id: 7,
            input: AgentInput::Key { key: "q".into() },
        };
        assert_eq!(
            serde_json::to_string(&msg).unwrap(),
            r#"{"type":"input","id":7,"input":{"kind":"key","key":"q"}}"#
        );
    }

    #[test]
    fn ack_roundtrips_for_every_status() {
        let cases = [
            (InputStatus::Delivered, "delivered"),
            (InputStatus::Dropped, "dropped"),
            (InputStatus::Ignored, "ignored"),
        ];
        for (status, name) in cases {
            let msg = AppToBridge::Ack { id: 7, status };
            let json = serde_json::to_string(&msg).unwrap();
            assert_eq!(
                json,
                format!(r#"{{"type":"ack","id":7,"status":"{name}"}}"#)
            );
            assert_eq!(roundtrip_app(&msg), msg);
        }
    }

    #[test]
    fn unknown_fields_are_ignored_within_a_version() {
        // The compatibility rule this module documents: an old peer must parse
        // a message carrying a field added later.
        let json = r#"{"type":"ack","id":7,"status":"delivered","added_later":true}"#;
        let msg: AppToBridge = serde_json::from_str(json).unwrap();
        assert_eq!(
            msg,
            AppToBridge::Ack {
                id: 7,
                status: InputStatus::Delivered,
            }
        );
    }

    #[test]
    fn one_line_serialization_has_no_raw_newlines() {
        // Even labels containing newlines must serialize to a single line,
        // or ndjson framing breaks.
        let root = Node::new("root", Role::App)
            .label("line one\nline two")
            .value("a\nb");
        let msg = AppToBridge::Snapshot(Snapshot::new(1, root));
        let json = serde_json::to_string(&msg).unwrap();
        assert!(
            !json.contains('\n'),
            "ndjson frame contains raw newline: {json}"
        );
    }
}
