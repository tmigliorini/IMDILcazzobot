use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use rand::Rng;
use rand::rngs::OsRng;
use teloxide::types::UserId;
use crate::handlers::amount_picker::OfferKind;

/// How long an abandoned wizard session is kept around before `create`'s lazy sweep evicts it -
/// shorter than the pending-offer lifetime in `repo::ComboOffers` (a wizard is a single person clicking through several screens in
/// one sitting, not a pending offer waiting for someone else to act).
const MAX_PENDING_AGE: Duration = Duration::from_secs(60 * 60);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum WizardMode {
    Single,
    Combo,
}

/// One leg's progress through the wizard. Every field starts empty/`None` and is filled in
/// screen by screen - `wizard::render` looks at which of these are still unset to decide which
/// screen to show next, so there's no separate "current step" field to keep in sync.
#[derive(Clone, Default)]
pub(crate) struct LegState {
    pub(crate) kind: Option<OfferKind>,

    /// Only meaningful once `kind` is `Donate`/`Presta` (pvp has no "pull" concept) - `true`
    /// means a request (donate: asking for ghei; presta: asking to borrow) rather than an offer.
    pub(crate) is_pull: bool,

    /// In-progress digits for the amount keypad, not yet committed.
    pub(crate) amount_buf: String,
    pub(crate) amount: Option<u16>,

    /// Only meaningful while entering a custom rate (presta) - flips the sign of `rate_buf` once
    /// committed.
    pub(crate) rate_is_negative: bool,
    /// In-progress digits for the rate/probability keypad, not yet committed.
    pub(crate) rate_buf: String,
    /// `None` = this leg hasn't reached/resolved the rate-or-probability screen yet (only shown
    /// for pvp's probability and presta's rate; donate skips it - see `LegState::needs_rate`).
    /// `Some(None)` = explicitly chose the default/standard value. `Some(Some(v))` = an explicit
    /// custom value (already signed, for presta's negative-rate case).
    pub(crate) rate_or_prob: Option<Option<f64>>,
}

impl LegState {
    pub(crate) fn needs_rate_screen(&self) -> bool {
        matches!(self.kind, Some(OfferKind::Pvp) | Some(OfferKind::Presta))
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.kind.is_some() && self.amount.is_some() && (!self.needs_rate_screen() || self.rate_or_prob.is_some())
    }
}

pub(crate) struct WizardState {
    pub(crate) owner: UserId,
    pub(crate) mode: Option<WizardMode>,
    pub(crate) leg1: LegState,
    pub(crate) leg2: LegState,

    /// Fetched once, the first time the wizard reaches the target screen - a name picked there
    /// is looked up by its index into this list (kept short in `callback_data`, see
    /// `wizard::Action::TargetPick`).
    pub(crate) target_candidates: Option<Vec<(UserId, String)>>,
    pub(crate) target_page: u32,
    /// `None` = not decided yet. `Some(None)` = open to anyone. `Some(Some(uid))` = a specific
    /// target picked from `target_candidates`.
    pub(crate) target: Option<Option<UserId>>,

    created_at: Instant,
}

impl WizardState {
    fn new(owner: UserId) -> Self {
        Self {
            owner,
            mode: None,
            leg1: LegState::default(),
            leg2: LegState::default(),
            target_candidates: None,
            target_page: 0,
            target: None,
            created_at: Instant::now(),
        }
    }

    pub(crate) fn leg(&self, n: u8) -> &LegState {
        if n == 2 { &self.leg2 } else { &self.leg1 }
    }

    pub(crate) fn leg_mut(&mut self, n: u8) -> &mut LegState {
        if n == 2 { &mut self.leg2 } else { &mut self.leg1 }
    }
}

/// Holds an in-progress wizard session behind a short random token, the same reasoning as
/// `repo::ComboOffers` (a session has far more fields than Telegram's 64-byte
/// `callback_data` could ever carry directly) - but unlike a pending offer, a wizard session
/// has exactly one owner who's allowed to touch it at all, checked on every access via
/// `with_state`.
#[derive(Clone, Default)]
pub struct WizardStore {
    inner: Arc<Mutex<HashMap<String, WizardState>>>,
}

impl WizardStore {
    pub(crate) fn create(&self, owner: UserId) -> String {
        let mut map = self.inner.lock().unwrap();
        map.retain(|_, s| s.created_at.elapsed() < MAX_PENDING_AGE);
        let token = format!("{:016x}", OsRng.gen::<u64>());
        map.insert(token.clone(), WizardState::new(owner));
        token
    }

    /// Runs `f` against the stored session for `token`, only if `requester` is its owner -
    /// `None` if the token is gone or belongs to someone else. Kept synchronous on purpose (no
    /// `.await` ever happens while the lock is held): callers needing async work (e.g. fetching
    /// the target candidate list) must do it between two short `with_state` calls instead.
    pub(crate) fn with_state<T>(&self, token: &str, requester: UserId, f: impl FnOnce(&mut WizardState) -> T) -> Option<T> {
        let mut map = self.inner.lock().unwrap();
        let state = map.get_mut(token)?;
        if state.owner != requester {
            return None;
        }
        Some(f(state))
    }

    pub(crate) fn remove(&self, token: &str) {
        self.inner.lock().unwrap().remove(token);
    }
}
