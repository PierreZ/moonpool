//! Tag-based topology for process distribution and fault targeting.
//!
//! Tags allow processes to be grouped by arbitrary attributes (datacenter,
//! rack, role, etc.) and targeted by fault injectors. Tags are distributed
//! round-robin across process instances.
//!
//! # Example
//!
//! ```ignore
//! // 5 processes with tags distributed round-robin:
//! // dc: east, west, eu, east, west
//! // rack: r1, r2, r1, r2, r1
//! .processes(5, || MyNode::new())
//!     .tags(&[
//!         ("dc", &["east", "west", "eu"]),
//!         ("rack", &["r1", "r2"]),
//!     ])
//! ```

use std::collections::HashMap;
use std::net::IpAddr;

/// Distribution specification for a single tag dimension.
///
/// Contains the tag key and the values to round-robin across processes.
#[derive(Debug, Clone, PartialEq)]
pub struct TagDimension {
    /// The tag key (e.g., "dc", "rack", "role").
    pub key: String,
    /// The values to distribute round-robin (e.g., ["east", "west", "eu"]).
    pub values: Vec<String>,
}

/// Tag distribution configuration for a set of processes.
///
/// Multiple tag dimensions are distributed independently via round-robin.
#[derive(Debug, Clone, Default)]
pub struct TagDistribution {
    dimensions: Vec<TagDimension>,
}

impl TagDistribution {
    /// Create a new empty tag distribution.
    pub fn new() -> Self {
        Self {
            dimensions: Vec::new(),
        }
    }

    /// Add a tag dimension from slices.
    pub fn add(&mut self, key: &str, values: &[&str]) {
        self.dimensions.push(TagDimension {
            key: key.to_string(),
            values: values.iter().map(|v| v.to_string()).collect(),
        });
    }

    /// Check if this distribution has any dimensions.
    pub fn is_empty(&self) -> bool {
        self.dimensions.is_empty()
    }

    /// Resolve tags for a specific process index using round-robin distribution.
    ///
    /// Each tag dimension is distributed independently: process `i` gets
    /// `values[i % values.len()]` for each dimension.
    pub fn resolve(&self, index: usize) -> ProcessTags {
        let mut tags = HashMap::new();
        for dim in &self.dimensions {
            if !dim.values.is_empty() {
                let value = &dim.values[index % dim.values.len()];
                tags.insert(dim.key.clone(), value.clone());
            }
        }
        ProcessTags { tags }
    }
}

/// Resolved tags for a specific process instance.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProcessTags {
    tags: HashMap<String, String>,
}

impl ProcessTags {
    /// Get the value of a tag by key.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.tags.get(key).map(|s| s.as_str())
    }

    /// Check if a tag key-value pair matches.
    pub fn matches(&self, key: &str, value: &str) -> bool {
        self.tags.get(key).is_some_and(|v| v == value)
    }

    /// Get all tags as key-value pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.tags.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Check if this process has any tags.
    pub fn is_empty(&self) -> bool {
        self.tags.is_empty()
    }
}

/// Registry mapping process IPs to their resolved tags.
///
/// Used by the orchestrator and fault context to look up tags by IP
/// and find IPs matching tag queries.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TagRegistry {
    ip_tags: HashMap<IpAddr, ProcessTags>,
}

impl TagRegistry {
    /// Create a new empty tag registry.
    pub fn new() -> Self {
        Self {
            ip_tags: HashMap::new(),
        }
    }

    /// Register tags for a process IP.
    pub fn register(&mut self, ip: IpAddr, tags: ProcessTags) {
        self.ip_tags.insert(ip, tags);
    }

    /// Get tags for a specific IP.
    pub fn tags_for(&self, ip: IpAddr) -> Option<&ProcessTags> {
        self.ip_tags.get(&ip)
    }

    /// Find all IPs matching a tag key-value pair.
    pub fn ips_tagged(&self, key: &str, value: &str) -> Vec<IpAddr> {
        self.ip_tags
            .iter()
            .filter(|(_, tags)| tags.matches(key, value))
            .map(|(ip, _)| *ip)
            .collect()
    }

    /// Get all registered process IPs.
    pub fn all_ips(&self) -> Vec<IpAddr> {
        self.ip_tags.keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_robin_distribution() {
        let mut dist = TagDistribution::new();
        dist.add("dc", &["east", "west", "eu"]);
        dist.add("rack", &["r1", "r2"]);

        // 5 processes
        let tags: Vec<_> = (0..5).map(|i| dist.resolve(i)).collect();

        assert_eq!(tags[0].get("dc"), Some("east"));
        assert_eq!(tags[1].get("dc"), Some("west"));
        assert_eq!(tags[2].get("dc"), Some("eu"));
        assert_eq!(tags[3].get("dc"), Some("east"));
        assert_eq!(tags[4].get("dc"), Some("west"));

        assert_eq!(tags[0].get("rack"), Some("r1"));
        assert_eq!(tags[1].get("rack"), Some("r2"));
        assert_eq!(tags[2].get("rack"), Some("r1"));
        assert_eq!(tags[3].get("rack"), Some("r2"));
        assert_eq!(tags[4].get("rack"), Some("r1"));
    }

    #[test]
    fn tag_matching() {
        let mut dist = TagDistribution::new();
        dist.add("dc", &["east", "west"]);
        let tags = dist.resolve(0);

        assert!(tags.matches("dc", "east"));
        assert!(!tags.matches("dc", "west"));
        assert!(!tags.matches("rack", "r1"));
    }

    #[test]
    fn tag_registry_queries() {
        let mut registry = TagRegistry::new();

        let ip1: IpAddr = "10.0.1.1".parse().expect("valid IP");
        let ip2: IpAddr = "10.0.1.2".parse().expect("valid IP");
        let ip3: IpAddr = "10.0.1.3".parse().expect("valid IP");

        let mut dist = TagDistribution::new();
        dist.add("dc", &["east", "west", "east"]);

        registry.register(ip1, dist.resolve(0)); // east
        registry.register(ip2, dist.resolve(1)); // west
        registry.register(ip3, dist.resolve(2)); // east

        let east_ips = registry.ips_tagged("dc", "east");
        assert_eq!(east_ips.len(), 2);
        assert!(east_ips.contains(&ip1));
        assert!(east_ips.contains(&ip3));

        let west_ips = registry.ips_tagged("dc", "west");
        assert_eq!(west_ips.len(), 1);
        assert!(west_ips.contains(&ip2));

        assert_eq!(registry.all_ips().len(), 3);
    }

    #[test]
    fn empty_distribution() {
        let dist = TagDistribution::new();
        assert!(dist.is_empty());
        let tags = dist.resolve(0);
        assert!(tags.is_empty());
    }
}
