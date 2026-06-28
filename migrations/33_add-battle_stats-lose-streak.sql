-- Symmetric to `win_streak_current`/`win_streak_max` (see migration 17): a player's current run
-- of consecutive losses, and the longest such run ever - so a battle result can announce a lost
-- streak just snapped by a win, mirroring how a win streak ending was already announced.
ALTER TABLE Battle_Stats ADD COLUMN lose_streak_current smallint NOT NULL DEFAULT 0 CHECK ( lose_streak_current >= 0 );
ALTER TABLE Battle_Stats ADD COLUMN lose_streak_max     smallint NOT NULL DEFAULT 0 CHECK ( lose_streak_max >= lose_streak_current );

CREATE OR REPLACE FUNCTION update_lose_streak_max_if_needed()
    RETURNS TRIGGER
    LANGUAGE PLPGSQL
AS $$
BEGIN
    IF NEW.lose_streak_current > NEW.lose_streak_max THEN
        NEW.lose_streak_max = NEW.lose_streak_current;
    END IF;
    RETURN NEW;
END
$$;

CREATE OR REPLACE TRIGGER trg_update_lose_streak_max_if_needed BEFORE INSERT OR UPDATE ON Battle_Stats
    FOR EACH ROW EXECUTE FUNCTION update_lose_streak_max_if_needed();
