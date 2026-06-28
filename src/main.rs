mod domain;
mod handlers;
mod repo;
mod help;
mod metrics;
mod config;
mod commands;
mod external_text;

use std::env::VarError;
use std::net::SocketAddr;
use futures::future::join_all;
use reqwest::Url;
use rust_i18n::i18n;
use teloxide::dispatching::dialogue::InMemStorage;
use teloxide::prelude::*;
use teloxide::dptree::deps;
use teloxide::update_listeners::webhooks::{axum_to_router, Options};
use teloxide::update_listeners::UpdateListener;
use crate::handlers::{checks, HelpCommands, LoanCommands, PrivacyCommands, PromoCommandState, StartCommands};
use crate::handlers::{DickCommands, DickOfDayCommands, ImportCommands, PromoCommands};
use crate::handlers::pvp::{BattleCommands, BattleCommandsNoArgs};
use crate::handlers::donate::{DonateCommands, DonateCommandsNoArgs};
use crate::handlers::tax::TaxCommands;
use crate::handlers::syntax::SyntaxCommands;
use crate::handlers::p2p_loan::{P2PLoanCommands, P2PLoanCommandsNoArgs, P2PLoanStatusCommands};
use crate::handlers::stats::StatsCommands;
use crate::handlers::statement::StatementCommands;
use crate::handlers::utils::locks::LockCallbackServiceFacade;
use crate::handlers::utils::details_store::DetailsStore;
use crate::handlers::utils::wizard_store::WizardStore;

const ENV_WEBHOOK_URL: &str = "WEBHOOK_URL";

