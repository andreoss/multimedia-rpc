CREATE TABLE input_metadata (
    media_id text PRIMARY KEY,
    metadata bytea NOT NULL,
    probed_at timestamptz NOT NULL DEFAULT now()
);
