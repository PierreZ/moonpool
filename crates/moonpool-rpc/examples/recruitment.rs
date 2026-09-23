//! Publishing and recruiting fresh interfaces across a restart, with the
//! manual API on real TCP.
//!
//! The application keeps its own identities apart from RPC identity:
//! [`ParticipantId`] (a durable participant), [`ConfigurationId`] (what it
//! was recruited for) and [`ServiceInterface`] (the fresh routing data a
//! boot publishes). RPC neither knows nor checks the first two.
//!
//! 1. Participant A boots, publishes interface I1 through its well-known
//!    directory; B learns I1 and calls it.
//! 2. A restarts at the **same address** and publishes I2. B's call through
//!    I1 is refused (`StaleIncarnation`) and never reaches I2; B learns I2
//!    from the directory, explicitly, and calls it.
//! 3. B recruits D and E for configuration 7: each creates a fresh
//!    interface and returns it. B forwards both to C, a third participant,
//!    which calls them and checks they serve configuration 7.
//!
//! Run with `cargo run -p moonpool-rpc --example recruitment`.

use std::net::SocketAddr;

use moonpool_core::TokioProviders;
use moonpool_rpc::{
    AccessClass, BootstrapAddress, IncomingRequest, InterfaceId, InterfaceMethod, InterfaceRef,
    MethodId, RpcConfig, RpcDriver, RpcHandle, RpcInterface, RpcMethod, SchemaVersion,
    ServiceGroup, WellKnownId, WellKnownRef,
};

/// A durable participant: the same across its restarts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ParticipantId(pub u64);

/// A configuration a participant is recruited into.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ConfigurationId(pub u64);

/// The role interface a participant serves.
pub struct Role;
impl RpcInterface for Role {
    const INTERFACE: InterfaceId = InterfaceId::new(0x726f_6c65);
    const VERSION: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "role";
}

/// A call to a role instance.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Probe {
    /// Caller-chosen text, echoed back.
    #[prost(string, tag = "1")]
    pub text: String,
}

/// Which instance answered: its participant, configuration and text.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Answer {
    /// The answering participant.
    #[prost(uint64, tag = "1")]
    pub participant: u64,
    /// The configuration the instance serves (0: none).
    #[prost(uint64, tag = "2")]
    pub configuration: u64,
    /// The probe's text.
    #[prost(string, tag = "3")]
    pub text: String,
}

/// The one method of a role instance.
pub struct Status;
impl RpcMethod for Status {
    type Request = Probe;
    type Reply = Answer;
    const METHOD: MethodId = MethodId::new(1);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "role.status";
}
impl InterfaceMethod<Role> for Status {}

/// What a boot publishes: routing data plus the application's identities.
#[derive(Clone, PartialEq, prost::Message)]
pub struct ServiceInterface {
    /// The participant that published it.
    #[prost(uint64, tag = "1")]
    pub participant: u64,
    /// The configuration it serves (0: none).
    #[prost(uint64, tag = "2")]
    pub configuration: u64,
    /// The fresh interface of this boot.
    #[prost(message, optional, tag = "3")]
    pub role: Option<InterfaceRef<Role>>,
}

/// Recruit a participant into a configuration.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Recruit {
    /// The configuration.
    #[prost(uint64, tag = "1")]
    pub configuration: u64,
}

/// Hand a third participant the recruited interfaces.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Adopt {
    /// The configuration they were recruited for.
    #[prost(uint64, tag = "1")]
    pub configuration: u64,
    /// The recruited interfaces.
    #[prost(message, repeated, tag = "2")]
    pub members: Vec<ServiceInterface>,
}

/// What the third participant observed.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Adopted {
    /// Answers from each member, in order.
    #[prost(message, repeated, tag = "1")]
    pub answers: Vec<Answer>,
}

