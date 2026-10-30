-- The identity key that certified a device. After an identity reset the
-- user's devices are revoked and the key changes; clients still need the
-- old key to check messages and channel keys those devices signed.
ALTER TABLE devices ADD COLUMN identity_key BYTEA;
UPDATE devices d SET identity_key = u.identity_key
    FROM users u
    WHERE d.user_id = u.id AND d.certificate IS NOT NULL;
