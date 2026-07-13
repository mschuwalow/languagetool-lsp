use crate::config::ClientOptions;
use crate::languagetool::LanguageToolMatch;
use crate::text_index::{ByteRange, Utf16Range};
use serde::{Deserialize, Serialize};
use tower_lsp_server::ls_types::{
    CodeDescription, Diagnostic, DiagnosticSeverity, NumberOrString, Range, Uri,
};

pub const SOURCE: &str = "LanguageTool";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticData {
    pub rule_id: String,
    pub category_id: Option<String>,
    pub issue_type: Option<String>,
    pub replacements: Vec<String>,
    pub matched_text: String,
    pub document_version: i32,
}

/// A diagnostic before it enters the cache, and while it lives in the cache.
///
/// `diagnostic` holds all fields except `data` (its `data` field is `None`).
/// `data` is kept in deserialized form so it can be cheaply updated on edits
/// and serialized exactly once at publish time via [`RawDiagnostic::finalize`].
#[derive(Debug, Clone)]
pub struct RawDiagnostic {
    pub doc_byte_range: ByteRange,
    pub diagnostic: Diagnostic,
    pub data: DiagnosticData,
}

impl RawDiagnostic {
    /// Serialize `data` into `diagnostic.data` and return the publishable [`Diagnostic`].
    pub fn finalize(&self) -> Diagnostic {
        let mut diagnostic = self.diagnostic.clone();
        diagnostic.data = serde_json::to_value(&self.data).ok();
        diagnostic
    }
}

#[derive(Debug, Clone)]
pub struct CheckedBlock {
    pub byte_range: ByteRange,
    pub diagnostics: Vec<RawDiagnostic>,
}

pub fn make_lsp_diagnostic_for_range(
    range: Range,
    item: &LanguageToolMatch,
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
        data: None,
    }
}

pub fn diagnostic_data_for_text(
    matched_text: String,
    item: &LanguageToolMatch,
    options: &ClientOptions,
    document_version: i32,
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
    let default_severity = options.default_diagnostic_severity.as_lsp();

    let Some(category_id) = item.rule.as_ref().and_then(|r| r.category.id.clone()) else {
        return default_severity;
    };

    options
        .diagnostic_severity_overrides
        .get(&category_id)
        .map(|s| s.as_lsp())
        .unwrap_or(default_severity)
}

pub fn parse_diagnostic_data(diagnostic: &Diagnostic) -> Option<DiagnosticData> {
    diagnostic.data.as_ref().and_then(|value| {
        serde_json::from_value(value.clone())
            .map_err(|err| {
                log::debug!("Failed to parse diagnostic data: {err}");
            })
            .ok()
    })
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
    fn maps_grammar_to_hint() {
        let options = ClientOptions::default();
        assert_eq!(
            severity_for(&lt_match("THIS_NNS", "GRAMMAR"), &options),
            DiagnosticSeverity::HINT
        );
    }
}
