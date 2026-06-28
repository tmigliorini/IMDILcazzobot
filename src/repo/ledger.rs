use anyhow::Context;
use chrono::{DateTime, Utc};
use teloxide::types::UserId;
use crate::repo::{ChatIdKind, ChatIdPartiality};
use crate::repository;

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "ledger_category", rename_all = "snake_case")]
pub enum LedgerCategory {
    Grow,
    Pvp,
    Donate,
    LoanInterest,
    Tax,
    /// Principal movements of both bank loans (`/loan`, no counterparty - see
    /// [`LedgerEntry::counterparty`]) and P2P loans (`/presta`, counterparty = the other side).
    /// Kept separate from `LoanInterest`, which only ever covers the interest portion.
    LoanPrincipal,
}

#[derive(sqlx::FromRow)]
pub struct CategoryBreakdown {
    pub category: LedgerCategory,
    pub dare: i64,
    pub avere: i64,
}

/// One row of a player's personal statement (`/estratto`) - see `crate::handlers::statement`.
pub struct LedgerEntry {
    pub category: LedgerCategory,
    pub amount: i32,
    /// The other side of a transfer (opponent, donor/receiver, lender/borrower), with their
    /// display name already resolved. `None` for grow/dod/bank-loan/pooled-tax events, which
    /// have no single human counterparty.
    pub counterparty: Option<(UserId, String)>,
    pub created_at: DateTime<Utc>,
}

struct LedgerEntryEntity {
    category: LedgerCategory,
    amount: i32,
    counterparty_uid: Option<i64>,
    counterparty_name: Option<String>,
    created_at: DateTime<Utc>,
}

impl From<LedgerEntryEntity> for LedgerEntry {
    fn from(value: LedgerEntryEntity) -> Self {
        let counterparty = value.counterparty_uid.zip(value.counterparty_name)
            .map(|(uid, name)| (UserId(uid as u64), name));
        Self {
            category: value.category,
            amount: value.amount,
            counterparty,
            created_at: value.created_at,
        }
    }
}

