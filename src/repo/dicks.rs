use anyhow::{anyhow, Context};
use futures::TryFutureExt;
use sqlx::{Executor, Pool, Postgres, Transaction};
use teloxide::types::UserId;
use crate::config::FeatureToggles;
use super::{ChatIdKind, ChatIdPartiality, Chats, WinRateAware, UID};
use super::pvpstats::win_rate_percentage;

#[derive(sqlx::FromRow, Debug)]
pub struct Dick {
    pub length: i32,
    pub owner_uid: UID,
    pub owner_name: String,
    pub grown_at: chrono::DateTime<chrono::Utc>,
    pub position: Option<i64>,
    pub battles_total: i32,
    pub battles_won: i32,
}

impl WinRateAware for Dick {
    fn win_rate_percentage(&self) -> f64 {
        win_rate_percentage(self.battles_won, self.battles_total)
    }
}

/// One row of the net-position `/top` view (see `Dicks::get_top_by_net`): `net` is what gets
/// ranked/ordered by, `raw_length` is the player's actual (debt/credit-free) length, kept
/// alongside it so the line renderer can show the credit/debit breakdown instead of battle stats.
#[derive(sqlx::FromRow, Debug)]
pub struct NetPositionRow {
    pub net: i32,
    pub raw_length: i32,
    pub owner_uid: UID,
    pub owner_name: String,
    pub grown_at: chrono::DateTime<chrono::Utc>,
    pub position: Option<i64>,
}

/// A single player's net standing for `/stats`: their rank in the net-position leaderboard plus
/// the credit/debit split behind it (`credit` = total still owed *to* them across active p2p
/// loans; `debt` = total they still owe - bank loan + p2p loans + loan-interest tax debts). The
/// net value itself is `raw_length + credit - debt` (the same figure `get_top_by_net` ranks by).
#[derive(sqlx::FromRow, Debug)]
pub struct NetPosition {
    pub net: i32,
    pub credit: i32,
    pub debt: i32,
    pub position: Option<i64>,
}

pub struct GrowthResult {
    pub new_length: i32,
    pub pos_in_top: Option<u64>,
}

#[derive(Clone)]
pub struct Dicks {
    pool: Pool<Postgres>,
    chats: Chats,
    features: FeatureToggles,
}

impl Dicks {
    pub fn new(pool: Pool<Postgres>, features: FeatureToggles) -> Self {
        Self {
            chats: Chats::new(pool.clone(), features),
            pool,
            features,
        }
    }

