# Generated from FR-MIGRATR-024 rev 7. Do not hand-edit: change the requirement and regenerate.
Feature: up --smuggl and down --smuggl ship migrations to the place through the rewind point

  @criterion-01a1b3a8-88cf-7231-abe3-abcac5d9bfce @module-migratr-cli
  Scenario: up --smuggl ships a local migration
    Given migratr dev against a place file and one new local migration
    When the agent runs migratr up --smuggl
    Then the place holds the migration in its ledger
    And the place's fingerprint equals the local database's

  @criterion-01a1b3a8-88cf-7231-abe3-abd6ed21993e @module-migratr-cli
  Scenario: the place's changes since dev arrive and the working rows stay home
    Given migratr dev against a place file
    And the place then inserts one row, updates one and deletes one
    And the working database gains a row of its own
    When the agent runs migratr up --smuggl
    Then the local database holds the place's insert and update and not its deleted row
    And the place does not hold the working database's row

  @criterion-01a1b3a8-88cf-7231-abe3-abe80a075576 @module-migratr-cli
  Scenario: autoclean removes the rewind point and the working snapshot on success
    Given migratr dev against a place file and one new local migration
    When the agent runs migratr up --smuggl with autoclean left on
    Then the rewind-point snapshot, its ledger row and the working database's snapshot are gone
    And the output names each
    Given the same with autoclean false in migratr.jsonc
    When the agent runs migratr up --smuggl
    Then all three remain

  @criterion-01a1b3a8-88cf-7231-abe3-abf2241734af @module-migratr-cli
  Scenario: a failed ship rolls back, keeps everything, and can be tried again
    Given migratr dev against a place file and a new local migration that drops a column, so migratr snapshots before it runs
    And the place will not end with the expected fingerprint
    When the agent runs migratr up --smuggl
    Then the local database's hashes equal the rewind point's
    And its ledger holds the rewind-point and fingerprint rows
    And the errors are printed and the exit code is non-zero
    And the rewind point snapshot and the working database's snapshot remain
    When the place is fixed and the agent runs migratr up --smuggl again
    Then the migration reaches the place

  @criterion-01a1b3a8-88cf-7231-abe3-ac0e97bfae77 @module-migratr-cli
  Scenario: no rewind point, no ship
    Given a ledger with no rewind point
    When the agent runs migratr up --smuggl, and then migratr down --smuggl
    Then nothing has changed anywhere
    And each says to run migratr dev

  @criterion-01a1b3a8-88cf-7231-abe3-ac1222c7e13b @module-migratr-cli @network @credentialed
  Scenario: a rebuild of a referenced table reaches D1 with every row
    Given migratr dev against a d1:// place whose table parent is referenced by child
    And a local migration that rebuilds parent
    When the agent runs migratr up --smuggl
    Then parent and child are rebuilt on the place
    And every row of both is there

  @criterion-01a1b3a8-88cf-7231-abe3-ac26f865b721 @module-migratr-cli
  Scenario: --discover tells you whether it is safe to go up or down
    Given migratr dev against a place file and a local migration that drops table old holding 3 rows
    When the agent runs migratr up --smuggl --discover, and then migratr down --smuggl --discover
    Then each prints the place's --discover report and the migrations it would send with their statements
    And the up report names table old and 3 rows destroyed
    And nothing has changed anywhere
    And each exits 0, or 3 when the run would stop

  @criterion-01a1b8aa-b174-7640-b370-33765226b181 @module-migratr-cli
  Scenario: down --smuggl reverts the latest migration on the place
    Given migratr dev, a new local migration, and migratr up --smuggl
    And migratr dev again
    When the agent runs migratr down --smuggl
    Then the place's ledger no longer holds that migration
    And the place's fingerprint equals the local database's

  @criterion-01a1b8aa-b174-7640-b370-3382a3282b82 @module-migratr-cli
  Scenario: a shape changed outside migratr stops the ship
    Given migratr dev against a place file
    And a column added to the place outside migratr
    When the agent runs migratr up --smuggl, and then migratr down --smuggl
    Then nothing has changed anywhere
    And each says the place's shape changed outside migratr and names --discover
