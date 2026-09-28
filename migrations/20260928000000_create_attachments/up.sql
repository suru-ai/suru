-- Attachments: media a user places on a Prompt beside its text, stored once
-- under the blake3 hash of its bytes (ADR 0037). A row is written when its
-- bytes are first uploaded and its bytes never change; an image's width and
-- height are read from its header alone, and a kind without pixels leaves
-- them empty. uploaded_at is when the bytes were last uploaded, first or
-- again: every client uploads an image right before binding it, so an
-- Attachment uploaded within the grace period may be about to be bound, and
-- neither a Session's deletion nor the orphan sweep reclaims it until that
-- period has passed.
CREATE TABLE attachments (
    id TEXT PRIMARY KEY NOT NULL,
    mime_type TEXT NOT NULL,
    byte_length BIGINT NOT NULL,
    width BIGINT,
    height BIGINT,
    uploaded_at BIGINT NOT NULL,
    bytes BLOB NOT NULL
);

-- Which Sessions reference which Attachments, derived from the bindings of
-- each Session's stored Prompts and Messages. One Attachment may be joined to
-- many Sessions; deleting a Session takes its rows here with it.
CREATE TABLE session_attachments (
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    attachment_id TEXT NOT NULL REFERENCES attachments(id) ON DELETE CASCADE,
    PRIMARY KEY (session_id, attachment_id)
);

CREATE INDEX session_attachments_attachment_idx ON session_attachments(attachment_id);
