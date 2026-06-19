use criterion::{Criterion, black_box, criterion_group, criterion_main};
use cwtools_game::constants::Game;
use cwtools_game::scope_engine::{ScopeContext, ScopeId};

// Stellaris has a populated hardcoded link table (HOI4 links come from config),
// so it exercises the real resolve path. Root = Country (200), matching the
// existing unit tests. Mixed case + prev/dotted keys exercise #10 (lowercase
// alloc), #11 (pop_n), #12 (is_subscope_or_eq).
const KEYS: &[&str] = &[
    "owner",
    "Owner",
    "controller",
    "PREV",
    "prevprev",
    "root",
    "from",
    "fromfrom",
    "leader",
    "planet",
    "star",
    "fleet",
    "ship",
    "capital_scope",
    "owner.capital_scope",
    "system",
];

fn bench_change_scope(c: &mut Criterion) {
    let base = ScopeContext::new(Game::Stellaris, ScopeId(200));
    c.bench_function("change_scope/stellaris_mixed", |b| {
        b.iter(|| {
            let mut ctx = base.clone();
            for k in KEYS {
                black_box(ctx.change_scope(black_box(k)));
            }
        })
    });
}

criterion_group!(benches, bench_change_scope);
criterion_main!(benches);
