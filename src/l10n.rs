// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Reproduction of the `dav` app's l10n for the two calendars whose
//! displayname `Calendar::__construct()` rewrites.
//!
//! `apps/dav/lib/CalDAV/Calendar.php:53-58`:
//!
//! ```php
//! if ($this->getName() === BirthdayService::BIRTHDAY_CALENDAR_URI
//!     && strcasecmp($this->calendarInfo['{DAV:}displayname'], 'Contact birthdays') === 0) {
//!     $this->calendarInfo['{DAV:}displayname'] = $l10n->t('Contact birthdays');
//! }
//! if ($this->getName() === CalDavBackend::PERSONAL_CALENDAR_URI
//!     && $this->calendarInfo['{DAV:}displayname'] === CalDavBackend::PERSONAL_CALENDAR_NAME) {
//!     $this->calendarInfo['{DAV:}displayname'] = $l10n->t('Personal');
//! }
//! ```
//!
//! `getName()` is the **wire** uri (owned `personal`, or
//! `personal_shared_by_<owner>` for a share), so the substitution only ever
//! fires on an owned calendar. `$l10n` is `\OC::$server->getL10N('dav')`, whose
//! language is resolved by `L10N\Factory::findLanguage('dav')`:
//! `force_language`, then the authenticated user's `core/lang` (when the `dav`
//! app has that language), then the request's `Accept-Language`, then
//! `default_language`, then English.
//!
//! The translations are read from `<server-root>/apps/dav/l10n/<lang>.json`
//! (`{"translations": {...}}`) and cached per file for the process lifetime. A
//! missing file/language falls back to the source string, exactly like
//! `L10N::t()` when the key is absent.

use crate::config::Config;
use crate::db::Db;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// `BirthdayService::BIRTHDAY_CALENDAR_URI`.
pub const BIRTHDAY_CALENDAR_URI: &str = "contact_birthdays";
/// The English source string `Calendar::__construct()` compares against.
pub const BIRTHDAY_CALENDAR_NAME: &str = "Contact birthdays";
/// `CalDavBackend::PERSONAL_CALENDAR_URI`.
pub const PERSONAL_CALENDAR_URI: &str = "personal";
/// `CalDavBackend::PERSONAL_CALENDAR_NAME`.
pub const PERSONAL_CALENDAR_NAME: &str = "Personal";

const DAV_APP: &str = "dav";

type Translations = HashMap<String, String>;

/// Parsed `<lang>.json` files, keyed by path.
static TRANSLATIONS_CACHE: OnceLock<Mutex<HashMap<PathBuf, Arc<Translations>>>> = OnceLock::new();
/// The available languages (file stems) per l10n directory.
static AVAILABLE_CACHE: OnceLock<Mutex<HashMap<PathBuf, Arc<Vec<String>>>>> = OnceLock::new();
/// Paths already reported unavailable, so the warning is emitted once per path
/// rather than on every request.
static WARNED_PATHS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

/// The `dav` app's translations for one request, or the identity.
///
/// `source_available` distinguishes "the l10n tree was read and the identity is
/// the correct answer" (English, or a language whose file genuinely has no
/// translation for the key) from "the l10n tree could not be read". In the
/// latter case a displayname that *would* be translated must delegate instead
/// of being served untranslated: that would be a silent wrong answer.
#[derive(Debug, Clone, Default)]
pub struct DavL10n {
    translations: Option<Arc<Translations>>,
    source_available: bool,
}

/// The result of `Calendar::__construct()`'s displayname rewrite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalizedDisplayname {
    /// The displayname to serve.
    Value(Option<String>),
    /// The stored name matches one of the two special cases, but the `dav`
    /// l10n source is unavailable. Serving the stored (English) string would
    /// be a silent wrong answer, so the caller must delegate (501).
    Unresolved,
}

impl DavL10n {
    /// The identity when the l10n source is **unavailable**. A displayname that
    /// would be translated then delegates rather than serving the stored
    /// English.
    pub fn identity() -> Self {
        Self::default()
    }

    /// English: the source was read, but the identity is the correct
    /// translation. Served, never delegated.
    fn english() -> Self {
        Self {
            translations: None,
            source_available: true,
        }
    }

    /// `L10N::t()`: the translation when present, otherwise the key itself.
    pub fn translate<'a>(&'a self, key: &'a str) -> &'a str {
        match &self.translations {
            Some(map) => map.get(key).map(String::as_str).unwrap_or(key),
            None => key,
        }
    }

