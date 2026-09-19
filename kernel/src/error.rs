
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

#[derive(Debug, Clone, Copy)]
pub struct NeedsRoundTrip;

impl std::fmt::Display for NeedsRoundTrip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("answering this needs a round trip to the database")
    }
}
impl std::error::Error for NeedsRoundTrip {}

pub fn needs_round_trip(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<NeedsRoundTrip>())
}

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
            if cause.is::<tokio_postgres::Error>() || cause.is::<crate::orm::TxEndFailed>() {
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
    ($($arg:tt)*) => {{
        let reason = format!($($arg)*);
        ::tracing::debug!(
            target: "odoo_kernel::refusal",
            site = concat!(file!(), ":", line!()),
            %reason,
            "refused"
        );
        anyhow::Error::new($crate::error::Refusal(reason))
    }};
}
macro_rules! refusal_at {
    ($site:expr, $($arg:tt)*) => {{
        let reason = format!($($arg)*);
        ::tracing::debug!(
            target: "odoo_kernel::refusal",
            site = $site,
            %reason,
            "refused"
        );
        anyhow::Error::new($crate::error::Refusal(reason))
    }};
}
macro_rules! refuse {
    ($($arg:tt)*) => {
        return Err($crate::error::refusal!($($arg)*))
    };
}
macro_rules! deny_access {
    ($($arg:tt)*) => {{
        let reason = format!($($arg)*);
        ::tracing::debug!(
            target: "odoo_kernel::access",
            site = concat!(file!(), ":", line!()),
            %reason,
            "denied"
        );
        return Err(anyhow::Error::new($crate::error::AccessDenied(reason)))
    }};
}
pub(crate) use {deny_access, refusal, refusal_at, refuse};

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
