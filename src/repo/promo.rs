use std::fmt::Debug;
use anyhow::{anyhow, Context};
use sqlx::{FromRow, Postgres};
use teloxide::types::UserId;
use crate::repository;

const PROMOCODE_ACTIVATIONS_PK: &str = "promo_code_activations_pkey";

pub struct ActivationResult {
    pub chats_affected: u64,
    pub bonus_length: i32,
    /// The internal `Chats.id` of every chat the bonus was actually applied to - the bonus is
    /// cross-chat (every chat the user has a `Dicks` row in), but debt settlement is per-chat
    /// (see `crate::handlers::promo::promo_activation_impl`), so the caller needs to know which
    /// chats to settle against, not just how many.
    pub affected_chat_internal_ids: Vec<i64>,
}

#[derive(Debug, strum_macros::Display)]
#[strum(serialize_all = "snake_case")]
pub enum ActivationError {
    NoActivationsLeft,
    NoDicks,
    AlreadyActivated,
    Other(anyhow::Error)
}

impl <T: Into<anyhow::Error>> From<T> for ActivationError {
    fn from(value: T) -> Self {
        Self::Other(anyhow!(value))
    }
}

#[cfg(test)]
pub struct PromoCodeParams {
    pub code: String,
    pub bonus_length: u32,
    pub capacity: u32,
}

#[derive(FromRow)]
struct PromoCodeInfo {
    found_code: String,
    bonus_length: i32,
}

repository!(Promo,
    #[cfg(test)]
    pub async fn create(&self, p: PromoCodeParams) -> anyhow::Result<()> {
        sqlx::query!("INSERT INTO Promo_Codes (code, bonus_length, capacity) VALUES ($1, $2, $3)",
                p.code, p.bonus_length as i32, p.capacity as i32)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
,
    pub async fn activate(&self, user_id: UserId, code: &str) -> Result<ActivationResult, ActivationError> {
        let mut tx = self.pool.begin().await?;

        let PromoCodeInfo { found_code, bonus_length } = Self::find_code_length_and_decr_capacity(&mut tx, code)
            .await?
            .ok_or(ActivationError::NoActivationsLeft)?;
        let affected_chat_internal_ids = Self::grow_dicks(&mut tx, user_id, bonus_length).await?;
        let chats_affected = affected_chat_internal_ids.len() as u64;
        if chats_affected < 1 {
            return Err(ActivationError::NoDicks)
        }
        Self::add_activation(&mut tx, user_id, &found_code, chats_affected)
            .await
            .map_err(|err| {
                match err.downcast() {
                    Ok(sqlx::Error::Database(e)) => {
                        e.constraint()
                            .filter(|c| c == &PROMOCODE_ACTIVATIONS_PK)
                            .map(|_| ActivationError::AlreadyActivated)
                            .unwrap_or(ActivationError::Other(e.into()))
                    },
                    Ok(e) => ActivationError::Other(anyhow!(e)),
                    Err(e) => ActivationError::Other(e)
                }
            })?;

        tx.commit().await?;
        Ok(ActivationResult{ chats_affected, bonus_length, affected_chat_internal_ids })
    }
,
    async fn find_code_length_and_decr_capacity(tx: &mut sqlx::Transaction<'_, Postgres>, code: &str) -> anyhow::Result<Option<PromoCodeInfo>> {
         sqlx::query_as!(PromoCodeInfo,
            "UPDATE Promo_Codes SET capacity = (capacity - 1)
                WHERE lower(code) = lower($1) AND capacity > 0 AND
                    (current_date BETWEEN since AND until
                    OR
                    current_date >= since AND until IS NULL)
                RETURNING bonus_length, code as found_code",
                code)
            .fetch_optional(&mut **tx)
            .await
            .context(format!("couldn't find a promo code length of {code}"))
    }
,
    /// Unlike most other gains, a promo bonus is cross-chat in one shot - every chat the user
    /// has a `Dicks` row in gets it. Debt settlement, by contrast, is per-chat (see
    /// `crate::handlers::debt_settlement::settle_gain_against_debts`), so the caller needs each
    /// affected chat's internal id to settle against, not just a count - hence `RETURNING
    /// chat_id` instead of the plain row count this used to return.
    async fn grow_dicks(tx: &mut sqlx::Transaction<'_, Postgres>, user_id: UserId, bonus: i32) -> anyhow::Result<Vec<i64>> {
        let chat_internal_ids = sqlx::query_scalar!("UPDATE Dicks SET bonus_attempts = (bonus_attempts + 1), length = (length + $2) WHERE uid = $1 RETURNING chat_id",
                user_id.0 as i64, bonus)
            .fetch_all(&mut **tx)
            .await
            .context(format!("couldn't grow dicks of {user_id} by {bonus}"))?;
        Ok(chat_internal_ids)
    }
,
    async fn add_activation(tx: &mut sqlx::Transaction<'_, Postgres>, uid: UserId, code: &str, affected_chats: u64) -> anyhow::Result<()> {
        let affected_chats: i32 = affected_chats.try_into()?;
        sqlx::query!("INSERT INTO Promo_Code_Activations (uid, code, affected_chats) VALUES ($1, $2, $3)",
                uid.0 as i64, code, affected_chats)
            .execute(&mut **tx)
            .await
            .context(format!("couldn't insert a promo code activation for {uid} and {code} with {affected_chats} affected chats"))?;
        Ok(())
    }
);
