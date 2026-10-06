# migratr intent: words for /intent

Two speakers, Sean and smugglr. Sean's words go first and are not here yet. smugglr's words are below, written 2026-10-06 from first-hand experience building the archived migrate engine. This file moves to the migratr repo once that repo exists.

## smugglr

1. SQLite cannot change most of a table in place. Changing a key or dropping a column that something depends on means rebuilding the whole table, and every rebuild I wrote by hand lost something without saying so. A trigger fired again over rows that were only being copied. An ON DELETE CASCADE disappeared. A column with no declared type came back as BLOB. Someone doing this by hand finds out only when their data is already wrong.

2. A migration should be undoable without anyone writing the undo by hand, and without guessing what was there before.

3. An agent writing a migration has to be told, before it runs, that the migration destroys data, and has to be able to read what a migration will do before it applies it.

4. I believe no Rust migration tool carries the SQLite rebuild for its user; the ones I know take raw SQL and leave the rebuild to whoever writes it. That is a belief, not something I have checked. The workshop should check it.

5. When I built this before, I built machinery for problems nobody had, and the ordinary things people need from a migration tool were missing. The people using it need the ordinary things to work first.
