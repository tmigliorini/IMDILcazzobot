-- `/debiti` only ever showed the current remaining `debt` (and, for a P2P loan,
-- `remaining_principal`/`remaining_interest`), with no memory of what the loan/tax debt
-- originally amounted to - so there was no way to show a player how much of it they'd already
-- paid off. `remaining_principal`/`remaining_interest` can't be (ab)used for this: repayments
-- drain interest first (see `split_payment`), so once a loan is partway repaid they no longer
-- reflect the amounts originally agreed. These new columns are immutable snapshots taken once at
-- creation (see `P2PLoans::lend_in_tx`, `LoanInterestTaxDebts::create`) purely for display - no
-- read path that affects an actual balance uses them.
ALTER TABLE P2P_Loans ADD COLUMN original_principal int;
ALTER TABLE P2P_Loans ADD COLUMN original_interest int;
ALTER TABLE Loan_Interest_Tax_Debts ADD COLUMN original_debt int;

-- Backfill for rows that predate this migration: the true original amounts are only recoverable
-- if nothing has been repaid yet, which can't be told apart from "already partially repaid" after
-- the fact - so this necessarily approximates existing rows as if they were untouched so far (the
-- same accepted tradeoff as migration 28's own backfill). Going forward, every new row sets these
-- correctly at creation time, so the approximation only ever affects loans/tax debts that already
-- existed when this migration ran.
UPDATE P2P_Loans SET original_principal = remaining_principal, original_interest = remaining_interest
    WHERE original_principal IS NULL;
UPDATE Loan_Interest_Tax_Debts SET original_debt = debt WHERE original_debt IS NULL;

ALTER TABLE P2P_Loans ALTER COLUMN original_principal SET NOT NULL;
ALTER TABLE P2P_Loans ALTER COLUMN original_interest SET NOT NULL;
ALTER TABLE Loan_Interest_Tax_Debts ALTER COLUMN original_debt SET NOT NULL;

ALTER TABLE P2P_Loans ADD CONSTRAINT p2p_loans_original_principal_non_negative CHECK (original_principal >= 0);
ALTER TABLE P2P_Loans ADD CONSTRAINT p2p_loans_original_interest_non_negative CHECK (original_interest >= 0);
ALTER TABLE P2P_Loans ADD CONSTRAINT p2p_loans_remaining_principal_le_original CHECK (remaining_principal <= original_principal);
ALTER TABLE P2P_Loans ADD CONSTRAINT p2p_loans_remaining_interest_le_original CHECK (remaining_interest <= original_interest);
ALTER TABLE Loan_Interest_Tax_Debts ADD CONSTRAINT loan_interest_tax_debts_original_debt_non_negative CHECK (original_debt >= 0);
ALTER TABLE Loan_Interest_Tax_Debts ADD CONSTRAINT loan_interest_tax_debts_debt_le_original CHECK (debt <= original_debt);
