//! Shared classification of Postgres constraint violations and transient errors.
//!
//! Every store in this crate maps a raw `sqlx::Error` into its own
//! domain-specific error enum, but nearly all of them start by asking the
//! same question: "did this write fail because of a particular kind of
//! constraint, and if so, which class of constraint?" This module answers
//! that question once so each store's mapper only has to handle the
//! constraint *names* (or codes) it specifically cares about, instead of
//! re-deriving the SQLSTATE-to-meaning mapping.
//!
//! This intentionally does not look at constraint *names* — those are
//! store-specific (e.g. `rate_limits_connection_fk` vs
//! `uq_router_telemetry_exporters_name`) and stay in each store's own
//! mapper, read via `sqlx::Error::as_database_error(...).constraint()`.

/// The category of Postgres constraint a write violated, keyed by SQLSTATE
/// class. See <https://www.postgresql.org/docs/current/errcodes-appendix.html>.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DbConstraint {
    /// `23505` unique_violation.
    Unique,
    /// `23503` foreign_key_violation.
    ForeignKey,
    /// `23514` check_violation.
    Check,
    /// `23502` not_null_violation.
    NotNull,
}

/// Classify a `sqlx::Error` as a specific kind of Postgres constraint
/// violation, if it is one. Returns `None` for anything else: connectivity
/// failures, other database errors, or non-database errors entirely.
pub(crate) fn classify(err: &sqlx::Error) -> Option<DbConstraint> {
    let code = err.as_database_error()?.code()?;
    match &*code {
        "23505" => Some(DbConstraint::Unique),
        "23503" => Some(DbConstraint::ForeignKey),
        "23514" => Some(DbConstraint::Check),
        "23502" => Some(DbConstraint::NotNull),
        _ => None,
    }
}

/// Retry guidance for idempotent accounting writes, not proof that an
/// admission rolled back. Authentication deliberately has a narrower policy.
pub(crate) fn is_transient(error: &sqlx::Error) -> bool {
    match error {
        sqlx::Error::Io(_)
        | sqlx::Error::Tls(_)
        | sqlx::Error::Protocol(_)
        | sqlx::Error::PoolTimedOut => true,
        sqlx::Error::Database(error) => error.code().is_some_and(|code| transient_sqlstate(&code)),
        _ => false,
    }
}

/// No connection could be checked out: the pool timed out or was closed.
pub(crate) fn is_pool_exhausted(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed)
}

/// `55P03` lock_not_available: a `lock_timeout` (or `NOWAIT`) expired.
pub(crate) fn is_lock_timeout(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .and_then(|error| error.code())
        .is_some_and(|code| code == "55P03")
}

fn transient_sqlstate(code: &str) -> bool {
    code.starts_with("08")
        || code.starts_with("53")
        || matches!(
            code,
            "40001" | "40003" | "40P01" | "55P03" | "57P01" | "57P02" | "57P03"
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_errors_only_retry_recoverable_accounting_failures() {
        for code in [
            "08006", "08007", "40001", "40003", "40P01", "53000", "53100", "53300", "55P03",
            "57P01", "57P02", "57P03",
        ] {
            assert!(transient_sqlstate(code), "{code}");
        }
        for code in ["42883", "P0001", "23505", "42501", "57014", "22003"] {
            assert!(!transient_sqlstate(code), "{code}");
        }
        assert!(is_transient(&sqlx::Error::PoolTimedOut));
        assert!(is_transient(&sqlx::Error::Io(std::io::Error::from(
            std::io::ErrorKind::ConnectionReset
        ))));
        assert!(is_transient(&sqlx::Error::Protocol(
            "connection lost".into()
        )));
        assert!(is_transient(&sqlx::Error::Tls(Box::new(
            std::io::Error::from(std::io::ErrorKind::ConnectionReset)
        ))));
        assert!(!is_transient(&sqlx::Error::PoolClosed));
        assert!(!is_transient(&sqlx::Error::ColumnNotFound(
            "bad decision".into()
        )));
    }

    #[test]
    fn classify_returns_none_for_non_database_errors() {
        let err = sqlx::Error::RowNotFound;
        assert_eq!(classify(&err), None);
    }
}
