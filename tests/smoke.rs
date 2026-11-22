//! The fake supervisor end to end: a two-card run, the cleanup that leaves no runner behind,
//! and a crash drive whose group kill takes down every process the drive started.
//!
//! The Rust form of the Python contract suite's `contract_smoke` (itself converted from
//! test_engine_cards.HappyPath.test_happy_path_two_cards_accepted_in_order).

#[macro_use]
mod common;

mod smoke {
    use std::collections::BTreeMap;

    use crate::common::git::run_git;
    use crate::common::json::{self, Json};
    use crate::common::supervisor::{CardOpts, FakeSupervisor, card, card_with};

    #[test]
    fn test_two_cards_accepted_in_order() {
        let mut w = FakeSupervisor::new(&[
            card(1),
            card_with(
                2,
                CardOpts {
                    depends: &[1],
                    risk: "medium",
                    ..CardOpts::default()
                },
            ),
        ]);
        w.start();
        let actions = w.run();

        assert_eq!(
            w.dispatch_ids(),
            [
                "c1-a1-implement-1",
                "c1-a1-review-1",
                "c2-a1-implement-1",
                "c2-a1-review-1"
            ]
        );
        let (first, review) = (&w.started[0], &w.started[1]);
        assert_json_eq!(
            Json::from(vec![
                first["agent"].clone(),
                first["agent_model"].clone(),
                first["agent_effort"].clone()
            ]),
            Json::from(&["leadhand:implementer", "sonnet", "medium"][..])
        );
        assert_json_eq!(
            first["route"],
            json::parse(br#"{"vendor": "claude", "model": "sonnet", "effort": "medium"}"#)
        );
        assert!(first["resume"].is_null(), "{}", first["resume"]);
        let (report, message) = (first["report"].as_str(), first["message"].as_str());
        assert!(
            matches!((report, message), (Some(r), Some(m)) if m.contains(r)),
            "{report:?} not in {message:?}"
        );
        assert_json_eq!(review["agent"], Json::from("leadhand:reviewer"));
        assert_json_eq!(actions.last().unwrap()["action"], Json::from("done"));

        let accepted = w.events("accepted");
        let cards: Vec<Json> = accepted.iter().map(|e| e["card"].clone()).collect();
        assert_json_eq!(Json::from(cards), json::parse(b"[1, 2]"));
        let run_tip = w.branch_tip("leadhand/demo");
        assert_json_eq!(Json::from(run_tip.as_str()), accepted[1]["sha"]);
        let first_sha = accepted[0]["sha"].as_str().unwrap();
        run_git(
            &w.repo,
            &["merge-base", "--is-ancestor", first_sha, &run_tip],
        );
        let card_cuts = w.events("card_cut");
        let cuts: BTreeMap<i64, &Json> = card_cuts
            .iter()
            .map(|e| (e["card"].as_i64().unwrap(), e))
            .collect();
        assert_json_eq!(cuts[&1]["base"], Json::from(w.base.as_str()));
        assert_json_eq!(cuts[&2]["base"], accepted[0]["sha"]);
        for d in ["c1-a1-implement-1", "c2-a1-implement-1"] {
            let n = &d[1..2];
            assert_eq!(
                run_git(
                    &w.repo,
                    &["show", &format!("leadhand/demo:src/card{n}/{d}.txt")]
                ),
                d
            );
        }

        // every worker charged once under its role, with one attempt record naming the charge
        let charges: BTreeMap<Vec<u8>, Json> = w
            .charges()
            .into_iter()
            .map(|c| (c["charge_id"].as_bytes().unwrap().to_vec(), c))
            .collect();
        assert_eq!(charges.len(), 4);
        let attempts = w.attempts();
        let dispatches: Vec<Json> = attempts.iter().map(|a| a["dispatch"].clone()).collect();
        assert_json_eq!(Json::from(dispatches), Json::from(&w.dispatch_ids()[..]));
        for a in &attempts {
            let ids = a["charge_ids"].as_array().unwrap();
            assert_eq!(ids.len(), 1, "{a}");
            let c = &charges[ids[0].as_bytes().unwrap()];
            let expected = format!(
                "{}:claude:{}",
                a["dispatch"].as_str().unwrap(),
                a["worker_id"].as_str().unwrap()
            );
            assert_json_eq!(c["charge_id"], Json::from(expected));
            assert_json_eq!(c["role"], a["role"]);
            assert!(
                c["usage"]["known"].as_bool() == Some(true),
                "{}",
                c["usage"]
            );
        }
        // the run folder keeps a copy of each dispatch's brief, report and reply
        let folder = w
            .run_dir
            .as_ref()
            .unwrap()
            .join("cards")
            .join("1")
            .join("c1-a1-review-1");
        for name in ["brief.md", "report.md", "reply.md"] {
            assert!(folder.join(name).is_file(), "{name}");
        }
    }
}

mod cleanup {
    use std::time::{Duration, Instant};

    use crate::common::json::Json;
    use crate::common::read_json;
    use crate::common::supervisor::{FakeSupervisor, Options, Step, card, recorded_alive};

