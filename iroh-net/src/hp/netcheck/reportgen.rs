//! The reportgen actor is responsible for generating a single netcheck report.
//!
//! It is implemented as an actor with [`Client`] as handle.
//!
//! The actor starts generating the report as soon as it is created, it does not receive any
//! messages from the client.  It follows roughly these steps:
//!
//! - Determines host IPv6 support.
//! - Creates hairpin actor.
//! - Creates portmapper future.
//! - Creates captive portal detection future.
//! - Creates Probe Set futures.
//!   - These send messages to the reportgen actor.
//! - Loops driving the futures and handling actor messages:
//!   - Disables futures as they are completed or aborted.
//!   - Stop if there are no outstanding tasks/futures, or on timeout.
//! - Sends the completed report to the netcheck actor.

use std::collections::BTreeMap;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use futures::stream::FuturesUnordered;
use futures::{FutureExt, StreamExt};
use iroh_metrics::inc;
use iroh_metrics::netcheck::Metrics as NetcheckMetrics;
use rand::seq::IteratorRandom;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{self, Instant};
use tracing::{debug, debug_span, error, info, instrument, trace, warn, Instrument};

use crate::hp::derp::{DerpMap, DerpNode, DerpRegion};
use crate::hp::netcheck::probe::{Probe, ProbePlan, ProbeProto};
use crate::hp::netcheck::{self, get_derp_addr, Report};
use crate::hp::ping::Pinger;
use crate::hp::{portmapper, stun};
use crate::net::interfaces;
use crate::util::{CancelOnDrop, MaybeFuture};

mod hairpin;

/// Fake DNS TLD used in tests for an invalid hostname.
const DOT_INVALID: &str = ".invalid";

/// The maximum amount of time netcheck will spend gathering a single report.
const OVERALL_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The maximum amount of time netcheck will spend probing with STUN packets without getting a
/// reply before switching to HTTP probing, on the assumption that outbound UDP is blocked.
const STUN_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// The maximum amount of time netcheck will spend probing with ICMP packets.
const ICMP_PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// How long to await for a captive-portal result, chosen semi-arbitrarily.
const CAPTIVE_PORTAL_DELAY: Duration = Duration::from_millis(200);

/// Timeout for captive portal checks, must be lower than OVERALL_PROBE_TIMEOUT
const CAPTIVE_PORTAL_TIMEOUT: Duration = Duration::from_secs(2);

const ENOUGH_REGIONS: usize = 3;

/// Holds the state for a single invocation of [`netcheck::Client::get_report`].
///
/// Dropping this will cancel the actor and stop the report generation.
#[derive(Debug)]
pub(super) struct Client {
    // Addr is currently only used by child actors, so not yet exposed here.
    _drop_guard: CancelOnDrop,
}

impl Client {
    /// Creates a new actor generating a single report.
    ///
    /// The actor starts running immediately and only generates a single report, after which
    /// it shuts down.  Dropping this handle will abort the actor.
    pub(super) fn new(
        netcheck: netcheck::Addr,
        last_report: Option<Arc<Report>>,
        port_mapper: Option<portmapper::Client>,
        skip_external_network: bool,
        derp_map: DerpMap,
        stun_sock4: Option<Arc<UdpSocket>>,
        stun_sock6: Option<Arc<UdpSocket>>,
    ) -> Self {
        let (msg_tx, msg_rx) = mpsc::channel(32);
        let addr = Addr {
            sender: msg_tx.clone(),
        };
        let incremental = last_report.is_some();
        let mut actor = Actor {
            msg_tx,
            msg_rx,
            netcheck: netcheck.clone(),
            last_report,
            port_mapper,
            skip_external_network,
            incremental,
            derp_map,
            stun_sock4,
            stun_sock6,
            report: Report::default(),
            hairpin_actor: hairpin::Client::new(netcheck, addr),
            outstanding_tasks: OutstandingTasks::default(),
        };
        let task = tokio::spawn(async move { actor.run().await });
        Self {
            _drop_guard: CancelOnDrop::new("reportgen actor", task.abort_handle()),
        }
    }
}

