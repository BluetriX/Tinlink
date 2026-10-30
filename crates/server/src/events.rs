//! In-process event hub for live updates over the WebSocket gateway.
//!
//! Handlers publish events after a change is committed. Every gateway
//! connection receives all events and filters them by the user's access, so
//! permission rules live in one place (`access`).

use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;
use tokio::sync::broadcast;
use uuid::Uuid;

/// Events buffered per connection before it is considered lagging.
const CAPACITY: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    MessageCreated,
    MessageUpdated,
    MessageDeleted,
    ChannelCreated,
    ChannelUpdated,
    ChannelDeleted,
    VoiceJoined,
    VoiceLeft,
    /// A member was moved; the client of `user_id` should join `data.to`.
    VoiceMoved,
    VoiceMuteChanged,
    MemberJoined,
    MemberLeft,
    /// Roles, overwrites, categories or group settings changed; clients
    /// should refetch channels and permissions.
    GroupUpdated,
    GroupDeleted,
    /// A member came online or went offline (`data.online`); sent once per
    /// group the user is in.
    PresenceChanged,
    /// `user_id` is typing in the channel; clients show it for a few seconds.
    Typing,
    /// A user changed their display name or avatar; `data` is the user.
    /// Sent to every connected user, like the profile it describes.
    UserUpdated,
    /// A user's identity key or devices changed (`user_id`); clients refetch
    /// that user's keys and share channel keys with new devices. Sent to
    /// every connected user, like the keys it describes.
    KeysChanged,
    /// New wrapped channel keys were stored (`data.epoch`); clients fetch
    /// the ones for their device.
    ChannelKeysAdded,
}

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    #[serde(rename = "type")]
    pub kind: EventKind,
    /// None for direct message channels, which belong to no group; delivery
    /// then requires being one of the two participants.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_id: Option<Uuid>,
    /// Set for channel-scoped events; delivery then requires VIEW_CHANNEL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<Uuid>,
    /// For member events: the affected user.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<Uuid>,
    pub data: Value,
}

#[derive(Clone)]
pub struct EventHub {
    tx: broadcast::Sender<Arc<Event>>,
}

impl Default for EventHub {
    fn default() -> Self {
        Self {
            tx: broadcast::channel(CAPACITY).0,
        }
    }
}

impl EventHub {
    pub fn publish(&self, event: Event) {
        // An error only means nobody is connected.
        let _ = self.tx.send(Arc::new(event));
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Event>> {
        self.tx.subscribe()
    }

    pub fn group(&self, kind: EventKind, group_id: Uuid, data: impl Serialize) {
        self.publish(Event {
            kind,
            group_id: Some(group_id),
            channel_id: None,
            user_id: None,
            data: serde_json::to_value(data).unwrap_or(Value::Null),
        });
    }

    pub fn channel(&self, kind: EventKind, group_id: Uuid, channel_id: Uuid, data: impl Serialize) {
        self.chat(kind, Some(group_id), channel_id, data);
    }

    /// A channel event for a group channel or a direct message channel.
    pub fn chat(
        &self,
        kind: EventKind,
        group_id: Option<Uuid>,
        channel_id: Uuid,
        data: impl Serialize,
    ) {
        self.publish(Event {
            kind,
            group_id,
            channel_id: Some(channel_id),
            user_id: None,
            data: serde_json::to_value(data).unwrap_or(Value::Null),
        });
    }

    pub fn user_updated(&self, user: &crate::auth::UserInfo) {
        self.publish(Event {
            kind: EventKind::UserUpdated,
            group_id: None,
            channel_id: None,
            user_id: Some(user.id),
            data: serde_json::to_value(user).unwrap_or(Value::Null),
        });
    }

    pub fn keys_changed(&self, user_id: Uuid) {
        self.publish(Event {
            kind: EventKind::KeysChanged,
            group_id: None,
            channel_id: None,
            user_id: Some(user_id),
            data: Value::Null,
        });
    }

    pub fn member(&self, kind: EventKind, group_id: Uuid, user_id: Uuid) {
        self.publish(Event {
            kind,
            group_id: Some(group_id),
            channel_id: None,
            user_id: Some(user_id),
            data: Value::Null,
        });
    }
}