repository!(Ledger, with_(chats)_(Chats),
    /// No-op for `amount == 0`, since a zero change isn't a real economic event.
    pub async fn record(&self, chat_id: &ChatIdPartiality, uid: UserId, category: LedgerCategory, amount: i32, counterparty: Option<UserId>) -> anyhow::Result<()> {
        let chat_internal_id = self.chats.upsert_chat(chat_id).await?;
        self.record_for_internal_chat(chat_internal_id, uid, category, amount, counterparty).await
            .context(format!("couldn't record a ledger entry for {uid} in {chat_id} ({category:?}, {amount})"))
    }
,
    pub async fn record_many(&self, chat_id: &ChatIdPartiality, category: LedgerCategory, entries: &[(UserId, i32, Option<UserId>)]) -> anyhow::Result<()> {
        for &(uid, amount, counterparty) in entries {
            self.record(chat_id, uid, category, amount, counterparty).await?;
        }
        Ok(())
    }
,
    /// Like `record`, but for callers that only have a `ChatIdKind` in scope (no inline-query-vs-
    /// database merging context to build a full `ChatIdPartiality`) and are certain the chat
    /// already exists - e.g. `crate::handlers::debt_settlement::settle_gain_against_debts`, only
    /// ever reached after a loan was created in that chat earlier. Looks the chat up via
    /// `Chats::get_internal_id` instead of upserting (which `record` does), since upserting
    /// would need the fuller `ChatIdPartiality` this caller doesn't have.
    pub async fn record_for_chat_kind(&self, chat_id: &ChatIdKind, uid: UserId, category: LedgerCategory, amount: i32, counterparty: Option<UserId>) -> anyhow::Result<()> {
        let chat_internal_id = self.chats.get_internal_id(chat_id).await
            .map_err(|e| anyhow::anyhow!(e))
            .context(format!("couldn't resolve the internal id of {chat_id}"))?;
        self.record_for_internal_chat(chat_internal_id, uid, category, amount, counterparty).await
            .context(format!("couldn't record a ledger entry for {uid} in {chat_id} ({category:?}, {amount})"))
    }
,
    pub async fn record_many_for_chat_kind(&self, chat_id: &ChatIdKind, category: LedgerCategory, entries: &[(UserId, i32, Option<UserId>)]) -> anyhow::Result<()> {
        for &(uid, amount, counterparty) in entries {
            self.record_for_chat_kind(chat_id, uid, category, amount, counterparty).await?;
        }
        Ok(())
    }
,
    /// Per-category totals for `uid` in `chat_id`: `dare` (total debited, as a positive number)
    /// and `avere` (total credited).
    pub async fn get_breakdown(&self, chat_id: &ChatIdPartiality, uid: UserId) -> anyhow::Result<Vec<CategoryBreakdown>> {
        let chat_internal_id = self.chats.upsert_chat(chat_id).await?;
        sqlx::query_as!(CategoryBreakdown,
            r#"SELECT category as "category: LedgerCategory",
                      coalesce(-sum(amount) FILTER (WHERE amount < 0), 0) as "dare!",
                      coalesce(sum(amount) FILTER (WHERE amount > 0), 0) as "avere!"
               FROM Ledger WHERE uid = $1 AND chat_id = $2 GROUP BY category"#,
            uid.0 as i64, chat_internal_id)
            .fetch_all(&self.pool)
            .await
            .context(format!("couldn't get the ledger breakdown for {uid} in {chat_id}"))
    }
,
    /// Like `get_breakdown`, but summed across every player in `chat_id` instead of a single
    /// `uid` - the data behind the aggregated economic report reachable from the "ℹ️ Informazion"
    /// menu (see `crate::handlers::info::InfoSection::Report`).
    pub async fn get_chat_breakdown(&self, chat_id: &ChatIdPartiality) -> anyhow::Result<Vec<CategoryBreakdown>> {
        let chat_internal_id = self.chats.upsert_chat(chat_id).await?;
        sqlx::query_as!(CategoryBreakdown,
            r#"SELECT category as "category: LedgerCategory",
                      coalesce(-sum(amount) FILTER (WHERE amount < 0), 0) as "dare!",
                      coalesce(sum(amount) FILTER (WHERE amount > 0), 0) as "avere!"
               FROM Ledger WHERE chat_id = $1 GROUP BY category"#,
            chat_internal_id)
            .fetch_all(&self.pool)
            .await
            .context(format!("couldn't get the chat-wide ledger breakdown for {chat_id}"))
    }
,
    /// A page of `uid`'s full transaction history in `chat_id`, newest first - the source of the
    /// personal statement (`/estratto`, see `crate::handlers::statement`). `limit` should be
    /// requested as `page_size + 1` by the caller, the same "fetch one extra row" trick
    /// `dick::top_impl` uses, to know whether another page exists without a separate COUNT query.
    pub async fn get_page(&self, chat_id: &ChatIdPartiality, uid: UserId, offset: u32, limit: u16) -> anyhow::Result<Vec<LedgerEntry>> {
        let chat_internal_id = self.chats.upsert_chat(chat_id).await?;
        sqlx::query_as!(LedgerEntryEntity,
            r#"SELECT l.category as "category: LedgerCategory", l.amount, l.created_at,
                      l.counterparty_uid as "counterparty_uid?", u.name as "counterparty_name?"
               FROM Ledger l
               LEFT JOIN Users u ON u.uid = l.counterparty_uid
               WHERE l.uid = $1 AND l.chat_id = $2
               ORDER BY l.created_at DESC, l.id DESC
               LIMIT $3 OFFSET $4"#,
            uid.0 as i64, chat_internal_id, limit as i64, offset as i64)
            .fetch_all(&self.pool)
            .await
            .context(format!("couldn't get a ledger page for {uid} in {chat_id}"))
            .map(|rows| rows.into_iter().map(LedgerEntry::from).collect())
    }
);

impl Ledger {
    /// No-op for `amount == 0`, since a zero change isn't a real economic event - shared by
    /// `record` and `record_for_chat_kind`, which only differ in how they resolve `chat_id` to
    /// `chat_internal_id`.
    async fn record_for_internal_chat(&self, chat_internal_id: i64, uid: UserId, category: LedgerCategory, amount: i32, counterparty: Option<UserId>) -> anyhow::Result<()> {
        if amount == 0 {
            return Ok(())
        }
        let counterparty_uid = counterparty.map(|c| c.0 as i64);
        sqlx::query!("INSERT INTO Ledger (uid, chat_id, category, amount, counterparty_uid) VALUES ($1, $2, $3, $4, $5)",
                uid.0 as i64, chat_internal_id, category as LedgerCategory, amount, counterparty_uid)
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(Into::into)
    }
}
