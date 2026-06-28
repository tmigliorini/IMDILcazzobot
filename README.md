[@DickGrowerBot](https://t.me/DickGrowerBot)
============================================

[![CI Build](https://github.com/kozalosev/DickGrowerBot/actions/workflows/ci-build.yaml/badge.svg?branch=main&event=push)](https://github.com/kozalosev/DickGrowerBot/actions/workflows/ci-build.yaml) [![@DickGrowerBot MAU](https://tgbotmau.quoi.dev/api/bot/DickGrowerBot/mau/badge?style=flat "@DickGrowerBot MAU")](https://tgbotmau.quoi.dev/?bot=DickGrowerBot)

A game bot for group chats that let its users grow their virtual "dicks" every day for some random count of centimeters (including negative values) and compete with friends and other chat members.

Additional mechanics
--------------------
_(compared with some competitors)_

* **The Dick of the Day** daily contest to grow a randomly chosen dick for a bit more.
* A way to play the game without the necessity to add the bot into a group (via inline queries with a callback button).
* Import from _@pipisabot_ and _@kraft28_bot_ (not tested! help of its users is required).
* **PvP fights with statistics**, including win/lose streaks and asymmetric ("skewed") odds: a player can self-handicap to a custom win probability, with the payout scaled accordingly.
* A bank **loan** for whoever's in the red, automatically repaid out of future growth.
* **Peer-to-peer loans** (`/presta`) between players, with configurable (even negative) interest, automatically repaid out of the borrower's future growth.
* A daily **tax** (`/tax`) that redistributes length from the chat's top players to its bottom ones.
* A **personal ledger** (`/estratto`) of every gain/loss, and an aggregated economic report for the whole chat.
* **Combo offers**: two offers (battle/donation/loan, in any combination) bundled into one atomic "both or nothing" deal.
* A guided, step-by-step **wizard** (and a quick amount-picker) for building any of the above offers from the inline listone, without needing to remember the free-text syntax.

Features
--------
* true system random from the environment's chaos by usage of the `get_random()` syscall (`BCryptGenRandom` on Windows, or other alternatives on different OSes);
* fully translated into Lombard (`lmo`) - the bot's only active locale right now; the underlying i18n infrastructure can still support more languages (see `src/domain/langcode.rs`);
* Prometheus-like metrics.

Technical stuff
---------------

### Requirements to run
* PostgreSQL;
* _\[optional]_ Docker (it makes the configuration a lot easier);
* _\[for webhook mode]_ a frontal proxy server with TLS support ([nginx-proxy](https://github.com/nginx-proxy/nginx-proxy), for example).

### How to rebuild .sqlx queries?
_(to build the application without a running RDBMS)_

```shell
cargo sqlx prepare -- --tests
```

### Adjustment hints

It's most probably you want to change the value of the `GROW_SHRINK_RATIO` environment variable to make the players upset and disappointed more or less often.

The economic mechanics (bank/P2P loans, tax) are all opt-in or separately tunable via their own environment variables - see `.env.example` for the full list (interest rates, payout ratios, tax ranks/rate, default amounts for the inline pickers, etc.).

### How to disable a command?

Most of the command can be hidden from both lists: command hints and inline results. To do so, specify an environment variable like `DISABLE_CMD_STATS` (where `STATS` is a command key) with any value.
Don't forget to pass this variable to the container by adding it to the `docker-compose.yml` file!

Combo offers have no `/command` of their own (they're recognized from free-text inline syntax), but can still be disabled the same way via `DISABLE_CMD_COMBO`.

### Help & syntax

`/help` (and the inline "Informazion" menu) gives a short overview; `/syntax` (or the matching inline menu entry) documents the full free-text syntax for battles, donations, P2P loans, tax and combo offers - always up to date, since it's loaded from `bot-text/syntax.html` at startup rather than baked into the binary.
