//! The command line itself: the version, usage errors, and the commands the run skill calls.
//!
//! The Rust form of the Python contract suite's `contract_cli` (converted from Python's `test_cli`,
//! plus the binary's half of test_supervisor_files.test_run_skill_commands_match_cli).

#[macro_use]
mod common;

mod version {
    use crate::common::{Env, TempRoot, cli};

    #[test]
    fn test_prints_package_version() {
        let tmp = TempRoot::new("lh cli ");
        let r = cli(&os_args!["version"], &Env::new(&tmp), &tmp).ok();
        assert_eq!(r.stdout_text(), "leadhand 0.2.1\n", "{r}");
    }
}

mod usage {
    use crate::common::{Env, TempRoot, cli};

    #[test]
    fn test_unknown_command_exits_2() {
        let tmp = TempRoot::new("lh cli ");
        let r = cli(&os_args!["nonsense"], &Env::new(&tmp), &tmp);
        assert_eq!(r.code, 2, "{r}");
    }
}

mod plugin_commands {
    use std::collections::BTreeMap;

    use crate::common::plugin::{cli_calls, flags, has_token, skill, split};
    use crate::common::{Env, Out, TempRoot, cli};

    #[test]
    fn test_every_cli_command_in_the_run_skill_is_a_registered_command() {
        // Facts both argparse and clap expose: `<name> --help` exits 0 for a registered
        // command, an unknown command exits 2, and the help names every flag and value.
        let tmp = TempRoot::new("lh cli ");
        let env = Env::new(&tmp);
        let (_, body) = split(&skill());
        let calls = cli_calls(&body);
        assert!(!calls.is_empty());
        let unknown = cli(&os_args!["no-such-command", "--help"], &env, &tmp);
        assert_eq!(unknown.code, 2, "{unknown}");
        let mut helps: BTreeMap<&str, Out> = BTreeMap::new();
        for (name, _) in &calls {
            if !helps.contains_key(name.as_str()) {
                helps.insert(name, cli(&os_args![name, "--help"], &env, &tmp).ok());
            }
        }
        for (name, args) in &calls {
            let help = &helps[name.as_str()];
            let text = help.stdout_text();
            for flag in flags(args) {
                assert!(
                    has_token(&text, &flag),
                    "{flag} of `<cli> {name}{args}` is not in\n{help}"
                );
            }
            if name == "record" {
                let kind = args.split_whitespace().nth(1).unwrap();
                assert!(
                    has_token(&text, kind),
                    "record kind {kind} is not in\n{help}"
                );
            }
        }
    }
}
