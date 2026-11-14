//! Deterministic route resolution (`route.py`): the vendor, model and effort for a card and
//! role, with one reason line per rule that applied.

use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::config::{Config, EFFORTS, RoleSpec, Route, VENDORS, py_repr, py_strip};
use crate::deck::Card;
use crate::error::Error;

/// The effort steps each profile adds.
pub const PROFILE_SHIFT: [(&str, i64); 3] = [("lean", -1), ("standard", 0), ("thorough", 1)];
const IMPLEMENTER_ROLES: [&str; 2] = ["implementer", "implementer_hard"];
const RISK_ROLES: [&str; 4] = [
    "implementer",
    "implementer_hard",
    "reviewer",
    "reviewer_hard",
];

/// What to resolve (`route.resolve`'s arguments).
pub struct RouteRequest<'a> {
    pub role: &'a str,
    pub card: Option<&'a Card>,
    /// 1 or more. The Python's `int`, as the command line passes it.
    pub attempt: i64,
    /// For a reviewer, the implementer's vendor; for implementer attempt 4, the vendor
    /// attempt 3 actually ran on.
    pub last_implementer_vendor: Option<&'a str>,
    pub installed: &'a [String],
    pub security: bool,
    /// The deck's `Profile:`, which wins over the configuration's.
    pub profile: Option<&'a str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolution {
    pub route: Route,
    /// The role key the route came from (`implementer_hard` for a Hard card's implementer).
    pub role: String,
    pub reasons: Vec<String>,
}

/// Why a route could not be resolved. The engine treats the first two specially.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouteError {
    /// Attempt 5 or later, or no other vendor for attempt 4 (`route.LadderExhausted`).
    LadderExhausted(String),
    /// The card's Route pins a vendor that is not installed (`route.PinnedVendorMissing`).
    PinnedVendorMissing(String),
    /// A configuration or argument error.
    Invalid(String),
}

impl fmt::Display for RouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RouteError::LadderExhausted(m)
            | RouteError::PinnedVendorMissing(m)
            | RouteError::Invalid(m) => f.write_str(m),
        }
    }
}

impl From<RouteError> for Error {
    fn from(e: RouteError) -> Error {
        Error::msg(e.to_string())
    }
}

fn effort_index(effort: &str) -> i64 {
    EFFORTS
        .iter()
        .position(|e| *e == effort)
        .expect("efforts are validated where they are read") as i64
}

/// `effort` moved by `steps`, clamped to low..xhigh (low..max when `allow_max`).
pub fn shift_effort(effort: &str, steps: i64, allow_max: bool) -> &'static str {
    let top = if allow_max {
        EFFORTS.len() as i64 - 1
    } else {
        effort_index("xhigh")
    };
    EFFORTS[(effort_index(effort) + steps).clamp(0, top) as usize]
}

fn spec<'c>(config: &'c Config, role: &str) -> Result<&'c RoleSpec, RouteError> {
    config
        .role(role)
        .ok_or_else(|| RouteError::Invalid(format!("roles.{role}: not configured")))
}

fn installed(installed: &[String], vendor: &str) -> bool {
    installed.iter().any(|v| v == vendor)
}

fn available(spec: &RoleSpec, inst: &[String]) -> Vec<&'static str> {
    VENDORS
        .into_iter()
        .filter(|v| installed(inst, v) && spec.route(v).is_some())
        .collect()
}

fn pick_vendor(
    spec: &RoleSpec,
    inst: &[String],
    last: Option<&str>,
    role: &str,
) -> Result<(&'static str, String), RouteError> {
    let avail = available(spec, inst);
    let Some(&first) = avail.first() else {
        return Err(RouteError::Invalid(format!(
            "no vendor available for role {role}: none of its routes is installed"
        )));
    };
    if spec.prefer == "other" {
        let Some(last) = last else {
            return Ok((first, format!("vendor {first}: prefer other")));
        };
        if let Some(&other) = avail.iter().find(|v| **v != last) {
            return Ok((other, format!("vendor {other}: other than {last}")));
        }
        return Ok((first, format!("vendor {first}: only vendor available")));
    }
    if let Some(&preferred) = avail.iter().find(|v| **v == spec.prefer) {
        return Ok((preferred, format!("vendor {preferred}: prefer")));
    }
    let why = if installed(inst, &spec.prefer) {
        "has no route"
    } else {
        "not installed"
    };
    Ok((first, format!("vendor {first}: {} {why}", spec.prefer)))
}

