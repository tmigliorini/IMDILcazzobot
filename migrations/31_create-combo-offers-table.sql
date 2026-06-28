DO $$ BEGIN
    CREATE TYPE combo_offer_leg_kind AS ENUM (
        'pvp',
        'donate',
        'p2ploan'
    );
EXCEPTION
    WHEN duplicate_object THEN null;
END $$;

-- A combo offer needs to survive a bot restart just like every other pending offer, but unlike a
-- single-leg one (whose full parameters fit in its own Accept button's callback_data - see
-- offer_keyboard() in the Rust code), two legs' worth of fields don't fit in Telegram's 64-byte
-- callback_data limit, so the offer itself has to live here, addressed by a short random token
-- instead. The flat leg1_*/leg2_* columns (rather than one row per offer type) are generic enough
-- to represent any combination of the three leg kinds without a JSON column or a table per kind.
CREATE TABLE IF NOT EXISTS ComboOffers(
    token varchar PRIMARY KEY,
    proposer_uid bigint NOT NULL,
    target_uid bigint,
    leg1_kind combo_offer_leg_kind NOT NULL,
    leg1_amount integer NOT NULL,
    leg1_rate_pct double precision,
    leg2_kind combo_offer_leg_kind NOT NULL,
    leg2_amount integer NOT NULL,
    leg2_rate_pct double precision,
    created_at timestamptz NOT NULL DEFAULT current_timestamp
);

CREATE INDEX IF NOT EXISTS idx_combooffers_created_at ON ComboOffers(created_at);

COMMENT ON COLUMN ComboOffers.leg1_amount IS 'Signed for donate/p2ploan (negative = a pull/request); always non-negative for pvp (the bet)';
COMMENT ON COLUMN ComboOffers.leg1_rate_pct IS 'pvp: an explicit win-probability override. p2ploan: an explicit interest-rate override. NULL means the configured default, or (pvp) the standard 50/50 odds. Unused for donate.';
