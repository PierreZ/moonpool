//! Owner-scoped targeted injection from a scripted fault injector: every
//! process holds the same path, and `FaultContext::corrupt_bytes` damages
//! exactly the named bytes of one of them (moonpool#296).

use std::time::Duration;

use async_trait::async_trait;
use moonpool_core::{OpenOptions, StorageFile, StorageProvider};
use moonpool_sim::{
    FaultContext, FaultInjector, Process, SimContext, SimulationBuilder, SimulationError,
    SimulationResult, TimeProvider, Workload,
};

const LEN: usize = 1024;
const DAMAGED: std::ops::Range<u64> = 100..103;

fn pattern() -> Vec<u8> {
    (0..LEN).map(|at| at.to_le_bytes()[0] ^ 0x3C).collect()
}

/// Writes and syncs `data`, waits for the injector, then publishes how many
/// of its bytes changed.
struct Replica;

#[async_trait]
impl Process for Replica {
    fn name(&self) -> &'static str {
        "replica"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let file = ctx
            .storage()
            .open("data", OpenOptions::create_write().read(true))
            .await?;
        file.write_at(0, &pattern()).await?;
        file.sync_all().await?;
        ctx.state()
            .publish(&format!("written:{}", ctx.my_ip()), true);
        while !ctx.state().get::<bool>("corrupted").unwrap_or(false) {
            ctx.time()
                .sleep(Duration::from_millis(5))
                .await
                .map_err(|e| SimulationError::InvalidState(format!("sleep: {e}")))?;
        }
        let mut bytes = vec![0; LEN];
        file.read_at(0, &mut bytes).await?;
        let clean = pattern();
        let changed: Vec<u64> = (0..LEN)
            .filter(|at| bytes[*at] != clean[*at])
            .map(|at| u64::try_from(at).expect("small"))
            .collect();
        ctx.state()
            .publish(&format!("changed:{}", ctx.my_ip()), changed);
        ctx.shutdown().cancelled().await;
        Ok(())
    }
}

/// Damages `DAMAGED` of the first replica once every replica has written.
struct Corrupter;

#[async_trait]
impl FaultInjector for Corrupter {
    fn name(&self) -> &'static str {
        "corrupter"
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        let ips = ctx.process_ips().to_vec();
        while !ips.iter().all(|ip| {
            ctx.state()
                .get::<bool>(&format!("written:{ip}"))
                .unwrap_or(false)
        }) {
            ctx.time()
                .sleep(Duration::from_millis(5))
                .await
                .map_err(|e| SimulationError::InvalidState(format!("sleep: {e}")))?;
        }
        // Read the target's disk first, as an injector aiming by layout would.
        let file = ctx
            .storage(&ips[0])?
            .open("data", OpenOptions::read_only())
            .await?;
        let mut bytes = vec![0; LEN];
        file.read_at(0, &mut bytes).await?;
        assert_eq!(bytes, pattern(), "the injector reads the target's bytes");
        ctx.corrupt_bytes(&ips[0], "data", DAMAGED)?;
        ctx.state().publish("corrupted", true);
        Ok(())
    }
}

/// Waits for every replica's verdict: the first saw exactly the damaged
/// bytes change, the others none.
struct Judge;

#[async_trait]
impl Workload for Judge {
    fn name(&self) -> &'static str {
        "judge"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let ips = ctx.topology().all_process_ips().to_vec();
        loop {
            let seen: Vec<Option<Vec<u64>>> = ips
                .iter()
                .map(|ip| ctx.state().get(&format!("changed:{ip}")))
                .collect();
            if seen.iter().all(Option::is_some) {
                for (rank, changed) in seen.into_iter().flatten().enumerate() {
                    let expected: Vec<u64> = if rank == 0 {
                        DAMAGED.collect()
                    } else {
                        Vec::new()
                    };
                    if changed != expected {
                        return Err(SimulationError::InvalidState(format!(
                            "replica {rank} changed {changed:?}, expected {expected:?}"
                        )));
                    }
                }
                return Ok(());
            }
            ctx.time()
                .sleep(Duration::from_millis(5))
                .await
                .map_err(|e| SimulationError::InvalidState(format!("sleep: {e}")))?;
        }
    }
}

#[test]
fn a_fault_injector_damages_one_replica_byte_for_byte() {
    let report = SimulationBuilder::new()
        .processes(3, || Box::new(Replica))
        .workload_factory(|| Box::new(Judge))
        .fault_factory(|| Box::new(Corrupter))
        .chaos_duration(Duration::from_secs(5))
        .set_iterations(3)
        .run()
        .expect("simulation configuration is valid");
    assert_eq!(report.failed_runs, 0, "{report:?}");
    assert_eq!(report.successful_runs, 3);
}
