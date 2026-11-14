//! Configuration (`config.py`): the preset, the user file and the repository file, merged key
//! by key and validated with the Python's messages; the settings this version refuses; and the
//! snapshot `start` writes into a run.
//!
//! The files are TOML and must be UTF-8, as in the Python. Tables keep the order their keys
//! first appear in, as Python's `dict` does, so iteration (the step-limit refusals, the first
//! invalid role) and the snapshot follow the files. The presets are compiled in.
//!
//! The `toml` crate reads TOML 1.1, and Python's `tomllib` TOML 1.0, so a file using 1.1's
//! additions (newlines and a trailing comma in an inline table, the `\e` and `\xHH` escapes,
//! times without seconds) loads here and is refused by the Python. Every file the presets and
//! the contract suite use is TOML 1.0.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{Serialize, SerializeMap, Serializer};
use toml::de::{DeTable, DeValue};

use crate::error::{Error, Result};

pub const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
pub const VENDORS: [&str; 2] = ["claude", "codex"];
pub const PRICE_KEYS: [&str; 5] = [
    "input",
    "output",
    "cache_read",
    "cache_write_5m",
    "cache_write_1h",
];
pub const DEFAULT_PRESET: &str = "claude-codex";
/// The run's configuration snapshot, in the run directory.
pub const SNAPSHOT: &str = "config.json";
/// The presets' `capacity.codex_pause_percent`.
pub const DEFAULT_CODEX_PAUSE_PERCENT: i64 = 95;

const PRESETS: [(&str, &str); 2] = [
    ("claude-codex", include_str!("../presets/claude-codex.toml")),
    ("claude-only", include_str!("../presets/claude-only.toml")),
];

/// The Claude worker agents' bodies, for their `maxTurns` (`briefs.agent_max_turns`).
const AGENTS: [(&str, &str); 4] = [
    ("implementer", include_str!("../agents/implementer.md")),
    ("reviewer", include_str!("../agents/reviewer.md")),
    ("verifier", include_str!("../agents/verifier.md")),
    ("scout", include_str!("../agents/scout.md")),
];

/// The Claude agent each role's worker runs as (`briefs.ROLE_AGENT`).
const ROLE_AGENT: [(&str, &str); 7] = [
    ("implementer", "implementer"),
    ("implementer_hard", "implementer"),
    ("reviewer", "reviewer"),
    ("reviewer_hard", "reviewer"),
    ("final_reviewer", "reviewer"),
    ("verifier", "verifier"),
    ("scout", "scout"),
];

/// The `limits` key a role uses when it is not the role's own (`dispatch.LIMITS_ROLE`).
const LIMITS_ROLE: [(&str, &str); 3] = [
    ("implementer_hard", "implementer"),
    ("reviewer_hard", "reviewer"),
    ("final_reviewer", "reviewer"),
];

const TOP_KEYS: [&str; 11] = [
    "preset",
    "profile",
    "docs_dir",
    "run",
    "after_run",
    "capacity",
    "codex",
    "limits",
    "roles",
    "notify",
    "prices",
];

const NOT_SUPPORTED: &str = "not supported in this version: ";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    pub vendor: String,
    pub model: String,
    pub effort: String,
}

/// A role's routes (at most one per vendor, in `VENDORS` order) and its preferred vendor:
/// `claude`, `codex` or `other`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoleSpec {
    pub routes: Vec<Route>,
    pub prefer: String,
}

