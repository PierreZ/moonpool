//! Network partition chaos: pair, send, recv partitions and bulk triggering.
//!
//! Public partition methods are exposed as inherent methods on
//! [`SimWorld`]. The `pub(super)` helpers are called from world.rs's
//! event-dispatch loop.

use std::{collections::HashSet, net::IpAddr, time::Duration};

use crate::{
    SimulationResult,
    chaos::fault_events::SimFaultEvent,
    network::PartitionStrategy,
    sim::{
        events::{ConnectionStateChange, Event, ScheduledEvent},
        rng::{sim_random, sim_random_range},
        state::PartitionState,
        world::{SimInner, SimWorld},
    },
};

impl SimWorld {
    /// Partition communication between two IP addresses for the given duration.
    pub fn partition_pair(
        &self,
        from_ip: std::net::IpAddr,
        to_ip: std::net::IpAddr,
        duration: Duration,
    ) -> SimulationResult<()> {
        let mut inner = self.inner.borrow_mut();
        let expires_at = inner.current_time + duration;

        inner
            .network
            .ip_partitions
            .insert((from_ip, to_ip), PartitionState { expires_at });

        let restore_event = Event::Connection {
            id: 0,
            state: ConnectionStateChange::PartitionRestore,
        };
        let sequence = inner.next_sequence;
        inner.next_sequence += 1;
        let scheduled_event = ScheduledEvent::new(expires_at, restore_event, sequence);
        inner.event_queue.schedule(scheduled_event);

        inner.emit_fault(SimFaultEvent::PartitionCreated {
            from: from_ip.to_string(),
            to: to_ip.to_string(),
        });

        tracing::debug!(
            "Partitioned {} -> {} until {:?}",
            from_ip,
            to_ip,
            expires_at
        );
        Ok(())
    }

    /// Block all outgoing communication from an IP address.
    pub fn partition_send_from(
        &self,
        ip: std::net::IpAddr,
        duration: Duration,
    ) -> SimulationResult<()> {
        let mut inner = self.inner.borrow_mut();
        let expires_at = inner.current_time + duration;

        inner.network.send_partitions.insert(ip, expires_at);

        let clear_event = Event::Connection {
            id: 0,
            state: ConnectionStateChange::SendPartitionClear,
        };
        let sequence = inner.next_sequence;
        inner.next_sequence += 1;
        let scheduled_event = ScheduledEvent::new(expires_at, clear_event, sequence);
        inner.event_queue.schedule(scheduled_event);

        inner.emit_fault(SimFaultEvent::SendPartitionCreated { ip: ip.to_string() });
        tracing::debug!("Partitioned sends from {} until {:?}", ip, expires_at);
        Ok(())
    }

    /// Block all incoming communication to an IP address.
    pub fn partition_recv_to(
        &self,
        ip: std::net::IpAddr,
        duration: Duration,
    ) -> SimulationResult<()> {
        let mut inner = self.inner.borrow_mut();
        let expires_at = inner.current_time + duration;

        inner.network.recv_partitions.insert(ip, expires_at);

        let clear_event = Event::Connection {
            id: 0,
            state: ConnectionStateChange::RecvPartitionClear,
        };
        let sequence = inner.next_sequence;
        inner.next_sequence += 1;
        let scheduled_event = ScheduledEvent::new(expires_at, clear_event, sequence);
        inner.event_queue.schedule(scheduled_event);

        inner.emit_fault(SimFaultEvent::RecvPartitionCreated { ip: ip.to_string() });
        tracing::debug!("Partitioned receives to {} until {:?}", ip, expires_at);
        Ok(())
    }

    /// Immediately restore communication between two IP addresses.
    pub fn restore_partition(
        &self,
        from_ip: std::net::IpAddr,
        to_ip: std::net::IpAddr,
    ) -> SimulationResult<()> {
        let mut inner = self.inner.borrow_mut();
        inner.network.ip_partitions.remove(&(from_ip, to_ip));
        inner.emit_fault(SimFaultEvent::PartitionHealed {
            from: from_ip.to_string(),
            to: to_ip.to_string(),
        });
        tracing::debug!("Restored partition {} -> {}", from_ip, to_ip);
        Ok(())
    }

    /// Check if communication between two IP addresses is currently partitioned.
    pub fn is_partitioned(
        &self,
        from_ip: std::net::IpAddr,
        to_ip: std::net::IpAddr,
    ) -> SimulationResult<bool> {
        let inner = self.inner.borrow();
        Ok(inner
            .network
            .is_partitioned(from_ip, to_ip, inner.current_time))
    }
}

