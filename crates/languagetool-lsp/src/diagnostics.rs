use serde::{Deserialize, Serialize};
use tower_lsp_server::ls_types::{
    CodeDescription, Diagnostic, DiagnosticSeverity, NumberOrString, Range, Uri,
};

use crate::config::ClientOptions;
use crate::languagetool::LanguageToolMatch;
use crate::text_index::Utf16Range;

pub const SOURCE: &str = "LanguageTool";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticData {
    pub rule_id: String,
    pub category_id: Option<String>,
    pub issue_type: Option<String>,
    pub replacements: Vec<String>,
    pub matched_text: String,
    #[serde(default)]
    pub document_version: Option<i32>,
}

pub fn make_lsp_diagnostic_for_range(
    range: Range,
    item: &LanguageToolMatch,
    data: DiagnosticData,
    options: &ClientOptions,
) -> Diagnostic {
    let rule_id = item.rule.as_ref().map(|rule| rule.id.as_str());
    Diagnostic {
        range,
        severity: Some(severity_for(item, options)),
        code: rule_id.map(|rule_id| NumberOrString::String(rule_id.to_string())),
        code_description: rule_id.and_then(|rule_id| code_description(rule_id, &options.language)),
        source: Some(SOURCE.to_string()),
        message: item.message.clone(),
        related_information: None,
        tags: None,
        data: serde_json::to_value(data).ok(),
    }
}

pub fn diagnostic_data_for_text(
    matched_text: String,
    item: &LanguageToolMatch,
    options: &ClientOptions,
    document_version: Option<i32>,
) -> DiagnosticData {
    let rule = item.rule.as_deref();
    let category_id = rule.and_then(|rule| rule.category.id.clone());
    let replacements = item
        .replacements
        .iter()
        .take(options.max_replacements)
        .filter_map(|replacement| replacement.value.clone())
        .collect::<Vec<_>>();
    DiagnosticData {
        rule_id: rule.map(|rule| rule.id.clone()).unwrap_or_default(),
        category_id,
        issue_type: rule.and_then(|rule| rule.issue_type.clone()),
        replacements,
        matched_text,
        document_version,
    }
}

pub fn match_utf16_range(item: &LanguageToolMatch) -> Option<Utf16Range> {
    Some(Utf16Range::new(
        usize::try_from(item.offset).ok()?,
        usize::try_from(item.offset + item.length).ok()?,
    ))
}

pub fn severity_for(item: &LanguageToolMatch, options: &ClientOptions) -> DiagnosticSeverity {
    if !options.diagnostic_severity_auto {
        return options.configured_severity();
    }

    let Some(rule) = item.rule.as_deref() else {
        return options.configured_severity();
    };

    if is_spelling_rule(&rule.id) {
        return DiagnosticSeverity::WARNING;
    }

    match rule.category.id.as_deref() {
        Some("GRAMMAR" | "PUNCTUATION" | "TYPOGRAPHY") => DiagnosticSeverity::WARNING,
        _ => options.configured_severity(),
    }
}

pub fn is_spelling_rule(rule_id: &str) -> bool {
    [
        "MORFOLOGIK_RULE",
        "SPELLER_RULE",
        "HUNSPELL_NO_SUGGEST_RULE",
        "HUNSPELL_RULE",
        "FR_SPELLING_RULE",
    ]
    .iter()
    .any(|marker| rule_id.contains(marker))
}

pub fn parse_diagnostic_data(diagnostic: &Diagnostic) -> Option<DiagnosticData> {
    diagnostic
        .data
        .as_ref()
        .and_then(|value| serde_json::from_value(value.clone()).ok())
}

fn code_description(rule_id: &str, language: &str) -> Option<CodeDescription> {
    if language.trim().is_empty() {
        return None;
    }
    let uri = format!(
        "https://community.languagetool.org/rule/show/{}?lang={}",
        urlencoding::encode(rule_id),
        urlencoding::encode(language)
    );
    uri.parse::<Uri>().ok().map(|href| CodeDescription { href })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::languagetool::{LanguageToolCategory, LanguageToolReplacement, LanguageToolRule};

    fn lt_match(rule_id: &str, category_id: &str) -> LanguageToolMatch {
        LanguageToolMatch {
            message: "message".to_string(),
            short_message: None,
            offset: 0,
            length: 4,
            replacements: Vec::<LanguageToolReplacement>::new(),
            context: Box::default(),
            sentence: String::new(),
            rule: Some(Box::new(LanguageToolRule {
                id: rule_id.to_string(),
                sub_id: None,
                description: String::new(),
                urls: None,
                issue_type: None,
                category: Box::new(LanguageToolCategory {
                    id: Some(category_id.to_string()),
                    name: None,
                }),
            })),
        }
    }

    #[test]
    fn detects_spelling_rules() {
        assert!(is_spelling_rule("MORFOLOGIK_RULE_EN_US"));
        assert!(is_spelling_rule("HUNSPELL_RULE"));
        assert!(!is_spelling_rule("THIS_NNS"));
    }

    #[test]
    fn maps_grammar_to_warning() {
        let options = ClientOptions::default();
        assert_eq!(
            severity_for(&lt_match("THIS_NNS", "GRAMMAR"), &options),
            DiagnosticSeverity::WARNING
        );
    }
}
