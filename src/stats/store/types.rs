//! Row structs and dimension/granularity enums shared by the store's query
//! methods. The `impl Store` query logic lives in sibling modules
//! (`auth`, `buckets`).

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AssetRow {
    pub path: String,
    pub requests: i64,
    pub bytes: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CountryClassRow {
    pub country: String,
    pub status_class: u8,
    pub requests: i64,
    pub bytes: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BucketTable {
    Minute,
    Hour,
    Day,
}

/// A stats breakdown dimension (path or country). Both share the same pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dimension {
    Path,
    Country,
}

impl Dimension {
    /// All dimensions, so writer/rollup/prune can iterate instead of repeating
    /// per-dimension calls. Adding a dimension is a one-line change here.
    pub const ALL: [Self; 2] = [Self::Path, Self::Country];

    /// Physical table backing this dimension at the given granularity.
    /// Names are compile-time literals from a closed enum, so callers may safely
    /// interpolate them into SQL.
    pub(super) const fn table(self, granularity: BucketTable) -> &'static str {
        match (self, granularity) {
            (Self::Path, BucketTable::Minute) => "bucket_minute",
            (Self::Path, BucketTable::Hour) => "bucket_hour",
            (Self::Path, BucketTable::Day) => "bucket_day",
            (Self::Country, BucketTable::Minute) => "country_minute",
            (Self::Country, BucketTable::Hour) => "country_hour",
            (Self::Country, BucketTable::Day) => "country_day",
        }
    }

    /// SQL key column name for this dimension.
    pub(super) const fn key_column(self) -> &'static str {
        match self {
            Self::Path => "path",
            Self::Country => "country",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopMetric {
    Requests,
    Bytes,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TimeseriesPoint {
    pub ts: i64,
    pub status_class: u8,
    pub requests: i64,
    pub bytes: i64,
}
