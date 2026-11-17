-- An import whose content was already imported under another idempotency key is refused unless the caller
-- asks for it, so the hash of the request is looked up on every import.
CREATE INDEX payment_batches_request_hash_idx ON payment_batches (request_hash);
