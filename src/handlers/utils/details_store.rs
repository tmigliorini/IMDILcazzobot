use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use rand::Rng;
use rand::rngs::OsRng;
use teloxide::types::UserId;

/// How long an unclaimed "Dettagli" token is kept around before `insert`'s lazy sweep evicts it -
/// mirrors `crate::repo::combo_offers`'s pending-offer TTL reasoning: generous on
/// purpose, this only guards against truly abandoned buttons.
const MAX_PENDING_AGE: Duration = Duration::from_secs(24 * 60 * 60);

struct StoredDetails {
    /// `None` means anyone may expand it (a PvP/donate/presta/dod result names other people, not
    /// just whoever happens to tap the button - there's no single "owner"); `Some` gates it to a
    /// single user (grow's own perks/position breakdown is genuinely personal).
    owner: Option<UserId>,
    text: String,
    created_at: Instant,
}

/// Holds the full (short + deferred-details) text of a result behind a short random token, so a
/// "📊 Dettagli" button's callback_data only ever has to carry that token - exactly the same
/// reasoning as `crate::repo::ComboOffers`, just for display text
/// instead of a pending offer. Unlike a combo offer, a Dettagli token is never "spent": tapping it
/// only reveals text, so it stays in the map until it ages out.
#[derive(Clone, Default)]
pub struct DetailsStore {
    inner: Arc<Mutex<HashMap<String, StoredDetails>>>,
}

impl DetailsStore {
    /// Stores `text` under a fresh token and returns it. Opportunistically sweeps out anything
    /// older than `MAX_PENDING_AGE` first, so abandoned entries don't accumulate forever without
    /// needing a background task.
    pub(crate) fn insert(&self, owner: Option<UserId>, text: String) -> String {
        let mut map = self.inner.lock().unwrap();
        map.retain(|_, d| d.created_at.elapsed() < MAX_PENDING_AGE);
        let token = format!("{:016x}", OsRng.gen::<u64>());
        map.insert(token.clone(), StoredDetails { owner, text, created_at: Instant::now() });
        token
    }

    /// The stored text, if `token` exists and `requester` is allowed to see it (always true for
    /// an ungated entry).
    pub(crate) fn get(&self, token: &str, requester: UserId) -> Option<String> {
        let map = self.inner.lock().unwrap();
        map.get(token)
            .filter(|d| d.owner.map_or(true, |owner| owner == requester))
            .map(|d| d.text.clone())
    }
}