impl RoleSpec {
    pub fn route(&self, vendor: &str) -> Option<&Route> {
        self.routes.iter().find(|r| r.vendor == vendor)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Limits {
    pub steps: i64,
    pub minutes: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunSettings {
    pub worktree: bool,
    pub test_first: String,
    pub evidence: String,
    pub card_review: String,
    pub cost_limit_usd: f64,
    pub branch_prefix: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AfterRun {
    pub step: String,
    pub on_incomplete: String,
    pub repeat_safe: bool,
    pub check: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexSettings {
    pub network: bool,
    pub writable_dirs: Vec<String>,
}

/// $/MTok for one model family (`PRICE_KEYS`).
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
pub struct Prices {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
}

/// The effective configuration (`config.Config`). The keyed tables keep their files' order.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub profile: String,
    pub docs_dir: String,
    pub run: RunSettings,
    pub after_run: AfterRun,
    pub codex_pause_percent: i64,
    pub codex: CodexSettings,
    pub limits: Vec<(String, Limits)>,
    pub roles: Vec<(String, RoleSpec)>,
    pub notify_command: String,
    pub prices: Vec<(String, Prices)>,
}

impl Config {
    pub fn role(&self, name: &str) -> Option<&RoleSpec> {
        self.roles.iter().find(|(k, _)| k == name).map(|(_, v)| v)
    }

    pub fn limits_for(&self, key: &str) -> Option<&Limits> {
        self.limits.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn prices_for(&self, family: &str) -> Option<&Prices> {
        self.prices
            .iter()
            .find(|(k, _)| k == family)
            .map(|(_, v)| v)
    }
}

/// A setting this version refuses in a repository, and the command that fixes it, when one
/// command does (as an argument vector).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unsupported {
    pub message: String,
    pub fix: Option<Vec<OsString>>,
}

/// `<model> <effort>` for `vendor` (`config.parse_route`).
pub fn parse_route(vendor: &str, text: &str) -> Result<Route> {
    if !VENDORS.contains(&vendor) {
        return Err(Error::msg(format!("unknown vendor {}", py_repr(vendor))));
    }
    let parts: Vec<&str> = py_split(text).collect();
    let [model, effort] = parts[..] else {
        return Err(Error::msg(format!(
            "expected '<model> <effort>', got {}",
            py_repr(text)
        )));
    };
    if !EFFORTS.contains(&effort) {
        return Err(Error::msg(format!(
            "effort {} is not one of {}",
            py_repr(effort),
            EFFORTS.join(", ")
        )));
    }
    Ok(Route {
        vendor: vendor.into(),
        model: model.into(),
        effort: effort.into(),
    })
}

/// The configuration for the repository at `repo_top`: the preset, then the user file
/// (`$LEADHAND_CONFIG`, else `~/.config/leadhand/config.toml`), then `<repo_top>/.leadhand.toml`.
pub fn load_config(repo_top: &Path) -> Result<Config> {
    let user_file = match std::env::var_os("LEADHAND_CONFIG") {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => PathBuf::from(expand_user(OsStr::new("~"))).join(".config/leadhand/config.toml"),
    };
    load_config_from(repo_top, &user_file)
}

/// `load_config` with the user file named.
pub fn load_config_from(repo_top: &Path, user_file: &Path) -> Result<Config> {
    let user = read_file(user_file)?;
    let repo = read_file(&repo_top.join(".leadhand.toml"))?;
    let default = Val::Str(DEFAULT_PRESET.into());
    let name = match lookup(&repo, "preset")
        .or_else(|| lookup(&user, "preset"))
        .unwrap_or(&default)
    {
        Val::Str(n) if !n.is_empty() && !n.contains('/') && !n.starts_with('.') => n,
        other => return Err(cerr(format!("preset: invalid name {}", other.repr()))),
    };
    let Some((_, text)) = PRESETS.iter().find(|(n, _)| n == name) else {
        return Err(cerr(format!("preset: unknown preset {}", py_repr(name))));
    };
    let preset = parse_toml(text).map_err(|e| cerr(format!("preset {name}: {e}")))?;
    build(&merge(merge(preset, user), repo))
}

/// The run docs directory, always absolute: `{repo}` is the repository folder's name, `~` the
/// home directory, and a relative `docs_dir` is resolved against the repository
/// (`config.docs_dir_for`).
pub fn docs_dir_for(config: &Config, repo_top: &Path) -> PathBuf {
    let root = real_path(repo_top);
    let name = root.file_name().unwrap_or_default().as_bytes();
    let mut text = Vec::new();
    for (i, part) in config.docs_dir.split("{repo}").enumerate() {
        if i > 0 {
            text.extend_from_slice(name);
        }
        text.extend_from_slice(part.as_bytes());
    }
    let docs = PathBuf::from(expand_user(&OsString::from_vec(text)));
    if docs.is_absolute() {
        docs.components().collect()
    } else {
        real_path(&root.join(docs))
    }
}

/// The settings this version refuses whatever the repository (`config.unsupported_settings`
/// without a repository), plus deliberate change 1: in-place runs.
pub fn unsupported_settings(config: &Config) -> Vec<String> {
    let mut out = Vec::new();
    if !config.run.worktree {
        out.push(format!("{NOT_SUPPORTED}run.worktree = false"));
    }
    if config.after_run.step != "none" {
        out.push(format!(
            "{NOT_SUPPORTED}after_run.step = {}",
            config.after_run.step
        ));
    }
    if config.after_run.on_incomplete != "none" {
        out.push(format!(
            "{NOT_SUPPORTED}after_run.on_incomplete = {}",
            config.after_run.on_incomplete
        ));
    }
    let cost = config.run.cost_limit_usd;
    if cost != 0.0 {
        // `int(c) if c == int(c) else c`
        let shown = if cost.is_finite() && cost.fract() == 0.0 {
            format!("{cost:.0}")
        } else {
            py_float(cost)
        };
        out.push(format!("{NOT_SUPPORTED}run.cost_limit_usd = {shown}"));
    }
    out.extend(unhonoured_step_limits(config, &agent_max_turns));
    out
}

/// The settings this version refuses in the repository at `repo_top` (the rest of
/// `config.unsupported_settings`): a `docs_dir` inside the repository's checkout (ruling R48),
/// and a sparse checkout (R63), which one command turns off.
pub fn unsupported_in_repo(config: &Config, repo_top: &Path) -> Result<Vec<Unsupported>> {
    let mut out = Vec::new();
    let root = real_path(repo_top);
    let docs = real_path(&docs_dir_for(config, repo_top));
    if docs.starts_with(&root) {
        out.push(Unsupported {
            message: format!(
                "{NOT_SUPPORTED}docs_dir inside the repository ({})",
                docs.display()
            ),
            fix: None,
        });
    }
    if let Some(sparse) = crate::git::sparse_problem(repo_top)? {
        let fix = [
            OsStr::new("git"),
            OsStr::new("-C"),
            repo_top.as_os_str(),
            OsStr::new("sparse-checkout"),
            OsStr::new("disable"),
        ];
        out.push(Unsupported {
            message: format!("{NOT_SUPPORTED}{sparse}"),
            fix: Some(fix.map(OsStr::to_os_string).to_vec()),
        });
    }
    Ok(out)
}

/// Settings this version reads but does not act on yet: a warning, not an error.
pub fn unsupported_warnings(config: &Config) -> Vec<String> {
    let mut out = Vec::new();
    if !config.notify_command.is_empty() {
        out.push(format!(
            "{NOT_SUPPORTED}notify.command = {}",
            config.notify_command
        ));
    }
    if config.codex_pause_percent != DEFAULT_CODEX_PAUSE_PERCENT {
        out.push(format!(
            "{NOT_SUPPORTED}capacity.codex_pause_percent = {}",
            config.codex_pause_percent
        ));
    }
    out
}

/// `limits.<key>.steps` that a Claude worker cannot honour (ruling R52): a Claude worker stops
/// at its agent file's `maxTurns`, so a limit is refused when it differs from that for any role
/// using it that has a Claude route.
fn unhonoured_step_limits(config: &Config, max_turns: &dyn Fn(&str) -> Option<i64>) -> Vec<String> {
    let mut out = Vec::new();
    for (key, lim) in &config.limits {
        let turns: BTreeSet<i64> = config
            .roles
            .iter()
            .filter(|(role, spec)| spec.route("claude").is_some() && limits_key(role) == key)
            .filter_map(|(role, _)| ROLE_AGENT.iter().find(|(r, _)| r == role))
            .filter_map(|(_, agent)| max_turns(agent))
            .filter(|m| *m != lim.steps)
            .collect();
        for m in turns {
            out.push(format!(
                "{NOT_SUPPORTED}limits.{key}.steps = {} (Claude workers use the agent file's \
                 maxTurns = {m})",
                lim.steps
            ));
        }
    }
    out
}

fn limits_key(role: &str) -> &str {
    LIMITS_ROLE
        .iter()
        .find(|(r, _)| *r == role)
        .map_or(role, |(_, key)| key)
}

/// The `maxTurns` in a Claude worker agent's frontmatter.
fn agent_max_turns(agent: &str) -> Option<i64> {
    AGENTS
        .iter()
        .find(|(name, _)| *name == agent)
        .and_then(|(_, body)| max_turns(body))
}

/// `maxTurns` from an agent body's frontmatter, or `None` when it sets none.
fn max_turns(body: &str) -> Option<i64> {
    let (head, _) = body.strip_prefix("---\n")?.split_once("\n---\n")?;
    py_splitlines(head).into_iter().find_map(|line| {
        let (key, value) = line.split_once(':').unwrap_or((line, ""));
        let value = py_strip(value);
        let digits = !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit());
        (py_strip(key) == "maxTurns" && digits)
            .then(|| value.parse().ok())
            .flatten()
    })
}

// --------------------------------------------------------------------------
// The snapshot

/// The effective configuration as the plain table `config_from_snapshot` reads back into an
/// equal `Config` (`config.config_to_dict`), in its key order.
pub struct ConfigSnapshot<'a>(pub &'a Config);

impl Serialize for ConfigSnapshot<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        #[derive(serde::Serialize)]
        struct RunOut<'a> {
            worktree: bool,
            test_first: &'a str,
            evidence: &'a str,
            card_review: &'a str,
            cost_limit_usd: f64,
            branch_prefix: &'a str,
        }
        #[derive(serde::Serialize)]
        struct AfterOut<'a> {
            step: &'a str,
            on_incomplete: &'a str,
            repeat_safe: bool,
            check: &'a str,
        }
        #[derive(serde::Serialize)]
        struct CapacityOut {
            codex_pause_percent: i64,
        }
        #[derive(serde::Serialize)]
        struct CodexOut<'a> {
            network: bool,
            writable_dirs: &'a [String],
        }
        #[derive(serde::Serialize)]
        struct NotifyOut<'a> {
            command: &'a str,
        }
        let c = self.0;
        let mut m = s.serialize_map(Some(10))?;
        m.serialize_entry("profile", &c.profile)?;
        m.serialize_entry("docs_dir", &c.docs_dir)?;
        m.serialize_entry(
            "run",
            &RunOut {
                worktree: c.run.worktree,
                test_first: &c.run.test_first,
                evidence: &c.run.evidence,
                card_review: &c.run.card_review,
                cost_limit_usd: c.run.cost_limit_usd,
                branch_prefix: &c.run.branch_prefix,
            },
        )?;
        m.serialize_entry(
            "after_run",
            &AfterOut {
                step: &c.after_run.step,
                on_incomplete: &c.after_run.on_incomplete,
                repeat_safe: c.after_run.repeat_safe,
                check: &c.after_run.check,
            },
        )?;
        m.serialize_entry(
            "capacity",
            &CapacityOut {
                codex_pause_percent: c.codex_pause_percent,
            },
        )?;
        m.serialize_entry(
            "codex",
            &CodexOut {
                network: c.codex.network,
                writable_dirs: &c.codex.writable_dirs,
            },
        )?;
        m.serialize_entry("limits", &Entries(&c.limits))?;
        m.serialize_entry("roles", &Entries(&c.roles))?;
        m.serialize_entry(
            "notify",
            &NotifyOut {
                command: &c.notify_command,
            },
        )?;
        m.serialize_entry("prices", &Entries(&c.prices))?;
        m.end()
    }
}

