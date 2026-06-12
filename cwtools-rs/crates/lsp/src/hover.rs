use std::collections::HashMap;

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::{Hover, HoverContents, HoverParams, MarkupContent, MarkupKind};

use cwtools_info::{PositionElement, ReferenceHint, element_at_position};

use crate::Backend;
use crate::RuleCursorInfo;
use crate::paths::{lang_display_name, logical_path_from_uri};

impl Backend {
    pub(crate) async fn hover_impl(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .to_string();
        let pos = params.text_document_position_params.position;

        // Snapshot the AST (a cheap `Arc` clone) and drop the documents guard
        // before taking ruleset, so hover never co-holds documents + ruleset and
        // its documents window stays tiny.
        let ast = {
            let docs = self.state.documents.lock();
            docs.get(&uri).and_then(|d| d.ast.clone())
        };
        if let Some(ast) = ast {
            let ws_uri = self.state.workspace_uri.lock().clone();
            let logical_path = logical_path_from_uri(&uri, &ws_uri);

            if let Some(RuleCursorInfo {
                element,
                hint,
                category,
                description: desc,
                required_scopes: scopes,
            }) = self.rule_info_at_cursor(&uri, pos, &logical_path)
            {
                let mut md = build_hover_markdown(
                    &element,
                    &hint,
                    category.as_deref(),
                    desc.as_deref(),
                    &scopes,
                );
                // For a variable read, append the known assigned value(s) so the
                // user sees what it resolves to without chasing the definition.
                if let ReferenceHint::Variable { name, .. } = &hint {
                    let info_guard = self.state.info_service.read();
                    let (values, more) = info_guard.variable_values(name, 5);
                    if !values.is_empty() {
                        let joined = values
                            .iter()
                            .map(|v| format!("`{}`", v))
                            .collect::<Vec<_>>()
                            .join(", ");
                        let suffix = if more { ", +more" } else { "" };
                        md.push_str(&format!("\n\nSet to: {}{}", joined, suffix));
                    }
                }
                // Append localisation translations. A reference resolves by leaf
                // value; a definition key (idea/decision) resolves by its key.
                append_localisation(&mut md, &element, &self.state.loc_text.read());
                return Ok(Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: md,
                    }),
                    range: None,
                }));
            }

            // Fallback: no-rule position finder
            if let Some(element) = element_at_position(
                &ast,
                pos.line + 1,
                pos.character as u16,
                &self.state.string_table,
            ) {
                let mut contents = match &element {
                    PositionElement::Leaf { key, value } => {
                        format!("**Field**: `{} = {}`", key, value)
                    }
                    PositionElement::LeafValue { value } => {
                        format!("**Value**: `{}`", value)
                    }
                };
                // Append localisation translations for the hovered element.
                append_localisation(&mut contents, &element, &self.state.loc_text.read());
                return Ok(Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: contents,
                    }),
                    range: None,
                }));
            }
        }
        Ok(None)
    }
}

/// Build a Markdown hover string from the classified element + the matched
/// rule's category, description, and required scopes (from
/// `rule_info_at_cursor`).
pub(crate) fn build_hover_markdown(
    element: &PositionElement,
    hint: &ReferenceHint,
    category: Option<&str>,
    rule_desc: Option<&str>,
    rule_scopes: &[String],
) -> String {
    let mut parts: Vec<String> = Vec::new();

    // Header: which alias category the key resolves through, so a trigger
    // reads as a trigger and an effect as an effect at a glance.
    if let (Some(cat), PositionElement::Leaf { key, .. }) = (category, element) {
        let label = match cat {
            "trigger" => "Trigger",
            "effect" => "Effect",
            "modifier" => "Modifier",
            other => other,
        };
        parts.push(format!("**{}** `{}`", label, key));
    }

    // Primary classification
    match hint {
        ReferenceHint::TypeRef { type_name, value } => {
            parts.push(format!(
                "**Type reference** — `{}` (`{}`)",
                value, type_name
            ));
        }
        ReferenceHint::EnumRef { enum_name, value } => {
            parts.push(format!(
                "**Enum value** — `{}` (member of `{}`)",
                value, enum_name
            ));
        }
        ReferenceHint::LocRef { key } => {
            parts.push(format!("**Localisation key** — `{}`", key));
        }
        ReferenceHint::FileRef { path } => {
            parts.push(format!("**File path** — `{}`", path));
        }
        ReferenceHint::ScopeName { name } => {
            parts.push(format!("**Scope** — `{}`", name));
        }
        ReferenceHint::Variable { name, namespace } => {
            parts.push(format!(
                "**Variable** — `{}` (namespace `{}`)",
                name, namespace
            ));
        }
        ReferenceHint::Unknown => {
            // Fall back to the raw element description
            match element {
                PositionElement::Leaf { key, value } => {
                    parts.push(format!("**Field** — `{} = {}`", key, value));
                }
                PositionElement::LeafValue { value } => {
                    parts.push(format!("**Value** — `{}`", value));
                }
            }
        }
    }

    // Append rule description if found
    if let Some(desc) = rule_desc {
        parts.push(format!("\n{}", desc));
    }

    // Append required scopes if any
    if !rule_scopes.is_empty() {
        parts.push(format!("\n**Required scopes**: {}", rule_scopes.join(", ")));
    }

    parts.join("\n\n")
}