/// The address of the reportstate [`Actor`].
///
/// Unlike the [`Client`] struct itself this is the raw channel to send message over.
/// Keeping this alive will not keep the actor alive, which makes this handy to pass to
/// internal tasks.
#[derive(Debug, Clone)]
pub(super) struct Addr {
    sender: mpsc::Sender<Message>,
}

impl Addr {
    /// Blocking send to the actor, to be used from a non-actor future.
    async fn send(&self, msg: Message) -> Result<(), mpsc::error::SendError<Message>> {
        self.sender.send(msg).await.map_err(|err| {
            error!("reportstate actor lost");
            err
        })
    }
}

/// Messages to send to the reportstate [`Actor`].
#[derive(Debug)]
enum Message {
    /// Set the hairpinning availability in the report.
    HairpinResult(bool),
    /// Check whether executing a probe would still help.
    // TODO: Ideally we remove the need for this message and the logic is inverted: once we
    // get a probe result we cancel all probes that are no longer needed.  But for now it's
    // this way around to ease conversion.
    ProbeWouldHelp(Probe, Arc<DerpNode>, oneshot::Sender<bool>),
    /// Abort all remaining probes.
    AbortProbes,
}

/// The reportstate actor.
///
/// This actor starts, generates a single report and exits.
#[derive(Debug)]
struct Actor {
    /// The sender of the message channel, so we can give out [`Addr`].
    msg_tx: mpsc::Sender<Message>,
    /// The receiver of the message channel.
    msg_rx: mpsc::Receiver<Message>,
    /// The address of the netcheck actor.
    netcheck: super::Addr,

    // Provided state
    /// The previous report, if it exists.
    last_report: Option<Arc<Report>>,
    /// The portmapper client, if there is one.
    port_mapper: Option<portmapper::Client>,
    skip_external_network: bool,
    /// The DERP configuration.
    derp_map: DerpMap,
    /// Socket to send IPv4 STUN requests from.
    stun_sock4: Option<Arc<UdpSocket>>,
    /// Socket so send IPv6 STUN requests from.
    stun_sock6: Option<Arc<UdpSocket>>,

    // Internal state.
    /// Whether we're doing an incremental report.
    incremental: bool,
    /// The report being built.
    report: Report,
    /// The hairping actor.
    hairpin_actor: hairpin::Client,
    /// Which tasks the [`Actor`] is still waiting on.
    ///
    /// This is essentially the summary of all the work the [`Actor`] is doing.
    outstanding_tasks: OutstandingTasks,
}

impl Actor {
    fn addr(&self) -> Addr {
        Addr {
            sender: self.msg_tx.clone(),
        }
    }

    #[instrument(name = "reportgen.actor", skip_all)]
    async fn run(&mut self) {
        match self.run_inner().await {
            Ok(_) => debug!("reportgen actor finished"),
            Err(err) => {
                error!("reportgen actor failed: {err:#}");
                self.netcheck
                    .send(netcheck::Message::ReportAborted)
                    .await
                    .ok();
            }
        }
    }

