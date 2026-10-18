-- SyncEngine control-plane schema (SRD §9).
--
-- The server stores metadata only: device identities, pairing state,
-- relationships, folder memberships, and relay-session bookkeeping. It never
-- stores file contents or device-local absolute paths.

CREATE TABLE devices (
    device_id    UUID PRIMARY KEY,
    device_name  TEXT NOT NULL CHECK (char_length(device_name) BETWEEN 1 AND 128),
    platform     TEXT NOT NULL CHECK (char_length(platform) BETWEEN 1 AND 64),
    public_key   TEXT NOT NULL CHECK (public_key ~ '^[0-9a-f]{64}$'),
    status       TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'revoked')),
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at TIMESTAMPTZ
);

CREATE TABLE pairing_requests (
    request_id          UUID PRIMARY KEY,
    initiator_device_id UUID NOT NULL REFERENCES devices (device_id) ON DELETE CASCADE,
    target_device_id    UUID NOT NULL REFERENCES devices (device_id) ON DELETE CASCADE,
    code_hash           TEXT NOT NULL CHECK (code_hash ~ '^[0-9a-f]{64}$'),
    status              TEXT NOT NULL DEFAULT 'pending'
                        CHECK (status IN ('pending', 'approved', 'expired', 'cancelled')),
    attempts            INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    max_attempts        INTEGER NOT NULL DEFAULT 5 CHECK (max_attempts > 0),
    expires_at          TIMESTAMPTZ NOT NULL,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    approved_at         TIMESTAMPTZ,
    CHECK (initiator_device_id <> target_device_id)
);

CREATE INDEX pairing_requests_pending_by_initiator
    ON pairing_requests (initiator_device_id, created_at)
    WHERE status = 'pending';
CREATE INDEX pairing_requests_pending_by_target
    ON pairing_requests (target_device_id)
    WHERE status = 'pending';

-- One row per device pair, stored in canonical (device_a < device_b) order.
CREATE TABLE device_relationships (
    device_a   UUID NOT NULL REFERENCES devices (device_id) ON DELETE CASCADE,
    device_b   UUID NOT NULL REFERENCES devices (device_id) ON DELETE CASCADE,
    status     TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'revoked')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (device_a, device_b),
    CHECK (device_a < device_b)
);

CREATE INDEX device_relationships_by_b ON device_relationships (device_b);

CREATE TABLE folder_memberships (
    folder_id  UUID NOT NULL,
    device_id  UUID NOT NULL REFERENCES devices (device_id) ON DELETE CASCADE,
    permission TEXT NOT NULL CHECK (permission IN ('read-only', 'read-write')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (folder_id, device_id)
);

CREATE INDEX folder_memberships_by_device ON folder_memberships (device_id);

CREATE TABLE relay_sessions (
    session_id         UUID PRIMARY KEY,
    sender_device_id   UUID NOT NULL REFERENCES devices (device_id) ON DELETE CASCADE,
    receiver_device_id UUID NOT NULL REFERENCES devices (device_id) ON DELETE CASCADE,
    folder_id          UUID NOT NULL,
    status             TEXT NOT NULL DEFAULT 'open'
                       CHECK (status IN ('open', 'active', 'closed', 'revoked')),
    expires_at         TIMESTAMPTZ NOT NULL,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (sender_device_id <> receiver_device_id)
);

CREATE INDEX relay_sessions_by_sender ON relay_sessions (sender_device_id, status);
CREATE INDEX relay_sessions_by_receiver ON relay_sessions (receiver_device_id, status);
CREATE INDEX relay_sessions_expiry ON relay_sessions (expires_at) WHERE status IN ('open', 'active');
