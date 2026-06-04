//! Channel bindings (#393, P3.c) — map an external channel conversation to a
//! Session Room.
//!
//! A channel (Discord, WhatsApp, the TUI, …) is a *client* of the gateway API,
//! never a direct store reader (Separation of Powers, #390). A binding lets a
//! channel-native conversation — a Discord thread, a WhatsApp chat — survive
//! reconnects and route the operator's replies back into the right room as
//! `Operator`-seat events. `(channel, external_id)` is the natural key: one
//! external conversation maps to exactly one room.

use serde::{Deserialize, Serialize};

/// A persisted mapping from an external channel conversation to a room.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelBinding {
    /// Channel kind, e.g. `"discord"`, `"whatsapp"`, `"tui"`.
    pub channel: String,
    /// The channel-native conversation id (thread / chat / room id).
    pub external_id: String,
    /// The room this conversation is bound to.
    pub root_session_id: String,
    pub created_at: String,
    pub updated_at: String,
}

/// Params for `channel.bind` — upsert a binding (idempotent on the natural key).
/// Derives `Serialize` too so channel clients can reuse it to *send* the request
/// (not just the gateway to receive it).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelBindParams {
    pub channel: String,
    pub external_id: String,
    pub root_session_id: String,
}

/// Params for `channel.resolve` — look up the room for a conversation. Derives
/// `Serialize` too so channel clients can reuse it to *send* the request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelResolveParams {
    pub channel: String,
    pub external_id: String,
}

/// Result of `channel.resolve` — the binding if one exists, else `None`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelResolveResult {
    pub binding: Option<ChannelBinding>,
}