    /// `Calendar::__construct()`'s displayname rewrite.
    ///
    /// Returns [`LocalizedDisplayname::Unresolved`] only when the name matches
    /// one of the two special cases **and** the l10n source could not be read;
    /// an ordinary name is always served, whether or not the source exists.
    pub fn localize_displayname(
        &self,
        wire_uri: &str,
        displayname: Option<String>,
    ) -> LocalizedDisplayname {
        let Some(name) = displayname else {
            return LocalizedDisplayname::Value(None);
        };
        let key = if wire_uri == BIRTHDAY_CALENDAR_URI
            && name.eq_ignore_ascii_case(BIRTHDAY_CALENDAR_NAME)
        {
            Some(BIRTHDAY_CALENDAR_NAME)
        } else if wire_uri == PERSONAL_CALENDAR_URI && name == PERSONAL_CALENDAR_NAME {
            Some(PERSONAL_CALENDAR_NAME)
        } else {
            None
        };
        match key {
            None => LocalizedDisplayname::Value(Some(name)),
            Some(key) if self.source_available => {
                LocalizedDisplayname::Value(Some(self.translate(key).to_string()))
            }
            Some(_) => LocalizedDisplayname::Unresolved,
        }
    }

    /// Resolves the language and loads the `dav` translations. Never fails: an
    /// unreadable tree yields the identity.
    pub async fn resolve(
        db: &Db,
        config: &Config,
        uid: &str,
        accept_language: Option<&str>,
    ) -> Self {
        if let Some(dir) = &config.l10n_dir {
            return Self::from_dir(dir, db, config, uid, accept_language).await;
        }
        // The server root derived from `config_path`, then the standard
        // Nextcloud path as a fallback (the production sidecar runs from the
        // same PVC, so `/var/www/html/apps/dav/l10n` is always the real tree).
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(root) = dav_server_root(config) {
            candidates.push(root.join("apps").join(DAV_APP).join("l10n"));
        }
        candidates.push(PathBuf::from("/var/www/html/apps/dav/l10n"));
        for dir in &candidates {
            if available_languages(dir).await.is_some() {
                return Self::from_dir(dir, db, config, uid, accept_language).await;
            }
        }
        for dir in &candidates {
            warn_unavailable(dir, "no l10n directory found");
        }
        Self::identity()
    }

    async fn from_dir(
        dir: &Path,
        db: &Db,
        config: &Config,
        uid: &str,
        accept_language: Option<&str>,
    ) -> Self {
        let Some(available) = available_languages(dir).await else {
            warn_unavailable(dir, "l10n directory could not be read");
            return Self::identity();
        };
        let language = resolve_language(db, config, uid, accept_language, &available).await;
        if language == "en" {
            // The source was read; English is the identity, not a failure.
            return Self::english();
        }
        match load_translations(dir, &language).await {
            Some(translations) => Self {
                translations: Some(translations),
                source_available: true,
            },
            None => {
                let path = dir.join(format!("{language}.json"));
                warn_unavailable(&path, "l10n file missing or malformed");
                Self::identity()
            }
        }
    }
}

/// Warn once per path when the l10n source cannot be read, so the fail-safe
/// delegation is loud without logging on every request.
fn warn_unavailable(path: &Path, reason: &str) {
    let warned = WARNED_PATHS.get_or_init(|| Mutex::new(HashSet::new()));
    let first = warned
        .lock()
        .map(|mut set| set.insert(path.to_path_buf()))
        .unwrap_or(false);
    if first {
        log::warn!(
            "dav l10n source unavailable at {} ({reason}); calendar displaynames that would be \
             translated will be delegated to PHP instead of served untranslated",
            path.display()
        );
    }
}

/// The Nextcloud server root: the parent of the `config/` directory holding
/// `config_path`.
pub fn dav_server_root(config: &Config) -> Option<PathBuf> {
    config.config_path.parent()?.parent().map(Path::to_path_buf)
}

/// `L10N\Factory::findLanguage('dav')`, minus the `forceLanguage` request
/// parameter (DAV clients do not send one).
async fn resolve_language(
    db: &Db,
    config: &Config,
    uid: &str,
    accept_language: Option<&str>,
    available: &[String],
) -> String {
    let exists = |lang: &str| available.iter().any(|candidate| candidate == lang);

    if let Some(language) = config.force_language.as_deref() {
        if exists(language) {
            return language.to_string();
        }
    }
    if let Some(language) = db
        .user_preference(uid, "core", "lang")
        .await
        .ok()
        .flatten()
    {
        if exists(&language) {
            return language;
        }
    }
    if let Some(header) = accept_language {
        if let Some(language) =
            pick_from_accept_language(header, available, config.default_language.as_deref())
        {
            return language;
        }
    }
    if let Some(language) = config.default_language.as_deref() {
        if exists(language) {
            return language.to_string();
        }
    }
    "en".to_string()
}

