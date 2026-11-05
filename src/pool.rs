//! Multi-account connection pool (port of `pymt5._pool`).

use std::collections::HashMap;

use futures_util::future::join_all;
use tracing::{error, info};

use crate::client::{ClientConfig, Mt5Client};
use crate::error::Result;

/// Credentials for one pool connection.
#[derive(Clone)]
pub struct PoolAccount {
    pub server: String,
    pub login: u64,
    pub password: String,
    pub label: String,
}

impl PoolAccount {
    pub fn new(server: impl Into<String>, login: u64, password: impl Into<String>) -> Self {
        Self { server: server.into(), login, password: password.into(), label: String::new() }
    }
}

/// Manages several concurrent client connections keyed by login.
pub struct Mt5ConnectionPool {
    accounts: Vec<PoolAccount>,
    base_config: ClientConfig,
    clients: HashMap<u64, Mt5Client>,
}

impl Mt5ConnectionPool {
    /// `base_config.uri` is overridden by each account's `server`.
    pub fn new(accounts: Vec<PoolAccount>, base_config: ClientConfig) -> Self {
        Self { accounts, base_config, clients: HashMap::new() }
    }

    /// Connect and log in all accounts concurrently; failures are logged, not fatal.
    pub async fn connect_all(&mut self) {
        let results = join_all(self.accounts.iter().map(|a| Self::connect_one(&self.base_config, a))).await;
        for (acct, res) in self.accounts.iter().zip(results) {
            let label = if acct.label.is_empty() { acct.login.to_string() } else { acct.label.clone() };
            match res {
                Ok(c) => { info!("pool: connected account {label}"); self.clients.insert(acct.login, c); }
                Err(e) => error!("failed to connect account {label}: {e}"),
            }
        }
    }

    async fn connect_one(base: &ClientConfig, acct: &PoolAccount) -> Result<Mt5Client> {
        let mut cfg = base.clone();
        cfg.uri = acct.server.clone();
        let client = Mt5Client::new(cfg);
        client.connect().await?;
        client.login(acct.login, &acct.password).await?;
        Ok(client)
    }

    pub async fn close_all(&mut self) {
        join_all(self.clients.values().map(|c| c.close())).await;
        self.clients.clear();
        info!("pool: all connections closed");
    }

    pub fn get_client(&self, login: u64) -> Option<&Mt5Client> { self.clients.get(&login) }
    pub fn clients(&self) -> Vec<&Mt5Client> { self.clients.values().collect() }

    /// Subscribe ticks on every client concurrently.
    pub async fn broadcast_subscribe_ticks(&self, symbol_ids: &[u32]) {
        join_all(self.clients.values().map(|c| c.subscribe_ticks(symbol_ids))).await;
    }

    pub async fn broadcast_load_symbols(&self) {
        join_all(self.clients.values().map(|c| c.load_symbols(true))).await;
    }
}
