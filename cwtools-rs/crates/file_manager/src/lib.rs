pub mod file_manager;

pub use file_manager::{
    classify_directory,
    classify_extension,
    compute_logical_path,
    parse_mod_descriptor,
    DirectoryType,
    FileError,
    FileKind,
    FileManager,
    FileManagerConfig,
    ModDescriptor,
    ParsedFile,
};