/// A keyed table, written in its order.
struct Entries<'a, V>(&'a [(String, V)]);

impl<V: Serialize> Serialize for Entries<'_, V> {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(self.0.len()))?;
        for (k, v) in self.0 {
            m.serialize_entry(k, v)?;
        }
        m.end()
    }
}

impl Serialize for RoleSpec {
    /// `{<vendor>: "<model> <effort>", …, "prefer": …}`.
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(self.routes.len() + 1))?;
        for r in &self.routes {
            m.serialize_entry(&r.vendor, &format!("{} {}", r.model, r.effort))?;
        }
        m.serialize_entry("prefer", &self.prefer)?;
        m.end()
    }
}

/// The configuration from a snapshot's JSON text, with the same validation as the TOML files
/// (`config.config_from_dict`).
pub fn config_from_snapshot(json: &str) -> Result<Config> {
    let value: Val = serde_json::from_str(json)
        .map_err(|e| cerr(format!("the configuration snapshot is not JSON: {e}")))?;
    match value {
        Val::Table(t) => build(&t),
        _ => Err(cerr("the configuration snapshot is not a table")),
    }
}

// --------------------------------------------------------------------------
// Values: TOML or JSON, as Python holds them

/// A configuration value. Tables keep their keys' first-appearance order.
#[derive(Clone, Debug, PartialEq)]
enum Val {
    /// JSON `null` (snapshots only).
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    /// A TOML date or time, as written.
    Datetime(String),
    Array(Vec<Val>),
    Table(Table),
}

