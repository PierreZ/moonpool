//! The certificate and lifecycle script: during the chaos window it moves
//! the scripted UTC (small steps, and jumps past a leaf's lifetime), issues
//! new server and member certificates (the authority's fault sites may flaw
//! them), moves the server's client roots away and back, and crashes the
//! server. At the end it issues clean certificates valid at the final time,
//! restores the roots and marks itself done.

use std::time::Duration;

use async_trait::async_trait;
use moonpool_rpc::security::tls::pki::{Issued, SubjectName, Usage};
use moonpool_sim::{
    FaultContext, FaultInjector, RandomProvider, SimulationError, SimulationResult, TimeProvider,
    assert_reachable,
};

use super::{LEAF_LIFETIME, MEMBER, Pki, SCRIPT_DONE_KEY, SERVER, invalid, ip_name, issue};

/// The campaign's fault script.
#[derive(Debug, Default)]
pub struct TlsFaults;

async fn pause(ctx: &FaultContext, millis: u64) -> SimulationResult<()> {
    ctx.time()
        .sleep(Duration::from_millis(millis))
        .await
        .map_err(|error| SimulationError::InvalidState(format!("sleep failed: {error}")))
}

fn server_leaf(pki: &Pki, ctx: &FaultContext) -> SimulationResult<Issued> {
    let (ip, utc) = pki.with(|inner| (inner.server_ip.clone(), inner.utc));
    let names = vec![SubjectName::Uri(SERVER.into()), ip_name(&ip)?];
    pki.with(|inner| issue(&inner.authority, names, Usage::Server, utc, ctx.random()))
}

fn member_leaf(pki: &Pki, ctx: &FaultContext) -> SimulationResult<Issued> {
    let utc = pki.utc();
    let names = vec![SubjectName::Uri(MEMBER.into())];
    pki.with(|inner| issue(&inner.authority, names, Usage::Client, utc, ctx.random()))
}

/// Serve `issued` from the next handshake on.
fn rotate_server(pki: &Pki, issued: Issued) -> SimulationResult<()> {
    pki.with(|inner| {
        if let Some(tls) = &inner.server_tls {
            tls.certificate()
                .replace(issued.chain.clone(), issued.key())
                .map_err(invalid)?;
        }
        inner.server = issued;
        Ok(())
    })
}

/// Present `issued` from the member's next handshake on.
fn rotate_member(pki: &Pki, issued: Issued) -> SimulationResult<()> {
    pki.with(|inner| {
        if let Some(certificate) = inner.member_tls.as_ref().and_then(|tls| tls.certificate()) {
            certificate
                .replace(issued.chain.clone(), issued.key())
                .map_err(invalid)?;
        }
        inner.member = issued;
        Ok(())
    })
}

fn set_roots_swapped(pki: &Pki, swapped: bool) -> SimulationResult<()> {
    pki.with(|inner| inner.roots_swapped = swapped);
    let roots = pki.client_roots();
    pki.with(|inner| match &inner.server_tls {
        Some(tls) if tls.client_auth().verifies_clients() => {
            tls.replace_client_roots(roots).map_err(invalid)
        }
        _ => Ok(()),
    })
}

/// A leaf without a simulation flaw: an operator reissues until it is
/// right.
fn clean(mut make: impl FnMut() -> SimulationResult<Issued>) -> SimulationResult<Issued> {
    for _ in 0..64 {
        let issued = make()?;
        if issued.flaw.is_none() {
            return Ok(issued);
        }
    }
    Err(invalid("no clean leaf in 64 issues"))
}

#[async_trait]
impl FaultInjector for TlsFaults {
    fn name(&self) -> &'static str {
        "rpc_tls_script"
    }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        let server = ctx.ips_in_group("server").into_iter().next();
        let impostor = ctx.ips_in_group("impostor").into_iter().next();
        let pki = Pki::of_topology(ctx.state(), ctx.random(), server.clone(), impostor)?;
        while !ctx.chaos_shutdown().is_cancelled() {
            pause(ctx, ctx.random().random_range(100..600)).await?;
            match ctx.random().random_range(0..12u8) {
                0..=3 => pki.advance(ctx.random().random_range(1..120)),
                4 => {
                    // Past a leaf's whole lifetime: every leaf issued so far
                    // expires until the next issue.
                    pki.advance(LEAF_LIFETIME + ctx.random().random_range(1..600));
                    assert_reachable!("rpc tls UTC jumped past the leaves' lifetime");
                }
                5 | 6 => {
                    let issued = server_leaf(&pki, ctx)?;
                    if issued.flaw.is_some() {
                        assert_reachable!("rpc tls server leaf issued with a flaw");
                    }
                    rotate_server(&pki, issued)?;
                    assert_reachable!("rpc tls server certificate rotated");
                }
                7 | 8 => {
                    let issued = member_leaf(&pki, ctx)?;
                    if issued.flaw.is_some() {
                        assert_reachable!("rpc tls member leaf issued with a flaw");
                    }
                    rotate_member(&pki, issued)?;
                    assert_reachable!("rpc tls member certificate rotated");
                }
                9 => {
                    set_roots_swapped(&pki, true)?;
                    assert_reachable!("rpc tls server client roots moved away");
                    pause(ctx, ctx.random().random_range(200..1500)).await?;
                    set_roots_swapped(&pki, false)?;
                }
                _ => {
                    if let Some(server) = &server {
                        ctx.crash(server)?;
                        assert_reachable!("rpc tls server crashed");
                        pause(ctx, ctx.random().random_range(50..800)).await?;
                        ctx.restart(server)?;
                    }
                }
            }
        }
        // The operator's fix: clean certificates valid now, the right roots.
        set_roots_swapped(&pki, false)?;
        let issued = clean(|| server_leaf(&pki, ctx))?;
        rotate_server(&pki, issued)?;
        let issued = clean(|| member_leaf(&pki, ctx))?;
        rotate_member(&pki, issued)?;
        ctx.state().publish(SCRIPT_DONE_KEY, true);
        Ok(())
    }
}
