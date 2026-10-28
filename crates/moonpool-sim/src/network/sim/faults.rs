//! Connection faults: partitions, random closes and black holes.

use std::{collections::BTreeSet, net::IpAddr, time::Duration};

use crate::{
    assert_reachable,
    chaos::fault_events::SimFaultEvent,
    locality::DomainLevel,
    network::PartitionStrategy,
    sim::{
        rng::{sim_random, sim_random_f64, sim_random_range},
        wakers::WakeBatch,
    },
};

use super::engine::{NetworkActions, NetworkSimulation};
use super::{ConnectionId, NetworkEvent, state::PartitionState};

/// Draw which directions a connection fault hits, as `(send, recv)`: this
/// endpoint's sends, its peer's, or both, roughly a third each. One draw.
fn draw_fault_direction() -> (bool, bool) {
    let a = sim_random_f64();
    (a > 0.33, a < 0.66)
}

/// Draw the gap to the next arrival of a Poisson process with `rate`
/// arrivals per simulated second (FDB's `poisson()`): exponential with mean
/// `1 / rate`. One draw; an infinite rate gives a zero gap.
fn sample_arrival_gap(rate: f64) -> Duration {
    if rate.is_infinite() {
        return Duration::ZERO;
    }
    // `1 - u` is in (0, 1], so the logarithm is finite and not positive.
    let u = sim_random_f64();
    let seconds = -(1.0 - u).ln() / rate;
    Duration::try_from_secs_f64(seconds).unwrap_or(Duration::MAX)
}

/// Advance a Poisson process stored in `next_at`: returns whether an
/// arrival is due at `now`. Draws randomness only when the process is
/// (re)armed, never on an event that finds the arrival still ahead.
fn arrival_due(next_at: &mut Option<Duration>, rate: f64, now: Duration) -> bool {
    if rate.is_nan() || rate <= 0.0 {
        *next_at = None;
        return false;
    }
    let due = *next_at.get_or_insert_with(|| now.saturating_add(sample_arrival_gap(rate)));
    now >= due
}

impl NetworkSimulation {
    /// Start a random partition when its arrival is due.
    ///
    /// `partition_rate` is a rate per simulated second, not a per-event coin
    /// (a per-event coin makes the partition count grow with traffic: a busy
    /// run saw hundreds per second). At most one random
    /// partition is in force at a time: the next arrival is drawn from the
    /// moment this one heals (`TigerBeetle`'s unpartition stability), so every
    /// partition is followed by a healed gap of mean `1 / partition_rate`.
    pub(super) fn randomly_trigger_partitions(&mut self, now: Duration) -> NetworkActions {
        let rate = self.state.config.chaos.partition_rate;
        if !arrival_due(&mut self.state.next_partition_at, rate, now) {
            return NetworkActions::default();
        }
        let (actions, heals_at) = self.start_random_partition(now);
        self.state.next_partition_at = Some(heals_at.saturating_add(sample_arrival_gap(rate)));
        actions
    }

