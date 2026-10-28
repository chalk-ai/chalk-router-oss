-- List-price estimate for one call, priced at write time so an aggregate over
-- any dimension sums costs that were each computed from the one model that
-- produced them. Pricing at read time cannot do this: a row grouped by API key
-- or usage pool has no model to price against.
--
-- NULL means the call was not priced -- an unknown model, or a call recorded
-- before this column existed -- and is deliberately distinct from 0, which a
-- fully cache-read call legitimately is. Queries sum the priced calls and count
-- the unpriced ones separately so a partial estimate cannot read as a complete
-- one.
ALTER TABLE chalk_router.usage_events
    ADD COLUMN estimated_cost_usd DOUBLE PRECISION
        CHECK (estimated_cost_usd >= 0);

COMMENT ON COLUMN chalk_router.usage_events.estimated_cost_usd IS
    'Public list-price estimate in USD at the rate card in force when the call was recorded. NULL means unpriced, which is not the same as zero.';
