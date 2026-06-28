ALTER TYPE ledger_category ADD VALUE IF NOT EXISTS 'loan_principal';

ALTER TABLE Ledger ADD COLUMN IF NOT EXISTS counterparty_uid bigint REFERENCES Users(uid);
COMMENT ON COLUMN Ledger.counterparty_uid IS 'The other side of a transfer (opponent, donor/receiver, lender/borrower); NULL for grow/dod/bank-loan/pooled-tax events, which have no single human counterparty';

CREATE INDEX IF NOT EXISTS idx_ledger_uid_chat_created_at ON Ledger(uid, chat_id, created_at DESC, id DESC);