    /// Runs the main reportgen actor logic.
    ///
    /// This actor runs by:
    ///
    /// - Creates a hairpin actor.
    /// - Creates a captive portal future.
    /// - Creates ProbeSet futures in a group of futures.
    /// - Runs a main loop:
    ///   - Drives all the above futures.
    ///   - Receives actor messages (sent by those futures).
    ///   - Updates the report, cancels unneeded futures.
    /// - Sends the report to the netcheck actor.
    async fn run_inner(&mut self) -> Result<()> {
        debug!(
            port_mapper = %self.port_mapper.is_some(),
            skip_external_network=%self.skip_external_network,
            "reportstate actor starting",
        );

        self.report.os_has_ipv6 = super::os_has_ipv6().await;

        let mut port_mapping = self.prepare_portmapper_task();
        let mut captive_task = self.prepare_captive_portal_task();
        let mut probes = self.prepare_probes_task().await?;

        let total_timer = tokio::time::sleep(OVERALL_PROBE_TIMEOUT);
        tokio::pin!(total_timer);
        let probe_timer = tokio::time::sleep(STUN_PROBE_TIMEOUT);
        tokio::pin!(probe_timer);

        loop {
            trace!(awaiting = ?self.outstanding_tasks, "tick; awaiting tasks");
            if self.outstanding_tasks.all_done() {
                debug!("all tasks done");
                break;
            }
            tokio::select! {
                _ = &mut total_timer => {
                    bail!("report timed out");
                }

                _ = &mut probe_timer => {
                    debug!("probes timed out");
                    self.handle_abort_probes();
                }

                // Drive the portmapper.
                pm = &mut port_mapping, if self.outstanding_tasks.port_mapper => {
                    self.report.portmap_probe = pm;
                    port_mapping.inner = None;
                    self.outstanding_tasks.port_mapper = false;
                    trace!("portmapper future done");
                }

                // Drive the probes.
                set_result = probes.next(), if self.outstanding_tasks.probes => {
                    match set_result {
                        Some(Ok(report)) => self.handle_probe_report(report),
                        Some(Err(_)) => (),
                        None => self.handle_abort_probes(),
                    }
                }

                // Drive the captive task.
                found = &mut captive_task, if self.outstanding_tasks.captive_task => {
                    self.report.captive_portal = found;
                    captive_task.inner = None;
                    self.outstanding_tasks.captive_task = false;
                    trace!("captive portal task future done");
                }

                // Handle actor messages.
                msg = self.msg_rx.recv() => {
                    match msg {
                        Some(msg) => self.handle_message(msg),
                        None => bail!("msg_rx closed, reportgen client must be dropped"),
                    }
                }
            }
        }

        if !probes.is_empty() {
            debug!(
                "aborting {} probe sets, already have enough reports",
                probes.len()
            );
            drop(probes);
        }

        debug!("Sending report to netcheck actor");
        self.netcheck
            .send(netcheck::Message::ReportReady {
                report: Box::new(self.report.clone()),
                derp_map: self.derp_map.clone(),
            })
            .await?;

        Ok(())
    }

    /// Handles an actor message.
    ///
    /// Returns `true` if all the probes need to be aborted.
    fn handle_message(&mut self, msg: Message) {
        trace!(?msg, "handling message");
        match msg {
            Message::HairpinResult(works) => {
                self.report.hair_pinning = Some(works);
                self.outstanding_tasks.hairpin = false;
            }
            Message::ProbeWouldHelp(probe, derp_node, response_tx) => {
                let res = self.probe_would_help(probe, derp_node);
                if response_tx.send(res).is_err() {
                    debug!("probe dropped before ProbeWouldHelp response sent");
                }
            }
            Message::AbortProbes => {
                self.handle_abort_probes();
            }
        }
    }

    fn handle_probe_report(&mut self, probe_report: ProbeReport) {
        debug!("finished probe: {:?}", probe_report);
        match probe_report.probe {
            Probe::Https { region, .. } => {
                if let Some(delay) = probe_report.delay {
                    self.report
                        .region_latency
                        .update_region(region.region_id, delay);
                }
            }
            Probe::Ipv4 { node, .. } | Probe::Ipv6 { node, .. } => {
                if let Some(delay) = probe_report.delay {
                    self.add_stun_addr_latency(node, probe_report.addr, delay);
                    if let Some(ref addr) = self.report.global_v4 {
                        // Only needed for the first IPv4 address discovered, but hairpin
                        // actor ignores subsequent messages.
                        self.hairpin_actor.start_check(*addr);
                        self.outstanding_tasks.hairpin = true;
                    }
                }
            }
        }
        self.report.ipv4_can_send = probe_report.ipv4_can_send;
        self.report.ipv6_can_send = probe_report.ipv6_can_send;
        self.report.icmpv4 = probe_report.icmpv4;
    }

    /// Whether running this probe would still improve our report.
    fn probe_would_help(&mut self, probe: Probe, derp_node: Arc<DerpNode>) -> bool {
        // If the probe is for a region we don't yet know about, that would help.
        if self
            .report
            .region_latency
            .get(derp_node.region_id)
            .is_none()
        {
            return true;
        }

        // If the probe is for IPv6 and we don't yet have an IPv6 report, that would help.
        if probe.proto() == ProbeProto::Ipv6 && self.report.region_v6_latency.is_empty() {
            return true;
        }

        // For IPv4, we need at least two IPv4 results overall to
        // determine whether we're behind a NAT that shows us as
        // different source IPs and/or ports depending on who we're
        // talking to. If we don't yet have two results yet
        // (`mapping_varies_by_dest_ip` is blank), then another IPv4 probe
        // would be good.
        if probe.proto() == ProbeProto::Ipv4 && self.report.mapping_varies_by_dest_ip.is_none() {
            return true;
        }

        // Otherwise not interesting.
        false
    }

