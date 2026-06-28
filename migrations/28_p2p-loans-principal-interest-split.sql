-- Splits `debt` into its two components so interest can be recognized (and logged to the
-- Ledger) only as it's actually collected, rather than all at once at loan creation - see
-- `P2PLoans::lend`/`P2PLoans::settle_from_award`. `remaining_principal` and `remaining_interest`
-- are each independently mutable, always non-negative (every row's `debt` is non-negative by
-- construction - see `P2PLoans::lend`, which now creates a separate reciprocal row instead of a
-- signed `debt` for a negative-rate loan), and must always sum back to `debt`; `debt` itself is
-- untouched, so every existing read path keeps working unmodified.
--
-- For a "normal" row (the borrower's principal + any positive interest), `remaining_principal`
-- starts at the literal transferred amount and `remaining_interest` at the agreed interest. For
-- a "reciprocal" row (a negative-rate loan's separate payback obligation), the whole `debt` is
-- interest and `remaining_principal` starts at 0. Repayments drain `remaining_interest` first
-- (see `split_payment`), so the Ledger can recognize the lender's yield as soon as it's actually
-- collected, before any principal repayment.
ALTER TABLE P2P_Loans ADD COLUMN remaining_principal int;
ALTER TABLE P2P_Loans ADD COLUMN remaining_interest int;

-- Backfill for rows that predate this migration: we can't tell a "normal" row from a
-- "reciprocal" one after the fact, nor recover the original principal or the actual
-- interest_rate used (a custom per-loan rate isn't persisted), so this is necessarily an
-- approximation - reverse-engineer a plausible split from 0.1, the configured
-- P2P_LOAN_INTEREST_RATE's value at the time this migration was written (a literal snapshot, not
-- a live env read - plain .sql migrations can't see env vars; if the deployed value has actually
-- differed from 0.1, this approximation is correspondingly less accurate for older loans, which
-- is accepted - the alternative is no backfill at all).
--
-- interest_fraction = r / (1 + r); remaining_interest = round(debt * interest_fraction), clamped
-- to [0, debt] so the CHECK constraint below can never be violated by a rounding edge case.
UPDATE P2P_Loans
SET remaining_interest = LEAST(GREATEST(ROUND(debt * (0.1 / 1.1))::int, 0), debt)
WHERE remaining_principal IS NULL;

-- Whatever isn't interest is principal, by construction.
UPDATE P2P_Loans
SET remaining_principal = debt - remaining_interest
WHERE remaining_principal IS NULL;

ALTER TABLE P2P_Loans ALTER COLUMN remaining_principal SET NOT NULL;
ALTER TABLE P2P_Loans ALTER COLUMN remaining_interest SET NOT NULL;

ALTER TABLE P2P_Loans ADD CONSTRAINT p2p_loans_principal_non_negative CHECK (remaining_principal >= 0);
ALTER TABLE P2P_Loans ADD CONSTRAINT p2p_loans_interest_non_negative CHECK (remaining_interest >= 0);
ALTER TABLE P2P_Loans ADD CONSTRAINT p2p_loans_debt_equals_principal_plus_interest
    CHECK (debt = remaining_principal + remaining_interest);
