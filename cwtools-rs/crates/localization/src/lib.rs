pub mod commands;
pub mod csv_parser;
pub mod loc_index;
pub mod loc_string;
pub mod pipeline;
pub mod scope_validation;
pub mod service;
pub mod validation;
pub mod yaml_parser;

pub use commands::*;
pub use csv_parser::*;
// loc_string defines JominiCommand/JominiParam; commands re-defines them
// (legacy parallel types).  Export loc_string variants under explicit names
// to avoid ambiguous glob re-exports.
pub use loc_index::LocIndex;
pub use loc_string::{
    JominiCommand as LocJominiCommand, JominiParam as LocJominiParam, LocElement,
    parse_loc_elements,
};
pub use pipeline::{
    LocDiagnostic, LocSeverity, loc_error_code, loc_error_severity, validate_loc_file_text,
    validate_loc_project, validate_loc_project_scoped,
};
pub use scope_validation::{LocCommandDiagnostic, LocScopeData, validate_loc_commands};
pub use service::*;
pub use validation::{
    HARDCODED_LOC, LocErrorKind, LocValidationError, build_key_union, validate_invalid_chars,
    validate_loc_file,
};
pub use yaml_parser::{
    LangHeaderDiagnostic, MissingBomDiagnostic, check_loc_file_lang, check_utf8_bom,
    find_invalid_loc_char, is_loc_value_char, lang_from_filename, parse_loc_text,
};