    /// Updates the report to note that node's latency and discovered address from STUN.
    ///
    /// Since this is only called for STUN probes, in other words [`Probe::Ipv4`] and
    /// [`Probe::Ipv6`], *ipp` is always `Some`.
    fn add_stun_addr_latency(
        &mut self,
        derp_node: String,
        ipp: Option<SocketAddr>,
        latency: Duration,
    ) {
        let Some(node) = self.derp_map.find_by_name(&derp_node) else {
            warn!("derp node missing from derp map");
            return;
        };

        debug!(node = %node.name, ?latency, "add udp node latency");
        self.report.udp = true;

        self.report
            .region_latency
            .update_region(node.region_id, latency);

        // Once we've heard from enough regions (3), start a timer to
        // give up on the other ones. The timer's duration is a
        // function of whether this is our initial full probe or an
        // incremental one. For incremental ones, wait for the
        // duration of the slowest region. For initial ones, double that.
        if self.report.region_latency.len() == ENOUGH_REGIONS {
            let mut timeout = self.report.region_latency.max_latency();
            if !self.incremental {
                timeout *= 2;
            }
            let reportcheck = self.addr();
            tokio::spawn(async move {
                time::sleep(timeout).await;
                reportcheck.send(Message::AbortProbes).await.ok();
            });
        }

        if let Some(ipp) = ipp {
            match ipp {
                SocketAddr::V4(_) => {
                    self.report
                        .region_v4_latency
                        .update_region(node.region_id, latency);
                    self.report.ipv4 = true;
                    if self.report.global_v4.is_none() {
                        self.report.global_v4 = Some(ipp);
                    } else if self.report.global_v4 != Some(ipp) {
                        self.report.mapping_varies_by_dest_ip = Some(true);
                    } else if self.report.mapping_varies_by_dest_ip.is_none() {
                        self.report.mapping_varies_by_dest_ip = Some(false);
                    }
                }
                SocketAddr::V6(_) => {
                    self.report
                        .region_v6_latency
                        .update_region(node.region_id, latency);
                    self.report.ipv6 = true;
                    self.report.global_v6 = Some(ipp);
                    // TODO: track MappingVariesByDestIP for IPv6 too? Would be sad if so, but
                    // who knows.
                }
            }
        }
    }

    /// Stops further probes.
    ///
    /// This makes sure that no further probes are run and also cancels the captive portal
    /// task if there were successful probes.  Be sure to only handle this after all the
    /// required [`ProbeReport`]s have been processed.
    fn handle_abort_probes(&mut self) {
        self.outstanding_tasks.probes = false;
        if self.report.udp {
            self.outstanding_tasks.captive_task = false;
        }
    }

    /// Creates the future which will perform the portmapper task.
    ///
    /// The returned future will run the portmapper, if enabled, resolving to it's result.
    fn prepare_portmapper_task(
        &mut self,
    ) -> MaybeFuture<Pin<Box<impl Future<Output = Option<portmapper::ProbeOutput>>>>> {
        let mut port_mapping = MaybeFuture::default();
        if !self.skip_external_network {
            if let Some(port_mapper) = self.port_mapper.clone() {
                port_mapping.inner = Some(Box::pin(async move {
                    match port_mapper.probe().await {
                        Ok(Ok(res)) => Some(res),
                        Ok(Err(err)) => {
                            warn!("skipping port mapping: {err:?}");
                            None
                        }
                        Err(recv_err) => {
                            warn!("skipping port mapping: {recv_err:?}");
                            None
                        }
                    }
                }));
                self.outstanding_tasks.port_mapper = true;
            }
        }
        port_mapping
    }