type Table = Vec<(String, Val)>;

static EMPTY: Table = Vec::new();

impl Val {
    /// Python's `repr` of the value, for messages.
    fn repr(&self) -> String {
        match self {
            Val::Null => "None".into(),
            Val::Bool(b) => if *b { "True" } else { "False" }.into(),
            Val::Int(n) => n.to_string(),
            Val::Float(x) => py_float(*x),
            Val::Str(s) => py_repr(s),
            Val::Datetime(d) => d.clone(),
            Val::Array(items) => {
                let shown: Vec<String> = items.iter().map(Val::repr).collect();
                format!("[{}]", shown.join(", "))
            }
            Val::Table(t) => {
                let shown: Vec<String> = t
                    .iter()
                    .map(|(k, v)| format!("{}: {}", py_repr(k), v.repr()))
                    .collect();
                format!("{{{}}}", shown.join(", "))
            }
        }
    }
}

fn lookup<'a>(t: &'a Table, key: &str) -> Option<&'a Val> {
    t.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// Sets `key` in place, or appends it, as assigning a `dict` key does.
fn insert(t: &mut Table, key: String, value: Val) {
    match t.iter_mut().find(|(k, _)| *k == key) {
        Some((_, v)) => *v = value,
        None => t.push((key, value)),
    }
}

/// `over` merged into `base`, table by table (`config._merge`).
fn merge(mut base: Table, over: Table) -> Table {
    for (k, v) in over {
        match (base.iter_mut().find(|(bk, _)| *bk == k), v) {
            (Some((_, Val::Table(b))), Val::Table(o)) => *b = merge(std::mem::take(b), o),
            (_, v) => insert(&mut base, k, v),
        }
    }
    base
}

/// The TOML file at `path`, or an empty table when there is no file (`config._read`).
fn read_file(path: &Path) -> Result<Table> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let bytes = std::fs::read(path).map_err(|e| Error::io(path, e))?;
    let text = String::from_utf8(bytes)
        .map_err(|e| cerr(format!("{}: not UTF-8: {e}", path.display())))?;
    parse_toml(&text).map_err(|e| cerr(format!("{}: {e}", path.display())))
}

/// A TOML document as an ordered table; the error as one line, with its position.
fn parse_toml(text: &str) -> std::result::Result<Table, String> {
    let doc = DeTable::parse(text).map_err(|e| {
        let message = e.message().trim().replace('\n', "; ");
        match e.span() {
            Some(span) => {
                let before = &text[..span.start.min(text.len())];
                let line = before.matches('\n').count() + 1;
                let column = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
                format!("{message} (at line {line}, column {column})")
            }
            None => message,
        }
    })?;
    from_de_table(doc.into_inner())
}

fn from_de_table(t: DeTable<'_>) -> std::result::Result<Table, String> {
    let mut entries: Vec<_> = t.into_iter().collect();
    // The order of first appearance, which `dict` keeps (the map itself is sorted).
    entries.sort_by_key(|(k, _)| k.span().start);
    entries
        .into_iter()
        .map(|(k, v)| Ok((k.into_inner().into_owned(), from_de_value(v.into_inner())?)))
        .collect()
}

fn from_de_value(v: DeValue<'_>) -> std::result::Result<Val, String> {
    Ok(match v {
        DeValue::String(s) => Val::Str(s.into_owned()),
        DeValue::Integer(n) => Val::Int(
            i64::from_str_radix(n.as_str(), n.radix())
                .map_err(|_| format!("integer {n} is out of range"))?,
        ),
        DeValue::Float(x) => Val::Float(
            x.as_str()
                .parse()
                .map_err(|_| format!("float {x} is out of range"))?,
        ),
        DeValue::Boolean(b) => Val::Bool(b),
        DeValue::Datetime(d) => Val::Datetime(d.to_string()),
        DeValue::Array(items) => Val::Array(
            items
                .into_iter()
                .map(|item| from_de_value(item.into_inner()))
                .collect::<std::result::Result<_, _>>()?,
        ),
        DeValue::Table(t) => Val::Table(from_de_table(t)?),
    })
}