/// `L10N\Factory::getLanguageFromRequest()`.
fn pick_from_accept_language(
    header: &str,
    available: &[String],
    default_language: Option<&str>,
) -> Option<String> {
    let header = clean_language(header);
    if header.is_empty() {
        return None;
    }
    let mut available = available.to_vec();
    available.sort();
    for preference in header.to_lowercase().split(',') {
        let preferred = preference
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .replace('-', "_");
        if preferred.is_empty() {
            continue;
        }
        let parts: Vec<&str> = preferred.split('_').collect();
        let first = parts[0];
        let last = parts[parts.len() - 1];
        for language in &available {
            if preferred == language.to_lowercase() {
                return Some(respect_default_language(
                    language,
                    &available,
                    default_language,
                ));
            }
            if language.to_lowercase() == format!("{first}_{last}") {
                return Some(language.clone());
            }
        }
        for language in &available {
            if first == language {
                return Some(language.clone());
            }
        }
    }
    None
}

/// `L10N\Factory::respectDefaultLanguage()`: only the `de`/`de_DE` special
/// case.
fn respect_default_language(
    language: &str,
    available: &[String],
    default_language: Option<&str>,
) -> String {
    let formal = language.eq_ignore_ascii_case("de")
        && default_language
            .map(|value| value.eq_ignore_ascii_case("de_DE"))
            .unwrap_or(false)
        && available.iter().any(|candidate| candidate == "de_DE");
    if formal {
        "de_DE".to_string()
    } else {
        language.to_string()
    }
}

/// `L10N\Factory::cleanLanguage()`.
fn clean_language(input: &str) -> String {
    let filtered: String = input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | ';' | ',' | '=' | '_' | '-'))
        .collect();
    filtered.replace("..", "")
}

/// `L10N\Factory::findAvailableLanguages('dav')`: the `*.json` file stems plus
/// English. `None` when the directory cannot be read.
async fn available_languages(dir: &Path) -> Option<Vec<String>> {
    if let Some(cached) = AVAILABLE_CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .ok()
        .and_then(|cache| cache.get(dir).cloned())
    {
        return Some((*cached).clone());
    }
    let names = list_dir(dir.to_path_buf()).await?;
    let mut languages = vec!["en".to_string()];
    for name in names {
        if let Some(stem) = name.strip_suffix(".json") {
            if !stem.starts_with("l10n") {
                languages.push(stem.to_string());
            }
        }
    }
    languages.sort();
    languages.dedup();
    let cached = Arc::new(languages.clone());
    if let Ok(mut cache) = AVAILABLE_CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
    {
        cache.insert(dir.to_path_buf(), cached);
    }
    Some(languages)
}

/// Loads `<dir>/<language>.json` and caches the parsed `translations` map.
/// English (or a missing/unparseable file) yields the identity.
async fn load_translations(dir: &Path, language: &str) -> Option<Arc<Translations>> {
    if language == "en" {
        return None;
    }
    let path = dir.join(format!("{language}.json"));
    if let Some(cached) = TRANSLATIONS_CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .ok()
        .and_then(|cache| cache.get(&path).cloned())
    {
        return Some(cached);
    }
    let bytes = read_file(path.clone()).await?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let object = value.get("translations")?.as_object()?;
    let translations: Translations = object
        .iter()
        .filter_map(|(key, value)| value.as_str().map(|text| (key.clone(), text.to_string())))
        .collect();
    let cached = Arc::new(translations);
    if let Ok(mut cache) = TRANSLATIONS_CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
    {
        cache.insert(path, cached.clone());
    }
    Some(cached)
}

async fn read_file(path: PathBuf) -> Option<Vec<u8>> {
    tokio::task::spawn_blocking(move || std::fs::read(path).ok())
        .await
        .ok()
        .flatten()
}

