//! `.cwt` field-type string parsing: the shared parser for both left-hand keys
//! and right-hand values, plus its range/sentinel helpers.

use super::*;

/// Shared field parser for both left-hand keys and right-hand values.
/// Matches F# processKey (RulesParser.fs:371-567).
pub(crate) fn field_from_string(s: &str) -> NewField {
    let trimmed = s.trim().trim_matches('"');

    match trimmed {
        "scalar" => return NewField::ScalarField,
        "bool" => return NewField::ValueField(ValueType::Bool),
        "int" => {
            return NewField::ValueField(ValueType::Int {
                min: INT_MIN,
                max: INT_MAX,
            });
        }
        "float" => {
            return NewField::ValueField(ValueType::Float {
                min: FLOAT_MIN,
                max: FLOAT_MAX,
            });
        }
        "percentage_field" => return NewField::ValueField(ValueType::Percent),
        "localisation" => {
            return NewField::LocalisationField {
                synced: false,
                is_inline: false,
            };
        }
        "localisation_synced" => {
            return NewField::LocalisationField {
                synced: true,
                is_inline: false,
            };
        }
        "localisation_inline" => {
            return NewField::LocalisationField {
                synced: false,
                is_inline: true,
            };
        }
        "filepath" => {
            return NewField::FilepathField {
                prefix: None,
                extension: None,
            };
        }
        "date_field" => return NewField::ValueField(ValueType::Date),
        "datetime_field" => return NewField::ValueField(ValueType::DateTime),
        "scope_field" => return NewField::ScopeField(vec!["any".to_string()]),
        "variable_field" => {
            return NewField::VariableField {
                is_int: false,
                is_32bit: false,
                min: FLOAT_MIN,
                max: FLOAT_MAX,
            };
        }
        "int_variable_field" => {
            return NewField::VariableField {
                is_int: true,
                is_32bit: false,
                min: INT_MIN as f64,
                max: INT_MAX as f64,
            };
        }
        "variable_field_32" => {
            return NewField::VariableField {
                is_int: false,
                is_32bit: true,
                min: FLOAT_MIN,
                max: FLOAT_MAX,
            };
        }
        "int_variable_field_32" => {
            return NewField::VariableField {
                is_int: true,
                is_32bit: true,
                min: INT_MIN as f64,
                max: INT_MAX as f64,
            };
        }
        "value_field" => {
            return NewField::ValueScopeMarkerField {
                is_int: false,
                min: FLOAT_MIN,
                max: FLOAT_MAX,
            };
        }
        "int_value_field" => {
            return NewField::ValueScopeMarkerField {
                is_int: true,
                min: INT_MIN as f64,
                max: INT_MAX as f64,
            };
        }
        "portrait_dna_field" => return NewField::ValueField(ValueType::Ck2Dna),
        "portrait_properties_field" => return NewField::ValueField(ValueType::Ck2DnaProperty),
        // Legacy aliases from earlier implementation
        "ck2_dna_field" => return NewField::ValueField(ValueType::Ck2Dna),
        "ck2_dna_property_field" => return NewField::ValueField(ValueType::Ck2DnaProperty),
        "ir_family_name_field" => return NewField::ValueField(ValueType::IrFamilyName),
        "ir_country_tag_field" => return NewField::MarkerField(Marker::IrCountryTag),
        "colour_field" | "color_field" => return NewField::MarkerField(Marker::ColourField),
        "ignore_field" => return NewField::IgnoreMarkerField,
        "stellaris_name_format" => {
            return NewField::ValueField(ValueType::StlNameFormat(String::new()));
        }
        "percent_field" => return NewField::ValueField(ValueType::Percent),
        _ => {}
    }

    // filepath[folder] or filepath[folder,ext]
    if trimmed.starts_with("filepath[") && trimmed.ends_with(']') {
        let inner = &trimmed[9..trimmed.len() - 1];
        return if inner.contains(',') {
            let mut parts = inner.splitn(2, ',');
            let folder = parts.next().unwrap_or("").to_string();
            let ext = parts.next().unwrap_or("").to_string();
            NewField::FilepathField {
                prefix: Some(folder),
                extension: Some(ext),
            }
        } else {
            NewField::FilepathField {
                prefix: Some(inner.to_string()),
                extension: None,
            }
        };
    }

    // int[min..max] with inf/-inf sentinels
    if trimmed.starts_with("int[") && trimmed.ends_with(']') {
        let inner = &trimmed[4..trimmed.len() - 1];
        if let Some((min_s, max_s)) = inner.split_once("..") {
            let min = parse_int_sentinel(min_s.trim());
            let max = parse_int_sentinel(max_s.trim());
            if let (Some(mn), Some(mx)) = (min, max) {
                return NewField::ValueField(ValueType::Int { min: mn, max: mx });
            }
        }
        return NewField::ValueField(ValueType::Int {
            min: INT_MIN,
            max: INT_MAX,
        });
    }

    // float[min..max] with inf/-inf sentinels
    if trimmed.starts_with("float[") && trimmed.ends_with(']') {
        let inner = &trimmed[6..trimmed.len() - 1];
        if let Some((min_s, max_s)) = inner.split_once("..") {
            let min = parse_float_sentinel(min_s.trim());
            let max = parse_float_sentinel(max_s.trim());
            if let (Some(mn), Some(mx)) = (min, max) {
                return NewField::ValueField(ValueType::Float { min: mn, max: mx });
            }
        }
        return NewField::ValueField(ValueType::Float {
            min: FLOAT_MIN,
            max: FLOAT_MAX,
        });
    }

    // enum[x]
    if trimmed.starts_with("enum[") && trimmed.ends_with(']') {
        let name = &trimmed[5..trimmed.len() - 1];
        return NewField::ValueField(ValueType::Enum(name.to_string()));
    }

    // complex_enum[x] — referenced on right side
    if trimmed.starts_with("complex_enum[") && trimmed.ends_with(']') {
        let name = &trimmed[13..trimmed.len() - 1];
        return NewField::ValueField(ValueType::Enum(name.to_string()));
    }

    // value[x] -> VariableGetField
    if trimmed.starts_with("value[") && trimmed.ends_with(']') {
        let var = &trimmed[6..trimmed.len() - 1];
        return NewField::VariableGetField(var.to_string());
    }

    // value_set[x] -> VariableSetField
    if trimmed.starts_with("value_set[") && trimmed.ends_with(']') {
        let var = &trimmed[10..trimmed.len() - 1];
        return NewField::VariableSetField(var.to_string());
    }

    // scope[x]
    if trimmed.starts_with("scope[") && trimmed.ends_with(']') {
        let scope = &trimmed[6..trimmed.len() - 1];
        return NewField::ScopeField(vec![scope.to_string()]);
    }

    // event_target[x] — same as scope[x]
    if trimmed.starts_with("event_target[") && trimmed.ends_with(']') {
        let scope = &trimmed[13..trimmed.len() - 1];
        return NewField::ScopeField(vec![scope.to_string()]);
    }

    // scope_group[x] — resolve to ScopeField with known group or scalar fallback
    if trimmed.starts_with("scope_group[") && trimmed.ends_with(']') {
        let _group = &trimmed[12..trimmed.len() - 1];
        // Without a scope-group map we fall back to any-scope
        return NewField::ScopeField(vec!["any".to_string()]);
    }

    // alias_keys_field[x] -> AliasValueKeysField
    if trimmed.starts_with("alias_keys_field[") && trimmed.ends_with(']') {
        let alias = &trimmed[17..trimmed.len() - 1];
        return NewField::AliasValueKeysField(alias.to_string());
    }

    // alias_name[x] / alias_match_left[x] -> AliasField
    if (trimmed.starts_with("alias_name[") || trimmed.starts_with("alias_match_left["))
        && trimmed.ends_with(']')
    {
        let inner_start = trimmed.find('[').unwrap() + 1;
        let alias = &trimmed[inner_start..trimmed.len() - 1];
        return NewField::AliasField(alias.to_string());
    }

    // single_alias_right[x] -> SingleAliasField
    if trimmed.starts_with("single_alias_right[") && trimmed.ends_with(']') {
        let alias = &trimmed[19..trimmed.len() - 1];
        return NewField::SingleAliasField(alias.to_string());
    }

    // icon[folder] -> IconField
    if trimmed.starts_with("icon[") && trimmed.ends_with(']') {
        let folder = &trimmed[5..trimmed.len() - 1];
        return NewField::IconField(folder.to_string());
    }

    // variable_field[min..max]
    if trimmed.starts_with("variable_field[") && trimmed.ends_with(']') {
        let inner = &trimmed[15..trimmed.len() - 1];
        let (mn, mx) = parse_float_range(inner, FLOAT_MIN, FLOAT_MAX);
        return NewField::VariableField {
            is_int: false,
            is_32bit: false,
            min: mn,
            max: mx,
        };
    }

    // int_variable_field[min..max]
    if trimmed.starts_with("int_variable_field[") && trimmed.ends_with(']') {
        let inner = &trimmed[19..trimmed.len() - 1];
        let (mn, mx) = parse_int_range_as_float(inner, INT_MIN as f64, INT_MAX as f64);
        return NewField::VariableField {
            is_int: true,
            is_32bit: false,
            min: mn,
            max: mx,
        };
    }

    // variable_field_32[min..max]
    if trimmed.starts_with("variable_field_32[") && trimmed.ends_with(']') {
        let inner = &trimmed[18..trimmed.len() - 1];
        let (mn, mx) = parse_float_range(inner, FLOAT_MIN, FLOAT_MAX);
        return NewField::VariableField {
            is_int: false,
            is_32bit: true,
            min: mn,
            max: mx,
        };
    }

    // int_variable_field_32[min..max]
    if trimmed.starts_with("int_variable_field_32[") && trimmed.ends_with(']') {
        let inner = &trimmed[22..trimmed.len() - 1];
        let (mn, mx) = parse_int_range_as_float(inner, INT_MIN as f64, INT_MAX as f64);
        return NewField::VariableField {
            is_int: true,
            is_32bit: true,
            min: mn,
            max: mx,
        };
    }

    // value_field[min..max]
    if trimmed.starts_with("value_field[") && trimmed.ends_with(']') {
        let inner = &trimmed[12..trimmed.len() - 1];
        let (mn, mx) = parse_float_range(inner, FLOAT_MIN, FLOAT_MAX);
        return NewField::ValueScopeMarkerField {
            is_int: false,
            min: mn,
            max: mx,
        };
    }

    // int_value_field[min..max]
    if trimmed.starts_with("int_value_field[") && trimmed.ends_with(']') {
        let inner = &trimmed[16..trimmed.len() - 1];
        let (mn, mx) = parse_int_range_as_float(inner, INT_MIN as f64, INT_MAX as f64);
        return NewField::ValueScopeMarkerField {
            is_int: true,
            min: mn,
            max: mx,
        };
    }

    // stellaris_name_format[x]
    if trimmed.starts_with("stellaris_name_format[") && trimmed.ends_with(']') {
        let var = &trimmed[22..trimmed.len() - 1];
        return NewField::ValueField(ValueType::StlNameFormat(var.to_string()));
    }

    // colour[rgb] / colour[hsv] handled in children_to_rules at leaf level
    // Here we just emit the marker so the post-processor can expand it
    if trimmed.starts_with("colour[") && trimmed.ends_with(']') {
        return NewField::MarkerField(Marker::ColourField);
    }

    // <type> simple
    if trimmed.starts_with('<') && trimmed.ends_with('>') && !trimmed.contains(' ') {
        let type_ref = &trimmed[1..trimmed.len() - 1];
        return NewField::TypeField(TypeType::Simple(type_ref.to_string()));
    }

    // prefix<type>suffix complex
    if trimmed.contains('<') && trimmed.contains('>') {
        let s = trimmed.trim_matches('"');
        if let (Some(pi), Some(si)) = (s.find('<'), s.find('>'))
            && pi < si
        {
            let prefix = s[..pi].to_string();
            let name = s[pi + 1..si].to_string();
            let suffix = s[si + 1..].to_string();
            return NewField::TypeField(TypeType::Complex {
                prefix,
                name,
                suffix,
            });
        }
    }

    // Default: specific string value
    NewField::SpecificField(trimmed.to_string())
}