impl<'de> Deserialize<'de> for Val {
    /// JSON in document order (`json.loads`; a repeated key keeps its first place).
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Val, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Val;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a JSON value")
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Val, E> {
                Ok(Val::Null)
            }
            fn visit_bool<E: de::Error>(self, b: bool) -> std::result::Result<Val, E> {
                Ok(Val::Bool(b))
            }
            fn visit_i64<E: de::Error>(self, n: i64) -> std::result::Result<Val, E> {
                Ok(Val::Int(n))
            }
            fn visit_u64<E: de::Error>(self, n: u64) -> std::result::Result<Val, E> {
                i64::try_from(n)
                    .map(Val::Int)
                    .map_err(|_| E::custom(format!("integer {n} is out of range")))
            }
            fn visit_f64<E: de::Error>(self, x: f64) -> std::result::Result<Val, E> {
                Ok(Val::Float(x))
            }
            fn visit_str<E: de::Error>(self, s: &str) -> std::result::Result<Val, E> {
                Ok(Val::Str(s.into()))
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Val, A::Error> {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element()? {
                    items.push(item);
                }
                Ok(Val::Array(items))
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Val, A::Error> {
                let mut t = Vec::new();
                while let Some((k, v)) = map.next_entry::<String, Val>()? {
                    insert(&mut t, k, v);
                }
                Ok(Val::Table(t))
            }
        }
        d.deserialize_any(V)
    }
}

// --------------------------------------------------------------------------
// Validation (`config._build`)

fn cerr(m: impl Into<String>) -> Error {
    Error::msg(m)
}

#[derive(Clone, Copy)]
enum Kind {
    Str,
    Bool,
    Int,
    Float,
    List,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Str => "str",
            Kind::Bool => "bool",
            Kind::Int => "int",
            Kind::Float => "float",
            Kind::List => "list",
        }
    }
}

fn path_of(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.into()
    } else {
        format!("{prefix}.{key}")
    }
}

/// The value at `key`, of `kind` (`config._get`); a float field takes an integer too.
fn field<'a>(d: &'a Table, prefix: &str, key: &str, kind: Kind) -> Result<&'a Val> {
    let path = path_of(prefix, key);
    let v = lookup(d, key).ok_or_else(|| cerr(format!("{path}: missing")))?;
    let ok = matches!(
        (kind, v),
        (Kind::Str, Val::Str(_))
            | (Kind::Bool, Val::Bool(_))
            | (Kind::Int, Val::Int(_))
            | (Kind::Float, Val::Int(_) | Val::Float(_))
            | (Kind::List, Val::Array(_))
    );
    if ok {
        Ok(v)
    } else {
        Err(cerr(format!(
            "{path}: expected {}, got {}",
            kind.name(),
            v.repr()
        )))
    }
}

fn string(d: &Table, prefix: &str, key: &str, choices: Option<&[&str]>) -> Result<String> {
    let Val::Str(s) = field(d, prefix, key, Kind::Str)? else {
        unreachable!("field checked the kind")
    };
    match choices {
        Some(choices) if !choices.contains(&s.as_str()) => Err(cerr(format!(
            "{}: {} is not one of {}",
            path_of(prefix, key),
            py_repr(s),
            choices.join(", ")
        ))),
        _ => Ok(s.clone()),
    }
}

fn boolean(d: &Table, prefix: &str, key: &str) -> Result<bool> {
    match field(d, prefix, key, Kind::Bool)? {
        Val::Bool(b) => Ok(*b),
        _ => unreachable!("field checked the kind"),
    }
}

fn int(d: &Table, prefix: &str, key: &str) -> Result<i64> {
    match field(d, prefix, key, Kind::Int)? {
        Val::Int(n) => Ok(*n),
        _ => unreachable!("field checked the kind"),
    }
}

fn float(d: &Table, prefix: &str, key: &str) -> Result<f64> {
    match field(d, prefix, key, Kind::Float)? {
        Val::Int(n) => Ok(*n as f64),
        Val::Float(x) => Ok(*x),
        _ => unreachable!("field checked the kind"),
    }
}

/// The table at `key`, empty when absent, with no key outside `allowed` (`config._table`).
fn sub_table<'a>(d: &'a Table, key: &str, allowed: Option<&[&str]>) -> Result<&'a Table> {
    let t = match lookup(d, key) {
        None => &EMPTY,
        Some(Val::Table(t)) => t,
        Some(_) => return Err(cerr(format!("{key}: expected a table"))),
    };
    if let Some(allowed) = allowed
        && let Some(extra) = t
            .iter()
            .map(|(k, _)| k.as_str())
            .filter(|k| !allowed.contains(k))
            .min()
    {
        return Err(cerr(format!("{key}.{extra}: unknown key")));
    }
    Ok(t)
}