async fn list_dir(dir: PathBuf) -> Option<Vec<String>> {
    tokio::task::spawn_blocking(move || {
        let entries = std::fs::read_dir(dir).ok()?;
        Some(
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect(),
        )
    })
    .await
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l10n_fr() -> DavL10n {
        let mut translations = Translations::new();
        translations.insert("Personal".to_string(), "Personnel".to_string());
        translations.insert(
            "Contact birthdays".to_string(),
            "Anniversaires des contacts".to_string(),
        );
        DavL10n {
            translations: Some(Arc::new(translations)),
            source_available: true,
        }
    }

    /// A language file that was read but carries no translation for the key.
    fn l10n_without_translations() -> DavL10n {
        DavL10n {
            translations: Some(Arc::new(Translations::new())),
            source_available: true,
        }
    }

    /// The l10n tree could not be read at all.
    fn l10n_unavailable() -> DavL10n {
        DavL10n::identity()
    }

    fn value(name: &str) -> LocalizedDisplayname {
        LocalizedDisplayname::Value(Some(name.to_string()))
    }

    #[test]
    fn personal_is_localized_only_for_the_exact_name_and_uri() {
        let l10n = l10n_fr();
        assert_eq!(
            l10n.localize_displayname("personal", Some("Personal".to_string())),
            value("Personnel")
        );
        // Exact case only.
        assert_eq!(
            l10n.localize_displayname("personal", Some("personal".to_string())),
            value("personal")
        );
        // Wrong uri.
        assert_eq!(
            l10n.localize_displayname("other", Some("Personal".to_string())),
            value("Personal")
        );
        // A share is served under `<uri>_shared_by_<owner>`.
        assert_eq!(
            l10n.localize_displayname(
                "personal_shared_by_bob",
                Some("Personal (Bob)".to_string())
            ),
            value("Personal (Bob)")
        );
    }

    #[test]
    fn birthday_is_localized_case_insensitively() {
        let l10n = l10n_fr();
        for stored in ["Contact birthdays", "contact BIRTHDAYS", "CONTACT BIRTHDAYS"] {
            assert_eq!(
                l10n.localize_displayname("contact_birthdays", Some(stored.to_string())),
                value("Anniversaires des contacts")
            );
        }
        // Wrong uri: the case-insensitive match does not leak to other uris.
        assert_eq!(
            l10n.localize_displayname("birthdays", Some("Contact birthdays".to_string())),
            value("Contact birthdays")
        );
    }

    #[test]
    fn missing_key_falls_back_to_the_source_string() {
        let l10n = l10n_fr();
        assert_eq!(l10n.translate("Calendar"), "Calendar");
        assert_eq!(DavL10n::identity().translate("Personal"), "Personal");
    }

    // --- the four fail-safe branches -----------------------------------------

    #[test]
    fn unavailable_source_special_displayname_is_unresolved() {
        // Source unavailable + special displayname -> the caller delegates.
        let l10n = l10n_unavailable();
        assert_eq!(
            l10n.localize_displayname("personal", Some("Personal".to_string())),
            LocalizedDisplayname::Unresolved
        );
        assert_eq!(
            l10n.localize_displayname("contact_birthdays", Some("Contact birthdays".to_string())),
            LocalizedDisplayname::Unresolved
        );
    }

    #[test]
    fn unavailable_source_ordinary_displayname_is_served() {
        // Source unavailable + ordinary displayname -> served unchanged.
        let l10n = l10n_unavailable();
        assert_eq!(
            l10n.localize_displayname("work", Some("Work".to_string())),
            value("Work")
        );
        // A name that is special for another uri is ordinary here.
        assert_eq!(
            l10n.localize_displayname("work", Some("Personal".to_string())),
            value("Personal")
        );
    }

    #[test]
    fn available_source_without_translation_serves_the_identity() {
        // Source read, language has no translation for the key -> identity is
        // correct and must be served, not delegated.
        let l10n = l10n_without_translations();
        assert_eq!(
            l10n.localize_displayname("personal", Some("Personal".to_string())),
            value("Personal")
        );
    }

    #[test]
    fn available_english_source_serves_the_identity() {
        let l10n = DavL10n::english();
        assert_eq!(
            l10n.localize_displayname("personal", Some("Personal".to_string())),
            value("Personal")
        );
    }

    #[test]
    fn accept_language_matches_php_fallbacks() {
        let available = vec![
            "en".to_string(),
            "de".to_string(),
            "de_DE".to_string(),
            "fr".to_string(),
        ];
        assert_eq!(
            pick_from_accept_language("fr-FR,fr;q=0.9", &available, None).as_deref(),
            Some("fr")
        );
        assert_eq!(
            pick_from_accept_language("de-DE", &available, Some("de_DE")).as_deref(),
            Some("de_DE")
        );
        assert_eq!(pick_from_accept_language("es", &available, None), None);
        // `cleanLanguage` strips the invalid bytes.
        assert_eq!(
            pick_from_accept_language("fr!!!", &available, None).as_deref(),
            Some("fr")
        );
    }
}
