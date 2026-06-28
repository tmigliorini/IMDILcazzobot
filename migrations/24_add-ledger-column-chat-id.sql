-- Ledger has had zero rows since its creation, so this is a safe non-backfilled NOT NULL add.
ALTER TABLE Ledger ADD COLUMN chat_id bigint NOT NULL REFERENCES Chats(id);

DROP INDEX IF EXISTS idx_ledger_uid_category;
CREATE INDEX IF NOT EXISTS idx_ledger_uid_chat_category ON Ledger(uid, chat_id, category);
