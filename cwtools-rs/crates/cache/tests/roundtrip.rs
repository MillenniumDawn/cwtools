use cwtools_cache::{convert, io};
use cwtools_file_manager::file_manager::{FileManager, FileManagerConfig};
use cwtools_parser::parser::parse_string;
use cwtools_string_table::string_table::StringTable;

#[test]
fn roundtrip_simple_file() {
    let input = r#"
# This is a comment
foo = bar
nested = {
    a = 1
    b = "hello"
    c = yes
}
"#;
    let table = StringTable::new();
    let parsed = parse_string(input, &table).unwrap();
    let cached = convert::arena_to_cached(&parsed.arena, &parsed.root_children, &table,
    );

    // Serialize to temp file
    let tmp = tempfile::NamedTempFile::with_suffix(".cwb").unwrap();
    io::serialize_to_file(&cached, tmp.path()).unwrap();

    // Deserialize
    let loaded = io::deserialize_from_file(tmp.path()).unwrap();

    // Convert back to arena
    let table2 = StringTable::new();
    let (arena2, root2) = convert::cached_to_arena(&loaded, &table2);

    // Verify structure counts match
    assert_eq!(arena2.nodes.len(), parsed.arena.nodes.len());
    assert_eq!(arena2.leaves.len(), parsed.arena.leaves.len());
    assert_eq!(arena2.leaf_values.len(), parsed.arena.leaf_values.len());
    assert_eq!(arena2.value_clauses.len(), parsed.arena.value_clauses.len());
    assert_eq!(arena2.comments.len(), parsed.arena.comments.len());
    assert_eq!(root2.len(), parsed.root_children.len());
}

#[test]
fn roundtrip_real_file() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../artifacts/bin/CWToolsTests/debug/testfiles/performancetest2/common/static_modifiers/cc_colony_events_static_modifiers.txt"
    );
    let input = std::fs::read_to_string(path).unwrap();
    let table = StringTable::new();
    let parsed = parse_string(&input, &table).unwrap();
    let cached = convert::arena_to_cached(
        &parsed.arena, &parsed.root_children, &table,
    );

    let tmp = tempfile::NamedTempFile::with_suffix(".cwb").unwrap();
    io::serialize_to_file(&cached, tmp.path()).unwrap();

    let loaded = io::deserialize_from_file(tmp.path()).unwrap();
    let table2 = StringTable::new();
    let (arena2, root2) = convert::cached_to_arena(&loaded, &table2);

    assert_eq!(arena2.nodes.len(), parsed.arena.nodes.len());
    assert_eq!(arena2.leaves.len(), parsed.arena.leaves.len());
    assert_eq!(root2.len(), parsed.root_children.len());

    // Verify some basic leaf content
    // First root child should be a leaf in this file
    if let cwtools_parser::ast::Child::Leaf(idx) = &root2[0] {
        let leaf = &arena2.leaves[*idx as usize];
        let key_str = table2.get_string(leaf.key.normal).unwrap();
        assert!(!key_str.is_empty());
    }
}

/// Gate: Cache round-trip verified for all 63 test files.
#[test]
fn roundtrip_all_performancetest_files() {
    let test_dir = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../artifacts/bin/CWToolsTests/debug/testfiles/performancetest2/"
    );

    let config = FileManagerConfig {
        root: std::path::PathBuf::from(test_dir),
        ..Default::default()
    };
    let mut manager = FileManager::new(config);
    let files = manager.discover_and_parse().unwrap();

    assert!(!files.is_empty(), "No files discovered in {}", test_dir);

    let mut total_files = 0;
    let mut total_leaves = 0;

    for parsed in files {
        let cached = convert::arena_to_cached(
            &parsed.arena, &parsed.root_children, &manager.string_table,
        );

        let tmp = tempfile::NamedTempFile::with_suffix(".cwb").unwrap();
        io::serialize_to_file(&cached, tmp.path()).unwrap();

        let loaded = io::deserialize_from_file(tmp.path()).unwrap();
        let table2 = StringTable::new();
        let (arena2, root2) = convert::cached_to_arena(&loaded, &table2);

        // Verify counts match
        assert_eq!(
            arena2.nodes.len(), parsed.arena.nodes.len(),
            "Node count mismatch for {}", parsed.path.display()
        );
        assert_eq!(
            arena2.leaves.len(), parsed.arena.leaves.len(),
            "Leaf count mismatch for {}", parsed.path.display()
        );
        assert_eq!(
            arena2.leaf_values.len(), parsed.arena.leaf_values.len(),
            "LeafValue count mismatch for {}", parsed.path.display()
        );
        assert_eq!(
            arena2.value_clauses.len(), parsed.arena.value_clauses.len(),
            "ValueClause count mismatch for {}", parsed.path.display()
        );
        assert_eq!(
            arena2.comments.len(), parsed.arena.comments.len(),
            "Comment count mismatch for {}", parsed.path.display()
        );
        assert_eq!(
            root2.len(), parsed.root_children.len(),
            "Root children count mismatch for {}", parsed.path.display()
        );

        total_files += 1;
        total_leaves += parsed.arena.leaves.len();
    }

    println!("Round-trip verified for {} files, {} total leaves", total_files, total_leaves);
    assert!(total_files >= 60, "Expected at least 60 files, got {}", total_files);
}
