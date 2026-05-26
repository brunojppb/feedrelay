-- runs: every Run in chronological order
CREATE TABLE runs (
    run_id TEXT PRIMARY KEY,
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    status TEXT NOT NULL,  -- queued | running | succeeded | failed
    query_used TEXT,
    candidates_returned INTEGER,
    candidates_after_filter INTEGER,
    selected_asset_id TEXT,
    caption TEXT,
    buffer_post_id TEXT,
    error TEXT,
    duration_ms INTEGER
);

-- posts: permanent dedup table, written only after a successful Buffer mutation
CREATE TABLE posts (
    immich_asset_id TEXT PRIMARY KEY,
    buffer_post_id TEXT NOT NULL,
    caption TEXT NOT NULL,
    posted_at INTEGER NOT NULL,
    run_id TEXT NOT NULL
);
CREATE INDEX idx_posts_posted_at ON posts(posted_at DESC);

-- pending_media: short-lived uuid → asset mapping for /pic/<uuid>.jpg
CREATE TABLE pending_media (
    uuid TEXT PRIMARY KEY,
    immich_asset_id TEXT NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE INDEX idx_pending_expires ON pending_media(expires_at);

-- captions: keyed by (asset, full rendered prompt) — survives retries
CREATE TABLE captions (
    immich_asset_id TEXT NOT NULL,
    prompt TEXT NOT NULL,
    caption TEXT NOT NULL,
    hashtags TEXT NOT NULL,  -- JSON array
    alt_text TEXT NOT NULL,
    generated_at INTEGER NOT NULL,
    PRIMARY KEY (immich_asset_id, prompt)
);