    /// Start one random partition under the configured strategy. Returns its
    /// actions and when it heals (`now` when no partition could be made).
    fn start_random_partition(&mut self, now: Duration) -> (NetworkActions, Duration) {
        let chaos = &self.state.config.chaos;
        let strategy = chaos.partition_strategy;
        let duration_range = chaos.partition_duration.clone();
        let mut ips = self
            .state
            .connections
            .values()
            .filter(|connection| !connection.flags.is_closed())
            .filter_map(|connection| connection.local_ip)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        ips.sort_unstable();
        if ips.len() < 2 {
            return (NetworkActions::default(), now);
        }
        let duration = crate::network::sample_duration(&duration_range);
        let deadline = now.saturating_add(duration);
        if matches!(
            strategy,
            PartitionStrategy::AsymmetricSend | PartitionStrategy::AsymmetricRecv
        ) {
            let ip = ips[sim_random_range(0..ips.len())];
            if !self.partition_allowed(&[ip], &[], now) {
                return (NetworkActions::default(), now);
            }
            let actions = if strategy == PartitionStrategy::AsymmetricSend {
                self.insert_send_partition(ip, duration, now)
            } else {
                self.insert_recv_partition(ip, duration, now)
            };
            return (actions, deadline);
        }
        let selected = self.select_partition_group(&ips, strategy);
        let mut actions = NetworkActions::default();
        if selected.is_empty() || selected.len() == ips.len() {
            return (actions, now);
        }
        let other = ips
            .iter()
            .filter(|ip| !selected.contains(ip))
            .copied()
            .collect::<Vec<_>>();
        if !self.partition_allowed(&selected, &other, now) {
            return (actions, now);
        }
        for from in selected {
            for &to in &other {
                if self.state.is_partitioned(from, to, now) {
                    continue;
                }
                self.state.ip_partitions.insert(
                    (from, to),
                    PartitionState {
                        expires_at: deadline,
                    },
                );
                self.state.ip_partitions.insert(
                    (to, from),
                    PartitionState {
                        expires_at: deadline,
                    },
                );
                actions.record(SimFaultEvent::PartitionCreated {
                    from: from.to_string(),
                    to: to.to_string(),
                });
            }
        }
        actions.schedule_at(
            deadline,
            NetworkEvent::PartitionRestore {
                expected_deadline: deadline,
            },
        );
        self.hold_partitioned_in_flight(now);
        (actions, deadline)
    }

    /// Clog one random IP's interface when the clog arrival is due (FDB's
    /// `RandomClogging` workload: a Poisson process of `clogInterface` calls
    /// on a random process).
    ///
    /// `clog_rate` is clogs per simulated second for the whole network, not a
    /// per-write coin, so traffic volume does not raise it. Each clog holds
    /// every open connection of the chosen IP for one `clog_duration` draw:
    /// its sends, its receives, or both (FDB `ClogDefault`). Clogs only
    /// delay; nothing is dropped. Clogs may overlap, as in FDB; an
    /// overlapping clog keeps the later deadline.
    pub(super) fn randomly_trigger_clogs(&mut self, now: Duration) -> NetworkActions {
        let rate = self.state.config.chaos.clog_rate;
        if !arrival_due(&mut self.state.next_clog_at, rate, now) {
            return NetworkActions::default();
        }
        self.state.next_clog_at = Some(now.saturating_add(sample_arrival_gap(rate)));
        let ips = self
            .state
            .connections
            .values()
            .filter(|connection| !connection.flags.is_closed())
            .filter_map(|connection| connection.local_ip)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        if ips.is_empty() {
            return NetworkActions::default();
        }
        let ip = ips[sim_random_range(0..ips.len())];
        let (clog_send, clog_recv) = draw_fault_direction();
        let duration = crate::network::sample_duration(&self.state.config.chaos.clog_duration);
        let deadline = now.saturating_add(duration);
        let ids = self
            .state
            .connections
            .iter()
            .filter(|(_, connection)| {
                !connection.flags.is_closed() && connection.local_ip == Some(ip)
            })
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        let mut actions = NetworkActions::default();
        for id in ids {
            if clog_send {
                actions.extend(self.clog_write_until(id, deadline));
            }
            if clog_recv {
                actions.extend(self.clog_read_until(id, deadline));
            }
        }
        let direction = match (clog_send, clog_recv) {
            (true, true) => "both",
            (true, false) => "send",
            _ => "recv",
        };
        assert_reachable!("network: interface clogged");
        actions.record(SimFaultEvent::InterfaceClogged {
            ip: ip.to_string(),
            direction: direction.to_string(),
            duration_ms: u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
        });
        actions
    }

