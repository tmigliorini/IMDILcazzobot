use rust_i18n::t;
use serde::Serialize;
use tinytemplate::TinyTemplate;
use crate::domain::SupportedLanguage::{EN, RU};
use crate::domain::{LanguageCode, Username};

static EN_HELP: &str = include_str!("en.html");
static RU_HELP: &str = include_str!("ru.html");

#[derive(Clone)]
pub struct HelpContainer {
    en: String,
    ru: String,
}

impl HelpContainer {
    /// `lmo_intro` is the bot's actual default/active locale's text, loaded from an external
    /// file at startup (see `crate::external_text::ExternalTexts`) so it can be edited without
    /// a rebuild - it's not covered by the EN/RU-only `SupportedLanguage` mapping used elsewhere,
    /// so it's checked explicitly first.
    pub fn get_start_message(&self, username: Username, lang_code: LanguageCode, lmo_intro: &str) -> String {
        let greeting = t!("titles.greeting", locale = &lang_code);
        format!("{}, <b>{}</b>!\n\n{}", greeting, username.escaped(), self.get_help_message(lang_code, lmo_intro))
    }

    pub fn get_help_message(&self, lang_code: LanguageCode, lmo_intro: &str) -> String {
        if lang_code.as_str() == "lmo" {
            return lmo_intro.to_owned()
        }
        match lang_code.to_supported_language() {
            RU => self.ru.clone(),
            EN => self.en.clone()
        }
    }
}

#[derive(Serialize, Clone)]
pub struct Context {
    pub bot_name: String,
    pub grow_min: String,
    pub grow_max: String,
    pub other_bots: String,
    pub admin_channel_ru: String,
    pub admin_channel_en: String,
    pub admin_chat_ru: String,
    pub admin_chat_en: String,
    pub git_repo: String,
    pub help_pussies_percentage: f64
}

pub fn render_help_messages(context: Context) -> Result<HelpContainer, tinytemplate::error::Error> {
    let mut tt = TinyTemplate::new();
    tt.add_template("en", EN_HELP)?;
    tt.add_template("ru", RU_HELP)?;
    Ok(HelpContainer {
        en: tt.render("en", &context)?,
        ru: tt.render("ru", &context)?,
    })
}
