use std::ops::Deref;
use derive_more::{Constructor, From};
use once_cell::sync::Lazy;
use teloxide::types::User;

static DEFAULT: Lazy<LanguageCode> = Lazy::new(|| LanguageCode("lmo".to_string()));
static RU_SPEAKING_LOCALES: [&str; 3] = ["ru", "uk", "be"];

#[derive(Clone, Debug, Constructor, From)]
pub struct LanguageCode(String);

#[derive(Hash, Copy, Clone, Eq, PartialEq, sqlx::Type)]
#[sqlx(type_name = "language_code", rename_all = "lowercase")]
#[cfg_attr(test, derive(Debug))]
pub enum SupportedLanguage {
    EN,
    RU,
}

impl LanguageCode {
    pub fn from_user(_user: &User) -> Self {
        DEFAULT.clone()
    }

    pub fn from_maybe_user(_maybe_user: Option<&User>) -> Self {
        DEFAULT.clone()
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn to_supported_language(&self) -> SupportedLanguage {
        let code = self.to_ascii_lowercase();
        if code.len() < 2 {
            SupportedLanguage::EN
        } else if RU_SPEAKING_LOCALES.contains(&&code[..2]) {
            SupportedLanguage::RU
        } else {
            SupportedLanguage::EN
        }
    }
}

impl Deref for LanguageCode {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl From<&User> for LanguageCode {
    fn from(value: &User) -> Self {
        Self::from_user(value)
    }
}

impl From<Option<&User>> for LanguageCode {
    fn from(value: Option<&User>) -> Self {
        Self::from_maybe_user(value)
    }
}

#[cfg(test)]
mod test_to_supported_language {
    use crate::domain::LanguageCode;
    use crate::domain::SupportedLanguage::{EN, RU};

    #[test]
    fn success() {
        let ru = [
            "RU", "ru", "Ru", "rU", "ru-RU", "RU-ru", "rU-Ru", "Ru-rU",
            "BE", "be", "Be", "bE", "be-BY", "BE-by", "bE-By", "Be-bY"
        ].map(|code| (code, RU));
        let en = [
            "EN", "en", "En", "eN", "en-US", "EN-us", "eN-Us", "En-uS",
            "c", "C", "POSIX"
        ].map(|code| (code, EN));
        let cases = ru.into_iter().chain(en);

        for (case, expected) in cases {
            let result = LanguageCode::new(case.to_string());
            assert_eq!(result.to_supported_language(), expected, "Case: {case}, result: {result:?}")
        }
    }

    #[test]
    fn empty() {
        let result = LanguageCode::new("".to_string());
        assert_eq!(result.to_supported_language(), EN)
    }
}