fn step_ok(step: &str) -> bool {
    matches!(step, "none" | "pr")
        || ["skill:", "command:"]
            .iter()
            .any(|p| step.starts_with(p) && step.len() > p.len())
}

fn build(d: &Table) -> Result<Config> {
    if let Some(extra) = d
        .iter()
        .map(|(k, _)| k.as_str())
        .filter(|k| !TOP_KEYS.contains(k))
        .min()
    {
        return Err(cerr(format!("{extra}: unknown top-level key")));
    }
    let profile = string(d, "", "profile", Some(&["lean", "standard", "thorough"]))?;
    let docs_dir = string(d, "", "docs_dir", None)?;

    let r = sub_table(
        d,
        "run",
        Some(&[
            "worktree",
            "test_first",
            "evidence",
            "card_review",
            "cost_limit_usd",
            "branch_prefix",
        ]),
    )?;
    let run = RunSettings {
        worktree: boolean(r, "run", "worktree")?,
        test_first: string(
            r,
            "run",
            "test_first",
            Some(&["when-cheap", "always", "off"]),
        )?,
        evidence: string(r, "run", "evidence", Some(&["record", "block"]))?,
        card_review: string(r, "run", "card_review", Some(&["always", "by-risk"]))?,
        cost_limit_usd: float(r, "run", "cost_limit_usd")?,
        branch_prefix: string(r, "run", "branch_prefix", None)?,
    };

    let a = sub_table(
        d,
        "after_run",
        Some(&["step", "on_incomplete", "repeat_safe", "check"]),
    )?;
    let step = string(a, "after_run", "step", None)?;
    if !step_ok(&step) {
        return Err(cerr(format!(
            "after_run.step: {} must be none, pr, skill:<name> or command:<cmd>",
            py_repr(&step)
        )));
    }
    let after_run = AfterRun {
        step,
        on_incomplete: string(a, "after_run", "on_incomplete", Some(&["none", "draft-pr"]))?,
        repeat_safe: boolean(a, "after_run", "repeat_safe")?,
        check: string(a, "after_run", "check", None)?,
    };

    let cap = sub_table(d, "capacity", Some(&["codex_pause_percent"]))?;
    let pause = int(cap, "capacity", "codex_pause_percent")?;
    if !(0..=100).contains(&pause) {
        return Err(cerr(format!(
            "capacity.codex_pause_percent: {pause} must be 0-100"
        )));
    }

    let cx = sub_table(d, "codex", Some(&["network", "writable_dirs"]))?;
    let Val::Array(dirs) = field(cx, "codex", "writable_dirs", Kind::List)? else {
        unreachable!("field checked the kind")
    };
    let writable_dirs = dirs
        .iter()
        .map(|x| match x {
            Val::Str(s) => Ok(s.clone()),
            _ => Err(cerr("codex.writable_dirs: expected a list of strings")),
        })
        .collect::<Result<Vec<_>>>()?;
    let codex = CodexSettings {
        network: boolean(cx, "codex", "network")?,
        writable_dirs,
    };

    let mut limits = Vec::new();
    for (role, v) in sub_table(d, "limits", None)? {
        let prefix = format!("limits.{role}");
        let t = match v {
            Val::Table(t) if has_exactly(t, &["steps", "minutes"]) => t,
            _ => return Err(cerr(format!("{prefix}: expected {{ steps, minutes }}"))),
        };
        let lim = Limits {
            steps: int(t, &prefix, "steps")?,
            minutes: int(t, &prefix, "minutes")?,
        };
        limits.push((role.clone(), lim));
    }

    let mut roles = Vec::new();
    for (role, v) in sub_table(d, "roles", None)? {
        let Val::Table(t) = v else {
            return Err(cerr(format!("roles.{role}: expected a table")));
        };
        if let Some(extra) = t
            .iter()
            .map(|(k, _)| k.as_str())
            .filter(|k| !VENDORS.contains(k) && *k != "prefer")
            .min()
        {
            return Err(cerr(format!("roles.{role}.{extra}: unknown key")));
        }
        let mut routes = Vec::new();
        for vendor in VENDORS {
            if let Some(value) = lookup(t, vendor) {
                let route = match value {
                    Val::Str(text) => parse_route(vendor, text),
                    other => Err(cerr(format!(
                        "expected '<model> <effort>', got {}",
                        other.repr()
                    ))),
                };
                routes.push(route.map_err(|e| cerr(format!("roles.{role}.{vendor}: {e}")))?);
            }
        }
        if routes.is_empty() {
            return Err(cerr(format!("roles.{role}: needs a claude or codex route")));
        }
        let prefer = match lookup(t, "prefer") {
            None => "claude".to_string(),
            Some(Val::Str(p)) if ["claude", "codex", "other"].contains(&p.as_str()) => p.clone(),
            Some(other) => {
                return Err(cerr(format!(
                    "roles.{role}.prefer: {} is not one of claude, codex, other",
                    other.repr()
                )));
            }
        };
        if VENDORS.contains(&prefer.as_str()) && !routes.iter().any(|r| r.vendor == prefer) {
            return Err(cerr(format!(
                "roles.{role}.prefer: no {prefer} route defined"
            )));
        }
        roles.push((role.clone(), RoleSpec { routes, prefer }));
    }

    let notify = sub_table(d, "notify", Some(&["command"]))?;
    let mut prices = Vec::new();
    for (family, v) in sub_table(d, "prices", None)? {
        let prefix = format!("prices.{family}");
        let t = match v {
            Val::Table(t) if has_exactly(t, &PRICE_KEYS) => t,
            _ => {
                return Err(cerr(format!(
                    "{prefix}: expected keys {}",
                    PRICE_KEYS.join(", ")
                )));
            }
        };
        let p = Prices {
            input: float(t, &prefix, "input")?,
            output: float(t, &prefix, "output")?,
            cache_read: float(t, &prefix, "cache_read")?,
            cache_write_5m: float(t, &prefix, "cache_write_5m")?,
            cache_write_1h: float(t, &prefix, "cache_write_1h")?,
        };
        prices.push((family.clone(), p));
    }

    Ok(Config {
        profile,
        docs_dir,
        run,
        after_run,
        codex_pause_percent: pause,
        codex,
        limits,
        roles,
        notify_command: string(notify, "notify", "command", None)?,
        prices,
    })
}

