-- Small images used as user avatars and group/channel icons. Clients scale
-- them down before uploading, so they are kept in the database. A new image
-- gets a new random id, which lets clients cache them forever.
CREATE TABLE images (
    id           UUID PRIMARY KEY,
    content_type TEXT NOT NULL,
    data         BYTEA NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

ALTER TABLE users ADD COLUMN avatar_id UUID REFERENCES images(id) ON DELETE SET NULL;
ALTER TABLE groups ADD COLUMN icon_id UUID REFERENCES images(id) ON DELETE SET NULL;
ALTER TABLE channels ADD COLUMN icon_id UUID REFERENCES images(id) ON DELETE SET NULL;
-- Alternative to an icon image: a short text, usually one emoji.
ALTER TABLE channels ADD COLUMN emoji TEXT;
