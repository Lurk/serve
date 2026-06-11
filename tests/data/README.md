# Test fixtures

`GeoLite2-Country-Test.mmdb` is a small synthetic MaxMind-format country
database used only by unit tests. It is sourced from the `maxmind/MaxMind-DB`
project's test data and contains a handful of fabricated network → country
mappings (no real geolocation data). It is committed so tests run offline.
