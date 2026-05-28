use crate::language::SupportedLanguage;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tower_lsp::lsp_types::DiagnosticSeverity;

pub const CONFIG_DIR: &str = ".zed";
pub const CONFIG_FILE: &str = "languagetool.json";

fn default_project_config_path() -> String {
    format!("{CONFIG_DIR}/{CONFIG_FILE}")
}

fn default_custom_url() -> String {
    "http://localhost:8081".to_string()
}

fn default_cloud_url() -> String {
    "https://api.languagetool.org".to_string()
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum BackendKind {
    #[default]
    Custom,
    Cloud,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DiagnosticSeverityConfig {
    Error,
    Warning,
    #[default]
    Information,
    Hint,
}

impl DiagnosticSeverityConfig {
    pub fn as_lsp(self) -> DiagnosticSeverity {
        match self {
            DiagnosticSeverityConfig::Error => DiagnosticSeverity::ERROR,
            DiagnosticSeverityConfig::Warning => DiagnosticSeverity::WARNING,
            DiagnosticSeverityConfig::Information => DiagnosticSeverity::INFORMATION,
            DiagnosticSeverityConfig::Hint => DiagnosticSeverity::HINT,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CheckingLevel {
    Default,
    Picky,
}

impl CheckingLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            CheckingLevel::Default => "default",
            CheckingLevel::Picky => "picky",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct ClientOptions {
    pub backend: BackendKind,
    pub custom_backend_url: String,
    pub username: Option<String>,
    pub api_key: Option<String>,
    pub language: String,
    pub mother_tongue: Option<String>,
    pub preferred_variants: Vec<String>,
    pub disabled_rules: Vec<String>,
    pub disabled_categories: Vec<String>,
    pub enabled_rules: Vec<String>,
    pub enabled_categories: Vec<String>,
    pub level: Option<CheckingLevel>,
    pub check_on_open: bool,
    pub check_on_save: bool,
    pub check_while_typing: bool,
    pub debounce_ms: u64,
    pub diagnostic_severity: DiagnosticSeverityConfig,
    pub diagnostic_severity_auto: bool,
    pub max_replacements: usize,
    pub enabled_languages: Vec<String>,
    pub disabled_languages: Vec<String>,
    pub ignored_words: Vec<String>,
    pub project_config_path: String,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            backend: BackendKind::default(),
            custom_backend_url: default_custom_url(),
            username: None,
            api_key: None,
            language: "en-US".to_string(),
            mother_tongue: None,
            preferred_variants: Vec::new(),
            disabled_rules: Vec::new(),
            disabled_categories: Vec::new(),
            enabled_rules: Vec::new(),
            enabled_categories: Vec::new(),
            level: None,
            check_on_open: true,
            check_on_save: true,
            check_while_typing: true,
            debounce_ms: 750,
            diagnostic_severity: DiagnosticSeverityConfig::Information,
            diagnostic_severity_auto: true,
            max_replacements: 8,
            enabled_languages: Vec::new(),
            disabled_languages: Vec::new(),
            ignored_words: Vec::new(),
            project_config_path: default_project_config_path(),
        }
    }
}

impl ClientOptions {
    pub fn from_value(value: Option<Value>) -> Self {
        match value {
            Some(value) => Self::parse_value(value).unwrap_or_else(|err| {
                log::error!("Failed to parse initialization options, using defaults: {err}");
                Self::default()
            }),
            None => Self::default(),
        }
    }

    pub fn parse_value(value: Value) -> serde_json::Result<Self> {
        serde_json::from_value(value)
    }

    pub fn merged_with_value(&self, value: Value) -> serde_json::Result<Self> {
        let mut merged = serde_json::to_value(self)?;
        merge_json_value(&mut merged, value);
        Self::parse_value(merged)
    }

    pub fn base_url(&self) -> String {
        let url = match self.backend {
            BackendKind::Custom => self.custom_backend_url.as_str(),
            BackendKind::Cloud => {
                let default_url = default_cloud_url();
                return default_url.trim().trim_end_matches('/').to_string();
            }
        };
        url.trim().trim_end_matches('/').to_string()
    }

    pub fn api_base_url(&self) -> String {
        format!("{}/v2", self.base_url())
    }

    pub fn endpoint(&self) -> String {
        format!("{}/check", self.api_base_url())
    }

    pub fn project_config_path(&self, root: &Path) -> PathBuf {
        let path = PathBuf::from(self.project_config_path.trim());
        if path.is_absolute() {
            path
        } else {
            root.join(path)
        }
    }

    pub fn project_config_display_path(&self) -> String {
        self.project_config_path.trim().to_string()
    }

    pub fn timeout(&self) -> Duration {
        match self.backend {
            BackendKind::Custom => Duration::from_secs(10),
            BackendKind::Cloud => Duration::from_secs(20),
        }
    }

    pub fn configured_severity(&self) -> DiagnosticSeverity {
        self.diagnostic_severity.as_lsp()
    }

    pub fn language_enabled(&self, language: &SupportedLanguage) -> bool {
        let language_id = language.id();

        if self
            .disabled_languages
            .iter()
            .any(|disabled| disabled == language_id)
        {
            return false;
        }

        self.enabled_languages.is_empty()
            || self
                .enabled_languages
                .iter()
                .any(|enabled| enabled == language_id)
    }

    pub fn is_ignored_word(&self, word: &str) -> bool {
        self.ignored_words
            .iter()
            .any(|ignored| ignored.eq_ignore_ascii_case(word))
    }
}

fn merge_json_value(base: &mut Value, update: Value) {
    match (base, update) {
        (Value::Object(base), Value::Object(update)) => {
            for (key, value) in update {
                match base.get_mut(&key) {
                    Some(base_value) => merge_json_value(base_value, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, update) => *base = update,
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ProjectConfig {
    #[serde(default)]
    pub ignored_words: Vec<String>,
    #[serde(default)]
    pub disabled_rules: Vec<String>,
    #[serde(default)]
    pub disabled_categories: Vec<String>,
}

impl ProjectConfig {
    pub async fn load(path: &Path) -> Self {
        let Ok(text) = tokio::fs::read_to_string(path).await else {
            return Self::default();
        };

        serde_json::from_str(&text).unwrap_or_else(|err| {
            log::warn!("Failed to parse {}: {err}", path.display());
            Self::default()
        })
    }

    pub async fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let text = self.to_json_string();
        tokio::fs::write(path, text).await
    }

    pub fn to_json_string(&self) -> String {
        let mut text = serde_json::to_string_pretty(self).expect("project config should serialize");
        text.push('\n');
        text
    }

    pub fn merged_options(&self, base: &ClientOptions) -> ClientOptions {
        let mut options = base.clone();
        options.ignored_words = merge_words(&self.ignored_words, &base.ignored_words);
        options.disabled_rules = merge_list(&self.disabled_rules, &base.disabled_rules);
        options.disabled_categories =
            merge_list(&self.disabled_categories, &base.disabled_categories);
        options
    }

    pub fn add_ignored_word(&mut self, word: &str) -> bool {
        let word = word.trim().to_lowercase();
        if word.is_empty() {
            return false;
        }
        push_unique_sorted(&mut self.ignored_words, word)
    }

    pub fn add_disabled_rule(&mut self, rule_id: &str) -> bool {
        let rule_id = rule_id.trim().to_uppercase();
        if rule_id.is_empty() {
            return false;
        }
        push_unique_sorted(&mut self.disabled_rules, rule_id)
    }

    pub fn add_disabled_category(&mut self, category_id: &str) -> bool {
        let category_id = category_id.trim().to_uppercase();
        if category_id.is_empty() {
            return false;
        }
        push_unique_sorted(&mut self.disabled_categories, category_id)
    }
}

fn merge_words(project: &[String], init: &[String]) -> Vec<String> {
    project
        .iter()
        .chain(init.iter())
        .map(|word| word.trim().to_lowercase())
        .filter(|word| !word.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn merge_list(project: &[String], init: &[String]) -> Vec<String> {
    project
        .iter()
        .map(String::as_str)
        .chain(init.iter().map(String::as_str))
        .map(|value| value.trim().to_uppercase())
        .filter(|value| !value.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn push_unique_sorted(values: &mut Vec<String>, value: String) -> bool {
    if values.iter().any(|existing| existing == &value) {
        return false;
    }
    values.push(value);
    values.sort();
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_api_urls_from_backend_base_url() {
        let options = ClientOptions::default();
        assert_eq!(options.base_url(), "http://localhost:8081");
        assert_eq!(options.api_base_url(), "http://localhost:8081/v2");
        assert_eq!(options.endpoint(), "http://localhost:8081/v2/check");

        let options = ClientOptions {
            backend: BackendKind::Cloud,
            custom_backend_url: " https://custom.example.test/ ".to_string(),
            ..ClientOptions::default()
        };
        assert_eq!(options.base_url(), "https://api.languagetool.org");
        assert_eq!(options.api_base_url(), "https://api.languagetool.org/v2");
        assert_eq!(options.endpoint(), "https://api.languagetool.org/v2/check");
    }

    #[test]
    fn parses_camel_case_options() {
        let options = ClientOptions::from_value(Some(serde_json::json!({
            "backend": "cloud",
            "enabledRules": ["WHITESPACE_RULE"],
            "enabledCategories": ["TYPOGRAPHY"],
            "preferredVariants": ["en-US"],
            "level": "picky",
            "checkOnSave": false,
            "projectConfigPath": ".config/languagetool/project.json",
            "diagnosticSeverity": "warning"
        })));
        assert_eq!(options.backend, BackendKind::Cloud);
        assert_eq!(options.custom_backend_url, default_custom_url());
        assert_eq!(options.base_url(), default_cloud_url());
        assert_eq!(options.enabled_rules, vec!["WHITESPACE_RULE"]);
        assert_eq!(options.enabled_categories, vec!["TYPOGRAPHY"]);
        assert_eq!(options.preferred_variants, vec!["en-US"]);
        assert_eq!(options.level, Some(CheckingLevel::Picky));
        assert!(!options.check_on_save);
        assert_eq!(
            options.project_config_path,
            ".config/languagetool/project.json"
        );
        assert_eq!(options.configured_severity(), DiagnosticSeverity::WARNING);
    }

    #[test]
    fn merges_partial_option_updates() {
        let options = ClientOptions {
            backend: BackendKind::Cloud,
            custom_backend_url: "https://example.test".to_string(),
            language: "de-DE".to_string(),
            debounce_ms: 750,
            check_on_save: false,
            ..Default::default()
        };

        let options = options
            .merged_with_value(serde_json::json!({ "debounceMs": 100 }))
            .unwrap();

        assert_eq!(options.backend, BackendKind::Cloud);
        assert_eq!(options.custom_backend_url, "https://example.test");
        assert_eq!(options.language, "de-DE");
        assert_eq!(options.debounce_ms, 100);
        assert!(!options.check_on_save);
    }

    #[test]
    fn merges_flat_backend_option_updates() {
        let options = ClientOptions {
            backend: BackendKind::Cloud,
            custom_backend_url: "https://old.example.test".to_string(),
            ..Default::default()
        };

        let options = options
            .merged_with_value(serde_json::json!({
                "customBackendUrl": "https://new.example.test"
            }))
            .unwrap();
        assert_eq!(options.backend, BackendKind::Cloud);
        assert_eq!(options.custom_backend_url, "https://new.example.test");
        assert_eq!(options.base_url(), default_cloud_url());

        let options = options
            .merged_with_value(serde_json::json!({
                "backend": "custom"
            }))
            .unwrap();
        assert_eq!(options.backend, BackendKind::Custom);
        assert_eq!(options.base_url(), "https://new.example.test");
    }

    #[test]
    fn resolves_project_config_paths() {
        let root = Path::new("/tmp/workspace");
        let options = ClientOptions::default();
        assert_eq!(
            options.project_config_path(root),
            PathBuf::from("/tmp/workspace/.zed/languagetool.json")
        );
        assert_eq!(
            options.project_config_display_path(),
            ".zed/languagetool.json"
        );

        let options = ClientOptions {
            project_config_path: ".idea/languagetool.json".to_string(),
            ..Default::default()
        };
        assert_eq!(
            options.project_config_path(root),
            PathBuf::from("/tmp/workspace/.idea/languagetool.json")
        );

        let options = ClientOptions {
            project_config_path: "/tmp/languagetool.json".to_string(),
            ..Default::default()
        };
        assert_eq!(
            options.project_config_path(root),
            PathBuf::from("/tmp/languagetool.json")
        );
    }

    #[test]
    fn project_config_merges_with_initialization_options() {
        let project = ProjectConfig {
            ignored_words: vec!["Zed".to_string()],
            disabled_rules: vec!["foo_rule".to_string()],
            disabled_categories: vec!["style".to_string()],
        };
        let base = ClientOptions {
            ignored_words: vec!["LanguageTool".to_string()],
            disabled_rules: vec!["bar_rule".to_string()],
            disabled_categories: vec!["grammar".to_string()],
            ..Default::default()
        };

        let options = project.merged_options(&base);
        assert_eq!(options.ignored_words, vec!["languagetool", "zed"]);
        assert_eq!(options.disabled_rules, vec!["BAR_RULE", "FOO_RULE"]);
        assert_eq!(options.disabled_categories, vec!["GRAMMAR", "STYLE"]);
    }

    #[test]
    fn add_config_values_are_unique_and_normalized() {
        let mut project = ProjectConfig::default();
        assert!(project.add_ignored_word(" Zed "));
        assert!(!project.add_ignored_word("zed"));
        assert!(project.add_disabled_rule(" foo_rule "));
        assert!(project.add_disabled_category(" grammar "));
        assert_eq!(project.ignored_words, vec!["zed"]);
        assert_eq!(project.disabled_rules, vec!["FOO_RULE"]);
        assert_eq!(project.disabled_categories, vec!["GRAMMAR"]);
    }

    #[test]
    fn serializes_flat_project_config() {
        let mut project = ProjectConfig::default();
        project.add_ignored_word("zed");
        project.add_disabled_rule("whitespace_rule");
        project.add_disabled_category("typography");

        let text = project.to_json_string();
        assert!(text.contains("\"ignored_words\""));
        assert!(text.contains("\"disabled_rules\""));
        assert!(text.contains("\"disabled_categories\""));
        assert!(!text.contains("\"languagetool\""));
    }
}