/// Clear expired pair partitions.
pub(super) fn clear_expired_partitions(inner: &mut SimInner) {
    let now = inner.current_time;
    let expired: Vec<_> = inner
        .network
        .ip_partitions
        .iter()
        .filter_map(|(pair, state)| (now >= state.expires_at).then_some(*pair))
        .collect();

    for pair in expired {
        inner.network.ip_partitions.remove(&pair);
        tracing::debug!("Restored IP partition {} -> {}", pair.0, pair.1);
    }
}

/// Clear expired send partitions.
pub(super) fn clear_expired_send_partitions(inner: &mut SimInner) {
    let now = inner.current_time;
    let expired: Vec<_> = inner
        .network
        .send_partitions
        .iter()
        .filter_map(|(ip, &expires_at)| (now >= expires_at).then_some(*ip))
        .collect();

    for ip in expired {
        inner.network.send_partitions.remove(&ip);
        tracing::debug!("Cleared send partition for {}", ip);
    }
}

/// Clear expired recv partitions.
pub(super) fn clear_expired_recv_partitions(inner: &mut SimInner) {
    let now = inner.current_time;
    let expired: Vec<_> = inner
        .network
        .recv_partitions
        .iter()
        .filter_map(|(ip, &expires_at)| (now >= expires_at).then_some(*ip))
        .collect();

    for ip in expired {
        inner.network.recv_partitions.remove(&ip);
        tracing::debug!("Cleared receive partition for {}", ip);
    }
}

/// Roll the configured chaos dice and partition a random subset of IPs.
///
/// Supports different partition strategies based on configuration:
/// - Random: randomly partition individual IP pairs
/// - UniformSize: create uniform-sized partition groups
/// - IsolateSingle: isolate exactly one node from the rest
pub(super) fn randomly_trigger_partitions(inner: &mut SimInner) {
    let partition_config = &inner.network.config;

    if partition_config.chaos.partition_probability == 0.0 {
        return;
    }

    if sim_random::<f64>() >= partition_config.chaos.partition_probability {
        return;
    }

    let unique_ips: HashSet<IpAddr> = inner
        .network
        .connections
        .values()
        .filter_map(|conn| conn.local_ip)
        .collect();

    if unique_ips.len() < 2 {
        return;
    }

    let ip_list: Vec<IpAddr> = unique_ips.into_iter().collect();
    let partition_duration =
        crate::network::sample_duration(&partition_config.chaos.partition_duration);
    let expires_at = inner.current_time + partition_duration;

    let partitioned_ips: Vec<IpAddr> = match partition_config.chaos.partition_strategy {
        PartitionStrategy::Random => ip_list
            .iter()
            .filter(|_| sim_random::<f64>() < 0.5)
            .copied()
            .collect(),
        PartitionStrategy::UniformSize => {
            let partition_size = sim_random_range(1..ip_list.len());
            let mut shuffled = ip_list.clone();
            for i in (1..shuffled.len()).rev() {
                let j = sim_random_range(0..i + 1);
                shuffled.swap(i, j);
            }
            shuffled.into_iter().take(partition_size).collect()
        }
        PartitionStrategy::IsolateSingle => {
            let idx = sim_random_range(0..ip_list.len());
            vec![ip_list[idx]]
        }
    };

    if partitioned_ips.is_empty() || partitioned_ips.len() == ip_list.len() {
        return;
    }

    let non_partitioned: Vec<IpAddr> = ip_list
        .iter()
        .filter(|ip| !partitioned_ips.contains(ip))
        .copied()
        .collect();

    for &from_ip in &partitioned_ips {
        for &to_ip in &non_partitioned {
            if inner
                .network
                .is_partitioned(from_ip, to_ip, inner.current_time)
            {
                continue;
            }

            inner
                .network
                .ip_partitions
                .insert((from_ip, to_ip), PartitionState { expires_at });
            inner
                .network
                .ip_partitions
                .insert((to_ip, from_ip), PartitionState { expires_at });

            inner.emit_fault(SimFaultEvent::PartitionCreated {
                from: from_ip.to_string(),
                to: to_ip.to_string(),
            });

            tracing::debug!(
                "Partition triggered: {} <-> {} until {:?} (strategy: {:?})",
                from_ip,
                to_ip,
                expires_at,
                partition_config.chaos.partition_strategy
            );
        }
    }

    let restore_event = Event::Connection {
        id: 0,
        state: ConnectionStateChange::PartitionRestore,
    };
    let sequence = inner.next_sequence;
    inner.next_sequence += 1;
    let scheduled_event = ScheduledEvent::new(expires_at, restore_event, sequence);
    inner.event_queue.schedule(scheduled_event);
}
