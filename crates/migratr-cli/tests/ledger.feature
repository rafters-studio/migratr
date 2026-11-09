# Generated from FR-MIGRATR-003 rev 12. Do not hand-edit: change the requirement and regenerate.
Feature: Ledger of applied migrations

  @criterion-01a1b8aa-b0a5-7423-9ad4-7ac657902dec @module-migratr-cli
  Scenario: dev's ledger rows are listed by kind
    Given a local database after migratr dev against a local place file
    When the agent runs migratr status
    Then the ledger holds one rewind-point row and one fingerprint row, each holding JSON
    And status lists each by its kind
    And status reports no missing file
    And migratr up applies a pending migration and migratr down reverts it, neither refusing on those rows

  @criterion-01a1b8b3-9194-7223-8261-e1ecc86056a7 @module-migratr-cli
  Scenario: the ledger records a down hash
    Given a pending migration that adds a column
    When the agent runs migratr up
    Then its ledger row's down hash equals the SHA-256 of what migratr plan down printed for it right after
