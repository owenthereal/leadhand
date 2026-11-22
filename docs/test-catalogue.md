# Test catalogue

A history map: each unit test of the removed Python implementation, as
`<file>::<Class>::<test>` (its file's name without the extension), and where its behaviour is
tested now. A disposition is one of:

- one or more Rust integration tests, as `<file>::<path>`, the test function at that path in
  `tests/<file>.rs`, separated by `; `;
- `internal: Rust unit test (<module>)`, a behaviour with no effect outside the binary, tested
  by that module's unit tests in `src/` (many carry a `// CATALOG: <key>` marker);
- `dropped with in-place mode`, a behaviour of in-place runs, which the Rust binary refuses;
- `plugin file check (plugin_files)`, a check of the plugin files in `tests/plugin_files.rs`;
- `reference-only (removed with the Python reference)`, where the converting test pinned a
  Python behaviour that the Rust binary changes on purpose; it left with the Python, and has
  no Rust form.

`tests/catalogue.rs` checks every row: a well-formed key, no key twice, and every Rust test a
row names exists.

| Python test | Where it is tested |
|---|---|
| test_agents::AgentTests::test_agent_frontmatter_matches_table | plugin file check (plugin_files) |
| test_agents::AgentTests::test_codex_proxy_runs_one_command | plugin file check (plugin_files) |
| test_agents::AgentTests::test_contract_carriers_contain_contract_verbatim | plugin file check (plugin_files) |
| test_agents::AgentTests::test_reviewer_rules | plugin file check (plugin_files) |
| test_agents::AgentTests::test_verifier_evidence_rule_follows_the_spec | plugin file check (plugin_files) |
| test_cli::CliTests::test_unknown_command_exits_2 | cli::usage::test_unknown_command_exits_2 |
| test_cli::CliTests::test_version_prints_package_version | cli::version::test_prints_package_version |
| test_supervisor_files::SupervisorFileTests::test_run_skill_commands_match_cli | cli::plugin_commands::test_every_cli_command_in_the_run_skill_is_a_registered_command; plugin_files::supervisor_files::test_run_skill_cli_calls_are_plain_commands |
| test_supervisor_files::SupervisorFileTests::test_run_skill_covers_every_action | plugin file check (plugin_files) |
| test_supervisor_files::SupervisorFileTests::test_run_skill_covers_run_level_dispatches | plugin file check (plugin_files) |
| test_supervisor_files::SupervisorFileTests::test_run_skill_records_started_before_waiting | plugin file check (plugin_files) |
| test_supervisor_files::SupervisorFileTests::test_supervisor_frontmatter | plugin file check (plugin_files) |
| test_deck::DeckTests::test_parse_basic_fixture | deck::briefs::test_basic_fixture_cards_reach_the_brief |
| test_deck::DeckTests::test_spec_example_header_parses | internal: Rust unit test (deck) |
| test_deck::DeckTests::test_hard_yes_requires_reason | deck::deck::test_hard_yes_requires_reason |
| test_deck::DeckTests::test_hard_yes_with_reason_forms | deck::deck::test_hard_yes_with_reason_forms; deck::briefs::test_hard_yes_reason_is_rendered |
| test_deck::DeckTests::test_fields_after_context_are_parsed | deck::deck::test_fields_after_context_are_parsed; deck::briefs::test_fields_after_context_are_parsed |
| test_deck::DeckTests::test_unknown_key_bullet_is_context | deck::briefs::test_unknown_key_bullet_is_context |
| test_deck::DeckTests::test_unknown_key_bullet_warns | deck::deck::test_unknown_key_bullet_warns |
| test_deck::DeckTests::test_scout_card_rejected_in_this_version | deck::deck::test_scout_card_rejected_in_this_version |
| test_deck::DeckTests::test_unsupported_config_reported_as_errors | deck::deck::test_unsupported_config_reported_as_errors |
| test_deck::DeckTests::test_dependency_cycle_is_error | deck::deck::test_dependency_cycle_is_error |
| test_deck::DeckTests::test_unknown_dependency_and_duplicates_and_finish_and_build | deck::deck::test_unknown_dependency_and_duplicates_and_finish_and_build |
| test_deck::DeckTests::test_requirement_to_missing_card_is_error | deck::deck::test_requirement_to_missing_card_is_error |
| test_deck::DeckTests::test_missing_required_fields_are_errors | deck::deck::test_missing_required_fields_are_errors |
| test_deck::DeckTests::test_dependency_order_is_stable_topological | internal: Rust unit test (deck) |
| test_deck::DeckTests::test_route_pin_parsed | deck::deck::test_route_pin_parsed |
| test_deck::DeckTests::test_digest_changes_when_file_changes | internal: Rust unit test (deck) |
| test_deck::DeckTests::test_parse_error_reports_line_number | deck::deck::test_parse_error_reports_line_number |
| test_deck::DeckTests::test_uncovered_requirement_is_warning | deck::deck::test_uncovered_requirement_is_warning |
| test_deck::DeckTests::test_base_commit_and_paths | deck::deck::test_base_commit_and_paths |
| test_deck::DeckTests::test_check_cli_exits_1_on_error | deck::deck::test_check_cli_exits_1_on_error |
| test_config::ConfigTests::test_default_preset_matches_spec | config::config::test_default_preset_matches_spec |
| test_config::ConfigTests::test_repo_file_overrides_user_file_key_by_key | config::config::test_repo_file_overrides_user_file_key_by_key |
| test_config::ConfigTests::test_preset_key_selects_claude_only | config::config::test_preset_key_selects_claude_only |
| test_config::ConfigTests::test_invalid_effort_raises_config_error_naming_key | config::config::test_invalid_effort_raises_config_error_naming_key |
| test_config::ConfigTests::test_unknown_top_level_key_raises | config::config::test_unknown_top_level_key_raises |
| test_config::ConfigTests::test_docs_dir_expands_repo_and_home | config::config::test_docs_dir_expands_repo_and_home |
| test_config::ConfigTests::test_after_run_step_validation | config::config::test_after_run_step_validation |
| test_config::ConfigTests::test_unsupported_settings_lists_each_rejection | config::config::test_unsupported_settings_lists_each_rejection |
| test_config::ConfigTests::test_step_limits_claude_workers_cannot_honour_are_rejected | config::config::test_step_limits_claude_workers_cannot_honour_are_rejected |
| test_config::ConfigTests::test_step_limits_follow_the_roles_routes_and_agent_files | internal: Rust unit test (config) |
| test_config::ConfigTests::test_parse_route_and_leadhand_config_env | config::config::test_leadhand_config_env_names_the_user_file |
| test_config_snapshot::Snapshot::test_snapshot_round_trips | internal: Rust unit test (config) |
| test_config_snapshot::Snapshot::test_start_writes_the_snapshot_and_the_engine_reads_only_it | config::snapshot::test_start_writes_the_snapshot_and_the_engine_reads_only_it |
| test_config_snapshot::Snapshot::test_worker_editing_the_repo_config_changes_nothing | config::snapshot::test_worker_editing_the_repo_config_changes_nothing; reference-only (removed with the Python reference) |
| test_route::RouteTests::test_standard_implementer_uses_codex_terra_high | route::route::test_standard_implementer_uses_codex_terra_high |
| test_route::RouteTests::test_hard_card_uses_implementer_hard | route::route::test_hard_card_uses_implementer_hard |
| test_route::RouteTests::test_reviewer_other_vendor_than_implementer | route::route::test_reviewer_other_vendor_than_implementer |
| test_route::RouteTests::test_high_risk_reviewer_uses_reviewer_hard_plus_one | route::route::test_high_risk_reviewer_uses_reviewer_hard_plus_one |
| test_route::RouteTests::test_security_uses_reviewer_hard | route::security_gate::test_security_uses_reviewer_hard |
| test_route::RouteTests::test_profile_lean_lowers_every_role | route::route::test_profile_lean_lowers_every_role |
| test_route::RouteTests::test_pin_wins_for_first_two_attempts | route::route::test_pin_wins_for_first_two_attempts |
| test_route::RouteTests::test_ladder_attempts_1_to_4_then_exhausted | route::route::test_ladder_attempts_1_to_4_then_exhausted |
| test_route::RouteTests::test_attempt_4_with_prefer_other_switches_from_attempt_3s_actual_vendor | route::route::test_attempt_4_with_prefer_other_switches_from_attempt_3s_actual_vendor |
| test_route::RouteTests::test_claude_only_install_exhausts_at_attempt_4 | route::route::test_claude_only_install_exhausts_at_attempt_4 |
| test_route::RouteTests::test_effort_clamped_to_xhigh_unless_pinned_max | route::route::test_effort_clamped_to_xhigh_unless_pinned_max |
| test_route::RouteTests::test_pin_requires_installed_vendor | route::route::test_pin_requires_installed_vendor |
| test_route::RouteTests::test_invalid_profile_is_config_error | route::route::test_invalid_profile_is_config_error |
| test_route::RouteTests::test_low_pin_not_shifted_by_profile_or_risk | route::route::test_low_pin_not_shifted_by_profile_or_risk |
| test_route::RouteTests::test_cli_route_prints_route_and_reasons | route::route::test_cli_route_prints_route_and_reasons |
| test_unsupported_layouts::Unsupported::test_docs_dir_inside_the_repo_is_refused | layouts::unsupported::test_docs_dir_inside_the_repo_is_refused |
| test_unsupported_layouts::Unsupported::test_relative_docs_dir_is_resolved_against_the_repo | layouts::unsupported::test_relative_docs_dir_is_resolved_against_the_repo |
| test_unsupported_layouts::Unsupported::test_defaults_are_supported | layouts::unsupported::test_defaults_are_supported |
| test_status::StatusTests::test_status_headline_while_implementing | status::status::test_status_headline_while_implementing; status::checkpoints::test_checkpoint_writes_status_json |
| test_status::StatusTests::test_status_headline_while_reviewing | status::status::test_status_headline_while_reviewing |
| test_status::StatusTests::test_status_attention_lists_parked_cards_and_decisions | status::status::test_status_attention_lists_parked_cards_and_decisions |
| test_status::StatusTests::test_ended_run_states | status::status::test_ended_run_states |
| test_status::StatusTests::test_status_cli_without_status_json | status::status::test_status_cli_without_status_json |
| test_status::StatusTests::test_status_prefers_status_json_when_present | status::checkpoints::test_checkpoint_writes_status_json |
| test_status::StatusTests::test_corrupt_event_line_is_an_attention_line_not_a_failure | status::status::test_corrupt_event_line_is_an_attention_line_not_a_failure |
| test_status::StatusTests::test_partial_last_line_is_ignored | status::status::test_partial_last_line_is_ignored |
| test_status::StatusTests::test_usage_comes_from_charges_and_excludes_nothing_by_role | status::status::test_usage_comes_from_charges_and_excludes_nothing_by_role |
| test_status::StatusTests::test_codex_meter_reads_latest_record | status::status::test_codex_meter_reads_latest_record |
| test_status::StatusTests::test_no_run_dir_picks_latest_unfinished_run | reference-only (removed with the Python reference) |
| test_status::FixRoundTests::test_attention_lists_violations_with_preserved_ref_and_next_action | status::fix_round::test_attention_lists_violations_with_preserved_ref_and_next_action |
| test_status::FixRoundTests::test_meter_before_is_taken_once_even_when_null | status::checkpoints::test_meter_before_is_taken_once_even_when_null |
| test_status::FixRoundTests::test_meter_is_not_scanned_at_every_checkpoint | status::checkpoints::test_meter_is_not_scanned_at_every_checkpoint |
| test_status::FixRoundTests::test_meter_scan_is_capped_to_newest_files | status::fix_round::test_meter_scan_is_capped_to_newest_files |
| test_status::FixRoundTests::test_unreadable_log_is_an_attention_line | status::fix_round::test_unreadable_log_is_an_attention_line |
| test_status::EngineStatusTests::test_engine_checkpoints_write_status_json_and_survive_status_errors | status::checkpoints::test_engine_checkpoints_write_status_json_and_survive_status_errors |
| test_report::ReportTests::test_report_outcome_is_line_3_after_title | report::report::test_report_outcome_is_line_3_after_title |
| test_report::ReportTests::test_outcome_qualifier | report::report::test_outcome_qualifier |
| test_report::ReportTests::test_report_sections_in_order | report::report::test_report_sections_in_order |
| test_report::ReportTests::test_incomplete_report_puts_parked_cards_under_attention | report::report::test_incomplete_report_puts_parked_cards_under_attention |
| test_report::ReportTests::test_report_usage_includes_supervisor_and_proxy_and_unknown_count | report::report::test_report_usage_includes_supervisor_and_proxy_and_unknown_count |
| test_report::ReportTests::test_routing_counts_worker_attempts_not_proxy | report::report::test_routing_counts_worker_attempts_not_proxy |
| test_report::ReportTests::test_write_report_and_cli | report::report::test_write_report_and_cli |
| test_report::FixRoundTests::test_unresolved_lists_parked_cards_with_branches_and_open_findings | report::fix_round::test_unresolved_lists_parked_cards_with_branches_and_open_findings |
| test_report::FixRoundTests::test_attention_has_next_action_for_parked_cards | report::fix_round::test_attention_has_next_action_for_parked_cards |
| test_report::FixRoundTests::test_evidence_shows_last_result_per_gate_and_marks_stale | report::fix_round::test_evidence_shows_last_result_per_gate_and_marks_stale |
| test_report::FixRoundTests::test_report_is_written_atomically_without_leftover_temp_files | report::fix_round::test_report_is_written_atomically_without_leftover_temp_files |
| test_report::FixRoundTests::test_world_is_hermetic_about_codex_home | report::fix_round::test_world_is_hermetic_about_codex_home |
| test_dispatch::ArgvTests::test_argv_passes_network_false_explicitly | dispatch::codex_command_line::test_argv_passes_network_false_explicitly |
| test_dispatch::ArgvTests::test_argv_network_true_when_enabled | dispatch::codex_command_line::test_argv_network_true_when_enabled |
| test_dispatch::ArgvTests::test_argv_workspace_write_with_git_common_dir_and_effort | dispatch::codex_command_line::test_argv_workspace_write_with_git_common_dir_and_effort |
| test_dispatch::ArgvTests::test_argv_scout_is_read_only | dispatch::codex_command_line::test_argv_scout_is_read_only |
| test_dispatch::ArgvTests::test_argv_sets_blocking_env_through_shell_policy | dispatch::codex_command_line::test_argv_sets_blocking_env_through_shell_policy |
| test_dispatch::ArgvTests::test_evidence_worker_may_write_the_evidence_base_worktree_only | dispatch::codex_command_line::test_evidence_worker_may_write_the_evidence_base_worktree_only |
| test_dispatch::ArgvTests::test_evidence_base_worktree_must_be_registered_and_reached_through_no_symlink | dispatch::codex_command_line::test_evidence_base_worktree_must_be_registered_and_reached_through_no_symlink |
| test_dispatch::ArgvTests::test_resume_argv_matches_findings | dispatch::codex_command_line::test_resume_argv_matches_findings |
| test_dispatch::ResultTests::test_ready_reply_parsed_and_attempt_recorded | dispatch::results::test_ready_reply_parsed_and_attempt_recorded |
| test_dispatch::ResultTests::test_reply_without_status_is_blocked_and_raw_kept | dispatch::results::test_reply_without_status_is_blocked_and_raw_kept |
| test_dispatch::ResultTests::test_step_limit_stops_process_and_marks_spent | dispatch::results::test_step_limit_stops_process_and_marks_spent |
| test_dispatch::ResultTests::test_time_limit_stops_process_and_marks_spent | dispatch::results::test_time_limit_stops_process_and_marks_spent |
| test_dispatch::ResultTests::test_usage_limit_is_blocked_by_usage_limit | dispatch::results::test_usage_limit_is_blocked_by_usage_limit |
| test_dispatch::ResultTests::test_killed_worker_usage_from_session_or_unknown | dispatch::usage::test_killed_worker_usage_from_session_or_unknown |
| test_dispatch::ResultTests::test_missing_codex_binary_is_blocked_environment | dispatch::results::test_missing_codex_binary_is_blocked_environment |
| test_dispatch::ResultTests::test_paths_with_spaces | dispatch::results::test_paths_with_spaces |
| test_dispatch::OwnershipTests::test_runner_survives_foreground_death_and_records_result | dispatch::runner_lifecycle::test_runner_survives_foreground_death_and_records_result |
| test_dispatch::OwnershipTests::test_second_dispatch_attaches_to_live_runner | dispatch::runner_lifecycle::test_second_dispatch_attaches_to_live_runner |
| test_dispatch::OwnershipTests::test_repeated_dispatch_returns_existing_result | dispatch::runner_lifecycle::test_repeated_dispatch_returns_existing_result |
| test_dispatch::OwnershipTests::test_cli_prints_reply_and_session_or_still_running | dispatch::runner_lifecycle::test_cli_prints_reply_and_session_or_still_running; dispatch::runner_lifecycle::test_foreground_wait_ends_with_exit_75_and_the_still_running_line |
| test_dispatch::CancelTests::test_signal_only_after_pid_and_start_time_match | dispatch::cancellation::test_signal_only_after_pid_and_start_time_match |
| test_dispatch::CancelTests::test_cancel_detached_worker_stops_every_process_and_writes_nothing_after | dispatch::cancellation::test_cancel_detached_worker_stops_every_process_and_writes_nothing_after |
| test_dispatch::CancelTests::test_stale_heartbeat_on_live_runner_is_cancelled_not_replaced_alongside | dispatch::cancellation::test_stale_heartbeat_on_live_runner_is_cancelled_not_replaced_alongside |
| test_dispatch::CancelTests::test_orphaned_codex_group_is_terminated_before_finalizing | dispatch::process_control::test_orphaned_codex_group_is_terminated_before_finalizing |
| test_dispatch::EscapedDescendantTests::test_double_forked_child_is_stopped_before_finalizing | dispatch::process_control::test_double_forked_child_is_stopped_before_finalizing |
| test_dispatch::FixRoundTests::test_run_id_passed_through_shell_policy | dispatch::codex_command_line::test_run_id_passed_through_shell_policy |
| test_dispatch::FixRoundTests::test_stream_usage_is_charged_when_session_missing | dispatch::usage::test_stream_usage_is_charged_when_session_missing |
| test_dispatch::FixRoundTests::test_usage_limit_text_does_not_override_clean_ready_reply | dispatch::results::test_usage_limit_text_does_not_override_clean_ready_reply |
| test_dispatch::FixRoundTests::test_runner_exception_is_blocked_environment | internal: Rust unit test (dispatch) |
| test_dispatch::FixRoundTests::test_unknown_codex_start_time_still_enforced | internal: Rust unit test (dispatch) |
| test_dispatch::FixRoundTests::test_runner_record_without_start_time_is_not_dead_while_lock_held | dispatch::runner_lifecycle::test_runner_record_without_start_time_is_not_dead_while_lock_held |
| test_dispatch::FixRoundTests::test_launch_intent_recorded_before_codex_starts | dispatch::runner_lifecycle::test_launch_intent_recorded_before_codex_starts |
| test_dispatch::FixRoundTests::test_launch_rechecks_cancel_immediately_before_starting | internal: Rust unit test (dispatch) |
| test_dispatch::FixRoundTests::test_cancel_requested_during_orphan_is_not_resurrected | dispatch::cancellation::test_cancel_requested_during_orphan_is_not_resurrected |
| test_dispatch::FixRoundTests::test_ready_result_landing_during_replacement_cancel_is_kept | dispatch::runner_lifecycle::test_ready_result_landing_during_replacement_cancel_is_kept |
| test_dispatch::FixRoundTests::test_heartbeat_refreshed_before_second_look_is_not_hung | dispatch::runner_lifecycle::test_heartbeat_refreshed_before_second_look_is_not_hung |
| test_dispatch::FixRoundTests::test_concurrent_dispatches_start_one_runner | dispatch::runner_lifecycle::test_concurrent_dispatches_start_one_runner |
| test_dispatch::FixRoundTests::test_second_runner_refuses_while_first_holds_lock | dispatch::runner_lifecycle::test_second_runner_refuses_while_first_holds_lock |
| test_dispatch::RunnerCommitTests::test_runner_commits_codex_work_on_card_branch | dispatch::runner_commits::test_runner_commits_codex_work_on_card_branch |
| test_dispatch::RunnerCommitTests::test_runner_commits_partial_work_when_spent | dispatch::runner_commits::test_runner_commits_partial_work_when_spent |
| test_dispatch::RunnerCommitTests::test_commit_intent_restart_does_not_double_commit | dispatch::runner_commits::test_commit_intent_restart_after_the_commit_does_not_commit_again; dispatch::runner_commits::test_commit_intent_restart_before_the_commit_still_commits |
| test_dispatch::RunnerCommitTests::test_runner_commits_a_new_file_in_a_subdirectory_workspace | dropped with in-place mode |
| test_dispatch::RunnerCommitTests::test_untracked_to_take_fences_the_run_folder_from_the_checkout_top | dropped with in-place mode |
| test_dispatch::RunnerCommitTests::test_untracked_to_take_keeps_only_paths_inside_a_subdirectory_workspace | dropped with in-place mode |
| test_dispatch::RunnerCommitTests::test_untracked_to_take_fails_closed_when_the_checkout_top_is_unknown | internal: Rust unit test (dispatch) |
| test_dispatch::RunnerCommitTests::test_runner_commit_leaves_untracked_files_outside_a_subdirectory_workspace | dropped with in-place mode |
| test_dispatch::RunnerCommitTests::test_a_fifo_left_at_the_report_never_hangs_the_commit | dispatch::runner_commits::test_a_fifo_left_at_the_report_never_hangs_the_commit |
| test_dispatch::RunnerCommitTests::test_runner_commits_an_edit_hidden_behind_an_index_flag | dispatch::runner_commits::test_runner_commits_an_edit_hidden_behind_an_index_flag |
| test_dispatch::RunnerCommitTests::test_failed_commit_is_blocked_environment_with_reason | dispatch::runner_commits::test_failed_commit_is_blocked_environment_with_reason |
| test_dispatch::RunnerCommitTests::test_unlocatable_git_path_is_an_error_not_an_operation | internal: Rust unit test (dispatch) |
| test_dispatch::RunnerCommitTests::test_commit_respects_the_cancel_deadline | dispatch::runner_commits::test_commit_respects_the_cancel_deadline |
| test_dispatch::RunnerCommitTests::test_externally_killed_codex_is_possibly_incomplete_and_partial_work_committed | dispatch::runner_commits::test_externally_killed_codex_is_possibly_incomplete_and_partial_work_committed |
| test_dispatch::RunnerCommitTests::test_exit_without_final_turn_completed_is_possibly_incomplete | dispatch::runner_commits::test_exit_without_final_turn_completed_is_possibly_incomplete |
| test_dispatch::RunnerCommitTests::test_complete_run_is_not_possibly_incomplete | dispatch::runner_commits::test_complete_run_is_not_possibly_incomplete |
| test_dispatch::RunnerCommitTests::test_wrong_branch_is_not_committed | dispatch::runner_commits::test_wrong_branch_is_not_committed |
| test_dispatch::RunnerCommitTests::test_operation_in_progress_is_not_committed | dispatch::runner_commits::test_operation_in_progress_is_not_committed |
| test_dispatch::RunnerCommitTests::test_check_dispatch_is_not_committed | dispatch::runner_commits::test_check_dispatch_is_not_committed |
| test_dispatch::RunnerCommitTests::test_cli_passes_kind_and_branch | dispatch::runner_commits::test_cli_passes_kind_and_branch |
| test_worker_env::WorkerEnvTests::test_worker_env_removes_tokens_and_sets_empty_gh_config | worker_env::worker_environment::test_worker_env_removes_tokens_and_sets_empty_gh_config |
| test_worker_env::WorkerEnvTests::test_existing_git_config_env_entries_preserved | worker_env::worker_environment::test_existing_git_config_env_entries_preserved |
| test_worker_env::WorkerEnvTests::test_ssh_push_fails_for_ssh_scp_and_explicit_pushurl | worker_env::worker_environment::test_ssh_push_fails_for_ssh_scp_and_explicit_pushurl |
| test_worker_env::WorkerEnvTests::test_https_push_without_credentials_fails_without_prompting | worker_env::worker_environment::test_https_push_without_credentials_fails_without_prompting |
| test_worker_env::WorkerEnvTests::test_configured_askpass_is_not_run | worker_env::worker_environment::test_configured_askpass_is_not_run; worker_env::worker_environment::test_existing_git_config_parameters_kept_and_askpass_cleared |
| test_worker_env::WorkerEnvTests::test_gh_reports_no_login_with_parent_token | worker_env::worker_environment::test_gh_reports_no_login_with_parent_token |
| test_worker_env::WorkerEnvTests::test_local_path_push_still_allowed | worker_env::worker_environment::test_local_path_push_still_allowed |
| test_usage::ArithmeticTests::test_add_sums_fields_and_known_is_conjunction | internal: Rust unit test (usage) |
| test_usage::ArithmeticTests::test_subtract_is_field_wise_and_floored_at_zero | internal: Rust unit test (usage) |
| test_usage::ArithmeticTests::test_estimate_cost_opus_matches_recorded_session | usage::pricing::test_estimate_cost_opus_matches_recorded_session |
| test_usage::ArithmeticTests::test_estimate_cost_unpriced_model_is_none | usage::pricing::test_estimate_cost_unpriced_model_is_none; usage::codex_charges::test_codex_cumulative_takes_last_token_count_and_maps_fields |
| test_usage::LocatorTests::test_claude_transcript_locators | usage::locators::test_claude_transcript_locators; usage::locators::test_wildcard_worker_id_finds_no_transcript |
| test_usage::LocatorTests::test_missing_projects_dir_is_none | usage::locators::test_missing_projects_dir_is_none |
| test_usage::ClaudeUsageTests::test_claude_usage_dedupes_by_message_id | usage::claude_transcripts::test_claude_usage_dedupes_by_message_id |
| test_usage::ClaudeUsageTests::test_claude_usage_skips_excluded_ids | usage::claude_transcripts::test_claude_usage_skips_messages_already_charged |
| test_usage::ClaudeUsageTests::test_claude_usage_ignores_partial_and_garbage_lines | usage::claude_transcripts::test_claude_usage_ignores_partial_and_garbage_lines |
| test_usage::ChargeClaudeTests::test_charge_claude_resumed_worker_counts_each_message_once | usage::claude_charges::test_charge_claude_resumed_worker_counts_each_message_once |
| test_usage::ChargeClaudeTests::test_charge_record_is_durable_and_round_trips | usage::claude_charges::test_charge_record_is_durable_and_round_trips |
| test_usage::ChargeClaudeTests::test_charge_tops_up_message_charged_mid_stream | usage::claude_charges::test_charge_tops_up_message_charged_mid_stream |
| test_usage::ChargeClaudeTests::test_charge_none_path_is_unknown_and_relative_path_is_rejected | usage::claude_charges::test_charge_missing_file_is_unknown_and_moves_no_checkpoint; usage::codex_charges::test_charge_none_thread_is_unknown; usage::locators::test_relative_transcript_path_is_rejected |
| test_usage::ChargeClaudeTests::test_repeated_charge_id_is_harmless | usage::claude_charges::test_repeated_charge_id_is_harmless; usage::codex_charges::test_runner_crash_after_charge_keeps_the_charge |
| test_usage::ChargeClaudeTests::test_charge_missing_file_is_unknown_and_moves_no_checkpoint | usage::claude_charges::test_charge_missing_file_is_unknown_and_moves_no_checkpoint; usage::codex_charges::test_codex_cumulative_unknown_when_missing_or_without_token_count |
| test_usage::ChargeClaudeTests::test_totals_group_by_role_dispatch_and_source | usage::claude_charges::test_totals_group_by_role_dispatch_and_source |
| test_usage::CodexTests::test_codex_cumulative_takes_last_token_count_and_maps_fields | usage::codex_charges::test_codex_cumulative_takes_last_token_count_and_maps_fields |
| test_usage::CodexTests::test_codex_cumulative_unknown_when_missing_or_without_token_count | usage::codex_charges::test_codex_cumulative_unknown_when_missing_or_without_token_count |
| test_usage::CodexTests::test_charge_codex_cumulative_deltas | usage::codex_charges::test_charge_codex_cumulative_deltas |
| test_usage::CodexTests::test_codex_stream_fallback_charged_when_session_missing | usage::codex_charges::test_codex_stream_fallback_charged_when_session_missing |
| test_usage::CodexTests::test_codex_stream_usage_sums_turn_completed | usage::codex_charges::test_codex_stream_usage_sums_turn_completed |
| test_usage::CodexRolloutFixtureTests::test_rollout_charges_every_response_not_the_token_count_total | usage::codex_rollout::test_rollout_charges_every_response_not_the_token_count_total |
| test_usage::CodexRolloutFixtureTests::test_rollout_charged_twice_charges_nothing_new | usage::codex_rollout::test_rollout_charged_twice_charges_nothing_new |
| test_usage::CodexRolloutFixtureTests::test_rollout_truncated_before_resume_charges_only_the_killed_turn | usage::codex_rollout::test_rollout_truncated_before_resume_charges_only_the_killed_turn |
| test_usage::CodexRolloutFixtureTests::test_rollout_repeated_response_id_counts_once | usage::codex_rollout::test_rollout_repeated_response_id_counts_once |
| test_usage::CodexRolloutFixtureTests::test_rollout_thread_total_mismatch_is_recorded_not_fatal | usage::codex_rollout::test_rollout_thread_total_mismatch_is_recorded_not_fatal |
| test_usage::CodexRolloutFixtureTests::test_possibly_incomplete_flag_is_recorded_on_the_charge | usage::codex_rollout::test_possibly_incomplete_flag_is_recorded_on_the_charge |
| test_usage::CodexRolloutFixtureTests::test_rollout_without_any_usable_record_is_unknown | usage::codex_rollout::test_rollout_without_any_usable_record_is_unknown |
| test_usage::CodexRolloutFixtureTests::test_token_count_is_only_a_fallback_for_a_rollout_without_records | usage::codex_rollout::test_token_count_is_only_a_fallback_for_a_rollout_without_records |
| test_usage::CodexStreamFixtureTests::test_stream_usage_sums_turn_completed | usage::codex_stream::test_stream_usage_sums_turn_completed |
| test_usage::CodexStreamFixtureTests::test_stream_cache_write_tokens_are_kept_as_five_minute_writes | usage::codex_stream::test_stream_cache_write_tokens_are_kept_as_five_minute_writes |
| test_usage::ClaudeFixtureTests::test_stopped_transcript_charges_its_one_message_once | usage::claude_transcripts::test_stopped_transcript_charges_its_one_message_once |
| test_usage::ClaudeFixtureTests::test_resumed_transcript_charges_each_message_once | usage::claude_transcripts::test_resumed_transcript_charges_each_message_once |
| test_usage::ClaudeFixtureTests::test_resumed_file_growth_is_charged_once | usage::claude_transcripts::test_resumed_file_growth_is_charged_once |
| test_usage::CrashAndConcurrencyTests::test_crash_after_charge_before_attempt_keeps_exact_total | usage::claude_charges::test_crash_after_charge_before_attempt_keeps_exact_total |
| test_usage::CrashAndConcurrencyTests::test_concurrent_supervisor_and_runner_charges_keep_exact_totals | usage::concurrency::test_concurrent_supervisor_and_runner_charges_keep_exact_totals |
| test_usage_models::PerModel::test_mixed_batch_is_priced_per_model | usage::pricing::test_mixed_batch_is_priced_per_model |
| test_usage_models::PerModel::test_single_model_and_empty_synthetic_records_stay_single | usage::pricing::test_single_model_and_empty_synthetic_records_stay_single |
| test_usage_models::PerModel::test_unpriced_part_makes_the_estimate_unknown | usage::pricing::test_unpriced_part_makes_the_estimate_unknown |
| test_usage_models::PerModel::test_charge_keeps_the_per_model_parts | usage::pricing::test_charge_keeps_the_per_model_parts |
| test_procs::ProcsTests::test_own_process_info | internal: Rust unit test (procs) |
| test_procs::ProcsTests::test_start_time_is_stable_and_differs_between_processes | internal: Rust unit test (procs) |
| test_procs::ProcsTests::test_child_in_new_session_has_own_group_and_is_a_descendant | internal: Rust unit test (procs) |
| test_procs::ProcsTests::test_same_process_checks_start_time_and_zombies | internal: Rust unit test (procs) |
| test_procs::ProcsTests::test_ps_fallback_parses_lstart_with_spaces | internal: Rust unit test (procs) |
| test_procs::ProcsTests::test_tagged_processes_finds_a_process_by_an_environment_entry | internal: Rust unit test (procs) |
| test_procs::ProcsTests::test_missing_pid | internal: Rust unit test (procs) |
| test_launch::ArgvTests::test_launch_argv_order_and_auto_mode | launch::argv::test_argv_order_and_auto_mode; launch::argv::test_no_supervisor_route_means_no_model_or_effort |
| test_launch::ArgvTests::test_run_passes_supervisor_model_effort_and_cli_settings | launch::argv::test_run_passes_supervisor_model_effort_and_cli_settings |
| test_launch::ArgvTests::test_print_mode_adds_headless_stream_flags_before_the_prompt | launch::argv::test_print_mode_adds_headless_stream_flags_before_the_prompt |
| test_launch::ArgvTests::test_cli_print_option_is_hidden_and_off_by_default | launch::argv::test_print_option_is_hidden_and_off_by_default |
| test_launch::ArgvTests::test_first_message_names_skill_invocation | launch::argv::test_first_message_names_skill_invocation |
| test_launch::ArgvTests::test_attended_use_warning_printed | launch::argv::test_attended_use_warning_printed |
| test_launch::ArgvTests::test_keep_awake_prefix_per_platform | launch::keep_awake::test_keep_awake_runs_beside_claude_which_stays_the_direct_child; launch::keep_awake::test_only_the_platforms_keep_awake_tool_is_used |
| test_launch::ChildTests::test_supervisor_cwd_is_workspace_when_launched_elsewhere | launch::child::test_supervisor_cwd_is_workspace_when_launched_elsewhere |
| test_launch::ChildTests::test_env_has_run_id_dir_and_cli | launch::child::test_env_has_run_id_dir_and_cli |
| test_launch::ChildTests::test_run_accepts_deck_md_path | launch::child::test_run_accepts_deck_md_path |
| test_launch::ChildTests::test_not_a_run_is_an_error_and_starts_nothing | launch::child::test_not_a_run_is_an_error_and_starts_nothing |
| test_launch::ChildTests::test_child_exit_code_returned | launch::child::test_child_exit_code_returned |
| test_launch::ChildTests::test_launch_line_quotes_paths_with_spaces | launch::launch_line::test_launch_line_quotes_paths_with_spaces |
| test_launch::OwnershipTests::test_refused_launch_changes_nothing | launch::ownership::test_refused_launch_changes_nothing |
| test_launch::OwnershipTests::test_cli_run_refusal_exits_3 | launch::ownership::test_cli_run_refusal_exits_3 |
| test_launch::OwnershipTests::test_lock_held_by_supervisor_after_launcher_killed | launch::ownership::test_lock_held_by_supervisor_after_launcher_killed |
| test_launch::OwnershipTests::test_lock_fd_alone_blocks_takeover | launch::ownership::test_lock_fd_alone_blocks_takeover |
| test_launch::OwnershipTests::test_recorded_live_supervisor_blocks_takeover | launch::ownership::test_recorded_live_supervisor_blocks_takeover |
| test_launch::OwnershipTests::test_resumed_appended_only_after_acquire | launch::ownership::test_resumed_appended_only_after_acquire |
| test_launch::OwnershipTests::test_first_launch_of_a_fresh_run_appends_no_resumed | launch::ownership::test_first_launch_of_a_fresh_run_appends_no_resumed |
| test_launch::OwnershipTests::test_holder_records_launcher_and_supervisor | launch::ownership::test_holder_records_launcher_and_supervisor |
| test_launch::OwnershipTests::test_signals_forwarded_to_child | launch::ownership::test_signals_forwarded_to_child |
| test_launch::FollowUpTests::test_linux_inhibitor_runs_beside_claude_which_stays_the_direct_child | launch::keep_awake::test_keep_awake_runs_beside_claude_which_stays_the_direct_child |
| test_launch::FollowUpTests::test_cli_path_with_spaces_warns_sandbox_exclusion_may_not_match | launch::argv::test_cli_path_with_spaces_warns_sandbox_exclusion_may_not_match |
| test_launch::FollowUpTests::test_signal_after_child_exit_is_not_forwarded | internal: Rust unit test (launch) |
| test_launch::FollowUpTests::test_launch_line_defaults_to_recorded_plugin_dir | launch::launch_line::test_launch_line_defaults_to_recorded_plugin_dir |
| test_launch::FollowUpTests::test_busy_lock_with_dead_recorded_supervisor_says_not_running | launch::ownership::test_busy_lock_with_dead_recorded_supervisor_says_not_running |
| test_launch::InheritedLockTests::test_relaunch_succeeds_while_a_cli_child_of_the_killed_supervisor_runs | launch::inherited_lock::test_relaunch_succeeds_while_a_cli_child_of_the_killed_supervisor_runs; launch::lock_fd_environment::test_a_worker_never_inherits_lead_hand_lock_fd |
| test_launch::InheritedLockTests::test_cli_closes_inherited_run_lock_fd | launch::inherited_lock::test_cli_closes_inherited_run_lock_fd; launch::lock_fd_environment::test_a_worker_never_inherits_lead_hand_lock_fd |
| test_launch::InheritedLockTests::test_cli_leaves_an_unrelated_fd_open | launch::inherited_lock::test_cli_leaves_an_unrelated_fd_open; launch::lock_fd_environment::test_a_worker_never_inherits_lead_hand_lock_fd |
| test_launch::InPlaceCheckoutTests::test_in_place_launch_is_refused_while_another_in_place_run_is_supervised | dropped with in-place mode |
| test_launch::InPlaceCheckoutTests::test_in_place_launch_ignores_dead_supervisors_and_worktree_runs | dropped with in-place mode |
| test_launch::InPlaceCheckoutTests::test_worktree_launch_is_not_held_back_by_an_in_place_run | launch::ownership::test_another_runs_live_supervisor_does_not_hold_back_a_launch |
| test_launch::CheckoutClaimTests::test_in_place_launch_is_refused_while_a_run_in_another_runs_folder_is_supervised | dropped with in-place mode |
| test_launch::CheckoutClaimTests::test_in_place_launch_is_refused_before_the_other_launch_records_its_supervisor | dropped with in-place mode |
| test_launch::CheckoutClaimTests::test_a_worker_deleting_the_claim_record_does_not_free_the_checkout | dropped with in-place mode |
| test_launch::CheckoutClaimTests::test_in_place_launch_from_a_subdirectory_of_a_claimed_checkout_is_refused | dropped with in-place mode |
| test_launch::CheckoutClaimTests::test_a_checkout_that_cannot_be_flocked_is_refused_with_a_clear_error | dropped with in-place mode |
| test_launch::CheckoutClaimTests::test_cli_closes_an_inherited_claim_on_the_checkout_top_of_a_subdirectory_run | dropped with in-place mode |
| test_launch::CheckoutClaimTests::test_an_in_place_relaunch_is_refused_while_an_edit_is_hidden_behind_an_index_flag | dropped with in-place mode |
| test_launch::CheckoutClaimTests::test_a_worktree_run_launch_is_refused_while_the_repository_is_sparse | launch::child::test_a_worktree_run_launch_is_refused_while_the_repository_is_sparse |
| test_launch::CheckoutClaimTests::test_the_claim_is_released_and_its_record_removed_when_the_supervisor_exits | dropped with in-place mode |
| test_launch::CheckoutClaimTests::test_worktree_launch_is_not_held_back_by_a_claimed_checkout | launch::ownership::test_another_runs_live_supervisor_does_not_hold_back_a_launch |
| test_launch::CheckoutClaimTests::test_a_cli_child_of_a_killed_supervisor_does_not_keep_the_checkout_claimed | dropped with in-place mode |
| test_launch::CheckoutClaimTests::test_cli_leaves_an_unrelated_fd_named_as_the_checkout_claim_open | dropped with in-place mode |
| test_engine_cards::HappyPath::test_happy_path_two_cards_accepted_in_order | smoke::smoke::test_two_cards_accepted_in_order |
| test_engine_cards::HappyPath::test_requested_dispatch_is_repeated_until_started | cards::happy_path::test_requested_dispatch_is_repeated_until_started |
| test_engine_cards::HappyPath::test_started_dispatch_returns_wait_then_stop_worker_after_deadline | cards::happy_path::test_started_dispatch_returns_wait_then_stop_worker_after_deadline |
| test_engine_cards::HappyPath::test_duplicate_result_is_harmless | cards::happy_path::test_duplicate_result_is_harmless |
| test_engine_cards::Loop::test_repeated_answers_and_reviews_get_distinct_ids | cards::loop_::test_repeated_answers_and_reviews_get_distinct_ids |
| test_engine_cards::Loop::test_by_risk_review_skips_low_risk_card | cards::loop_::test_by_risk_review_skips_low_risk_card |
| test_engine_cards::Loop::test_defects_get_one_fix_round_then_rereview_on_production_change | cards::loop_::test_defects_get_one_fix_round_then_rereview_on_production_change |
| test_engine_cards::Loop::test_clean_verdict_with_medium_finding_counts_as_defects | cards::loop_::test_clean_verdict_with_medium_finding_counts_as_defects |
| test_engine_cards::Loop::test_clean_rereview_noting_fixed_findings_stays_clean | cards::loop_::test_clean_rereview_noting_fixed_findings_stays_clean |
| test_engine_cards::Loop::test_clean_verdict_cross_check_reads_only_findings_lines | cards::loop_::test_clean_verdict_cross_check_reads_only_findings_lines |
| test_engine_cards::Loop::test_clean_verdict_cross_check_counts_bullet_findings_lines | cards::loop_::test_clean_verdict_cross_check_counts_bullet_findings_lines |
| test_engine_cards::Loop::test_test_only_fix_returns_check_diff | cards::loop_::test_test_only_fix_returns_check_diff |
| test_engine_cards::Loop::test_defects_after_fix_round_escalate_with_fresh_worker | cards::loop_::test_defects_after_fix_round_escalate_with_fresh_worker |
| test_engine_cards::Loop::test_spent_escalates_and_exhausted_ladder_parks | cards::loop_::test_spent_escalates_and_exhausted_ladder_parks |
| test_engine_cards::Loop::test_usage_limit_ends_blocked_without_switching_vendor | cards::loop_::test_usage_limit_ends_blocked_without_switching_vendor |
| test_engine_cards::Loop::test_other_blocked_retries_once_then_ends_blocked_environment | cards::loop_::test_other_blocked_retries_once_then_ends_blocked_environment |
| test_engine_cards::Loop::test_question_ruling_resumes_worker | cards::loop_::test_question_ruling_resumes_worker |
| test_engine_cards::Loop::test_parked_card_skips_dependents_and_continues_independent | cards::loop_::test_parked_card_skips_dependents_and_continues_independent |
| test_engine_cards::Start::test_start_returns_existing_unfinished_run_unless_new | cards::start::test_start_returns_existing_unfinished_run_unless_new |
| test_engine_cards::Start::test_start_never_reuses_an_unfinished_run_of_another_clone | cards::start::test_start_never_reuses_an_unfinished_run_of_another_clone |
| test_engine_cards::Start::test_start_never_reuses_another_runs_worktree | cards::start::test_start_never_reuses_another_runs_worktree |
| test_engine_cards::Start::test_start_refuses_unsupported_settings_and_scout_cards | cards::start::test_start_refuses_unsupported_settings_and_scout_cards |
| test_engine_cards::Start::test_check_and_start_refuse_step_limits_claude_workers_cannot_honour | cards::start::test_check_and_start_refuse_step_limits_claude_workers_cannot_honour |
| test_engine_cards::Start::test_deck_changed_since_start_refuses | cards::start::test_deck_changed_since_start_refuses |
| test_engine_cards::Records::test_reply_without_status_blocks_and_keeps_raw | cards::records::test_reply_without_status_blocks_and_keeps_raw |
| test_engine_cards::Records::test_turn_limit_reply_is_spent | cards::records::test_turn_limit_reply_is_spent |
| test_engine_cards::Records::test_invalid_record_transition_raises | cards::records::test_invalid_record_transition_raises |
| test_engine_cards::Records::test_timer_returns_at_deadline | cards::records::test_timer_returns_at_deadline |
| test_engine_cards::Records::test_cli_records_reply_from_stdin | cards::records::test_cli_records_reply_from_stdin |
| test_engine_cards::Codex::test_codex_route_dispatches_proxy_with_null_agent_model | cards::codex::test_codex_route_dispatches_proxy_with_null_agent_model |
| test_engine_cards::Codex::test_stop_worker_for_codex_cancels_runner_and_records_before_returning | cards::codex::test_stop_worker_for_codex_cancels_runner_and_records_before_returning |
| test_engine_cards::Crashes::test_crash_after_fast_forward_before_accepted_reconciles | cards::crashes::test_crash_after_fast_forward_before_accepted_reconciles |
| test_engine_cards::Crashes::test_crash_after_result_files_before_event_reconciles | cards::crashes::test_crash_after_result_files_before_event_reconciles |
| test_engine_cards::Crashes::test_crash_after_charge_before_attempt_retry_keeps_one_charge | cards::crashes::test_crash_after_charge_before_attempt_retry_keeps_one_charge |
| test_engine_cards::Supervisor::test_supervisor_usage_charged_once_across_calls | cards::supervisor::test_supervisor_usage_charged_once_across_calls |
| test_engine_by_risk::ByRisk::test_ready_at_the_attempt_start_tip_goes_to_review | cards::by_risk::test_ready_at_the_attempt_start_tip_goes_to_review |
| test_engine_by_risk::ByRisk::test_low_risk_ready_with_commits_is_still_accepted_without_review | cards::by_risk::test_low_risk_ready_with_commits_is_still_accepted_without_review |
| test_engine_decisions::DecisionNeeded::test_question_parked_on_a_decision_ends_blocked_and_unpark_continues | cards::decision_needed::test_question_parked_on_a_decision_ends_blocked_and_unpark_continues |
| test_engine_decisions::DecisionNeeded::test_card_waiting_on_a_decision_card_still_ends_blocked | cards::decision_needed::test_card_waiting_on_a_decision_card_still_ends_blocked |
| test_engine_decisions::DecisionNeeded::test_violation_cap_park_ends_blocked_for_a_decision | cards::decision_needed::test_violation_cap_park_ends_blocked_for_a_decision |
| test_engine_decisions::DecisionNeeded::test_park_that_is_not_a_decision_still_ends_incomplete | cards::decision_needed::test_park_that_is_not_a_decision_still_ends_incomplete |
| test_engine_decisions::DecisionNeeded::test_unpark_refuses_on_a_run_it_cannot_reopen_and_hides_the_hint | cards::decision_needed::test_unpark_refuses_on_a_run_it_cannot_reopen_and_hides_the_hint |
| test_engine_reply_file::ReplyFile::test_dispatch_action_carries_a_git_ignored_reply_file_in_the_scratch_dir | cards::reply_file::test_dispatch_action_carries_a_git_ignored_reply_file_in_the_scratch_dir |
| test_engine_reply_file::ReplyFile::test_wait_names_the_same_reply_file | cards::reply_file::test_wait_names_the_same_reply_file |
| test_engine_reply_file::ReplyFile::test_cli_records_reply_from_a_file_with_spaces_in_its_path | cards::reply_file::test_cli_records_reply_from_a_file_with_spaces_in_its_path |
| test_engine_reply_file::ReplyFile::test_reply_and_reply_file_are_exclusive | cards::reply_file::test_reply_and_reply_file_are_exclusive |
| test_engine_reply_file::ReplyFile::test_missing_reply_file_falls_back_to_the_report_head_with_a_note | cards::reply_file::test_missing_reply_file_falls_back_to_the_report_head_with_a_note |
| test_engine_reply_file::ReplyFile::test_new_request_removes_a_stale_reply_file | cards::reply_file::test_new_request_removes_a_stale_reply_file |
| test_engine_codex_result::CodexResultFromRunner::test_early_still_running_reply_waits_and_starts_no_second_implementer | cards::codex_result_from_runner::test_early_still_running_reply_waits_and_starts_no_second_implementer |
| test_engine_codex_result::CodexResultFromRunner::test_invented_verdict_without_a_worker_is_not_accepted | cards::codex_result_from_runner::test_invented_verdict_without_a_worker_is_not_accepted |
| test_engine_codex_result::CodexResultFromRunner::test_skipping_a_codex_card_stops_its_runner | cards::codex_result_from_runner::test_skipping_a_codex_card_stops_its_runner |
| test_engine_codex_e2e::NonCommittingCodex::test_two_card_run_with_gates_completes_with_the_runners_commits | cards::non_committing_codex::test_two_card_run_with_gates_completes_with_the_runners_commits |
| test_engine_run_branch::ImplementerGuard::test_implementer_commit_on_the_run_branch_is_a_violation | cards::implementer_guard::test_implementer_commit_on_the_run_branch_is_a_violation |
| test_engine_run_branch::RunHeadCheck::test_run_branch_moved_between_dispatches_fails_the_run | cards::run_head_check::test_run_branch_moved_between_dispatches_fails_the_run |
| test_engine_run_branch::RunHeadCheck::test_outcome_checks_the_run_head_after_a_cancel | cards::run_head_check::test_outcome_checks_the_run_head_after_a_cancel |
| test_engine_untracked::EngineSide::test_parked_cards_untracked_file_does_not_land_via_the_next_codex_card | cards::engine_side::test_parked_cards_untracked_file_does_not_land_via_the_next_codex_card |
| test_engine_untracked::EngineSide::test_cancel_partial_commit_leaves_untracked_files_that_were_there_before | cards::engine_side::test_cancel_partial_commit_leaves_untracked_files_that_were_there_before |
| test_engine_untracked::RunnerSide::test_runner_commits_only_tracked_changes_and_files_created_during_the_dispatch | cards::runner_side::test_runner_commits_only_tracked_changes_and_files_created_during_the_dispatch |
| test_briefs::BriefTests::test_implement_brief_contains_contract_fields_verbatim | briefs::briefs::test_implement_brief_contains_contract_fields_verbatim |
| test_briefs::BriefTests::test_test_first_defaults_to_when_cheap | briefs::briefs::test_test_first_defaults_to_when_cheap |
| test_briefs::BriefTests::test_every_brief_kind_has_a_template | briefs::briefs::test_every_brief_kind_fills_its_values; briefs::briefs::test_integrate_permits_rebase_onto_run_tip; briefs::briefs::test_continue_brief_carries_on_from_the_branch |
| test_briefs::BriefTests::test_kind_specific_content | briefs::briefs::test_kind_specific_content; briefs::briefs::test_continue_brief_carries_on_from_the_branch |
| test_briefs::BriefTests::test_card_briefs_require_card_base_and_branch | briefs::briefs::test_card_briefs_render_card_base_and_branch |
| test_briefs::BriefTests::test_review_needs_implementer_report_unless_test_first_off | briefs::briefs::test_review_names_the_implementer_report |
| test_briefs::BriefTests::test_integrate_permits_rebase_onto_run_tip | briefs::briefs::test_integrate_permits_rebase_onto_run_tip |
| test_briefs::BriefTests::test_final_briefs_list_scope | briefs::briefs::test_final_briefs_list_scope; briefs::briefs::test_final_briefs_cover_only_the_accepted_cards |
| test_briefs::BriefTests::test_evidence_requires_cards | internal: Rust unit test (briefs) |
| test_briefs::BriefTests::test_answer_quotes_every_ruling_line | briefs::briefs::test_answer_quotes_every_ruling_line |
| test_briefs::BriefTests::test_verify_says_skip_for_empty_commands | internal: Rust unit test (briefs) |
| test_briefs::BriefTests::test_missing_extra_and_unknown_kind_raise | internal: Rust unit test (briefs) |
| test_briefs::BriefTests::test_brief_and_report_paths_are_inside_workspace_scratch | briefs::briefs::test_brief_and_report_paths_are_inside_workspace_scratch |
| test_briefs::BriefTests::test_scratch_dir_is_ignored_by_git | briefs::briefs::test_scratch_dir_is_ignored_by_git |
| test_briefs::BriefTests::test_agent_for_maps_every_role_and_vendor | briefs::briefs::test_agent_for_maps_every_role_and_vendor; cards::codex::test_codex_route_dispatches_proxy_with_null_agent_model; cards::codex_result_from_runner::test_invented_verdict_without_a_worker_is_not_accepted |
| test_briefs::BriefTests::test_claude_dispatch_message_names_paths | briefs::briefs::test_claude_dispatch_message_names_paths |
| test_briefs::BriefTests::test_codex_dispatch_message_is_one_quoted_command | briefs::briefs::test_codex_dispatch_message_is_one_quoted_command; briefs::briefs::test_codex_dispatch_message_quotes_shell_metacharacters |
| test_briefs::BriefTests::test_worker_prompt_for_codex_workers | briefs::briefs::test_worker_prompt_for_codex_workers |
| test_briefs::BriefTests::test_codex_implementer_prompt_forbids_commits_and_claude_agents_do_not | briefs::briefs::test_codex_implementer_prompt_forbids_commits_and_claude_agents_do_not |
| test_briefs::BriefTests::test_parse_report_head_variants | briefs::briefs::test_parse_report_head_variants |
| test_engine_integration::Integration::test_resumed_parked_card_is_rebased_rereviewed_verified_and_fast_forwarded | integration::integration::test_resumed_parked_card_is_rebased_rereviewed_verified_and_fast_forwarded |
| test_engine_integration::Integration::test_integration_conflict_requests_integrate_attempt | integration::integration::test_integration_conflict_requests_integrate_attempt |
| test_engine_integration::Integration::test_crash_after_rebase_before_event_reconciles | integration::integration::test_crash_after_rebase_before_event_reconciles |
| test_engine_integration::Integration::test_crash_before_rebase_rebases_on_restart | integration::integration::test_crash_before_rebase_rebases_on_restart |
| test_engine_integration::Integration::test_failed_verification_escalates | integration::integration::test_failed_verification_escalates |
| test_engine_integration::Unpark::test_unpark_without_a_known_worker_starts_a_fresh_attempt | integration::unpark::test_unpark_without_a_known_worker_starts_a_fresh_attempt |
| test_engine_integration::Unpark::test_unpark_cli_appends_unparked_and_refuses_a_card_not_parked | integration::unpark::test_unpark_cli_appends_unparked_and_refuses_a_card_not_parked |
| test_engine_integration::Violations::test_reviewer_tracked_change_voids_review_and_reruns | integration::violations::test_reviewer_tracked_change_voids_review_and_reruns |
| test_engine_integration::Violations::test_reviewer_moving_head_is_violation | integration::violations::test_reviewer_moving_head_is_violation |
| test_engine_integration::Violations::test_reviewer_hiding_an_edit_behind_an_index_flag_is_violation | integration::violations::test_reviewer_hiding_an_edit_behind_an_index_flag_is_violation |
| test_engine_integration::Violations::test_a_partial_commit_takes_an_edit_hidden_behind_an_index_flag | integration::violations::test_a_partial_commit_takes_an_edit_hidden_behind_an_index_flag |
| test_engine_integration::Violations::test_verifier_staged_change_is_violation | integration::violations::test_verifier_staged_change_is_violation |
| test_engine_integration::Violations::test_violation_preserves_index_and_worktree_on_ref | integration::violations::test_violation_preserves_index_and_worktree_on_ref |
| test_engine_integration::Violations::test_untracked_build_output_is_not_a_violation | integration::violations::test_untracked_build_output_is_not_a_violation |
| test_engine_integration::Violations::test_reviewer_committing_on_run_branch_and_returning_to_card_tip_is_violation | integration::violations::test_reviewer_committing_on_run_branch_and_returning_to_card_tip_is_violation |
| test_engine_integration::Violations::test_reviewer_creating_and_deleting_refs_is_violation | integration::violations::test_reviewer_creating_and_deleting_refs_is_violation |
| test_engine_integration::Violations::test_violation_interrupted_before_restore_still_voids_on_restart | integration::violations::test_violation_interrupted_before_restore_still_voids_on_restart |
| test_engine_integration::Violations::test_voided_reviews_do_not_spend_the_retry_budget | integration::violations::test_voided_reviews_do_not_spend_the_retry_budget |
| test_engine_integration::OneCardInFlight::test_unpark_while_another_card_is_in_flight_waits | integration::one_card_in_flight::test_unpark_while_another_card_is_in_flight_waits |
| test_engine_integration::OneCardInFlight::test_engine_ref_moves_after_dispatch_are_not_violations | integration::one_card_in_flight::test_engine_ref_moves_after_dispatch_are_not_violations |
| test_engine_integration::FixRound1::test_repeated_check_violations_park_the_card | integration::fix_round1::test_repeated_check_violations_park_the_card |
| test_engine_integration::FixRound1::test_unpark_after_relaunch_starts_fresh_attempt_with_note | integration::fix_round1::test_unpark_after_relaunch_starts_fresh_attempt_with_note |
| test_engine_integration::FixRound1::test_operation_left_in_progress_is_blocked_and_preserved | integration::fix_round1::test_operation_left_in_progress_is_blocked_and_preserved |
| test_engine_integration::FixRound1::test_implementer_leftovers_are_preserved_before_review_without_voiding | integration::fix_round1::test_implementer_leftovers_are_preserved_before_review_without_voiding |
| test_engine_integration::FixRound1::test_integration_keeps_the_pre_integration_tip | integration::fix_round1::test_integration_keeps_the_pre_integration_tip |
| test_engine_integration::FixRound1::test_integrate_moving_run_branch_is_violation | integration::fix_round1::test_integrate_moving_run_branch_is_violation |
| test_engine_integration::FixRound1::test_integrate_dropping_card_work_escalates | integration::fix_round1::test_integrate_dropping_card_work_escalates |
| test_engine_integration::FixRound1::test_integrate_question_answer_goes_through_integration | integration::fix_round1::test_integrate_question_answer_goes_through_integration |
| test_engine_integration::FixRound2::test_voided_answer_to_integrate_keeps_its_resume_chain | integration::fix_round2::test_voided_answer_to_integrate_keeps_its_resume_chain |
| test_engine_integration::FixRound2::test_unpark_after_violation_cap_reruns_the_check | integration::fix_round2::test_unpark_after_violation_cap_reruns_the_check |
| test_engine_integration::FixRound2::test_voided_rereview_is_rerun_by_a_fresh_worker | integration::fix_round2::test_voided_rereview_is_rerun_by_a_fresh_worker |
| test_engine_integration::CodexIntegrate::test_integrate_after_codex_implementer_routes_to_claude | integration::codex_integrate::test_integrate_after_codex_implementer_routes_to_claude |
| test_engine_integration::CodexIntegrate::test_failed_runner_commit_is_recorded_blocked_environment | integration::codex_integrate::test_failed_runner_commit_is_recorded_blocked_environment |
| test_engine_integration::CodexIntegrate::test_integrate_without_claude_parks_the_card | integration::codex_integrate::test_integrate_without_claude_parks_the_card |
| test_engine_finish::Complete::test_complete_run_writes_outcome_and_report | finish::complete::test_complete_run_writes_outcome_and_report |
| test_engine_finish::Complete::test_runs_md_line_appended_once | finish::complete::test_runs_md_line_appended_once |
| test_engine_finish::SharedRunIndex::test_concurrent_runs_never_lose_a_runs_md_line | finish::shared_run_index::test_concurrent_runs_never_lose_a_runs_md_line |
| test_engine_finish::RelativeDocsDir::test_relative_docs_dir_resolves_against_the_repository_from_any_cwd | finish::relative_docs_dir::test_relative_docs_dir_resolves_against_the_repository_from_any_cwd |
| test_engine_finish::FixWaves::test_final_review_defects_get_a_fix_wave_then_all_gates_rerun | finish::fix_waves::test_final_review_defects_get_a_fix_wave_then_all_gates_rerun |
| test_engine_finish::FixWaves::test_verify_fix_after_security_pass_reruns_security_and_evidence | finish::fix_waves::test_verify_fix_after_security_pass_reruns_security_and_evidence |
| test_engine_finish::FixWaves::test_fix_waves_are_capped | finish::fix_waves::test_fix_waves_are_capped |
| test_engine_finish::FixWaves::test_rejected_fix_is_not_accepted_and_spends_its_wave | finish::fix_waves::test_rejected_fix_is_not_accepted_and_spends_its_wave |
| test_engine_finish::Evidence::test_evidence_block_gap_ends_blocked | finish::evidence::test_evidence_block_gap_ends_blocked |
| test_engine_finish::Evidence::test_evidence_record_gap_makes_incomplete_with_hold | finish::evidence::test_evidence_record_gap_makes_incomplete_with_hold |
| test_engine_finish::Evidence::test_resumed_blocked_run_keeps_gates_that_passed_on_the_same_head | finish::evidence::test_resumed_blocked_run_keeps_gates_that_passed_on_the_same_head |
| test_engine_finish::Security::test_security_gate_only_for_medium_or_high_or_sensitive_paths | finish::security::test_security_gate_only_for_medium_or_high_or_sensitive_paths |
| test_engine_finish::Verification::test_verification_failure_after_last_wave_is_failed | finish::verification::test_verification_failure_after_last_wave_is_failed |
| test_engine_finish::Unpark::test_unparked_card_waits_for_an_open_gate_then_gates_rerun | finish::unpark::test_unparked_card_waits_for_an_open_gate_then_gates_rerun |
| test_engine_finish::Guards::test_final_fix_moving_the_run_branch_is_voided_and_restored | finish::guards::test_final_fix_moving_the_run_branch_is_voided_and_restored |
| test_engine_finish::Guards::test_accepted_work_missing_from_the_run_branch_fails_the_run | finish::guards::test_accepted_work_missing_from_the_run_branch_fails_the_run |
| test_engine_finish::Guards::test_evidence_base_worktree_is_reset_and_guarded | finish::guards::test_evidence_base_worktree_is_reset_and_guarded |
| test_engine_finish::Guards::test_evidence_base_edit_hidden_behind_an_index_flag_is_violation | finish::guards::test_evidence_base_edit_hidden_behind_an_index_flag_is_violation |
| test_engine_finish::Guards::test_gate_violation_reruns_with_a_fresh_worker | finish::guards::test_gate_violation_reruns_with_a_fresh_worker |
| test_engine_finish::Guards::test_gate_violation_cap_ends_blocked | finish::guards::test_gate_violation_cap_ends_blocked |
| test_engine_finish::RunWorkerFailures::test_gate_without_a_result_twice_ends_blocked_environment | finish::run_worker_failures::test_gate_without_a_result_twice_ends_blocked_environment |
| test_engine_finish::RunWorkerFailures::test_gate_usage_limit_ends_blocked_provider_capacity | finish::run_worker_failures::test_gate_usage_limit_ends_blocked_provider_capacity |
| test_engine_finish::Crashes::test_crash_before_gate_record_records_once | finish::crashes::test_crash_before_gate_record_records_once |
| test_engine_finish::Crashes::test_crash_before_final_fast_forward_reconciles | finish::crashes::test_crash_before_final_fast_forward_reconciles |
| test_engine_finish::Crashes::test_crash_after_final_fast_forward_reconciles | finish::crashes::test_crash_after_final_fast_forward_reconciles |
| test_engine_finish::FixRound2::test_spent_final_fix_is_not_rechecked_after_a_card_accept_crash | finish::fix_round2::test_spent_final_fix_is_not_rechecked_after_a_card_accept_crash |
| test_engine_finish::FixRound2::test_crash_between_base_violation_found_and_restored_recovers | finish::fix_round2::test_crash_between_base_violation_found_and_restored_recovers |
| test_engine_finish::FixRound2::test_evidence_rerun_after_a_worker_failure_gets_a_reset_base | finish::fix_round2::test_evidence_rerun_after_a_worker_failure_gets_a_reset_base |
| test_engine_finish::FixRound2::test_voided_final_fix_reruns_from_the_run_head | finish::fix_round2::test_voided_final_fix_reruns_from_the_run_head |
| test_engine_finish::NothingAccepted::test_run_with_every_card_parked_runs_no_gates | finish::nothing_accepted::test_run_with_every_card_parked_runs_no_gates |
| test_engine_finish::BaseWorktreeOwnership::test_in_place_run_refuses_a_foreign_worktree_at_its_base_path | dropped with in-place mode |
| test_engine_finish::BaseWorktreeOwnership::test_worktree_run_refuses_a_foreign_worktree_at_its_base_path | finish::base_worktree_ownership::test_worktree_run_refuses_a_foreign_worktree_at_its_base_path |
| test_engine_finish::BaseWorktreeOwnership::test_a_foreign_worktree_after_a_failed_add_is_refused | finish::base_worktree_ownership::test_a_foreign_worktree_after_a_failed_add_is_refused |
| test_engine_finish::BaseWorktreeOwnership::test_a_foreign_worktree_replacing_the_runs_base_worktree_is_refused | finish::base_worktree_ownership::test_a_foreign_worktree_replacing_the_runs_base_worktree_is_refused |
| test_engine_finish::BaseWorktreeOwnership::test_a_base_worktree_left_by_a_crash_is_refused_once_it_holds_work | finish::base_worktree_ownership::test_a_base_worktree_left_by_a_crash_is_refused_once_it_holds_work |
| test_engine_finish::BaseWorktreeOwnership::test_a_crash_window_worktree_hiding_a_tracked_edit_is_refused | finish::base_worktree_ownership::test_a_crash_window_worktree_hiding_a_tracked_edit_is_refused |
| test_engine_finish::BaseWorktreeOwnership::test_a_base_worktree_added_before_a_crash_is_still_the_runs | finish::base_worktree_ownership::test_a_base_worktree_added_before_a_crash_is_still_the_runs |
| test_engine_finish::SparseCheckout::test_a_worktree_run_in_a_sparse_checkout_is_refused_at_start | finish::sparse_checkout::test_a_worktree_run_in_a_sparse_checkout_is_refused_at_start |
| test_engine_finish::SparseCheckout::test_a_sparse_checkout_a_reviewer_turns_on_is_a_violation | finish::sparse_checkout::test_a_sparse_checkout_a_reviewer_turns_on_is_a_violation |
| test_final_review_minors::Timer::test_timer_returns_promptly_when_the_run_is_paused_or_cancelled | finish::timer::test_timer_returns_promptly_when_the_run_is_paused_or_cancelled |
| test_final_review_minors::PinnedIntegrate::test_integrate_after_a_codex_pinned_card_runs_on_claude | finish::pinned_integrate::test_integrate_after_a_codex_pinned_card_runs_on_claude |
| test_final_review_minors::OperatorCommands::test_pause_cancel_and_handoff_print_the_quoted_absolute_cli | finish::operator_commands::test_pause_cancel_and_handoff_print_the_quoted_absolute_cli |
| test_final_review_minors::OperatorCommands::test_relaunch_refusal_names_the_quoted_start_command | finish::operator_commands::test_relaunch_refusal_names_the_quoted_start_command |
| test_final_review_minors::Settings::test_notify_and_capacity_are_warned_as_not_supported | finish::settings::test_notify_and_capacity_are_warned_as_not_supported |
| test_final_review_minors::Settings::test_defaults_are_not_warned | finish::settings::test_defaults_are_not_warned |
| test_final_review_minors::Start::test_start_skips_an_old_run_whose_event_log_is_unreadable | finish::start::test_start_skips_an_old_run_whose_event_log_is_unreadable |
| test_final_review_minors::CopyFiles::test_a_symlinked_report_is_not_followed | finish::copy_files::test_a_symlinked_report_is_not_followed |
| test_final_review_minors::RunnerFinalize::test_heartbeat_is_touched_while_finalizing | finish::runner_finalize::test_heartbeat_is_touched_while_finalizing |
| test_final_review_minors::RunnerFinalize::test_non_utf8_git_output_does_not_kill_the_commit | finish::runner_finalize::test_non_utf8_git_output_does_not_kill_the_commit |
| test_recovery::Reconcile::test_restart_after_truncated_final_event_then_append | recovery::reconcile::test_restart_after_truncated_final_event_then_append |
| test_recovery::Reconcile::test_report_without_recorded_result_is_recorded | recovery::reconcile::test_report_without_recorded_result_is_recorded |
| test_recovery::Reconcile::test_commits_without_report_continue_not_discard | recovery::reconcile::test_commits_without_report_continue_not_discard |
| test_recovery::Reconcile::test_nothing_on_the_branch_requests_the_same_attempt_again | recovery::reconcile::test_nothing_on_the_branch_requests_the_same_attempt_again |
| test_recovery::Reconcile::test_fix_round_resumes_with_a_fresh_worker_from_the_findings | recovery::reconcile::test_fix_round_resumes_with_a_fresh_worker_from_the_findings |
| test_recovery::Reconcile::test_acceptance_started_completes_after_restart | recovery::reconcile::test_acceptance_started_completes_after_restart |
| test_recovery::Reconcile::test_unmatched_state_fails_with_reason | recovery::reconcile::test_unmatched_state_fails_with_reason |
| test_recovery::Reconcile::test_missing_card_branch_fails_with_reason | recovery::reconcile::test_missing_card_branch_fails_with_reason |
| test_recovery::Reconcile::test_usage_after_interrupted_resume_equals_session_totals | recovery::reconcile::test_usage_after_interrupted_resume_equals_session_totals |
| test_recovery::Codex::test_live_codex_runner_is_waited_not_restarted | recovery::codex::test_live_codex_runner_is_waited_not_restarted |
| test_recovery::Codex::test_hung_runner_with_stale_heartbeat_is_cancelled_before_redispatch | recovery::codex::test_hung_runner_with_stale_heartbeat_is_cancelled_before_redispatch |
| test_recovery::Codex::test_dead_runner_without_result_is_finalized_and_dispatched_again | recovery::codex::test_dead_runner_without_result_is_finalized_and_dispatched_again |
| test_recovery::PauseCancel::test_pause_ends_blocked_paused_with_handoff | recovery::pause_cancel::test_pause_ends_blocked_paused_with_handoff |
| test_recovery::PauseCancel::test_cancel_ends_incomplete_cancelled_and_keeps_card_branches | recovery::pause_cancel::test_cancel_ends_incomplete_cancelled_and_keeps_card_branches |
| test_recovery::PauseCancel::test_cancel_works_after_the_source_deck_was_edited | recovery::pause_cancel::test_cancel_works_after_the_source_deck_was_edited |
| test_recovery::PauseCancel::test_pause_works_after_the_source_deck_was_deleted_but_nothing_new_starts | recovery::pause_cancel::test_pause_works_after_the_source_deck_was_deleted_but_nothing_new_starts |
| test_recovery::PauseCancel::test_cancel_survives_a_relaunch_before_it_finished | recovery::pause_cancel::test_cancel_survives_a_relaunch_before_it_finished |
| test_recovery::PauseCancel::test_cancel_confirms_detached_codex_stopped_before_outcome | recovery::pause_cancel::test_cancel_confirms_detached_codex_stopped_before_outcome |
| test_recovery::PauseCancel::test_crash_after_cancel_request_finishes_the_cancel_on_restart | recovery::pause_cancel::test_crash_after_cancel_request_finishes_the_cancel_on_restart |
| test_recovery::PauseCancel::test_relaunch_refuses_a_finished_run | recovery::pause_cancel::test_relaunch_refuses_a_finished_run |
| test_recovery::PauseCancel::test_run_skill_steers_with_plain_commands | plugin file check (plugin_files) |
| test_recovery::FixRound1::test_cancel_after_an_interrupted_acceptance_reconciles_first | recovery::fix_round1::test_cancel_after_an_interrupted_acceptance_reconciles_first |
| test_recovery::FixRound1::test_cancel_commits_a_stopped_claude_workers_partial_work | recovery::fix_round1::test_cancel_commits_a_stopped_claude_workers_partial_work |
| test_recovery::FixRound1::test_cancel_commits_partial_work_of_a_worker_that_died_with_its_session | recovery::fix_round1::test_cancel_commits_partial_work_of_a_worker_that_died_with_its_session |
| test_recovery::FixRound1::test_stopped_check_worker_changes_are_not_committed | recovery::fix_round1::test_stopped_check_worker_changes_are_not_committed |
| test_recovery::FixRound1::test_failed_reconciliation_stops_live_codex_before_the_outcome | recovery::fix_round1::test_failed_reconciliation_stops_live_codex_before_the_outcome |
| test_recovery::FixRound1::test_orphaned_codex_is_cancelled_before_anything_else | recovery::fix_round1::test_orphaned_codex_is_cancelled_before_anything_else |
| test_recovery::FixRound1::test_crash_in_reconciliations_cancel_window_is_repeat_safe | recovery::fix_round1::test_crash_in_reconciliations_cancel_window_is_repeat_safe |
| test_recovery::FixRound1::test_unrecorded_codex_start_past_its_deadline_is_not_charged_or_stopped | recovery::fix_round1::test_unrecorded_codex_start_past_its_deadline_is_not_charged_or_stopped |
| test_recovery::FixRound1::test_interrupted_review_without_report_runs_again | recovery::fix_round1::test_interrupted_review_without_report_runs_again |
| test_recovery::FixRound1::test_interrupted_gate_without_report_runs_again | recovery::fix_round1::test_interrupted_gate_without_report_runs_again |
| test_recovery::FixRound1::test_interrupted_final_fix_before_its_commit_runs_again | recovery::fix_round1::test_interrupted_final_fix_before_its_commit_runs_again |
| test_recovery::FixRound1::test_interrupted_final_fix_after_its_commit_keeps_it | recovery::fix_round1::test_interrupted_final_fix_after_its_commit_keeps_it |
| test_recovery::FixRound2::test_rewritten_branch_fails_the_run | recovery::fix_round2::test_rewritten_branch_fails_the_run |
| test_recovery::FixRound2::test_rewritten_branch_fails_the_run_even_with_a_cancel_pending | recovery::fix_round2::test_rewritten_branch_fails_the_run_even_with_a_cancel_pending |
| test_recovery::FixRound2::test_cancel_with_the_run_branch_gone_fails_with_every_accepted_card_missing | recovery::fix_round2::test_cancel_with_the_run_branch_gone_fails_with_every_accepted_card_missing |
| test_recovery::FixRound2::test_run_branch_deleted_after_acceptance_ends_failed_on_relaunch | recovery::fix_round2::test_run_branch_deleted_after_acceptance_ends_failed_on_relaunch |
| test_recovery::RunnerHelpers::test_runner_state | internal: Rust unit test (dispatch) |
| test_recovery::RunnerHelpers::test_stop_strays_stops_what_a_dead_runner_left_and_leaves_a_live_runner_alone | recovery::runner_helpers::test_stop_strays_stops_what_a_dead_runner_left_and_leaves_a_live_runner_alone |
| test_recovery::EveryCrashPoint::test_restart_from_every_crash_point_reaches_same_final_state | recovery::every_crash_point::test_restart_from_every_crash_point_reaches_same_final_state |
| test_engine_recovery::ResumeAfterBlocked::test_blocked_provider_capacity_resumes_with_a_new_dispatch | recovery::resume_after_blocked::test_blocked_provider_capacity_resumes_with_a_new_dispatch |
| test_engine_recovery::ResumeAfterBlocked::test_blocked_environment_resumes_with_a_fresh_retry_budget | recovery::resume_after_blocked::test_blocked_environment_resumes_with_a_fresh_retry_budget |
| test_engine_recovery::ResumeAfterBlocked::test_relaunch_without_an_outcome_keeps_the_retry_budget | recovery::resume_after_blocked::test_relaunch_without_an_outcome_keeps_the_retry_budget |
| test_engine_recovery::ResumeAfterBlocked::test_park_after_the_run_ended_raises | recovery::resume_after_blocked::test_park_after_the_run_ended_raises |
| test_engine_recovery::RunCollisions::test_start_returns_a_blocked_run_as_existing | recovery::run_collisions::test_start_returns_a_blocked_run_as_existing |
| test_engine_recovery::RunCollisions::test_new_run_suffixes_a_branch_owned_by_another_run | recovery::run_collisions::test_new_run_suffixes_a_branch_owned_by_another_run |
| test_engine_recovery::RunCollisions::test_start_after_a_finished_run_suffixes_its_branch | recovery::run_collisions::test_start_after_a_finished_run_suffixes_its_branch |
| test_engine_recovery::RunCollisions::test_interrupted_start_retry_uses_a_free_branch | recovery::run_collisions::test_interrupted_start_retry_uses_a_free_branch |
| test_engine_recovery::RunCollisions::test_unrecorded_existing_branch_is_not_reused | recovery::run_collisions::test_unrecorded_existing_branch_is_not_reused |
| test_engine_recovery::RunCollisions::test_existing_card_branch_makes_its_run_branch_name_taken | recovery::run_collisions::test_existing_card_branch_makes_its_run_branch_name_taken |
| test_engine_recovery::RunCollisions::test_existing_final_branch_makes_its_run_branch_name_taken | recovery::run_collisions::test_existing_final_branch_makes_its_run_branch_name_taken |
| test_engine_recovery::RunCollisions::test_card_branch_the_run_did_not_cut_is_refused | recovery::run_collisions::test_card_branch_the_run_did_not_cut_is_refused |
| test_engine_recovery::RunCollisions::test_new_run_without_worktree_ignores_a_stale_scratch_report | dropped with in-place mode |
| test_engine_recovery::CodexStops::test_hung_proxy_over_a_finished_runner_keeps_its_status | recovery::codex_stops::test_hung_proxy_over_a_finished_runner_keeps_its_status |
| test_engine_recovery::CodexStops::test_codex_stop_before_the_proxy_ran_prewrites_the_request | recovery::codex_stops::test_codex_stop_before_the_proxy_ran_prewrites_the_request |
| test_engine_recovery::Untested::test_reviewer_spent_twice_parks_the_card | recovery::untested::test_reviewer_spent_twice_parks_the_card |
| test_engine_recovery::Untested::test_pinned_vendor_not_installed_parks_the_card | recovery::untested::test_pinned_vendor_not_installed_parks_the_card |
| test_engine_recovery::Untested::test_worker_crash_before_commit_escalates | recovery::untested::test_worker_crash_before_commit_escalates |
| test_engine_recovery::Untested::test_worker_crash_after_commit_keeps_the_commit | recovery::untested::test_worker_crash_after_commit_keeps_the_commit |
| test_engine_recovery::Untested::test_crash_before_result_record_writes_nothing | recovery::untested::test_crash_before_result_record_writes_nothing |
| test_security::H1ForgedCodexBinary::test_runner_never_runs_a_binary_named_in_request_json | security::h1_forged_codex_binary::test_runner_never_runs_a_binary_named_in_request_json |
| test_security::H1ForgedCodexBinary::test_test_override_is_honoured_only_under_leadhand_testing | security::h1_forged_codex_binary::test_test_override_is_honoured_only_under_leadhand_testing |
| test_security::H1ForgedCodexBinary::test_request_json_carries_no_executable | security::h1_forged_codex_binary::test_request_json_carries_no_executable |
| test_security::H2ConfinedDispatch::test_cwd_in_another_repository_is_refused | security::h2_confined_dispatch::test_cwd_in_another_repository_is_refused |
| test_security::H2ConfinedDispatch::test_run_dir_other_than_the_sessions_is_refused | security::h2_confined_dispatch::test_run_dir_other_than_the_sessions_is_refused |
| test_security::H2ConfinedDispatch::test_without_the_session_variable_a_dir_without_run_json_is_refused | security::h2_confined_dispatch::test_without_the_session_variable_a_dir_without_run_json_is_refused |
| test_security::H2ConfinedDispatch::test_without_the_session_variable_a_run_owned_by_a_live_supervisor_is_refused | security::h2_confined_dispatch::test_without_the_session_variable_a_run_owned_by_a_live_supervisor_is_refused |
| test_security::H2ConfinedDispatch::test_an_empty_session_variable_is_refused_not_read_as_unset | security::h2_confined_dispatch::test_an_empty_session_variable_is_refused_not_read_as_unset |
| test_security::H2ConfinedDispatch::test_run_dir_inside_its_own_workspace_is_refused | security::h2_confined_dispatch::test_run_dir_inside_its_own_workspace_is_refused |
| test_security::H2ConfinedDispatch::test_brief_or_report_outside_the_workspace_scratch_dir_is_refused | security::h2_confined_dispatch::test_brief_or_report_outside_the_workspace_scratch_dir_is_refused |
| test_security::H2ConfinedDispatch::test_the_sessions_run_in_its_workspace_is_accepted | security::h2_confined_dispatch::test_the_sessions_run_in_its_workspace_is_accepted |
| test_security::H2ConfinedDispatch::test_the_runs_evidence_base_worktree_is_accepted_as_cwd | security::h2_confined_dispatch::test_the_runs_evidence_base_worktree_is_accepted_as_cwd |
| test_security::M1ConfinedStop::test_stop_dispatch_refuses_a_forged_folder_outside_the_run | security::m1_confined_stop::test_stop_dispatch_refuses_a_forged_folder_outside_the_run |
| test_security::M1ConfinedStop::test_stop_dispatch_refuses_a_dispatch_id_that_leaves_the_workers_folder | security::m1_confined_stop::test_stop_dispatch_refuses_a_dispatch_id_that_leaves_the_workers_folder |
| test_security::M1ConfinedStop::test_stop_dispatch_refuses_a_worker_folder_that_is_a_symlink_out_of_the_run | security::m1_confined_stop::test_stop_dispatch_refuses_a_worker_folder_that_is_a_symlink_out_of_the_run |
| test_security::M1ConfinedStop::test_stop_dispatch_refuses_a_request_for_another_repository | security::m1_confined_stop::test_stop_dispatch_refuses_a_request_for_another_repository |
| test_security::M1ConfinedStop::test_runner_commit_refuses_a_request_for_another_repository | security::m1_confined_stop::test_runner_commit_refuses_a_request_for_another_repository |
| test_security::M1ConfinedStop::test_runner_refuses_a_folder_outside_the_run | security::m1_confined_stop::test_runner_refuses_a_folder_outside_the_run |
| test_security::M1ConfinedStop::test_runner_refuses_a_request_for_another_repository | security::m1_confined_stop::test_runner_refuses_a_request_for_another_repository |
| test_security::H3FilterDrivers::test_the_global_filter_fetches_for_plain_git | security::h3_filter_drivers::test_checkouts_resets_and_stashes_fetch_nothing |
| test_security::H3FilterDrivers::test_checkout_reset_and_stash_fetch_nothing | security::h3_filter_drivers::test_checkouts_resets_and_stashes_fetch_nothing |
| test_security::H3FilterDrivers::test_runner_git_calls_fetch_nothing | security::h3_filter_drivers::test_runner_commits_lfs_files_as_pointers_and_fetches_nothing |
| test_security::H3FilterDrivers::test_every_call_carries_the_environment | security::h3_filter_drivers::test_checkouts_resets_and_stashes_fetch_nothing; security::h3_filter_drivers::test_runner_commits_lfs_files_as_pointers_and_fetches_nothing |
| test_security::H3FilterDrivers::test_checked_out_lfs_content_does_not_read_as_modified | security::h3_filter_drivers::test_checked_out_lfs_content_does_not_read_as_modified |
| test_security::H3FilterDrivers::test_runner_commits_lfs_files_as_pointers | security::h3_filter_drivers::test_runner_commits_lfs_files_as_pointers_and_fetches_nothing |
| test_security::M3SymlinkedBaseWorktree::test_evidence_reset_refuses_a_symlinked_base_worktree | security::m3_symlinked_base_worktree::test_evidence_reset_refuses_a_symlinked_base_worktree |
| test_security::M3SymlinkedBaseWorktree::test_evidence_reset_refuses_a_symlinked_worktrees_folder | security::m3_symlinked_base_worktree::test_evidence_reset_refuses_a_symlinked_worktrees_folder |
| test_security::M3SymlinkedBaseWorktree::test_prepare_workspace_refuses_a_symlinked_worktree | security::m3_symlinked_base_worktree::test_start_refuses_a_symlinked_worktree |
| test_security::L1ReplyFile::test_another_path_is_refused | security::l1_reply_file::test_another_path_is_refused |
| test_security::L1ReplyFile::test_the_reply_file_as_a_symlink_is_refused | security::l1_reply_file::test_the_reply_file_as_a_symlink_is_refused |
| test_security::L1ReplyFile::test_a_symlinked_scratch_folder_is_refused | security::l1_reply_file::test_a_symlinked_scratch_folder_is_refused |
| test_security::L1ReplyFile::test_a_report_behind_a_symlinked_scratch_folder_is_never_copied | security::l1_reply_file::test_a_report_behind_a_symlinked_scratch_folder_is_never_copied |
| test_security::L2SymlinkedScratch::test_a_symlinked_dispatch_folder_is_refused | security::l2_symlinked_scratch::test_a_symlinked_dispatch_folder_is_refused; worktree_safety::scratch_folders::test_symlinked_scratch_folder_refused_without_writing |
| test_security::L2SymlinkedScratch::test_a_symlinked_card_folder_is_refused | security::l2_symlinked_scratch::test_a_symlinked_card_folder_is_refused |
| test_security::L2SymlinkedScratch::test_a_file_in_place_of_the_dispatch_folder_is_refused | security::l2_symlinked_scratch::test_a_file_in_place_of_the_dispatch_folder_is_refused |
| test_security::L6ToolsInsideTheWorkspace::test_run_refuses_a_cli_inside_the_workspace | security::l6_tools_inside_the_workspace::test_run_refuses_a_cli_inside_the_workspace |
| test_security::L6ToolsInsideTheWorkspace::test_run_refuses_a_plugin_dir_inside_the_workspace | security::l6_tools_inside_the_workspace::test_run_refuses_a_plugin_dir_inside_the_workspace |
| test_security::L6ToolsInsideTheWorkspace::test_run_accepts_a_plugin_dir_outside_the_workspace | security::l6_tools_inside_the_workspace::test_run_accepts_a_plugin_dir_outside_the_workspace |
| test_security::L6StartInPlace::test_start_refuses_an_in_place_run_of_the_repository_holding_the_cli | dropped with in-place mode |
| test_engine_in_place::InPlace::test_operators_untracked_files_survive_and_are_never_committed | worktree_safety::operator_checkout::test_operator_checkout_untouched_by_a_complete_run |
| test_engine_in_place::InPlace::test_undecodable_untracked_filename_survives_start | dropped with in-place mode |
| test_engine_in_place::InPlace::test_json_output_escapes_undecodable_names | worktree_safety::worktree_rewrites::test_json_output_escapes_undecodable_names |
| test_engine_in_place::InPlace::test_run_started_in_a_subdirectory_preserves_a_parked_cards_untracked_file | dropped with in-place mode |
| test_engine_in_place::InPlace::test_run_started_in_a_subdirectory_leaves_untracked_files_outside_it_alone | dropped with in-place mode |
| test_engine_in_place::InPlace::test_docs_dir_elsewhere_in_the_checkout_of_a_subdirectory_run_is_refused | dropped with in-place mode |
| test_engine_in_place::InPlace::test_ignored_file_protection_is_for_in_place_runs_only | worktree_safety::worktree_rewrites::test_an_ignored_file_a_worker_left_does_not_block_a_card_cut |
| test_engine_in_place::Exclusive::test_second_in_place_run_is_refused_while_the_first_is_unfinished | dropped with in-place mode |
| test_engine_in_place::Exclusive::test_in_place_start_is_refused_while_a_run_in_another_runs_folder_is_supervised | dropped with in-place mode |
| test_engine_in_place::Exclusive::test_in_place_start_from_a_subdirectory_is_refused_while_the_checkout_is_supervised | dropped with in-place mode |
| test_engine_in_place::Exclusive::test_in_place_start_from_a_subdirectory_is_refused_while_a_run_is_unfinished | dropped with in-place mode |
| test_engine_in_place::Exclusive::test_a_checkout_that_cannot_be_flocked_refuses_an_in_place_start_clearly | dropped with in-place mode |
| test_engine_in_place::Exclusive::test_start_returns_the_unfinished_run_while_its_own_supervisor_holds_the_checkout | dropped with in-place mode |
| test_engine_in_place::Exclusive::test_worktree_runs_are_not_held_back_by_an_in_place_run | dropped with in-place mode |
| test_gitops::GitopsTests::test_prepare_workspace_without_worktree_uses_checkout | dropped with in-place mode |
| test_gitops::GitopsTests::test_cut_card_branch_refuses_an_existing_branch_the_run_did_not_record | worktree_safety::unrecorded_card_branch::test_unrecorded_card_branch_adopted_only_at_run_tip |
| test_gitops::GitopsTests::test_fast_forward_moves_a_run_branch_checked_out_in_the_workspace | worktree_safety::worktree_rewrites::test_accept_moves_a_run_branch_checked_out_in_the_worktree |
| test_gitops::GitopsTests::test_prepare_workspace_without_worktree_refuses_uncommitted_changes | dropped with in-place mode |
| test_gitops::GitopsTests::test_default_base_after_a_run_without_worktree | dropped with in-place mode |
| test_gitops::SubdirectoryWorkspaceTests::test_stage_work_takes_a_new_file_in_the_subdirectory | dropped with in-place mode |
| test_gitops::SubdirectoryWorkspaceTests::test_stage_work_leaves_out_a_gitlink_in_the_subdirectory | dropped with in-place mode |
| test_gitops::SubdirectoryWorkspaceTests::test_preserve_keeps_and_removes_untracked_files_named_from_the_top | dropped with in-place mode |
| test_gitops::SubdirectoryWorkspaceTests::test_preserve_leaves_untracked_files_outside_the_workspace_alone | dropped with in-place mode |
| test_gitops::SubdirectoryWorkspaceTests::test_stage_work_never_takes_a_new_file_outside_the_workspace | dropped with in-place mode |
| test_gitops::SubdirectoryWorkspaceTests::test_cleanliness_probes_see_the_whole_checkout_whatever_diff_relative_says | dropped with in-place mode |
| test_gitops::SubdirectoryWorkspaceTests::test_patches_keep_the_standard_prefixes_whatever_the_users_diff_settings | worktree_safety::worktree_rewrites::test_check_diff_patch_keeps_the_standard_prefixes_whatever_the_repo_diff_settings |
| test_gitops::SubdirectoryWorkspaceTests::test_preserve_never_removes_the_workspace_folder_itself | dropped with in-place mode |
| test_gitops::OperatorFilesInPlaceTests::test_start_refuses_a_base_that_would_overwrite_an_ignored_file | dropped with in-place mode |
| test_gitops::OperatorFilesInPlaceTests::test_switching_to_a_card_branch_never_overwrites_an_ignored_file | dropped with in-place mode |
| test_gitops::OperatorFilesInPlaceTests::test_fast_forward_never_overwrites_an_ignored_file | dropped with in-place mode |
| test_gitops::OperatorFilesInPlaceTests::test_rebase_never_overwrites_an_ignored_file | dropped with in-place mode |
| test_gitops::OperatorFilesInPlaceTests::test_rebase_never_overwrites_an_ignored_file_an_intermediate_commit_adds | dropped with in-place mode |
| test_gitops::OperatorFilesInPlaceTests::test_rebase_never_replaces_an_ignored_file_with_a_folder | dropped with in-place mode |
| test_gitops::OperatorFilesInPlaceTests::test_start_refuses_a_folder_over_an_ignored_file_before_changing_anything | dropped with in-place mode |
| test_gitops::OperatorFilesInPlaceTests::test_a_folder_of_tracked_files_a_rebase_turns_into_a_file_is_not_in_the_way | dropped with in-place mode |
| test_gitops::OperatorFilesInPlaceTests::test_rebase_never_replaces_a_folder_holding_an_ignored_file_with_a_file | dropped with in-place mode |
| test_gitops::OperatorFilesInPlaceTests::test_a_fold_equal_name_that_is_another_file_is_still_in_the_way | dropped with in-place mode |
| test_gitops::OperatorFilesInPlaceTests::test_a_case_only_rename_is_not_in_the_way_on_a_case_insensitive_file_system | dropped with in-place mode |
| test_gitops::OperatorFilesInPlaceTests::test_a_worktree_run_may_overwrite_an_ignored_file_it_holds | worktree_safety::worktree_rewrites::test_an_ignored_file_a_worker_left_does_not_block_a_card_cut |
| test_gitops::OperatorFilesInPlaceTests::test_a_conflicted_merge_is_refused_as_uncommitted_changes | dropped with in-place mode |
| test_gitops::OperatorFilesInPlaceTests::test_a_sparse_checkout_is_refused_in_place_with_short_advice | worktree_safety::sparse_checkouts::test_sparse_checkout_refused_by_check_start_and_run |
| test_gitops::OperatorFilesInPlaceTests::test_start_refuses_hidden_index_entries_in_place | dropped with in-place mode |
| test_git_safety::EngineGitCalls::test_engine_git_calls_run_no_fsmonitor_or_hook | security::git_safety::test_leadhand_git_calls_run_no_fsmonitor_or_hook |
| test_git_safety::EngineGitCalls::test_safe_options_are_passed_on_every_call | security::git_safety::test_leadhand_git_calls_run_no_fsmonitor_or_hook; worktree_safety::saved_state::test_a_nested_repository_at_a_gitlink_is_not_a_violation |
| test_git_safety::RunnerGitlinks::test_runner_never_commits_a_gitlink | security::git_safety::test_runner_never_commits_a_gitlink |
| test_gitops::GitopsTests::test_default_base_uses_origin_default_branch | internal: Rust unit test (git) |
| test_gitops::GitopsTests::test_default_base_without_origin_uses_local_default | internal: Rust unit test (git) |
| test_gitops::GitopsTests::test_prepare_workspace_is_idempotent_and_excludes_paths | worktree_safety::workspace::test_start_adds_each_exclude_once_and_the_run_worktree_on_the_run_branch |
| test_gitops::GitopsTests::test_cut_card_branch_from_run_tip_and_reuse | smoke::smoke::test_two_cards_accepted_in_order; integration::integration::test_resumed_parked_card_is_rebased_rereviewed_verified_and_fast_forwarded |
| test_gitops::GitopsTests::test_fast_forward_is_idempotent | cards::crashes::test_crash_after_fast_forward_before_accepted_reconciles |
| test_gitops::GitopsTests::test_fast_forward_refuses_divergence | cards::run_head_check::test_run_branch_moved_between_dispatches_fails_the_run |
| test_gitops::GitopsTests::test_needs_integration_after_independent_accept | integration::integration::test_resumed_parked_card_is_rebased_rereviewed_verified_and_fast_forwarded |
| test_gitops::GitopsTests::test_is_ancestor | internal: Rust unit test (git) |
| test_gitops::GitopsTests::test_rebase_onto_success | integration::integration::test_resumed_parked_card_is_rebased_rereviewed_verified_and_fast_forwarded |
| test_gitops::GitopsTests::test_rebase_onto_conflict_returns_paths_and_leaves_branch_unchanged | integration::integration::test_integration_conflict_requests_integrate_attempt |
| test_gitops::GitopsTests::test_detect_violation_head_moved | integration::violations::test_reviewer_moving_head_is_violation; worktree_safety::saved_state::test_a_moved_card_tip_is_kept_when_head_is_detached |
| test_gitops::GitopsTests::test_detect_violation_index_dirty | integration::violations::test_verifier_staged_change_is_violation |
| test_gitops::GitopsTests::test_detect_violation_tracked_change | integration::violations::test_reviewer_tracked_change_voids_review_and_reruns; worktree_safety::saved_state::test_a_tracked_edit_and_a_deletion_are_both_listed |
| test_gitops::GitopsTests::test_untracked_build_output_is_not_a_violation | integration::violations::test_untracked_build_output_is_not_a_violation; worktree_safety::saved_state::test_a_stat_only_touch_and_untracked_output_are_not_a_violation |
| test_gitops::GitopsTests::test_preserve_keeps_staged_and_unstaged_versions_separately | integration::violations::test_violation_preserves_index_and_worktree_on_ref |
| test_gitops::GitopsTests::test_preserve_keeps_staged_change_whose_working_copy_was_restored | worktree_safety::saved_state::test_a_staged_change_whose_working_copy_was_put_back_is_kept |
| test_gitops::GitopsTests::test_restore_leaves_expected_head_and_clean_index | integration::violations::test_reviewer_moving_head_is_violation; worktree_safety::saved_state::test_restore_returns_to_the_card_from_another_branch_and_keeps_untracked_files |
| test_gitops::GitopsTests::test_preserve_refuses_a_ref_outside_refs | internal: Rust unit test (git) |
| test_gitops::GitopsTests::test_commits_created_by_library_are_never_signed | worktree_safety::saved_state::test_a_conflicted_merge_is_saved_and_cleared_without_signing; worktree_safety::workspace::test_the_integration_rebase_is_never_signed |
| test_gitops::GitopsTests::test_preserve_keeps_a_moved_branch_tip_when_head_is_detached | worktree_safety::saved_state::test_a_moved_card_tip_is_kept_when_head_is_detached |
| test_gitops::GitopsTests::test_preserve_keeps_a_moved_branch_tip_when_another_branch_is_checked_out | worktree_safety::saved_state::test_a_moved_card_tip_is_kept_when_another_branch_is_checked_out |
| test_gitops::GitopsTests::test_preserve_recreates_a_deleted_branch | worktree_safety::saved_state::test_a_deleted_card_branch_is_recreated |
| test_gitops::GitopsTests::test_preserve_does_not_keep_a_branch_tip_already_in_the_saved_commit | worktree_safety::saved_state::test_a_card_tip_already_in_the_saved_commit_is_not_kept_twice |
| test_gitops::GitopsTests::test_preserve_saves_and_clears_a_conflicted_merge | worktree_safety::saved_state::test_a_conflicted_merge_is_saved_and_cleared_without_signing |
| test_gitops::GitopsTests::test_preserve_saves_and_clears_a_conflicted_rebase_and_cherry_pick | worktree_safety::saved_state::test_a_conflicted_rebase_or_cherry_pick_is_saved_and_cleared |
| test_gitops::GitopsTests::test_preserve_keeps_a_conflicted_merge_autostash | worktree_safety::saved_state::test_an_operations_autostash_is_kept_beside_the_saved_commit |
| test_gitops::GitopsTests::test_preserve_keeps_a_conflicted_rebase_autostash | worktree_safety::saved_state::test_an_operations_autostash_is_kept_beside_the_saved_commit |
| test_gitops::GitopsTests::test_preserve_drops_a_stale_branch_sibling | internal: Rust unit test (git) |
| test_gitops::GitopsTests::test_prepare_workspace_rejects_unsafe_slugs | internal: Rust unit test (git) |
| test_gitops::GitopsTests::test_paths_with_spaces | worktree_safety::saved_state::test_paths_with_spaces_and_non_ascii_names; cards::paths::test_spaces_everywhere_from_unrelated_cwd |
| test_gitops::GitopsTests::test_git_helpers_refuse_paths_outside_the_fixture | internal: Rust unit test (testing-tools) |
| test_gitops::GitopsTests::test_git_refuses_a_missing_workspace_directory | internal: Rust unit test (git) |
| test_gitops::GitopsTests::test_git_error_includes_stderr | internal: Rust unit test (git) |
| test_gitops::GitopsTests::test_is_test_or_doc_path | cards::loop_::test_which_paths_make_a_fix_test_only |
| test_state::RunPathsTests::test_run_paths_names | internal: Rust unit test (state) |
| test_state::RunPathsTests::test_new_run_dir_suffixes_existing | cards::start::test_run_folders_are_dated_and_suffixed |
| test_state::RunPathsTests::test_new_run_dir_rejects_path_in_slug | internal: Rust unit test (state) |
| test_state::LockTests::test_state_lock_is_reentrant_within_a_process | internal: Rust unit test (state) |
| test_state::LockTests::test_state_lock_released_after_exception | internal: Rust unit test (state) |
| test_state::LockTests::test_state_lock_is_not_the_run_lock | status::state_lock::test_state_lock_is_not_the_run_lock |
| test_state::AppendTests::test_append_event_assigns_seq_and_fsyncs | cards::start::test_every_event_line_carries_seq_ts_and_kind |
| test_state::AppendTests::test_append_attempt_and_read_attempts | internal: Rust unit test (state) |
| test_state::AppendTests::test_read_missing_files_are_empty | internal: Rust unit test (state) |
| test_state::AppendTests::test_read_events_ignores_partial_trailing_line | status::status::test_partial_last_line_is_ignored; recovery::reconcile::test_restart_after_truncated_final_event_then_append |
| test_state::AppendTests::test_read_events_ignores_complete_json_without_newline | status::status::test_complete_json_without_a_newline_is_ignored |
| test_state::AppendTests::test_read_attempts_ignores_partial_trailing_line | recovery::reconcile::test_appends_repair_a_torn_last_line_in_any_record_file |
| test_state::AppendTests::test_read_events_raises_on_corrupt_complete_line | status::status::test_corrupt_event_line_is_an_attention_line_not_a_failure; finish::start::test_start_skips_an_old_run_whose_event_log_is_unreadable |
| test_state::AppendTests::test_read_holds_state_lock_so_tail_repair_cannot_tear_it | status::state_lock::test_writers_and_readers_wait_while_another_process_holds_state_lock |
| test_state::AppendTests::test_read_of_missing_run_dir_is_empty | internal: Rust unit test (state) |
| test_state::AppendTests::test_append_after_truncated_final_record_stays_valid | recovery::reconcile::test_restart_after_truncated_final_event_then_append |
| test_state::AppendTests::test_append_when_whole_file_is_one_partial_line | recovery::reconcile::test_appends_repair_a_torn_last_line_in_any_record_file |
| test_state::AppendTests::test_append_jsonl_repairs_partial_line_in_any_file | recovery::reconcile::test_appends_repair_a_torn_last_line_in_any_record_file |
| test_state::AppendTests::test_failed_serialisation_leaves_file_intact | internal: Rust unit test (state) |
| test_state::AppendTests::test_unknown_event_kind_rejected | internal: Rust unit test (state) |
| test_state::AppendTests::test_reserved_field_names_rejected | internal: Rust unit test (state) |
| test_state::AppendTests::test_concurrent_appends_from_threads | internal: Rust unit test (state) |
| test_state::AppendTests::test_concurrent_appends_from_two_processes_keep_whole_lines_and_unique_seq | usage::concurrency::test_concurrent_supervisor_and_runner_charges_keep_exact_totals |
| test_state::WriteJsonAtomicTests::test_undecodable_filenames_round_trip_through_json_and_jsonl | worktree_safety::worktree_rewrites::test_json_output_escapes_undecodable_names; worktree_safety::worktree_rewrites::test_events_jsonl_keeps_undecodable_names_as_escapes |
| test_state::WriteJsonAtomicTests::test_write_json_atomic_leaves_no_temp_files | cards::start::test_a_whole_run_leaves_no_temporary_files |
| test_state::WriteJsonAtomicTests::test_write_json_atomic_error_keeps_old_content_and_no_temp | internal: Rust unit test (state) |
| test_state::WriteJsonAtomicTests::test_write_json_atomic_replace_failure_cleans_up | internal: Rust unit test (state) |
| test_state::WriteJsonAtomicTests::test_write_json_atomic_fsyncs | internal: Rust unit test (state) |
| test_state::CrashPointTests::test_crash_point_exits_97_only_when_testing | recovery::crash_protocol::test_a_crash_point_exits_97_only_when_testing_and_named |
| test_state::CrashPointTests::test_crash_points_listed | recovery::every_crash_point::test_restart_from_every_crash_point_reaches_same_final_state; recovery::pause_cancel::test_crash_after_cancel_request_finishes_the_cancel_on_restart |
| test_state::CrashPointTests::test_event_kinds_listed | internal: Rust unit test (state) |
| test_state::FoldCardsTests::test_fold_cards_tracks_requested_started_and_accepting | internal: Rust unit test (state) |
| test_state::FoldCardsTests::test_fold_cards_fix_round | internal: Rust unit test (state) |
| test_state::FoldCardsTests::test_fold_cards_integration_resets_verdict | internal: Rust unit test (state) |
| test_state::FoldCardsTests::test_fold_cards_park_and_unpark | internal: Rust unit test (state) |
| test_state::FoldCardsTests::test_fold_cards_unparked_without_branch_is_pending | internal: Rust unit test (state) |
| test_state::FoldCardsTests::test_fold_cards_accepted_is_terminal_and_unknown_cards_ignored | internal: Rust unit test (state) |
| test_state::FoldCardsTests::test_fold_cards_ignores_stale_dispatch_events | internal: Rust unit test (state) |
| test_state::FoldCardsTests::test_fold_cards_parked_stays_parked_on_late_events | internal: Rust unit test (state) |
| test_state::FoldCardsTests::test_fold_cards_accepted_overrides_parked | internal: Rust unit test (state) |
| test_state::FoldCardsTests::test_fold_cards_exposes_dispatch_kind_for_phase_disambiguation | internal: Rust unit test (state) |
| test_state::FoldCardsTests::test_card_states_constant | internal: Rust unit test (state) |
| test_state::OutcomeTests::test_all_accepted_and_current_is_complete | internal: Rust unit test (state) |
| test_state::OutcomeTests::test_removed_cards_count_as_done | internal: Rust unit test (state) |
| test_state::OutcomeTests::test_complete_requires_current_gates | internal: Rust unit test (state) |
| test_state::OutcomeTests::test_parked_card_makes_incomplete | internal: Rust unit test (state) |
| test_state::OutcomeTests::test_unfinished_card_makes_incomplete | internal: Rust unit test (state) |
| test_state::OutcomeTests::test_open_findings_make_incomplete | internal: Rust unit test (state) |
| test_state::OutcomeTests::test_holds_make_incomplete | internal: Rust unit test (state) |
| test_state::OutcomeTests::test_reasons_accumulate | internal: Rust unit test (state) |
| test_state::OutcomeTests::test_cancel_is_incomplete_cancelled | internal: Rust unit test (state) |
| test_state::OutcomeTests::test_pause_is_blocked_paused | internal: Rust unit test (state) |
| test_state::OutcomeTests::test_blocked_qualifiers | internal: Rust unit test (state) |
| test_state::OutcomeTests::test_unreconciled_is_failed | internal: Rust unit test (state) |
| test_state::OutcomeTests::test_verification_failed_is_failed | internal: Rust unit test (state) |
| test_state::OutcomeTests::test_first_match_wins | internal: Rust unit test (state) |
| test_state::OutcomeTests::test_outcome_values | internal: Rust unit test (state) |
| test_state::OutcomeTests::test_allowed_next_step_matrix | internal: Rust unit test (state) |
| test_test_hooks::ClockOffset::test_now_adds_offset_only_when_testing | cards::clock_offset_hook::test_clock_offset_is_honoured_only_under_leadhand_testing |
| test_test_hooks::ClockOffset::test_clock_offset_reaches_cli_deadlines | cards::clock_offset_hook::test_clock_offset_is_honoured_only_under_leadhand_testing |
| test_test_hooks::SelfPath::test_self_path_override_names_dispatch_line_and_runner | cards::paths::test_spaced_cli_path_from_unrelated_cwd |
| test_test_hooks::SelfPath::test_cli_path_ignores_override_without_testing | launch::self_path_hook::test_self_path_override_is_honoured_only_under_leadhand_testing |
| test_test_hooks::SelfPath::test_launch_line_uses_override | launch::self_path_hook::test_launch_and_pause_name_the_override; launch::self_path_hook::test_self_path_override_is_honoured_only_under_leadhand_testing |
| test_test_hooks::ForegroundWait::test_default_is_540_seconds | internal: Rust unit test (dispatch) |
| test_test_hooks::ForegroundWait::test_hook_is_honoured_under_testing | dispatch::runner_lifecycle::test_foreground_wait_ends_with_exit_75_and_the_still_running_line; dispatch::foreground_wait_hook::test_foreground_wait_hook_is_honoured_only_under_leadhand_testing |
| test_test_hooks::ForegroundWait::test_hook_is_ignored_without_testing | dispatch::foreground_wait_hook::test_foreground_wait_hook_is_honoured_only_under_leadhand_testing |
