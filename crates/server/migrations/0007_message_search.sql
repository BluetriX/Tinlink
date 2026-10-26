-- Full-text search over messages. The 'simple' configuration does no
-- stemming, so it works the same for German, English and anything else;
-- queries must use the identical expression to hit the index.
CREATE INDEX messages_content_search ON messages USING GIN (to_tsvector('simple', content));