    /// Whether the partition guard, if one is installed, allows splitting
    /// `a` from `b` (an empty `b`: cutting `a` off). Draws nothing.
    fn partition_allowed(&self, a: &[IpAddr], b: &[IpAddr], now: Duration) -> bool {
        let Some(guard) = self.partition_guard.0.clone() else {
            return true;
        };
        let cut = |x: IpAddr, y: IpAddr| {
            self.state.is_partitioned(x, y, now) || self.state.is_partitioned(y, x, now)
        };
        guard(a, b, &cut)
    }

    fn select_partition_group(&self, ips: &[IpAddr], strategy: PartitionStrategy) -> Vec<IpAddr> {
        match strategy {
            PartitionStrategy::UniformSize => {
                let count = sim_random_range(1..ips.len());
                let mut shuffled = ips.to_vec();
                for i in (1..shuffled.len()).rev() {
                    shuffled.swap(i, sim_random_range(0..i + 1));
                }
                shuffled.into_iter().take(count).collect()
            }
            PartitionStrategy::IsolateSingle => vec![ips[sim_random_range(0..ips.len())]],
            PartitionStrategy::IsolateZone => self
                .select_domain_group(ips, DomainLevel::Zone)
                .unwrap_or_else(|| Self::select_random_group(ips)),
            PartitionStrategy::IsolateDatacenter => self
                .select_domain_group(ips, DomainLevel::Datacenter)
                .unwrap_or_else(|| Self::select_random_group(ips)),
            _ => Self::select_random_group(ips),
        }
    }

    fn select_random_group(ips: &[IpAddr]) -> Vec<IpAddr> {
        ips.iter()
            .filter(|_| sim_random::<f64>() < 0.5)
            .copied()
            .collect()
    }

    fn select_domain_group(&self, ips: &[IpAddr], level: DomainLevel) -> Option<Vec<IpAddr>> {
        let mut domains = ips
            .iter()
            .filter_map(|ip| self.localities.get(ip))
            .map(|l| l.id_for(level))
            .collect::<Vec<_>>();
        domains.sort_unstable();
        domains.dedup();
        if domains.is_empty() {
            return None;
        }
        let selected = domains[sim_random_range(0..domains.len())];
        Some(
            ips.iter()
                .filter(|ip| {
                    self.localities
                        .get(ip)
                        .is_some_and(|l| l.id_for(level) == selected)
                })
                .copied()
                .collect(),
        )
    }

    pub(crate) fn partition_pair(
        &mut self,
        from: IpAddr,
        to: IpAddr,
        duration: Duration,
        now: Duration,
    ) -> NetworkActions {
        let deadline = now.saturating_add(duration);
        self.state.ip_partitions.insert(
            (from, to),
            PartitionState {
                expires_at: deadline,
            },
        );
        let mut actions = NetworkActions::default();
        actions.schedule_at(
            deadline,
            NetworkEvent::PartitionRestore {
                expected_deadline: deadline,
            },
        );
        actions.record(SimFaultEvent::PartitionCreated {
            from: from.to_string(),
            to: to.to_string(),
        });
        self.hold_partitioned_in_flight(now);
        actions
    }

    pub(crate) fn insert_send_partition(
        &mut self,
        ip: IpAddr,
        duration: Duration,
        now: Duration,
    ) -> NetworkActions {
        let deadline = now.saturating_add(duration);
        self.state.send_partitions.insert(ip, deadline);
        let mut actions = NetworkActions::default();
        actions.schedule_at(
            deadline,
            NetworkEvent::SendPartitionClear {
                expected_deadline: deadline,
            },
        );
        actions.record(SimFaultEvent::SendPartitionCreated { ip: ip.to_string() });
        self.hold_partitioned_in_flight(now);
        actions
    }

    pub(crate) fn insert_recv_partition(
        &mut self,
        ip: IpAddr,
        duration: Duration,
        now: Duration,
    ) -> NetworkActions {
        let deadline = now.saturating_add(duration);
        self.state.recv_partitions.insert(ip, deadline);
        let mut actions = NetworkActions::default();
        actions.schedule_at(
            deadline,
            NetworkEvent::RecvPartitionClear {
                expected_deadline: deadline,
            },
        );
        actions.record(SimFaultEvent::RecvPartitionCreated { ip: ip.to_string() });
        self.hold_partitioned_in_flight(now);
        actions
    }

