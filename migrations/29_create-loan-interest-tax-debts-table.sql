-- The interest tax computed at loan creation (`debt_and_interest_tax`'s `tax`, see
-- `repo::p2p_loans::interest_and_tax`) is no longer withheld in one lump sum at loan-acceptance
-- time. Instead it becomes its own debt obligation owed by whichever side actually realizes the
-- interest (the lender for the usual non-negative rate, the borrower for a negative-rate loan -
-- see `P2PLoans::lend`), repaid gradually out of THEIR future gains exactly like a P2P loan
-- installment - except that each installment collected here isn't credited to a single
-- creditor, it's redistributed to the chat's bottom-N players at THAT moment (recomputed
-- dynamically, see `handlers::tax::redistribute_to_bottom`), mirroring how `/tax` itself works.
CREATE TABLE IF NOT EXISTS Loan_Interest_Tax_Debts (
    id serial PRIMARY KEY,
    chat_id bigint NOT NULL REFERENCES Chats(id),
    payer_uid bigint NOT NULL REFERENCES Users(uid),
    -- traceability only - which p2p loan row's interest produced this tax debt; nullable so a
    -- future non-loan-triggered tax debt (unlikely, but avoid over-coupling) isn't forced to
    -- fake one. Not used for any read path, so it isn't cleaned up if the loan row is ever
    -- deleted - hence no ON DELETE behavior is specified.
    source_loan_id int REFERENCES P2P_Loans(id),
    debt int NOT NULL CHECK (debt >= 0),
    created_at timestamptz NOT NULL DEFAULT current_timestamp,
    repaid_at timestamptz
);

CREATE INDEX IF NOT EXISTS idx_loan_interest_tax_debts_payer ON Loan_Interest_Tax_Debts(payer_uid, chat_id) WHERE repaid_at IS NULL;

CREATE OR REPLACE FUNCTION set_timestamp_if_tax_debt_repaid()
    RETURNS TRIGGER
    LANGUAGE PLPGSQL
AS $$
BEGIN
    IF NEW.debt = 0 THEN
        NEW.repaid_at := current_timestamp;
    END IF;
    RETURN NEW;
END
$$;

CREATE OR REPLACE TRIGGER trg_set_timestamp_if_tax_debt_repaid BEFORE UPDATE ON Loan_Interest_Tax_Debts
    FOR EACH ROW EXECUTE FUNCTION set_timestamp_if_tax_debt_repaid();
