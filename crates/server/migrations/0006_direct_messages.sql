-- Direct messages: one channel per pair of users, outside any group. They
-- reuse messages, attachments and read markers of regular channels.
ALTER TABLE channels ALTER COLUMN group_id DROP NOT NULL;

CREATE TABLE direct_channels (
    channel_id UUID PRIMARY KEY REFERENCES channels(id) ON DELETE CASCADE,
    -- The two participants, ordered so that each pair has a single row.
    user_a     UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    user_b     UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    CHECK (user_a < user_b),
    UNIQUE (user_a, user_b)
);
CREATE INDEX direct_channels_user_b ON direct_channels(user_b);
