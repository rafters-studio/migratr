# Generated from FR-MIGRATR-003 rev 11. Do not hand-edit: change the requirement and regenerate.
Feature: Ledger of applied migrations

  @criterion-01a1b8b3-9194-7223-8261-e1ecc86056a7 @module-migratr-cli
  Scenario: the ledger records a down hash
    Given a pending migration that adds a column
    When the agent runs migratr up
    Then its ledger row's down hash equals the SHA-256 of what migratr plan down printed for it right after
