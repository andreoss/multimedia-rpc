CREATE TABLE derived_cache (
    digest text PRIMARY KEY,
    outputs jsonb NOT NULL,
    encoder text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
