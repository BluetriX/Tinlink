-- Up to which message a user has read each channel.
CREATE TABLE read_states (
    user_id      UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    channel_id   UUID NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
    last_read_id UUID NOT NULL,
    PRIMARY KEY (user_id, channel_id)
);
CREATE INDEX read_states_channel_id ON read_states(channel_id);

-- Users mentioned with @name in a message.
CREATE TABLE message_mentions (
    message_id UUID NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    user_id    UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    PRIMARY KEY (message_id, user_id)
);
CREATE INDEX message_mentions_user_id ON message_mentions(user_id, message_id);
