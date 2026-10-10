//! Where an account registered from, kept only as keyed hashes (anti-botting,
//! shadow mode). The coordination detection compares them for equality and
//! nothing else, so the address itself is never stored.
//!
//! An [`Origin`] holds two 64-bit HMAC-SHA256 values under
//! `REGISTRATION_HASH_KEY`: the client's address (an IPv4 address, or the
//! /64 of an IPv6 one, which a single host rotates its addresses within)
//! and its network (the IPv4 /24 or the IPv6 /48). The key stays out of the
//! database: the IPv4 space is small enough to hash whole, so a dump is
//! only as safe as the key. Each value carries the key's id; values made
//! under different keys never compare equal.

use std::net::IpAddr;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use thiserror::Error;

type HmacSha256 = Hmac<Sha256>;

pub const ENV_KEY: &str = "REGISTRATION_HASH_KEY";
const MIN_KEY_LEN: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Origin {
    pub key_id: i32,
    pub address: i64,
    pub network: i64,
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum OriginKeyError {
    #[error(
        "REGISTRATION_HASH_KEY must be at least 32 random bytes, base64-encoded \
         (openssl rand -base64 32)"
    )]
    Invalid,
}

/// The secret registration addresses are hashed with.
#[derive(Clone)]
pub struct OriginKey {
    key: Vec<u8>,
    id: i32,
}

impl std::fmt::Debug for OriginKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginKey").field("id", &self.id).finish()
    }
}

impl OriginKey {
    /// `None` when `REGISTRATION_HASH_KEY` is unset or blank: no origin is
    /// recorded then.
    pub fn from_env() -> Result<Option<Self>, OriginKeyError> {
        match std::env::var(ENV_KEY) {
            Ok(v) if !v.trim().is_empty() => Self::from_base64(&v).map(Some),
            _ => Ok(None),
        }
    }

    pub fn from_base64(encoded: &str) -> Result<Self, OriginKeyError> {
        let key = BASE64
            .decode(encoded.trim())
            .map_err(|_| OriginKeyError::Invalid)?;
        if key.len() < MIN_KEY_LEN {
            return Err(OriginKeyError::Invalid);
        }
        let id = i32::from_be_bytes(mac(&key, b"key-id", &[])[..4].try_into().unwrap());
        Ok(Self { key, id })
    }

    pub fn id(&self) -> i32 {
        self.id
    }

    pub fn origin(&self, ip: IpAddr) -> Origin {
        let (address, network): (Vec<u8>, Vec<u8>) = match ip.to_canonical() {
            IpAddr::V4(v4) => {
                let o = v4.octets();
                (vec![4, o[0], o[1], o[2], o[3]], vec![4, o[0], o[1], o[2]])
            }
            IpAddr::V6(v6) => {
                let o = v6.octets();
                let mut address = vec![6];
                address.extend_from_slice(&o[..8]);
                let mut network = vec![6];
                network.extend_from_slice(&o[..6]);
                (address, network)
            }
        };
        let value = |label: &[u8], bytes: &[u8]| {
            i64::from_be_bytes(mac(&self.key, label, bytes)[..8].try_into().unwrap())
        };
        Origin {
            key_id: self.id,
            address: value(b"address", &address),
            network: value(b"network", &network),
        }
    }
}

fn mac(key: &[u8], label: &[u8], bytes: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts keys of any length");
    mac.update(b"scrobblr-origin/");
    mac.update(label);
    mac.update(&[0]);
    mac.update(bytes);
    mac.finalize().into_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=";
    const OTHER: &str = "CAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAg=";

    fn origin(key: &str, ip: &str) -> Origin {
        OriginKey::from_base64(key)
            .unwrap()
            .origin(ip.parse().unwrap())
    }

    #[test]
    fn addresses_hash_the_same_under_one_key() {
        assert_eq!(origin(KEY, "198.51.100.7"), origin(KEY, "198.51.100.7"));
        assert_ne!(
            origin(KEY, "198.51.100.7").address,
            origin(KEY, "198.51.100.8").address
        );
    }

    #[test]
    fn networks_group_an_ipv4_24_and_an_ipv6_48() {
        let a = origin(KEY, "198.51.100.7");
        let b = origin(KEY, "198.51.100.200");
        let c = origin(KEY, "198.51.101.7");
        assert_eq!(a.network, b.network);
        assert_ne!(a.network, c.network);

        // A host's privacy addresses rotate within its /64.
        let d = origin(KEY, "2001:db8:1:2:aaaa::1");
        let e = origin(KEY, "2001:db8:1:2:bbbb::2");
        let f = origin(KEY, "2001:db8:1:3::1");
        let g = origin(KEY, "2001:db8:2::1");
        assert_eq!(d.address, e.address);
        assert_ne!(d.address, f.address);
        assert_eq!(d.network, f.network);
        assert_ne!(d.network, g.network);
    }

    #[test]
    fn an_ipv4_mapped_address_is_its_ipv4_address() {
        assert_eq!(
            origin(KEY, "::ffff:198.51.100.7"),
            origin(KEY, "198.51.100.7")
        );
    }

    #[test]
    fn address_and_network_values_never_coincide() {
        // An IPv4 /24 and the address 0 inside it hash with different labels.
        let a = origin(KEY, "198.51.100.0");
        assert_ne!(a.address, a.network);
    }

    #[test]
    fn another_key_gives_other_values_and_another_id() {
        let a = origin(KEY, "198.51.100.7");
        let b = origin(OTHER, "198.51.100.7");
        assert_ne!(a.key_id, b.key_id);
        assert_ne!(a.address, b.address);
        assert_ne!(a.network, b.network);
    }

    #[test]
    fn short_or_malformed_keys_are_refused() {
        assert!(OriginKey::from_base64("c2hvcnQ=").is_err());
        assert!(OriginKey::from_base64("not base64!").is_err());
        let key = OriginKey::from_base64(KEY).unwrap();
        assert!(!format!("{key:?}").contains("BwcH"));
    }
}
