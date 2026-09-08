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
//! [`PROTOCOL_VERSION`](crate::PROTOCOL_VERSION). Nothing in this crate acts
//! on that number; what a mismatch costs is decided by the transport. Peers on
//! different versions disagree about the shape of every message, so a v0 app
//! fails to parse each input a v1 bridge sends it and skips them all, which is
//! why the reference bridge (`taria-mcp`) splits its tool surface by
//! direction on a mismatch: it keeps the connection and logs a warning,
//! reading the tree still works for as long as the peer's snapshots parse,
//! and every tool that sends input refuses up front with an error naming both
//! versions, having sent nothing. Reading is the common case, not a promise:
//! the changes that need a bump are exactly the ones that break parsing, so a
//! peer whose [`Snapshot`] shape moved leaves the reader with no tree at all.
//! A bridge that forwards input across a mismatch instead
//! writes into a peer that cannot parse it, and leaves the agent reading "it
//! may not have reacted yet" for a session that can never react. A mismatch is
//! something to fix before use, not something to note.
//!
//! After the handshake the app streams [`AppToBridge::Snapshot`] messages, and
//! the bridge sends [`BridgeToApp::Input`] whenever an agent submits input.
//! Every input carries an [`InputId`], and the app answers it with an
//! [`AppToBridge::Ack`] naming the same id, so the bridge can tell an input the
//! app acted on from one it never saw.
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
//! Neither property covers a value nested inside a message the peer does want,
//! which is why the vocabularies carry their own fallbacks: an unknown
//! [`Role`](crate::Role) reads as [`Role::Other`](crate::Role::Other) and an
//! unknown [`Action`](crate::Action) name as
//! [`Action::Custom`](crate::Action::Custom), keeping its name. A role or an
//! action added later therefore costs one degraded field on one node instead of
//! the whole [`Snapshot`] that node sits in. Without that, a single leaf makes
//! every tree unparseable, the reader skips each one and goes on serving its
//! last good tree, and the app looks to an agent like it has stopped
//! responding. New values in those two vocabularies are additive. An
//! [`InputStatus`] has no such fallback, and its own documentation says why
//! that one is safe.
//!
//! Both fallbacks are hand-written deserializers that call `deserialize_any`,
//! so [`Role`](crate::Role) and [`Action`](crate::Action) decode from
//! self-describing formats only. The transport is ndjson, which costs this
//! nothing, but a format that needs the type to know what it is reading
//! (bincode and its relatives) cannot carry these two types.
//!
//! Additive on the wire is not automatically additive in Rust. A peer skips a
//! message variant it cannot parse, but a peer *rebuilt* against the version
//! that added one meets it in an exhaustive `match` and stops compiling. The
//! peers this format is written for are exactly the ones that match on these
//! types: an adapter dispatching [`BridgeToApp`], a bridge dispatching
//! [`AppToBridge`] and reading an [`InputStatus`]. So every type a version-1
//! addition can reach is `#[non_exhaustive]`: those three, [`AgentInput`],
//! [`Action`](crate::Action), [`Key`](crate::key::Key),
//! [`Modifiers`](crate::key::Modifiers), [`Role`](crate::Role),
//! [`Node`](crate::Node) and [`Snapshot`]. Marking them is itself a breaking
//! change, which is why it is done before the first release rather than at the
//! first addition. Each one costs an outside peer one wildcard arm (or one
//! `..` in a pattern), and buys back that a new variant, field, role, action
//! or key is a recompile rather than a repair.
//!
//! The same reasoning reaches one level further in. A new optional field on an
//! existing message is the format's cheapest additive change and is also the
//! one that breaks an outside peer hardest, because it breaks everything that
//! builds or destructures that message. So the struct-like variants carry the
//! attribute as well: [`AppToBridge::Hello`], [`AppToBridge::Ack`],
//! [`BridgeToApp::Input`], and [`Act`](AgentInput::Act),
//! [`Key`](AgentInput::Key) and [`Text`](AgentInput::Text) on [`AgentInput`].
//! For a peer that means two habits: build them with the constructor beside
//! each one, since a marked variant has no struct literal from outside, and
//! end a destructuring pattern with `..`.
//! [`AppToBridge::Snapshot`] needs neither. It is a
//! newtype around [`Snapshot`], which is marked already and built by
//! [`Snapshot::new`](crate::Snapshot::new).
//!
//! An unknown [`AgentInput`] kind degrades the same way a role or an action
//! does, to [`AgentInput::Unknown`], and for a sharper reason than either: the
//! [`InputId`] sits outside the input, on the message, so the fallback is what
//! turns "an input this build cannot read" into an ack the agent receives
//! rather than a skipped line it waits out.
//!
//! # Tree depth
//!
//! A snapshot is one JSON object, and JSON parsers bound how far they recurse
//! into one, so there is a depth past which a tree stops arriving at all. It
//! arrives as nothing rather than as an error: the reader skips the line the
//! way it skips a truncated one, and goes on serving its last good tree.
//! [`MAX_NODE_DEPTH`](crate::MAX_NODE_DEPTH) is the bound both peers can state
//! and rely on, with the measurement it comes from;
//! [`Node::check_depth`](crate::Node::check_depth) is what an adapter calls
//! before publishing.
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
/// A bridge must not reuse an id for the lifetime of its process, not merely
/// for the lifetime of one connection. Acks outlive the connection they were
/// sent on: an app can ack an input, then die, and that ack can still be in
/// flight while the bridge is already serving a waiter on the next connection.
/// Ids that never repeat make such an ack impossible to mistake for the answer
/// to a live input, so a bridge that restarts its ids per connection can
/// resolve a waiter with a dead app's verdict. A monotonic counter owned by the
/// process satisfies this.
pub type InputId = u64;

