use anyhow::{anyhow, Context};
use teloxide::types::UserId;
use crate::repo::dicks::Dicks;
use crate::repo::ChatIdPartiality;
#[cfg(test)]
use crate::repo::ChatIdKind;
use crate::repository;

repository!(TaxRepo, with_(chats)_(Chats),
    /// Records today's tax event for the chat and applies all the deltas atomically.
    /// Returns `false` if the chat has already been taxed today (nothing is changed in that case).
    pub async fn tax_chat(&self, chat_id: &ChatIdPartiality, deltas: &[(UserId, i32)]) -> anyhow::Result<bool> {
        let internal_chat_id = self.chats.upsert_chat(chat_id).await?;
        let mut tx = self.pool.begin().await?;

        let inserted = sqlx::query!("INSERT INTO Tax_Log (chat_id) VALUES ($1) ON CONFLICT DO NOTHING", internal_chat_id)
            .execute(&mut *tx)
            .await
            .context(format!("couldn't record a tax event for {chat_id}"))?
            .rows_affected() > 0;
        if !inserted {
            return Ok(false)
        }

        for &(uid, delta) in deltas {
            Dicks::grow_no_attempts_check_internal(&mut *tx, internal_chat_id, uid.0 as i64, delta).await?
                .ok_or_else(|| anyhow!("couldn't find a dick of ({chat_id}, {uid}) while applying the tax"))?;
        }

        tx.commit().await?;
        Ok(true)
    }
,
    #[cfg(test)]
    pub async fn was_taxed_today(&self, chat_id: &ChatIdKind) -> anyhow::Result<bool> {
        sqlx::query_scalar!(
            r#"SELECT EXISTS(SELECT 1 FROM Tax_Log tl JOIN Chats c ON tl.chat_id = c.id
                WHERE (c.chat_id = $1::bigint OR c.chat_instance = $1::text) AND tl.created_at = current_date) AS "exists!""#,
            chat_id.value() as String)
            .fetch_one(&self.pool)
            .await
            .map_err(Into::into)
    }
);
