-- Why a match was cancelled:
--   dismissed        a person rejected a suggestion
--   reversed         a person cancelled a confirmed match
--   replaced         a person matched the payment by hand instead of confirming the suggestion
--   invoice_cleared  another payment cleared an invoice of the suggestion, which could not be confirmed any more
-- NULL for matches that are not cancelled, and for those cancelled before the reason was recorded.
ALTER TABLE matches
    ADD COLUMN cancel_reason text
        CONSTRAINT matches_cancel_reason
            CHECK (cancel_reason IN ('dismissed', 'reversed', 'replaced', 'invoice_cleared')),
    ADD CONSTRAINT matches_cancel_reason_only_when_cancelled
        CHECK (cancel_reason IS NULL OR state = 'cancelled');

-- When `decision` was made: at import, or when the payment was last decided again while unmatched.
ALTER TABLE payments ADD COLUMN decided_at timestamptz;
UPDATE payments SET decided_at = created_at;
ALTER TABLE payments
    ALTER COLUMN decided_at SET NOT NULL,
    ALTER COLUMN decided_at SET DEFAULT now();
