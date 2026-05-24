use crate::config::ClientOptions;
use languagetool_client as api;
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum LanguageToolError {
    #[error("LanguageTool request to {endpoint} failed: {source}")]
    Request {
        endpoint: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("LanguageTool request to {endpoint} failed: {source}")]
    Api {
        endpoint: String,
        #[source]
        source: api::apis::Error<api::apis::default_api::CheckPostError>,
    },
}

#[derive(Debug, Clone)]
pub struct LanguageToolClient;

impl Default for LanguageToolClient {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageToolClient {
    pub fn new() -> Self {
        Self
    }

    pub async fn check(
        &self,
        text: &str,
        options: &ClientOptions,
    ) -> Result<LanguageToolResponse, LanguageToolError> {
        self.check_with_payload(CheckPayload::Text(text), options)
            .await
    }

    pub async fn check_annotated(
        &self,
        data: &AnnotatedText,
        options: &ClientOptions,
    ) -> Result<LanguageToolResponse, LanguageToolError> {
        self.check_with_payload(CheckPayload::Data(data), options)
            .await
    }

    async fn check_with_payload(
        &self,
        payload: CheckPayload<'_>,
        options: &ClientOptions,
    ) -> Result<LanguageToolResponse, LanguageToolError> {
        let endpoint = options.endpoint();
        let preferred_variants = join_parameter(&options.preferred_variants);
        let disabled_rules = join_parameter(&options.disabled_rules);
        let disabled_categories = join_parameter(&options.disabled_categories);
        let enabled_rules = join_parameter(&options.enabled_rules);
        let enabled_categories = join_parameter(&options.enabled_categories);

        let (text, data_json) = match payload {
            CheckPayload::Text(text) => (Some(text), None),
            CheckPayload::Data(data) => (
                None,
                Some(serde_json::to_string(data).expect("annotated text should serialize")),
            ),
        };
        let mother_tongue = options
            .mother_tongue
            .as_deref()
            .filter(|value| !value.trim().is_empty());
        let preferred_variants = none_if_empty(&preferred_variants);
        let disabled_rules = none_if_empty(&disabled_rules);
        let disabled_categories = none_if_empty(&disabled_categories);
        let enabled_rules = none_if_empty(&enabled_rules);
        let enabled_categories = none_if_empty(&enabled_categories);
        let level = options.level.map(|level| level.as_str());

        let (username, api_key) = match (&options.username, &options.api_key) {
            (Some(username), Some(api_key))
                if !username.trim().is_empty() && !api_key.trim().is_empty() =>
            {
                (Some(username.as_str()), Some(api_key.as_str()))
            }
            _ => (None, None),
        };

        let client = reqwest::Client::builder()
            .timeout(options.timeout())
            .build()
            .map_err(|source| LanguageToolError::Request {
                endpoint: endpoint.clone(),
                source,
            })?;
        let configuration = api::apis::configuration::Configuration {
            base_path: endpoint.trim_end_matches("/check").to_string(),
            client,
            ..api::apis::configuration::Configuration::default()
        };

        api::apis::default_api::check_post(
            &configuration,
            &options.language,
            text,
            data_json.as_deref(),
            username,
            api_key,
            None,
            mother_tongue,
            preferred_variants,
            enabled_rules,
            disabled_rules,
            enabled_categories,
            disabled_categories,
            None,
            level,
        )
        .await
        .map(LanguageToolResponse::from)
        .map_err(|source| LanguageToolError::Api { endpoint, source })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageToolResponse {
    pub software: Option<LanguageToolSoftware>,
    pub matches: Vec<LanguageToolMatch>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageToolSoftware {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageToolMatch {
    pub message: String,
    pub offset: i32,
    pub length: i32,
    pub replacements: Vec<String>,
    pub rule: Option<LanguageToolRule>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageToolRule {
    pub id: String,
    pub issue_type: Option<String>,
    pub category: Option<LanguageToolCategory>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageToolCategory {
    pub id: Option<String>,
}

impl From<api::models::CheckPost200Response> for LanguageToolResponse {
    fn from(response: api::models::CheckPost200Response) -> Self {
        Self {
            software: response.software.map(|software| LanguageToolSoftware {
                name: software.name,
                version: software.version,
            }),
            matches: response
                .matches
                .unwrap_or_default()
                .into_iter()
                .map(LanguageToolMatch::from)
                .collect(),
        }
    }
}

impl From<api::models::CheckPost200ResponseMatchesInner> for LanguageToolMatch {
    fn from(item: api::models::CheckPost200ResponseMatchesInner) -> Self {
        Self {
            message: item.message,
            offset: item.offset,
            length: item.length,
            replacements: item
                .replacements
                .into_iter()
                .filter_map(|replacement| replacement.value)
                .collect(),
            rule: item.rule.map(|rule| LanguageToolRule {
                id: rule.id,
                issue_type: rule.issue_type,
                category: Some(LanguageToolCategory {
                    id: rule.category.id,
                }),
            }),
        }
    }
}

enum CheckPayload<'a> {
    Text(&'a str),
    Data(&'a AnnotatedText),
}

fn join_parameter(values: &[String]) -> String {
    values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(",")
}

fn none_if_empty(value: &str) -> Option<&str> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AnnotatedText {
    pub annotation: Vec<Annotation>,
}

impl AnnotatedText {
    pub fn has_text(&self) -> bool {
        self.annotation.iter().any(|annotation| {
            annotation
                .as_text()
                .is_some_and(|text| !text.trim().is_empty())
        })
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum Annotation {
    Text {
        text: String,
    },
    Markup {
        markup: String,
        #[serde(rename = "interpretAs", skip_serializing_if = "Option::is_none")]
        interpret_as: Option<String>,
    },
}

impl Annotation {
    pub fn text(text: String) -> Self {
        Self::Text { text }
    }

    pub fn markup(markup: String, interpret_as: Option<String>) -> Self {
        Self::Markup {
            markup,
            interpret_as,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Annotation::Text { text } => Some(text),
            Annotation::Markup { .. } => None,
        }
    }

    pub fn as_markup(&self) -> Option<&str> {
        match self {
            Annotation::Text { .. } => None,
            Annotation::Markup { markup, .. } => Some(markup),
        }
    }

    pub fn interpret_as(&self) -> Option<&str> {
        match self {
            Annotation::Text { .. } => None,
            Annotation::Markup { interpret_as, .. } => interpret_as.as_deref(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_language_tool_response() {
        let json = r#"{
          "software": {
            "name": "LanguageTool",
            "version": "6.6",
            "buildDate": "2024-01-01",
            "apiVersion": 1
          },
          "warnings": {"incompleteResults": false},
          "language": {
            "name": "English",
            "code": "en-US",
            "detectedLanguage": {"name": "English", "code": "en"}
          },
          "matches": [{
            "message": "Possible spelling mistake found.",
            "shortMessage": "Spelling mistake",
            "offset": 11,
            "length": 4,
            "replacements": [{"value": "test"}],
            "context": {"text": "This are a tset.", "offset": 11, "length": 4},
            "sentence": "This are a tset.",
            "rule": {
              "id": "MORFOLOGIK_RULE_EN_US",
              "description": "Possible spelling mistake",
              "issueType": "misspelling",
              "category": {"id": "TYPOS", "name": "Possible Typo"}
            }
          }]
        }"#;

        let response = LanguageToolResponse::from(
            serde_json::from_str::<api::models::CheckPost200Response>(json).unwrap(),
        );
        assert_eq!(response.matches.len(), 1);
        assert_eq!(response.matches[0].replacements, vec!["test"]);
        assert_eq!(
            response.matches[0]
                .rule
                .as_ref()
                .map(|rule| rule.id.as_str()),
            Some("MORFOLOGIK_RULE_EN_US")
        );
    }
}
