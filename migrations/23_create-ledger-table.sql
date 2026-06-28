DO $$ BEGIN
    CREATE TYPE ledger_category AS ENUM (
        'grow',
        'pvp',
        'donate',
        'loan_interest',
        'tax'
    );
EXCEPTION
    WHEN duplicate_object THEN null;
END $$;

CREATE TABLE IF NOT EXISTS Ledger (
    id bigserial PRIMARY KEY,
    uid bigint NOT NULL REFERENCES Users(uid),
    category ledger_category NOT NULL,
    amount int NOT NULL,
    created_at timestamptz NOT NULL DEFAULT current_timestamp
);

CREATE INDEX IF NOT EXISTS idx_ledger_uid_category ON Ledger(uid, category);

COMMENT ON TABLE  Ledger          IS 'A per-player log of every length change, tagged by economic category, for the personal stats dare/avere breakdown';
COMMENT ON COLUMN Ledger.amount   IS 'Positive (avere/credit) or negative (dare/debit); never zero';
