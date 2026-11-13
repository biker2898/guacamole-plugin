-- Media traffic per user and computer, in 10-minute buckets (unix time of
-- the bucket start). Kept for 8 days; the console shows a rolling 24 hours.
CREATE TABLE traffic (
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    connection_id INTEGER NOT NULL,
    bucket INTEGER NOT NULL,
    bytes_out INTEGER NOT NULL DEFAULT 0,
    bytes_in INTEGER NOT NULL DEFAULT 0,
    seconds INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (user_id, connection_id, bucket)
);
CREATE INDEX traffic_bucket ON traffic (bucket);
