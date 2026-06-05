#[cfg(test)]
mod debug_tests {
    use cwtools_parser::parser::parse_string;
    use cwtools_string_table::string_table::StringTable;

    #[test]
    fn debug_parse_angles() {
        let table = StringTable::new();
        let parsed = parse_string("ethos = \u003cethos\u003e", &table).unwrap();
        println!("root children: {}", parsed.root_children.len());
        for child in &parsed.root_children {
            println!("child: {:?}", child);
        }
    }
}
