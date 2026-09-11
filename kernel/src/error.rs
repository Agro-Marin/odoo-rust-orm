//! Errors cross the Python and HTTP boundaries without guessing from prose.
//! Only explicit capability/input refusals may bypass the routing breaker.

#[derive(Debug)]
pub struct Refusal(pub String);

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Refusal {}

#[derive(Debug)]
pub struct AccessDenied(pub String);

impl std::fmt::Display for AccessDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for AccessDenied {}

#[derive(Debug, Clone, Copy)]
pub struct RegistryStale;

impl std::fmt::Display for RegistryStale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "the Odoo registry changed; rebuild the kernel from a fresh live registry export",
        )
    }
}
impl std::error::Error for RegistryStale {}

#[derive(Debug, PartialEq, Eq)]
pub enum ErrorKind {
    Refused,
    AccessDenied,
    RegistryStale,
    Database,
    Internal,
}

impl ErrorKind {
    pub fn of(error: &anyhow::Error) -> Self {
        for cause in error.chain() {
            if cause.is::<tokio_postgres::Error>() {
                return Self::Database;
            }
            if cause.is::<RegistryStale>() {
                return Self::RegistryStale;
            }
            if cause.is::<AccessDenied>() {
                return Self::AccessDenied;
            }
            if cause.is::<Refusal>() {
                return Self::Refused;
            }
        }
        Self::Internal
    }
}

macro_rules! refusal {
    ($($arg:tt)*) => {
        anyhow::Error::new($crate::error::Refusal(format!($($arg)*)))
    };
}
macro_rules! refuse {
    ($($arg:tt)*) => {
        return Err($crate::error::refusal!($($arg)*))
    };
}
macro_rules! deny_access {
    ($($arg:tt)*) => {
        return Err(anyhow::Error::new($crate::error::AccessDenied(format!($($arg)*))))
    };
}
pub(crate) use {deny_access, refusal, refuse};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_errors_are_not_refusals_and_context_preserves_categories() {
        let refused = refusal!("unsupported operator").context("compile domain");
        assert_eq!(ErrorKind::of(&refused), ErrorKind::Refused);
        let denied = anyhow::Error::new(AccessDenied("restricted field".into())).context("read");
        assert_eq!(ErrorKind::of(&denied), ErrorKind::AccessDenied);
        assert_eq!(
            ErrorKind::of(&RegistryStale.into()),
            ErrorKind::RegistryStale
        );
        let bug = anyhow::anyhow!("unsupported operator");
        assert_eq!(ErrorKind::of(&bug), ErrorKind::Internal);
        let db = "unknown_setting=yes"
            .parse::<tokio_postgres::Config>()
            .unwrap_err();
        assert_eq!(ErrorKind::of(&db.into()), ErrorKind::Database);
    }
}
