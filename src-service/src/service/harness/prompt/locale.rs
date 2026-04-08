/// Supported UI languages for agent response templates.
/// Defaults to Traditional Chinese (`ZhTW`).
#[derive(Clone, Copy, Default, Debug)]
pub(crate) enum Locale {
    #[default]
    ZhTW,
    ZhCN,
    En,
    Ja,
    De,
    Ko,
}

impl Locale {
    /// Parse a BCP-47 language tag (e.g. "zh-TW", "en") into a `Locale`.
    /// Unknown tags fall back to `ZhTW`.
    pub(crate) fn from_tag(tag: &str) -> Self {
        match tag {
            "zh-TW" | "zh-tw" => Self::ZhTW,
            "zh-CN" | "zh-cn" | "zh" => Self::ZhCN,
            "en" => Self::En,
            "ja" => Self::Ja,
            "de" => Self::De,
            "ko" => Self::Ko,
            _ => Self::ZhTW,
        }
    }
}
