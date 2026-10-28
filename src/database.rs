//! PostgreSQL connection setup shared by the serve, migrate, and seed paths.

use std::{ffi::OsStr, time::Duration};

use sqlx::postgres::PgConnectOptions;

/// Serving-pool liveness checks: recently returned connections have already
/// passed SQLx's release check. Ping again only after an idle interval. A zero
/// interval restores SQLx's unconditional acquisition check for rollback.
/// Release validation stays enabled, including after canceled queries.
pub fn serving_pool_options(
    max_connections: u32,
    acquire_timeout: Duration,
    health_check_idle: Duration,
) -> sqlx::postgres::PgPoolOptions {
    let options = sqlx::postgres::PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(acquire_timeout);
    if health_check_idle.is_zero() {
        return options;
    }
    options
        .test_before_acquire(false)
        .before_acquire(move |connection, metadata| {
            Box::pin(async move {
                if metadata.idle_for >= health_check_idle {
                    sqlx::Connection::ping(connection).await?;
                }
                Ok(true)
            })
        })
}

/// Parse a router database URL without libpq startup options.
///
/// The platform may supply a shared metrics-store URI containing an `options`
/// query parameter (often to set another consumer's `search_path`). Some
/// PostgreSQL poolers reject that startup parameter before accepting a
/// connection. Router runtime and migration SQL are schema-qualified, so
/// inherited startup options are both unnecessary and unsafe to forward.
pub fn connect_options(database_url: &str) -> Result<PgConnectOptions, String> {
    ensure_pgoptions_unset(std::env::var_os("PGOPTIONS").as_deref())?;
    let url = sanitized_database_url(database_url)?;

    url.as_str()
        .parse::<PgConnectOptions>()
        .map_err(|error| format!("invalid PostgreSQL URL: {error}"))
}

fn ensure_pgoptions_unset(pgoptions: Option<&OsStr>) -> Result<(), String> {
    if pgoptions.is_some_and(|value| !value.is_empty()) {
        // PgConnectOptions has no public API to clear startup options inherited
        // from the environment. Failing closed is the only way to guarantee
        // they are not forwarded to PgBouncer.
        return Err(
            "PGOPTIONS must be unset for transaction-pool-safe router connections".to_string(),
        );
    }
    Ok(())
}

fn sanitized_database_url(database_url: &str) -> Result<reqwest::Url, String> {
    let mut url = reqwest::Url::parse(database_url)
        .map_err(|error| format!("invalid PostgreSQL URL: {error}"))?;
    let query_pairs: Vec<_> = url
        .query_pairs()
        .filter(|(key, _)| key != "options" && !key.starts_with("options["))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    url.query_pairs_mut().clear().extend_pairs(query_pairs);
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_inherited_startup_options_but_keeps_other_url_settings() {
        let url = sanitized_database_url(
            "postgresql://user:password@example.com:5432/router?sslmode=require&options=-c%20search_path%3Dmetrics&options[statement_timeout]=5s",
        )
        .expect("database URL should sanitize");
        assert_eq!(
            url.query_pairs().collect::<Vec<_>>(),
            vec![("sslmode".into(), "require".into())]
        );

        let options = connect_options(
            "postgresql://user:password@example.com:5432/router?sslmode=require&options=-c%20search_path%3Dmetrics",
        )
        .expect("database URL should parse after removing options");

        let rendered = sqlx::ConnectOptions::to_url_lossy(&options).to_string();
        assert!(rendered.contains("sslmode=require"));
    }

    #[test]
    fn preserves_credentials_and_unrelated_repeated_parameters() {
        let url = sanitized_database_url(
            "postgresql://user:p%40ss@example.com/router?application_name=chalk-router&options=-c%20search_path%3Dmetrics&sslmode=require&application_name=router-migrate",
        )
        .expect("database URL should parse after removing options");

        assert_eq!(url.username(), "user");
        assert_eq!(url.password(), Some("p%40ss"));
        assert_eq!(
            url.query_pairs().collect::<Vec<_>>(),
            vec![
                ("application_name".into(), "chalk-router".into()),
                ("sslmode".into(), "require".into()),
                ("application_name".into(), "router-migrate".into()),
            ]
        );
    }

    #[test]
    fn rejects_nonempty_pgoptions_from_the_environment() {
        assert!(ensure_pgoptions_unset(None).is_ok());
        assert!(ensure_pgoptions_unset(Some(OsStr::new(""))).is_ok());
        let error = ensure_pgoptions_unset(Some(OsStr::new("-c search_path=public")))
            .expect_err("PGOPTIONS cannot be cleared from PgConnectOptions");
        assert!(error.contains("PGOPTIONS must be unset"));
    }
}
