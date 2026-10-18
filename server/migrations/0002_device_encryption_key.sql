-- Pairing needs both identity keys of a device: the Ed25519 signing key
-- (already stored) and the X25519 encryption key used for end-to-end sealed
-- transport sessions. Devices registered before this migration have NULL until
-- their next registration.
ALTER TABLE devices ADD COLUMN encryption_key TEXT
    CHECK (encryption_key IS NULL OR encryption_key ~ '^[0-9a-f]{64}$');
