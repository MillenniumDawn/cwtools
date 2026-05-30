use crate::ErrorSeverity;

/// Structured error code catalog matching F# CWTools error codes.
/// Each code has a fixed ID (CW###), a severity level, and a message template.
#[derive(Debug, Clone, PartialEq)]
pub struct ErrorCode {
    pub id: &'static str,
    pub severity: ErrorSeverity,
    pub message_template: &'static str,
}

impl ErrorCode {
    pub fn format(&self, params: &[impl AsRef<str>]) -> String {
        let mut msg = self.message_template.to_string();
        for (i, param) in params.iter().enumerate() {
            msg = msg.replace(&format!("{{{}}}", i), param.as_ref());
        }
        msg
    }
}

// ── Error Code Catalog ─────────────────────────────────

/// Mixed key/values and values block (missing equals sign).
pub const CW002_MIXED_BLOCK: ErrorCode = ErrorCode {
    id: "CW002",
    severity: ErrorSeverity::Error,
    message_template: "This block has mixed key/values and values, it is probably a missing equals sign inside it.",
};

/// Missing localisation key.
pub const CW100_MISSING_LOCALISATION: ErrorCode = ErrorCode {
    id: "CW100",
    severity: ErrorSeverity::Warning,
    message_template: "Localisation key {} is not defined for {}",
};

/// Undefined variable (@var).
pub const CW101_UNDEFINED_VARIABLE: ErrorCode = ErrorCode {
    id: "CW101",
    severity: ErrorSeverity::Error,
    message_template: "{} is not defined",
};

/// Unknown trigger used.
pub const CW102_UNDEFINED_TRIGGER: ErrorCode = ErrorCode {
    id: "CW102",
    severity: ErrorSeverity::Error,
    message_template: "unknown trigger {} used.",
};

/// Unknown effect used.
pub const CW103_UNDEFINED_EFFECT: ErrorCode = ErrorCode {
    id: "CW103",
    severity: ErrorSeverity::Error,
    message_template: "unknown effect {} used.",
};

/// Trigger used in wrong scope.
pub const CW104_INCORRECT_TRIGGER_SCOPE: ErrorCode = ErrorCode {
    id: "CW104",
    severity: ErrorSeverity::Error,
    message_template: "Trigger {} is used in wrong scope ({} instead of {})",
};

/// Missing required field.
pub const CW200_MISSING_FIELD: ErrorCode = ErrorCode {
    id: "CW200",
    severity: ErrorSeverity::Error,
    message_template: "Missing required field {}",
};

/// Unexpected field.
pub const CW201_UNEXPECTED_FIELD: ErrorCode = ErrorCode {
    id: "CW201",
    severity: ErrorSeverity::Error,
    message_template: "Unexpected field {}",
};

/// Invalid value type.
pub const CW202_INVALID_VALUE: ErrorCode = ErrorCode {
    id: "CW202",
    severity: ErrorSeverity::Error,
    message_template: "Field '{}' has value '{}', expected {}",
};

/// Cardinality violation (too few).
pub const CW203_CARDINALITY_MIN: ErrorCode = ErrorCode {
    id: "CW203",
    severity: ErrorSeverity::Error,
    message_template: "Field '{}' appears {} time(s), expected at least {}",
};

/// Cardinality violation (too many).
pub const CW204_CARDINALITY_MAX: ErrorCode = ErrorCode {
    id: "CW204",
    severity: ErrorSeverity::Error,
    message_template: "Field '{}' appears {} time(s), expected at most {}",
};

/// Undefined enum value.
pub const CW205_UNDEFINED_ENUM: ErrorCode = ErrorCode {
    id: "CW205",
    severity: ErrorSeverity::Error,
    message_template: "Undefined enum value '{}' for enum {}",
};

/// Event may fire every tick (performance warning).
pub const CW300_EVENT_EVERY_TICK: ErrorCode = ErrorCode {
    id: "CW300",
    severity: ErrorSeverity::Warning,
    message_template: "Event is missing mean_time_to_happen, is_triggered_only, fire_only_once, or trigger={always=no}. Performance concern: event may fire every tick.",
};

/// Pre-trigger at wrong level.
pub const CW301_PRE_TRIGGER_LEVEL: ErrorCode = ErrorCode {
    id: "CW301",
    severity: ErrorSeverity::Warning,
    message_template: "Pre-trigger '{}' should be inside a 'trigger' block, not at event root",
};

/// Unknown scope reference.
pub const CW400_UNKNOWN_SCOPE: ErrorCode = ErrorCode {
    id: "CW400",
    severity: ErrorSeverity::Error,
    message_template: "Unknown scope reference: {}",
};

/// Type not found.
pub const CW500_TYPE_NOT_FOUND: ErrorCode = ErrorCode {
    id: "CW500",
    severity: ErrorSeverity::Error,
    message_template: "Type '{}' not found",
};

/// Duplicate type definition.
pub const CW501_DUPLICATE_TYPE: ErrorCode = ErrorCode {
    id: "CW501",
    severity: ErrorSeverity::Warning,
    message_template: "Type '{}' appears {} times in file (unique violation)",
};

/// Unused type definition.
pub const CW502_UNUSED_TYPE: ErrorCode = ErrorCode {
    id: "CW502",
    severity: ErrorSeverity::Warning,
    message_template: "Type '{}' is defined but never referenced",
};

/// Generate a hash from error code + location + parameters.
pub fn error_code_hash(code: &ErrorCode, file: &str, line: u32, params: &[impl AsRef<str>]) -> String {
    let sev = match code.severity {
        ErrorSeverity::Error => "error",
        ErrorSeverity::Warning => "warning",
        ErrorSeverity::Information => "information",
        ErrorSeverity::Hint => "hint",
    };
    let msg = code.format(params);
    format!("{}|{}|{}|{}", sev, file, line, msg)
}
