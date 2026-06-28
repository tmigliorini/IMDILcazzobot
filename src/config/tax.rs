use crate::config::env::get_env_value_or_default;

#[derive(Clone, Default)]
pub struct TaxConfig {
    /// how many of the top players get taxed
    pub top_ranks: usize,
    /// the maximum possible tax rate (0.0-1.0) of a player's own length; actually charged in
    /// proportion to how far that player's length stands out from the rest of the taxed group
    /// (see the distance-based weighting in the tax handler)
    pub max_rate: f64,
    /// how many of the bottom players receive a share of the collected pool, proportional to
    /// how far below the benchmark (the player just above this group) each of them is
    pub bottom_ranks: usize,
}

impl TaxConfig {
    pub fn from_env() -> Self {
        let top_ranks = get_env_value_or_default("TAX_TOP_RANKS", 0usize);
        let max_rate = get_env_value_or_default("TAX_MAX_RATE", 0.5);
        let bottom_ranks = get_env_value_or_default("TAX_BOTTOM_RANKS", 0usize);
        Self { top_ranks, max_rate, bottom_ranks }
    }

    pub fn is_enabled(&self) -> bool {
        self.top_ranks > 0 && self.bottom_ranks > 0
    }
}