    /// Creates the future which will perform the captive portal check.
    fn prepare_captive_portal_task(
        &mut self,
    ) -> MaybeFuture<Pin<Box<impl Future<Output = Option<bool>>>>> {
        // If we're doing a full probe, also check for a captive portal. We
        // delay by a bit to wait for UDP STUN to finish, to avoid the probe if
        // it's unnecessary.
        if !self.incremental {
            // Even if we're doing a non-incremental update, we may want to try our
            // preferred DERP region for captive portal detection.
            let preferred_derp = self.last_report.as_ref().map(|l| l.preferred_derp);

            let dm = self.derp_map.clone();
            self.outstanding_tasks.captive_task = true;
            MaybeFuture {
                inner: Some(Box::pin(async move {
                    tokio::time::sleep(CAPTIVE_PORTAL_DELAY).await;
                    let captive_portal_check = tokio::time::timeout(
                        CAPTIVE_PORTAL_TIMEOUT,
                        check_captive_portal(&dm, preferred_derp)
                            .instrument(debug_span!("captive-portal")),
                    );
                    match captive_portal_check.await {
                        Ok(Ok(found)) => Some(found),
                        Ok(Err(err)) => {
                            info!("check_captive_portal error: {:?}", err);
                            None
                        }
                        Err(_) => {
                            info!("check_captive_portal timed out");
                            None
                        }
                    }
                })),
            }
        } else {
            self.outstanding_tasks.captive_task = false;
            MaybeFuture::default()
        }
    }

    /// Prepares the future which will run all the probes as per generated ProbePlan.
    async fn prepare_probes_task(
        &mut self,
    ) -> Result<FuturesUnordered<Pin<Box<impl Future<Output = Result<ProbeReport>>>>>> {
        let if_state = interfaces::State::new().await;
        let plan = ProbePlan::new(&self.derp_map, &if_state, self.last_report.as_deref());
        trace!(%plan, "probe plan");

        let pinger = if plan.has_https_probes() {
            match Pinger::new().await {
                Ok(pinger) => Some(pinger),
                Err(err) => {
                    debug!("failed to create pinger: {err:#}");
                    None
                }
            }
        } else {
            None
        };

        // A collection of futures running probe sets.
        let probes = FuturesUnordered::default();
        let mut derp_nodes_cache: BTreeMap<String, Arc<DerpNode>> = BTreeMap::new();

        for probe_set in plan.values() {
            let mut set = FuturesUnordered::default();
            for probe in probe_set {
                let reportstate = self.addr();
                let stun_sock4 = self.stun_sock4.clone();
                let stun_sock6 = self.stun_sock6.clone();
                let derp_node = match derp_nodes_cache.get(probe.node()) {
                    Some(node) => node.clone(),
                    None => {
                        let name = probe.node().to_string();
                        let node = self
                            .derp_map
                            .find_by_name(&name)
                            .with_context(|| format!("missing named derp node {}", probe.node()))?;
                        let node = Arc::new(node.clone());
                        derp_nodes_cache.insert(name, node.clone());
                        node
                    }
                };
                let derp_node = derp_node.clone();
                let probe = probe.clone();
                let netcheck = self.netcheck.clone();
                let pinger = pinger.clone();

                set.push(Box::pin(async move {
                    run_probe(
                        reportstate,
                        stun_sock4,
                        stun_sock6,
                        derp_node,
                        probe,
                        netcheck,
                        pinger,
                    )
                    .await
                }));
            }

            // Add the probe set to all futures of probe sets.  Handle aborting a probe set
            // if needed, only normal errors means the set continues.
            probes.push(Box::pin(async move {
                // Hack because ProbeSet is not it's own type yet.
                let mut probe_proto = None;
                while let Some(res) = set.next().await {
                    match res {
                        Ok(report) => return Ok(report),
                        Err(ProbeError::Error(err, probe)) => {
                            probe_proto = Some(probe.proto());
                            warn!(?probe, "probe failed: {:#}", err);
                            continue;
                        }
                        Err(ProbeError::AbortSet(err, probe)) => {
                            debug!(?probe, "probe set aborted: {:#}", err);
                            return Err(err);
                        }
                    }
                }
                warn!(?probe_proto, "no successfull probes in ProbeSet");
                Err(anyhow!("All probes in ProbeSet failed"))
            }));
        }
        self.outstanding_tasks.probes = true;

        Ok(probes)
    }
}

