# Generated from the legion requirement. Do not hand-edit: change the requirement and regenerate.
Feature: restore requires confirmation

  @smoke @criterion-smoke
  Scenario: restore without --yes changes nothing
    Given a project with two applied migrations
    When I run "restore" without --yes
    Then the process exits 1 with error code "restore_not_confirmed"
