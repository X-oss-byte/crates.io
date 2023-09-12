use diesel::prelude::*;
use diesel::r2d2::{self, ConnectionManager, CustomizeConnection};
use prometheus::Histogram;
use secrecy::{ExposeSecret, SecretString};
use std::sync::{Arc, Mutex, MutexGuard};
use std::{
    ops::{Deref, DerefMut},
    time::Duration,
};
use thiserror::Error;
use url::Url;

use crate::config;

pub mod sql_types;

pub type ConnectionPool = r2d2::Pool<ConnectionManager<PgConnection>>;

#[derive(Clone)]
pub enum DieselPool {
    Pool {
        pool: ConnectionPool,
        time_to_obtain_connection_metric: Histogram,
    },
    BackgroundJobPool {
        pool: ConnectionPool,
    },
    Test(Arc<Mutex<PgConnection>>),
}

impl DieselPool {
    pub(crate) fn new(
        url: &SecretString,
        config: &config::DatabasePools,
        r2d2_config: r2d2::Builder<ConnectionManager<PgConnection>>,
        time_to_obtain_connection_metric: Histogram,
    ) -> Result<DieselPool, PoolError> {
        let manager = ConnectionManager::new(connection_url(config, url.expose_secret()));

        // For crates.io we want the behavior of creating a database pool to be slightly different
        // than the defaults of R2D2: the library's build() method assumes its consumers always
        // need a database connection to operate, so it blocks creating a pool until a minimum
        // number of connections is available.
        //
        // crates.io can actually operate in a limited capacity without a database connections,
        // especially by serving download requests to our users. Because of that we don't want to
        // block indefinitely waiting for a connection: we instead need to wait for a bit (to avoid
        // serving errors for the first connections until the pool is initialized) and if we can't
        // establish any connection continue booting up the application. The database pool will
        // automatically be marked as unhealthy and the rest of the application will adapt.
        let pool = DieselPool::Pool {
            pool: r2d2_config.build_unchecked(manager),
            time_to_obtain_connection_metric,
        };
        match pool.wait_until_healthy(Duration::from_secs(5)) {
            Ok(()) => {}
            Err(PoolError::UnhealthyPool) => {}
            Err(err) => return Err(err),
        }

        Ok(pool)
    }

    pub fn new_background_worker(pool: r2d2::Pool<ConnectionManager<PgConnection>>) -> Self {
        Self::BackgroundJobPool { pool }
    }

    pub(crate) fn to_real_pool(&self) -> Option<ConnectionPool> {
        match self {
            Self::Pool { pool, .. } | Self::BackgroundJobPool { pool } => Some(pool.clone()),
            _ => None,
        }
    }

    pub(crate) fn new_test(config: &config::DatabasePools, url: &SecretString) -> DieselPool {
        let mut conn = PgConnection::establish(&connection_url(config, url.expose_secret()))
            .expect("failed to establish connection");
        conn.begin_test_transaction()
            .expect("failed to begin test transaction");
        DieselPool::Test(Arc::new(Mutex::new(conn)))
    }