/// The route for a role (`route.resolve`).
///
/// Escalation (implementer roles only): attempt 2 is attempt 1 plus one effort step. Attempt 3
/// uses implementer_hard with the profile and risk steps, plus one more step when the card was
/// already Hard or pinned. Attempt 4 uses implementer_hard on the vendor other than attempt 3's
/// (`last_implementer_vendor`, or recomputed when absent), with attempt 3's effort. Attempt 5
/// or later, or no other vendor for attempt 4, exhausts the ladder.
pub fn resolve(config: &Config, req: &RouteRequest) -> Result<Resolution, RouteError> {
    let attempt = req.attempt;
    if attempt < 1 {
        return Err(RouteError::Invalid(format!(
            "attempt must be 1 or more, got {attempt}"
        )));
    }
    let card = req.card;
    let hard = card.is_some_and(|c| c.hard);
    let pin = card.and_then(|c| c.route_pin.as_ref());
    let mut reasons = Vec::new();
    let is_impl = IMPLEMENTER_ROLES.contains(&req.role);
    if is_impl && attempt >= 5 {
        return Err(RouteError::LadderExhausted(format!(
            "attempt {attempt}: the escalation ladder is exhausted"
        )));
    }

    // A pin replaces the role, vendor and effort steps for attempts 1 and 2.
    if let (true, Some(pin), Some(card)) = (is_impl && attempt <= 2, pin, card) {
        if !installed(req.installed, &pin.vendor) {
            return Err(RouteError::PinnedVendorMissing(format!(
                "card {} is pinned to {}, which is not installed",
                card.number, pin.vendor
            )));
        }
        let mut steps = 0;
        reasons.push(format!(
            "pinned: {} {} {}",
            pin.vendor, pin.model, pin.effort
        ));
        if attempt == 2 {
            steps = 1;
            reasons.push("attempt 2: +1".into());
        }
        let effort = shift_effort(&pin.effort, steps, pin.effort == "max");
        if effort_index(&pin.effort) + steps != effort_index(effort) {
            reasons.push(format!("clamped to {effort}"));
        }
        let route = Route {
            vendor: pin.vendor.clone(),
            model: pin.model.clone(),
            effort: effort.into(),
        };
        return Ok(Resolution {
            route,
            role: req.role.into(),
            reasons,
        });
    }

    // Step 1: the role key.
    let mut key = req.role;
    if req.role == "implementer" {
        if hard {
            key = "implementer_hard";
            reasons.push("role implementer_hard: card is Hard".into());
        }
    } else if req.role == "reviewer" {
        let why = if hard {
            Some("card is Hard")
        } else if card.is_some_and(|c| c.risk == "high") {
            Some("risk high")
        } else if req.security {
            Some("security review")
        } else {
            None
        };
        if let Some(why) = why {
            key = "reviewer_hard";
            reasons.push(format!("role reviewer_hard: {why}"));
        }
    }
    if is_impl && attempt >= 3 {
        if key != "implementer_hard" {
            reasons.push(format!("role implementer_hard: attempt {attempt}"));
        }
        key = "implementer_hard";
    }
    let spec = spec(config, key)?;

    // Step 2: the vendor.
    let (mut vendor, mut why) = pick_vendor(spec, req.installed, req.last_implementer_vendor, key)?;
    if is_impl && attempt == 4 {
        // Attempt 3's vendor: the one it actually ran on when the caller knows it;
        // recomputing it from last_implementer_vendor would be wrong under prefer = "other".
        let third = match req.last_implementer_vendor.filter(|v| !v.is_empty()) {
            Some(v) => v.to_string(),
            None => pick_vendor(spec, req.installed, None, key)?.0.to_string(),
        };
        let Some(&other) = available(spec, req.installed).iter().find(|v| **v != third) else {
            return Err(RouteError::LadderExhausted(
                "attempt 4: no other vendor is installed with an implementer_hard route".into(),
            ));
        };
        (vendor, why) = (other, format!("vendor {other}: other than {third}"));
    }
    reasons.push(why);
    let base = spec.route(vendor).expect("an available vendor has a route");

    // Step 3: the effort steps.
    let mut steps = 0;
    let profile = req
        .profile
        .filter(|p| !p.is_empty())
        .unwrap_or(&config.profile);
    let Some(&(_, shift)) = PROFILE_SHIFT.iter().find(|(p, _)| *p == profile) else {
        return Err(RouteError::Invalid(format!(
            "profile: {} is not one of lean, standard, thorough",
            py_repr(profile)
        )));
    };
    if shift != 0 {
        steps += shift;
        reasons.push(format!("profile {profile}: {shift:+}"));
    }
    if card.is_some_and(|c| c.risk == "high") && RISK_ROLES.contains(&key) {
        steps += 1;
        reasons.push("risk high: +1".into());
    }
    if is_impl {
        if attempt == 2 {
            steps += 1;
            reasons.push("attempt 2: +1".into());
        } else if attempt >= 3 && (hard || pin.is_some()) {
            steps += 1;
            reasons.push(format!("attempt {attempt}: +1"));
        }
    }

    // Step 6: the clamp.
    let effort = shift_effort(&base.effort, steps, false);
    if effort_index(&base.effort) + steps != effort_index(effort) {
        reasons.push(format!("clamped to {effort}"));
    }
    let route = Route {
        vendor: vendor.into(),
        model: base.model.clone(),
        effort: effort.into(),
    };
    Ok(Resolution {
        route,
        role: key.into(),
        reasons,
    })
}

