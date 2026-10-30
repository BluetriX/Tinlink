-- End-to-end encryption: public keys and wrapped channel keys. The server
-- only stores and hands out what clients upload; it never holds a private
-- key or a channel key in the clear. See docs/e2ee.md.

-- Ed25519 identity key of the user, which certifies the user's devices, and
-- the identity private key encrypted with the user's recovery key.
ALTER TABLE users ADD COLUMN identity_key BYTEA;
ALTER TABLE users ADD COLUMN identity_backup BYTEA;

CREATE TABLE devices (
    -- Chosen by the client, which signs it into the certificate.
    id          UUID PRIMARY KEY,
    user_id     UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- 'device': an app or browser; 'backup': the recovery key, which
    -- receives channel keys like a device but never sends.
    kind        TEXT NOT NULL CHECK (kind IN ('device', 'backup')),
    name        TEXT NOT NULL,
    -- Ed25519 key signing wrapped channel keys and messages; none for backups.
    sign_key    BYTEA,
    -- X25519 key channel keys are wrapped to.
    dh_key      BYTEA NOT NULL,
    -- Signature of the identity key over the device; NULL until another
    -- device or the recovery key approves it.
    certificate BYTEA,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Revoked devices are kept so signatures by them stay checkable.
    revoked_at  TIMESTAMPTZ
);
CREATE INDEX devices_user_id ON devices(user_id);
CREATE UNIQUE INDEX devices_one_backup ON devices(user_id)
    WHERE kind = 'backup' AND revoked_at IS NULL;

-- Each channel key has an epoch; a new epoch replaces the key when someone
-- loses access to the channel.
CREATE TABLE channel_key_epochs (
    channel_id UUID NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
    epoch      INT NOT NULL CHECK (epoch > 0),
    created_by UUID NOT NULL REFERENCES devices(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (channel_id, epoch)
);

-- A channel key wrapped to one device, signed by the sending device.
CREATE TABLE channel_keys (
    channel_id UUID NOT NULL,
    epoch      INT NOT NULL,
    device_id  UUID NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
    sender_id  UUID NOT NULL REFERENCES devices(id),
    wrapped    BYTEA NOT NULL,
    signature  BYTEA NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (channel_id, epoch, device_id),
    FOREIGN KEY (channel_id, epoch) REFERENCES channel_key_epochs(channel_id, epoch) ON DELETE CASCADE
);
CREATE INDEX channel_keys_device_id ON channel_keys(device_id);
