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

/// A compile that would have had to ask the database, raised by a `Db` in
/// offline mode before any statement is sent. It is not a refusal of the
/// question: the caller asks again with the database available, which on the
/// persistence port means inside a savepoint -- and in the common case, where
/// the watermark, the identity and the rules are already known, it is never
/// raised and no savepoint is paid for.
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

// Every refusal and denial announces itself with the source location that
// decided it, so a routing miss is attributable to one site without a
// bisect. `odoo_kernel::refusal` and `odoo_kernel::access` are the two
// targets that report which capability the kernel is missing, aggregated
// over a corpus, and the fields are the input a future session needs to
// decide whether the site is worth implementing.
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
/// A refusal whose SITE is supplied by the caller.
///
/// A helper that reports why it stopped to a caller that decides whether that
/// is a refusal cannot use `refusal!`: the site would be the line that wrapped
/// the message, so every cause the helper has collapses onto one row of the
/// census. The helper passes `concat!(file!(), ":", line!())` from where it
/// actually gave up.
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