    pub(crate) fn restore_partition(
        &mut self,
        from: IpAddr,
        to: IpAddr,
        now: Duration,
    ) -> NetworkActions {
        self.state.ip_partitions.remove(&(from, to));
        self.state.ip_partitions.remove(&(to, from));
        let mut actions = NetworkActions::default();
        actions.record(SimFaultEvent::PartitionHealed {
            from: from.to_string(),
            to: to.to_string(),
        });
        self.resume_after_heal(now, &mut actions);
        actions
    }

    /// Heal every environmental partition currently in force: directed pair
    /// cuts plus the send-side and receive-side blocks that
    /// [`restore_partition`](Self::restore_partition) cannot reach.
    ///
    /// Connections held back by a partition are re-driven — what was frozen in
    /// flight thaws and the stalled send queue follows it — so a stalled
    /// stream resumes instead of waiting out a deadline that no longer
    /// applies. No new randomness: this only re-times work already sampled.
    pub(crate) fn heal_all_partitions(&mut self, now: Duration) -> NetworkActions {
        let mut actions = NetworkActions::default();
        for (from, to) in std::mem::take(&mut self.state.ip_partitions).into_keys() {
            actions.record(SimFaultEvent::PartitionHealed {
                from: from.to_string(),
                to: to.to_string(),
            });
        }
        for ip in std::mem::take(&mut self.state.send_partitions).into_keys() {
            actions.record(SimFaultEvent::SendPartitionHealed { ip: ip.to_string() });
        }
        for ip in std::mem::take(&mut self.state.recv_partitions).into_keys() {
            actions.record(SimFaultEvent::RecvPartitionHealed { ip: ip.to_string() });
        }
        self.resume_after_heal(now, &mut actions);
        actions
    }

    /// Stop sampling new network faults (see
    /// [`ChaosConfiguration::disable_fault_injection`](crate::network::ChaosConfiguration::disable_fault_injection)).
    ///
    /// Consumes no randomness and leaves every already-produced effect in
    /// place, including the per-pair latencies sampled so far.
    pub(crate) fn disable_fault_injection(&mut self) {
        self.state.config.disable_fault_injection();
    }

    pub(crate) fn is_partitioned(&self, from: IpAddr, to: IpAddr, now: Duration) -> bool {
        self.state.is_partitioned(from, to, now)
    }

    pub(crate) fn roll_random_close(
        &mut self,
        id: ConnectionId,
        now: Duration,
    ) -> (Option<bool>, NetworkActions, WakeBatch) {
        let config = &self.state.config.chaos;
        if config.random_close_probability <= 0.0
            || now.saturating_sub(self.state.last_random_close_time) < config.random_close_cooldown
            || !crate::buggify_with_prob!(config.random_close_probability)
        {
            return (None, NetworkActions::default(), WakeBatch::default());
        }
        self.state.last_random_close_time = now;
        let (close_send, close_recv) = draw_fault_direction();
        let wakes = self.close_asymmetric(id, close_send, close_recv);
        let explicit = sim_random_f64() < self.state.config.chaos.random_close_explicit_ratio;
        let mut actions = NetworkActions::default();
        actions.record(SimFaultEvent::RandomClose {
            connection_id: id.0,
        });
        (Some(explicit), actions, wakes)
    }

