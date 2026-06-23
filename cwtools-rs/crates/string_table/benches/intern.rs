use criterion::{Criterion, criterion_group, criterion_main};
use cwtools_string_table::string_table::StringTable;
use std::hint::black_box;

// Distinct tokens — interning these into a FRESH table each iteration measures
// the cold-load path (#28/#76): write-lock + Arc::from per miss.
const TOKENS: &[&str] = &[
    "focus_tree",
    "id",
    "country",
    "factor",
    "focus",
    "x",
    "y",
    "cost",
    "text",
    "available",
    "has_country_flag",
    "completion_reward",
    "add_political_power",
    "hidden_effect",
    "set_country_flag",
    "ai_will_do",
    "prerequisite",
    "relative_position_id",
    "second_focus",
    "test_focus",
    "owner",
    "controller",
    "capital",
    "political_power",
    "stability",
    "war_support",
    "GER",
    "USA",
    "SOV",
    "ENG",
    "FRA",
    "ITA",
    "JAP",
    "add_stability",
    "add_war_support",
    "set_politics",
    "ruling_party",
    "fascism",
    "democratic",
    "communism",
    "neutrality",
    "create_country_leader",
    "set_party_name",
    "load_focus_tree",
    "mio:my_org",
    "var:my_var",
    "token:my_token",
    "event_target:my_target",
    "scope:root",
];

fn bench_intern(c: &mut Criterion) {
    c.bench_function("intern/cold_50_distinct", |b| {
        b.iter(|| {
            let table = StringTable::new();
            for t in TOKENS {
                black_box(table.intern(black_box(t)));
            }
        })
    });
}

criterion_group!(benches, bench_intern);
criterion_main!(benches);