/// Tasks on which the reportgen [`Actor`] is still waiting.
///
/// There is no particular progression, e.g. hairpin starts `false`, moves to `true` when a
/// check is started and then becomes `false` again once it is finished.
#[derive(Debug, Default)]
struct OutstandingTasks {
    probes: bool,
    port_mapper: bool,
    captive_task: bool,
    hairpin: bool,
}

impl OutstandingTasks {
    fn all_done(&self) -> bool {
        !(self.probes || self.port_mapper || self.captive_task || self.hairpin)
    }
}

/// The success result of [`run_probe`].
#[derive(Debug)]
struct ProbeReport {
    /// Whether we can send IPv4 UDP packets.
    ipv4_can_send: bool,
    /// Whether we can send IPv6 UDP packets.
    ipv6_can_send: bool,
    /// Whether we can send ICMP packets.
    icmpv4: bool,
    /// The latency to the derp node.
    delay: Option<Duration>,
    /// The probe that generated this report.
    probe: Probe,
    /// The discovered public address.
    addr: Option<SocketAddr>,
}

impl ProbeReport {
    fn new(probe: Probe) -> Self {
        ProbeReport {
            probe,
            ipv4_can_send: false,
            ipv6_can_send: false,
            icmpv4: false,
            delay: None,
            addr: None,
        }
    }
}

/// Errors for [`run_probe`].
///
/// The main purpose is to signal whether other probes in this probe set should still be
/// run.  Recall that a probe set is normally a set of identical probes with delays,
/// effectively creating retries, and the first successful probe of a probe set will cancel
/// the others in the set.  So this allows an unsuccessful probe to cancel the remainder of
/// the set or not.
#[derive(Debug)]
enum ProbeError {
    /// Abort the current set.
    AbortSet(anyhow::Error, Probe),
    /// Continue the other probes in the set.
    Error(anyhow::Error, Probe),
}

