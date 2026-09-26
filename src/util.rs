// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Small HTTP/path helpers.

use axum::http::HeaderValue;
use base64::Engine;

/// Percent-decodes a URL path segment, leaving invalid escapes untouched.
pub fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// PHP's `urldecode()`: like [`percent_decode`] but `+` also decodes to a
/// space (`Sabre\Uri\split` callers feed this to `getPrincipalByPath()`).
pub fn urldecode(input: &str) -> String {
    percent_decode(&input.replace('+', " "))
}

/// Percent-encodes one path segment the way `Sabre\HTTP\encodePathSegment()`
/// does: `A-Za-z0-9_-~():@` are left alone, everything else is `%xx`
/// (lower-case hex).
pub fn encode_path_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        let c = byte as char;
        if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '~' | '(' | ')' | ':' | '@') {
            out.push(c);
        } else {
            out.push_str(&format!("%{byte:02x}"));
        }
    }
    out
}

fn hex_val(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Parses an HTTP Basic `Authorization` header.
pub fn parse_basic_auth(header: Option<&HeaderValue>) -> Option<(String, String)> {
    let value = header?.to_str().ok()?;
    let (scheme, encoded) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (user, password) = text.split_once(':')?;
    Some((user.to_string(), password.to_string()))
}

/// Formats a unix timestamp as an HTTP date (`getlastmodified`).
pub fn http_date(timestamp: i64) -> String {
    let time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(timestamp.max(0) as u64);
    httpdate::fmt_http_date(time)
}

/// Parses an HTTP date exactly like `Sabre\HTTP\parseDate()`
/// (`3rdparty/sabre/http/lib/functions.php:31`, as vendored by Nextcloud 33):
/// only the three RFC 2616 formats, trimmed of spaces (spaces only), weekday
/// names validated but never cross-checked against the date, and
/// `new \DateTime($string)` semantics for the value — including its day
/// overflow (`31 Feb 1994` rolls into March) and two-digit-year folding
/// (`00`-`69` → `2000`-`2069`, `70`-`99` → `1970`-`1999`). Returns the unix
/// timestamp in seconds.
pub fn parse_http_date(value: &str) -> Option<i64> {
    let s = value.trim_matches(' ');
    let (year, month, day, hour, minute, second) =
        if let Some((weekday, rest)) = s.split_once(", ") {
            // `Wkd, DD Mon YYYY HH:MM:SS GMT` (RFC 1123) or
            // `Weekday, DD-Mon-YY HH:MM:SS GMT` (RFC 850).
            if !is_http_weekday(weekday) {
                return None;
            }
            let rest = rest.strip_suffix(" GMT")?;
            let (date, time) = rest.rsplit_once(' ')?;
            let (hour, minute, second) = parse_hms(time)?;
            if let Some((day, month, year)) = split3(date, ' ') {
                (year4(year)?, month_num(month)?, day2(day)?, hour, minute, second)
            } else {
                let (day, month, year) = split3(date, '-')?;
                (year2(year)?, month_num(month)?, day2(day)?, hour, minute, second)
            }
        } else {
            // ANSI C's asctime(): `Sun Nov  6 08:49:37 1994`, implicit GMT.
            let weekday = s.get(..3)?;
            if !matches!(weekday, "Mon" | "Tue" | "Wed" | "Thu" | "Fri" | "Sat" | "Sun") {
                return None;
            }
            let rest = s.get(3..)?.strip_prefix(' ')?;
            let month = month_num(rest.get(..3)?)?;
            let rest = rest.get(3..)?.strip_prefix(' ')?;
            // The day is exactly two characters: `10`-`31`, or ` 1`-` 9`.
            let day = match rest.get(..2)?.as_bytes() {
                [b' ', d @ b'1'..=b'9'] => i64::from(*d - b'0'),
                [d1 @ b'1'..=b'3', d2 @ b'0'..=b'9'] => {
                    let day = i64::from(*d1 - b'0') * 10 + i64::from(*d2 - b'0');
                    if day > 31 {
                        return None;
                    }
                    day
                }
                _ => return None,
            };
            let rest = rest.get(2..)?.strip_prefix(' ')?;
            let (time, year) = rest.rsplit_once(' ')?;
            let (hour, minute, second) = parse_hms(time)?;
            (year4(year)?, month, day, hour, minute, second)
        };
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second)
}

fn split3(s: &str, sep: char) -> Option<(&str, &str, &str)> {
    let mut parts = s.split(sep);
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(a), Some(b), Some(c), None) => Some((a, b, c)),
        _ => None,
    }
}

fn parse_hms(s: &str) -> Option<(i64, i64, i64)> {
    let b = s.as_bytes();
    if b.len() != 8 || b[2] != b':' || b[5] != b':' {
        return None;
    }
    let hour = two(&s[0..2])?;
    let minute = two(&s[3..5])?;
    let second = two(&s[6..8])?;
    // The PHP regex is `([0-1]\d|2[0-3])(:[0-5]\d){2}`.
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    Some((hour, minute, second))
}

fn two(s: &str) -> Option<i64> {
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
        s.parse().ok()
    } else {
        None
    }
}

/// Exactly two digits, `01`-`31` (`0[1-9]|[12]\d|3[01]`).
fn day2(s: &str) -> Option<i64> {
    let day = two(s)?;
    (1..=31).contains(&day).then_some(day)
}

