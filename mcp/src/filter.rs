//! Building Meilisearch filter expressions safely.
//!
//! Filter values come from tool arguments, which come from a model, which
//! may have picked them up from a document. Quoting them properly keeps a
//! stray `"` or `AND` from changing the shape of the expression.

/// Accumulates `AND`-joined clauses.
#[derive(Debug, Default, Clone)]
pub struct Filter {
    clauses: Vec<String>,
}

impl Filter {
    /// Start from the region envelope. Every query begins here, so nothing
    /// outside the region can surface even if something outside it was
    /// somehow indexed.
    pub fn within(region_clause: &str) -> Self {
        Self {
            clauses: vec![region_clause.to_string()],
        }
    }

    pub fn and(&mut self, clause: impl Into<String>) -> &mut Self {
        let clause = clause.into();
        if !clause.is_empty() {
            self.clauses.push(clause);
        }
        self
    }

    /// `field = "value"`.
    pub fn eq(&mut self, field: &str, value: &str) -> &mut Self {
        self.and(format!("{field} = {}", quote(value)))
    }

    /// `field IN ["a", "b"]`. A no-op for an empty list, which otherwise
    /// produces `IN []` and silently matches nothing.
    pub fn any_of(&mut self, field: &str, values: &[String]) -> &mut Self {
        let vals: Vec<String> = values
            .iter()
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
            .map(quote)
            .collect();
        if vals.is_empty() {
            return self;
        }
        self.and(format!("{field} IN [{}]", vals.join(", ")))
    }

    pub fn geo_radius(&mut self, lat: f64, lng: f64, radius_m: u32) -> &mut Self {
        self.and(format!("_geoRadius({lat}, {lng}, {radius_m})"))
    }

    pub fn gte(&mut self, field: &str, value: i64) -> &mut Self {
        self.and(format!("{field} >= {value}"))
    }

    pub fn lte(&mut self, field: &str, value: i64) -> &mut Self {
        self.and(format!("{field} <= {value}"))
    }

    pub fn build(&self) -> String {
        self.clauses.join(" AND ")
    }
}

/// Quote a value for a Meilisearch filter expression.
fn quote(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clauses_are_and_joined_starting_from_the_region() {
        let mut f = Filter::within("_geoBoundingBox([41, -102], [36, -109])");
        f.eq("city", "Denver").geo_radius(39.7, -104.9, 15000);
        assert_eq!(
            f.build(),
            "_geoBoundingBox([41, -102], [36, -109]) AND city = \"Denver\" AND _geoRadius(39.7, -104.9, 15000)"
        );
    }

    #[test]
    fn values_are_quoted_and_escaped() {
        let mut f = Filter::default();
        f.eq("city", "O\"Brien \\ Springs");
        assert_eq!(f.build(), r#"city = "O\"Brien \\ Springs""#);
    }

    #[test]
    fn an_injected_operator_stays_a_literal_value() {
        let mut f = Filter::within("region");
        f.eq("city", "Denver\" OR city = \"Boulder");
        // The whole thing must remain one quoted literal, not two clauses.
        assert_eq!(
            f.build(),
            r#"region AND city = "Denver\" OR city = \"Boulder""#
        );
    }

    #[test]
    fn an_empty_list_adds_no_clause() {
        let mut f = Filter::within("region");
        f.any_of("categories", &[]);
        f.any_of("tags", &["  ".to_string()]);
        assert_eq!(f.build(), "region");
    }

    #[test]
    fn any_of_builds_an_in_list() {
        let mut f = Filter::default();
        f.any_of("categories", &["cafe".into(), "bakery".into()]);
        assert_eq!(f.build(), r#"categories IN ["cafe", "bakery"]"#);
    }
}