/// Message sent from a TUI app to the bridge.
///
/// `#[non_exhaustive]` because this module's own compatibility rule calls a new
/// message variant additive, and that is only true on the wire: a bridge
/// matches on this enum to dispatch every line it reads, so without the
/// attribute the additive change breaks its build.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
#[non_exhaustive]
pub enum AppToBridge {
    /// Handshake, sent once as the app's first message.
    ///
    /// `#[non_exhaustive]`: build it with [`hello`](Self::hello) and
    /// destructure it with `..`, so a field added later (a capability list, a
    /// process id) does not break the peer reading it.
    #[non_exhaustive]
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
    ///
    /// `#[non_exhaustive]`: build it with [`ack`](Self::ack) and destructure it
    /// with `..`.
    #[non_exhaustive]
    Ack {
        /// The [`InputId`] of the input being answered.
        id: InputId,
        status: InputStatus,
    },
}

impl AppToBridge {
    /// Build the [`Hello`](Self::Hello) handshake.
    ///
    /// The version is a parameter rather than filled in from
    /// [`PROTOCOL_VERSION`](crate::PROTOCOL_VERSION), unlike
    /// [`Snapshot::new`](crate::Snapshot::new), because the messages anyone
    /// hand-writes are mostly the mismatched ones: a bridge's tests have to be
    /// able to build the hello they are testing their mismatch handling
    /// against.
    pub fn hello(app_label: impl Into<String>, protocol_version: u32) -> Self {
        Self::Hello {
            app_label: app_label.into(),
            protocol_version,
        }
    }

    /// Build an [`Ack`](Self::Ack) answering one input.
    pub fn ack(id: InputId, status: InputStatus) -> Self {
        Self::Ack { id, status }
    }
}

/// Fate of one input, reported back by the app.
///
/// One input may be acked more than once, and the last ack wins. An app acks
/// [`Delivered`](Self::Delivered) the moment its event loop dequeues the
/// input, before it knows what it will do with it, and may follow up with
/// [`Ignored`](Self::Ignored) once it turns out to have done nothing. Acks for
/// one input reach the bridge in the order the app sent them, so the newest
/// one received is the app's current answer.
///
/// A status name this build does not know fails the [`AppToBridge::Ack`]
/// carrying it, and the reader skips that line. There is no fallback variant
/// here, unlike [`Role::Other`](crate::Role::Other) and
/// [`Action::Custom`](crate::Action::Custom), because a status sits at the top
/// of a message rather than nested inside one: losing the ack loses only the
/// ack, and an input with no ack already means "unacknowledged", which is the
/// safe reading of a status the receiver cannot interpret.
///
/// `#[non_exhaustive]` because a status added later is worth adding, and a
/// bridge matches on this enum to decide what to report about an input it
/// sent. The attribute makes such a bridge keep compiling; what it must then
/// do with a status it cannot interpret is not silence, and not a guess about
/// whether the input landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
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
///
/// `#[non_exhaustive]` for the reason on [`AppToBridge`], from the other side:
/// an adapter matches on this enum to apply what the bridge sends, so a variant
/// added within version 1 must not break its build.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
#[non_exhaustive]
pub enum BridgeToApp {
    /// Agent input to apply against the latest snapshot.
    ///
    /// `#[non_exhaustive]`: build it with [`input`](Self::input) and
    /// destructure it with `..`.
    #[non_exhaustive]
    Input {
        /// Identifier the app echoes in its [`AppToBridge::Ack`].
        id: InputId,
        input: AgentInput,
    },
}