/// Exactly four digits, first non-zero (`[1-9]\d{3}`).
fn year4(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() == 4 && b[0] != b'0' && s.bytes().all(|b| b.is_ascii_digit()) {
        s.parse().ok()
    } else {
        None
    }
}

/// Two digits (`\d{2}`) with PHP's two-digit-year folding.
fn year2(s: &str) -> Option<i64> {
    let year = two(s)?;
    Some(if year < 70 { 2000 + year } else { 1900 + year })
}

fn month_num(name: &str) -> Option<i64> {
    Some(match name {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    })
}

fn is_http_weekday(name: &str) -> bool {
    matches!(
        name,
        "Mon"
            | "Tue"
            | "Wed"
            | "Thu"
            | "Fri"
            | "Sat"
            | "Sun"
            | "Monday"
            | "Tuesday"
            | "Wednesday"
            | "Thursday"
            | "Friday"
            | "Saturday"
            | "Sunday"
    )
}

/// Days since 1970-01-01 (Howard Hinnant's civil-calendar algorithm). `day`
/// may exceed the month length, which reproduces PHP's `DateTime` overflow
/// (`31 Feb` rolls into March).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;

    #[test]
    fn parse_http_date_accepts_the_three_rfc2616_formats() {
        // The classic RFC 7231 example: 1994-11-06T08:49:37Z.
        let expected = 784_111_777;
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(expected),
            "RFC 1123"
        );
        assert_eq!(
            parse_http_date("Sunday, 06-Nov-94 08:49:37 GMT"),
            Some(expected),
            "RFC 850 (two-digit year)"
        );
        assert_eq!(
            parse_http_date("Sun Nov  6 08:49:37 1994"),
            Some(expected),
            "asctime (implicit GMT)"
        );
        assert_eq!(
            parse_http_date("  Sun, 06 Nov 1994 08:49:37 GMT  "),
            Some(expected),
            "space-trimmed like PHP"
        );
    }

    #[test]
    fn parse_http_date_folds_two_digit_years_like_php() {
        assert_eq!(
            parse_http_date("Sunday, 01-Jan-69 00:00:00 GMT"),
            Some(days_from_civil(2069, 1, 1) * 86_400)
        );
        assert_eq!(
            parse_http_date("Thursday, 01-Jan-70 00:00:00 GMT"),
            Some(days_from_civil(1970, 1, 1) * 86_400)
        );
    }

    #[test]
    fn parse_http_date_overflows_the_day_like_php_datetime() {
        // `new \DateTime('31 Feb 1994 ...')` rolls into March instead of
        // failing, and so must the port.
        assert_eq!(
            parse_http_date("Tue, 31 Feb 1994 00:00:00 GMT"),
            parse_http_date("Thu, 03 Mar 1994 00:00:00 GMT")
        );
    }

    #[test]
    fn parse_http_date_rejects_everything_else() {
        for bad in [
            "",
            "Sun, 06 Nov 1994 08:49:37",       // no GMT
            "1994-11-06T08:49:37Z",            // ISO 8601, not an HTTP date
            "Bog, 06 Nov 1994 08:49:37 GMT",   // invented weekday
            "Sun, 06 Nov 1994 24:00:00 GMT",   // hour out of range
            "Sun, 00 Nov 1994 08:49:37 GMT",   // day zero
            "Sun, 06 Nov 0994 08:49:37 GMT",   // year regex `[1-9]\d{3}`
            "Sun Nov 32 08:49:37 1994",        // asctime day out of range
            "Sun, 06 Nov 1994 08:49:37 GMT extra",
        ] {
            assert_eq!(parse_http_date(bad), None, "accepted {bad:?}");
        }
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("al%20ice"), "al ice");
        assert_eq!(percent_decode("a%2Fb"), "a/b");
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn urldecoding_matches_php() {
        assert_eq!(urldecode("a+b"), "a b");
        assert_eq!(urldecode("a%20b"), "a b");
        assert_eq!(urldecode("parity-team"), "parity-team");
    }

    #[test]
    fn path_segment_encoding_matches_sabre() {
        assert_eq!(encode_path_segment("contacts"), "contacts");
        assert_eq!(encode_path_segment("a b.vcf"), "a%20b.vcf");
        assert_eq!(encode_path_segment("a/b"), "a%2fb");
        assert_eq!(encode_path_segment("user@example.com"), "user@example.com");
        assert_eq!(encode_path_segment("x(y)"), "x(y)");
    }

    #[test]
    fn basic_auth_parsing() {
        let value = HeaderValue::from_str(&format!(
            "Basic {}",
            STANDARD.encode("alice:s3cret:with:colons")
        ))
        .unwrap();
        assert_eq!(
            parse_basic_auth(Some(&value)),
            Some(("alice".to_string(), "s3cret:with:colons".to_string()))
        );
        let bearer = HeaderValue::from_static("Bearer abc");
        assert_eq!(parse_basic_auth(Some(&bearer)), None);
        assert_eq!(parse_basic_auth(None), None);
    }

    #[test]
    fn http_date_is_rfc1123() {
        assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(http_date(1_700_000_000), "Tue, 14 Nov 2023 22:13:20 GMT");
    }
}