macro_rules! well_known {
    ($(#[$doc:meta])* $name:ident, $request:ty, $reply:ty, $id:expr) => {
        $(#[$doc])*
        pub struct $name;
        impl RpcMethod for $name {
            type Request = $request;
            type Reply = $reply;
            const METHOD: MethodId = MethodId::new($id);
            const SCHEMA: SchemaVersion = SchemaVersion::new(1);
            const NAME: &'static str = stringify!($name);
        }
    };
}

well_known!(
    /// The participant's directory: its current interface.
    Directory, Probe, ServiceInterface, 0x10
);
well_known!(
    /// Recruit into a configuration: a fresh interface for it.
    Recruiter, Recruit, ServiceInterface, 0x11
);
well_known!(
    /// Adopt recruited members and call each of them.
    Adopter, Adopt, Adopted, 0x12
);

const DIRECTORY: WellKnownId = WellKnownId::new(1);
const RECRUITER: WellKnownId = WellKnownId::new(2);
const ADOPTER: WellKnownId = WellKnownId::new(3);

type Rpc = RpcHandle<TokioProviders>;

async fn runtime(address: &str) -> (Rpc, tokio::task::JoinHandle<()>) {
    let (driver, rpc) = RpcDriver::listen(TokioProviders::new(), address, RpcConfig::default())
        .await
        .expect("listen");
    (
        rpc,
        tokio::spawn(async move {
            let _ = driver.run().await;
        }),
    )
}

/// Serve one role instance until its group is dropped.
fn role_instance(
    rpc: &Rpc,
    participant: ParticipantId,
    configuration: ConfigurationId,
) -> (ServiceInterface, ServiceGroup<Role>) {
    let group = rpc
        .register_group::<Role>(AccessClass::Public)
        .expect("register");
    let mut status = group.serve::<Status>().expect("serve");
    tokio::spawn(async move {
        while let Some(IncomingRequest { request, reply }) = status.recv().await {
            let _ = reply.send(&Answer {
                participant: participant.0,
                configuration: configuration.0,
                text: request.text,
            });
        }
    });
    let published = ServiceInterface {
        participant: participant.0,
        configuration: configuration.0,
        role: Some(group.interface_ref()),
    };
    (published, group)
}

/// One boot of a participant: a role instance, a directory publishing it,
/// and a recruiter that creates more instances on demand.
async fn boot(address: &str, participant: ParticipantId) -> (Rpc, tokio::task::JoinHandle<()>) {
    let (rpc, driver) = runtime(address).await;
    let (published, group) = role_instance(&rpc, participant, ConfigurationId(0));
    let (_, mut directory) = rpc
        .register_well_known::<Directory>(DIRECTORY, AccessClass::Public)
        .expect("directory");
    tokio::spawn(async move {
        let _group = group;
        while let Some(IncomingRequest { reply, .. }) = directory.recv().await {
            let _ = reply.send(&published);
        }
    });
    let (_, mut recruiter) = rpc
        .register_well_known::<Recruiter>(RECRUITER, AccessClass::Public)
        .expect("recruiter");
    let recruiting = rpc.clone();
    tokio::spawn(async move {
        let mut groups = Vec::new();
        while let Some(IncomingRequest { request, reply }) = recruiter.recv().await {
            let (published, group) = role_instance(
                &recruiting,
                participant,
                ConfigurationId(request.configuration),
            );
            groups.push(group);
            let _ = reply.send(&published);
        }
    });
    (rpc, driver)
}

fn well_known<M: RpcMethod>(
    rpc: &Rpc,
    address: SocketAddr,
    id: WellKnownId,
) -> moonpool_rpc::ServiceClient<TokioProviders, M> {
    WellKnownRef::<M>::new(BootstrapAddress::Resolved(address), id, AccessClass::Public)
        .at(address)
        .bind(rpc)
}

async fn call(
    rpc: &Rpc,
    interface: &ServiceInterface,
    text: &str,
) -> Result<Answer, moonpool_rpc::RpcError> {
    let role = interface.role.as_ref().expect("an interface");
    role.bind(rpc)?
        .method::<Status>()
        .try_get_reply(&Probe { text: text.into() })
        .await
}

#[tokio::main]
async fn main() {
    let a = ParticipantId(1);
    let (a_rpc, a_driver) = boot("127.0.0.1:0", a).await;
    let a_address = a_rpc.address().expect("listening");
    let (b_rpc, _b_driver) = runtime("127.0.0.1:0").await;

    // 1. B learns I1 through A's directory and calls it.
    let directory = well_known::<Directory>(&b_rpc, a_address, DIRECTORY);
    let i1 = directory
        .try_get_reply(&Probe::default())
        .await
        .expect("I1");
    let answer = call(&b_rpc, &i1, "hello").await.expect("I1 answers");
    assert_eq!(answer.participant, a.0);
    println!("B called I1 at {a_address}");

    // 2. A crashes and restarts at the same address with I2.
    a_driver.abort();
    let _ = a_driver.await;
    let (_a_rpc, _a_driver) = boot(&a_address.to_string(), a).await;
    let stale = call(&b_rpc, &i1, "again").await.expect_err("I1 is dead");
    println!("I1 refused: {stale}");
    assert_eq!(*stale.reason(), moonpool_rpc::ErrorReason::StaleIncarnation);
    let i2 = directory
        .try_get_reply(&Probe::default())
        .await
        .expect("I2");
    assert_ne!(i1, i2, "a fresh interface");
    let answer = call(&b_rpc, &i2, "hello again").await.expect("I2 answers");
    assert_eq!(answer.participant, a.0, "same durable participant");
    println!("B learned I2 explicitly and called it");

    // 3. Recruit D and E for configuration 7; forward them to C.
    let configuration = ConfigurationId(7);
    let mut members = Vec::new();
    let mut recruited = Vec::new();
    for id in [ParticipantId(4), ParticipantId(5)] {
        let (rpc, driver) = boot("127.0.0.1:0", id).await;
        let address = rpc.address().expect("listening");
        recruited.push(driver);
        let recruit_client = well_known::<Recruiter>(&b_rpc, address, RECRUITER);
        let member = recruit_client
            .try_get_reply(&Recruit {
                configuration: configuration.0,
            })
            .await
            .expect("recruited");
        members.push(member);
    }
    let (c_rpc, _c_driver) = runtime("127.0.0.1:0").await;
    let (_, mut adopter) = c_rpc
        .register_well_known::<Adopter>(ADOPTER, AccessClass::Public)
        .expect("adopter");
    let c_calls = c_rpc.clone();
    tokio::spawn(async move {
        while let Some(IncomingRequest { request, reply }) = adopter.recv().await {
            let mut answers = Vec::new();
            for member in &request.members {
                answers.push(
                    call(&c_calls, member, "adopt")
                        .await
                        .expect("member answers"),
                );
            }
            let _ = reply.send(&Adopted { answers });
        }
    });
    let c_address = c_rpc.address().expect("listening");
    let observed = well_known::<Adopter>(&b_rpc, c_address, ADOPTER)
        .try_get_reply(&Adopt {
            configuration: configuration.0,
            members,
        })
        .await
        .expect("C adopted");
    let participants: Vec<u64> = observed.answers.iter().map(|a| a.participant).collect();
    assert_eq!(participants, [4, 5]);
    assert!(
        observed
            .answers
            .iter()
            .all(|answer| answer.configuration == configuration.0)
    );
    println!("C called the recruited D and E for configuration {configuration:?}");
}
