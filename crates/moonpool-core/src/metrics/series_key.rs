//! Series identity: the canonical `name{key="value",...}` key that recorded
//! series and scraped samples share.

use std::fmt;

use super::series_identity;

/// A Prometheus series identity: a metric name plus a label set.
///
/// Labels are kept sorted by key, so two label sets that differ only in
/// insertion order are the same series. [`Display`](fmt::Display) renders the
/// canonical form `name{key="value",...}` — byte-identical to
/// [`MetricSample::sort_key`], which is what recorded series are keyed by.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SeriesKey {
    /// Metric name, e.g. `http_requests_total`.
    pub metric: String,
    /// Label pairs, sorted by key.
    pub labels: Vec<(String, String)>,
}

impl SeriesKey {
    /// Build a key, sorting `labels` into canonical order.
    #[must_use]
    pub fn new(metric: impl Into<String>, mut labels: Vec<(String, String)>) -> Self {
        labels.sort();
        Self {
            metric: metric.into(),
            labels,
        }
    }

    /// Parse the canonical `name{key="value",...}` form.
    ///
    /// Label values are unescaped as in the Prometheus text format (`\\`,
    /// `\"`, `\n`), the escapes the key formatter writes, so every key
    /// round trips exactly. Anything that is not that shape is taken as a bare
    /// metric name with no labels.
    #[must_use]
    pub fn parse(key: &str) -> Self {
        let Some((metric, rest)) = key.split_once('{') else {
            return Self::new(key, Vec::new());
        };
        let Some(inner) = rest.strip_suffix('}') else {
            return Self::new(key, Vec::new());
        };
        match parse_labels(inner) {
            Some(labels) => Self::new(metric, labels),
            None => Self::new(key, Vec::new()),
        }
    }

    /// The value of `key`, when this series carries that label.
    #[must_use]
    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// Parse `key="value",...` with escaped values. `None` when malformed.
fn parse_labels(inner: &str) -> Option<Vec<(String, String)>> {
    let mut labels = Vec::new();
    let mut rest = inner;
    while !rest.is_empty() {
        let (key, after) = rest.split_once("=\"")?;
        let mut value = String::new();
        let mut chars = after.char_indices();
        let end = loop {
            match chars.next()? {
                (i, '"') => break i,
                (_, '\\') => match chars.next()?.1 {
                    'n' => value.push('\n'),
                    escaped => value.push(escaped),
                },
                (_, c) => value.push(c),
            }
        };
        labels.push((key.to_owned(), value));
        rest = &after[end + 1..];
        if let Some(next) = rest.strip_prefix(',') {
            rest = next;
        } else if !rest.is_empty() {
            return None;
        }
    }
    Some(labels)
}

impl fmt::Display for SeriesKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&series_identity(&self.metric, &self.labels))
    }
}

#[cfg(test)]
mod tests {
    use super::super::{MetricSample, MetricValue};
    use super::SeriesKey;

    #[test]
    fn label_values_are_escaped_so_identities_do_not_collide() {
        let tricky = MetricSample::new(
            "m",
            vec![("a".to_owned(), r#"x",b="y"#.to_owned())],
            MetricValue::Counter(1.0),
        );
        let plain = MetricSample::new(
            "m",
            vec![
                ("a".to_owned(), "x".to_owned()),
                ("b".to_owned(), "y".to_owned()),
            ],
            MetricValue::Counter(1.0),
        );
        assert_ne!(tricky.sort_key(), plain.sort_key());

        for value in [r#"x",b="y"#, r"back\slash", "new\nline", "", "a,b"] {
            let key = SeriesKey::new(
                "m",
                vec![
                    ("a".to_owned(), value.to_owned()),
                    ("z".to_owned(), "1".to_owned()),
                ],
            );
            let parsed = SeriesKey::parse(&key.to_string());
            assert_eq!(parsed, key, "{value:?} round trips");
        }
    }

    #[test]
    fn parse_round_trips_the_canonical_form() {
        let key = SeriesKey::parse(r#"requests_total{method="GET",code="200"}"#);
        assert_eq!(key.metric, "requests_total");
        assert_eq!(key.label("code"), Some("200"));
        assert_eq!(
            key.to_string(),
            r#"requests_total{code="200",method="GET"}"#
        );

        let bare = SeriesKey::parse("uptime");
        assert_eq!(bare.metric, "uptime");
        assert!(bare.labels.is_empty(), "a bare metric name has no labels");
        assert_eq!(bare.to_string(), "uptime");
    }

    #[test]
    fn a_sample_and_its_recorded_series_agree_on_identity() {
        let sample = MetricSample::new(
            "requests_total",
            vec![
                ("op".to_owned(), "write".to_owned()),
                ("instance".to_owned(), "10.0.1.1".to_owned()),
            ],
            MetricValue::Counter(1.0),
        );
        assert_eq!(
            SeriesKey::parse(&sample.sort_key()),
            SeriesKey::new(sample.name.clone(), sample.labels.clone()),
        );
    }
}
