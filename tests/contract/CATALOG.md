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
| test_dispatch.py::ArgvTests::test_argv_passes_network_false_explicitly | contract_dispatch.CodexCommandLine.test_argv_passes_network_false_explicitly |
| test_dispatch.py::ArgvTests::test_argv_network_true_when_enabled | contract_dispatch.CodexCommandLine.test_argv_network_true_when_enabled |
| test_dispatch.py::ArgvTests::test_argv_workspace_write_with_git_common_dir_and_effort | contract_dispatch.CodexCommandLine.test_argv_workspace_write_with_git_common_dir_and_effort |
| test_dispatch.py::ArgvTests::test_argv_scout_is_read_only | contract_dispatch.CodexCommandLine.test_argv_scout_is_read_only |
| test_dispatch.py::ArgvTests::test_argv_sets_blocking_env_through_shell_policy | contract_dispatch.CodexCommandLine.test_argv_sets_blocking_env_through_shell_policy |
| test_dispatch.py::ArgvTests::test_evidence_worker_may_write_the_evidence_base_worktree_only | contract_dispatch.CodexCommandLine.test_evidence_worker_may_write_the_evidence_base_worktree_only |
| test_dispatch.py::ArgvTests::test_evidence_base_worktree_must_be_registered_and_reached_through_no_symlink | contract_dispatch.CodexCommandLine.test_evidence_base_worktree_must_be_registered_and_reached_through_no_symlink |
| test_dispatch.py::ArgvTests::test_resume_argv_matches_findings | contract_dispatch.CodexCommandLine.test_resume_argv_matches_findings |
| test_dispatch.py::ResultTests::test_ready_reply_parsed_and_attempt_recorded | contract_dispatch.Results.test_ready_reply_parsed_and_attempt_recorded |
| test_dispatch.py::ResultTests::test_reply_without_status_is_blocked_and_raw_kept | contract_dispatch.Results.test_reply_without_status_is_blocked_and_raw_kept |
| test_dispatch.py::ResultTests::test_step_limit_stops_process_and_marks_spent | contract_dispatch.Results.test_step_limit_stops_process_and_marks_spent |
| test_dispatch.py::ResultTests::test_time_limit_stops_process_and_marks_spent | contract_dispatch.Results.test_time_limit_stops_process_and_marks_spent |
| test_dispatch.py::ResultTests::test_usage_limit_is_blocked_by_usage_limit | contract_dispatch.Results.test_usage_limit_is_blocked_by_usage_limit |
| test_dispatch.py::ResultTests::test_killed_worker_usage_from_session_or_unknown | contract_dispatch.Usage.test_killed_worker_usage_from_session_or_unknown |
| test_dispatch.py::ResultTests::test_missing_codex_binary_is_blocked_environment | contract_dispatch.Results.test_missing_codex_binary_is_blocked_environment |
| test_dispatch.py::ResultTests::test_paths_with_spaces | contract_dispatch.Results.test_paths_with_spaces |
| test_dispatch.py::OwnershipTests::test_runner_survives_foreground_death_and_records_result | contract_dispatch.RunnerLifecycle.test_runner_survives_foreground_death_and_records_result |
| test_dispatch.py::OwnershipTests::test_second_dispatch_attaches_to_live_runner | contract_dispatch.RunnerLifecycle.test_second_dispatch_attaches_to_live_runner |
| test_dispatch.py::OwnershipTests::test_repeated_dispatch_returns_existing_result | contract_dispatch.RunnerLifecycle.test_repeated_dispatch_returns_existing_result |
| test_dispatch.py::OwnershipTests::test_cli_prints_reply_and_session_or_still_running | contract_dispatch.RunnerLifecycle.test_cli_prints_reply_and_session_or_still_running; contract_dispatch.RunnerLifecycle.test_foreground_wait_ends_with_exit_75_and_the_still_running_line |
| test_dispatch.py::CancelTests::test_signal_only_after_pid_and_start_time_match | contract_dispatch.Cancellation.test_signal_only_after_pid_and_start_time_match |
| test_dispatch.py::CancelTests::test_cancel_detached_worker_stops_every_process_and_writes_nothing_after | contract_dispatch.Cancellation.test_cancel_detached_worker_stops_every_process_and_writes_nothing_after |
| test_dispatch.py::CancelTests::test_stale_heartbeat_on_live_runner_is_cancelled_not_replaced_alongside | contract_dispatch.Cancellation.test_stale_heartbeat_on_live_runner_is_cancelled_not_replaced_alongside |
| test_dispatch.py::CancelTests::test_orphaned_codex_group_is_terminated_before_finalizing | contract_dispatch.ProcessControl.test_orphaned_codex_group_is_terminated_before_finalizing |
| test_dispatch.py::EscapedDescendantTests::test_double_forked_child_is_stopped_before_finalizing | contract_dispatch.ProcessControl.test_double_forked_child_is_stopped_before_finalizing |
| test_dispatch.py::FixRoundTests::test_run_id_passed_through_shell_policy | contract_dispatch.CodexCommandLine.test_run_id_passed_through_shell_policy |
| test_dispatch.py::FixRoundTests::test_stream_usage_is_charged_when_session_missing | contract_dispatch.Usage.test_stream_usage_is_charged_when_session_missing |
| test_dispatch.py::FixRoundTests::test_usage_limit_text_does_not_override_clean_ready_reply | contract_dispatch.Results.test_usage_limit_text_does_not_override_clean_ready_reply |
| test_dispatch.py::FixRoundTests::test_runner_exception_is_blocked_environment | internal: Rust unit test (dispatch) |
| test_dispatch.py::FixRoundTests::test_unknown_codex_start_time_still_enforced | internal: Rust unit test (dispatch) |
| test_dispatch.py::FixRoundTests::test_runner_record_without_start_time_is_not_dead_while_lock_held | contract_dispatch.RunnerLifecycle.test_runner_record_without_start_time_is_not_dead_while_lock_held |
| test_dispatch.py::FixRoundTests::test_launch_intent_recorded_before_codex_starts | contract_dispatch.RunnerLifecycle.test_launch_intent_recorded_before_codex_starts |
| test_dispatch.py::FixRoundTests::test_launch_rechecks_cancel_immediately_before_starting | internal: Rust unit test (dispatch) |
| test_dispatch.py::FixRoundTests::test_cancel_requested_during_orphan_is_not_resurrected | contract_dispatch.Cancellation.test_cancel_requested_during_orphan_is_not_resurrected |
| test_dispatch.py::FixRoundTests::test_ready_result_landing_during_replacement_cancel_is_kept | contract_dispatch.RunnerLifecycle.test_ready_result_landing_during_replacement_cancel_is_kept |
| test_dispatch.py::FixRoundTests::test_heartbeat_refreshed_before_second_look_is_not_hung | contract_dispatch.RunnerLifecycle.test_heartbeat_refreshed_before_second_look_is_not_hung |
| test_dispatch.py::FixRoundTests::test_concurrent_dispatches_start_one_runner | contract_dispatch.RunnerLifecycle.test_concurrent_dispatches_start_one_runner |
| test_dispatch.py::FixRoundTests::test_second_runner_refuses_while_first_holds_lock | contract_dispatch.RunnerLifecycle.test_second_runner_refuses_while_first_holds_lock |
| test_dispatch.py::RunnerCommitTests::test_runner_commits_codex_work_on_card_branch | contract_dispatch.RunnerCommits.test_runner_commits_codex_work_on_card_branch |
| test_dispatch.py::RunnerCommitTests::test_runner_commits_partial_work_when_spent | contract_dispatch.RunnerCommits.test_runner_commits_partial_work_when_spent |
| test_dispatch.py::RunnerCommitTests::test_commit_intent_restart_does_not_double_commit | contract_dispatch.RunnerCommits.test_commit_intent_restart_after_the_commit_does_not_commit_again; contract_dispatch.RunnerCommits.test_commit_intent_restart_before_the_commit_still_commits |
| test_dispatch.py::RunnerCommitTests::test_runner_commits_a_new_file_in_a_subdirectory_workspace | dropped with in-place mode |
| test_dispatch.py::RunnerCommitTests::test_untracked_to_take_fences_the_run_folder_from_the_checkout_top | dropped with in-place mode |
| test_dispatch.py::RunnerCommitTests::test_untracked_to_take_keeps_only_paths_inside_a_subdirectory_workspace | dropped with in-place mode |
| test_dispatch.py::RunnerCommitTests::test_untracked_to_take_fails_closed_when_the_checkout_top_is_unknown | internal: Rust unit test (dispatch) |
| test_dispatch.py::RunnerCommitTests::test_runner_commit_leaves_untracked_files_outside_a_subdirectory_workspace | dropped with in-place mode |
| test_dispatch.py::RunnerCommitTests::test_a_fifo_left_at_the_report_never_hangs_the_commit | contract_dispatch.RunnerCommits.test_a_fifo_left_at_the_report_never_hangs_the_commit |
| test_dispatch.py::RunnerCommitTests::test_runner_commits_an_edit_hidden_behind_an_index_flag | contract_dispatch.RunnerCommits.test_runner_commits_an_edit_hidden_behind_an_index_flag |
| test_dispatch.py::RunnerCommitTests::test_failed_commit_is_blocked_environment_with_reason | contract_dispatch.RunnerCommits.test_failed_commit_is_blocked_environment_with_reason |
| test_dispatch.py::RunnerCommitTests::test_unlocatable_git_path_is_an_error_not_an_operation | internal: Rust unit test (dispatch) |
| test_dispatch.py::RunnerCommitTests::test_commit_respects_the_cancel_deadline | contract_dispatch.RunnerCommits.test_commit_respects_the_cancel_deadline |
| test_dispatch.py::RunnerCommitTests::test_externally_killed_codex_is_possibly_incomplete_and_partial_work_committed | contract_dispatch.RunnerCommits.test_externally_killed_codex_is_possibly_incomplete_and_partial_work_committed |
| test_dispatch.py::RunnerCommitTests::test_exit_without_final_turn_completed_is_possibly_incomplete | contract_dispatch.RunnerCommits.test_exit_without_final_turn_completed_is_possibly_incomplete |
| test_dispatch.py::RunnerCommitTests::test_complete_run_is_not_possibly_incomplete | contract_dispatch.RunnerCommits.test_complete_run_is_not_possibly_incomplete |
| test_dispatch.py::RunnerCommitTests::test_wrong_branch_is_not_committed | contract_dispatch.RunnerCommits.test_wrong_branch_is_not_committed |
| test_dispatch.py::RunnerCommitTests::test_operation_in_progress_is_not_committed | contract_dispatch.RunnerCommits.test_operation_in_progress_is_not_committed |
| test_dispatch.py::RunnerCommitTests::test_check_dispatch_is_not_committed | contract_dispatch.RunnerCommits.test_check_dispatch_is_not_committed |
| test_dispatch.py::RunnerCommitTests::test_cli_passes_kind_and_branch | contract_dispatch.RunnerCommits.test_cli_passes_kind_and_branch |
