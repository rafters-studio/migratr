# migratr

## Acceptance scenarios

- Acceptance criteria are Gherkin scenarios, one per criterion, tagged `@criterion-<id>`. The runner is rstest-bdd (`rstest-bdd` and `rstest-bdd-macros` 0.6, workspace dev-dependencies).
- Scenarios are generated from the legion requirement and are never hand-edited. Every `.feature` file starts with a comment saying so. To change one, change the requirement and regenerate.
- Scenarios live in a crate's `tests/`, mirroring `src/`: the scenario for behaviour whose entry point is `src/a/b.rs` lives at `tests/a/b.feature`, beside any `tests/a/b.rs`.
- Step code lives in the crate's `tests/bdd.rs`, which ends with `scenarios!("tests", fixtures = [world: World])` and so runs every `.feature` under that crate's `tests/`. A crate that gets its first `.feature` needs its own `tests/bdd.rs` and the three rstest dev-dependencies (`rstest`, `rstest-bdd`, `rstest-bdd-macros`, all `workspace = true`).
- `cargo test --workspace` runs them, in CI too. A scenario with no matching step code fails the test and the failure names the scenario. Cargo does not watch `.feature` files: after adding one, touch `tests/bdd.rs` to rebuild.
- `@smoke` scenarios (`crates/migratr-cli/tests/main.feature`) prove the wiring against documented behaviour.