/// Executes a particular [`Probe`], including using a delayed start if needed.
///
/// If *stun_sock4* and *stun_sock6* are `None` the STUN probes are disabled.
#[allow(clippy::too_many_arguments)]
#[instrument(level = "debug", skip_all, fields(probe = %probe))]
async fn run_probe(
    reportstate: Addr,
    stun_sock4: Option<Arc<UdpSocket>>,
    stun_sock6: Option<Arc<UdpSocket>>,
    derp_node: Arc<DerpNode>,
    probe: Probe,
    netcheck: netcheck::Addr,
    pinger: Option<Pinger>,
) -> Result<ProbeReport, ProbeError> {
    if !probe.delay().is_zero() {
        debug!("delaying probe");
        tokio::time::sleep(probe.delay()).await;
    }
    debug!("starting probe");

    let (would_help_tx, would_help_rx) = oneshot::channel();
    reportstate
        .send(Message::ProbeWouldHelp(
            probe.clone(),
            derp_node.clone(),
            would_help_tx,
        ))
        .await
        .map_err(|err| ProbeError::AbortSet(err.into(), probe.clone()))?;
    if !would_help_rx.await.map_err(|_| {
        ProbeError::AbortSet(anyhow!("ReportCheck actor dropped sender"), probe.clone())
    })? {
        return Err(ProbeError::AbortSet(
            anyhow!("ReportCheck says probe set no longer useful"),
            probe,
        ));
    }

    let derp_addr = get_derp_addr(&derp_node, probe.proto())
        .await
        .context("no derp node addr")
        .map_err(|e| ProbeError::AbortSet(e, probe.clone()))?;
    let txid = stun::TransactionId::default();
    let req = stun::request(txid);

    let (stun_tx, stun_rx) = oneshot::channel();
    let (stun_ready_tx, stun_ready_rx) = oneshot::channel();
    netcheck
        .send(netcheck::Message::InFlightStun(
            netcheck::Inflight {
                txn: txid,
                start: Instant::now(),
                s: stun_tx,
            },
            stun_ready_tx,
        ))
        .await
        .map_err(|e| ProbeError::Error(e.into(), probe.clone()))?;
    stun_ready_rx
        .await
        .map_err(|e| ProbeError::Error(e.into(), probe.clone()))?;
    let mut result = ProbeReport::new(probe.clone());

    match probe {
        Probe::Ipv4 { .. } => {
            if let Some(ref sock) = stun_sock4 {
                let n = sock.send_to(&req, derp_addr).await;
                inc!(NetcheckMetrics, stun_packets_sent_ipv4);
                debug!(%derp_addr, send_res=?n, %txid, "sending probe Ipv4");
                // TODO:  || neterror.TreatAsLostUDP(err)
                if n.is_ok() && n.unwrap() == req.len() {
                    result.ipv4_can_send = true;

                    let (delay, addr) = stun_rx
                        .await
                        .map_err(|e| ProbeError::Error(e.into(), probe.clone()))?;
                    result.delay = Some(delay);
                    result.addr = Some(addr);
                }
            }
        }
        Probe::Ipv6 { .. } => {
            if let Some(ref pc6) = stun_sock6 {
                let n = pc6.send_to(&req, derp_addr).await;
                inc!(NetcheckMetrics, stun_packets_sent_ipv6);
                debug!(%derp_addr, snd_res=?n, %txid, "sending probe Ipv6");
                // TODO:  || neterror.TreatAsLostUDP(err)
                if n.is_ok() && n.unwrap() == req.len() {
                    result.ipv6_can_send = true;

                    let (delay, addr) = stun_rx
                        .await
                        .map_err(|e| ProbeError::Error(e.into(), probe.clone()))?;
                    result.delay = Some(delay);
                    result.addr = Some(addr);
                }
            }
        }
        Probe::Https { ref region, .. } => {
            debug!(icmp=%pinger.is_some(), "sending probe HTTPS");

            let res = if let Some(ref pinger) = pinger {
                tokio::join!(
                    time::timeout(
                        ICMP_PROBE_TIMEOUT,
                        measure_icmp_latency(region, pinger).map(Some)
                    ),
                    measure_https_latency(region)
                )
            } else {
                (Ok(None), measure_https_latency(region).await)
            };
            if let Ok(Some(icmp_res)) = res.0 {
                match icmp_res {
                    Ok(d) => {
                        result.delay = Some(d);
                        result.ipv4_can_send = true;
                        result.icmpv4 = true;
                    }
                    Err(err) => {
                        warn!("icmp latency measurement failed: {:?}", err);
                    }
                }
            }
            match res.1 {
                Ok((d, ip)) => {
                    result.delay = Some(d);
                    // We set these IPv4 and IPv6 but they're not really used
                    // and we don't necessarily set them both. If UDP is blocked
                    // and both IPv4 and IPv6 are available over TCP, it's basically
                    // random which fields end up getting set here.
                    // Since they're not needed, that's fine for now.
                    match ip {
                        IpAddr::V4(_) => result.ipv4_can_send = true,
                        IpAddr::V6(_) => result.ipv6_can_send = true,
                    }
                }
                Err(err) => {
                    warn!("https latency measurement failed: {:?}", err);
                }
            }
        }
    }

    trace!(probe = ?probe, "probe successfull");
    Ok(result)
}