    #[test]
    fn test_no_runner_outlives_its_test() {
        // The world is dropped (its cleanups run) at the end of this block, then inspected.
        let records: Vec<Json> = {
            let mut w = FakeSupervisor::with_options(
                &[card(1)],
                Options {
                    installed: &["claude", "codex"],
                    ..Options::default()
                },
            );
            w.start();
            let a = w.next();
            assert_eq!(a["agent"], "leadhand:codex", "{a}");
            w.launch(&a);
            let proxy = w.start_proxy(
                &a,
                &Step {
                    sleep: Some(120.0),
                    ..Step::default()
                },
            );
            let run_dir = w.run_dir.clone().unwrap();
            let runner_json = run_dir
                .join("workers")
                .join(a["dispatch_id"].as_str().unwrap())
                .join("runner.json");
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                let data = if runner_json.exists() {
                    read_json(&runner_json)
                } else {
                    Json::object([])
                };
                let live = ["runner", "codex"].map(|who| data.get(who).cloned());
                if live.iter().all(|r| r.as_ref().is_some_and(recorded_alive)) {
                    break;
                }
                if proxy.poll().is_some() {
                    let output = match proxy.communicate(Duration::from_secs(10)) {
                        Some((out, err)) => format!(
                            "{}{}",
                            String::from_utf8_lossy(&out),
                            String::from_utf8_lossy(&err)
                        ),
                        // a descendant still holds the pipes
                        None => "(not read: a descendant held the pipes)".to_string(),
                    };
                    panic!(
                        "the dispatch line exited ({:?}) before its runner and Codex were both running:\n{output}",
                        proxy.returncode()
                    );
                }
                if Instant::now() > deadline {
                    panic!(
                        "no live runner and Codex in {} after 30 s: {data}",
                        runner_json.display()
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let mut found = Vec::new();
            for entry in std::fs::read_dir(run_dir.join("workers")).unwrap() {
                let path = entry.unwrap().path().join("runner.json");
                if path.exists() {
                    found.push(read_json(&path));
                }
            }
            found
        };
        assert!(!records.is_empty(), "{records:?}");
        let survivors: Vec<(&str, &Json)> = records
            .iter()
            .flat_map(|rec| {
                ["runner", "codex"]
                    .into_iter()
                    .filter_map(move |who| rec.get(who).map(|r| (who, r)))
            })
            .filter(|(_, r)| recorded_alive(r))
            .collect();
        assert_eq!(survivors, Vec::<(&str, &Json)>::new());
    }
}

mod crash_drive {
    use std::time::{Duration, Instant};

    use rustix::process::{Pid, Signal, kill_process};

    use crate::common::json::Json;
    use crate::common::read_jsonl;
    use crate::common::supervisor::{FakeSupervisor, Options, Step, card, recorded_alive};

    /// Kills, on drop, every process in `0` that is still the process it names.
    struct KillSurvivors(Vec<Json>);

    impl Drop for KillSurvivors {
        fn drop(&mut self) {
            for p in &self.0 {
                if recorded_alive(p)
                    && let Some(pid) = p["pid"].as_i64().and_then(|n| Pid::from_raw(n as i32))
                {
                    let _ = kill_process(pid, Signal::KILL);
                }
            }
        }
    }

    #[test]
    fn test_runner_crash_takes_down_everything_the_drive_started() {
        // The drive runs in a forked child that is its own process group, as a supervisor
        // session is: this test re-executed (FakeSupervisor::fork), which takes the world
        // from attach_drive. The runner's crash point kills that group: the dispatch line in
        // flight must die with it, as FakeWorld's in-process engine and proxies did. Codex
        // ignores SIGINT, so a dispatch line left alive would sit in its cancel of the
        // orphaned Codex for up to 30 s.
        let mut w = match FakeSupervisor::attach_drive("crash drive") {
            Some(w) => w,
            None => {
                let mut w = FakeSupervisor::with_options(
                    &[card(1)],
                    Options {
                        installed: &["claude", "codex"],
                        ..Options::default()
                    },
                );
                w.start();
                w
            }
        };
        w.script(
            1,
            "implement",
            [Step {
                sleep: Some(120.0),
                ..Step::default()
            }],
        );
        let started = w.tmp.join("started processes.jsonl");

        let forked = w.fork("crash drive", Duration::from_secs(120), |w, pid| {
            w.env
                .set("LEADHAND_CRASH_AT", "after_worker_launch")
                .set("LEADHAND_CRASH_KILL_PGID", pid.to_string())
                .set("FAKE_CODEX_IGNORE_SIGINT", "1");
            w.popen_log = Some(started.clone()); // every process the drive starts is logged
            w.run();
        });
        assert!(!forked.hung, "the crash drive hung");
        let procs = KillSurvivors(read_jsonl(&started));
        assert!(
            forked.error.is_none(),
            "{}",
            forked.error.as_deref().unwrap_or("")
        );
        assert!(
            procs.0.iter().any(|p| p["argv"]
                .as_array()
                .is_some_and(|argv| argv.get(1).is_some_and(|a| a == "dispatch"))),
            "{:?}",
            procs.0
        );
        let end = Instant::now() + Duration::from_secs(5);
        while procs.0.iter().any(recorded_alive) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(50));
        }
        let alive: Vec<&Json> = procs
            .0
            .iter()
            .filter(|p| recorded_alive(p))
            .map(|p| &p["argv"])
            .collect();
        assert_eq!(alive, Vec::<&Json>::new());
    }
}
