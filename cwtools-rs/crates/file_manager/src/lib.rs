pub mod file_manager;

pub use file_manager::{
    DirectoryType, FileEncoding, FileError, FileKind, FileManager, FileManagerConfig,
    ModDescriptor, ParsedFile, ResolvedMod, classify_directory, classify_extension,
    compute_logical_path, discover_files_multi_mod, expand_multiple_mods, parse_mod_descriptor,
    read_text, read_text_with_encoding,
};
