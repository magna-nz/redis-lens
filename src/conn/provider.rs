//! Managed services recognised from the host name, and what each one degrades.
//! Contract stubs for the quick-wins wave (agent C builds them).

use crate::model::Provider;

/// The managed service `host` belongs to, when the host name shows it. A hint only.
pub fn classify(_host: &str) -> Option<Provider> {
    None
}

/// A one-line notice shown before a scan of a profile on `provider`, saying which
/// figures will be missing and why, or `None` when nothing degrades.
pub fn notice(_provider: Option<Provider>) -> Option<&'static str> {
    None
}
