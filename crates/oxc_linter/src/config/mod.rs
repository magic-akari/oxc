use std::path::Path;

pub mod errors;
use oxc_diagnostics::{Error, FailedToOpenFileError, Report};
use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::Value;

use crate::{rules::RuleEnum, AllowWarnDeny, JsxA11y, LintSettings};

use self::errors::{
    FailedToParseConfigError, FailedToParseConfigJsonError, FailedToParseRuleValueError,
};

pub struct ESLintConfig {
    rules: Vec<ESLintRuleConfig>,
    settings: LintSettings,
}

#[derive(Debug)]
pub struct ESLintRuleConfig {
    plugin_name: String,
    rule_name: String,
    severity: AllowWarnDeny,
    config: Option<serde_json::Value>,
}

impl ESLintConfig {
    pub fn new(path: &Path) -> Result<Self, Report> {
        let json = Self::read_json(path)?;
        let rules = parse_rules(&json)?;
        let settings = parse_settings_from_root(&json);
        Ok(Self { rules, settings })
    }

    pub fn settings(self) -> LintSettings {
        self.settings
    }

    fn read_json(path: &Path) -> Result<serde_json::Value, Error> {
        let file = match std::fs::read_to_string(path) {
            Ok(file) => file,
            Err(e) => {
                return Err(FailedToParseConfigError(vec![Error::new(FailedToOpenFileError(
                    path.to_path_buf(),
                    e,
                ))])
                .into());
            }
        };

        serde_json::from_str::<serde_json::Value>(&file).map_err(|err| {
            let guess = mime_guess::from_path(path);
            let err = match guess.first() {
                // syntax error
                Some(mime) if mime.subtype() == "json" => err.to_string(),
                Some(_) => "only json configuration is supported".to_string(),
                None => {
                    format!(
                        "{err}, if the configuration is not a json file, please use json instead."
                    )
                }
            };
            FailedToParseConfigError(vec![Error::new(FailedToParseConfigJsonError(
                path.to_path_buf(),
                err,
            ))])
            .into()
        })
    }

    #[allow(clippy::option_if_let_else)]
    pub fn override_rules(
        &self,
        rules_for_override: &mut FxHashSet<RuleEnum>,
        all_rules: &[RuleEnum],
    ) {
        use itertools::Itertools;
        let mut rules_to_replace = vec![];
        let mut rules_to_remove = vec![];

        // Rules can have the same name but different plugin names
        let lookup = self.rules.iter().into_group_map_by(|r| r.rule_name.as_str());

        for (name, rule_configs) in &lookup {
            match rule_configs.len() {
                0 => unreachable!(),
                1 => {
                    let rule_config = &rule_configs[0];
                    let rule_name = &rule_config.rule_name;
                    let plugin_name = &rule_config.plugin_name;
                    match rule_config.severity {
                        AllowWarnDeny::Warn | AllowWarnDeny::Deny => {
                            if let Some(rule) = all_rules
                                .iter()
                                .find(|r| r.name() == rule_name && r.plugin_name() == plugin_name)
                            {
                                rules_to_replace.push(rule.read_json(rule_config.config.clone()));
                            }
                        }
                        AllowWarnDeny::Allow => {
                            if let Some(rule) = rules_for_override
                                .iter()
                                .find(|r| r.name() == rule_name && r.plugin_name() == plugin_name)
                            {
                                rules_to_remove.push(rule.clone());
                            }
                        }
                    }
                }
                _ => {
                    // For overlapping rule names, use the "error" one
                    // "no-loss-of-precision": "off",
                    // "@typescript-eslint/no-loss-of-precision": "error"
                    if let Some(rule_config) =
                        rule_configs.iter().find(|r| r.severity.is_warn_deny())
                    {
                        if let Some(rule) = rules_for_override.iter().find(|r| r.name() == *name) {
                            rules_to_replace.push(rule.read_json(rule_config.config.clone()));
                        }
                    } else if rule_configs.iter().all(|r| r.severity.is_allow()) {
                        if let Some(rule) = rules_for_override.iter().find(|r| r.name() == *name) {
                            rules_to_remove.push(rule.clone());
                        }
                    }
                }
            }
        }

        for rule in rules_to_remove {
            rules_for_override.remove(&rule);
        }
        for rule in rules_to_replace {
            rules_for_override.replace(rule);
        }
    }
}

