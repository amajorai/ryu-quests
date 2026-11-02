//! The language the completion judge writes its reason in.
//!
//! The judge's `REASON:` sentence is shown verbatim in the task row and in the
//! "Looks done" / "Not done yet" toasts, next to the translated UI. So the judge
//! is told to write it in the node's UI language. Core's scheduler passes that
//! locale in the judge body; a manual "Check now" from the app's own page carries
//! the page's `Accept-Language` instead. The three labels (`MET:`,
//! `CONFIDENCE:`, `REASON:`) and the `yes`/`no` answer stay English because
//! [`crate::parse_verdict`] reads them.
//!
//! Smaller local models follow a format less reliably once told to write in
//! another language (some translate the labels). [`reason_fallback`] keeps such a
//! reply's sentence instead of dropping it.

/// English names of the languages the app's UI is translated into, as a model is
/// most reliably told them. English itself needs no instruction.
const LANGUAGE_NAMES: &[(&str, &str)] = &[
    ("ar", "Arabic"),
    ("de", "German"),
    ("es", "Spanish"),
    ("fr", "French"),
    ("hi", "Hindi"),
    ("it", "Italian"),
    ("ja", "Japanese"),
    ("ko", "Korean"),
    ("pt", "Brazilian Portuguese"),
    ("ru", "Russian"),
    ("zh", "Simplified Chinese"),
];

/// The English name of a locale tag's language (`ja-JP` → `Japanese`), or `None`
/// for English and for languages the UI has no translation for. Traditional
/// Chinese (`zh-TW`, `zh-Hant`) has no catalog, so it is not mapped to the
/// Simplified one.
pub fn language_name(tag: &str) -> Option<&'static str> {
    let tag = tag.trim().replace('_', "-").to_ascii_lowercase();
    let mut parts = tag.split('-');
    let language = parts.next()?;
    if language == "zh" && parts.any(|part| matches!(part, "hant" | "tw" | "hk" | "mo")) {
        return None;
    }
    LANGUAGE_NAMES
        .iter()
        .find(|(code, _)| *code == language)
        .map(|(_, name)| *name)
}

/// The first language of an `Accept-Language` header (`ja,en;q=0.8` → `ja`).
pub fn accept_language_tag(header: &str) -> Option<String> {
    header
        .split(',')
        .map(|part| part.split(';').next().unwrap_or_default().trim())
        .find(|tag| !tag.is_empty() && *tag != "*")
        .map(str::to_owned)
}

/// The system-prompt line asking for the reason in `locale`'s language, or
/// `None` when the reason stays English.
pub fn judge_language_line(locale: Option<&str>) -> Option<String> {
    let name = language_name(locale?)?;
    Some(format!(
        "Write the REASON sentence in {name}. Keep the labels MET, CONFIDENCE and REASON and \
         the yes/no answer in English."
    ))
}

/// The text after a `REASON:` label anywhere on `line` (any case, ASCII or
/// full-width colon). Searched on the original text, so a non-ASCII reason is
/// never sliced at an offset taken from a lowercased copy.
pub fn reason_after_label(line: &str) -> Option<&str> {
    const LABEL: &str = "reason";
    let line = line.trim();
    line.char_indices().find_map(|(at, _)| {
        let label = line.get(at..at + LABEL.len())?;
        if !label.eq_ignore_ascii_case(LABEL) {
            return None;
        }
        let rest = &line[at + LABEL.len()..];
        rest.strip_prefix(':').or_else(|| rest.strip_prefix('：'))
    })
}

/// A reason for a reply with no `REASON:` line: the first line that is not the
/// `MET:` or `CONFIDENCE:` line, minus a short leading label a model may have
/// translated (`理由：…`, `Begründung: …`).
pub fn reason_fallback(text: &str) -> Option<String> {
    let labeled = |line: &str, label: &str| {
        line.get(..label.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(label))
            && line[label.len()..].trim_start().starts_with([':', '：'])
    };
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !labeled(line, "met") && !labeled(line, "confidence"))?;
    let unlabeled = line
        .split_once([':', '：'])
        .filter(|(label, rest)| label.chars().count() <= 16 && !rest.trim().is_empty())
        .map_or(line, |(_, rest)| rest);
    let reason = unlabeled.trim();
    (!reason.is_empty()).then(|| reason.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_language_subtag() {
        assert_eq!(language_name("ja"), Some("Japanese"));
        assert_eq!(language_name("pt_BR"), Some("Brazilian Portuguese"));
        assert_eq!(language_name("zh-CN"), Some("Simplified Chinese"));
        assert_eq!(language_name("zh-Hant-TW"), None);
        assert_eq!(language_name("en-US"), None);
        assert_eq!(language_name(""), None);
    }

    #[test]
    fn the_line_names_the_language_and_keeps_labels_english() {
        assert_eq!(judge_language_line(None), None);
        assert_eq!(judge_language_line(Some("en")), None);
        let line = judge_language_line(Some("ja")).unwrap();
        assert!(line.contains("Japanese"));
        assert!(line.contains("MET, CONFIDENCE and REASON"));
    }

    #[test]
    fn accept_language_takes_the_first_tag() {
        assert_eq!(
            accept_language_tag("de-DE,de;q=0.9,en;q=0.5").as_deref(),
            Some("de-DE")
        );
        assert_eq!(accept_language_tag("*"), None);
        assert_eq!(accept_language_tag(""), None);
    }

    #[test]
    fn a_translated_label_still_yields_the_reason() {
        let reply = "MET: no\nCONFIDENCE: 30\n理由：下書きはまだ編集中です。";
        assert_eq!(
            reason_fallback(reply).as_deref(),
            Some("下書きはまだ編集中です。")
        );
        let unlabeled = "MET: yes\nCONFIDENCE: 90\nDie PR wurde zusammengeführt: siehe GitHub.";
        assert_eq!(
            reason_fallback(unlabeled).as_deref(),
            Some("Die PR wurde zusammengeführt: siehe GitHub.")
        );
        assert_eq!(reason_fallback("MET: yes\nCONFIDENCE: 90"), None);
        assert_eq!(
            reason_fallback("MET: no\nCONFIDENCE: 10\nMeteo-App noch offen").as_deref(),
            Some("Meteo-App noch offen")
        );
    }
}
