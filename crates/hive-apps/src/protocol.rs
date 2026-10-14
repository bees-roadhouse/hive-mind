//! The wire between the host and a process app (D53 §1): one HTTP request per
//! invocation over the one socket the app listens on, the same JSON shapes
//! the ABI used, `{trust, data}` in and `{trust, data}` out.
//!
//! The shape is small on purpose and the SDKs (`@hive/app`, `hive_app`) are
//! written against it, so a change here is a change to every app. What the
//! host tells the app it could not learn otherwise: who is calling (the
//! actor and the principal, never the credential), which install it is, and
//! the trust of the input. What the app tells the host: the trust it claims
//! for its answer, which the host then floors (invariant 12: the host decides
//! taint, the app only lowers it).

use hive_identity::Credential;
use hive_trust::Level;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Where the host sends an invocation.
pub const INVOKE_PATH: &str = "/invoke";
/// What the host asks before the first invocation after a start, until it
/// answers 200 or the start timeout passes.
pub const READY_PATH: &str = "/ready";

/// Why the host is calling. The four D53 names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InvokeKind {
    Tool,
    Route,
    Event,
    Cron,
}

/// The HTTP facts of a route call, absent for the other kinds.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RouteFacts {
    pub method: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
}

/// Who is calling, as the app may know it. The credential itself never
/// crosses: the app holds its own install credential for what it does, and
/// what it learns about the caller is the pair invariant 2 names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallerFacts {
    pub actor: Uuid,
    pub principal_kind: String,
    pub principal_id: Uuid,
}

impl From<&Credential> for CallerFacts {
    fn from(c: &Credential) -> CallerFacts {
        CallerFacts {
            actor: c.actor_id,
            principal_kind: c.principal_kind.as_str().to_string(),
            principal_id: c.principal_id,
        }
    }
}

/// What the host sends.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub kind: InvokeKind,
    /// The tool, route function, subscription or cron entry the manifest named.
    pub name: String,
    pub install: Uuid,
    pub caller: CallerFacts,
    pub trust: Level,
    /// The arguments, the body, the event or nothing. Raw JSON.
    pub data: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<RouteFacts>,
}

/// What the app answers with 200. A missing or unknown `trust` is read as
/// untrusted (D58's rule for the same envelope on a stream): an app that
/// forgets to say is not trusted by default.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Response {
    #[serde(default)]
    pub trust: Option<String>,
    #[serde(default)]
    pub data: serde_json::Value,
}

/// What the app answers with anything but 200.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ErrorBody {
    #[serde(default)]
    pub error: String,
}

/// The host's rule for the answer's trust: the input's taint carries through
/// (monotonic, invariant 12), and the app may only lower it. `None` and any
/// word but `trusted` lower it.
pub fn fold_trust(input: Level, claimed: Option<&str>) -> Level {
    match (input, claimed) {
        (Level::Trusted, Some("trusted")) => Level::Trusted,
        _ => Level::Untrusted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_only_goes_down() {
        assert_eq!(fold_trust(Level::Trusted, Some("trusted")), Level::Trusted);
        assert_eq!(
            fold_trust(Level::Trusted, Some("untrusted")),
            Level::Untrusted
        );
        assert_eq!(fold_trust(Level::Trusted, None), Level::Untrusted);
        assert_eq!(
            fold_trust(Level::Trusted, Some("TRUSTED")),
            Level::Untrusted
        );
        assert_eq!(
            fold_trust(Level::Untrusted, Some("trusted")),
            Level::Untrusted
        );
    }

    #[test]
    fn the_request_names_the_caller_and_never_a_token() {
        let cred = Credential::new(Uuid::nil(), hive_identity::PrincipalKind::User, Uuid::nil());
        let req = Request {
            kind: InvokeKind::Tool,
            name: "add".into(),
            install: Uuid::nil(),
            caller: CallerFacts::from(&cred),
            trust: Level::Trusted,
            data: serde_json::json!({"x": 1}),
            route: None,
        };
        let raw = serde_json::to_string(&req).unwrap();
        assert!(raw.contains("\"kind\":\"tool\""), "{raw}");
        assert!(raw.contains("\"principal_kind\":\"user\""), "{raw}");
        assert!(!raw.contains("token"), "{raw}");
        assert!(
            !raw.contains("route"),
            "a tool call carries no route facts: {raw}"
        );
    }
}
