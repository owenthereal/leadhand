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
| test_worker_env.py::WorkerEnvTests::test_worker_env_removes_tokens_and_sets_empty_gh_config | contract_worker_env.WorkerEnvironment.test_worker_env_removes_tokens_and_sets_empty_gh_config |
| test_worker_env.py::WorkerEnvTests::test_existing_git_config_env_entries_preserved | contract_worker_env.WorkerEnvironment.test_existing_git_config_env_entries_preserved |
| test_worker_env.py::WorkerEnvTests::test_ssh_push_fails_for_ssh_scp_and_explicit_pushurl | contract_worker_env.WorkerEnvironment.test_ssh_push_fails_for_ssh_scp_and_explicit_pushurl |
| test_worker_env.py::WorkerEnvTests::test_https_push_without_credentials_fails_without_prompting | contract_worker_env.WorkerEnvironment.test_https_push_without_credentials_fails_without_prompting |
| test_worker_env.py::WorkerEnvTests::test_configured_askpass_is_not_run | contract_worker_env.WorkerEnvironment.test_configured_askpass_is_not_run; contract_worker_env.WorkerEnvironment.test_existing_git_config_parameters_kept_and_askpass_cleared |
| test_worker_env.py::WorkerEnvTests::test_gh_reports_no_login_with_parent_token | contract_worker_env.WorkerEnvironment.test_gh_reports_no_login_with_parent_token |
| test_worker_env.py::WorkerEnvTests::test_local_path_push_still_allowed | contract_worker_env.WorkerEnvironment.test_local_path_push_still_allowed |
| test_usage.py::ArithmeticTests::test_add_sums_fields_and_known_is_conjunction | internal: Rust unit test (usage) |
| test_usage.py::ArithmeticTests::test_subtract_is_field_wise_and_floored_at_zero | internal: Rust unit test (usage) |
| test_usage.py::ArithmeticTests::test_estimate_cost_opus_matches_recorded_session | contract_usage.Pricing.test_estimate_cost_opus_matches_recorded_session |
| test_usage.py::ArithmeticTests::test_estimate_cost_unpriced_model_is_none | contract_usage.Pricing.test_estimate_cost_unpriced_model_is_none; contract_usage.CodexCharges.test_codex_cumulative_takes_last_token_count_and_maps_fields |
| test_usage.py::LocatorTests::test_claude_transcript_locators | contract_usage.Locators.test_claude_transcript_locators; contract_usage.Locators.test_wildcard_worker_id_finds_no_transcript |
| test_usage.py::LocatorTests::test_missing_projects_dir_is_none | contract_usage.Locators.test_missing_projects_dir_is_none |
| test_usage.py::ClaudeUsageTests::test_claude_usage_dedupes_by_message_id | contract_usage.ClaudeTranscripts.test_claude_usage_dedupes_by_message_id |
| test_usage.py::ClaudeUsageTests::test_claude_usage_skips_excluded_ids | contract_usage.ClaudeTranscripts.test_claude_usage_skips_messages_already_charged |
| test_usage.py::ClaudeUsageTests::test_claude_usage_ignores_partial_and_garbage_lines | contract_usage.ClaudeTranscripts.test_claude_usage_ignores_partial_and_garbage_lines |
| test_usage.py::ChargeClaudeTests::test_charge_claude_resumed_worker_counts_each_message_once | contract_usage.ClaudeCharges.test_charge_claude_resumed_worker_counts_each_message_once |
| test_usage.py::ChargeClaudeTests::test_charge_record_is_durable_and_round_trips | contract_usage.ClaudeCharges.test_charge_record_is_durable_and_round_trips |
| test_usage.py::ChargeClaudeTests::test_charge_tops_up_message_charged_mid_stream | contract_usage.ClaudeCharges.test_charge_tops_up_message_charged_mid_stream |
| test_usage.py::ChargeClaudeTests::test_charge_none_path_is_unknown_and_relative_path_is_rejected | contract_usage.ClaudeCharges.test_charge_missing_file_is_unknown_and_moves_no_checkpoint; contract_usage.CodexCharges.test_charge_none_thread_is_unknown; contract_usage.Locators.test_relative_transcript_path_is_rejected |
| test_usage.py::ChargeClaudeTests::test_repeated_charge_id_is_harmless | contract_usage.ClaudeCharges.test_repeated_charge_id_is_harmless; contract_usage.CodexCharges.test_runner_crash_after_charge_keeps_the_charge |
| test_usage.py::ChargeClaudeTests::test_charge_missing_file_is_unknown_and_moves_no_checkpoint | contract_usage.ClaudeCharges.test_charge_missing_file_is_unknown_and_moves_no_checkpoint; contract_usage.CodexCharges.test_codex_cumulative_unknown_when_missing_or_without_token_count |
| test_usage.py::ChargeClaudeTests::test_totals_group_by_role_dispatch_and_source | contract_usage.ClaudeCharges.test_totals_group_by_role_dispatch_and_source |
| test_usage.py::CodexTests::test_codex_cumulative_takes_last_token_count_and_maps_fields | contract_usage.CodexCharges.test_codex_cumulative_takes_last_token_count_and_maps_fields |
| test_usage.py::CodexTests::test_codex_cumulative_unknown_when_missing_or_without_token_count | contract_usage.CodexCharges.test_codex_cumulative_unknown_when_missing_or_without_token_count |
| test_usage.py::CodexTests::test_charge_codex_cumulative_deltas | contract_usage.CodexCharges.test_charge_codex_cumulative_deltas |
| test_usage.py::CodexTests::test_codex_stream_fallback_charged_when_session_missing | contract_usage.CodexCharges.test_codex_stream_fallback_charged_when_session_missing |
| test_usage.py::CodexTests::test_codex_stream_usage_sums_turn_completed | contract_usage.CodexCharges.test_codex_stream_usage_sums_turn_completed |
| test_usage.py::CodexRolloutFixtureTests::test_rollout_charges_every_response_not_the_token_count_total | contract_usage.CodexRollout.test_rollout_charges_every_response_not_the_token_count_total |
| test_usage.py::CodexRolloutFixtureTests::test_rollout_charged_twice_charges_nothing_new | contract_usage.CodexRollout.test_rollout_charged_twice_charges_nothing_new |
| test_usage.py::CodexRolloutFixtureTests::test_rollout_truncated_before_resume_charges_only_the_killed_turn | contract_usage.CodexRollout.test_rollout_truncated_before_resume_charges_only_the_killed_turn |
| test_usage.py::CodexRolloutFixtureTests::test_rollout_repeated_response_id_counts_once | contract_usage.CodexRollout.test_rollout_repeated_response_id_counts_once |
| test_usage.py::CodexRolloutFixtureTests::test_rollout_thread_total_mismatch_is_recorded_not_fatal | contract_usage.CodexRollout.test_rollout_thread_total_mismatch_is_recorded_not_fatal |
| test_usage.py::CodexRolloutFixtureTests::test_possibly_incomplete_flag_is_recorded_on_the_charge | contract_usage.CodexRollout.test_possibly_incomplete_flag_is_recorded_on_the_charge |
| test_usage.py::CodexRolloutFixtureTests::test_rollout_without_any_usable_record_is_unknown | contract_usage.CodexRollout.test_rollout_without_any_usable_record_is_unknown |
| test_usage.py::CodexRolloutFixtureTests::test_token_count_is_only_a_fallback_for_a_rollout_without_records | contract_usage.CodexRollout.test_token_count_is_only_a_fallback_for_a_rollout_without_records |
| test_usage.py::CodexStreamFixtureTests::test_stream_usage_sums_turn_completed | contract_usage.CodexStream.test_stream_usage_sums_turn_completed |
| test_usage.py::CodexStreamFixtureTests::test_stream_cache_write_tokens_are_kept_as_five_minute_writes | contract_usage.CodexStream.test_stream_cache_write_tokens_are_kept_as_five_minute_writes |
| test_usage.py::ClaudeFixtureTests::test_stopped_transcript_charges_its_one_message_once | contract_usage.ClaudeTranscripts.test_stopped_transcript_charges_its_one_message_once |
| test_usage.py::ClaudeFixtureTests::test_resumed_transcript_charges_each_message_once | contract_usage.ClaudeTranscripts.test_resumed_transcript_charges_each_message_once |
| test_usage.py::ClaudeFixtureTests::test_resumed_file_growth_is_charged_once | contract_usage.ClaudeTranscripts.test_resumed_file_growth_is_charged_once |
| test_usage.py::CrashAndConcurrencyTests::test_crash_after_charge_before_attempt_keeps_exact_total | contract_usage.ClaudeCharges.test_crash_after_charge_before_attempt_keeps_exact_total |
| test_usage.py::CrashAndConcurrencyTests::test_concurrent_supervisor_and_runner_charges_keep_exact_totals | contract_usage.Concurrency.test_concurrent_supervisor_and_runner_charges_keep_exact_totals |
| test_usage_models.py::PerModel::test_mixed_batch_is_priced_per_model | contract_usage.Pricing.test_mixed_batch_is_priced_per_model |
| test_usage_models.py::PerModel::test_single_model_and_empty_synthetic_records_stay_single | contract_usage.Pricing.test_single_model_and_empty_synthetic_records_stay_single |
| test_usage_models.py::PerModel::test_unpriced_part_makes_the_estimate_unknown | contract_usage.Pricing.test_unpriced_part_makes_the_estimate_unknown |
| test_usage_models.py::PerModel::test_charge_keeps_the_per_model_parts | contract_usage.Pricing.test_charge_keeps_the_per_model_parts |
| test_procs.py::ProcsTests::test_own_process_info | internal: Rust unit test (procs) |
| test_procs.py::ProcsTests::test_start_time_is_stable_and_differs_between_processes | internal: Rust unit test (procs) |
| test_procs.py::ProcsTests::test_child_in_new_session_has_own_group_and_is_a_descendant | internal: Rust unit test (procs) |
| test_procs.py::ProcsTests::test_same_process_checks_start_time_and_zombies | internal: Rust unit test (procs) |
| test_procs.py::ProcsTests::test_ps_fallback_parses_lstart_with_spaces | internal: Rust unit test (procs) |
| test_procs.py::ProcsTests::test_tagged_processes_finds_a_process_by_an_environment_entry | internal: Rust unit test (procs) |
| test_procs.py::ProcsTests::test_missing_pid | internal: Rust unit test (procs) |
| test_launch.py::ArgvTests::test_launch_argv_order_and_auto_mode | contract_launch.Argv.test_argv_order_and_auto_mode; contract_launch.Argv.test_no_supervisor_route_means_no_model_or_effort |
| test_launch.py::ArgvTests::test_run_passes_supervisor_model_effort_and_cli_settings | contract_launch.Argv.test_run_passes_supervisor_model_effort_and_cli_settings |
| test_launch.py::ArgvTests::test_print_mode_adds_headless_stream_flags_before_the_prompt | contract_launch.Argv.test_print_mode_adds_headless_stream_flags_before_the_prompt |
| test_launch.py::ArgvTests::test_cli_print_option_is_hidden_and_off_by_default | contract_launch.Argv.test_print_option_is_hidden_and_off_by_default |
| test_launch.py::ArgvTests::test_first_message_names_skill_invocation | contract_launch.Argv.test_first_message_names_skill_invocation |
| test_launch.py::ArgvTests::test_attended_use_warning_printed | contract_launch.Argv.test_attended_use_warning_printed |
| test_launch.py::ArgvTests::test_keep_awake_prefix_per_platform | contract_launch.KeepAwake.test_keep_awake_runs_beside_claude_which_stays_the_direct_child; contract_launch.KeepAwake.test_only_the_platforms_keep_awake_tool_is_used |
| test_launch.py::ChildTests::test_supervisor_cwd_is_workspace_when_launched_elsewhere | contract_launch.Child.test_supervisor_cwd_is_workspace_when_launched_elsewhere |
| test_launch.py::ChildTests::test_env_has_run_id_dir_and_cli | contract_launch.Child.test_env_has_run_id_dir_and_cli |
| test_launch.py::ChildTests::test_run_accepts_deck_md_path | contract_launch.Child.test_run_accepts_deck_md_path |
| test_launch.py::ChildTests::test_not_a_run_is_an_error_and_starts_nothing | contract_launch.Child.test_not_a_run_is_an_error_and_starts_nothing |
| test_launch.py::ChildTests::test_child_exit_code_returned | contract_launch.Child.test_child_exit_code_returned |
| test_launch.py::ChildTests::test_launch_line_quotes_paths_with_spaces | contract_launch.LaunchLine.test_launch_line_quotes_paths_with_spaces |
| test_launch.py::OwnershipTests::test_refused_launch_changes_nothing | contract_launch.Ownership.test_refused_launch_changes_nothing |
| test_launch.py::OwnershipTests::test_cli_run_refusal_exits_3 | contract_launch.Ownership.test_cli_run_refusal_exits_3 |
| test_launch.py::OwnershipTests::test_lock_held_by_supervisor_after_launcher_killed | contract_launch.Ownership.test_lock_held_by_supervisor_after_launcher_killed |
| test_launch.py::OwnershipTests::test_lock_fd_alone_blocks_takeover | contract_launch.Ownership.test_lock_fd_alone_blocks_takeover |
| test_launch.py::OwnershipTests::test_recorded_live_supervisor_blocks_takeover | contract_launch.Ownership.test_recorded_live_supervisor_blocks_takeover |
| test_launch.py::OwnershipTests::test_resumed_appended_only_after_acquire | contract_launch.Ownership.test_resumed_appended_only_after_acquire |
| test_launch.py::OwnershipTests::test_first_launch_of_a_fresh_run_appends_no_resumed | contract_launch.Ownership.test_first_launch_of_a_fresh_run_appends_no_resumed |
| test_launch.py::OwnershipTests::test_holder_records_launcher_and_supervisor | contract_launch.Ownership.test_holder_records_launcher_and_supervisor |
| test_launch.py::OwnershipTests::test_signals_forwarded_to_child | contract_launch.Ownership.test_signals_forwarded_to_child |
| test_launch.py::FollowUpTests::test_linux_inhibitor_runs_beside_claude_which_stays_the_direct_child | contract_launch.KeepAwake.test_keep_awake_runs_beside_claude_which_stays_the_direct_child |
| test_launch.py::FollowUpTests::test_cli_path_with_spaces_warns_sandbox_exclusion_may_not_match | contract_launch.Argv.test_cli_path_with_spaces_warns_sandbox_exclusion_may_not_match |
| test_launch.py::FollowUpTests::test_signal_after_child_exit_is_not_forwarded | internal: Rust unit test (launch) |
| test_launch.py::FollowUpTests::test_launch_line_defaults_to_recorded_plugin_dir | contract_launch.LaunchLine.test_launch_line_defaults_to_recorded_plugin_dir |
| test_launch.py::FollowUpTests::test_busy_lock_with_dead_recorded_supervisor_says_not_running | contract_launch.Ownership.test_busy_lock_with_dead_recorded_supervisor_says_not_running |
| test_launch.py::InheritedLockTests::test_relaunch_succeeds_while_a_cli_child_of_the_killed_supervisor_runs | contract_launch.InheritedLock.test_relaunch_succeeds_while_a_cli_child_of_the_killed_supervisor_runs; contract_launch.LockFdEnvironment.test_a_worker_never_inherits_lead_hand_lock_fd |
| test_launch.py::InheritedLockTests::test_cli_closes_inherited_run_lock_fd | contract_launch.InheritedLock.test_cli_closes_inherited_run_lock_fd; contract_launch.LockFdEnvironment.test_a_worker_never_inherits_lead_hand_lock_fd |
| test_launch.py::InheritedLockTests::test_cli_leaves_an_unrelated_fd_open | contract_launch.InheritedLock.test_cli_leaves_an_unrelated_fd_open; contract_launch.LockFdEnvironment.test_a_worker_never_inherits_lead_hand_lock_fd |
| test_launch.py::InPlaceCheckoutTests::test_in_place_launch_is_refused_while_another_in_place_run_is_supervised | dropped with in-place mode |
| test_launch.py::InPlaceCheckoutTests::test_in_place_launch_ignores_dead_supervisors_and_worktree_runs | dropped with in-place mode |
| test_launch.py::InPlaceCheckoutTests::test_worktree_launch_is_not_held_back_by_an_in_place_run | contract_launch.Ownership.test_another_runs_live_supervisor_does_not_hold_back_a_launch |
| test_launch.py::CheckoutClaimTests::test_in_place_launch_is_refused_while_a_run_in_another_runs_folder_is_supervised | dropped with in-place mode |
| test_launch.py::CheckoutClaimTests::test_in_place_launch_is_refused_before_the_other_launch_records_its_supervisor | dropped with in-place mode |
| test_launch.py::CheckoutClaimTests::test_a_worker_deleting_the_claim_record_does_not_free_the_checkout | dropped with in-place mode |
| test_launch.py::CheckoutClaimTests::test_in_place_launch_from_a_subdirectory_of_a_claimed_checkout_is_refused | dropped with in-place mode |
| test_launch.py::CheckoutClaimTests::test_a_checkout_that_cannot_be_flocked_is_refused_with_a_clear_error | dropped with in-place mode |
| test_launch.py::CheckoutClaimTests::test_cli_closes_an_inherited_claim_on_the_checkout_top_of_a_subdirectory_run | dropped with in-place mode |
| test_launch.py::CheckoutClaimTests::test_an_in_place_relaunch_is_refused_while_an_edit_is_hidden_behind_an_index_flag | dropped with in-place mode |
| test_launch.py::CheckoutClaimTests::test_a_worktree_run_launch_is_refused_while_the_repository_is_sparse | contract_launch.Child.test_a_worktree_run_launch_is_refused_while_the_repository_is_sparse |
| test_launch.py::CheckoutClaimTests::test_the_claim_is_released_and_its_record_removed_when_the_supervisor_exits | dropped with in-place mode |
| test_launch.py::CheckoutClaimTests::test_worktree_launch_is_not_held_back_by_a_claimed_checkout | contract_launch.Ownership.test_another_runs_live_supervisor_does_not_hold_back_a_launch |
| test_launch.py::CheckoutClaimTests::test_a_cli_child_of_a_killed_supervisor_does_not_keep_the_checkout_claimed | dropped with in-place mode |
| test_launch.py::CheckoutClaimTests::test_cli_leaves_an_unrelated_fd_named_as_the_checkout_claim_open | dropped with in-place mode |
| test_engine_cards.py::HappyPath::test_happy_path_two_cards_accepted_in_order | contract_smoke.Smoke.test_two_cards_accepted_in_order |
| test_engine_cards.py::HappyPath::test_requested_dispatch_is_repeated_until_started | contract_cards.HappyPath.test_requested_dispatch_is_repeated_until_started |
| test_engine_cards.py::HappyPath::test_started_dispatch_returns_wait_then_stop_worker_after_deadline | contract_cards.HappyPath.test_started_dispatch_returns_wait_then_stop_worker_after_deadline |
| test_engine_cards.py::HappyPath::test_duplicate_result_is_harmless | contract_cards.HappyPath.test_duplicate_result_is_harmless |
| test_engine_cards.py::Loop::test_repeated_answers_and_reviews_get_distinct_ids | contract_cards.Loop.test_repeated_answers_and_reviews_get_distinct_ids |
| test_engine_cards.py::Loop::test_by_risk_review_skips_low_risk_card | contract_cards.Loop.test_by_risk_review_skips_low_risk_card |
| test_engine_cards.py::Loop::test_defects_get_one_fix_round_then_rereview_on_production_change | contract_cards.Loop.test_defects_get_one_fix_round_then_rereview_on_production_change |
| test_engine_cards.py::Loop::test_clean_verdict_with_medium_finding_counts_as_defects | contract_cards.Loop.test_clean_verdict_with_medium_finding_counts_as_defects |
| test_engine_cards.py::Loop::test_clean_rereview_noting_fixed_findings_stays_clean | contract_cards.Loop.test_clean_rereview_noting_fixed_findings_stays_clean |
| test_engine_cards.py::Loop::test_clean_verdict_cross_check_reads_only_findings_lines | contract_cards.Loop.test_clean_verdict_cross_check_reads_only_findings_lines |
| test_engine_cards.py::Loop::test_clean_verdict_cross_check_counts_bullet_findings_lines | contract_cards.Loop.test_clean_verdict_cross_check_counts_bullet_findings_lines |
| test_engine_cards.py::Loop::test_test_only_fix_returns_check_diff | contract_cards.Loop.test_test_only_fix_returns_check_diff |
| test_engine_cards.py::Loop::test_defects_after_fix_round_escalate_with_fresh_worker | contract_cards.Loop.test_defects_after_fix_round_escalate_with_fresh_worker |
| test_engine_cards.py::Loop::test_spent_escalates_and_exhausted_ladder_parks | contract_cards.Loop.test_spent_escalates_and_exhausted_ladder_parks |
| test_engine_cards.py::Loop::test_usage_limit_ends_blocked_without_switching_vendor | contract_cards.Loop.test_usage_limit_ends_blocked_without_switching_vendor |
| test_engine_cards.py::Loop::test_other_blocked_retries_once_then_ends_blocked_environment | contract_cards.Loop.test_other_blocked_retries_once_then_ends_blocked_environment |
| test_engine_cards.py::Loop::test_question_ruling_resumes_worker | contract_cards.Loop.test_question_ruling_resumes_worker |
| test_engine_cards.py::Loop::test_parked_card_skips_dependents_and_continues_independent | contract_cards.Loop.test_parked_card_skips_dependents_and_continues_independent |
| test_engine_cards.py::Start::test_start_returns_existing_unfinished_run_unless_new | contract_cards.Start.test_start_returns_existing_unfinished_run_unless_new |
| test_engine_cards.py::Start::test_start_never_reuses_an_unfinished_run_of_another_clone | contract_cards.Start.test_start_never_reuses_an_unfinished_run_of_another_clone |
| test_engine_cards.py::Start::test_start_never_reuses_another_runs_worktree | contract_cards.Start.test_start_never_reuses_another_runs_worktree |
| test_engine_cards.py::Start::test_start_refuses_unsupported_settings_and_scout_cards | contract_cards.Start.test_start_refuses_unsupported_settings_and_scout_cards |
| test_engine_cards.py::Start::test_check_and_start_refuse_step_limits_claude_workers_cannot_honour | contract_cards.Start.test_check_and_start_refuse_step_limits_claude_workers_cannot_honour |
| test_engine_cards.py::Start::test_deck_changed_since_start_refuses | contract_cards.Start.test_deck_changed_since_start_refuses |
| test_engine_cards.py::Records::test_reply_without_status_blocks_and_keeps_raw | contract_cards.Records.test_reply_without_status_blocks_and_keeps_raw |
| test_engine_cards.py::Records::test_turn_limit_reply_is_spent | contract_cards.Records.test_turn_limit_reply_is_spent |
| test_engine_cards.py::Records::test_invalid_record_transition_raises | contract_cards.Records.test_invalid_record_transition_raises |
| test_engine_cards.py::Records::test_timer_returns_at_deadline | contract_cards.Records.test_timer_returns_at_deadline |
| test_engine_cards.py::Records::test_cli_records_reply_from_stdin | contract_cards.Records.test_cli_records_reply_from_stdin |
| test_engine_cards.py::Codex::test_codex_route_dispatches_proxy_with_null_agent_model | contract_cards.Codex.test_codex_route_dispatches_proxy_with_null_agent_model |
| test_engine_cards.py::Codex::test_stop_worker_for_codex_cancels_runner_and_records_before_returning | contract_cards.Codex.test_stop_worker_for_codex_cancels_runner_and_records_before_returning |
| test_engine_cards.py::Crashes::test_crash_after_fast_forward_before_accepted_reconciles | contract_cards.Crashes.test_crash_after_fast_forward_before_accepted_reconciles |
| test_engine_cards.py::Crashes::test_crash_after_result_files_before_event_reconciles | contract_cards.Crashes.test_crash_after_result_files_before_event_reconciles |
| test_engine_cards.py::Crashes::test_crash_after_charge_before_attempt_retry_keeps_one_charge | contract_cards.Crashes.test_crash_after_charge_before_attempt_retry_keeps_one_charge |
| test_engine_cards.py::Supervisor::test_supervisor_usage_charged_once_across_calls | contract_cards.Supervisor.test_supervisor_usage_charged_once_across_calls |
| test_engine_by_risk.py::ByRisk::test_ready_at_the_attempt_start_tip_goes_to_review | contract_cards.ByRisk.test_ready_at_the_attempt_start_tip_goes_to_review |
| test_engine_by_risk.py::ByRisk::test_low_risk_ready_with_commits_is_still_accepted_without_review | contract_cards.ByRisk.test_low_risk_ready_with_commits_is_still_accepted_without_review |
| test_engine_decisions.py::DecisionNeeded::test_question_parked_on_a_decision_ends_blocked_and_unpark_continues | contract_cards.DecisionNeeded.test_question_parked_on_a_decision_ends_blocked_and_unpark_continues |
| test_engine_decisions.py::DecisionNeeded::test_card_waiting_on_a_decision_card_still_ends_blocked | contract_cards.DecisionNeeded.test_card_waiting_on_a_decision_card_still_ends_blocked |
| test_engine_decisions.py::DecisionNeeded::test_violation_cap_park_ends_blocked_for_a_decision | contract_cards.DecisionNeeded.test_violation_cap_park_ends_blocked_for_a_decision |
| test_engine_decisions.py::DecisionNeeded::test_park_that_is_not_a_decision_still_ends_incomplete | contract_cards.DecisionNeeded.test_park_that_is_not_a_decision_still_ends_incomplete |
| test_engine_decisions.py::DecisionNeeded::test_unpark_refuses_on_a_run_it_cannot_reopen_and_hides_the_hint | contract_cards.DecisionNeeded.test_unpark_refuses_on_a_run_it_cannot_reopen_and_hides_the_hint |
| test_engine_reply_file.py::ReplyFile::test_dispatch_action_carries_a_git_ignored_reply_file_in_the_scratch_dir | contract_cards.ReplyFile.test_dispatch_action_carries_a_git_ignored_reply_file_in_the_scratch_dir |
| test_engine_reply_file.py::ReplyFile::test_wait_names_the_same_reply_file | contract_cards.ReplyFile.test_wait_names_the_same_reply_file |
| test_engine_reply_file.py::ReplyFile::test_cli_records_reply_from_a_file_with_spaces_in_its_path | contract_cards.ReplyFile.test_cli_records_reply_from_a_file_with_spaces_in_its_path |
| test_engine_reply_file.py::ReplyFile::test_reply_and_reply_file_are_exclusive | contract_cards.ReplyFile.test_reply_and_reply_file_are_exclusive |
| test_engine_reply_file.py::ReplyFile::test_missing_reply_file_falls_back_to_the_report_head_with_a_note | contract_cards.ReplyFile.test_missing_reply_file_falls_back_to_the_report_head_with_a_note |
| test_engine_reply_file.py::ReplyFile::test_new_request_removes_a_stale_reply_file | contract_cards.ReplyFile.test_new_request_removes_a_stale_reply_file |
| test_engine_codex_result.py::CodexResultFromRunner::test_early_still_running_reply_waits_and_starts_no_second_implementer | contract_cards.CodexResultFromRunner.test_early_still_running_reply_waits_and_starts_no_second_implementer |
| test_engine_codex_result.py::CodexResultFromRunner::test_invented_verdict_without_a_worker_is_not_accepted | contract_cards.CodexResultFromRunner.test_invented_verdict_without_a_worker_is_not_accepted |
| test_engine_codex_result.py::CodexResultFromRunner::test_skipping_a_codex_card_stops_its_runner | contract_cards.CodexResultFromRunner.test_skipping_a_codex_card_stops_its_runner |
| test_engine_codex_e2e.py::NonCommittingCodex::test_two_card_run_with_gates_completes_with_the_runners_commits | contract_cards.NonCommittingCodex.test_two_card_run_with_gates_completes_with_the_runners_commits |
| test_engine_run_branch.py::ImplementerGuard::test_implementer_commit_on_the_run_branch_is_a_violation | contract_cards.ImplementerGuard.test_implementer_commit_on_the_run_branch_is_a_violation |
| test_engine_run_branch.py::RunHeadCheck::test_run_branch_moved_between_dispatches_fails_the_run | contract_cards.RunHeadCheck.test_run_branch_moved_between_dispatches_fails_the_run |
| test_engine_run_branch.py::RunHeadCheck::test_outcome_checks_the_run_head_after_a_cancel | contract_cards.RunHeadCheck.test_outcome_checks_the_run_head_after_a_cancel |
| test_engine_untracked.py::EngineSide::test_parked_cards_untracked_file_does_not_land_via_the_next_codex_card | contract_cards.EngineSide.test_parked_cards_untracked_file_does_not_land_via_the_next_codex_card |
| test_engine_untracked.py::EngineSide::test_cancel_partial_commit_leaves_untracked_files_that_were_there_before | contract_cards.EngineSide.test_cancel_partial_commit_leaves_untracked_files_that_were_there_before |
| test_engine_untracked.py::RunnerSide::test_runner_commits_only_tracked_changes_and_files_created_during_the_dispatch | contract_cards.RunnerSide.test_runner_commits_only_tracked_changes_and_files_created_during_the_dispatch |
| test_briefs.py::BriefTests::test_implement_brief_contains_contract_fields_verbatim | contract_briefs.Briefs.test_implement_brief_contains_contract_fields_verbatim |
| test_briefs.py::BriefTests::test_test_first_defaults_to_when_cheap | contract_briefs.Briefs.test_test_first_defaults_to_when_cheap |
| test_briefs.py::BriefTests::test_every_brief_kind_has_a_template | contract_briefs.Briefs.test_every_brief_kind_fills_its_values; contract_briefs.Briefs.test_integrate_permits_rebase_onto_run_tip; contract_briefs.Briefs.test_continue_brief_carries_on_from_the_branch |
| test_briefs.py::BriefTests::test_kind_specific_content | contract_briefs.Briefs.test_kind_specific_content; contract_briefs.Briefs.test_continue_brief_carries_on_from_the_branch |
| test_briefs.py::BriefTests::test_card_briefs_require_card_base_and_branch | contract_briefs.Briefs.test_card_briefs_render_card_base_and_branch |
| test_briefs.py::BriefTests::test_review_needs_implementer_report_unless_test_first_off | contract_briefs.Briefs.test_review_names_the_implementer_report |
| test_briefs.py::BriefTests::test_integrate_permits_rebase_onto_run_tip | contract_briefs.Briefs.test_integrate_permits_rebase_onto_run_tip |
| test_briefs.py::BriefTests::test_final_briefs_list_scope | contract_briefs.Briefs.test_final_briefs_list_scope; contract_briefs.Briefs.test_final_briefs_cover_only_the_accepted_cards |
| test_briefs.py::BriefTests::test_evidence_requires_cards | internal: Rust unit test (briefs) |
| test_briefs.py::BriefTests::test_answer_quotes_every_ruling_line | contract_briefs.Briefs.test_answer_quotes_every_ruling_line |
| test_briefs.py::BriefTests::test_verify_says_skip_for_empty_commands | internal: Rust unit test (briefs) |
| test_briefs.py::BriefTests::test_missing_extra_and_unknown_kind_raise | internal: Rust unit test (briefs) |
| test_briefs.py::BriefTests::test_brief_and_report_paths_are_inside_workspace_scratch | contract_briefs.Briefs.test_brief_and_report_paths_are_inside_workspace_scratch |
| test_briefs.py::BriefTests::test_scratch_dir_is_ignored_by_git | contract_briefs.Briefs.test_scratch_dir_is_ignored_by_git |
| test_briefs.py::BriefTests::test_agent_for_maps_every_role_and_vendor | contract_briefs.Briefs.test_agent_for_maps_every_role_and_vendor; contract_cards.Codex.test_codex_route_dispatches_proxy_with_null_agent_model; contract_cards.CodexResultFromRunner.test_invented_verdict_without_a_worker_is_not_accepted |
| test_briefs.py::BriefTests::test_claude_dispatch_message_names_paths | contract_briefs.Briefs.test_claude_dispatch_message_names_paths |
| test_briefs.py::BriefTests::test_codex_dispatch_message_is_one_quoted_command | contract_briefs.Briefs.test_codex_dispatch_message_is_one_quoted_command; contract_briefs.Briefs.test_codex_dispatch_message_quotes_shell_metacharacters |
| test_briefs.py::BriefTests::test_worker_prompt_for_codex_workers | contract_briefs.Briefs.test_worker_prompt_for_codex_workers |
| test_briefs.py::BriefTests::test_codex_implementer_prompt_forbids_commits_and_claude_agents_do_not | contract_briefs.Briefs.test_codex_implementer_prompt_forbids_commits_and_claude_agents_do_not |
| test_briefs.py::BriefTests::test_parse_report_head_variants | contract_briefs.Briefs.test_parse_report_head_variants |
| test_engine_integration.py::Integration::test_resumed_parked_card_is_rebased_rereviewed_verified_and_fast_forwarded | contract_integration.Integration.test_resumed_parked_card_is_rebased_rereviewed_verified_and_fast_forwarded |
| test_engine_integration.py::Integration::test_integration_conflict_requests_integrate_attempt | contract_integration.Integration.test_integration_conflict_requests_integrate_attempt |
| test_engine_integration.py::Integration::test_crash_after_rebase_before_event_reconciles | contract_integration.Integration.test_crash_after_rebase_before_event_reconciles |
| test_engine_integration.py::Integration::test_crash_before_rebase_rebases_on_restart | contract_integration.Integration.test_crash_before_rebase_rebases_on_restart |
| test_engine_integration.py::Integration::test_failed_verification_escalates | contract_integration.Integration.test_failed_verification_escalates |
| test_engine_integration.py::Unpark::test_unpark_without_a_known_worker_starts_a_fresh_attempt | contract_integration.Unpark.test_unpark_without_a_known_worker_starts_a_fresh_attempt |
| test_engine_integration.py::Unpark::test_unpark_cli_appends_unparked_and_refuses_a_card_not_parked | contract_integration.Unpark.test_unpark_cli_appends_unparked_and_refuses_a_card_not_parked |
| test_engine_integration.py::Violations::test_reviewer_tracked_change_voids_review_and_reruns | contract_integration.Violations.test_reviewer_tracked_change_voids_review_and_reruns |
| test_engine_integration.py::Violations::test_reviewer_moving_head_is_violation | contract_integration.Violations.test_reviewer_moving_head_is_violation |
| test_engine_integration.py::Violations::test_reviewer_hiding_an_edit_behind_an_index_flag_is_violation | contract_integration.Violations.test_reviewer_hiding_an_edit_behind_an_index_flag_is_violation |
| test_engine_integration.py::Violations::test_a_partial_commit_takes_an_edit_hidden_behind_an_index_flag | contract_integration.Violations.test_a_partial_commit_takes_an_edit_hidden_behind_an_index_flag |
| test_engine_integration.py::Violations::test_verifier_staged_change_is_violation | contract_integration.Violations.test_verifier_staged_change_is_violation |
| test_engine_integration.py::Violations::test_violation_preserves_index_and_worktree_on_ref | contract_integration.Violations.test_violation_preserves_index_and_worktree_on_ref |
| test_engine_integration.py::Violations::test_untracked_build_output_is_not_a_violation | contract_integration.Violations.test_untracked_build_output_is_not_a_violation |
| test_engine_integration.py::Violations::test_reviewer_committing_on_run_branch_and_returning_to_card_tip_is_violation | contract_integration.Violations.test_reviewer_committing_on_run_branch_and_returning_to_card_tip_is_violation |
| test_engine_integration.py::Violations::test_reviewer_creating_and_deleting_refs_is_violation | contract_integration.Violations.test_reviewer_creating_and_deleting_refs_is_violation |
| test_engine_integration.py::Violations::test_violation_interrupted_before_restore_still_voids_on_restart | contract_integration.Violations.test_violation_interrupted_before_restore_still_voids_on_restart |
| test_engine_integration.py::Violations::test_voided_reviews_do_not_spend_the_retry_budget | contract_integration.Violations.test_voided_reviews_do_not_spend_the_retry_budget |
| test_engine_integration.py::OneCardInFlight::test_unpark_while_another_card_is_in_flight_waits | contract_integration.OneCardInFlight.test_unpark_while_another_card_is_in_flight_waits |
| test_engine_integration.py::OneCardInFlight::test_engine_ref_moves_after_dispatch_are_not_violations | contract_integration.OneCardInFlight.test_engine_ref_moves_after_dispatch_are_not_violations |
| test_engine_integration.py::FixRound1::test_repeated_check_violations_park_the_card | contract_integration.FixRound1.test_repeated_check_violations_park_the_card |
| test_engine_integration.py::FixRound1::test_unpark_after_relaunch_starts_fresh_attempt_with_note | contract_integration.FixRound1.test_unpark_after_relaunch_starts_fresh_attempt_with_note |
| test_engine_integration.py::FixRound1::test_operation_left_in_progress_is_blocked_and_preserved | contract_integration.FixRound1.test_operation_left_in_progress_is_blocked_and_preserved |
| test_engine_integration.py::FixRound1::test_implementer_leftovers_are_preserved_before_review_without_voiding | contract_integration.FixRound1.test_implementer_leftovers_are_preserved_before_review_without_voiding |
| test_engine_integration.py::FixRound1::test_integration_keeps_the_pre_integration_tip | contract_integration.FixRound1.test_integration_keeps_the_pre_integration_tip |
| test_engine_integration.py::FixRound1::test_integrate_moving_run_branch_is_violation | contract_integration.FixRound1.test_integrate_moving_run_branch_is_violation |
| test_engine_integration.py::FixRound1::test_integrate_dropping_card_work_escalates | contract_integration.FixRound1.test_integrate_dropping_card_work_escalates |
| test_engine_integration.py::FixRound1::test_integrate_question_answer_goes_through_integration | contract_integration.FixRound1.test_integrate_question_answer_goes_through_integration |
| test_engine_integration.py::FixRound2::test_voided_answer_to_integrate_keeps_its_resume_chain | contract_integration.FixRound2.test_voided_answer_to_integrate_keeps_its_resume_chain |
| test_engine_integration.py::FixRound2::test_unpark_after_violation_cap_reruns_the_check | contract_integration.FixRound2.test_unpark_after_violation_cap_reruns_the_check |
| test_engine_integration.py::FixRound2::test_voided_rereview_is_rerun_by_a_fresh_worker | contract_integration.FixRound2.test_voided_rereview_is_rerun_by_a_fresh_worker |
| test_engine_integration.py::CodexIntegrate::test_integrate_after_codex_implementer_routes_to_claude | contract_integration.CodexIntegrate.test_integrate_after_codex_implementer_routes_to_claude |
| test_engine_integration.py::CodexIntegrate::test_failed_runner_commit_is_recorded_blocked_environment | contract_integration.CodexIntegrate.test_failed_runner_commit_is_recorded_blocked_environment |
| test_engine_integration.py::CodexIntegrate::test_integrate_without_claude_parks_the_card | contract_integration.CodexIntegrate.test_integrate_without_claude_parks_the_card |
| test_engine_finish.py::Complete::test_complete_run_writes_outcome_and_report | contract_finish.Complete.test_complete_run_writes_outcome_and_report |
| test_engine_finish.py::Complete::test_runs_md_line_appended_once | contract_finish.Complete.test_runs_md_line_appended_once |
| test_engine_finish.py::SharedRunIndex::test_concurrent_runs_never_lose_a_runs_md_line | contract_finish.SharedRunIndex.test_concurrent_runs_never_lose_a_runs_md_line |
| test_engine_finish.py::RelativeDocsDir::test_relative_docs_dir_resolves_against_the_repository_from_any_cwd | contract_finish.RelativeDocsDir.test_relative_docs_dir_resolves_against_the_repository_from_any_cwd |
| test_engine_finish.py::FixWaves::test_final_review_defects_get_a_fix_wave_then_all_gates_rerun | contract_finish.FixWaves.test_final_review_defects_get_a_fix_wave_then_all_gates_rerun |
| test_engine_finish.py::FixWaves::test_verify_fix_after_security_pass_reruns_security_and_evidence | contract_finish.FixWaves.test_verify_fix_after_security_pass_reruns_security_and_evidence |
| test_engine_finish.py::FixWaves::test_fix_waves_are_capped | contract_finish.FixWaves.test_fix_waves_are_capped |
| test_engine_finish.py::FixWaves::test_rejected_fix_is_not_accepted_and_spends_its_wave | contract_finish.FixWaves.test_rejected_fix_is_not_accepted_and_spends_its_wave |
| test_engine_finish.py::Evidence::test_evidence_block_gap_ends_blocked | contract_finish.Evidence.test_evidence_block_gap_ends_blocked |
| test_engine_finish.py::Evidence::test_evidence_record_gap_makes_incomplete_with_hold | contract_finish.Evidence.test_evidence_record_gap_makes_incomplete_with_hold |
| test_engine_finish.py::Evidence::test_resumed_blocked_run_keeps_gates_that_passed_on_the_same_head | contract_finish.Evidence.test_resumed_blocked_run_keeps_gates_that_passed_on_the_same_head |
| test_engine_finish.py::Security::test_security_gate_only_for_medium_or_high_or_sensitive_paths | contract_finish.Security.test_security_gate_only_for_medium_or_high_or_sensitive_paths |
| test_engine_finish.py::Verification::test_verification_failure_after_last_wave_is_failed | contract_finish.Verification.test_verification_failure_after_last_wave_is_failed |
| test_engine_finish.py::Unpark::test_unparked_card_waits_for_an_open_gate_then_gates_rerun | contract_finish.Unpark.test_unparked_card_waits_for_an_open_gate_then_gates_rerun |
| test_engine_finish.py::Guards::test_final_fix_moving_the_run_branch_is_voided_and_restored | contract_finish.Guards.test_final_fix_moving_the_run_branch_is_voided_and_restored |
| test_engine_finish.py::Guards::test_accepted_work_missing_from_the_run_branch_fails_the_run | contract_finish.Guards.test_accepted_work_missing_from_the_run_branch_fails_the_run |
| test_engine_finish.py::Guards::test_evidence_base_worktree_is_reset_and_guarded | contract_finish.Guards.test_evidence_base_worktree_is_reset_and_guarded |
| test_engine_finish.py::Guards::test_evidence_base_edit_hidden_behind_an_index_flag_is_violation | contract_finish.Guards.test_evidence_base_edit_hidden_behind_an_index_flag_is_violation |
| test_engine_finish.py::Guards::test_gate_violation_reruns_with_a_fresh_worker | contract_finish.Guards.test_gate_violation_reruns_with_a_fresh_worker |
| test_engine_finish.py::Guards::test_gate_violation_cap_ends_blocked | contract_finish.Guards.test_gate_violation_cap_ends_blocked |
| test_engine_finish.py::RunWorkerFailures::test_gate_without_a_result_twice_ends_blocked_environment | contract_finish.RunWorkerFailures.test_gate_without_a_result_twice_ends_blocked_environment |
| test_engine_finish.py::RunWorkerFailures::test_gate_usage_limit_ends_blocked_provider_capacity | contract_finish.RunWorkerFailures.test_gate_usage_limit_ends_blocked_provider_capacity |
| test_engine_finish.py::Crashes::test_crash_before_gate_record_records_once | contract_finish.Crashes.test_crash_before_gate_record_records_once |
| test_engine_finish.py::Crashes::test_crash_before_final_fast_forward_reconciles | contract_finish.Crashes.test_crash_before_final_fast_forward_reconciles |
| test_engine_finish.py::Crashes::test_crash_after_final_fast_forward_reconciles | contract_finish.Crashes.test_crash_after_final_fast_forward_reconciles |
| test_engine_finish.py::FixRound2::test_spent_final_fix_is_not_rechecked_after_a_card_accept_crash | contract_finish.FixRound2.test_spent_final_fix_is_not_rechecked_after_a_card_accept_crash |
| test_engine_finish.py::FixRound2::test_crash_between_base_violation_found_and_restored_recovers | contract_finish.FixRound2.test_crash_between_base_violation_found_and_restored_recovers |
| test_engine_finish.py::FixRound2::test_evidence_rerun_after_a_worker_failure_gets_a_reset_base | contract_finish.FixRound2.test_evidence_rerun_after_a_worker_failure_gets_a_reset_base |
| test_engine_finish.py::FixRound2::test_voided_final_fix_reruns_from_the_run_head | contract_finish.FixRound2.test_voided_final_fix_reruns_from_the_run_head |
| test_engine_finish.py::NothingAccepted::test_run_with_every_card_parked_runs_no_gates | contract_finish.NothingAccepted.test_run_with_every_card_parked_runs_no_gates |
| test_engine_finish.py::BaseWorktreeOwnership::test_in_place_run_refuses_a_foreign_worktree_at_its_base_path | dropped with in-place mode |
| test_engine_finish.py::BaseWorktreeOwnership::test_worktree_run_refuses_a_foreign_worktree_at_its_base_path | contract_finish.BaseWorktreeOwnership.test_worktree_run_refuses_a_foreign_worktree_at_its_base_path |
| test_engine_finish.py::BaseWorktreeOwnership::test_a_foreign_worktree_after_a_failed_add_is_refused | contract_finish.BaseWorktreeOwnership.test_a_foreign_worktree_after_a_failed_add_is_refused |
| test_engine_finish.py::BaseWorktreeOwnership::test_a_foreign_worktree_replacing_the_runs_base_worktree_is_refused | contract_finish.BaseWorktreeOwnership.test_a_foreign_worktree_replacing_the_runs_base_worktree_is_refused |
| test_engine_finish.py::BaseWorktreeOwnership::test_a_base_worktree_left_by_a_crash_is_refused_once_it_holds_work | contract_finish.BaseWorktreeOwnership.test_a_base_worktree_left_by_a_crash_is_refused_once_it_holds_work |
| test_engine_finish.py::BaseWorktreeOwnership::test_a_crash_window_worktree_hiding_a_tracked_edit_is_refused | contract_finish.BaseWorktreeOwnership.test_a_crash_window_worktree_hiding_a_tracked_edit_is_refused |
| test_engine_finish.py::BaseWorktreeOwnership::test_a_base_worktree_added_before_a_crash_is_still_the_runs | contract_finish.BaseWorktreeOwnership.test_a_base_worktree_added_before_a_crash_is_still_the_runs |
| test_engine_finish.py::SparseCheckout::test_a_worktree_run_in_a_sparse_checkout_is_refused_at_start | contract_finish.SparseCheckout.test_a_worktree_run_in_a_sparse_checkout_is_refused_at_start |
| test_engine_finish.py::SparseCheckout::test_a_sparse_checkout_a_reviewer_turns_on_is_a_violation | contract_finish.SparseCheckout.test_a_sparse_checkout_a_reviewer_turns_on_is_a_violation |
| test_final_review_minors.py::Timer::test_timer_returns_promptly_when_the_run_is_paused_or_cancelled | contract_finish.Timer.test_timer_returns_promptly_when_the_run_is_paused_or_cancelled |
| test_final_review_minors.py::PinnedIntegrate::test_integrate_after_a_codex_pinned_card_runs_on_claude | contract_finish.PinnedIntegrate.test_integrate_after_a_codex_pinned_card_runs_on_claude |
| test_final_review_minors.py::OperatorCommands::test_pause_cancel_and_handoff_print_the_quoted_absolute_cli | contract_finish.OperatorCommands.test_pause_cancel_and_handoff_print_the_quoted_absolute_cli |
| test_final_review_minors.py::OperatorCommands::test_relaunch_refusal_names_the_quoted_start_command | contract_finish.OperatorCommands.test_relaunch_refusal_names_the_quoted_start_command |
| test_final_review_minors.py::Settings::test_notify_and_capacity_are_warned_as_not_supported | contract_finish.Settings.test_notify_and_capacity_are_warned_as_not_supported |
| test_final_review_minors.py::Settings::test_defaults_are_not_warned | contract_finish.Settings.test_defaults_are_not_warned |
| test_final_review_minors.py::Start::test_start_skips_an_old_run_whose_event_log_is_unreadable | contract_finish.Start.test_start_skips_an_old_run_whose_event_log_is_unreadable |
| test_final_review_minors.py::CopyFiles::test_a_symlinked_report_is_not_followed | contract_finish.CopyFiles.test_a_symlinked_report_is_not_followed |
| test_final_review_minors.py::RunnerFinalize::test_heartbeat_is_touched_while_finalizing | contract_finish.RunnerFinalize.test_heartbeat_is_touched_while_finalizing |
| test_final_review_minors.py::RunnerFinalize::test_non_utf8_git_output_does_not_kill_the_commit | contract_finish.RunnerFinalize.test_non_utf8_git_output_does_not_kill_the_commit |
| test_recovery.py::Reconcile::test_restart_after_truncated_final_event_then_append | contract_recovery.Reconcile.test_restart_after_truncated_final_event_then_append |
| test_recovery.py::Reconcile::test_report_without_recorded_result_is_recorded | contract_recovery.Reconcile.test_report_without_recorded_result_is_recorded |
| test_recovery.py::Reconcile::test_commits_without_report_continue_not_discard | contract_recovery.Reconcile.test_commits_without_report_continue_not_discard |
| test_recovery.py::Reconcile::test_nothing_on_the_branch_requests_the_same_attempt_again | contract_recovery.Reconcile.test_nothing_on_the_branch_requests_the_same_attempt_again |
| test_recovery.py::Reconcile::test_fix_round_resumes_with_a_fresh_worker_from_the_findings | contract_recovery.Reconcile.test_fix_round_resumes_with_a_fresh_worker_from_the_findings |
| test_recovery.py::Reconcile::test_acceptance_started_completes_after_restart | contract_recovery.Reconcile.test_acceptance_started_completes_after_restart |
| test_recovery.py::Reconcile::test_unmatched_state_fails_with_reason | contract_recovery.Reconcile.test_unmatched_state_fails_with_reason |
| test_recovery.py::Reconcile::test_missing_card_branch_fails_with_reason | contract_recovery.Reconcile.test_missing_card_branch_fails_with_reason |
| test_recovery.py::Reconcile::test_usage_after_interrupted_resume_equals_session_totals | contract_recovery.Reconcile.test_usage_after_interrupted_resume_equals_session_totals |
| test_recovery.py::Codex::test_live_codex_runner_is_waited_not_restarted | contract_recovery.Codex.test_live_codex_runner_is_waited_not_restarted |
| test_recovery.py::Codex::test_hung_runner_with_stale_heartbeat_is_cancelled_before_redispatch | contract_recovery.Codex.test_hung_runner_with_stale_heartbeat_is_cancelled_before_redispatch |
| test_recovery.py::Codex::test_dead_runner_without_result_is_finalized_and_dispatched_again | contract_recovery.Codex.test_dead_runner_without_result_is_finalized_and_dispatched_again |
| test_recovery.py::PauseCancel::test_pause_ends_blocked_paused_with_handoff | contract_recovery.PauseCancel.test_pause_ends_blocked_paused_with_handoff |
| test_recovery.py::PauseCancel::test_cancel_ends_incomplete_cancelled_and_keeps_card_branches | contract_recovery.PauseCancel.test_cancel_ends_incomplete_cancelled_and_keeps_card_branches |
| test_recovery.py::PauseCancel::test_cancel_works_after_the_source_deck_was_edited | contract_recovery.PauseCancel.test_cancel_works_after_the_source_deck_was_edited |
| test_recovery.py::PauseCancel::test_pause_works_after_the_source_deck_was_deleted_but_nothing_new_starts | contract_recovery.PauseCancel.test_pause_works_after_the_source_deck_was_deleted_but_nothing_new_starts |
| test_recovery.py::PauseCancel::test_cancel_survives_a_relaunch_before_it_finished | contract_recovery.PauseCancel.test_cancel_survives_a_relaunch_before_it_finished |
| test_recovery.py::PauseCancel::test_cancel_confirms_detached_codex_stopped_before_outcome | contract_recovery.PauseCancel.test_cancel_confirms_detached_codex_stopped_before_outcome |
| test_recovery.py::PauseCancel::test_crash_after_cancel_request_finishes_the_cancel_on_restart | contract_recovery.PauseCancel.test_crash_after_cancel_request_finishes_the_cancel_on_restart |
| test_recovery.py::PauseCancel::test_relaunch_refuses_a_finished_run | contract_recovery.PauseCancel.test_relaunch_refuses_a_finished_run |
| test_recovery.py::PauseCancel::test_run_skill_steers_with_plain_commands | plugin file check (contract_plugin_files) |
| test_recovery.py::FixRound1::test_cancel_after_an_interrupted_acceptance_reconciles_first | contract_recovery.FixRound1.test_cancel_after_an_interrupted_acceptance_reconciles_first |
| test_recovery.py::FixRound1::test_cancel_commits_a_stopped_claude_workers_partial_work | contract_recovery.FixRound1.test_cancel_commits_a_stopped_claude_workers_partial_work |
| test_recovery.py::FixRound1::test_cancel_commits_partial_work_of_a_worker_that_died_with_its_session | contract_recovery.FixRound1.test_cancel_commits_partial_work_of_a_worker_that_died_with_its_session |
| test_recovery.py::FixRound1::test_stopped_check_worker_changes_are_not_committed | contract_recovery.FixRound1.test_stopped_check_worker_changes_are_not_committed |
| test_recovery.py::FixRound1::test_failed_reconciliation_stops_live_codex_before_the_outcome | contract_recovery.FixRound1.test_failed_reconciliation_stops_live_codex_before_the_outcome |
| test_recovery.py::FixRound1::test_orphaned_codex_is_cancelled_before_anything_else | contract_recovery.FixRound1.test_orphaned_codex_is_cancelled_before_anything_else |
| test_recovery.py::FixRound1::test_crash_in_reconciliations_cancel_window_is_repeat_safe | contract_recovery.FixRound1.test_crash_in_reconciliations_cancel_window_is_repeat_safe |
| test_recovery.py::FixRound1::test_unrecorded_codex_start_past_its_deadline_is_not_charged_or_stopped | contract_recovery.FixRound1.test_unrecorded_codex_start_past_its_deadline_is_not_charged_or_stopped |
| test_recovery.py::FixRound1::test_interrupted_review_without_report_runs_again | contract_recovery.FixRound1.test_interrupted_review_without_report_runs_again |
| test_recovery.py::FixRound1::test_interrupted_gate_without_report_runs_again | contract_recovery.FixRound1.test_interrupted_gate_without_report_runs_again |
| test_recovery.py::FixRound1::test_interrupted_final_fix_before_its_commit_runs_again | contract_recovery.FixRound1.test_interrupted_final_fix_before_its_commit_runs_again |
| test_recovery.py::FixRound1::test_interrupted_final_fix_after_its_commit_keeps_it | contract_recovery.FixRound1.test_interrupted_final_fix_after_its_commit_keeps_it |
| test_recovery.py::FixRound2::test_rewritten_branch_fails_the_run | contract_recovery.FixRound2.test_rewritten_branch_fails_the_run |
| test_recovery.py::FixRound2::test_rewritten_branch_fails_the_run_even_with_a_cancel_pending | contract_recovery.FixRound2.test_rewritten_branch_fails_the_run_even_with_a_cancel_pending |
| test_recovery.py::FixRound2::test_cancel_with_the_run_branch_gone_fails_with_every_accepted_card_missing | contract_recovery.FixRound2.test_cancel_with_the_run_branch_gone_fails_with_every_accepted_card_missing |
| test_recovery.py::FixRound2::test_run_branch_deleted_after_acceptance_ends_failed_on_relaunch | contract_recovery.FixRound2.test_run_branch_deleted_after_acceptance_ends_failed_on_relaunch |
| test_recovery.py::RunnerHelpers::test_runner_state | internal: Rust unit test (dispatch) |
| test_recovery.py::RunnerHelpers::test_stop_strays_stops_what_a_dead_runner_left_and_leaves_a_live_runner_alone | contract_recovery.RunnerHelpers.test_stop_strays_stops_what_a_dead_runner_left_and_leaves_a_live_runner_alone |
| test_recovery.py::EveryCrashPoint::test_restart_from_every_crash_point_reaches_same_final_state | contract_recovery.EveryCrashPoint.test_restart_from_every_crash_point_reaches_same_final_state |
| test_engine_recovery.py::ResumeAfterBlocked::test_blocked_provider_capacity_resumes_with_a_new_dispatch | contract_recovery.ResumeAfterBlocked.test_blocked_provider_capacity_resumes_with_a_new_dispatch |
| test_engine_recovery.py::ResumeAfterBlocked::test_blocked_environment_resumes_with_a_fresh_retry_budget | contract_recovery.ResumeAfterBlocked.test_blocked_environment_resumes_with_a_fresh_retry_budget |
| test_engine_recovery.py::ResumeAfterBlocked::test_relaunch_without_an_outcome_keeps_the_retry_budget | contract_recovery.ResumeAfterBlocked.test_relaunch_without_an_outcome_keeps_the_retry_budget |
| test_engine_recovery.py::ResumeAfterBlocked::test_park_after_the_run_ended_raises | contract_recovery.ResumeAfterBlocked.test_park_after_the_run_ended_raises |
| test_engine_recovery.py::RunCollisions::test_start_returns_a_blocked_run_as_existing | contract_recovery.RunCollisions.test_start_returns_a_blocked_run_as_existing |
| test_engine_recovery.py::RunCollisions::test_new_run_suffixes_a_branch_owned_by_another_run | contract_recovery.RunCollisions.test_new_run_suffixes_a_branch_owned_by_another_run |
| test_engine_recovery.py::RunCollisions::test_start_after_a_finished_run_suffixes_its_branch | contract_recovery.RunCollisions.test_start_after_a_finished_run_suffixes_its_branch |
| test_engine_recovery.py::RunCollisions::test_interrupted_start_retry_uses_a_free_branch | contract_recovery.RunCollisions.test_interrupted_start_retry_uses_a_free_branch |
| test_engine_recovery.py::RunCollisions::test_unrecorded_existing_branch_is_not_reused | contract_recovery.RunCollisions.test_unrecorded_existing_branch_is_not_reused |
| test_engine_recovery.py::RunCollisions::test_existing_card_branch_makes_its_run_branch_name_taken | contract_recovery.RunCollisions.test_existing_card_branch_makes_its_run_branch_name_taken |
| test_engine_recovery.py::RunCollisions::test_existing_final_branch_makes_its_run_branch_name_taken | contract_recovery.RunCollisions.test_existing_final_branch_makes_its_run_branch_name_taken |
| test_engine_recovery.py::RunCollisions::test_card_branch_the_run_did_not_cut_is_refused | contract_recovery.RunCollisions.test_card_branch_the_run_did_not_cut_is_refused |
| test_engine_recovery.py::RunCollisions::test_new_run_without_worktree_ignores_a_stale_scratch_report | dropped with in-place mode |
| test_engine_recovery.py::CodexStops::test_hung_proxy_over_a_finished_runner_keeps_its_status | contract_recovery.CodexStops.test_hung_proxy_over_a_finished_runner_keeps_its_status |
| test_engine_recovery.py::CodexStops::test_codex_stop_before_the_proxy_ran_prewrites_the_request | contract_recovery.CodexStops.test_codex_stop_before_the_proxy_ran_prewrites_the_request |
| test_engine_recovery.py::Untested::test_reviewer_spent_twice_parks_the_card | contract_recovery.Untested.test_reviewer_spent_twice_parks_the_card |
| test_engine_recovery.py::Untested::test_pinned_vendor_not_installed_parks_the_card | contract_recovery.Untested.test_pinned_vendor_not_installed_parks_the_card |
| test_engine_recovery.py::Untested::test_worker_crash_before_commit_escalates | contract_recovery.Untested.test_worker_crash_before_commit_escalates |
| test_engine_recovery.py::Untested::test_worker_crash_after_commit_keeps_the_commit | contract_recovery.Untested.test_worker_crash_after_commit_keeps_the_commit |
| test_engine_recovery.py::Untested::test_crash_before_result_record_writes_nothing | contract_recovery.Untested.test_crash_before_result_record_writes_nothing |
| test_security.py::H1ForgedCodexBinary::test_runner_never_runs_a_binary_named_in_request_json | contract_security.H1ForgedCodexBinary.test_runner_never_runs_a_binary_named_in_request_json |
| test_security.py::H1ForgedCodexBinary::test_test_override_is_honoured_only_under_leadhand_testing | contract_security.H1ForgedCodexBinary.test_test_override_is_honoured_only_under_leadhand_testing |
| test_security.py::H1ForgedCodexBinary::test_request_json_carries_no_executable | contract_security.H1ForgedCodexBinary.test_request_json_carries_no_executable |
| test_security.py::H2ConfinedDispatch::test_cwd_in_another_repository_is_refused | contract_security.H2ConfinedDispatch.test_cwd_in_another_repository_is_refused |
| test_security.py::H2ConfinedDispatch::test_run_dir_other_than_the_sessions_is_refused | contract_security.H2ConfinedDispatch.test_run_dir_other_than_the_sessions_is_refused |
| test_security.py::H2ConfinedDispatch::test_without_the_session_variable_a_dir_without_run_json_is_refused | contract_security.H2ConfinedDispatch.test_without_the_session_variable_a_dir_without_run_json_is_refused |
| test_security.py::H2ConfinedDispatch::test_without_the_session_variable_a_run_owned_by_a_live_supervisor_is_refused | contract_security.H2ConfinedDispatch.test_without_the_session_variable_a_run_owned_by_a_live_supervisor_is_refused |
| test_security.py::H2ConfinedDispatch::test_an_empty_session_variable_is_refused_not_read_as_unset | contract_security.H2ConfinedDispatch.test_an_empty_session_variable_is_refused_not_read_as_unset |
| test_security.py::H2ConfinedDispatch::test_run_dir_inside_its_own_workspace_is_refused | contract_security.H2ConfinedDispatch.test_run_dir_inside_its_own_workspace_is_refused |
| test_security.py::H2ConfinedDispatch::test_brief_or_report_outside_the_workspace_scratch_dir_is_refused | contract_security.H2ConfinedDispatch.test_brief_or_report_outside_the_workspace_scratch_dir_is_refused |
| test_security.py::H2ConfinedDispatch::test_the_sessions_run_in_its_workspace_is_accepted | contract_security.H2ConfinedDispatch.test_the_sessions_run_in_its_workspace_is_accepted |
| test_security.py::H2ConfinedDispatch::test_the_runs_evidence_base_worktree_is_accepted_as_cwd | contract_security.H2ConfinedDispatch.test_the_runs_evidence_base_worktree_is_accepted_as_cwd |
| test_security.py::M1ConfinedStop::test_stop_dispatch_refuses_a_forged_folder_outside_the_run | contract_security.M1ConfinedStop.test_stop_dispatch_refuses_a_forged_folder_outside_the_run |
| test_security.py::M1ConfinedStop::test_stop_dispatch_refuses_a_dispatch_id_that_leaves_the_workers_folder | contract_security.M1ConfinedStop.test_stop_dispatch_refuses_a_dispatch_id_that_leaves_the_workers_folder |
| test_security.py::M1ConfinedStop::test_stop_dispatch_refuses_a_worker_folder_that_is_a_symlink_out_of_the_run | contract_security.M1ConfinedStop.test_stop_dispatch_refuses_a_worker_folder_that_is_a_symlink_out_of_the_run |
| test_security.py::M1ConfinedStop::test_stop_dispatch_refuses_a_request_for_another_repository | contract_security.M1ConfinedStop.test_stop_dispatch_refuses_a_request_for_another_repository |
| test_security.py::M1ConfinedStop::test_runner_commit_refuses_a_request_for_another_repository | contract_security.M1ConfinedStop.test_runner_commit_refuses_a_request_for_another_repository |
| test_security.py::M1ConfinedStop::test_runner_refuses_a_folder_outside_the_run | contract_security.M1ConfinedStop.test_runner_refuses_a_folder_outside_the_run |
| test_security.py::M1ConfinedStop::test_runner_refuses_a_request_for_another_repository | contract_security.M1ConfinedStop.test_runner_refuses_a_request_for_another_repository |
| test_security.py::H3FilterDrivers::test_the_global_filter_fetches_for_plain_git | contract_security.H3FilterDrivers.test_checkouts_resets_and_stashes_fetch_nothing |
| test_security.py::H3FilterDrivers::test_checkout_reset_and_stash_fetch_nothing | contract_security.H3FilterDrivers.test_checkouts_resets_and_stashes_fetch_nothing |
| test_security.py::H3FilterDrivers::test_runner_git_calls_fetch_nothing | contract_security.H3FilterDrivers.test_runner_commits_lfs_files_as_pointers_and_fetches_nothing |
| test_security.py::H3FilterDrivers::test_every_call_carries_the_environment | contract_security.H3FilterDrivers.test_checkouts_resets_and_stashes_fetch_nothing; contract_security.H3FilterDrivers.test_runner_commits_lfs_files_as_pointers_and_fetches_nothing |
| test_security.py::H3FilterDrivers::test_checked_out_lfs_content_does_not_read_as_modified | contract_security.H3FilterDrivers.test_checked_out_lfs_content_does_not_read_as_modified |
| test_security.py::H3FilterDrivers::test_runner_commits_lfs_files_as_pointers | contract_security.H3FilterDrivers.test_runner_commits_lfs_files_as_pointers_and_fetches_nothing |
| test_security.py::M3SymlinkedBaseWorktree::test_evidence_reset_refuses_a_symlinked_base_worktree | contract_security.M3SymlinkedBaseWorktree.test_evidence_reset_refuses_a_symlinked_base_worktree |
| test_security.py::M3SymlinkedBaseWorktree::test_evidence_reset_refuses_a_symlinked_worktrees_folder | contract_security.M3SymlinkedBaseWorktree.test_evidence_reset_refuses_a_symlinked_worktrees_folder |
| test_security.py::M3SymlinkedBaseWorktree::test_prepare_workspace_refuses_a_symlinked_worktree | contract_security.M3SymlinkedBaseWorktree.test_start_refuses_a_symlinked_worktree |
| test_security.py::L1ReplyFile::test_another_path_is_refused | contract_security.L1ReplyFile.test_another_path_is_refused |
| test_security.py::L1ReplyFile::test_the_reply_file_as_a_symlink_is_refused | contract_security.L1ReplyFile.test_the_reply_file_as_a_symlink_is_refused |
| test_security.py::L1ReplyFile::test_a_symlinked_scratch_folder_is_refused | contract_security.L1ReplyFile.test_a_symlinked_scratch_folder_is_refused |
| test_security.py::L1ReplyFile::test_a_report_behind_a_symlinked_scratch_folder_is_never_copied | contract_security.L1ReplyFile.test_a_report_behind_a_symlinked_scratch_folder_is_never_copied |
| test_security.py::L2SymlinkedScratch::test_a_symlinked_dispatch_folder_is_refused | contract_security.L2SymlinkedScratch.test_a_symlinked_dispatch_folder_is_refused; contract_worktree_safety.ScratchFolders.test_symlinked_scratch_folder_refused_without_writing |
| test_security.py::L2SymlinkedScratch::test_a_symlinked_card_folder_is_refused | contract_security.L2SymlinkedScratch.test_a_symlinked_card_folder_is_refused |
| test_security.py::L2SymlinkedScratch::test_a_file_in_place_of_the_dispatch_folder_is_refused | contract_security.L2SymlinkedScratch.test_a_file_in_place_of_the_dispatch_folder_is_refused |
| test_security.py::L6ToolsInsideTheWorkspace::test_run_refuses_a_cli_inside_the_workspace | contract_security.L6ToolsInsideTheWorkspace.test_run_refuses_a_cli_inside_the_workspace |
| test_security.py::L6ToolsInsideTheWorkspace::test_run_refuses_a_plugin_dir_inside_the_workspace | contract_security.L6ToolsInsideTheWorkspace.test_run_refuses_a_plugin_dir_inside_the_workspace |
| test_security.py::L6ToolsInsideTheWorkspace::test_run_accepts_a_plugin_dir_outside_the_workspace | contract_security.L6ToolsInsideTheWorkspace.test_run_accepts_a_plugin_dir_outside_the_workspace |
| test_security.py::L6StartInPlace::test_start_refuses_an_in_place_run_of_the_repository_holding_the_cli | dropped with in-place mode |
| test_engine_in_place.py::InPlace::test_operators_untracked_files_survive_and_are_never_committed | contract_worktree_safety.OperatorCheckout.test_operator_checkout_untouched_by_a_complete_run |
| test_engine_in_place.py::InPlace::test_undecodable_untracked_filename_survives_start | dropped with in-place mode |
| test_engine_in_place.py::InPlace::test_json_output_escapes_undecodable_names | contract_worktree_safety.WorktreeRewrites.test_json_output_escapes_undecodable_names |
| test_engine_in_place.py::InPlace::test_run_started_in_a_subdirectory_preserves_a_parked_cards_untracked_file | dropped with in-place mode |
| test_engine_in_place.py::InPlace::test_run_started_in_a_subdirectory_leaves_untracked_files_outside_it_alone | dropped with in-place mode |
| test_engine_in_place.py::InPlace::test_docs_dir_elsewhere_in_the_checkout_of_a_subdirectory_run_is_refused | dropped with in-place mode |
| test_engine_in_place.py::InPlace::test_ignored_file_protection_is_for_in_place_runs_only | contract_worktree_safety.WorktreeRewrites.test_an_ignored_file_a_worker_left_does_not_block_a_card_cut |
| test_engine_in_place.py::Exclusive::test_second_in_place_run_is_refused_while_the_first_is_unfinished | dropped with in-place mode |
| test_engine_in_place.py::Exclusive::test_in_place_start_is_refused_while_a_run_in_another_runs_folder_is_supervised | dropped with in-place mode |
| test_engine_in_place.py::Exclusive::test_in_place_start_from_a_subdirectory_is_refused_while_the_checkout_is_supervised | dropped with in-place mode |
| test_engine_in_place.py::Exclusive::test_in_place_start_from_a_subdirectory_is_refused_while_a_run_is_unfinished | dropped with in-place mode |
| test_engine_in_place.py::Exclusive::test_a_checkout_that_cannot_be_flocked_refuses_an_in_place_start_clearly | dropped with in-place mode |
| test_engine_in_place.py::Exclusive::test_start_returns_the_unfinished_run_while_its_own_supervisor_holds_the_checkout | dropped with in-place mode |
| test_engine_in_place.py::Exclusive::test_worktree_runs_are_not_held_back_by_an_in_place_run | dropped with in-place mode |
| test_gitops.py::GitopsTests::test_prepare_workspace_without_worktree_uses_checkout | dropped with in-place mode |
| test_gitops.py::GitopsTests::test_cut_card_branch_refuses_an_existing_branch_the_run_did_not_record | contract_worktree_safety.UnrecordedCardBranch.test_unrecorded_card_branch_adopted_only_at_run_tip |
| test_gitops.py::GitopsTests::test_fast_forward_moves_a_run_branch_checked_out_in_the_workspace | contract_worktree_safety.WorktreeRewrites.test_accept_moves_a_run_branch_checked_out_in_the_worktree |
| test_gitops.py::GitopsTests::test_prepare_workspace_without_worktree_refuses_uncommitted_changes | dropped with in-place mode |
| test_gitops.py::GitopsTests::test_default_base_after_a_run_without_worktree | dropped with in-place mode |
| test_gitops.py::SubdirectoryWorkspaceTests::test_stage_work_takes_a_new_file_in_the_subdirectory | dropped with in-place mode |
| test_gitops.py::SubdirectoryWorkspaceTests::test_stage_work_leaves_out_a_gitlink_in_the_subdirectory | dropped with in-place mode |
| test_gitops.py::SubdirectoryWorkspaceTests::test_preserve_keeps_and_removes_untracked_files_named_from_the_top | dropped with in-place mode |
| test_gitops.py::SubdirectoryWorkspaceTests::test_preserve_leaves_untracked_files_outside_the_workspace_alone | dropped with in-place mode |
| test_gitops.py::SubdirectoryWorkspaceTests::test_stage_work_never_takes_a_new_file_outside_the_workspace | dropped with in-place mode |
| test_gitops.py::SubdirectoryWorkspaceTests::test_cleanliness_probes_see_the_whole_checkout_whatever_diff_relative_says | dropped with in-place mode |
| test_gitops.py::SubdirectoryWorkspaceTests::test_patches_keep_the_standard_prefixes_whatever_the_users_diff_settings | contract_worktree_safety.WorktreeRewrites.test_check_diff_patch_keeps_the_standard_prefixes_whatever_the_repo_diff_settings |
| test_gitops.py::SubdirectoryWorkspaceTests::test_preserve_never_removes_the_workspace_folder_itself | dropped with in-place mode |
| test_gitops.py::OperatorFilesInPlaceTests::test_start_refuses_a_base_that_would_overwrite_an_ignored_file | dropped with in-place mode |
| test_gitops.py::OperatorFilesInPlaceTests::test_switching_to_a_card_branch_never_overwrites_an_ignored_file | dropped with in-place mode |
| test_gitops.py::OperatorFilesInPlaceTests::test_fast_forward_never_overwrites_an_ignored_file | dropped with in-place mode |
| test_gitops.py::OperatorFilesInPlaceTests::test_rebase_never_overwrites_an_ignored_file | dropped with in-place mode |
| test_gitops.py::OperatorFilesInPlaceTests::test_rebase_never_overwrites_an_ignored_file_an_intermediate_commit_adds | dropped with in-place mode |
| test_gitops.py::OperatorFilesInPlaceTests::test_rebase_never_replaces_an_ignored_file_with_a_folder | dropped with in-place mode |
| test_gitops.py::OperatorFilesInPlaceTests::test_start_refuses_a_folder_over_an_ignored_file_before_changing_anything | dropped with in-place mode |
| test_gitops.py::OperatorFilesInPlaceTests::test_a_folder_of_tracked_files_a_rebase_turns_into_a_file_is_not_in_the_way | dropped with in-place mode |
| test_gitops.py::OperatorFilesInPlaceTests::test_rebase_never_replaces_a_folder_holding_an_ignored_file_with_a_file | dropped with in-place mode |
| test_gitops.py::OperatorFilesInPlaceTests::test_a_fold_equal_name_that_is_another_file_is_still_in_the_way | dropped with in-place mode |
| test_gitops.py::OperatorFilesInPlaceTests::test_a_case_only_rename_is_not_in_the_way_on_a_case_insensitive_file_system | dropped with in-place mode |
| test_gitops.py::OperatorFilesInPlaceTests::test_a_worktree_run_may_overwrite_an_ignored_file_it_holds | contract_worktree_safety.WorktreeRewrites.test_an_ignored_file_a_worker_left_does_not_block_a_card_cut |
| test_gitops.py::OperatorFilesInPlaceTests::test_a_conflicted_merge_is_refused_as_uncommitted_changes | dropped with in-place mode |
| test_gitops.py::OperatorFilesInPlaceTests::test_a_sparse_checkout_is_refused_in_place_with_short_advice | contract_worktree_safety.SparseCheckouts.test_sparse_checkout_refused_by_check_start_and_run |
| test_gitops.py::OperatorFilesInPlaceTests::test_start_refuses_hidden_index_entries_in_place | dropped with in-place mode |
