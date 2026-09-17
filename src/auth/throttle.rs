// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Brute-force throttling parity.
//!
//! Ports the arithmetic of `OC\Security\Bruteforce\Throttler` and the subnet
//! normalisation of `OC\Security\Normalizer\IpAddress`, so an operator does not
//! get a second, unthrottled credential oracle. See design doc §3.5.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// `IThrottler::MAX_DELAY` (seconds).
pub const MAX_DELAY: f64 = 25.0;
/// `IThrottler::MAX_DELAY_MS`.
pub const MAX_DELAY_MS: i64 = 25_000;
/// `IThrottler::MAX_ATTEMPTS`.
pub const MAX_ATTEMPTS: i64 = 10;

/// `Throttler::calculateDelay()`: exponential backoff capped at 25 s.
pub fn calculate_delay(attempts: i64, max_attempts: i64) -> i64 {
    if attempts == 0 {
        return 0;
    }
    if attempts > max_attempts {
        return MAX_DELAY_MS;
    }
    let delay = 0.1 * 2f64.powi(attempts as i32);
    if delay > MAX_DELAY {
        return MAX_DELAY_MS;
    }
    (delay * 1000.0).ceil() as i64
}

/// `IpAddress::getSubnet()`: IPv4 `/32`, IPv4-mapped IPv6 unwrapped to `/32`,
/// everything else masked to `ipv6_size` bits (default 56, clamped to 32..=64).
pub fn normalized_subnet(ip: IpAddr, ipv6_size: u8) -> String {
    match ip {
        IpAddr::V4(v4) => format!("{v4}/32"),
        IpAddr::V6(v6) => {
            if let Some(v4) = embedded_ipv4(v6) {
                format!("{v4}/32")
            } else {
                let size = ipv6_size.clamp(32, 64);
                format!("{}/{}", mask_ipv6(v6, size), size)
            }
        }
    }
}

fn embedded_ipv4(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    let bytes = v6.octets();
    if bytes[..10].iter().all(|b| *b == 0) && bytes[10] == 0xFF && bytes[11] == 0xFF {
        Some(Ipv4Addr::new(bytes[12], bytes[13], bytes[14], bytes[15]))
    } else {
        None
    }
}

fn mask_ipv6(v6: Ipv6Addr, size: u8) -> Ipv6Addr {
    let mut bytes = v6.octets();
    for (i, byte) in bytes.iter_mut().enumerate() {
        let bit_start = i as u32 * 8;
        if bit_start + 8 <= size as u32 {
            // Fully inside the mask.
            continue;
        }
        if bit_start >= size as u32 {
            *byte = 0;
        } else {
            let keep = size as u32 - bit_start;
            *byte &= 0xFFu8 << (8 - keep);
        }
    }
    Ipv6Addr::from(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn delay_backoff_matches_nextcloud() {
        // 0.1 * 2^n seconds, capped at 25 s; attempts > max => 25 s.
        assert_eq!(calculate_delay(0, 10), 0);
        assert_eq!(calculate_delay(1, 10), 200);
        assert_eq!(calculate_delay(4, 10), 1600);
        assert_eq!(calculate_delay(8, 10), 25_000);
        assert_eq!(calculate_delay(11, 10), 25_000);
        assert_eq!(calculate_delay(100, 10), 25_000);
    }

    #[test]
    fn delay_caps_without_overflowing() {
        // 2^63 overflows a naive integer implementation; PHP uses floats.
        assert_eq!(calculate_delay(63, 1000), 25_000);
    }

    #[test]
    fn ipv4_is_slash_32() {
        let ip = IpAddr::from_str("203.0.113.7").unwrap();
        assert_eq!(normalized_subnet(ip, 56), "203.0.113.7/32");
    }

    #[test]
    fn ipv4_mapped_ipv6_is_unwrapped() {
        let ip = IpAddr::from_str("::ffff:203.0.113.7").unwrap();
        assert_eq!(normalized_subnet(ip, 56), "203.0.113.7/32");
    }

    #[test]
    fn ipv6_is_masked_to_56_bits() {
        let ip = IpAddr::from_str("2001:db8:abcd:1234:5678:9abc:def0:1234").unwrap();
        assert_eq!(normalized_subnet(ip, 56), "2001:db8:abcd:1200::/56");
    }

    #[test]
    fn ipv6_mask_size_is_clamped() {
        let ip = IpAddr::from_str("2001:db8::1").unwrap();
        // 8 is clamped up to 32, 100 down to 64.
        assert_eq!(normalized_subnet(ip, 8), normalized_subnet(ip, 32));
        assert_eq!(normalized_subnet(ip, 100), normalized_subnet(ip, 64));
    }
}