    #[instrument(name = "db.connect", skip_all)]
    pub fn get(&self) -> Result<DieselPooledConn<'_>, PoolError> {
        match self {
            DieselPool::Pool {
                pool,
                time_to_obtain_connection_metric,
            } => time_to_obtain_connection_metric.observe_closure_duration(|| {
                if let Some(conn) = pool.try_get() {
                    Ok(DieselPooledConn::Pool(conn))
                } else if !self.is_healthy() {
                    Err(PoolError::UnhealthyPool)
                } else {
                    Ok(DieselPooledConn::Pool(pool.get()?))
                }
            }),
            DieselPool::BackgroundJobPool { pool } => Ok(DieselPooledConn::Pool(pool.get()?)),
            DieselPool::Test(conn) => Ok(DieselPooledConn::Test(conn.try_lock().unwrap())),
        }
    }

    pub fn state(&self) -> PoolState {
        match self {
            DieselPool::Pool { pool, .. } | DieselPool::BackgroundJobPool { pool } => {
                let state = pool.state();
                PoolState {
                    connections: state.connections,
                    idle_connections: state.idle_connections,
                }
            }
            DieselPool::Test(_) => PoolState {
                connections: 0,
                idle_connections: 0,
            },
        }
    }

    #[instrument(skip_all)]
    pub fn wait_until_healthy(&self, timeout: Duration) -> Result<(), PoolError> {
        match self {
            DieselPool::Pool { pool, .. } | DieselPool::BackgroundJobPool { pool } => {
                match pool.get_timeout(timeout) {
                    Ok(_) => Ok(()),
                    Err(_) if !self.is_healthy() => Err(PoolError::UnhealthyPool),
                    Err(err) => Err(PoolError::R2D2(err)),
                }
            }
            DieselPool::Test(_) => Ok(()),
        }
    }

    fn is_healthy(&self) -> bool {
        self.state().connections > 0
    }
}

#[derive(Debug, Copy, Clone)]
pub struct PoolState {
    pub connections: u32,
    pub idle_connections: u32,
}

#[allow(clippy::large_enum_variant)]
pub enum DieselPooledConn<'a> {
    Pool(r2d2::PooledConnection<ConnectionManager<PgConnection>>),
    Test(MutexGuard<'a, PgConnection>),
}

impl Deref for DieselPooledConn<'_> {
    type Target = PgConnection;

    fn deref(&self) -> &Self::Target {
        match self {
            DieselPooledConn::Pool(conn) => conn.deref(),
            DieselPooledConn::Test(conn) => conn.deref(),
        }
    }
}

impl DerefMut for DieselPooledConn<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            DieselPooledConn::Pool(conn) => conn.deref_mut(),
            DieselPooledConn::Test(conn) => conn.deref_mut(),
        }
    }
}

pub fn oneoff_connection_with_config(
    config: &config::DatabasePools,
) -> ConnectionResult<PgConnection> {
    let url = connection_url(config, config.primary.url.expose_secret());
    PgConnection::establish(&url)
}

pub fn oneoff_connection() -> ConnectionResult<PgConnection> {
    let config = config::DatabasePools::full_from_environment(&config::Base::from_environment());
    oneoff_connection_with_config(&config)
}

pub fn connection_url(config: &config::DatabasePools, url: &str) -> String {
    let mut url = Url::parse(url).expect("Invalid database URL");

    if config.enforce_tls {
        maybe_append_url_param(&mut url, "sslmode", "require");
    }

    // Configure the time it takes for diesel to return an error when there is full packet loss
    // between the application and the database.
    maybe_append_url_param(
        &mut url,
        "tcp_user_timeout",
        &config.tcp_timeout_ms.to_string(),
    );

    url.into()
}

fn maybe_append_url_param(url: &mut Url, key: &str, value: &str) {
    if !url.query_pairs().any(|(k, _)| k == key) {
        url.query_pairs_mut().append_pair(key, value);
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ConnectionConfig {
    pub statement_timeout: Duration,
    pub read_only: bool,
}

impl CustomizeConnection<PgConnection, r2d2::Error> for ConnectionConfig {
    fn on_acquire(&self, conn: &mut PgConnection) -> Result<(), r2d2::Error> {
        use diesel::sql_query;

        sql_query(format!(
            "SET statement_timeout = {}",
            self.statement_timeout.as_millis()
        ))
        .execute(conn)
        .map_err(r2d2::Error::QueryError)?;
        if self.read_only {
            sql_query("SET default_transaction_read_only = 't'")
                .execute(conn)
                .map_err(r2d2::Error::QueryError)?;
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum PoolError {
    #[error(transparent)]
    R2D2(#[from] r2d2::PoolError),
    #[error("unhealthy database pool")]
    UnhealthyPool,
}