fn has_exactly(t: &Table, keys: &[&str]) -> bool {
    t.len() == keys.len() && keys.iter().all(|k| lookup(t, k).is_some())
}

// --------------------------------------------------------------------------
// Paths, as Python's os.path resolves them

/// `path` made absolute and resolved as far as it exists, `..` and symlinks included
/// (`os.path.realpath`, not strict).
pub(crate) fn real_path(path: &Path) -> PathBuf {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut real = PathBuf::from("/");
    for part in abs.components() {
        match part {
            Component::RootDir | Component::Prefix(_) | Component::CurDir => {}
            Component::ParentDir => {
                real.pop();
            }
            Component::Normal(name) => {
                real.push(name);
                if let Ok(resolved) = real.canonicalize() {
                    real = resolved;
                }
            }
        }
    }
    real
}

/// `~` and `~/…` expanded to the home directory (`os.path.expanduser`). `~user` forms are
/// left as they are.
fn expand_user(path: &OsStr) -> OsString {
    let bytes = path.as_bytes();
    if !(bytes == b"~" || bytes.starts_with(b"~/")) {
        return path.to_os_string();
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::home_dir().map(PathBuf::into_os_string))
        .unwrap_or_default();
    let mut out = home.as_bytes().to_vec();
    while out.last() == Some(&b'/') {
        out.pop();
    }
    out.extend_from_slice(&bytes[1..]);
    if out.is_empty() {
        out.push(b'/');
    }
    OsString::from_vec(out)
}

// --------------------------------------------------------------------------
// Python's text rules, for parity in parsing and messages