fn parse_int_sentinel(s: &str) -> Option<i32> {
    match s {
        "inf" => Some(INT_MAX),
        "-inf" => Some(INT_MIN),
        other => other.parse::<i32>().ok(),
    }
}

fn parse_float_sentinel(s: &str) -> Option<f64> {
    match s {
        "inf" => Some(FLOAT_MAX),
        "-inf" => Some(FLOAT_MIN),
        other => other.parse::<f64>().ok(),
    }
}

fn parse_float_range(inner: &str, default_min: f64, default_max: f64) -> (f64, f64) {
    if let Some((min_s, max_s)) = inner.split_once("..") {
        let mn = parse_float_sentinel(min_s.trim()).unwrap_or(default_min);
        let mx = parse_float_sentinel(max_s.trim()).unwrap_or(default_max);
        (mn, mx)
    } else {
        (default_min, default_max)
    }
}

fn parse_int_range_as_float(inner: &str, default_min: f64, default_max: f64) -> (f64, f64) {
    if let Some((min_s, max_s)) = inner.split_once("..") {
        let mn = parse_int_sentinel(min_s.trim())
            .map(|v| v as f64)
            .unwrap_or(default_min);
        let mx = parse_int_sentinel(max_s.trim())
            .map(|v| v as f64)
            .unwrap_or(default_max);
        (mn, mx)
    } else {
        (default_min, default_max)
    }
}
