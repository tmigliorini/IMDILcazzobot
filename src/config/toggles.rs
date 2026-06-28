use std::collections::HashMap;
use std::sync::{Arc, RwLock};

#[derive(Copy, Clone, Default, derive_more::FromStr, derive_more::Display)]
#[allow(clippy::upper_case_acronyms)]
pub enum DickOfDaySelectionMode {
    WEIGHTS,
    EXCLUSION,
    #[default]
    RANDOM
}

#[derive(Clone, Copy)]
pub struct FeatureToggles {
    pub chats_merging: bool,
    pub top_unlimited: bool,
    pub multiple_loans: bool,
    pub dod_selection_mode: DickOfDaySelectionMode,
    pub pvp: BattlesFeatureToggles,
}

#[cfg(test)]
impl Default for FeatureToggles {
    fn default() -> Self {
        Self {
            chats_merging: true,
            top_unlimited: true,
            multiple_loans: false,
            dod_selection_mode: Default::default(),
            pvp: Default::default(),
        }
    }
}

#[derive(Copy, Clone, Default)]
pub struct BattlesFeatureToggles {
    pub check_acceptor_length: bool,
    pub callback_locks: bool,
    pub show_stats: bool,
    pub show_stats_notice: bool,
    pub skill_based_probability: bool,
}

#[derive(Clone, Default)]
pub struct CachedEnvToggles {
    map: Arc<RwLock<HashMap<String, bool>>>
}

impl CachedEnvToggles {
    pub fn enabled(&self, key: &str) -> bool {
        log::debug!("trying to take a read lock for key '{key}'...");
        // a poisoned lock just means some other thread panicked while holding it; the map itself
        // (plain bools keyed by command name) is never left in a half-written state by a single
        // insert, so recovering the guard instead of propagating the panic here is safe.
        let maybe_enabled = self.map.read().unwrap_or_else(|poisoned| poisoned.into_inner()).get(key).copied();
        // maybe_enabled is required to drop the read lock
        maybe_enabled.unwrap_or_else(|| {
            let enabled = Self::enabled_in_env(key);
            log::debug!("trying to take a write lock for key '{key}'...");
            self.map.write().unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(key.to_owned(), enabled);
            enabled
        })
    }

    fn enabled_in_env(key: &str) -> bool {
        std::env::var_os(format!("DISABLE_CMD_{}", key.to_uppercase())).is_none()
    }
}