/// The vendors whose CLI is on `PATH` (`shutil.which`), in `VENDORS` order.
pub fn installed_on_path() -> Vec<String> {
    let path = std::env::var_os("PATH").unwrap_or_else(|| "/bin:/usr/bin".into());
    VENDORS
        .into_iter()
        .filter(|v| on_path(&path, v))
        .map(str::to_string)
        .collect()
}

/// Whether `PATH` holds an executable file `name` (an empty entry is the current directory).
fn on_path(path: &OsStr, name: &str) -> bool {
    std::env::split_paths(path).any(|dir| {
        let candidate: PathBuf = if dir.as_os_str().is_empty() {
            PathBuf::from(name)
        } else {
            dir.join(name)
        };
        is_executable_file(&candidate)
    })
}

fn is_executable_file(p: &Path) -> bool {
    p.exists() && !p.is_dir() && rustix::fs::access(p, rustix::fs::Access::EXEC_OK).is_ok()
}

/// The vendors `--installed` names: a comma list, whitespace around names ignored
/// (`route._installed_from`).
pub fn installed_from(text: &OsStr) -> crate::error::Result<Vec<String>> {
    let text = text.to_string_lossy();
    let names: Vec<&str> = text
        .split(',')
        .map(py_strip)
        .filter(|t| !t.is_empty())
        .collect();
    if let Some(bad) = names.iter().filter(|n| !VENDORS.contains(n)).min() {
        return Err(Error::msg(format!(
            "--installed: unknown vendor {}",
            py_repr(bad)
        )));
    }
    let mut out: Vec<String> = Vec::new();
    for v in VENDORS {
        if names.contains(&v) {
            out.push(v.into());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shift_effort_clamps_to_xhigh_unless_max_is_allowed() {
        assert_eq!(shift_effort("xhigh", 1, false), "xhigh");
        assert_eq!(shift_effort("xhigh", 1, true), "max");
        assert_eq!(shift_effort("low", -2, false), "low");
        assert_eq!(shift_effort("max", 0, false), "xhigh");
        assert_eq!(shift_effort("medium", 1, false), "high");
    }

    #[test]
    fn installed_names_a_comma_list_of_known_vendors() {
        assert_eq!(
            installed_from(OsStr::new(" codex , claude,")).unwrap(),
            ["claude", "codex"]
        );
        assert_eq!(
            installed_from(OsStr::new("")).unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            installed_from(OsStr::new("gemini,claude,aider"))
                .unwrap_err()
                .message,
            "--installed: unknown vendor 'aider'"
        );
    }
}
