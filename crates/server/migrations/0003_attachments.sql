-- Files uploaded to a channel. An attachment is uploaded first and then
-- linked to a message; unlinked uploads are removed by the maintenance task.
CREATE TABLE attachments (
    id           UUID PRIMARY KEY,
    channel_id   UUID NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
    uploader_id  UUID REFERENCES users(id) ON DELETE SET NULL,
    message_id   UUID REFERENCES messages(id) ON DELETE SET NULL,
    filename     TEXT NOT NULL,
    content_type TEXT NOT NULL,
    size_bytes   BIGINT NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX attachments_message_id ON attachments(message_id);
CREATE INDEX attachments_orphans ON attachments(created_at) WHERE message_id IS NULL;
