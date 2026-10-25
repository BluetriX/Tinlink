-- Who is currently connected to the voice room of a channel, maintained
-- from LiveKit webhooks.
CREATE TABLE voice_participants (
    channel_id UUID NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
    user_id    UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    joined_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (channel_id, user_id)
);
CREATE INDEX voice_participants_user_id ON voice_participants(user_id);
