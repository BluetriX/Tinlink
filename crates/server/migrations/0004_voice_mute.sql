-- Server-side voice mute per group member: voice tokens then omit the
-- microphone, and connected participants lose it immediately.
ALTER TABLE members ADD COLUMN voice_muted BOOLEAN NOT NULL DEFAULT false;
