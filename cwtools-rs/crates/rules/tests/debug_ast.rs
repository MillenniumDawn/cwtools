use cwtools_parser::parser::parse_string;
use cwtools_string_table::string_table::StringTable;

#[test]
fn debug_simple_cwt() {
    let input = r#"
types = {
    type[ship_size] = {
        path = "game/common/ship_sizes"
        subtype[starbase] = {
            class = shipclass_starbase
        }
    }
}
"#;
    let table = StringTable::new();
    let parsed = parse_string(input, &table).unwrap();

    println!("\n=== ROOT CHILDREN: {} ===", parsed.root_children.len());
    for (i, child) in parsed.root_children.iter().enumerate() {
        match child {
            cwtools_parser::ast::Child::Leaf(idx) => {
                let leaf = &parsed.arena.leaves[*idx as usize];
                let key = table.get_string(leaf.key.normal).unwrap_or_default();
                println!("root[{}] Leaf key={}", i, key);
                if let cwtools_parser::ast::Value::Clause(children) = &leaf.value {
                    for (j, c) in children.iter().enumerate() {
                        match c {
                            cwtools_parser::ast::Child::Leaf(lidx) => {
                                let l = &parsed.arena.leaves[*lidx as usize];
                                let k = table.get_string(l.key.normal).unwrap_or_default();
                                println!("  clause_child[{}] Leaf key={}", j, k);
                                if let cwtools_parser::ast::Value::Clause(cc) = &l.value {
                                    for (m, cc2) in cc.iter().enumerate() {
                                        if let cwtools_parser::ast::Child::Leaf(l2idx) = cc2 {
                                            let l2 = &parsed.arena.leaves[*l2idx as usize];
                                            let k2 =
                                                table.get_string(l2.key.normal).unwrap_or_default();
                                            println!("    inner[{}] Leaf key={}", m, k2);
                                        }
                                    }
                                }
                            }
                            _ => println!("  clause_child[{}] other", j),
                        }
                    }
                }
            }
            _ => println!("root[{}] other", i),
        }
    }
}