    /// Roll the black-hole coin for one I/O on `id` (the `rollRandomClose`
    /// shape: own probability, own cooldown, one `buggify_with_prob!` draw).
    ///
    /// A hit black-holes this endpoint's sends, its peer's, or both — the same
    /// three-way direction draw a random close makes — and is recorded once as
    /// [`SimFaultEvent::BlackHole`]. Nothing is returned to the caller: the
    /// operation that drew the fault proceeds normally, which is the point.
    /// Draws nothing while the family is off.
    pub(crate) fn roll_black_hole(&mut self, id: ConnectionId, now: Duration) -> NetworkActions {
        let config = &self.state.config.chaos;
        if self
            .connection(id)
            .is_none_or(|connection| connection.flags.is_closed())
            || config.black_hole_probability <= 0.0
            || now.saturating_sub(self.state.last_black_hole_time) < config.black_hole_cooldown
            || !crate::buggify_with_prob!(config.black_hole_probability)
        {
            return NetworkActions::default();
        }
        self.state.last_black_hole_time = now;
        let (hole_send, hole_recv) = draw_fault_direction();
        assert_reachable!("network: connection black-holed");
        self.black_hole(id, hole_send, hole_recv)
    }

    /// Black-hole `id`'s sends (`hole_send`) and/or its peer's (`hole_recv`).
    ///
    /// Permanent for the connection's lifetime and idempotent per direction;
    /// the fault is recorded only when a direction that was not yet holed is.
    pub(crate) fn black_hole(
        &mut self,
        id: ConnectionId,
        hole_send: bool,
        hole_recv: bool,
    ) -> NetworkActions {
        let paired = self.paired(id);
        let mut newly_send = false;
        if hole_send
            && let Some(c) = self.state.connections.get_mut(&id)
            && !c.flags.send_black_holed()
        {
            c.flags.set_send_black_holed(true);
            newly_send = true;
        }
        let mut newly_recv = false;
        if hole_recv
            && let Some(c) = paired.and_then(|peer| self.state.connections.get_mut(&peer))
            && !c.flags.send_black_holed()
        {
            c.flags.set_send_black_holed(true);
            newly_recv = true;
        }
        let mut actions = NetworkActions::default();
        let direction = match (newly_send, newly_recv) {
            (true, true) => "both",
            (true, false) => "send",
            (false, true) => "recv",
            (false, false) => return actions,
        };
        actions.record(SimFaultEvent::BlackHole {
            connection_id: id.0,
            direction: direction.to_string(),
        });
        actions
    }

    pub(crate) fn is_send_black_holed(&self, id: ConnectionId) -> bool {
        self.connection(id)
            .is_some_and(|connection| connection.flags.send_black_holed())
    }
}

#[cfg(test)]
mod tests {
    use super::arrival_due;
    use crate::sim::rng::{reset_sim_rng, set_sim_seed};
    use std::time::Duration;

    /// Count the arrivals a Poisson process with `rate` makes when it is
    /// polled every `step` for `span` of simulated time.
    fn arrivals(rate: f64, step: Duration, span: Duration) -> u32 {
        reset_sim_rng();
        set_sim_seed(7);
        let mut next_at = None;
        let mut count = 0;
        let mut now = Duration::ZERO;
        while now < span {
            if arrival_due(&mut next_at, rate, now) {
                count += 1;
                next_at = None;
            }
            now += step;
        }
        count
    }

    /// The arrival count follows simulated time, not how often the process
    /// is polled: a per-event coin made a busy run see hundreds of
    /// partitions per second.
    #[test]
    fn arrivals_follow_simulated_time_not_event_count() {
        let span = Duration::from_secs(100);
        let sparse = arrivals(0.5, Duration::from_millis(100), span);
        let dense = arrivals(0.5, Duration::from_micros(100), span);
        for count in [sparse, dense] {
            assert!(
                (25..=80).contains(&count),
                "expected about 50 arrivals in 100 s at 0.5/s, got {count}"
            );
        }
    }

    #[test]
    fn zero_rate_never_arrives_and_disarms() {
        let mut next_at = Some(Duration::ZERO);
        assert!(!arrival_due(&mut next_at, 0.0, Duration::from_secs(1)));
        assert_eq!(next_at, None);
    }
}
