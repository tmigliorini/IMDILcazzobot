CREATE TABLE IF NOT EXISTS P2P_Loans (
    id serial PRIMARY KEY,
    chat_id bigint NOT NULL REFERENCES Chats(id),
    lender_uid bigint NOT NULL REFERENCES Users(uid),
    borrower_uid bigint NOT NULL REFERENCES Users(uid),
    debt int NOT NULL CHECK ( debt >= 0 ),
    payout_ratio real NOT NULL CHECK ( payout_ratio > 0.0 AND payout_ratio < 1.0 ),
    created_at timestamptz NOT NULL DEFAULT current_timestamp,
    repaid_at timestamptz
);

CREATE INDEX IF NOT EXISTS idx_p2p_loans_borrower ON P2P_Loans(borrower_uid);

-- a borrower may only have one active (unpaid) P2P loan per chat at a time, to keep
-- the automatic repayment simple (no need to arbitrate between multiple lenders)
CREATE UNIQUE INDEX IF NOT EXISTS idx_p2p_loans_one_active_per_borrower
    ON P2P_Loans(chat_id, borrower_uid) WHERE repaid_at IS NULL;

CREATE OR REPLACE FUNCTION set_timestamp_if_p2p_debt_repaid()
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

CREATE OR REPLACE TRIGGER trg_set_timestamp_if_p2p_debt_repaid BEFORE UPDATE ON P2P_Loans
    FOR EACH ROW EXECUTE FUNCTION set_timestamp_if_p2p_debt_repaid();
