pub mod file_manager;

pub use file_manager::{
    classify_directory,
    classify_extension,
    compute_logical_path,
    discover_files_multi_mod,
    expand_multiple_mods,
    parse_mod_descriptor,
    read_text,
    DirectoryType,
    FileError,
    FileKind,
    FileManager,
    FileManagerConfig,
    ModDescriptor,
    ParsedFile,
    ResolvedMod,
};
