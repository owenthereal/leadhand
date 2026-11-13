# Contract catalogue

Each Python test in `tests/test_*.py`, as `file::Class::test`, mapped to one of:

- the contract test that converts it, as its unittest ID (`contract_<x>.<Class>.<test>`);
- `internal: Rust unit test (<module>)`;
- `dropped with in-place mode`;
- `plugin file check (contract_plugin_files)`.

| Python test | Contract test or disposition |
|---|---|
| test_agents.py::AgentTests::test_agent_frontmatter_matches_table | plugin file check (contract_plugin_files) |
| test_agents.py::AgentTests::test_codex_proxy_runs_one_command | plugin file check (contract_plugin_files) |
| test_agents.py::AgentTests::test_contract_carriers_contain_contract_verbatim | plugin file check (contract_plugin_files) |
| test_agents.py::AgentTests::test_reviewer_rules | plugin file check (contract_plugin_files) |
| test_agents.py::AgentTests::test_verifier_evidence_rule_follows_the_spec | plugin file check (contract_plugin_files) |
| test_cli.py::CliTests::test_unknown_command_exits_2 | contract_cli.Usage.test_unknown_command_exits_2 |
| test_cli.py::CliTests::test_version_prints_package_version | contract_cli.Version.test_prints_package_version |
| test_supervisor_files.py::SupervisorFileTests::test_run_skill_commands_match_cli | contract_cli.PluginCommands.test_every_cli_command_in_the_run_skill_is_a_registered_command |
| test_supervisor_files.py::SupervisorFileTests::test_run_skill_covers_every_action | plugin file check (contract_plugin_files) |
| test_supervisor_files.py::SupervisorFileTests::test_run_skill_covers_run_level_dispatches | plugin file check (contract_plugin_files) |
| test_supervisor_files.py::SupervisorFileTests::test_run_skill_records_started_before_waiting | plugin file check (contract_plugin_files) |
| test_supervisor_files.py::SupervisorFileTests::test_supervisor_frontmatter | plugin file check (contract_plugin_files) |
| test_deck.py::DeckTests::test_parse_basic_fixture | contract_deck.Briefs.test_basic_fixture_cards_reach_the_brief |
| test_deck.py::DeckTests::test_spec_example_header_parses | internal: Rust unit test (deck) |
| test_deck.py::DeckTests::test_hard_yes_requires_reason | contract_deck.Deck.test_hard_yes_requires_reason |
| test_deck.py::DeckTests::test_hard_yes_with_reason_forms | contract_deck.Deck.test_hard_yes_with_reason_forms; contract_deck.Briefs.test_hard_yes_reason_is_rendered |
| test_deck.py::DeckTests::test_fields_after_context_are_parsed | contract_deck.Deck.test_fields_after_context_are_parsed; contract_deck.Briefs.test_fields_after_context_are_parsed |
| test_deck.py::DeckTests::test_unknown_key_bullet_is_context | contract_deck.Briefs.test_unknown_key_bullet_is_context |
| test_deck.py::DeckTests::test_unknown_key_bullet_warns | contract_deck.Deck.test_unknown_key_bullet_warns |
| test_deck.py::DeckTests::test_scout_card_rejected_in_this_version | contract_deck.Deck.test_scout_card_rejected_in_this_version |
| test_deck.py::DeckTests::test_unsupported_config_reported_as_errors | contract_deck.Deck.test_unsupported_config_reported_as_errors |
| test_deck.py::DeckTests::test_dependency_cycle_is_error | contract_deck.Deck.test_dependency_cycle_is_error |
| test_deck.py::DeckTests::test_unknown_dependency_and_duplicates_and_finish_and_build | contract_deck.Deck.test_unknown_dependency_and_duplicates_and_finish_and_build |
| test_deck.py::DeckTests::test_requirement_to_missing_card_is_error | contract_deck.Deck.test_requirement_to_missing_card_is_error |
| test_deck.py::DeckTests::test_missing_required_fields_are_errors | contract_deck.Deck.test_missing_required_fields_are_errors |
| test_deck.py::DeckTests::test_dependency_order_is_stable_topological | internal: Rust unit test (deck) |
| test_deck.py::DeckTests::test_route_pin_parsed | contract_deck.Deck.test_route_pin_parsed |
| test_deck.py::DeckTests::test_digest_changes_when_file_changes | internal: Rust unit test (deck) |
| test_deck.py::DeckTests::test_parse_error_reports_line_number | contract_deck.Deck.test_parse_error_reports_line_number |
| test_deck.py::DeckTests::test_uncovered_requirement_is_warning | contract_deck.Deck.test_uncovered_requirement_is_warning |
| test_deck.py::DeckTests::test_base_commit_and_paths | contract_deck.Deck.test_base_commit_and_paths |
| test_deck.py::DeckTests::test_check_cli_exits_1_on_error | contract_deck.Deck.test_check_cli_exits_1_on_error |
| test_config.py::ConfigTests::test_default_preset_matches_spec | contract_config.Config.test_default_preset_matches_spec |
| test_config.py::ConfigTests::test_repo_file_overrides_user_file_key_by_key | contract_config.Config.test_repo_file_overrides_user_file_key_by_key |
| test_config.py::ConfigTests::test_preset_key_selects_claude_only | contract_config.Config.test_preset_key_selects_claude_only |
| test_config.py::ConfigTests::test_invalid_effort_raises_config_error_naming_key | contract_config.Config.test_invalid_effort_raises_config_error_naming_key |
| test_config.py::ConfigTests::test_unknown_top_level_key_raises | contract_config.Config.test_unknown_top_level_key_raises |
| test_config.py::ConfigTests::test_docs_dir_expands_repo_and_home | contract_config.Config.test_docs_dir_expands_repo_and_home |
| test_config.py::ConfigTests::test_after_run_step_validation | contract_config.Config.test_after_run_step_validation |
| test_config.py::ConfigTests::test_unsupported_settings_lists_each_rejection | contract_config.Config.test_unsupported_settings_lists_each_rejection |
| test_config.py::ConfigTests::test_step_limits_claude_workers_cannot_honour_are_rejected | contract_config.Config.test_step_limits_claude_workers_cannot_honour_are_rejected |
| test_config.py::ConfigTests::test_step_limits_follow_the_roles_routes_and_agent_files | internal: Rust unit test (config) |
| test_config.py::ConfigTests::test_parse_route_and_leadhand_config_env | contract_config.Config.test_leadhand_config_env_names_the_user_file |
| test_config_snapshot.py::Snapshot::test_snapshot_round_trips | internal: Rust unit test (config) |
| test_config_snapshot.py::Snapshot::test_start_writes_the_snapshot_and_the_engine_reads_only_it | contract_config.Snapshot.test_start_writes_the_snapshot_and_the_engine_reads_only_it |
| test_config_snapshot.py::Snapshot::test_worker_editing_the_repo_config_changes_nothing | contract_config.Snapshot.test_worker_editing_the_repo_config_changes_nothing; contract_layouts.InPlace.test_python_accepts_run_worktree_false |
| test_route.py::RouteTests::test_standard_implementer_uses_codex_terra_high | contract_route.Route.test_standard_implementer_uses_codex_terra_high |
| test_route.py::RouteTests::test_hard_card_uses_implementer_hard | contract_route.Route.test_hard_card_uses_implementer_hard |
| test_route.py::RouteTests::test_reviewer_other_vendor_than_implementer | contract_route.Route.test_reviewer_other_vendor_than_implementer |
| test_route.py::RouteTests::test_high_risk_reviewer_uses_reviewer_hard_plus_one | contract_route.Route.test_high_risk_reviewer_uses_reviewer_hard_plus_one |
| test_route.py::RouteTests::test_security_uses_reviewer_hard | contract_route.SecurityGate.test_security_uses_reviewer_hard |
| test_route.py::RouteTests::test_profile_lean_lowers_every_role | contract_route.Route.test_profile_lean_lowers_every_role |
| test_route.py::RouteTests::test_pin_wins_for_first_two_attempts | contract_route.Route.test_pin_wins_for_first_two_attempts |
| test_route.py::RouteTests::test_ladder_attempts_1_to_4_then_exhausted | contract_route.Route.test_ladder_attempts_1_to_4_then_exhausted |
| test_route.py::RouteTests::test_attempt_4_with_prefer_other_switches_from_attempt_3s_actual_vendor | contract_route.Route.test_attempt_4_with_prefer_other_switches_from_attempt_3s_actual_vendor |
| test_route.py::RouteTests::test_claude_only_install_exhausts_at_attempt_4 | contract_route.Route.test_claude_only_install_exhausts_at_attempt_4 |
| test_route.py::RouteTests::test_effort_clamped_to_xhigh_unless_pinned_max | contract_route.Route.test_effort_clamped_to_xhigh_unless_pinned_max |
| test_route.py::RouteTests::test_pin_requires_installed_vendor | contract_route.Route.test_pin_requires_installed_vendor |
| test_route.py::RouteTests::test_invalid_profile_is_config_error | contract_route.Route.test_invalid_profile_is_config_error |
| test_route.py::RouteTests::test_low_pin_not_shifted_by_profile_or_risk | contract_route.Route.test_low_pin_not_shifted_by_profile_or_risk |
| test_route.py::RouteTests::test_cli_route_prints_route_and_reasons | contract_route.Route.test_cli_route_prints_route_and_reasons |
| test_unsupported_layouts.py::Unsupported::test_docs_dir_inside_the_repo_is_refused | contract_layouts.Unsupported.test_docs_dir_inside_the_repo_is_refused |
| test_unsupported_layouts.py::Unsupported::test_relative_docs_dir_is_resolved_against_the_repo | contract_layouts.Unsupported.test_relative_docs_dir_is_resolved_against_the_repo |
| test_unsupported_layouts.py::Unsupported::test_defaults_are_supported | contract_layouts.Unsupported.test_defaults_are_supported |
| test_status.py::StatusTests::test_status_headline_while_implementing | contract_status.Status.test_status_headline_while_implementing; contract_status.Checkpoints.test_checkpoint_writes_status_json |
| test_status.py::StatusTests::test_status_headline_while_reviewing | contract_status.Status.test_status_headline_while_reviewing |
| test_status.py::StatusTests::test_status_attention_lists_parked_cards_and_decisions | contract_status.Status.test_status_attention_lists_parked_cards_and_decisions |
| test_status.py::StatusTests::test_ended_run_states | contract_status.Status.test_ended_run_states |
| test_status.py::StatusTests::test_status_cli_without_status_json | contract_status.Status.test_status_cli_without_status_json |
| test_status.py::StatusTests::test_status_prefers_status_json_when_present | contract_status.Checkpoints.test_checkpoint_writes_status_json |
| test_status.py::StatusTests::test_corrupt_event_line_is_an_attention_line_not_a_failure | contract_status.Status.test_corrupt_event_line_is_an_attention_line_not_a_failure |
| test_status.py::StatusTests::test_partial_last_line_is_ignored | contract_status.Status.test_partial_last_line_is_ignored |
| test_status.py::StatusTests::test_usage_comes_from_charges_and_excludes_nothing_by_role | contract_status.Status.test_usage_comes_from_charges_and_excludes_nothing_by_role |
| test_status.py::StatusTests::test_codex_meter_reads_latest_record | contract_status.Status.test_codex_meter_reads_latest_record |
| test_status.py::StatusTests::test_no_run_dir_picks_latest_unfinished_run | contract_status.StatusWithoutRunDir.test_no_run_dir_picks_latest_unfinished_run |
| test_status.py::FixRoundTests::test_attention_lists_violations_with_preserved_ref_and_next_action | contract_status.FixRound.test_attention_lists_violations_with_preserved_ref_and_next_action |
| test_status.py::FixRoundTests::test_meter_before_is_taken_once_even_when_null | contract_status.Checkpoints.test_meter_before_is_taken_once_even_when_null |
| test_status.py::FixRoundTests::test_meter_is_not_scanned_at_every_checkpoint | contract_status.Checkpoints.test_meter_is_not_scanned_at_every_checkpoint |
| test_status.py::FixRoundTests::test_meter_scan_is_capped_to_newest_files | contract_status.FixRound.test_meter_scan_is_capped_to_newest_files |
| test_status.py::FixRoundTests::test_unreadable_log_is_an_attention_line | contract_status.FixRound.test_unreadable_log_is_an_attention_line |
| test_status.py::EngineStatusTests::test_engine_checkpoints_write_status_json_and_survive_status_errors | contract_status.Checkpoints.test_engine_checkpoints_write_status_json_and_survive_status_errors |
| test_report.py::ReportTests::test_report_outcome_is_line_3_after_title | contract_report.Report.test_report_outcome_is_line_3_after_title |
| test_report.py::ReportTests::test_outcome_qualifier | contract_report.Report.test_outcome_qualifier |
| test_report.py::ReportTests::test_report_sections_in_order | contract_report.Report.test_report_sections_in_order |
| test_report.py::ReportTests::test_incomplete_report_puts_parked_cards_under_attention | contract_report.Report.test_incomplete_report_puts_parked_cards_under_attention |
| test_report.py::ReportTests::test_report_usage_includes_supervisor_and_proxy_and_unknown_count | contract_report.Report.test_report_usage_includes_supervisor_and_proxy_and_unknown_count |
| test_report.py::ReportTests::test_routing_counts_worker_attempts_not_proxy | contract_report.Report.test_routing_counts_worker_attempts_not_proxy |
| test_report.py::ReportTests::test_write_report_and_cli | contract_report.Report.test_write_report_and_cli |
| test_report.py::FixRoundTests::test_unresolved_lists_parked_cards_with_branches_and_open_findings | contract_report.FixRound.test_unresolved_lists_parked_cards_with_branches_and_open_findings |
| test_report.py::FixRoundTests::test_attention_has_next_action_for_parked_cards | contract_report.FixRound.test_attention_has_next_action_for_parked_cards |
| test_report.py::FixRoundTests::test_evidence_shows_last_result_per_gate_and_marks_stale | contract_report.FixRound.test_evidence_shows_last_result_per_gate_and_marks_stale |
| test_report.py::FixRoundTests::test_report_is_written_atomically_without_leftover_temp_files | contract_report.FixRound.test_report_is_written_atomically_without_leftover_temp_files |
| test_report.py::FixRoundTests::test_world_is_hermetic_about_codex_home | contract_report.FixRound.test_world_is_hermetic_about_codex_home |