/// `str.isspace`: Unicode White_Space, plus U+001C..U+001F.
pub(crate) fn py_isspace(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// `str.strip()`.
pub(crate) fn py_strip(s: &str) -> &str {
    s.trim_matches(py_isspace)
}

/// `str.split()`.
pub(crate) fn py_split(s: &str) -> impl Iterator<Item = &str> {
    s.split(py_isspace).filter(|w| !w.is_empty())
}

/// `str.splitlines()`: lines end at `\n`, `\r\n`, `\r`, `\v`, `\f`, U+001C..U+001E, U+0085,
/// U+2028 and U+2029.
pub(crate) fn py_splitlines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let boundary = matches!(
            c,
            '\n' | '\r'
                | '\u{0b}'
                | '\u{0c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if boundary {
            out.push(&s[start..i]);
            let mut end = i + c.len_utf8();
            if c == '\r' && chars.peek().map(|(_, n)| *n) == Some('\n') {
                chars.next();
                end += 1;
            }
            start = end;
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// `repr(s)` for a `str`.
pub(crate) fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        let cp = c as u32;
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if !py_printable(c) => match cp {
                0..=0xff => out.push_str(&format!("\\x{cp:02x}")),
                0x100..=0xffff => out.push_str(&format!("\\u{cp:04x}")),
                _ => out.push_str(&format!("\\U{cp:08x}")),
            },
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// An approximation of `str.isprintable` for one character: not a control, format, separator
/// (other than the space) or private-use character.
fn py_printable(c: char) -> bool {
    let cp = c as u32;
    !(c.is_control()
        || (c.is_whitespace() && c != ' ')
        || matches!(
            cp,
            0xad | 0x600..=0x605
                | 0x61c
                | 0x6dd
                | 0x70f
                | 0x180e
                | 0x200b..=0x200f
                | 0x202a..=0x202e
                | 0x2060..=0x2064
                | 0x2066..=0x206f
                | 0xfeff
                | 0xfff9..=0xfffb
                | 0xe000..=0xf8ff
                | 0xf0000..=0xffffd
                | 0x100000..=0x10fffd
        ))
}

/// `repr(x)` (and `str(x)`) for a `float`: the shortest digits that read back, in fixed
/// notation for exponents -4 to 15, else as `d.ddde±XX`.
fn py_float(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf" } else { "-inf" }.into();
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0.0" } else { "0.0" }.into();
    }
    let sci = format!("{:e}", x.abs());
    let (mantissa, exp) = sci.split_once('e').expect("{:e} writes an exponent");
    let exp: i32 = exp.parse().expect("{:e} writes an integer exponent");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let sign = if x < 0.0 { "-" } else { "" };
    if (-4..16).contains(&exp) {
        let point = exp + 1;
        let body = if point <= 0 {
            format!("0.{}{digits}", "0".repeat(point.unsigned_abs() as usize))
        } else if point as usize >= digits.len() {
            format!("{digits}{}.0", "0".repeat(point as usize - digits.len()))
        } else {
            format!(
                "{}.{}",
                &digits[..point as usize],
                &digits[point as usize..]
            )
        };
        format!("{sign}{body}")
    } else {
        let (first, rest) = digits.split_at(1);
        let mantissa = if rest.is_empty() {
            first.to_string()
        } else {
            format!("{first}.{rest}")
        };
        let esign = if exp < 0 { '-' } else { '+' };
        format!("{sign}{mantissa}e{esign}{:02}", exp.abs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repository folder and a user file for `load_config_from`.
    struct Files {
        _dir: tempfile::TempDir,
        repo: PathBuf,
        user: PathBuf,
    }

    impl Files {
        fn new(user: &str, repo: &str) -> Files {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let repo_dir = root.join("my repo");
            std::fs::create_dir(&repo_dir).unwrap();
            if !repo.is_empty() {
                std::fs::write(repo_dir.join(".leadhand.toml"), repo).unwrap();
            }
            let user_file = root.join("user.toml");
            std::fs::write(&user_file, user).unwrap();
            Files {
                _dir: dir,
                repo: repo_dir,
                user: user_file,
            }
        }

        fn load(&self) -> Result<Config> {
            load_config_from(&self.repo, &self.user)
        }
    }

    fn codex_only(spec: &RoleSpec) -> RoleSpec {
        RoleSpec {
            routes: vec![spec.route("codex").unwrap().clone()],
            prefer: "codex".into(),
        }
    }

    // CATALOG: test_config.py::ConfigTests::test_step_limits_follow_the_roles_routes_and_agent_files
    #[test]
    fn step_limits_follow_the_roles_routes_and_agent_files() {
        let f = Files::new(
            "",
            "[limits]\nimplementer = { steps = 200, minutes = 60 }\n\
             reviewer = { steps = 80, minutes = 20 }\n",
        );
        let mut c = f.load().unwrap();
        // Codex-only implementer roles honour any step limit; final_reviewer still has a Claude
        // route, and it uses limits.reviewer and the reviewer agent.
        for (role, spec) in &mut c.roles {
            if [
                "implementer",
                "implementer_hard",
                "reviewer",
                "reviewer_hard",
            ]
            .contains(&&**role)
            {
                *spec = codex_only(spec);
            }
        }
        assert_eq!(
            unsupported_settings(&c),
            ["not supported in this version: limits.reviewer.steps = 80 \
              (Claude workers use the agent file's maxTurns = 60)"]
        );
        // M comes from the agent file.
        let verifier = include_str!("../agents/verifier.md");
        assert!(verifier.contains("maxTurns: 40\n"));
        let edited = verifier.replace("maxTurns: 40\n", "maxTurns: 45\n");
        let turns = |agent: &str| match agent {
            "verifier" => max_turns(&edited),
            other => agent_max_turns(other),
        };
        let c = f.load().unwrap();
        assert_eq!(
            unhonoured_step_limits(&c, &turns)
                .last()
                .map(String::as_str),
            Some(
                "not supported in this version: limits.verifier.steps = 40 \
                 (Claude workers use the agent file's maxTurns = 45)"
            )
        );
        assert_eq!(
            unsupported_settings(&c).last(),
            unhonoured_step_limits(&c, &agent_max_turns).last()
        );
    }

    // CATALOG: test_config_snapshot.py::Snapshot::test_snapshot_round_trips
    #[test]
    fn snapshot_round_trips() {
        let f = Files::new(
            "docs_dir = \"/tmp/docs dir\"\n[codex]\nwritable_dirs = [\"/cache\", \"/b\u{e9}\"]\n",
            "[run]\ncost_limit_usd = 2.5\n[roles.implementer]\nprefer = \"claude\"\n",
        );
        let live = f.load().unwrap();
        let json = crate::wire::to_pretty(&ConfigSnapshot(&live)).unwrap();
        assert_eq!(config_from_snapshot(&json).unwrap(), live);
    }

    #[test]
    fn tables_keep_the_order_keys_first_appear_in() {
        // As `tomllib` builds a dict: a table named by a later header keeps its first place.
        let t = parse_toml("[b.y]\nq = 1\n[a]\nz = 1\n[b.x]\nw = 1\n").unwrap();
        let keys = |t: &Table| t.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>();
        assert_eq!(keys(&t), ["b", "a"]);
        let Some(Val::Table(b)) = lookup(&t, "b") else {
            panic!("{t:?}")
        };
        assert_eq!(keys(b), ["y", "x"]);
        // A merged file keeps the preset's order, and appends what it adds.
        let f = Files::new("[roles.zeta]\nclaude = \"x high\"\n", "");
        let roles: Vec<String> = f
            .load()
            .unwrap()
            .roles
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(
            roles,
            [
                "implementer",
                "implementer_hard",
                "reviewer",
                "reviewer_hard",
                "final_reviewer",
                "verifier",
                "scout",
                "dealer",
                "supervisor",
                "zeta"
            ]
        );
    }
}
