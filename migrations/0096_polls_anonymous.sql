-- Anonymous voting support for polls.
-- When anonymous=true, the tally endpoint suppresses voter identities (counts only).
ALTER TABLE polls ADD COLUMN anonymous BOOLEAN NOT NULL DEFAULT false;