fn parse_rules(root_json: &Value) -> Result<Vec<ESLintRuleConfig>, Error> {
    let Value::Object(rules_object) = root_json else { return Ok(Vec::default()) };

    let Some(Value::Object(rules_object)) = rules_object.get("rules") else {
        return Ok(Vec::default());
    };

    rules_object
        .into_iter()
        .map(|(key, value)| {
            let (plugin_name, rule_name) = parse_rule_name(key);
            let (severity, config) = resolve_rule_value(value)?;
            Ok(ESLintRuleConfig {
                plugin_name: plugin_name.to_string(),
                rule_name: rule_name.to_string(),
                severity,
                config,
            })
        })
        .collect::<Result<Vec<_>, Error>>()
}

fn parse_settings_from_root(root_json: &Value) -> LintSettings {
    let Value::Object(root_object) = root_json else { return LintSettings::default() };

    let Some(settings_value) = root_object.get("settings") else { return LintSettings::default() };

    parse_settings(settings_value)
}

pub fn parse_settings(setting_value: &Value) -> LintSettings {
    if let Value::Object(settings_object) = setting_value {
        if let Some(Value::Object(jsx_a11y)) = settings_object.get("jsx-a11y") {
            let mut jsx_a11y_setting = JsxA11y::new(None, FxHashMap::default());

            if let Some(Value::Object(components)) = jsx_a11y.get("components") {
                let components_map: FxHashMap<String, String> = components
                    .iter()
                    .map(|(key, value)| (String::from(key), String::from(value.as_str().unwrap())))
                    .collect();

                jsx_a11y_setting.set_components(components_map);
            }

            if let Some(Value::String(polymorphic_prop_name)) = jsx_a11y.get("polymorphicPropName")
            {
                jsx_a11y_setting
                    .set_polymorphic_prop_name(Some(String::from(polymorphic_prop_name)));
            }

            return LintSettings::new(jsx_a11y_setting);
        }
    }

    LintSettings::default()
}

fn parse_rule_name(name: &str) -> (&str, &str) {
    if let Some((category, name)) = name.split_once('/') {
        let category = category.trim_start_matches('@');

        let category = match category {
            // if it matches typescript-eslint, map it to typescript
            "typescript-eslint" => "typescript",
            // plugin name in RuleEnum is in snake_case
            "jsx-a11y" => "jsx_a11y",
            _ => category,
        };

        (category, name)
    } else {
        ("eslint", name)
    }
}

/// Resolves the level of a rule and its config
///
/// Three cases here
/// ```json
/// {
///     "rule": "off",
///     "rule": ["off", "config"],
///     "rule": ["off", "config1", "config2"],
/// }
/// ```
fn resolve_rule_value(value: &serde_json::Value) -> Result<(AllowWarnDeny, Option<Value>), Error> {
    if let Some(v) = value.as_str() {
        return Ok((AllowWarnDeny::try_from(v)?, None));
    }

    if let Some(v) = value.as_array() {
        let mut config = Vec::new();
        for item in v.iter().skip(1).take(2) {
            config.push(item.clone());
        }
        let config = if config.is_empty() { None } else { Some(Value::Array(config)) };
        if let Some(v_idx_0) = v.first() {
            return Ok((AllowWarnDeny::try_from(v_idx_0)?, config));
        }
    }

    Err(FailedToParseRuleValueError(value.to_string(), "Invalid rule value").into())
}

#[cfg(test)]
mod test {
    use super::parse_rules;
    use std::env;

    #[test]
    fn test_parse_rules() {
        let fixture_path = env::current_dir().unwrap().join("fixtures/eslint_config.json");
        let input = std::fs::read_to_string(fixture_path).unwrap();
        let file = serde_json::from_str::<serde_json::Value>(&input).unwrap();
        let rules = parse_rules(&file).unwrap();
        insta::assert_debug_snapshot!(rules);
    }
}
