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
