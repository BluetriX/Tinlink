-- Initial schema: accounts, groups, roles, channels, messages.
-- Permission bitfields are stored as BIGINT with the same bit pattern as
-- tinlink_protocol::Permissions (u64 reinterpreted as i64).

CREATE TABLE users (
    id            UUID PRIMARY KEY,
    -- Local part of the federated id `name@domain`, lower case.
    name          TEXT NOT NULL UNIQUE,
    display_name  TEXT NOT NULL,
    password_hash TEXT NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE sessions (
    -- SHA-256 of the bearer token; the token itself is never stored.
    token_hash BYTEA PRIMARY KEY,
    user_id    UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX sessions_user_id ON sessions(user_id);

CREATE TABLE groups (
    id                 UUID PRIMARY KEY,
    name               TEXT NOT NULL,
    owner_id           UUID NOT NULL REFERENCES users(id),
    -- Maximum number of user-created channels per member.
    user_channel_limit INT NOT NULL DEFAULT 2,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- The @everyone role of a group has the same id as the group.
CREATE TABLE roles (
    id          UUID PRIMARY KEY,
    group_id    UUID NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    color       INT,
    -- Higher position = more powerful. @everyone is always 0.
    position    INT NOT NULL,
    permissions BIGINT NOT NULL
);
CREATE INDEX roles_group_id ON roles(group_id);

CREATE TABLE members (
    group_id  UUID NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    user_id   UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    joined_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (group_id, user_id)
);
CREATE INDEX members_user_id ON members(user_id);

CREATE TABLE member_roles (
    group_id UUID NOT NULL,
    user_id  UUID NOT NULL,
    role_id  UUID NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
    PRIMARY KEY (group_id, user_id, role_id),
    FOREIGN KEY (group_id, user_id) REFERENCES members(group_id, user_id) ON DELETE CASCADE
);

CREATE TABLE categories (
    id                  UUID PRIMARY KEY,
    group_id            UUID NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    name                TEXT NOT NULL,
    position            INT NOT NULL DEFAULT 0,
    -- Members with CREATE_OWN_CHANNELS may create channels here.
    allow_user_channels BOOLEAN NOT NULL DEFAULT false
);
CREATE INDEX categories_group_id ON categories(group_id);

CREATE TABLE channels (
    id            UUID PRIMARY KEY,
    group_id      UUID NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    category_id   UUID REFERENCES categories(id) ON DELETE SET NULL,
    name          TEXT NOT NULL,
    position      INT NOT NULL DEFAULT 0,
    voice_enabled BOOLEAN NOT NULL DEFAULT true,
    -- Set for channels created by members via CREATE_OWN_CHANNELS.
    created_by    UUID REFERENCES users(id) ON DELETE SET NULL,
    temporary     BOOLEAN NOT NULL DEFAULT false,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX channels_group_id ON channels(group_id);

CREATE TABLE overwrites (
    channel_id  UUID NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
    -- 'role' or 'member'
    target_kind TEXT NOT NULL CHECK (target_kind IN ('role', 'member')),
    target_id   UUID NOT NULL,
    allow_bits  BIGINT NOT NULL DEFAULT 0,
    deny_bits   BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (channel_id, target_kind, target_id)
);

CREATE TABLE messages (
    -- UUIDv7, so ordering by id is ordering by time.
    id         UUID PRIMARY KEY,
    channel_id UUID NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
    author_id  UUID REFERENCES users(id) ON DELETE SET NULL,
    content    TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    edited_at  TIMESTAMPTZ
);
CREATE INDEX messages_channel_id ON messages(channel_id, id DESC);

CREATE TABLE invites (
    code       TEXT PRIMARY KEY,
    group_id   UUID NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    created_by UUID REFERENCES users(id) ON DELETE SET NULL,
    uses       INT NOT NULL DEFAULT 0,
    max_uses   INT,
    expires_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
