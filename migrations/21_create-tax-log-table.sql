CREATE TABLE IF NOT EXISTS Tax_Log (
    chat_id bigint NOT NULL REFERENCES Chats(id),
    created_at date NOT NULL DEFAULT current_date,

    PRIMARY KEY (chat_id, created_at)
);
