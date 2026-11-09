# Generated from FR-MIGRATR-002 rev 7. Do not hand-edit: change the requirement and regenerate.
Feature: new stops when safe is on and there is no rewind point

  @criterion-01a1b31d-2b72-7750-a527-073a2ea9b81d @module-migratr-cli
  Scenario: new stops when safe is on and there is no rewind point
    Given a migratr.jsonc with smuggl set to a local place file and safe left at its default
    And a ledger holding no rewind point
    When the agent runs migratr new add_title
    Then no migration file is written
    And the output says there is no rewind point and to run migratr dev

  @criterion-01a1b31d-2b72-7750-a527-07480aa9c8d2 @module-migratr-cli
  Scenario: new writes the file when safe is off, smuggl is unset, or a rewind point exists
    Given each of: a migratr.jsonc with safe false and smuggl set; a migratr.jsonc with no smuggl; a migratr.jsonc with smuggl set and a ledger holding a rewind point
    When the agent runs migratr new add_title in each
    Then each writes one migration file as new did before
