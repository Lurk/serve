use crate::errors::ServeError;
use smol_str::SmolStr;
use std::net::IpAddr;
use std::path::Path;

/// The catch-all code for loopback/private/unknown IPs and lookup failures.
pub const UNKNOWN_COUNTRY: &str = "ZZ";

/// Resolves a client IP to an ISO 3166-1 alpha-2 country code.
pub trait CountryResolver: Send + Sync {
    /// Uppercase 2-letter code for `ip`, or [`UNKNOWN_COUNTRY`] when the IP is
    /// non-routable, absent from the database, or the lookup fails. Never panics.
    fn country_code(&self, ip: IpAddr) -> SmolStr;
}

/// A `GeoIP` `.mmdb`-backed resolver. Provider-agnostic: works with any
/// `MaxMind`-format country database (`MaxMind` `GeoLite2`, DB-IP Lite, `IPinfo`).
pub struct GeoResolver {
    reader: maxminddb::Reader<Vec<u8>>,
}

impl GeoResolver {
    /// Open the `.mmdb` file at `path`, reading it fully into memory.
    ///
    /// # Errors
    /// Returns `ServeError::Stats` if the file cannot be read or is not a valid
    /// `MaxMind` database.
    pub fn open(path: &Path) -> Result<Self, ServeError> {
        let reader = maxminddb::Reader::open_readfile(path)
            .map_err(|e| ServeError::Stats(format!("opening geoip db {}: {e}", path.display())))?;
        Ok(Self { reader })
    }

    /// The single place that touches the maxminddb lookup API, so a crate
    /// upgrade only needs adjusting here. Returns the ISO code if present.
    ///
    /// maxminddb 0.29 splits the lookup in two: `lookup` returns a
    /// `Result<LookupResult, _>` (a lightweight record locator) and
    /// `LookupResult::decode` returns `Result<Option<T>, _>` — `Ok(None)` when
    /// the IP is absent from the database. The `.ok()?` collapses each `Err`
    /// arm to `None`, and the extra `?` on `decode` unwraps the not-found
    /// `Option`.
    fn lookup_iso(&self, ip: IpAddr) -> Option<SmolStr> {
        let record = self.reader.lookup(ip).ok()?;
        let country: maxminddb::geoip2::Country = record.decode().ok()??;
        let code = country.country.iso_code?;
        Some(SmolStr::new(code))
    }
}

impl CountryResolver for GeoResolver {
    fn country_code(&self, ip: IpAddr) -> SmolStr {
        let ip = normalize_ip(ip);
        if is_non_routable(ip) {
            return SmolStr::new(UNKNOWN_COUNTRY);
        }
        self.lookup_iso(ip)
            .unwrap_or_else(|| SmolStr::new(UNKNOWN_COUNTRY))
    }
}

/// Collapse an IPv4-mapped IPv6 address (`::ffff:a.b.c.d`) to its IPv4 form so
/// the routability check and DB lookup see the real address. Without this a
/// mapped private/loopback IP would slip past [`is_non_routable`] (the IPv6
/// arm doesn't match the mapped form) and reach the database as `ZZ`.
const fn normalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => ip,
        },
        v4 @ IpAddr::V4(_) => v4,
    }
}

/// Loopback, private, link-local, and unspecified addresses can't be
/// geolocated — short-circuit them to the unknown bucket.
const fn is_non_routable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    const FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/GeoLite2-Country-Test.mmdb"
    );

    fn resolver() -> GeoResolver {
        GeoResolver::open(std::path::Path::new(FIXTURE)).expect("open fixture")
    }

    #[test]
    fn resolves_known_public_ip_to_country() {
        let r = resolver();
        let cc = r.country_code("81.2.69.142".parse::<IpAddr>().unwrap());
        // Canonical GB test address in the MaxMind country test database.
        assert_eq!(cc.as_str(), "GB");
    }

    #[test]
    fn loopback_and_private_resolve_to_unknown() {
        let r = resolver();
        assert_eq!(
            r.country_code("127.0.0.1".parse::<IpAddr>().unwrap())
                .as_str(),
            UNKNOWN_COUNTRY
        );
        assert_eq!(
            r.country_code("10.0.0.1".parse::<IpAddr>().unwrap())
                .as_str(),
            UNKNOWN_COUNTRY
        );
        assert_eq!(
            r.country_code("::1".parse::<IpAddr>().unwrap()).as_str(),
            UNKNOWN_COUNTRY
        );
    }

    #[test]
    fn ipv6_link_local_and_unique_local_resolve_to_unknown() {
        let r = resolver();
        // fe80::/10 link-local and fc00::/7 unique-local are non-routable and
        // must never reach the database lookup.
        assert_eq!(
            r.country_code("fe80::1".parse::<IpAddr>().unwrap())
                .as_str(),
            UNKNOWN_COUNTRY
        );
        assert_eq!(
            r.country_code("fc00::1".parse::<IpAddr>().unwrap())
                .as_str(),
            UNKNOWN_COUNTRY
        );
    }

    #[test]
    fn ipv4_mapped_ipv6_is_unmapped_before_lookup() {
        let r = resolver();
        // ::ffff:81.2.69.142 must resolve like the bare IPv4 (GB), and a mapped
        // private address must short-circuit to unknown rather than reach the DB.
        assert_eq!(
            r.country_code("::ffff:81.2.69.142".parse::<IpAddr>().unwrap())
                .as_str(),
            "GB"
        );
        assert_eq!(
            r.country_code("::ffff:10.0.0.1".parse::<IpAddr>().unwrap())
                .as_str(),
            UNKNOWN_COUNTRY
        );
    }

    #[test]
    fn resolves_known_public_ipv6_to_country() {
        let r = resolver();
        // Canonical MaxMind documentation IPv6 address; the country test
        // database maps 2001:218::/32 to JP.
        let cc = r.country_code("2001:218:85a3:0:0:8a2e:370:7334".parse::<IpAddr>().unwrap());
        assert_eq!(cc.as_str(), "JP");
    }

    #[test]
    fn unknown_public_ip_resolves_to_unknown() {
        let r = resolver();
        // 203.0.113.0/24 is TEST-NET-3 (RFC 5737) — not in the fixture.
        let cc = r.country_code("203.0.113.7".parse::<IpAddr>().unwrap());
        assert_eq!(cc.as_str(), UNKNOWN_COUNTRY);
    }
}