    /// Starts a transaction on the same pool every other method here implicitly commits its own
    /// version of - exposed so a caller that needs to span more than one of those mutations in a
    /// single transaction (`crate::handlers::combo`) can get one without reaching into a `Pool`
    /// directly. Every repo struct is constructed from the very same pool (see
    /// `crate::repo::Repositories::new`), so it doesn't matter which one's `begin_tx` is used to
    /// start a transaction that ends up spanning calls into several of them.
    pub(crate) async fn begin_tx(&self) -> anyhow::Result<Transaction<'_, Postgres>> {
        self.pool.begin().await.map_err(Into::into)
    }

    pub async fn create_or_grow(&self, uid: UserId, chat_id: &ChatIdPartiality, increment: i32) -> anyhow::Result<GrowthResult> {
        let uid = uid.0 as i64;
        let internal_chat_id = self.chats.upsert_chat(chat_id).await?;
        let new_length = sqlx::query_scalar!(
            "INSERT INTO dicks(uid, chat_id, length, updated_at) VALUES ($1, $2, $3, current_timestamp)
                ON CONFLICT (uid, chat_id) DO UPDATE SET length = (dicks.length + $3), updated_at = current_timestamp
                RETURNING length",
                uid, internal_chat_id, increment)
            .fetch_one(&self.pool)
            .await
            .context(format!("couldn't upsert the dick of {uid} in {chat_id} with increment of {increment}"))?;
        let pos_in_top = self.get_position_in_top(internal_chat_id, uid).await?;
        Ok(GrowthResult { new_length, pos_in_top })
    }

    pub async fn fetch_length(&self, uid: UserId, chat_id: &ChatIdKind) -> anyhow::Result<i32> {
        sqlx::query_scalar!("SELECT d.length FROM Dicks d \
                JOIN Chats c ON d.chat_id = c.id \
                WHERE uid = $1 AND \
                    c.chat_id = $2::bigint OR c.chat_instance = $2::text",
                uid.0 as i64, chat_id.value() as String)
            .fetch_optional(&self.pool)
            .await
            .map(Option::unwrap_or_default)
            .context(format!("couldn't fetch length for {chat_id} and {uid}"))
    }

    pub async fn fetch_dick(&self, uid: UserId, chat_id: &ChatIdKind) -> anyhow::Result<Option<Dick>> {
        sqlx::query_as!(Dick,
            r#"SELECT length, uid as owner_uid, name as owner_name, updated_at as grown_at, position,
                    battles_total as "battles_total!", battles_won as "battles_won!" FROM (
                 SELECT d.uid, name, d.length as length, updated_at,
                        ROW_NUMBER() OVER (ORDER BY length DESC, updated_at DESC, name) AS position,
                        COALESCE(bs.battles_total, 0) AS battles_total,
                        COALESCE(bs.battles_won, 0) AS battles_won
                   FROM Dicks d
                   JOIN users using (uid)
                   JOIN Chats c ON d.chat_id = c.id
                   LEFT JOIN Battle_Stats bs ON bs.uid = d.uid AND bs.chat_id = d.chat_id
                   WHERE c.chat_id = $2::bigint OR c.chat_instance = $2::text
               ) AS _
               WHERE uid = $1"#,
                uid.0 as i64, chat_id.value() as String)
            .fetch_optional(&self.pool)
            .await
            .context(format!("couldn't fetch dick for {chat_id} and {uid}"))
    }

    pub async fn get_top(&self, chat_id: &ChatIdKind, offset: u32, limit: u32) -> anyhow::Result<Vec<Dick>> {
        sqlx::query_as!(Dick,
            r#"SELECT length, d.uid as owner_uid, name as owner_name, updated_at as grown_at,
                    ROW_NUMBER() OVER (ORDER BY length DESC, updated_at DESC, name) AS position,
                    COALESCE(bs.battles_total, 0) AS "battles_total!",
                    COALESCE(bs.battles_won, 0) AS "battles_won!"
                FROM dicks d
                JOIN users using (uid)
                JOIN chats c ON c.id = d.chat_id
                LEFT JOIN battle_stats bs ON bs.uid = d.uid AND bs.chat_id = d.chat_id
                WHERE c.chat_id = $1::bigint OR c.chat_instance = $1::text
                OFFSET $2 LIMIT $3"#,
                chat_id.value() as String, offset as i64, limit as i32)
            .fetch_all(&self.pool)
            .await
            .context(format!("couldn't get the top of {chat_id} with offset = {offset} and limit = {limit}"))
    }

    /// Like `get_top`, but ranked by net position rather than raw `length`: each player's length
    /// minus everything they owe (bank loan + P2P-loan-as-borrower + loan-interest tax debt) plus
    /// everything owed to them (P2P-loan-as-lender) - the debt/credit-aware view behind `/top`'s
    /// view-toggle button. No battle stats here (the line renderer shows the credit/debit
    /// breakdown in their place instead - see `NetPositionRow`), so no `battle_stats` join either.
    pub async fn get_top_by_net(&self, chat_id: &ChatIdKind, offset: u32, limit: u32) -> anyhow::Result<Vec<NetPositionRow>> {
        sqlx::query_as!(NetPositionRow,
            r#"SELECT net AS "net!", raw_length AS "raw_length!", owner_uid, owner_name, grown_at,
                    ROW_NUMBER() OVER (ORDER BY net DESC, grown_at DESC, owner_name) AS position
                FROM (
                    SELECT
                        (d.length
                            - COALESCE(bank.debt, 0)
                            - COALESCE(borrow.debt, 0)
                            + COALESCE(lend.debt, 0)
                            - COALESCE(tax.debt, 0)
                        )::int AS net,
                        d.length AS raw_length,
                        d.uid AS owner_uid, u.name AS owner_name, d.updated_at AS grown_at
                    FROM dicks d
                    JOIN users u ON u.uid = d.uid
                    JOIN chats c ON c.id = d.chat_id
                    LEFT JOIN loans bank ON bank.uid = d.uid AND bank.chat_id = d.chat_id AND bank.repaid_at IS NULL
                    LEFT JOIN (
                        SELECT borrower_uid, chat_id, SUM(debt) AS debt FROM p2p_loans
                            WHERE repaid_at IS NULL GROUP BY borrower_uid, chat_id
                    ) borrow ON borrow.borrower_uid = d.uid AND borrow.chat_id = d.chat_id
                    LEFT JOIN (
                        SELECT lender_uid, chat_id, SUM(debt) AS debt FROM p2p_loans
                            WHERE repaid_at IS NULL GROUP BY lender_uid, chat_id
                    ) lend ON lend.lender_uid = d.uid AND lend.chat_id = d.chat_id
                    LEFT JOIN (
                        SELECT payer_uid, chat_id, SUM(debt) AS debt FROM loan_interest_tax_debts
                            WHERE repaid_at IS NULL GROUP BY payer_uid, chat_id
                    ) tax ON tax.payer_uid = d.uid AND tax.chat_id = d.chat_id
                    WHERE c.chat_id = $1::bigint OR c.chat_instance = $1::text
                ) base
                ORDER BY position
                OFFSET $2 LIMIT $3"#,
                chat_id.value() as String, offset as i64, limit as i32)
            .fetch_all(&self.pool)
            .await
            .context(format!("couldn't get the net-position top of {chat_id} with offset = {offset} and limit = {limit}"))
    }

    /// One player's row out of the same net-position ranking `get_top_by_net` builds (so the
    /// `position` here matches that leaderboard exactly), plus the credit/debit breakdown behind
    /// their net - for the personal `/stats` view. `None` if the player has no dick in the chat.
    pub async fn get_net_position(&self, chat_id: &ChatIdKind, uid: UserId) -> anyhow::Result<Option<NetPosition>> {
        sqlx::query_as!(NetPosition,
            r#"SELECT net AS "net!", credit AS "credit!", debt AS "debt!", position FROM (
                    SELECT net, credit, debt, owner_uid,
                        ROW_NUMBER() OVER (ORDER BY net DESC, grown_at DESC, owner_name) AS position
                    FROM (
                        SELECT
                            (d.length
                                - COALESCE(bank.debt, 0)
                                - COALESCE(borrow.debt, 0)
                                + COALESCE(lend.debt, 0)
                                - COALESCE(tax.debt, 0)
                            )::int AS net,
                            COALESCE(lend.debt, 0)::int AS credit,
                            (COALESCE(bank.debt, 0) + COALESCE(borrow.debt, 0) + COALESCE(tax.debt, 0))::int AS debt,
                            d.uid AS owner_uid, u.name AS owner_name, d.updated_at AS grown_at
                        FROM dicks d
                        JOIN users u ON u.uid = d.uid
                        JOIN chats c ON c.id = d.chat_id
                        LEFT JOIN loans bank ON bank.uid = d.uid AND bank.chat_id = d.chat_id AND bank.repaid_at IS NULL
                        LEFT JOIN (
                            SELECT borrower_uid, chat_id, SUM(debt) AS debt FROM p2p_loans
                                WHERE repaid_at IS NULL GROUP BY borrower_uid, chat_id
                        ) borrow ON borrow.borrower_uid = d.uid AND borrow.chat_id = d.chat_id
                        LEFT JOIN (
                            SELECT lender_uid, chat_id, SUM(debt) AS debt FROM p2p_loans
                                WHERE repaid_at IS NULL GROUP BY lender_uid, chat_id
                        ) lend ON lend.lender_uid = d.uid AND lend.chat_id = d.chat_id
                        LEFT JOIN (
                            SELECT payer_uid, chat_id, SUM(debt) AS debt FROM loan_interest_tax_debts
                                WHERE repaid_at IS NULL GROUP BY payer_uid, chat_id
                        ) tax ON tax.payer_uid = d.uid AND tax.chat_id = d.chat_id
                        WHERE c.chat_id = $1::bigint OR c.chat_instance = $1::text
                    ) base
                ) ranked
                WHERE owner_uid = $2"#,
                chat_id.value() as String, uid.0 as i64)
            .fetch_optional(&self.pool)
            .await
            .context(format!("couldn't get the net position of {uid} in {chat_id}"))
    }

    pub async fn set_dod_winner(&self, chat_id: &ChatIdPartiality, user_id: UserId, bonus: u16) -> anyhow::Result<Option<GrowthResult>> {
        let internal_chat_id = self.chats.upsert_chat(chat_id).await?;

        let mut tx = self.pool.begin().await?;
        let uid = user_id.0 as i64;
        let new_length = match Self::grow_no_attempts_check_internal(&mut *tx, internal_chat_id, uid, bonus as i32).await? {
            Some(length) => length,
            None => return Ok(None)
        };
        Self::insert_to_dod_table(&mut tx, internal_chat_id, uid).await?;
        tx.commit().await?;

        let pos_in_top = self.get_position_in_top(internal_chat_id, uid).await?;
        Ok(Some(GrowthResult { new_length, pos_in_top }))
    }

    pub async fn check_dick(&self, chat_id: &ChatIdKind, user_id: UserId, length: u16) -> anyhow::Result<bool> {
        Self::check_dick_with(&self.pool, chat_id, user_id, length).await
    }

    /// Same check as `check_dick`, but against an arbitrary executor - in particular, an
    /// in-progress `Transaction` - so a caller juggling more than one mutation in the same
    /// transaction (see `crate::handlers::combo`) can check a balance that an earlier,
    /// not-yet-committed step in *that same transaction* already changed.
    pub(crate) async fn check_dick_with<'c, E>(executor: E, chat_id: &ChatIdKind, user_id: UserId, length: u16) -> anyhow::Result<bool>
    where E: Executor<'c, Database = Postgres>,
    {
        sqlx::query_scalar!(r#"SELECT length >= $3 AS "enough!" FROM Dicks d
                JOIN Chats c ON d.chat_id = c.id
                WHERE (c.chat_id = $1::bigint OR c.chat_instance = $1::text)
                    AND uid = $2"#,
                chat_id.value() as String, user_id.0 as i64, length as i32)
            .fetch_optional(executor)
            .map_ok(|opt| opt.unwrap_or(false))
            .await
            .context(format!("couldn't check the dick {chat_id}, {user_id} to have at least {length} cm"))
    }

    /// Resolves `chat_id` to its internal numeric id - exposed so a caller that needs to run
    /// several transfers against the same chat (again, `crate::handlers::combo`) only has to
    /// resolve it once and can reuse the result, rather than every helper re-resolving its own.
    pub(crate) async fn resolve_chat(&self, chat_id: &ChatIdPartiality) -> anyhow::Result<i64> {
        self.chats.upsert_chat(chat_id).await
    }

    /// A `(new_length, position)` pair for `uid`, given a length value already known (typically
    /// just written, e.g. by `move_length_in_tx`) - the position lookup is the only part that
    /// still needs a query. Exposed so callers that did the actual write through `..._in_tx`
    /// (and so already have the resulting length in hand) can build the same `GrowthResult` this
    /// struct's own `pub` methods return, without duplicating the position lookup themselves.
    pub(crate) async fn growth_result_after(&self, internal_chat_id: i64, uid: UserId, new_length: i32) -> anyhow::Result<GrowthResult> {
        let pos_in_top = self.get_position_in_top(internal_chat_id, uid.0 as i64).await?;
        Ok(GrowthResult { new_length, pos_in_top })
    }

    pub async fn move_length(&self, chat_id: &ChatIdPartiality, from: UserId, to: UserId, length: u16) -> anyhow::Result<(GrowthResult, GrowthResult)> {
        let internal_chat_id = self.resolve_chat(chat_id).await?;

        let mut tx = self.pool.begin().await?;
        let (length_from, length_to) = Self::move_length_in_tx(&mut tx, internal_chat_id, from, to, length).await?;
        tx.commit().await?;

        let gr_from = self.growth_result_after(internal_chat_id, from, length_from).await?;
        let gr_to = self.growth_result_after(internal_chat_id, to, length_to).await?;
        Ok((gr_from, gr_to))
    }

    /// The actual transfer behind `move_length`, against an externally owned `tx` that this
    /// function never commits - so a caller juggling more than one transfer in one transaction
    /// (`crate::handlers::combo`, for a true "both happen or neither does" guarantee across two
    /// otherwise-independent offers) can run this for each leg and only commit once both have
    /// succeeded. `move_length` itself is just this plus its own begin/commit around it.
    pub(crate) async fn move_length_in_tx(tx: &mut Transaction<'_, Postgres>, chat_id_internal: i64, from: UserId, to: UserId, length: u16) -> anyhow::Result<(i32, i32)> {
        let length_from = Self::move_length_for_one_user(tx, chat_id_internal, from.0, -(length as i32)).await?;
        let length_to = Self::move_length_for_one_user(tx, chat_id_internal, to.0, length as i32).await?;
        Ok((length_from, length_to))
    }

    async fn move_length_for_one_user(tx: &mut Transaction<'_, Postgres>, chat_id_internal: i64, user_id: u64, change: i32) -> anyhow::Result<i32> {
        sqlx::query_scalar!("UPDATE Dicks SET length = (length + $3), bonus_attempts = (bonus_attempts + 1) WHERE chat_id = $1 AND uid = $2 RETURNING length",
                    chat_id_internal, user_id as i64, change)
            .fetch_one(&mut **tx)
            .await
            .context(format!("couldn't update the length by {change} for {chat_id_internal}, {user_id}"))
    }

    async fn get_position_in_top(&self, chat_id_internal: i64, uid: i64) -> anyhow::Result<Option<u64>> {
        if !self.features.top_unlimited {
            return Ok(None)
        }
        sqlx::query_scalar!(
                r#"SELECT position AS "position!" FROM (
                    SELECT uid, ROW_NUMBER() OVER (ORDER BY length DESC, updated_at DESC, name) AS position
                    FROM dicks
                    JOIN users using (uid)
                    WHERE chat_id = $1
                ) AS _
                WHERE uid = $2"#,
                chat_id_internal, uid)
            .fetch_one(&self.pool)
            .await
            .map(|pos| Some(pos as u64))
            .context(format!("couldn't get the top for {chat_id_internal} and {uid}"))
    }
    
    pub async fn grow_no_attempts_check(&self, chat_id: &ChatIdKind, user_id: UserId, change: i32) -> anyhow::Result<GrowthResult> {
        let chat_internal_id = self.chats.get_internal_id(chat_id).await?;
        let uid = user_id.0 as i64;
    
        let new_length = Self::grow_no_attempts_check_internal(&self.pool, chat_internal_id, uid, change).await?
            .ok_or(anyhow!("couldn't find a dick of ({chat_id}, {uid}) for some reason"))?;
        let pos_in_top = self.get_position_in_top(chat_internal_id, uid).await?;
        
        Ok(GrowthResult { new_length, pos_in_top })
    }

    pub(super) async fn grow_no_attempts_check_internal<'c, E>(executor: E, chat_id_internal: i64, user_id: i64, bonus: i32) -> anyhow::Result<Option<i32>>
    where E: Executor<'c, Database = Postgres>,
    {
        sqlx::query_scalar!(
            "UPDATE Dicks SET bonus_attempts = (bonus_attempts + 1), length = (length + $3)
                WHERE chat_id = $1 AND uid = $2
                RETURNING length",
                chat_id_internal, user_id, bonus)
            .fetch_optional(executor)
            .await
            .context(format!("couldn't grow the dick without attempts check for {chat_id_internal} and {user_id} by {bonus}"))
    }

    /// The chat's already-chosen Dick of the Day winner for *today*, if any - used to build a
    /// proper, HTML-escaped error message when `set_dod_winner`'s underlying trigger rejects a
    /// second pick (`GD0E2`), instead of the caller trusting the trigger's raw exception message
    /// as a display-ready, pre-escaped name (a name coming straight from Telegram's first/last
    /// name fields, interpolated unescaped into an HTML-parsed message, would otherwise be an
    /// HTML-injection foothold).
    pub async fn get_today_dod_winner_name(&self, chat_id: &ChatIdKind) -> anyhow::Result<Option<String>> {
        sqlx::query_scalar!(
            r#"SELECT u.name FROM Dick_of_Day dod
                JOIN Users u ON dod.winner_uid = u.uid
                JOIN Chats c ON dod.chat_id = c.id
                WHERE dod.created_at = current_date AND (c.chat_id = $1::bigint OR c.chat_instance = $1::text)"#,
            chat_id.value() as String)
            .fetch_optional(&self.pool)
            .await
            .context(format!("couldn't get today's dod winner for {chat_id}"))
    }

    async fn insert_to_dod_table(tx: &mut Transaction<'_, Postgres>, chat_id_internal: i64, user_id: i64) -> anyhow::Result<()> {
        sqlx::query!("INSERT INTO Dick_of_Day (chat_id, winner_uid) VALUES ($1, $2)",
                chat_id_internal, user_id)
            .execute(&mut **tx)
            .await
            .context(format!("couldn't insert to DOD table for {chat_id_internal} and {user_id}"))?;
        Ok(())
    }
}
