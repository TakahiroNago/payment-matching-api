-- An invoice registered by mistake can be voided. A void invoice takes no part in matching and keeps its row,
-- because cancelled suggestions may still refer to it.
ALTER TABLE invoices
    DROP CONSTRAINT invoices_status,
    ADD CONSTRAINT invoices_status CHECK (status IN ('open', 'cleared', 'void'));

-- The number of a void invoice can be registered again, with the right customer, amount or date. The index
-- keeps the name of the constraint it replaces: the application recognises the violation by that name.
ALTER TABLE invoices DROP CONSTRAINT invoices_number_key;
CREATE UNIQUE INDEX invoices_number_key ON invoices (invoice_number) WHERE status <> 'void';

-- invoice_voided: an invoice of the suggestion was voided, so the suggestion could not be confirmed any more.
ALTER TABLE matches
    DROP CONSTRAINT matches_cancel_reason,
    ADD CONSTRAINT matches_cancel_reason
        CHECK (cancel_reason IN ('dismissed', 'reversed', 'replaced', 'invoice_cleared', 'invoice_voided'));