i18n!(fallback = "lmo");    // load localizations with default parameters

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(debug_assertions)]
    dotenvy::dotenv()?;

    pretty_env_logger::init();

    let app_config = config::AppConfig::from_env();
    let database_config = config::DatabaseConfig::from_env()?;
    let db_conn = repo::establish_database_connection(&database_config).await?;

    let handler = dptree::entry()
        .branch(Update::filter_message().filter_command::<StartCommands>().endpoint(handlers::start_cmd_handler))
        .branch(Update::filter_message().filter_command::<HelpCommands>().endpoint(handlers::help_cmd_handler))
        .branch(Update::filter_message().filter_command::<PrivacyCommands>().endpoint(handlers::privacy_cmd_handler))
        .branch(Update::filter_message().filter_command::<DickCommands>().filter(checks::is_group_chat).endpoint(handlers::dick_cmd_handler))
        .branch(Update::filter_message().filter_command::<DickOfDayCommands>().filter(checks::is_group_chat).endpoint(handlers::dod_cmd_handler))
        .branch(Update::filter_message().filter_command::<BattleCommands>().filter(checks::is_group_chat).endpoint(handlers::pvp::cmd_handler))
        .branch(Update::filter_message().filter_command::<BattleCommandsNoArgs>().filter(checks::is_group_chat).endpoint(handlers::pvp::cmd_handler_no_args))
        .branch(Update::filter_message().filter_command::<DonateCommands>().filter(checks::is_group_chat).endpoint(handlers::donate::cmd_handler))
        .branch(Update::filter_message().filter_command::<DonateCommandsNoArgs>().filter(checks::is_group_chat).endpoint(handlers::donate::cmd_handler_no_args))
        .branch(Update::filter_message().filter_command::<TaxCommands>().filter(checks::is_group_chat).endpoint(handlers::tax::cmd_handler))
        .branch(Update::filter_message().filter_command::<P2PLoanCommands>().filter(checks::is_group_chat).endpoint(handlers::p2p_loan::cmd_handler))
        .branch(Update::filter_message().filter_command::<P2PLoanCommandsNoArgs>().filter(checks::is_group_chat).endpoint(handlers::p2p_loan::cmd_handler_no_args))
        .branch(Update::filter_message().filter_command::<P2PLoanStatusCommands>().filter(checks::is_group_chat).endpoint(handlers::p2p_loan::status_cmd_handler))
        .branch(Update::filter_message().filter_command::<SyntaxCommands>().endpoint(handlers::syntax::cmd_handler))
        .branch(Update::filter_message().filter_command::<StatsCommands>().endpoint(handlers::stats::cmd_handler))
        .branch(Update::filter_message().filter_command::<LoanCommands>().filter(checks::is_group_chat).endpoint(handlers::loan::cmd_handler))
        .branch(Update::filter_message().filter_command::<StatementCommands>().filter(checks::is_group_chat).endpoint(handlers::statement::cmd_handler))
        .branch(Update::filter_message().filter_command::<ImportCommands>().filter(checks::is_group_chat).endpoint(handlers::import_cmd_handler))
        .branch(Update::filter_message().filter_command::<PromoCommands>().filter(checks::is_not_group_chat).enter_dialogue::<Message, InMemStorage<PromoCommandState>, PromoCommandState>()
            .branch(dptree::case![PromoCommandState::Start].endpoint(handlers::promo_cmd_handler)))
        .branch(Update::filter_message().enter_dialogue::<Message, InMemStorage<PromoCommandState>, PromoCommandState>()
            .branch(dptree::case![PromoCommandState::Requested].endpoint(handlers::promo_requested_handler)))
        .branch(Update::filter_message().filter(checks::is_not_group_chat).endpoint(checks::handle_not_group_chat))
        // combo must be checked before pvp/donate/p2p_loan: pvp's bare-number fallback is
        // permissive enough to also match a combo query whole (treating " combo ..." as a
        // target name), so it has to lose that race on purpose.
        .branch(Update::filter_inline_query().filter(checks::inline::is_group_chat).filter(handlers::combo::inline_filter).endpoint(handlers::combo::inline_handler))
        .branch(Update::filter_inline_query().filter(checks::inline::is_group_chat).filter(handlers::pvp::inline_filter).endpoint(handlers::pvp::inline_handler))
        .branch(Update::filter_inline_query().filter(checks::inline::is_group_chat).filter(handlers::donate::inline_filter).endpoint(handlers::donate::inline_handler))
        .branch(Update::filter_inline_query().filter(checks::inline::is_group_chat).filter(handlers::p2p_loan::inline_filter).endpoint(handlers::p2p_loan::inline_handler))
        .branch(Update::filter_inline_query().filter(handlers::promo_inline_filter).endpoint(handlers::promo_inline_handler))
        .branch(Update::filter_inline_query().filter(checks::inline::is_group_chat).endpoint(handlers::inline_handler))
        .branch(Update::filter_inline_query().filter(checks::inline::is_not_group_chat).endpoint(checks::inline::handle_not_group_chat))
        .branch(Update::filter_chosen_inline_result().filter(handlers::pvp::chosen_inline_result_filter).endpoint(handlers::pvp::inline_chosen_handler))
        .branch(Update::filter_chosen_inline_result().filter(handlers::donate::chosen_inline_result_filter).endpoint(handlers::donate::inline_chosen_handler))
        .branch(Update::filter_chosen_inline_result().filter(handlers::p2p_loan::chosen_inline_result_filter).endpoint(handlers::p2p_loan::inline_chosen_handler))
        .branch(Update::filter_chosen_inline_result().filter(handlers::combo::chosen_inline_result_filter).endpoint(handlers::combo::inline_chosen_handler))
        .branch(Update::filter_chosen_inline_result().endpoint(handlers::inline_chosen_handler))
        .branch(Update::filter_callback_query().filter(handlers::page_callback_filter).endpoint(handlers::page_callback_handler))
        .branch(Update::filter_callback_query().filter(handlers::cancel_offer_callback_filter).endpoint(handlers::cancel_offer_callback_handler))
        .branch(Update::filter_callback_query().filter(handlers::reject_offer_callback_filter).endpoint(handlers::reject_offer_callback_handler))
        .branch(Update::filter_callback_query().filter(handlers::info::callback_filter).endpoint(handlers::info::callback_handler))
        .branch(Update::filter_callback_query().filter(handlers::amount_picker::callback_filter).endpoint(handlers::amount_picker::callback_handler))
        .branch(Update::filter_callback_query().filter(handlers::details::callback_filter).endpoint(handlers::details::callback_handler))
        .branch(Update::filter_callback_query().filter(handlers::wizard::callback_filter).endpoint(handlers::wizard::callback_handler))
        .branch(Update::filter_callback_query().filter(handlers::pvp::callback_filter).endpoint(handlers::pvp::callback_handler))
        .branch(Update::filter_callback_query().filter(handlers::donate::callback_filter).endpoint(handlers::donate::callback_handler))
        .branch(Update::filter_callback_query().filter(handlers::p2p_loan::callback_filter).endpoint(handlers::p2p_loan::callback_handler))
        .branch(Update::filter_callback_query().filter(handlers::p2p_loan::debiti_callback_filter).endpoint(handlers::p2p_loan::debiti_callback_handler))
        .branch(Update::filter_callback_query().filter(handlers::loan::callback_filter).endpoint(handlers::loan::callback_handler))
        .branch(Update::filter_callback_query().filter(handlers::statement::callback_filter).endpoint(handlers::statement::callback_handler))
        .branch(Update::filter_callback_query().filter(handlers::combo::callback_filter).endpoint(handlers::combo::callback_handler))
        .branch(Update::filter_callback_query().endpoint(handlers::callback_handler));

    let bot = Bot::from_env();
    bot.delete_webhook().await?;

    // The bot speaks only Lombard now. Telegram's setMyCommands only accepts real IETF language
    // codes ("lmo" isn't one and gets rejected with "Bad Request: invalid language code specified"),
    // so commands are registered once under "" (the global default shown to every user), resolved
    // through the i18n fallback to Lombard.
    let set_my_commands_requests = [commands::set_my_commands(&bot, "", &app_config.command_toggles)];
    let set_my_commands_failed = join_all(set_my_commands_requests)
        .await
        .into_iter()
        .any(|res| res.is_err());
    if set_my_commands_failed {
        Err("couldn't set the bot's commands")?
    }

    let me = bot.get_me().await?;
    let repos = repo::Repositories::new(&db_conn, &app_config);

    // one-off retrofit of mutual-debt netting onto debts that predate the on-creation netting:
    // gated by an env flag so it only runs on the deploy that's meant to clean up history, and
    // idempotent anyway (a second run finds nothing left to net - see
    // `P2PLoans::rationalize_all_mutual_debts`). Set RATIONALIZE_MUTUAL_DEBTS=true for that one
    // deploy, then drop it.
    if std::env::var("RATIONALIZE_MUTUAL_DEBTS").map(|v| v == "true" || v == "1").unwrap_or(false) {
        match repos.p2p_loans.rationalize_all_mutual_debts().await {
            Ok((pairs, cancelled)) => log::info!("mutual-debt rationalization done: netted {pairs} pair(s), cancelled {cancelled} ghei of gross debt"),
            Err(e) => log::error!("mutual-debt rationalization failed: {e}"),
        }
    }

    let perks = handlers::perks::all(&db_conn, &app_config);
    let incrementor = handlers::utils::Incrementor::from_env(&repos.dicks, perks);
    let help_context = config::build_context_for_help_messages(me, &incrementor, &handlers::ORIGINAL_BOT_USERNAMES)?;
    let help_container = help::render_help_messages(help_context)?;
    let bot_text_dir = std::env::var("BOT_TEXT_DIR").unwrap_or_else(|_| "bot-text".to_owned());
    let external_texts = external_text::ExternalTexts::load(&bot_text_dir)
        .map_err(|e| format!("couldn't load the external texts from '{bot_text_dir}': {e}"))?;
    let battle_locker = LockCallbackServiceFacade::from_config(app_config.features);
    let details_store = DetailsStore::default();
    let wizard_store = WizardStore::default();

    let webhook_url: Option<Url> = match std::env::var(ENV_WEBHOOK_URL) {
        Ok(env_url) if !env_url.is_empty() => Some(env_url.parse()?),
        Ok(env_url) if env_url.is_empty() => None,
        Err(VarError::NotPresent) => None,
        _ => Err("invalid webhook URL!")?
    };
    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));
    let metrics_router = metrics::init();

    let ignore_unknown_updates = |_| Box::pin(async {});
    let deps = deps![
        repos,
        incrementor,
        app_config,
        help_container,
        external_texts,
        battle_locker,
        details_store,
        wizard_store,
        InMemStorage::<PromoCommandState>::new()
    ];

    match webhook_url {
        Some(url) => {
            log::info!("Setting a webhook: {url}");

            let (mut listener, stop_flag, bot_router) = axum_to_router(bot.clone(), Options::new(addr, url)).await?;
            let stop_token = listener.stop_token();

            let error_handler = LoggingErrorHandler::with_custom_text("An error from the update listener");
            let mut dispatcher = Dispatcher::builder(bot, handler)
                .default_handler(ignore_unknown_updates)
                .dependencies(deps)
                .build();
            let bot_fut = dispatcher.dispatch_with_listener(listener, error_handler);

            let srv = tokio::spawn(async move {
                let tcp_listener = tokio::net::TcpListener::bind(addr)
                    .await
                    .map_err(|err| {
                        stop_token.stop();
                        err
                    })?;
                let app = axum::Router::new()
                    .merge(metrics_router)
                    .merge(bot_router);
                axum::serve(tcp_listener, app)
                    .with_graceful_shutdown(stop_flag)
                    .await
            });

            let (res, _) = futures::join!(srv, bot_fut);
            res
        }
        None => {
            log::info!("The polling dispatcher is activating...");

            let bot_fut = tokio::spawn(async move {
                Dispatcher::builder(bot, handler)
                    .default_handler(ignore_unknown_updates)
                    .dependencies(deps)
                    .enable_ctrlc_handler()
                    .build()
                    .dispatch()
                    .await
            });

            let srv = tokio::spawn(async move {
                let tcp_listener = tokio::net::TcpListener::bind(addr).await?;
                axum::serve(tcp_listener, metrics_router)
                    .with_graceful_shutdown(async {
                        tokio::signal::ctrl_c()
                            .await
                            .expect("failed to install CTRL+C signal handler");
                        log::info!("Shutdown of the metrics server")
                    })
                    .await
            });

            let (res, _) = futures::join!(srv, bot_fut);
            res
        }
    }?.map_err(Into::into)
}
