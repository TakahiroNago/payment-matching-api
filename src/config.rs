//! Configuration from environment variables, validated once at startup.

use std::net::SocketAddr;

use crate::domain::Yen;
use crate::domain::matching::MatchingConfig;

/// Shorter tokens are rejected at startup.
const MIN_TOKEN_LENGTH: usize = 16;

// No `Debug`: the struct holds the API token and the database password.
#[derive(Clone)]
pub struct Config {
    pub database_url: String,
    pub api_token: String,
    pub bind_addr: SocketAddr,
    pub db_max_connections: u32,
    pub matching: MatchingConfig,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("{0} is not set")]
    Missing(&'static str),
    #[error("{name} is invalid: {problem}")]
    Invalid { name: &'static str, problem: String },
}

impl Config {
    /// Reads the process environment.
    ///
    /// # Errors
    /// When a required variable is missing or a value does not parse.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Reads the configuration through `lookup`, so that tests do not touch the process environment.
    ///
    /// # Errors
    /// When a required variable is missing or a value does not parse.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let get = |name: &'static str| lookup(name).filter(|v| !v.trim().is_empty());
        let invalid = |name: &'static str, problem: String| ConfigError::Invalid { name, problem };

        let database_url = get("DATABASE_URL").ok_or(ConfigError::Missing("DATABASE_URL"))?;

        let api_token = get("API_TOKEN").ok_or(ConfigError::Missing("API_TOKEN"))?;
        if api_token.len() < MIN_TOKEN_LENGTH {
            return Err(invalid(
                "API_TOKEN",
                format!("must be at least {MIN_TOKEN_LENGTH} characters"),
            ));
        }

        let bind_addr = get("BIND_ADDR")
            .unwrap_or_else(|| "127.0.0.1:8080".to_owned())
            .parse()
            .map_err(|e: std::net::AddrParseError| invalid("BIND_ADDR", e.to_string()))?;

        let db_max_connections = match get("DB_MAX_CONNECTIONS") {
            None => 10,
            Some(v) => match v.parse() {
                Ok(n) if n >= 1 => n,
                _ => {
                    return Err(invalid(
                        "DB_MAX_CONNECTIONS",
                        "must be a positive integer".to_owned(),
                    ));
                }
            },
        };

        let mut matching = MatchingConfig::default();
        if let Some(list) = get("FEE_AMOUNTS") {
            let mut fees = Vec::new();
            for item in list.split(',') {
                match item.trim().parse::<i64>() {
                    Ok(fee) if fee > 0 => fees.push(Yen(fee)),
                    _ => {
                        return Err(invalid(
                            "FEE_AMOUNTS",
                            "must be a comma separated list of positive integers".to_owned(),
                        ));
                    }
                }
            }
            fees.sort_unstable();
            fees.dedup();
            matching.fee_amounts = fees;
        }

        Ok(Self {
            database_url,
            api_token,
            bind_addr,
            db_max_connections,
            matching,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn load(vars: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        Config::from_lookup(|name| vars.get(name).cloned())
    }

    const URL: (&str, &str) = ("DATABASE_URL", "postgres://localhost/db");
    const TOKEN: (&str, &str) = ("API_TOKEN", "0123456789abcdef");

    #[test]
    fn defaults() {
        let config = load(&[URL, TOKEN]).unwrap();
        assert_eq!(config.bind_addr.to_string(), "127.0.0.1:8080");
        assert_eq!(config.db_max_connections, 10);
        assert_eq!(config.matching, MatchingConfig::default());
    }

    #[test]
    fn required_variables() {
        assert_eq!(
            load(&[TOKEN]).err(),
            Some(ConfigError::Missing("DATABASE_URL"))
        );
        assert_eq!(load(&[URL]).err(), Some(ConfigError::Missing("API_TOKEN")));
        assert_eq!(
            load(&[URL, ("API_TOKEN", "  ")]).err(),
            Some(ConfigError::Missing("API_TOKEN"))
        );
    }

    #[test]
    fn short_token_is_rejected() {
        assert!(matches!(
            load(&[URL, ("API_TOKEN", "short")]),
            Err(ConfigError::Invalid {
                name: "API_TOKEN",
                ..
            })
        ));
    }

    #[test]
    fn fee_amounts_are_sorted_and_deduplicated() {
        let config = load(&[URL, TOKEN, ("FEE_AMOUNTS", "440, 110,440")]).unwrap();
        assert_eq!(config.matching.fee_amounts, vec![Yen(110), Yen(440)]);
    }

    #[test]
    fn invalid_values_are_rejected() {
        for (name, value) in [
            ("FEE_AMOUNTS", "110,abc"),
            ("FEE_AMOUNTS", "0"),
            ("BIND_ADDR", "nowhere"),
            ("DB_MAX_CONNECTIONS", "0"),
        ] {
            assert!(
                matches!(load(&[URL, TOKEN, (name, value)]), Err(ConfigError::Invalid { name: n, .. }) if n == name),
                "{name}={value}"
            );
        }
    }
}
