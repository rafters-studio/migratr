# Generated from FR-MIGRATR-023 rev 6. Do not hand-edit: change the requirement and regenerate.
Feature: migratr dev gets the local database ready to work against the place

  @criterion-01a1b3a8-887e-74e3-b23b-9a144be8724b @module-migratr-cli
  Scenario: dev brings the place's rows except ignored tables
    Given a place file with rows in tables a and b, and b in smugglr's ignore list
    And a migratr.jsonc with smuggl naming that place
    When the agent runs migratr dev
    Then the local database holds a's rows and b's shape with no rows
    And the output ends with Dev ready

  @criterion-01a1b3a8-887e-74e3-b23b-9a29750b8c74 @module-migratr-cli
  Scenario: dev's copy fires no trigger and keeps triggers, indexes and views
    Given a place file with a trigger that writes a row to an audit table on insert, an index and a view
    When the agent runs migratr dev
    Then the local audit table holds exactly the place's audit rows
    And the local database holds the place's trigger, index and view

  @criterion-01a1b3a8-887e-74e3-b23b-9a3f4e87b214 @module-migratr-cli
  Scenario: dev keeps the old local database
    Given a local database that already holds rows
    When the agent runs migratr dev
    Then a snapshot of the old local database exists among migratr's snapshots
    And the output names it

  @criterion-01a1b3a8-887e-74e3-b23b-9a4d621653e4 @module-migratr-cli
  Scenario: dev records the rewind point in the ledger
    When the agent runs migratr dev against a place file
    Then the ledger holds a rewind-point JSON row naming a snapshot and the SHA-256 of its file
    And that file's SHA-256 is the one recorded
    And smugglr sync --dryrun between the snapshot and the local database moves nothing

  @criterion-01a1b3a8-887e-74e3-b23b-9a5023b3e936 @module-migratr-cli
  Scenario: dev adopts a place whose shape is what the migrations make, and stops on one that is not
    Given a place file with tables and no migratr ledger whose schema by structure is what the migrations make
    When the agent runs migratr dev
    Then the ledger records every migration and the place's fingerprint
    Given instead a place file whose table t lacks a CHECK the migrations make
    When the agent runs migratr dev
    Then the output names table t and the missing CHECK
    And neither the place nor the local database has changed

  @criterion-01a1b3a8-887e-74e3-b23b-9a67502f9626 @module-migratr-cli
  Scenario: dev needs smugglr with exec
    Given no smugglr on the path, or a smugglr whose exec read form is refused
    When the agent runs migratr dev
    Then nothing has changed
    And the output names smugglr 0.6.3 to install

  @criterion-01a1b8aa-b111-74c1-be0e-204e0f4da497 @module-migratr-cli
  Scenario: a second dev does not rediscover, and stops on a shape changed outside migratr
    Given migratr dev has run against a place file
    When the agent runs migratr dev again
    Then no adoption runs and the output ends with Dev ready
    Given a column added to the place outside migratr
    When the agent runs migratr dev
    Then nothing has changed on either side
    And the output says the place's shape changed outside migratr and names --discover

  @criterion-01a1b8aa-b111-74c1-be0e-2054b46cc56b @module-migratr-cli
  Scenario: dev --discover reports and changes nothing
    When the agent runs migratr dev --discover against a place file
    Then the --discover report of the local database and the place is printed
    And neither has changed

  @criterion-01a1b8aa-b111-74c1-be0e-20600eafac30 @module-migratr-cli
  Scenario: dev without a place names the setting
    Given a migratr.jsonc with no smuggl
    When the agent runs migratr dev
    Then nothing has changed
    And the output names the smuggl setting
