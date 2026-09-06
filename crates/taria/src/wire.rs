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
//! submits input.

use serde::{Deserialize, Serialize};

use crate::{AgentInput, Snapshot};

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
}

/// Message sent from the bridge to a TUI app.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum BridgeToApp {
    /// Agent input to apply against the latest snapshot.
    Input(AgentInput),
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
    fn input_messages_roundtrip_with_type_tag() {
        let inputs = [
            AgentInput::Act {
                node: NodeId("btn".into()),
                action: Action::Custom("archive".into()),
                value: Some("v".into()),
            },
            AgentInput::Key {
                key: "ctrl+c".into(),
            },
        ];
        for input in inputs {
            let msg = BridgeToApp::Input(input);
            let json = serde_json::to_string(&msg).unwrap();
            assert!(json.contains(r#""type":"input""#), "json: {json}");
            assert_eq!(roundtrip_bridge(&msg), msg);
        }
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
