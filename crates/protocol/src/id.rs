//! Federation-ready identifiers.
//!
//! Every user is addressed as `name@domain` from day one, so that users
//! from other Tinlink servers can later join groups without a new account.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IdError {
    #[error("user id must have the form name@domain")]
    MissingDomain,
    #[error("user name must be 1-32 characters of [a-z0-9_.-]")]
    InvalidName,
    #[error("domain is invalid")]
    InvalidDomain,
}

/// A user identifier of the form `name@domain`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct UserId {
    name: String,
    domain: String,
}

impl UserId {
    pub fn new(name: &str, domain: &str) -> Result<Self, IdError> {
        let name = name.to_ascii_lowercase();
        let domain = domain.to_ascii_lowercase();
        let name_ok = (1..=32).contains(&name.len())
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
        if !name_ok {
            return Err(IdError::InvalidName);
        }
        let domain_ok = !domain.is_empty()
            && domain.len() <= 253
            && domain.split('.').all(|label| {
                !label.is_empty()
                    && label
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | ':'))
            });
        if !domain_ok {
            return Err(IdError::InvalidDomain);
        }
        Ok(Self { name, domain })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn domain(&self) -> &str {
        &self.domain
    }

    /// True if this user belongs to the given (local) server domain.
    pub fn is_local_to(&self, domain: &str) -> bool {
        self.domain.eq_ignore_ascii_case(domain)
    }
}

impl fmt::Display for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.name, self.domain)
    }
}

impl FromStr for UserId {
    type Err = IdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (name, domain) = s.rsplit_once('@').ok_or(IdError::MissingDomain)?;
        Self::new(name, domain)
    }
}

impl TryFrom<String> for UserId {
    type Error = IdError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<UserId> for String {
    fn from(id: UserId) -> Self {
        id.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_normalizes() {
        let id: UserId = "Erik@Example.org".parse().unwrap();
        assert_eq!(id.to_string(), "erik@example.org");
        assert!(id.is_local_to("example.org"));
    }

    #[test]
    fn allows_port_in_domain() {
        assert!("dev@localhost:8080".parse::<UserId>().is_ok());
    }

    #[test]
    fn rejects_invalid() {
        assert_eq!("erik".parse::<UserId>(), Err(IdError::MissingDomain));
        assert_eq!("@example.org".parse::<UserId>(), Err(IdError::InvalidName));
        assert_eq!("erik@".parse::<UserId>(), Err(IdError::InvalidDomain));
        assert_eq!(
            "e rik@example.org".parse::<UserId>(),
            Err(IdError::InvalidName)
        );
    }

    #[test]
    fn serde_roundtrip() {
        let id: UserId = "erik@example.org".parse().unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"erik@example.org\"");
        assert_eq!(serde_json::from_str::<UserId>(&json).unwrap(), id);
    }
}