/// Append localisation translations for the hovered element to `md`.
///
/// A reference (`add_ideas = DEN_Maersk1`) looks up the leaf value. A definition
/// key (`DEN_Maersk1 = { ... }`, value empty) looks up the key itself: for ideas,
/// decisions and similar entities the token IS the loc key, and the `<key>_desc`
/// entry holds the description tooltip. The cwt config doesn't model this, so it
/// can't be resolved through the rule walk.
pub(crate) fn append_localisation(
    md: &mut String,
    element: &PositionElement,
    loc_text: &HashMap<String, Vec<(cwtools_localization::Lang, String)>>,
) {
    let (name_key, desc_key): (Option<String>, Option<String>) = match element {
        PositionElement::Leaf { key, value } if value.is_empty() => {
            let k = key.to_lowercase();
            (Some(k.clone()), Some(format!("{k}_desc")))
        }
        PositionElement::Leaf { value, .. } => (Some(value.to_lowercase()), None),
        PositionElement::LeafValue { value } => (Some(value.to_lowercase()), None),
    };
    let mut emit = |loc_key: &str, label: &str| {
        if let Some(translations) = loc_text.get(loc_key) {
            md.push_str(label);
            for (lang, text) in translations {
                md.push_str(&format!("\n- {}: {}", lang_display_name(*lang), text));
            }
        }
    };
    if let Some(nk) = name_key {
        emit(&nk, "\n\n**Localisation**:");
    }
    if let Some(dk) = desc_key {
        emit(&dk, "\n\n**Description**:");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hover_type_ref() {
        let md = build_hover_markdown(
            &PositionElement::Leaf {
                key: "ethos".to_string(),
                value: "my_ethos".to_string(),
            },
            &ReferenceHint::TypeRef {
                type_name: "ethoses".to_string(),
                value: "my_ethos".to_string(),
            },
            None,
            None,
            &[],
        );
        assert!(md.contains("Type reference"), "got: {}", md);
        assert!(md.contains("my_ethos"), "got: {}", md);
        assert!(md.contains("ethoses"), "got: {}", md);
    }

    #[test]
    fn test_hover_enum_ref() {
        let md = build_hover_markdown(
            &PositionElement::Leaf {
                key: "kind".to_string(),
                value: "alpha".to_string(),
            },
            &ReferenceHint::EnumRef {
                enum_name: "my_enum".to_string(),
                value: "alpha".to_string(),
            },
            None,
            None,
            &[],
        );
        assert!(md.contains("Enum value"), "got: {}", md);
        assert!(md.contains("alpha"), "got: {}", md);
        assert!(md.contains("my_enum"), "got: {}", md);
    }

    #[test]
    fn test_hover_unknown_falls_back_to_raw() {
        let md = build_hover_markdown(
            &PositionElement::Leaf {
                key: "foo".to_string(),
                value: "bar".to_string(),
            },
            &ReferenceHint::Unknown,
            None,
            None,
            &[],
        );
        assert!(md.contains("foo") && md.contains("bar"), "got: {}", md);
    }

    #[test]
    fn test_hover_with_rule_description() {
        let md = build_hover_markdown(
            &PositionElement::Leaf {
                key: "kind".to_string(),
                value: "alpha".to_string(),
            },
            &ReferenceHint::EnumRef {
                enum_name: "my_enum".to_string(),
                value: "alpha".to_string(),
            },
            None,
            Some("The kind of this thing"),
            &["country".to_string()],
        );
        assert!(md.contains("The kind of this thing"), "got: {}", md);
        assert!(md.contains("Required scopes"), "got: {}", md);
    }
}
