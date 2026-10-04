//! `SimContext::crash_self`: a process crashes itself at an exact point of
//! its own protocol, here while a sync is in flight, and the unsynced
//! sectors resolve by crash physics (moonpool#297).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use moonpool_core::{OpenOptions, StorageFile, StorageProvider};
use moonpool_sim::{
    Process, RebootKind, SimContext, SimulationBuilder, SimulationError, SimulationResult,
    TimeProvider, Workload,
};

const SECTOR: usize = 512;
const SECTORS: usize = 8;
const OLD: u8 = 0x11;
const NEW: u8 = 0x22;

/// First boot: sync `OLD`, write `NEW`, crash itself, then sync. Second
/// boot: publish what each sector holds.
struct Writer;

#[async_trait]
impl Process for Writer {
    fn name(&self) -> &'static str {
        "writer"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let boots: u64 = ctx.state().get("boots").unwrap_or(0) + 1;
        ctx.state().publish("boots", boots);
        let options = if boots == 1 {
            OpenOptions::create_write().read(true)
        } else {
            OpenOptions::read_write()
        };
        let file = ctx.storage().open("data", options).await?;
        if boots == 1 {
            file.write_at(0, &[OLD; SECTOR * SECTORS]).await?;
            file.sync_all().await?;
            file.write_at(0, &[NEW; SECTOR * SECTORS]).await?;
            ctx.crash_self(RebootKind::Crash, Some(Duration::from_millis(10)))?;
            file.sync_all().await?;
            ctx.state().publish("ran past the crash", true);
        } else {
            let mut bytes = vec![0; SECTOR * SECTORS];
            file.read_at(0, &mut bytes).await?;
            let sectors: Vec<u8> = bytes.chunks(SECTOR).map(|sector| sector[0]).collect();
            for sector in bytes.chunks(SECTOR) {
                assert!(
                    sector.iter().all(|byte| *byte == sector[0]),
                    "a sector resolves whole"
                );
            }
            ctx.state().publish("sectors", sectors);
        }
        ctx.shutdown().cancelled().await;
        Ok(())
    }
}

/// Waits for the second boot's verdict and records it.
struct Judge {
    seen: Arc<Mutex<Vec<Vec<u8>>>>,
}

#[async_trait]
impl Workload for Judge {
    fn name(&self) -> &'static str {
        "judge"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        assert!(
            ctx.crash_self(RebootKind::Crash, None).is_err(),
            "a workload cannot crash itself"
        );
        loop {
            if let Some(sectors) = ctx.state().get::<Vec<u8>>("sectors") {
                assert!(
                    !ctx.state()
                        .get::<bool>("ran past the crash")
                        .unwrap_or(false),
                    "nothing runs past the crash"
                );
                self.seen
                    .lock()
                    .expect("Mutex poisoned: prior task panicked")
                    .push(sectors);
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
fn a_crash_issued_before_a_sync_completes_resolves_by_crash_physics() {
    let seen: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
    let judge_seen = Arc::clone(&seen);
    let report = SimulationBuilder::new()
        .processes(1, || Box::new(Writer))
        .workload_factory(move || {
            Box::new(Judge {
                seen: Arc::clone(&judge_seen),
            })
        })
        .set_iterations(10)
        .run()
        .expect("simulation configuration is valid");
    assert_eq!(report.failed_runs, 0, "{report:?}");
    let seen = seen.lock().expect("Mutex poisoned: prior task panicked");
    assert_eq!(seen.len(), 10, "every seed reached the second boot");
    for sectors in seen.iter() {
        assert!(
            sectors.iter().all(|byte| *byte == OLD || *byte == NEW),
            "each sector is old or new: {sectors:?}"
        );
    }
    assert!(
        seen.iter().flatten().any(|byte| *byte == OLD),
        "the sync never completed: some sector rolled back"
    );
}
