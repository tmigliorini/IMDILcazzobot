use teloxide::Bot;
use teloxide::macros::BotCommands;
use teloxide::types::Message;
use crate::handlers::{HandlerResult, reply_html};
use crate::{metrics, reply_html};
use crate::external_text::ExternalTexts;

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase")]
pub enum SyntaxCommands {
    #[command(description = "syntax")]
    Syntax,
}

pub async fn cmd_handler(bot: Bot, msg: Message, texts: ExternalTexts) -> HandlerResult {
    metrics::CMD_SYNTAX_COUNTER.chat.inc();
    reply_html!(bot, msg, texts.syntax.clone());
    Ok(())
}
