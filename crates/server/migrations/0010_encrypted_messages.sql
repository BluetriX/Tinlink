-- End-to-end encrypted messages: `content` then holds the base64 envelope
-- of docs/e2ee.md, which only clients can open. Search skips them.
ALTER TABLE messages ADD COLUMN encrypted BOOLEAN NOT NULL DEFAULT false;

DROP INDEX messages_content_search;
CREATE INDEX messages_content_search ON messages USING GIN (to_tsvector('simple', content))
    WHERE NOT encrypted;