impl BridgeToApp {
    /// Build an [`Input`](Self::Input) message.
    ///
    /// The id must not repeat for the lifetime of the bridge process; see
    /// [`InputId`] for what a reused one costs.
    pub fn input(id: InputId, input: AgentInput) -> Self {
        Self::Input { id, input }
    }
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
    fn unknown_vocabulary_does_not_cost_the_whole_snapshot() {
        // The failure this guards against: one leaf node using a role or an
        // action added in a later version used to make the entire snapshot
        // unparseable, so a reader that skips malformed lines went on serving
        // its last tree and the app looked frozen with no error anywhere.
        let line = r#"{"type":"snapshot","protocol_version":1,"seq":4,"root":{"id":"root","role":"app","focused":false,"children":[{"id":"chart","role":"sparkline","label":"cpu","focused":false,"actions":["zoom",{"set_range":{"from":1,"to":9}}]},{"id":"btn","role":"button","label":"Save","focused":true,"actions":["activate"]}]}}"#;

        let AppToBridge::Snapshot(snapshot) = serde_json::from_str(line).unwrap() else {
            panic!("expected a snapshot message");
        };
        assert_eq!(snapshot.seq, 4);
        assert_eq!(snapshot.root.role, Role::App);

        let chart = &snapshot.root.children[0];
        assert_eq!(chart.role, Role::Other);
        assert_eq!(chart.label.as_deref(), Some("cpu"));
        assert_eq!(
            chart.actions,
            vec![
                Action::Custom("zoom".into()),
                Action::Custom("set_range".into())
            ]
        );

        // The rest of the tree is untouched: the degraded node costs only
        // itself.
        let button = &snapshot.root.children[1];
        assert_eq!(button.role, Role::Button);
        assert_eq!(button.label.as_deref(), Some("Save"));
        assert_eq!(button.actions, vec![Action::Activate]);
        assert!(button.focused);
    }

    #[test]
    fn the_constructors_build_the_variants_they_name() {
        // Outside this crate the marked variants have no struct literal, so
        // these are the only way to build them and must reach every field.
        assert_eq!(
            AppToBridge::hello("demo-app", 7),
            AppToBridge::Hello {
                app_label: "demo-app".into(),
                protocol_version: 7,
            }
        );
        assert_eq!(
            AppToBridge::ack(9, InputStatus::Dropped),
            AppToBridge::Ack {
                id: 9,
                status: InputStatus::Dropped,
            }
        );
        assert_eq!(
            BridgeToApp::input(9, AgentInput::key("q")),
            BridgeToApp::Input {
                id: 9,
                input: AgentInput::Key { key: "q".into() },
            }
        );
    }

    #[test]
    fn an_input_kind_from_a_later_version_keeps_its_id() {
        // The whole reason `AgentInput::Unknown` exists. A kind this build has
        // never heard of used to fail the line it arrived on, so the app
        // skipped it and the agent waited out a timeout with no ack at all.
        // The id lives on the message rather than inside the input, so it
        // survives, and an ack becomes possible.
        let line = r#"{"type":"input","id":41,"input":{"kind":"paste","text":"hi","from":"clip"}}"#;
        let BridgeToApp::Input { id, input, .. } = serde_json::from_str(line).unwrap();
        assert_eq!(id, 41);
        assert_eq!(input, AgentInput::Unknown);

        // And that is the ack: the app truly did nothing with the input.
        assert_eq!(
            serde_json::to_string(&AppToBridge::ack(id, InputStatus::Ignored)).unwrap(),
            r#"{"type":"ack","id":41,"status":"ignored"}"#
        );
    }

    #[test]
    fn unknown_status_costs_only_its_own_ack() {
        // No fallback variant exists for InputStatus, so an unfamiliar status
        // fails its ack and the reader skips that line. The input then reads
        // as unacknowledged, which is the safe reading. This pins that
        // documented behaviour rather than endorsing it.
        let line = r#"{"type":"ack","id":7,"status":"coalesced"}"#;
        assert!(serde_json::from_str::<AppToBridge>(line).is_err());
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
