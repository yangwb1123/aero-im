ALTER TABLE messages ADD COLUMN expires_at TIMESTAMPTZ;
CREATE INDEX idx_messages_expires_at ON messages (expires_at) WHERE expires_at IS NOT NULL;
