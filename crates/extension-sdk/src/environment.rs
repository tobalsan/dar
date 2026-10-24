//! Process environment appended to each surface's local identity copy.

use std::sync::OnceLock;

use nix::unistd::{gethostname, Uid, User};

struct Environment {
    host: String,
    os_user: String,
}

static ENVIRONMENT: OnceLock<Environment> = OnceLock::new();

fn current() -> &'static Environment {
    ENVIRONMENT.get_or_init(|| Environment {
        host: gethostname()
            .ok()
            .and_then(|host| host.into_string().ok())
            .filter(|host| !host.is_empty())
            .unwrap_or_else(|| "unknown".into()),
        os_user: User::from_uid(Uid::effective())
            .ok()
            .flatten()
            .map(|user| user.name)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "unknown".into()),
    })
}

/// Read the host and effective OS user once during host startup.
pub fn initialize() {
    current();
}

/// Append a surface label and cached host details without replacing an empty
/// identity (which must retain the backend's default system prompt).
pub fn append_to_identity(identity: &str, interface: &str) -> String {
    if identity.is_empty() {
        return String::new();
    }
    let environment = current();
    format!(
        "{identity}\n\n[ENVIRONMENT]\ninterface: {interface}\nhost: {}\nos_user: {}\n[END ENVIRONMENT]",
        environment.host, environment.os_user
    )
}
