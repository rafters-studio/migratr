# Generated from FR-MIGRATR-025 rev 4. Do not hand-edit: change the requirement and regenerate.
Feature: migratr.jsonc holds the place and how loud, safe and tidy a run is

  @criterion-01a1b8aa-b1ea-7a10-abfa-a2da7344b81d @module-migratr-cli
  Scenario: settings are found walking up, or named by --config
    Given a migratr.jsonc with a comment and loud set to false, two directories above the working directory
    When the agent runs migratr status
    Then the output holds only the result
    Given another migratr.jsonc with loud left on, named by --config
    When the agent runs migratr status --config <that file>
    Then the output names that file as the settings used, and loud as on, set by the default

  @criterion-01a1b8aa-b1ea-7a10-abfa-a2e53e16a495 @module-migratr-cli
  Scenario: the defaults
    Given no migratr.jsonc anywhere above the working directory
    When the agent runs migratr status
    Then the output says no settings file was found
    And it names loud, safe and autoclean as on and smuggl as unset, each set by the default

  @criterion-01a1b8aa-b1ea-7a10-abfa-a2f4b6e04948 @module-migratr-cli
  Scenario: loud and quiet
    Given one pending migration and no migratr.jsonc
    When the agent runs migratr up
    Then the output says no settings file was found, names each key with its value set by the default, and names each migration as it starts and each statement it runs
    And it ends with a note that loud can be turned off in migratr.jsonc
    Given the same with loud false in migratr.jsonc
    When the agent runs migratr up
    Then the output holds only the result and any errors

  @criterion-01a1b8aa-b1ea-7a10-abfa-a30334f232e0 @module-migratr-cli
  Scenario: an unknown key is refused
    Given a migratr.jsonc holding a key token
    When the agent runs migratr up
    Then the output names the key token
    And the exit code is 2
    And nothing has changed