/// Reports whether or not we think the system is behind a
/// captive portal, detected by making a request to a URL that we know should
/// return a "204 No Content" response and checking if that's what we get.
///
/// The boolean return is whether we think we have a captive portal.
async fn check_captive_portal(dm: &DerpMap, preferred_derp: Option<u16>) -> Result<bool> {
    // If we have a preferred DERP region with more than one node, try
    // that; otherwise, pick a random one not marked as "Avoid".
    let preferred_derp = if preferred_derp.is_none()
        || dm.regions.get(&preferred_derp.unwrap()).is_none()
        || (preferred_derp.is_some()
            && dm
                .regions
                .get(&preferred_derp.unwrap())
                .unwrap()
                .nodes
                .is_empty())
    {
        let mut rids = Vec::with_capacity(dm.regions.len());
        for (id, reg) in dm.regions.iter() {
            if reg.avoid || reg.nodes.is_empty() {
                continue;
            }
            rids.push(id);
        }

        if rids.is_empty() {
            return Ok(false);
        }

        let i = (0..rids.len())
            .choose(&mut rand::thread_rng())
            .unwrap_or_default();
        *rids[i]
    } else {
        preferred_derp.unwrap()
    };

    // Has a node, as we filtered out regions without nodes above.
    let node = &dm.regions.get(&preferred_derp).unwrap().nodes[0];

    if node
        .url
        .host_str()
        .map(|s| s.ends_with(&DOT_INVALID))
        .unwrap_or_default()
    {
        // Don't try to connect to invalid hostnames. This occurred in tests:
        // https://github.com/tailscale/tailscale/issues/6207
        // TODO(bradfitz,andrew-d): how to actually handle this nicely?
        return Ok(false);
    }

    let client = reqwest::ClientBuilder::new()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;

    // Note: the set of valid characters in a challenge and the total
    // length is limited; see is_challenge_char in bin/derper for more
    // details.

    let host_name = node.url.host_str().unwrap_or_default();
    let challenge = format!("ts_{}", host_name);
    let portal_url = format!("http://{}/generate_204", host_name);
    let res = client
        .request(reqwest::Method::GET, portal_url)
        .header("X-Tailscale-Challenge", &challenge)
        .send()
        .await?;

    let expected_response = format!("response {challenge}");
    let is_valid_response = res
        .headers()
        .get("X-Tailscale-Response")
        .map(|s| s.to_str().unwrap_or_default())
        == Some(&expected_response);

    info!(
        "check_captive_portal url={} status_code={} valid_response={}",
        res.url(),
        res.status(),
        is_valid_response,
    );
    let has_captive = res.status() != 204 || !is_valid_response;

    Ok(has_captive)
}

async fn measure_icmp_latency(reg: &DerpRegion, p: &Pinger) -> Result<Duration> {
    if reg.nodes.is_empty() {
        anyhow::bail!(
            "no nodes for region {} ({})",
            reg.region_id,
            reg.region_code
        );
    }

    // Try pinging the first node in the region
    let node = &reg.nodes[0];

    // Get the IPAddr by asking for the UDP address that we would use for
    // STUN and then using that IP.
    let node_addr = get_derp_addr(node, ProbeProto::Ipv4)
        .await
        .with_context(|| format!("no address for node {}", node.name))?;

    debug!(
        "ICMP ping start to {} with payload len {} - derp {} {}",
        node_addr,
        node.name.as_bytes().len(),
        node.name,
        reg.region_id
    );
    // Use the unique node.name field as the packet data to reduce the
    // likelihood that we get a mismatched echo response.
    let d = p.send(node_addr.ip(), node.name.as_bytes()).await?;
    debug!(
        "ICMP ping done {} with latency {}ms - derp {} {}",
        node_addr,
        d.as_millis(),
        node.name,
        reg.region_id
    );
    Ok(d)
}

async fn measure_https_latency(_reg: &DerpRegion) -> Result<(Duration, IpAddr)> {
    anyhow::bail!("not implemented");
    // TODO:
    // - needs derphttp::Client
    // - measurement hooks to measure server processing time

    // metricHTTPSend.Add(1)
    // let ctx, cancel := context.WithTimeout(httpstat.WithHTTPStat(ctx, &result), overallProbeTimeout);
    // let dc := derphttp.NewNetcheckClient(c.logf);
    // let tlsConn, tcpConn, node := dc.DialRegionTLS(ctx, reg)?;
    // if ta, ok := tlsConn.RemoteAddr().(*net.TCPAddr);
    // req, err := http.NewRequestWithContext(ctx, "GET", "https://"+node.HostName+"/derp/latency-check", nil);
    // resp, err := hc.Do(req);

    // // DERPs should give us a nominal status code, so anything else is probably
    // // an access denied by a MITM proxy (or at the very least a signal not to
    // // trust this latency check).
    // if resp.StatusCode > 299 {
    //     return 0, ip, fmt.Errorf("unexpected status code: %d (%s)", resp.StatusCode, resp.Status)
    // }
    // _, err = io.Copy(io.Discard, io.LimitReader(resp.Body, 8<<10));
    // result.End(c.timeNow())

    // // TODO: decide best timing heuristic here.
    // // Maybe the server should return the tcpinfo_rtt?
    // return result.ServerProcessing, ip, nil
}