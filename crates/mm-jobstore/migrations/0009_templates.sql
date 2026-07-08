CREATE TABLE templates (
    id uuid PRIMARY KEY,
    user_id text NOT NULL,
    name text NOT NULL,
    spec bytea NOT NULL,
    parameters bytea NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (user_id, name)
);
