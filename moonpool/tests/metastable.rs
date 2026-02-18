//! AWS DynamoDB metastable failure simulation.
//!
//! Models the October 2025 AWS DynamoDB outage: a DNS race condition triggers
//! congestive collapse in the fleet management layer. The defining characteristic
//! of a metastable failure is that the trigger resolves but the system stays broken.
//!
//! Architecture:
//! - DnsManager: service discovery with latent race condition (trigger)
//! - LeaseStore: capacity-limited lease service (DynamoDB analog)
//! - FleetManager: 16 hosts with autonomous lease renewal loops (DWFM analog)
//! - ClientDriver: traffic generator + metastable failure detector

#[path = "metastable/mod.rs"]
mod metastable;
