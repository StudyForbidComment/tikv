// Copyright 2017 TiKV Project Authors. Licensed under Apache-2.0.

use std::borrow::Cow;
use std::cmp::{Ord, Ordering as CmpOrdering};
use std::collections::VecDeque;
use std::fmt::{self, Debug, Formatter};
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
#[cfg(test)]
use std::sync::mpsc::Sender;
use std::sync::mpsc::SyncSender;
use std::sync::Arc;
use std::{cmp, usize};

use batch_system::{BasicMailbox, BatchRouter, BatchSystem, Fsm, HandlerBuilder, PollHandler};
use crossbeam::channel::{TryRecvError, TrySendError};
use engine_rocks::{RocksEngine, RocksSnapshot};
use engine_traits::{KvEngine, MiscExt, Peekable, Snapshot, WriteBatch, WriteBatchVecExt};
use engine_traits::{ALL_CFS, CF_DEFAULT, CF_LOCK, CF_RAFT, CF_WRITE};
use kvproto::import_sstpb::SstMeta;
use kvproto::metapb::{Peer as PeerMeta, Region, RegionEpoch};
use kvproto::raft_cmdpb::{
    AdminCmdType, AdminRequest, AdminResponse, ChangePeerRequest, CmdType, CommitMergeRequest,
    RaftCmdRequest, RaftCmdResponse, Request, Response,
};
use kvproto::raft_serverpb::{
    MergeState, PeerState, RaftApplyState, RaftTruncatedState, RegionLocalState,
};
use raft::eraftpb::{ConfChange, ConfChangeType, Entry, EntryType, Snapshot as RaftSnapshot};
use uuid::Builder as UuidBuilder;

use crate::coprocessor::{Cmd, CoprocessorHost};
use crate::store::fsm::{RaftPollerBuilder, RaftRouter};
use crate::store::metrics::*;
use crate::store::msg::{Callback, PeerMsg, ReadResponse, SignificantMsg, SnapshotCallback};
use crate::store::peer::Peer;
use crate::store::peer_storage::{self, write_initial_apply_state, write_peer_state};
use crate::store::util::KeysInfoFormatter;
use crate::store::util::{check_region_epoch, compare_region_epoch};
use crate::store::{cmd_resp, util, Config, RegionSnapshot};
use crate::{Error, Result};
use sst_importer::SSTImporter;
use tikv_util::config::{Tracker, VersionTrack};
use tikv_util::escape;
use tikv_util::mpsc::{loose_bounded, LooseBoundedSender, Receiver};
use tikv_util::time::{duration_to_sec, Instant};
use tikv_util::worker::Scheduler;
use tikv_util::Either;
use tikv_util::MustConsumeVec;

use super::metrics::*;

use super::super::RegionTask;

const DEFAULT_APPLY_WB_SIZE: usize = 4 * 1024;
const WRITE_BATCH_LIMIT: usize = 16;
const APPLY_WB_SHRINK_SIZE: usize = 1024 * 1024;
const SHRINK_PENDING_CMD_QUEUE_CAP: usize = 64;

pub struct PendingCmd {
    pub index: u64,
    pub term: u64,
    pub cb: Option<Callback<RocksEngine>>,
}

impl PendingCmd {
    fn new(index: u64, term: u64, cb: Callback<RocksEngine>) -> PendingCmd {
        PendingCmd {
            index,
            term,
            cb: Some(cb),
        }
    }
}

impl Drop for PendingCmd {
    fn drop(&mut self) {
        if self.cb.is_some() {
            safe_panic!(
                "callback of pending command at [index: {}, term: {}] is leak",
                self.index,
                self.term
            );
        }
    }
}

impl Debug for PendingCmd {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "PendingCmd [index: {}, term: {}, has_cb: {}]",
            self.index,
            self.term,
            self.cb.is_some()
        )
    }
}

/// Commands waiting to be committed and applied.
#[derive(Default, Debug)]
pub struct PendingCmdQueue {
    normals: VecDeque<PendingCmd>,
    conf_change: Option<PendingCmd>,
}

impl PendingCmdQueue {
    fn pop_normal(&mut self, index: u64, term: u64) -> Option<PendingCmd> {
        self.normals.pop_front().and_then(|cmd| {
            if self.normals.capacity() > SHRINK_PENDING_CMD_QUEUE_CAP
                && self.normals.len() < SHRINK_PENDING_CMD_QUEUE_CAP
            {
                self.normals.shrink_to_fit();
            }
            if (cmd.term, cmd.index) > (term, index) {
                self.normals.push_front(cmd);
                return None;
            }
            Some(cmd)
        })
    }

    fn append_normal(&mut self, cmd: PendingCmd) {
        self.normals.push_back(cmd);
    }

    fn take_conf_change(&mut self) -> Option<PendingCmd> {
        // conf change will not be affected when changing between follower and leader,
        // so there is no need to check term.
        self.conf_change.take()
    }

    // TODO: seems we don't need to separate conf change from normal entries.
    fn set_conf_change(&mut self, cmd: PendingCmd) {
        self.conf_change = Some(cmd);
    }
}

#[derive(Default, Debug)]
pub struct ChangePeer {
    pub index: u64,
    pub conf_change: ConfChange,
    pub peer: PeerMeta,
    pub region: Region,
}

#[derive(Debug)]
pub struct Range {
    pub cf: String,
    pub start_key: Vec<u8>,
    pub end_key: Vec<u8>,
}

impl Range {
    fn new(cf: String, start_key: Vec<u8>, end_key: Vec<u8>) -> Range {
        Range {
            cf,
            start_key,
            end_key,
        }
    }
}

#[derive(Debug)]
pub enum ExecResult {
    ChangePeer(ChangePeer),
    CompactLog {
        state: RaftTruncatedState,
        first_index: u64,
    },
    SplitRegion {
        regions: Vec<Region>,
        derived: Region,
    },
    PrepareMerge {
        region: Region,
        state: MergeState,
    },
    CommitMerge {
        region: Region,
        source: Region,
    },
    RollbackMerge {
        region: Region,
        commit: u64,
    },
    ComputeHash {
        region: Region,
        index: u64,
        snap: RocksSnapshot,
    },
    VerifyHash {
        index: u64,
        hash: Vec<u8>,
    },
    DeleteRange {
        ranges: Vec<Range>,
    },
    IngestSst {
        ssts: Vec<SstMeta>,
    },
}

/// The possible returned value when applying logs.
pub enum ApplyResult {
    None,
    Yield,
    /// Additional result that needs to be sent back to raftstore.
    Res(ExecResult),
    /// It is unable to apply the `CommitMerge` until the source peer
    /// has applied to the required position and sets the atomic boolean
    /// to true.
    WaitMergeSource(Arc<AtomicU64>),
}

struct ExecContext {
    apply_state: RaftApplyState,
    index: u64,
    term: u64,
}

impl ExecContext {
    pub fn new(apply_state: RaftApplyState, index: u64, term: u64) -> ExecContext {
        ExecContext {
            apply_state,
            index,
            term,
        }
    }
}

struct ApplyCallback {
    region: Region,
    cbs: Vec<(Option<Callback<RocksEngine>>, RaftCmdResponse)>,
}

impl ApplyCallback {
    fn new(region: Region) -> ApplyCallback {
        let cbs = vec![];
        ApplyCallback { region, cbs }
    }

    fn invoke_all(self, host: &CoprocessorHost) {
        for (cb, mut resp) in self.cbs {
            host.post_apply(&self.region, &mut resp);
            if let Some(cb) = cb {
                cb.invoke_with_response(resp)
            };
        }
    }

    fn push(&mut self, cb: Option<Callback<RocksEngine>>, resp: RaftCmdResponse) {
        self.cbs.push((cb, resp));
    }
}

#[derive(Clone)]
pub enum Notifier {
    Router(RaftRouter<RocksEngine>),
    #[cfg(test)]
    Sender(Sender<PeerMsg<RocksEngine>>),
}

impl Notifier {
    fn notify(&self, region_id: u64, msg: PeerMsg<RocksEngine>) {
        match *self {
            Notifier::Router(ref r) => {
                r.force_send(region_id, msg).unwrap();
            }
            #[cfg(test)]
            Notifier::Sender(ref s) => s.send(msg).unwrap(),
        }
    }
}

struct ApplyContext<W: WriteBatch + WriteBatchVecExt<RocksEngine>> {
    tag: String,
    timer: Option<Instant>,
    host: CoprocessorHost,
    importer: Arc<SSTImporter>,
    router: ApplyRouter,
    notifier: Notifier,
    engine: RocksEngine,
    cbs: MustConsumeVec<ApplyCallback>,
    apply_res: Vec<ApplyRes>,
    exec_ctx: Option<ExecContext>,

    kv_wb: Option<W>,
    kv_wb_last_bytes: u64,
    kv_wb_last_keys: u64,

    last_applied_index: u64,
    committed_count: usize,

    // Indicates that WAL can be synchronized when data is written to KV engine.
    enable_sync_log: bool,
    // Whether synchronize WAL is preferred.
    sync_log_hint: bool,
    // Whether to use the delete range API instead of deleting one by one.
    use_delete_range: bool,
}

impl<W: WriteBatch + WriteBatchVecExt<RocksEngine>> ApplyContext<W> {
    pub fn new(
        tag: String,
        host: CoprocessorHost,
        importer: Arc<SSTImporter>,
        engine: RocksEngine,
        router: ApplyRouter,
        notifier: Notifier,
        cfg: &Config,
    ) -> ApplyContext<W> {
        ApplyContext::<W> {
            tag,
            timer: None,
            host,
            importer,
            engine,
            router,
            notifier,
            kv_wb: None,
            cbs: MustConsumeVec::new("callback of apply context"),
            apply_res: vec![],
            kv_wb_last_bytes: 0,
            kv_wb_last_keys: 0,
            last_applied_index: 0,
            committed_count: 0,
            enable_sync_log: cfg.sync_log,
            sync_log_hint: false,
            exec_ctx: None,
            use_delete_range: cfg.use_delete_range,
        }
    }

    /// Prepares for applying entries for `delegate`.
    ///
    /// A general apply progress for a delegate is:
    /// `prepare_for` -> `commit` [-> `commit` ...] -> `finish_for`.
    /// After all delegates are handled, `write_to_db` method should be called.
    pub fn prepare_for(&mut self, delegate: &mut ApplyDelegate) {
        self.prepare_write_batch();
        self.cbs.push(ApplyCallback::new(delegate.region.clone()));
        self.last_applied_index = delegate.apply_state.get_applied_index();

        if let Some(observe_cmd) = &delegate.observe_cmd {
            let region_id = delegate.region_id();
            if observe_cmd.enabled.load(Ordering::Acquire) {
                self.host.prepare_for_apply(observe_cmd.id, region_id);
            } else {
                info!("region is no longer observerd";
                    "region_id" => region_id);
                delegate.observe_cmd.take();
            }
        }
    }

    /// Prepares WriteBatch.
    ///
    /// If `enable_multi_batch_write` was set true, we create `RocksWriteBatchVec`.
    /// Otherwise create `RocksWriteBatch`.
    pub fn prepare_write_batch(&mut self) {
        if self.kv_wb.is_none() {
            let kv_wb = W::write_batch_vec(&self.engine, WRITE_BATCH_LIMIT, DEFAULT_APPLY_WB_SIZE);
            self.kv_wb = Some(kv_wb);
            self.kv_wb_last_bytes = 0;
            self.kv_wb_last_keys = 0;
        }
    }

    /// Commits all changes have done for delegate. `persistent` indicates whether
    /// write the changes into rocksdb.
    ///
    /// This call is valid only when it's between a `prepare_for` and `finish_for`.
    pub fn commit(&mut self, delegate: &mut ApplyDelegate) {
        if self.last_applied_index < delegate.apply_state.get_applied_index() {
            delegate.write_apply_state(self.kv_wb.as_mut().unwrap());
        }
        // last_applied_index doesn't need to be updated, set persistent to true will
        // force it call `prepare_for` automatically.
        self.commit_opt(delegate, true);
    }

    fn commit_opt(&mut self, delegate: &mut ApplyDelegate, persistent: bool) {
        delegate.update_metrics(self);
        if persistent {
            self.write_to_db();
            self.prepare_for(delegate);
        }
        self.kv_wb_last_bytes = self.kv_wb().data_size() as u64;
        self.kv_wb_last_keys = self.kv_wb().count() as u64;
    }

    /// Writes all the changes into RocksDB.
    /// If it returns true, all pending writes are persisted in engines.
    pub fn write_to_db(&mut self) -> bool {
        let need_sync = self.enable_sync_log && self.sync_log_hint;
        if self.kv_wb.as_ref().map_or(false, |wb| !wb.is_empty()) {
            let mut write_opts = engine_traits::WriteOptions::new();
            write_opts.set_sync(need_sync);
            self.kv_wb()
                .write_to_engine(&self.engine, &write_opts)
                .unwrap_or_else(|e| {
                    panic!("failed to write to engine: {:?}", e);
                });
            self.sync_log_hint = false;
            let data_size = self.kv_wb().data_size();
            if data_size > APPLY_WB_SHRINK_SIZE {
                // Control the memory usage for the WriteBatch.
                let kv_wb =
                    W::write_batch_vec(&self.engine, WRITE_BATCH_LIMIT, DEFAULT_APPLY_WB_SIZE);
                self.kv_wb = Some(kv_wb);
            } else {
                // Clear data, reuse the WriteBatch, this can reduce memory allocations and deallocations.
                self.kv_wb_mut().clear();
            }
            self.kv_wb_last_bytes = 0;
            self.kv_wb_last_keys = 0;
        }
        // Call it before invoking callback for preventing Commit is executed before Prewrite is observed.
        self.host.on_flush_apply();

        for cbs in self.cbs.drain(..) {
            cbs.invoke_all(&self.host);
        }
        need_sync
    }

    /// Finishes `Apply`s for the delegate.
    pub fn finish_for(&mut self, delegate: &mut ApplyDelegate, results: VecDeque<ExecResult>) {
        if !delegate.pending_remove {
            delegate.write_apply_state(self.kv_wb.as_mut().unwrap());
        }
        self.commit_opt(delegate, false);
        self.apply_res.push(ApplyRes {
            region_id: delegate.region_id(),
            apply_state: delegate.apply_state.clone(),
            exec_res: results,
            metrics: delegate.metrics.clone(),
            applied_index_term: delegate.applied_index_term,
        });
    }

    pub fn delta_bytes(&self) -> u64 {
        self.kv_wb().data_size() as u64 - self.kv_wb_last_bytes
    }

    pub fn delta_keys(&self) -> u64 {
        self.kv_wb().count() as u64 - self.kv_wb_last_keys
    }

    #[inline]
    pub fn kv_wb(&self) -> &W {
        self.kv_wb.as_ref().unwrap()
    }

    #[inline]
    pub fn kv_wb_mut(&mut self) -> &mut W {
        self.kv_wb.as_mut().unwrap()
    }

    /// Flush all pending writes to engines.
    /// If it returns true, all pending writes are persisted in engines.
    pub fn flush(&mut self) -> bool {
        // TODO: this check is too hacky, need to be more verbose and less buggy.
        let t = match self.timer.take() {
            Some(t) => t,
            None => return false,
        };

        // Write to engine
        // raftstore.sync-log = true means we need prevent data loss when power failure.
        // take raft log gc for example, we write kv WAL first, then write raft WAL,
        // if power failure happen, raft WAL may synced to disk, but kv WAL may not.
        // so we use sync-log flag here.
        let is_synced = self.write_to_db();

        if !self.apply_res.is_empty() {
            for res in self.apply_res.drain(..) {
                self.notifier.notify(
                    res.region_id,
                    PeerMsg::ApplyRes {
                        res: TaskRes::Apply(res),
                    },
                );
            }
        }

        let elapsed = t.elapsed();
        STORE_APPLY_LOG_HISTOGRAM.observe(duration_to_sec(elapsed) as f64);

        slow_log!(
            elapsed,
            "{} handle ready {} committed entries",
            self.tag,
            self.committed_count
        );
        self.committed_count = 0;
        is_synced
    }
}

/// Calls the callback of `cmd` when the Region is removed.
fn notify_region_removed(region_id: u64, peer_id: u64, mut cmd: PendingCmd) {
    debug!(
        "region is removed, notify commands";
        "region_id" => region_id,
        "peer_id" => peer_id,
        "index" => cmd.index,
        "term" => cmd.term
    );
    notify_req_region_removed(region_id, cmd.cb.take().unwrap());
}

pub fn notify_req_region_removed(region_id: u64, cb: Callback<RocksEngine>) {
    let region_not_found = Error::RegionNotFound(region_id);
    let resp = cmd_resp::new_error(region_not_found);
    cb.invoke_with_response(resp);
}

/// Calls the callback of `cmd` when it can not be processed further.
fn notify_stale_command(region_id: u64, peer_id: u64, term: u64, mut cmd: PendingCmd) {
    info!(
        "command is stale, skip";
        "region_id" => region_id,
        "peer_id" => peer_id,
        "index" => cmd.index,
        "term" => cmd.term
    );
    notify_stale_req(term, cmd.cb.take().unwrap());
}

pub fn notify_stale_req(term: u64, cb: Callback<RocksEngine>) {
    let resp = cmd_resp::err_resp(Error::StaleCommand, term);
    cb.invoke_with_response(resp);
}

/// Checks if a write is needed to be issued before handling the command.
fn should_write_to_engine(cmd: &RaftCmdRequest) -> bool {
    if cmd.has_admin_request() {
        match cmd.get_admin_request().get_cmd_type() {
            // ComputeHash require an up to date snapshot.
            AdminCmdType::ComputeHash |
            // Merge needs to get the latest apply index.
            AdminCmdType::CommitMerge |
            AdminCmdType::RollbackMerge => return true,
            _ => {}
        }
    }

    // Some commands may modify keys covered by the current write batch, so we
    // must write the current write batch to the engine first.
    for req in cmd.get_requests() {
        if req.has_delete_range() {
            return true;
        }
        if req.has_ingest_sst() {
            return true;
        }
    }

    false
}

/// Checks if a write is needed to be issued after handling the command.
fn should_sync_log(cmd: &RaftCmdRequest) -> bool {
    if cmd.has_admin_request() {
        return true;
    }

    for req in cmd.get_requests() {
        // After ingest sst, sst files are deleted quickly. As a result,
        // ingest sst command can not be handled again and must be synced.
        // See more in Cleanup worker.
        if req.has_ingest_sst() {
            return true;
        }
    }

    false
}

/// A struct that stores the state related to Merge.
///
/// When executing a `CommitMerge`, the source peer may have not applied
/// to the required index, so the target peer has to abort current execution
/// and wait for it asynchronously.
///
/// When rolling the stack, all states required to recover are stored in
/// this struct.
/// TODO: check whether generator/coroutine is a good choice in this case.
struct WaitSourceMergeState {
    /// A flag that indicates whether the source peer has applied to the required
    /// index. If the source peer is ready, this flag should be set to the region id
    /// of source peer.
    logs_up_to_date: Arc<AtomicU64>,
}

struct YieldState {
    /// All of the entries that need to continue to be applied after
    /// the source peer has applied its logs.
    pending_entries: Vec<Entry>,
    /// All of messages that need to continue to be handled after
    /// the source peer has applied its logs and pending entries
    /// are all handled.
    pending_msgs: Vec<Msg>,
}

impl Debug for YieldState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("YieldState")
            .field("pending_entries", &self.pending_entries.len())
            .field("pending_msgs", &self.pending_msgs.len())
            .finish()
    }
}

impl Debug for WaitSourceMergeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WaitSourceMergeState")
            .field("logs_up_to_date", &self.logs_up_to_date)
            .finish()
    }
}

/// The apply delegate of a Region which is responsible for handling committed
/// raft log entries of a Region.
///
/// `Apply` is a term of Raft, which means executing the actual commands.
/// In Raft, once some log entries are committed, for every peer of the Raft
/// group will apply the logs one by one. For write commands, it does write or
/// delete to local engine; for admin commands, it does some meta change of the
/// Raft group.
///
/// `Delegate` is just a structure to congregate all apply related fields of a
/// Region. The apply worker receives all the apply tasks of different Regions
/// located at this store, and it will get the corresponding apply delegate to
/// handle the apply task to make the code logic more clear.
#[derive(Debug)]
pub struct ApplyDelegate {
    /// The ID of the peer.
    id: u64,
    /// The term of the Region.
    term: u64,
    /// The Region information of the peer.
    region: Region,
    /// Peer_tag, "[region region_id] peer_id".
    tag: String,

    /// If the delegate should be stopped from polling.
    /// A delegate can be stopped in conf change, merge or requested by destroy message.
    stopped: bool,
    written: bool,
    /// Set to true when removing itself because of `ConfChangeType::RemoveNode`, and then
    /// any following committed logs in same Ready should be applied failed.
    pending_remove: bool,

    /// The commands waiting to be committed and applied
    pending_cmds: PendingCmdQueue,
    /// The counter of pending request snapshots. See more in `Peer`.
    pending_request_snapshot_count: Arc<AtomicUsize>,

    /// Indicates the peer is in merging, if that compact log won't be performed.
    is_merging: bool,
    /// Records the epoch version after the last merge.
    last_merge_version: u64,
    yield_state: Option<YieldState>,
    /// A temporary state that keeps track of the progress of the source peer state when
    /// CommitMerge is unable to be executed.
    wait_merge_state: Option<WaitSourceMergeState>,
    // ID of last region that reports ready.
    ready_source_region_id: u64,

    /// TiKV writes apply_state to KV RocksDB, in one write batch together with kv data.
    ///
    /// If we write it to Raft RocksDB, apply_state and kv data (Put, Delete) are in
    /// separate WAL file. When power failure, for current raft log, apply_index may synced
    /// to file, but KV data may not synced to file, so we will lose data.
    apply_state: RaftApplyState,
    /// The term of the raft log at applied index.
    applied_index_term: u64,
    /// The latest synced apply index.
    last_sync_apply_index: u64,

    /// Info about cmd observer.
    observe_cmd: Option<ObserveCmd>,

    /// The local metrics, and it will be flushed periodically.
    metrics: ApplyMetrics,
}

impl ApplyDelegate {
    fn from_registration(reg: Registration) -> ApplyDelegate {
        ApplyDelegate {
            id: reg.id,
            tag: format!("[region {}] {}", reg.region.get_id(), reg.id),
            region: reg.region,
            pending_remove: false,
            last_sync_apply_index: reg.apply_state.get_applied_index(),
            apply_state: reg.apply_state,
            applied_index_term: reg.applied_index_term,
            term: reg.term,
            stopped: false,
            written: false,
            ready_source_region_id: 0,
            yield_state: None,
            wait_merge_state: None,
            is_merging: reg.is_merging,
            pending_cmds: Default::default(),
            metrics: Default::default(),
            last_merge_version: 0,
            pending_request_snapshot_count: reg.pending_request_snapshot_count,
            observe_cmd: None,
        }
    }

    pub fn region_id(&self) -> u64 {
        self.region.get_id()
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    /// Handles all the committed_entries, namely, applies the committed entries.
    fn handle_raft_committed_entries<W: WriteBatch + WriteBatchVecExt<RocksEngine>>(
        &mut self,
        apply_ctx: &mut ApplyContext<W>,
        mut committed_entries: Vec<Entry>,
    ) {
        if committed_entries.is_empty() {
            return;
        }
        apply_ctx.prepare_for(self);
        // If we send multiple ConfChange commands, only first one will be proposed correctly,
        // others will be saved as a normal entry with no data, so we must re-propose these
        // commands again.
        apply_ctx.committed_count += committed_entries.len();
        let mut drainer = committed_entries.drain(..);
        let mut results = VecDeque::new();
        while let Some(entry) = drainer.next() {
            if self.pending_remove {
                // This peer is about to be destroyed, skip everything.
                break;
            }

            let expect_index = self.apply_state.get_applied_index() + 1;
            if expect_index != entry.get_index() {
                // Msg::CatchUpLogs may have arrived before Msg::Apply.
                if expect_index > entry.get_index() && self.is_merging {
                    info!(
                        "skip log as it's already applied";
                        "region_id" => self.region_id(),
                        "peer_id" => self.id(),
                        "index" => entry.get_index()
                    );
                    continue;
                }
                panic!(
                    "{} expect index {}, but got {}",
                    self.tag,
                    expect_index,
                    entry.get_index()
                );
            }

            let res = match entry.get_entry_type() {
                EntryType::EntryNormal => self.handle_raft_entry_normal(apply_ctx, &entry),
                EntryType::EntryConfChange => self.handle_raft_entry_conf_change(apply_ctx, &entry),
                EntryType::EntryConfChangeV2 => unimplemented!(),
            };

            match res {
                ApplyResult::None => {}
                ApplyResult::Res(res) => results.push_back(res),
                _ => {
                    // Both cancel and merge will yield current processing.
                    apply_ctx.committed_count -= drainer.len() + 1;
                    let mut pending_entries = Vec::with_capacity(drainer.len() + 1);
                    // Note that current entry is skipped when yield.
                    pending_entries.push(entry);
                    pending_entries.extend(drainer);
                    apply_ctx.finish_for(self, results);
                    self.yield_state = Some(YieldState {
                        pending_entries,
                        pending_msgs: Vec::default(),
                    });
                    if let ApplyResult::WaitMergeSource(logs_up_to_date) = res {
                        self.wait_merge_state = Some(WaitSourceMergeState { logs_up_to_date });
                    }
                    return;
                }
            }
        }

        apply_ctx.finish_for(self, results);

        if self.pending_remove {
            self.destroy(apply_ctx);
        }
    }

    fn update_metrics<W: WriteBatch + WriteBatchVecExt<RocksEngine>>(
        &mut self,
        apply_ctx: &ApplyContext<W>,
    ) {
        self.metrics.written_bytes += apply_ctx.delta_bytes();
        self.metrics.written_keys += apply_ctx.delta_keys();
    }

    fn write_apply_state<W: WriteBatch + WriteBatchVecExt<RocksEngine>>(&self, wb: &mut W) {
        wb.put_msg_cf(
            CF_RAFT,
            &keys::apply_state_key(self.region.get_id()),
            &self.apply_state,
        )
        .unwrap_or_else(|e| {
            panic!(
                "{} failed to save apply state to write batch, error: {:?}",
                self.tag, e
            );
        });
    }

    fn handle_raft_entry_normal<W: WriteBatch + WriteBatchVecExt<RocksEngine>>(
        &mut self,
        apply_ctx: &mut ApplyContext<W>,
        entry: &Entry,
    ) -> ApplyResult {
        fail_point!("yield_apply_1000", self.region_id() == 1000, |_| {
            ApplyResult::Yield
        });

        let index = entry.get_index();
        let term = entry.get_term();
        let data = entry.get_data();

        if !data.is_empty() {
            let cmd = util::parse_data_at(data, index, &self.tag);

            if should_write_to_engine(&cmd) || apply_ctx.kv_wb().should_write_to_engine() {
                apply_ctx.commit(self);
                if self.written {
                    return ApplyResult::Yield;
                }
                self.written = true;
            }

            return self.process_raft_cmd(apply_ctx, index, term, cmd);
        }
        // TOOD(cdc): should we observe empty cmd, aka leader change?

        self.apply_state.set_applied_index(index);
        self.applied_index_term = term;
        assert!(term > 0);

        // 1. When a peer become leader, it will send an empty entry.
        // 2. When a leader tries to read index during transferring leader,
        //    it will also propose an empty entry. But that entry will not contain
        //    any associated callback. So no need to clear callback.
        while let Some(mut cmd) = self.pending_cmds.pop_normal(std::u64::MAX, term - 1) {
            apply_ctx
                .cbs
                .last_mut()
                .unwrap()
                .push(cmd.cb.take(), cmd_resp::err_resp(Error::StaleCommand, term));
        }
        ApplyResult::None
    }

    fn handle_raft_entry_conf_change<W: WriteBatch + WriteBatchVecExt<RocksEngine>>(
        &mut self,
        apply_ctx: &mut ApplyContext<W>,
        entry: &Entry,
    ) -> ApplyResult {
        // Although conf change can't yield in normal case, it is convenient to
        // simulate yield before applying a conf change log.
        fail_point!("yield_apply_conf_change_3", self.id() == 3, |_| {
            ApplyResult::Yield
        });
        let index = entry.get_index();
        let term = entry.get_term();
        let conf_change: ConfChange = util::parse_data_at(entry.get_data(), index, &self.tag);
        let cmd = util::parse_data_at(conf_change.get_context(), index, &self.tag);
        match self.process_raft_cmd(apply_ctx, index, term, cmd) {
            ApplyResult::None => {
                // If failed, tell Raft that the `ConfChange` was aborted.
                ApplyResult::Res(ExecResult::ChangePeer(Default::default()))
            }
            ApplyResult::Res(mut res) => {
                if let ExecResult::ChangePeer(ref mut cp) = res {
                    cp.conf_change = conf_change;
                } else {
                    panic!(
                        "{} unexpected result {:?} for conf change {:?} at {}",
                        self.tag, res, conf_change, index
                    );
                }
                ApplyResult::Res(res)
            }
            ApplyResult::Yield | ApplyResult::WaitMergeSource(_) => unreachable!(),
        }
    }

    fn find_cb(
        &mut self,
        index: u64,
        term: u64,
        is_conf_change: bool,
    ) -> Option<Callback<RocksEngine>> {
        let (region_id, peer_id) = (self.region_id(), self.id());
        if is_conf_change {
            if let Some(mut cmd) = self.pending_cmds.take_conf_change() {
                if cmd.index == index && cmd.term == term {
                    return Some(cmd.cb.take().unwrap());
                } else {
                    notify_stale_command(region_id, peer_id, self.term, cmd);
                }
            }
            return None;
        }
        while let Some(mut head) = self.pending_cmds.pop_normal(index, term) {
            if head.term == term {
                if head.index == index {
                    return Some(head.cb.take().unwrap());
                } else {
                    panic!(
                        "{} unexpected callback at term {}, found index {}, expected {}",
                        self.tag, term, head.index, index
                    );
                }
            } else {
                // Because of the lack of original RaftCmdRequest, we skip calling
                // coprocessor here.
                notify_stale_command(region_id, peer_id, self.term, head);
            }
        }
        None
    }

    fn process_raft_cmd<W: WriteBatch + WriteBatchVecExt<RocksEngine>>(
        &mut self,
        apply_ctx: &mut ApplyContext<W>,
        index: u64,
        term: u64,
        cmd: RaftCmdRequest,
    ) -> ApplyResult {
        if index == 0 {
            panic!(
                "{} processing raft command needs a none zero index",
                self.tag
            );
        }

        // Set sync log hint if the cmd requires so.
        apply_ctx.sync_log_hint |= should_sync_log(&cmd);

        let is_conf_change = get_change_peer_cmd(&cmd).is_some();
        apply_ctx.host.pre_apply(&self.region, &cmd);
        let (mut resp, exec_result) = self.apply_raft_cmd(apply_ctx, index, term, &cmd);
        if let ApplyResult::WaitMergeSource(_) = exec_result {
            return exec_result;
        }

        debug!(
            "applied command";
            "region_id" => self.region_id(),
            "peer_id" => self.id(),
            "index" => index
        );

        // TODO: if we have exec_result, maybe we should return this callback too. Outer
        // store will call it after handing exec result.
        cmd_resp::bind_term(&mut resp, self.term);
        let cmd_cb = self.find_cb(index, term, is_conf_change);
        if let Some(observe_cmd) = self.observe_cmd.as_ref() {
            let cmd = Cmd::new(index, cmd, resp.clone());
            apply_ctx
                .host
                .on_apply_cmd(observe_cmd.id, self.region_id(), cmd);
        }

        apply_ctx.cbs.last_mut().unwrap().push(cmd_cb, resp);

        exec_result
    }

    /// Applies raft command.
    ///
    /// An apply operation can fail in the following situations:
    ///   1. it encounters an error that will occur on all stores, it can continue
    /// applying next entry safely, like epoch not match for example;
    ///   2. it encounters an error that may not occur on all stores, in this case
    /// we should try to apply the entry again or panic. Considering that this
    /// usually due to disk operation fail, which is rare, so just panic is ok.
    fn apply_raft_cmd<W: WriteBatch + WriteBatchVecExt<RocksEngine>>(
        &mut self,
        ctx: &mut ApplyContext<W>,
        index: u64,
        term: u64,
        req: &RaftCmdRequest,
    ) -> (RaftCmdResponse, ApplyResult) {
        // if pending remove, apply should be aborted already.
        assert!(!self.pending_remove);

        ctx.exec_ctx = Some(self.new_ctx(index, term));
        ctx.kv_wb_mut().set_save_point();
        let (resp, exec_result) = match self.exec_raft_cmd(ctx, &req) {
            Ok(a) => {
                ctx.kv_wb_mut().pop_save_point().unwrap();
                a
            }
            Err(e) => {
                // clear dirty values.
                ctx.kv_wb_mut().rollback_to_save_point().unwrap();
                match e {
                    Error::EpochNotMatch(..) => debug!(
                        "epoch not match";
                        "region_id" => self.region_id(),
                        "peer_id" => self.id(),
                        "err" => ?e
                    ),
                    _ => error!(
                        "execute raft command";
                        "region_id" => self.region_id(),
                        "peer_id" => self.id(),
                        "err" => ?e
                    ),
                }
                (cmd_resp::new_error(e), ApplyResult::None)
            }
        };
        if let ApplyResult::WaitMergeSource(_) = exec_result {
            return (resp, exec_result);
        }

        let mut exec_ctx = ctx.exec_ctx.take().unwrap();
        exec_ctx.apply_state.set_applied_index(index);

        self.apply_state = exec_ctx.apply_state;
        self.applied_index_term = term;

        if let ApplyResult::Res(ref exec_result) = exec_result {
            match *exec_result {
                ExecResult::ChangePeer(ref cp) => {
                    self.region = cp.region.clone();
                }
                ExecResult::ComputeHash { .. }
                | ExecResult::VerifyHash { .. }
                | ExecResult::CompactLog { .. }
                | ExecResult::DeleteRange { .. }
                | ExecResult::IngestSst { .. } => {}
                ExecResult::SplitRegion { ref derived, .. } => {
                    self.region = derived.clone();
                    self.metrics.size_diff_hint = 0;
                    self.metrics.delete_keys_hint = 0;
                }
                ExecResult::PrepareMerge { ref region, .. } => {
                    self.region = region.clone();
                    self.is_merging = true;
                }
                ExecResult::CommitMerge { ref region, .. } => {
                    self.region = region.clone();
                    self.last_merge_version = region.get_region_epoch().get_version();
                }
                ExecResult::RollbackMerge { ref region, .. } => {
                    self.region = region.clone();
                    self.is_merging = false;
                }
            }
        }

        (resp, exec_result)
    }

    fn destroy<W: WriteBatch + WriteBatchVecExt<RocksEngine>>(
        &mut self,
        apply_ctx: &mut ApplyContext<W>,
    ) {
        self.stopped = true;
        apply_ctx.router.close(self.region_id());
        for cmd in self.pending_cmds.normals.drain(..) {
            notify_region_removed(self.region.get_id(), self.id, cmd);
        }
        if let Some(cmd) = self.pending_cmds.conf_change.take() {
            notify_region_removed(self.region.get_id(), self.id, cmd);
        }
    }

    fn clear_all_commands_as_stale(&mut self) {
        let (region_id, peer_id) = (self.region_id(), self.id());
        for cmd in self.pending_cmds.normals.drain(..) {
            notify_stale_command(region_id, peer_id, self.term, cmd);
        }
        if let Some(cmd) = self.pending_cmds.conf_change.take() {
            notify_stale_command(region_id, peer_id, self.term, cmd);
        }
    }

    fn new_ctx(&self, index: u64, term: u64) -> ExecContext {
        ExecContext::new(self.apply_state.clone(), index, term)
    }
}

impl ApplyDelegate {
    // Only errors that will also occur on all other stores should be returned.
    fn exec_raft_cmd<W: WriteBatch + WriteBatchVecExt<RocksEngine>>(
        &mut self,
        ctx: &mut ApplyContext<W>,
        req: &RaftCmdRequest,
    ) -> Result<(RaftCmdResponse, ApplyResult)> {
        // Include region for epoch not match after merge may cause key not in range.
        let include_region =
            req.get_header().get_region_epoch().get_version() >= self.last_merge_version;
        check_region_epoch(req, &self.region, include_region)?;
        if req.has_admin_request() {
            self.exec_admin_cmd(ctx, req)
        } else {
            self.exec_write_cmd(ctx, req)
        }
    }

    fn exec_admin_cmd<W: WriteBatch + WriteBatchVecExt<RocksEngine>>(
        &mut self,
        ctx: &mut ApplyContext<W>,
        req: &RaftCmdRequest,
    ) -> Result<(RaftCmdResponse, ApplyResult)> {
        let request = req.get_admin_request();
        let cmd_type = request.get_cmd_type();
        if cmd_type != AdminCmdType::CompactLog && cmd_type != AdminCmdType::CommitMerge {
            info!(
                "execute admin command";
                "region_id" => self.region_id(),
                "peer_id" => self.id(),
                "term" => ctx.exec_ctx.as_ref().unwrap().term,
                "index" => ctx.exec_ctx.as_ref().unwrap().index,
                "command" => ?request
            );
        }

        let (mut response, exec_result) = match cmd_type {
            AdminCmdType::ChangePeer => self.exec_change_peer(ctx, request),
            AdminCmdType::Split => self.exec_split(ctx, request),
            AdminCmdType::BatchSplit => self.exec_batch_split(ctx, request),
            AdminCmdType::CompactLog => self.exec_compact_log(ctx, request),
            AdminCmdType::TransferLeader => Err(box_err!("transfer leader won't exec")),
            AdminCmdType::ComputeHash => self.exec_compute_hash(ctx, request),
            AdminCmdType::VerifyHash => self.exec_verify_hash(ctx, request),
            // TODO: is it backward compatible to add new cmd_type?
            AdminCmdType::PrepareMerge => self.exec_prepare_merge(ctx, request),
            AdminCmdType::CommitMerge => self.exec_commit_merge(ctx, request),
            AdminCmdType::RollbackMerge => self.exec_rollback_merge(ctx, request),
            AdminCmdType::InvalidAdmin => Err(box_err!("unsupported admin command type")),
        }?;
        response.set_cmd_type(cmd_type);

        let mut resp = RaftCmdResponse::default();
        if !req.get_header().get_uuid().is_empty() {
            let uuid = req.get_header().get_uuid().to_vec();
            resp.mut_header().set_uuid(uuid);
        }
        resp.set_admin_response(response);
        Ok((resp, exec_result))
    }

    fn exec_write_cmd<W: WriteBatch + WriteBatchVecExt<RocksEngine>>(
        &mut self,
        ctx: &mut ApplyContext<W>,
        req: &RaftCmdRequest,
    ) -> Result<(RaftCmdResponse, ApplyResult)> {
        fail_point!(
            "on_apply_write_cmd",
            cfg!(release) || self.id() == 3,
            |_| {
                unimplemented!();
            }
        );

        let requests = req.get_requests();
        let mut responses = Vec::with_capacity(requests.len());

        let mut ranges = vec![];
        let mut ssts = vec![];
        for req in requests {
            let cmd_type = req.get_cmd_type();
            let mut resp = match cmd_type {
                CmdType::Put => self.handle_put(ctx.kv_wb_mut(), req),
                CmdType::Delete => self.handle_delete(ctx.kv_wb_mut(), req),
                CmdType::DeleteRange => {
                    self.handle_delete_range(&ctx.engine, req, &mut ranges, ctx.use_delete_range)
                }
                CmdType::IngestSst => {
                    self.handle_ingest_sst(&ctx.importer, &ctx.engine, req, &mut ssts)
                }
                // Readonly commands are handled in raftstore directly.
                // Don't panic here in case there are old entries need to be applied.
                // It's also safe to skip them here, because a restart must have happened,
                // hence there is no callback to be called.
                CmdType::Snap | CmdType::Get => {
                    warn!(
                        "skip readonly command";
                        "region_id" => self.region_id(),
                        "peer_id" => self.id(),
                        "command" => ?req
                    );
                    continue;
                }
                CmdType::Prewrite | CmdType::Invalid | CmdType::ReadIndex => {
                    Err(box_err!("invalid cmd type, message maybe corrupted"))
                }
            }?;

            resp.set_cmd_type(cmd_type);

            responses.push(resp);
        }

        let mut resp = RaftCmdResponse::default();
        if !req.get_header().get_uuid().is_empty() {
            let uuid = req.get_header().get_uuid().to_vec();
            resp.mut_header().set_uuid(uuid);
        }
        resp.set_responses(responses.into());

        assert!(ranges.is_empty() || ssts.is_empty());
        let exec_res = if !ranges.is_empty() {
            ApplyResult::Res(ExecResult::DeleteRange { ranges })
        } else if !ssts.is_empty() {
            ApplyResult::Res(ExecResult::IngestSst { ssts })
        } else {
            ApplyResult::None
        };

        Ok((resp, exec_res))
    }
}

// Write commands related.
impl ApplyDelegate {
    fn handle_put<W: WriteBatch>(&mut self, wb: &mut W, req: &Request) -> Result<Response> {
        let (key, value) = (req.get_put().get_key(), req.get_put().get_value());
        // region key range has no data prefix, so we must use origin key to check.
        util::check_key_in_region(key, &self.region)?;

        let resp = Response::default();
        let key = keys::data_key(key);
        self.metrics.size_diff_hint += key.len() as i64;
        self.metrics.size_diff_hint += value.len() as i64;
        if !req.get_put().get_cf().is_empty() {
            let cf = req.get_put().get_cf();
            // TODO: don't allow write preseved cfs.
            if cf == CF_LOCK {
                self.metrics.lock_cf_written_bytes += key.len() as u64;
                self.metrics.lock_cf_written_bytes += value.len() as u64;
            }
            // TODO: check whether cf exists or not.
            wb.put_cf(cf, &key, value).unwrap_or_else(|e| {
                panic!(
                    "{} failed to write ({}, {}) to cf {}: {:?}",
                    self.tag,
                    hex::encode_upper(&key),
                    escape(value),
                    cf,
                    e
                )
            });
        } else {
            wb.put(&key, value).unwrap_or_else(|e| {
                panic!(
                    "{} failed to write ({}, {}): {:?}",
                    self.tag,
                    hex::encode_upper(&key),
                    escape(value),
                    e
                );
            });
        }
        Ok(resp)
    }

    fn handle_delete<W: WriteBatch>(&mut self, wb: &mut W, req: &Request) -> Result<Response> {
        let key = req.get_delete().get_key();
        // region key range has no data prefix, so we must use origin key to check.
        util::check_key_in_region(key, &self.region)?;

        let key = keys::data_key(key);
        // since size_diff_hint is not accurate, so we just skip calculate the value size.
        self.metrics.size_diff_hint -= key.len() as i64;
        let resp = Response::default();
        if !req.get_delete().get_cf().is_empty() {
            let cf = req.get_delete().get_cf();
            // TODO: check whether cf exists or not.
            wb.delete_cf(cf, &key).unwrap_or_else(|e| {
                panic!(
                    "{} failed to delete {}: {}",
                    self.tag,
                    hex::encode_upper(&key),
                    e
                )
            });

            if cf == CF_LOCK {
                // delete is a kind of write for RocksDB.
                self.metrics.lock_cf_written_bytes += key.len() as u64;
            } else {
                self.metrics.delete_keys_hint += 1;
            }
        } else {
            wb.delete(&key).unwrap_or_else(|e| {
                panic!(
                    "{} failed to delete {}: {}",
                    self.tag,
                    hex::encode_upper(&key),
                    e
                )
            });
            self.metrics.delete_keys_hint += 1;
        }

        Ok(resp)
    }

    fn handle_delete_range(
        &mut self,
        engine: &RocksEngine,
        req: &Request,
        ranges: &mut Vec<Range>,
        use_delete_range: bool,
    ) -> Result<Response> {
        let s_key = req.get_delete_range().get_start_key();
        let e_key = req.get_delete_range().get_end_key();
        let notify_only = req.get_delete_range().get_notify_only();
        if !e_key.is_empty() && s_key >= e_key {
            return Err(box_err!(
                "invalid delete range command, start_key: {:?}, end_key: {:?}",
                s_key,
                e_key
            ));
        }
        // region key range has no data prefix, so we must use origin key to check.
        util::check_key_in_region(s_key, &self.region)?;
        let end_key = keys::data_end_key(e_key);
        let region_end_key = keys::data_end_key(self.region.get_end_key());
        if end_key > region_end_key {
            return Err(Error::KeyNotInRegion(e_key.to_vec(), self.region.clone()));
        }

        let resp = Response::default();
        let mut cf = req.get_delete_range().get_cf();
        if cf.is_empty() {
            cf = CF_DEFAULT;
        }
        if ALL_CFS.iter().find(|x| **x == cf).is_none() {
            return Err(box_err!("invalid delete range command, cf: {:?}", cf));
        }

        let start_key = keys::data_key(s_key);
        // Use delete_files_in_range to drop as many sst files as possible, this
        // is a way to reclaim disk space quickly after drop a table/index.
        if !notify_only {
            engine
                .delete_files_in_range_cf(cf, &start_key, &end_key, /* include_end */ false)
                .unwrap_or_else(|e| {
                    panic!(
                        "{} failed to delete files in range [{}, {}): {:?}",
                        self.tag,
                        hex::encode_upper(&start_key),
                        hex::encode_upper(&end_key),
                        e
                    )
                });

            // Delete all remaining keys.
            engine
                .delete_all_in_range_cf(cf, &start_key, &end_key, use_delete_range)
                .unwrap_or_else(|e| {
                    panic!(
                        "{} failed to delete all in range [{}, {}), cf: {}, err: {:?}",
                        self.tag,
                        hex::encode_upper(&start_key),
                        hex::encode_upper(&end_key),
                        cf,
                        e
                    );
                });
        }

        // TODO: Should this be executed when `notify_only` is set?
        ranges.push(Range::new(cf.to_owned(), start_key, end_key));

        Ok(resp)
    }

    fn handle_ingest_sst(
        &mut self,
        importer: &Arc<SSTImporter>,
        engine: &RocksEngine,
        req: &Request,
        ssts: &mut Vec<SstMeta>,
    ) -> Result<Response> {
        let sst = req.get_ingest_sst().get_sst();

        if let Err(e) = check_sst_for_ingestion(sst, &self.region) {
            error!(
                 "ingest fail";
                 "region_id" => self.region_id(),
                 "peer_id" => self.id(),
                 "sst" => ?sst,
                 "region" => ?&self.region,
                 "err" => ?e
            );
            // This file is not valid, we can delete it here.
            let _ = importer.delete(sst);
            return Err(e);
        }

        importer.ingest(sst, engine).unwrap_or_else(|e| {
            // If this failed, it means that the file is corrupted or something
            // is wrong with the engine, but we can do nothing about that.
            panic!("{} ingest {:?}: {:?}", self.tag, sst, e);
        });

        ssts.push(sst.clone());
        Ok(Response::default())
    }
}

// Admin commands related.
impl ApplyDelegate {
    fn exec_change_peer<W: WriteBatch + WriteBatchVecExt<RocksEngine>>(
        &mut self,
        ctx: &mut ApplyContext<W>,
        request: &AdminRequest,
    ) -> Result<(AdminResponse, ApplyResult)> {
        let request = request.get_change_peer();
        let peer = request.get_peer();
        let store_id = peer.get_store_id();
        let change_type = request.get_change_type();
        let mut region = self.region.clone();

        fail_point!(
            "apply_on_conf_change_1_3_1",
            (self.id == 1 || self.id == 3) && self.region_id() == 1,
            |_| panic!("should not use return")
        );
        fail_point!(
            "apply_on_conf_change_3_1",
            self.id == 3 && self.region_id() == 1,
            |_| panic!("should not use return")
        );
        fail_point!(
            "apply_on_conf_change_all_1",
            self.region_id() == 1,
            |_| panic!("should not use return")
        );
        info!(
            "exec ConfChange";
            "region_id" => self.region_id(),
            "peer_id" => self.id(),
            "type" => util::conf_change_type_str(change_type),
            "epoch" => ?region.get_region_epoch(),
        );

        // TODO: we should need more check, like peer validation, duplicated id, etc.
        let conf_ver = region.get_region_epoch().get_conf_ver() + 1;
        region.mut_region_epoch().set_conf_ver(conf_ver);

        match change_type {
            ConfChangeType::AddNode => {
                let add_ndoe_fp = || {
                    fail_point!(
                        "apply_on_add_node_1_2",
                        { self.id == 2 && self.region_id() == 1 },
                        |_| {}
                    )
                };
                add_ndoe_fp();

                PEER_ADMIN_CMD_COUNTER_VEC
                    .with_label_values(&["add_peer", "all"])
                    .inc();

                let mut exists = false;
                if let Some(p) = util::find_peer_mut(&mut region, store_id) {
                    exists = true;
                    if !p.get_is_learner() || p.get_id() != peer.get_id() {
                        error!(
                            "can't add duplicated peer";
                            "region_id" => self.region_id(),
                            "peer_id" => self.id(),
                            "peer" => ?peer,
                            "region" => ?&self.region
                        );
                        return Err(box_err!(
                            "can't add duplicated peer {:?} to region {:?}",
                            peer,
                            self.region
                        ));
                    } else {
                        p.set_is_learner(false);
                    }
                }
                if !exists {
                    // TODO: Do we allow adding peer in same node?
                    region.mut_peers().push(peer.clone());
                }

                PEER_ADMIN_CMD_COUNTER_VEC
                    .with_label_values(&["add_peer", "success"])
                    .inc();
                info!(
                    "add peer successfully";
                    "region_id" => self.region_id(),
                    "peer_id" => self.id(),
                    "peer" => ?peer,
                    "region" => ?&self.region
                );
            }
            ConfChangeType::RemoveNode => {
                PEER_ADMIN_CMD_COUNTER_VEC
                    .with_label_values(&["remove_peer", "all"])
                    .inc();

                if let Some(p) = util::remove_peer(&mut region, store_id) {
                    // Considering `is_learner` flag in `Peer` here is by design.
                    if &p != peer {
                        error!(
                            "ignore remove unmatched peer";
                            "region_id" => self.region_id(),
                            "peer_id" => self.id(),
                            "expect_peer" => ?peer,
                            "get_peeer" => ?p
                        );
                        return Err(box_err!(
                            "remove unmatched peer: expect: {:?}, get {:?}, ignore",
                            peer,
                            p
                        ));
                    }
                    if self.id == peer.get_id() {
                        // Remove ourself, we will destroy all region data later.
                        // So we need not to apply following logs.
                        self.stopped = true;
                        self.pending_remove = true;
                    }
                } else {
                    error!(
                        "remove missing peer";
                        "region_id" => self.region_id(),
                        "peer_id" => self.id(),
                        "peer" => ?peer,
                        "region" => ?&self.region,
                    );
                    return Err(box_err!(
                        "remove missing peer {:?} from region {:?}",
                        peer,
                        self.region
                    ));
                }

                PEER_ADMIN_CMD_COUNTER_VEC
                    .with_label_values(&["remove_peer", "success"])
                    .inc();
                info!(
                    "remove peer successfully";
                    "region_id" => self.region_id(),
                    "peer_id" => self.id(),
                    "peer" => ?peer,
                    "region" => ?&self.region
                );
            }
            ConfChangeType::AddLearnerNode => {
                PEER_ADMIN_CMD_COUNTER_VEC
                    .with_label_values(&["add_learner", "all"])
                    .inc();

                if util::find_peer(&region, store_id).is_some() {
                    error!(
                        "can't add duplicated learner";
                        "region_id" => self.region_id(),
                        "peer_id" => self.id(),
                        "peer" => ?peer,
                        "region" => ?&self.region
                    );
                    return Err(box_err!(
                        "can't add duplicated learner {:?} to region {:?}",
                        peer,
                        self.region
                    ));
                }
                region.mut_peers().push(peer.clone());

                PEER_ADMIN_CMD_COUNTER_VEC
                    .with_label_values(&["add_learner", "success"])
                    .inc();
                info!(
                    "add learner successfully";
                    "region_id" => self.region_id(),
                    "peer_id" => self.id(),
                    "peer" => ?peer,
                    "region" => ?&self.region,
                );
            }
        }

        let state = if self.pending_remove {
            PeerState::Tombstone
        } else {
            PeerState::Normal
        };
        if let Err(e) = write_peer_state(ctx.kv_wb_mut(), &region, state, None) {
            panic!("{} failed to update region state: {:?}", self.tag, e);
        }

        let mut resp = AdminResponse::default();
        resp.mut_change_peer().set_region(region.clone());

        Ok((
            resp,
            ApplyResult::Res(ExecResult::ChangePeer(ChangePeer {
                index: ctx.exec_ctx.as_ref().unwrap().index,
                conf_change: Default::default(),
                peer: peer.clone(),
                region,
            })),
        ))
    }

    fn exec_split<W: WriteBatch + WriteBatchVecExt<RocksEngine>>(
        &mut self,
        ctx: &mut ApplyContext<W>,
        req: &AdminRequest,
    ) -> Result<(AdminResponse, ApplyResult)> {
        info!(
            "split is deprecated, redirect to use batch split";
            "region_id" => self.region_id(),
            "peer_id" => self.id(),
        );
        let split = req.get_split().to_owned();
        let mut admin_req = AdminRequest::default();
        admin_req
            .mut_splits()
            .set_right_derive(split.get_right_derive());
        admin_req.mut_splits().mut_requests().push(split);
        // This method is executed only when there are unapplied entries after being restarted.
        // So there will be no callback, it's OK to return a response that does not matched
        // with its request.
        self.exec_batch_split(ctx, &admin_req)
    }

    fn exec_batch_split<W: WriteBatch + WriteBatchVecExt<RocksEngine>>(
        &mut self,
        ctx: &mut ApplyContext<W>,
        req: &AdminRequest,
    ) -> Result<(AdminResponse, ApplyResult)> {
        fail_point!(
            "apply_before_split_1_3",
            { self.id == 3 && self.region_id() == 1 },
            |_| { unreachable!() }
        );

        PEER_ADMIN_CMD_COUNTER.batch_split.all.inc();

        let split_reqs = req.get_splits();
        let right_derive = split_reqs.get_right_derive();
        if split_reqs.get_requests().is_empty() {
            return Err(box_err!("missing split requests"));
        }
        let mut derived = self.region.clone();
        let new_region_cnt = split_reqs.get_requests().len();
        let mut regions = Vec::with_capacity(new_region_cnt + 1);
        let mut keys: VecDeque<Vec<u8>> = VecDeque::with_capacity(new_region_cnt + 1);
        for req in split_reqs.get_requests() {
            let split_key = req.get_split_key();
            if split_key.is_empty() {
                return Err(box_err!("missing split key"));
            }
            if split_key
                <= keys
                    .back()
                    .map_or_else(|| derived.get_start_key(), Vec::as_slice)
            {
                return Err(box_err!("invalid split request: {:?}", split_reqs));
            }
            if req.get_new_peer_ids().len() != derived.get_peers().len() {
                return Err(box_err!(
                    "invalid new peer id count, need {:?}, but got {:?}",
                    derived.get_peers(),
                    req.get_new_peer_ids()
                ));
            }
            keys.push_back(split_key.to_vec());
        }

        util::check_key_in_region(keys.back().unwrap(), &self.region)?;

        info!(
            "split region";
            "region_id" => self.region_id(),
            "peer_id" => self.id(),
            "region" => ?derived,
            "keys" => %KeysInfoFormatter(keys.iter()),
        );
        let new_version = derived.get_region_epoch().get_version() + new_region_cnt as u64;
        derived.mut_region_epoch().set_version(new_version);
        // Note that the split requests only contain ids for new regions, so we need
        // to handle new regions and old region separately.
        if right_derive {
            // So the range of new regions is [old_start_key, split_key1, ..., last_split_key].
            keys.push_front(derived.get_start_key().to_vec());
        } else {
            // So the range of new regions is [split_key1, ..., last_split_key, old_end_key].
            keys.push_back(derived.get_end_key().to_vec());
            derived.set_end_key(keys.front().unwrap().to_vec());
            regions.push(derived.clone());
        }
        let kv_wb_mut = ctx.kv_wb.as_mut().unwrap();
        for req in split_reqs.get_requests() {
            let mut new_region = Region::default();
            // TODO: check new region id validation.
            new_region.set_id(req.get_new_region_id());
            new_region.set_region_epoch(derived.get_region_epoch().to_owned());
            new_region.set_start_key(keys.pop_front().unwrap());
            new_region.set_end_key(keys.front().unwrap().to_vec());
            new_region.set_peers(derived.get_peers().to_vec().into());
            for (peer, peer_id) in new_region
                .mut_peers()
                .iter_mut()
                .zip(req.get_new_peer_ids())
            {
                peer.set_id(*peer_id);
            }
            write_peer_state(kv_wb_mut, &new_region, PeerState::Normal, None)
                .and_then(|_| write_initial_apply_state(kv_wb_mut, new_region.get_id()))
                .unwrap_or_else(|e| {
                    panic!(
                        "{} fails to save split region {:?}: {:?}",
                        self.tag, new_region, e
                    )
                });
            regions.push(new_region);
        }
        if right_derive {
            derived.set_start_key(keys.pop_front().unwrap());
            regions.push(derived.clone());
        }
        write_peer_state(kv_wb_mut, &derived, PeerState::Normal, None).unwrap_or_else(|e| {
            panic!("{} fails to update region {:?}: {:?}", self.tag, derived, e)
        });
        let mut resp = AdminResponse::default();
        resp.mut_splits().set_regions(regions.clone().into());
        PEER_ADMIN_CMD_COUNTER.batch_split.success.inc();

        Ok((
            resp,
            ApplyResult::Res(ExecResult::SplitRegion { regions, derived }),
        ))
    }

    fn exec_prepare_merge<W: WriteBatch + WriteBatchVecExt<RocksEngine>>(
        &mut self,
        ctx: &mut ApplyContext<W>,
        req: &AdminRequest,
    ) -> Result<(AdminResponse, ApplyResult)> {
        fail_point!("apply_before_prepare_merge");

        PEER_ADMIN_CMD_COUNTER.prepare_merge.all.inc();

        let prepare_merge = req.get_prepare_merge();
        let index = prepare_merge.get_min_index();
        let exec_ctx = ctx.exec_ctx.as_ref().unwrap();
        let first_index = peer_storage::first_index(&exec_ctx.apply_state);
        if index < first_index {
            // We filter `CompactLog` command before.
            panic!(
                "{} first index {} > min_index {}, skip pre merge",
                self.tag, first_index, index
            );
        }
        let mut region = self.region.clone();
        let region_version = region.get_region_epoch().get_version() + 1;
        region.mut_region_epoch().set_version(region_version);
        // In theory conf version should not be increased when executing prepare_merge.
        // However, we don't want to do conf change after prepare_merge is committed.
        // This can also be done by iterating all proposal to find if prepare_merge is
        // proposed before proposing conf change, but it make things complicated.
        // Another way is make conf change also check region version, but this is not
        // backward compatible.
        let conf_version = region.get_region_epoch().get_conf_ver() + 1;
        region.mut_region_epoch().set_conf_ver(conf_version);
        let mut merging_state = MergeState::default();
        merging_state.set_min_index(index);
        merging_state.set_target(prepare_merge.get_target().to_owned());
        merging_state.set_commit(exec_ctx.index);
        write_peer_state(
            ctx.kv_wb.as_mut().unwrap(),
            &region,
            PeerState::Merging,
            Some(merging_state.clone()),
        )
        .unwrap_or_else(|e| {
            panic!(
                "{} failed to save merging state {:?} for region {:?}: {:?}",
                self.tag, merging_state, region, e
            )
        });
        fail_point!("apply_after_prepare_merge");
        PEER_ADMIN_CMD_COUNTER.prepare_merge.success.inc();

        Ok((
            AdminResponse::default(),
            ApplyResult::Res(ExecResult::PrepareMerge {
                region,
                state: merging_state,
            }),
        ))
    }

    // The target peer should send missing log entries to the source peer.
    //
    // So, the merge process order would be:
    // 1.   `exec_commit_merge` in target apply fsm and send `CatchUpLogs` to source peer fsm
    // 2.   `on_catch_up_logs_for_merge` in source peer fsm
    // 3.   if the source peer has already executed the corresponding `on_ready_prepare_merge`, set pending_remove and jump to step 6
    // 4.   ... (raft append and apply logs)
    // 5.   `on_ready_prepare_merge` in source peer fsm and set pending_remove (means source region has finished applying all logs)
    // 6.   `logs_up_to_date_for_merge` in source apply fsm (destroy its apply fsm and send Noop to trigger the target apply fsm)
    // 7.   resume `exec_commit_merge` in target apply fsm
    // 8.   `on_ready_commit_merge` in target peer fsm and send `MergeResult` to source peer fsm
    // 9.   `on_merge_result` in source peer fsm (destroy itself)
    fn exec_commit_merge<W: WriteBatch + WriteBatchVecExt<RocksEngine>>(
        &mut self,
        ctx: &mut ApplyContext<W>,
        req: &AdminRequest,
    ) -> Result<(AdminResponse, ApplyResult)> {
        {
            let apply_before_commit_merge = || {
                fail_point!(
                    "apply_before_commit_merge_except_1_4",
                    { self.region_id() == 1 && self.id != 4 },
                    |_| {}
                );
            };
            apply_before_commit_merge();
        }

        PEER_ADMIN_CMD_COUNTER.commit_merge.all.inc();

        let merge = req.get_commit_merge();
        let source_region = merge.get_source();
        let source_region_id = source_region.get_id();

        // No matter whether the source peer has applied to the required index,
        // it's a race to write apply state in both source delegate and target
        // delegate. So asking the source delegate to stop first.
        if self.ready_source_region_id != source_region_id {
            if self.ready_source_region_id != 0 {
                panic!(
                    "{} unexpected ready source region {}, expecting {}",
                    self.tag, self.ready_source_region_id, source_region_id
                );
            }
            info!(
                "asking delegate to stop";
                "region_id" => self.region_id(),
                "peer_id" => self.id(),
                "source_region_id" => source_region_id
            );
            fail_point!("before_handle_catch_up_logs_for_merge");
            // Sends message to the source peer fsm and pause `exec_commit_merge` process
            let logs_up_to_date = Arc::new(AtomicU64::new(0));
            let msg = SignificantMsg::CatchUpLogs(CatchUpLogs {
                target_region_id: self.region_id(),
                merge: merge.to_owned(),
                logs_up_to_date: logs_up_to_date.clone(),
            });
            ctx.notifier
                .notify(source_region_id, PeerMsg::SignificantMsg(msg));
            return Ok((
                AdminResponse::default(),
                ApplyResult::WaitMergeSource(logs_up_to_date),
            ));
        }

        info!(
            "execute CommitMerge";
            "region_id" => self.region_id(),
            "peer_id" => self.id(),
            "commit" => merge.get_commit(),
            "entries" => merge.get_entries().len(),
            "term" => ctx.exec_ctx.as_ref().unwrap().term,
            "index" => ctx.exec_ctx.as_ref().unwrap().index,
            "source_region" => ?source_region
        );

        self.ready_source_region_id = 0;

        let region_state_key = keys::region_state_key(source_region_id);
        let state: RegionLocalState = match ctx.engine.get_msg_cf(CF_RAFT, &region_state_key) {
            Ok(Some(s)) => s,
            e => panic!(
                "{} failed to get regions state of {:?}: {:?}",
                self.tag, source_region, e
            ),
        };
        match state.get_state() {
            PeerState::Normal | PeerState::Merging => {}
            _ => panic!(
                "{} unexpected state of merging region {:?}",
                self.tag, state
            ),
        }
        let exist_region = state.get_region().to_owned();
        if *source_region != exist_region {
            panic!(
                "{} source_region {:?} not match exist region {:?}",
                self.tag, source_region, exist_region
            );
        }
        let mut region = self.region.clone();
        // Use a max value so that pd can ensure overlapped region has a priority.
        let version = cmp::max(
            source_region.get_region_epoch().get_version(),
            region.get_region_epoch().get_version(),
        ) + 1;
        region.mut_region_epoch().set_version(version);
        if keys::enc_end_key(&region) == keys::enc_start_key(source_region) {
            region.set_end_key(source_region.get_end_key().to_vec());
        } else {
            region.set_start_key(source_region.get_start_key().to_vec());
        }
        let kv_wb_mut = ctx.kv_wb.as_mut().unwrap();
        write_peer_state(kv_wb_mut, &region, PeerState::Normal, None)
            .and_then(|_| {
                // TODO: maybe all information needs to be filled?
                let mut merging_state = MergeState::default();
                merging_state.set_target(self.region.clone());
                write_peer_state(
                    kv_wb_mut,
                    source_region,
                    PeerState::Tombstone,
                    Some(merging_state),
                )
            })
            .unwrap_or_else(|e| {
                panic!(
                    "{} failed to save merge region {:?}: {:?}",
                    self.tag, region, e
                )
            });

        PEER_ADMIN_CMD_COUNTER.commit_merge.success.inc();

        let resp = AdminResponse::default();
        Ok((
            resp,
            ApplyResult::Res(ExecResult::CommitMerge {
                region,
                source: source_region.to_owned(),
            }),
        ))
    }

    fn exec_rollback_merge<W: WriteBatch + WriteBatchVecExt<RocksEngine>>(
        &mut self,
        ctx: &mut ApplyContext<W>,
        req: &AdminRequest,
    ) -> Result<(AdminResponse, ApplyResult)> {
        PEER_ADMIN_CMD_COUNTER.rollback_merge.all.inc();
        let region_state_key = keys::region_state_key(self.region_id());
        let state: RegionLocalState = match ctx.engine.get_msg_cf(CF_RAFT, &region_state_key) {
            Ok(Some(s)) => s,
            e => panic!("{} failed to get regions state: {:?}", self.tag, e),
        };
        assert_eq!(state.get_state(), PeerState::Merging, "{}", self.tag);
        let rollback = req.get_rollback_merge();
        assert_eq!(
            state.get_merge_state().get_commit(),
            rollback.get_commit(),
            "{}",
            self.tag
        );
        let mut region = self.region.clone();
        let version = region.get_region_epoch().get_version();
        // Update version to avoid duplicated rollback requests.
        region.mut_region_epoch().set_version(version + 1);
        let kv_wb_mut = ctx.kv_wb.as_mut().unwrap();
        write_peer_state(kv_wb_mut, &region, PeerState::Normal, None).unwrap_or_else(|e| {
            panic!(
                "{} failed to rollback merge {:?}: {:?}",
                self.tag, rollback, e
            )
        });

        PEER_ADMIN_CMD_COUNTER.rollback_merge.success.inc();
        let resp = AdminResponse::default();
        Ok((
            resp,
            ApplyResult::Res(ExecResult::RollbackMerge {
                region,
                commit: rollback.get_commit(),
            }),
        ))
    }

    fn exec_compact_log<W: WriteBatch + WriteBatchVecExt<RocksEngine>>(
        &mut self,
        ctx: &mut ApplyContext<W>,
        req: &AdminRequest,
    ) -> Result<(AdminResponse, ApplyResult)> {
        PEER_ADMIN_CMD_COUNTER.compact.all.inc();

        let compact_index = req.get_compact_log().get_compact_index();
        let resp = AdminResponse::default();
        let apply_state = &mut ctx.exec_ctx.as_mut().unwrap().apply_state;
        let first_index = peer_storage::first_index(apply_state);
        if compact_index <= first_index {
            debug!(
                "compact index <= first index, no need to compact";
                "region_id" => self.region_id(),
                "peer_id" => self.id(),
                "compact_index" => compact_index,
                "first_index" => first_index,
            );
            return Ok((resp, ApplyResult::None));
        }
        if self.is_merging {
            info!(
                "in merging mode, skip compact";
                "region_id" => self.region_id(),
                "peer_id" => self.id(),
                "compact_index" => compact_index
            );
            return Ok((resp, ApplyResult::None));
        }

        let compact_term = req.get_compact_log().get_compact_term();
        // TODO: add unit tests to cover all the message integrity checks.
        if compact_term == 0 {
            info!(
                "compact term missing, skip";
                "region_id" => self.region_id(),
                "peer_id" => self.id(),
                "command" => ?req.get_compact_log()
            );
            // old format compact log command, safe to ignore.
            return Err(box_err!(
                "command format is outdated, please upgrade leader"
            ));
        }

        // compact failure is safe to be omitted, no need to assert.
        compact_raft_log(&self.tag, apply_state, compact_index, compact_term)?;

        PEER_ADMIN_CMD_COUNTER.compact.success.inc();

        Ok((
            resp,
            ApplyResult::Res(ExecResult::CompactLog {
                state: apply_state.get_truncated_state().clone(),
                first_index,
            }),
        ))
    }

    fn exec_compute_hash<W: WriteBatch + WriteBatchVecExt<RocksEngine>>(
        &self,
        ctx: &ApplyContext<W>,
        _: &AdminRequest,
    ) -> Result<(AdminResponse, ApplyResult)> {
        let resp = AdminResponse::default();
        Ok((
            resp,
            ApplyResult::Res(ExecResult::ComputeHash {
                region: self.region.clone(),
                index: ctx.exec_ctx.as_ref().unwrap().index,
                // This snapshot may be held for a long time, which may cause too many
                // open files in rocksdb.
                // TODO: figure out another way to do consistency check without snapshot
                // or short life snapshot.
                snap: ctx.engine.snapshot(),
            }),
        ))
    }

    fn exec_verify_hash<W: WriteBatch + WriteBatchVecExt<RocksEngine>>(
        &self,
        _: &ApplyContext<W>,
        req: &AdminRequest,
    ) -> Result<(AdminResponse, ApplyResult)> {
        let verify_req = req.get_verify_hash();
        let index = verify_req.get_index();
        let hash = verify_req.get_hash().to_vec();
        let resp = AdminResponse::default();
        Ok((
            resp,
            ApplyResult::Res(ExecResult::VerifyHash { index, hash }),
        ))
    }
}

pub fn get_change_peer_cmd(msg: &RaftCmdRequest) -> Option<&ChangePeerRequest> {
    if !msg.has_admin_request() {
        return None;
    }
    let req = msg.get_admin_request();
    if !req.has_change_peer() {
        return None;
    }

    Some(req.get_change_peer())
}

fn check_sst_for_ingestion(sst: &SstMeta, region: &Region) -> Result<()> {
    let uuid = sst.get_uuid();
    if let Err(e) = UuidBuilder::from_slice(uuid) {
        return Err(box_err!("invalid uuid {:?}: {:?}", uuid, e));
    }

    let cf_name = sst.get_cf_name();
    if cf_name != CF_DEFAULT && cf_name != CF_WRITE {
        return Err(box_err!("invalid cf name {}", cf_name));
    }

    let region_id = sst.get_region_id();
    if region_id != region.get_id() {
        return Err(Error::RegionNotFound(region_id));
    }

    let epoch = sst.get_region_epoch();
    let region_epoch = region.get_region_epoch();
    if epoch.get_conf_ver() != region_epoch.get_conf_ver()
        || epoch.get_version() != region_epoch.get_version()
    {
        let error = format!("{:?} != {:?}", epoch, region_epoch);
        return Err(Error::EpochNotMatch(error, vec![region.clone()]));
    }

    let range = sst.get_range();
    util::check_key_in_region(range.get_start(), region)?;
    util::check_key_in_region(range.get_end(), region)?;

    Ok(())
}

/// Updates the `state` with given `compact_index` and `compact_term`.
///
/// Remember the Raft log is not deleted here.
pub fn compact_raft_log(
    tag: &str,
    state: &mut RaftApplyState,
    compact_index: u64,
    compact_term: u64,
) -> Result<()> {
    debug!("{} compact log entries to prior to {}", tag, compact_index);

    if compact_index <= state.get_truncated_state().get_index() {
        return Err(box_err!("try to truncate compacted entries"));
    } else if compact_index > state.get_applied_index() {
        return Err(box_err!(
            "compact index {} > applied index {}",
            compact_index,
            state.get_applied_index()
        ));
    }

    // we don't actually delete the logs now, we add an async task to do it.

    state.mut_truncated_state().set_index(compact_index);
    state.mut_truncated_state().set_term(compact_term);

    Ok(())
}

pub struct Apply {
    pub region_id: u64,
    pub term: u64,
    pub entries: Vec<Entry>,
    pub last_commit_index: u64,
    pub commit_index: u64,
    pub commit_term: u64,
}

impl Apply {
    pub fn new(
        region_id: u64,
        term: u64,
        entries: Vec<Entry>,
        last_commit_index: u64,
        commit_term: u64,
        commit_index: u64,
    ) -> Apply {
        Apply {
            region_id,
            term,
            entries,
            last_commit_index,
            commit_index,
            commit_term,
        }
    }
}

#[derive(Default, Clone)]
pub struct Registration {
    pub id: u64,
    pub term: u64,
    pub apply_state: RaftApplyState,
    pub applied_index_term: u64,
    pub region: Region,
    pub pending_request_snapshot_count: Arc<AtomicUsize>,
    pub is_merging: bool,
}

impl Registration {
    pub fn new(peer: &Peer) -> Registration {
        Registration {
            id: peer.peer_id(),
            term: peer.term(),
            apply_state: peer.get_store().apply_state().clone(),
            applied_index_term: peer.get_store().applied_index_term(),
            region: peer.region().clone(),
            pending_request_snapshot_count: peer.pending_request_snapshot_count.clone(),
            is_merging: peer.pending_merge_state.is_some(),
        }
    }
}

pub struct Proposal {
    is_conf_change: bool,
    index: u64,
    term: u64,
    pub cb: Callback<RocksEngine>,
}

impl Proposal {
    pub fn new(is_conf_change: bool, index: u64, term: u64, cb: Callback<RocksEngine>) -> Proposal {
        Proposal {
            is_conf_change,
            index,
            term,
            cb,
        }
    }
}

pub struct RegionProposal {
    pub id: u64,
    pub region_id: u64,
    pub props: Vec<Proposal>,
}

impl RegionProposal {
    pub fn new(id: u64, region_id: u64, props: Vec<Proposal>) -> RegionProposal {
        RegionProposal {
            id,
            region_id,
            props,
        }
    }
}

pub struct Destroy {
    region_id: u64,
    async_remove: bool,
}

/// A message that asks the delegate to apply to the given logs and then reply to
/// target mailbox.
#[derive(Default, Debug)]
pub struct CatchUpLogs {
    /// The target region to be notified when given logs are applied.
    pub target_region_id: u64,
    /// Merge request that contains logs to be applied.
    pub merge: CommitMergeRequest,
    /// A flag indicate that all source region's logs are applied.
    ///
    /// This is still necessary although we have a mailbox field already.
    /// Mailbox is used to notify target region, and trigger a round of polling.
    /// But due to the FIFO natural of channel, we need a flag to check if it's
    /// ready when polling.
    pub logs_up_to_date: Arc<AtomicU64>,
}

pub struct GenSnapTask {
    pub(crate) region_id: u64,
    //pub(crate) peer_id: u64,
    commit_index: u64,
    snap_notifier: SyncSender<RaftSnapshot>,
}

impl GenSnapTask {
    pub fn new(
        region_id: u64,
        //peer_id: u64,
        commit_index: u64,
        snap_notifier: SyncSender<RaftSnapshot>,
    ) -> GenSnapTask {
        GenSnapTask {
            region_id,
            //peer_id,
            commit_index,
            snap_notifier,
        }
    }

    pub fn commit_index(&self) -> u64 {
        self.commit_index
    }

    pub fn generate_and_schedule_snapshot(
        self,
        kv_snap: RocksSnapshot,
        last_applied_index_term: u64,
        last_applied_state: RaftApplyState,
        region_sched: &Scheduler<RegionTask>,
    ) -> Result<()> {
        let snapshot = RegionTask::Gen {
            region_id: self.region_id,
            notifier: self.snap_notifier,
            last_applied_index_term,
            last_applied_state,
            // This snapshot may be held for a long time, which may cause too many
            // open files in rocksdb.
            kv_snap,
        };
        box_try!(region_sched.schedule(snapshot));
        Ok(())
    }
}

impl Debug for GenSnapTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GenSnapTask")
            .field("region_id", &self.region_id)
            .field("commit_index", &self.commit_index)
            .finish()
    }
}

static OBSERVE_ID_ALLOC: AtomicUsize = AtomicUsize::new(0);

/// A unique identifier for checking stale observed commands.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ObserveID(usize);

impl ObserveID {
    pub fn new() -> ObserveID {
        ObserveID(OBSERVE_ID_ALLOC.fetch_add(1, Ordering::Relaxed))
    }
}

struct ObserveCmd {
    id: ObserveID,
    enabled: Arc<AtomicBool>,
}

impl Debug for ObserveCmd {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("ObserveCmd").field("id", &self.id).finish()
    }
}

#[derive(Debug)]
pub enum ChangeCmd {
    RegisterObserver {
        observe_id: ObserveID,
        region_id: u64,
        enabled: Arc<AtomicBool>,
    },
    Snapshot {
        observe_id: ObserveID,
        region_id: u64,
    },
}

pub enum Msg {
    Apply {
        start: Instant,
        apply: Apply,
    },
    Registration(Registration),
    Proposal(RegionProposal),
    LogsUpToDate(CatchUpLogs),
    Noop,
    Destroy(Destroy),
    Change {
        cmd: ChangeCmd,
        region_epoch: RegionEpoch,
        cb: Callback<RocksEngine>,
    },
    Snapshot {
        cb: SnapshotCallback<RocksEngine>,
        region_id: u64,
        sync: bool,
    },
    #[cfg(any(test, feature = "testexport"))]
    Validate(u64, Box<dyn FnOnce((&ApplyDelegate, bool)) + Send>),
}

impl Msg {
    pub fn apply(apply: Apply) -> Msg {
        Msg::Apply {
            start: Instant::now(),
            apply,
        }
    }

    pub fn register(peer: &Peer) -> Msg {
        Msg::Registration(Registration::new(peer))
    }

    pub fn destroy(region_id: u64, async_remove: bool) -> Msg {
        Msg::Destroy(Destroy {
            region_id,
            async_remove,
        })
    }
}

impl Debug for Msg {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Msg::Apply { apply, .. } => write!(f, "[region {}] async apply", apply.region_id),
            Msg::Proposal(ref p) => write!(f, "[region {}] {} region proposal", p.region_id, p.id),
            Msg::Registration(ref r) => {
                write!(f, "[region {}] Reg {:?}", r.region.get_id(), r.apply_state)
            }
            Msg::LogsUpToDate(_) => write!(f, "logs are updated"),
            Msg::Noop => write!(f, "noop"),
            Msg::Destroy(ref d) => write!(f, "[region {}] destroy", d.region_id),
            Msg::Snapshot { region_id, .. } => {
                write!(f, "[region {}] requests a snapshot", region_id)
            }
            Msg::Change {
                cmd: ChangeCmd::RegisterObserver { region_id, .. },
                ..
            } => write!(f, "[region {}] registers cmd observer", region_id),
            Msg::Change {
                cmd: ChangeCmd::Snapshot { region_id, .. },
                ..
            } => write!(f, "[region {}] cmd snapshot", region_id),
            #[cfg(any(test, feature = "testexport"))]
            Msg::Validate(region_id, _) => write!(f, "[region {}] validate", region_id),
        }
    }
}

#[derive(Default, Clone, Debug, PartialEq)]
pub struct ApplyMetrics {
    /// an inaccurate difference in region size since last reset.
    pub size_diff_hint: i64,
    /// delete keys' count since last reset.
    pub delete_keys_hint: u64,

    pub written_bytes: u64,
    pub written_keys: u64,
    pub lock_cf_written_bytes: u64,
}

#[derive(Debug)]
pub struct ApplyRes {
    pub region_id: u64,
    pub apply_state: RaftApplyState,
    pub applied_index_term: u64,
    pub exec_res: VecDeque<ExecResult>,
    pub metrics: ApplyMetrics,
}

#[derive(Debug)]
pub enum TaskRes {
    Apply(ApplyRes),
    Destroy {
        // ID of region that has been destroyed.
        region_id: u64,
        // ID of peer that has been destroyed.
        peer_id: u64,
    },
}

pub struct ApplyFsm {
    delegate: ApplyDelegate,
    receiver: Receiver<Msg>,
    mailbox: Option<BasicMailbox<ApplyFsm>>,
}

impl ApplyFsm {
    fn from_peer(peer: &Peer) -> (LooseBoundedSender<Msg>, Box<ApplyFsm>) {
        let reg = Registration::new(peer);
        ApplyFsm::from_registration(reg)
    }

    fn from_registration(reg: Registration) -> (LooseBoundedSender<Msg>, Box<ApplyFsm>) {
        let (tx, rx) = loose_bounded(usize::MAX);
        let delegate = ApplyDelegate::from_registration(reg);
        (
            tx,
            Box::new(ApplyFsm {
                delegate,
                receiver: rx,
                mailbox: None,
            }),
        )
    }

    /// Handles peer registration. When a peer is created, it will register an apply delegate.
    fn handle_registration(&mut self, reg: Registration) {
        info!(
            "re-register to apply delegates";
            "region_id" => self.delegate.region_id(),
            "peer_id" => self.delegate.id(),
            "term" => reg.term
        );
        assert_eq!(self.delegate.id, reg.id);
        self.delegate.term = reg.term;
        self.delegate.clear_all_commands_as_stale();
        self.delegate = ApplyDelegate::from_registration(reg);
    }

    /// Handles apply tasks, and uses the apply delegate to handle the committed entries.
    fn handle_apply<W: WriteBatch + WriteBatchVecExt<RocksEngine>>(
        &mut self,
        apply_ctx: &mut ApplyContext<W>,
        apply: Apply,
    ) {
        if apply_ctx.timer.is_none() {
            apply_ctx.timer = Some(Instant::now_coarse());
        }

        fail_point!("on_handle_apply_1003", self.delegate.id() == 1003, |_| {});
        fail_point!("on_handle_apply", |_| {});

        if apply.entries.is_empty() || self.delegate.pending_remove || self.delegate.stopped {
            return;
        }

        self.delegate.metrics = ApplyMetrics::default();
        self.delegate.term = apply.term;
        let prev_state = (
            self.delegate.apply_state.get_last_commit_index(),
            self.delegate.apply_state.get_commit_index(),
            self.delegate.apply_state.get_commit_term(),
        );
        let cur_state = (
            apply.last_commit_index,
            apply.commit_index,
            apply.commit_term,
        );
        if prev_state.0 > cur_state.0 || prev_state.1 > cur_state.1 || prev_state.2 > cur_state.2 {
            panic!(
                "{} commit state jump backward {:?} -> {:?}",
                self.delegate.tag, prev_state, cur_state
            );
        }
        // The apply state may not be written to disk if entries is empty,
        // which seems OK.
        self.delegate.apply_state.set_last_commit_index(cur_state.0);
        self.delegate.apply_state.set_commit_index(cur_state.1);
        self.delegate.apply_state.set_commit_term(cur_state.2);

        self.delegate
            .handle_raft_committed_entries(apply_ctx, apply.entries);
        if self.delegate.yield_state.is_some() {
            return;
        }
    }

    /// Handles proposals, and appends the commands to the apply delegate.
    fn handle_proposal(&mut self, region_proposal: RegionProposal) {
        let (region_id, peer_id) = (self.delegate.region_id(), self.delegate.id());
        let propose_num = region_proposal.props.len();
        assert_eq!(self.delegate.id, region_proposal.id);
        if self.delegate.stopped {
            for p in region_proposal.props {
                let cmd = PendingCmd::new(p.index, p.term, p.cb);
                notify_stale_command(region_id, peer_id, self.delegate.term, cmd);
            }
            return;
        }
        for p in region_proposal.props {
            let cmd = PendingCmd::new(p.index, p.term, p.cb);
            if p.is_conf_change {
                if let Some(cmd) = self.delegate.pending_cmds.take_conf_change() {
                    // if it loses leadership before conf change is replicated, there may be
                    // a stale pending conf change before next conf change is applied. If it
                    // becomes leader again with the stale pending conf change, will enter
                    // this block, so we notify leadership may have been changed.
                    notify_stale_command(region_id, peer_id, self.delegate.term, cmd);
                }
                self.delegate.pending_cmds.set_conf_change(cmd);
            } else {
                self.delegate.pending_cmds.append_normal(cmd);
            }
        }
        // TODO: observe it in batch.
        APPLY_PROPOSAL.observe(propose_num as f64);
    }

    fn destroy<W: WriteBatch + WriteBatchVecExt<RocksEngine>>(
        &mut self,
        ctx: &mut ApplyContext<W>,
    ) {
        let region_id = self.delegate.region_id();
        if ctx.apply_res.iter().any(|res| res.region_id == region_id) {
            // Flush before destroying to avoid reordering messages.
            ctx.flush();
        }
        fail_point!(
            "before_peer_destroy_1003",
            self.delegate.id() == 1003,
            |_| {}
        );
        info!(
            "remove delegate from apply delegates";
            "region_id" => self.delegate.region_id(),
            "peer_id" => self.delegate.id(),
        );
        self.delegate.destroy(ctx);
    }

    /// Handles peer destroy. When a peer is destroyed, the corresponding apply delegate should be removed too.
    fn handle_destroy<W: WriteBatch + WriteBatchVecExt<RocksEngine>>(
        &mut self,
        ctx: &mut ApplyContext<W>,
        d: Destroy,
    ) {
        assert_eq!(d.region_id, self.delegate.region_id());
        if !self.delegate.stopped {
            self.destroy(ctx);
            if d.async_remove {
                ctx.notifier.notify(
                    self.delegate.region_id(),
                    PeerMsg::ApplyRes {
                        res: TaskRes::Destroy {
                            region_id: self.delegate.region_id(),
                            peer_id: self.delegate.id,
                        },
                    },
                );
            }
        }
    }

    fn resume_pending<W: WriteBatch + WriteBatchVecExt<RocksEngine>>(
        &mut self,
        ctx: &mut ApplyContext<W>,
    ) -> bool {
        if let Some(ref state) = self.delegate.wait_merge_state {
            let source_region_id = state.logs_up_to_date.load(Ordering::SeqCst);
            if source_region_id == 0 {
                return false;
            }
            self.delegate.ready_source_region_id = source_region_id;
        }
        self.delegate.wait_merge_state = None;

        let mut state = self.delegate.yield_state.take().unwrap();

        if ctx.timer.is_none() {
            ctx.timer = Some(Instant::now_coarse());
        }
        if !state.pending_entries.is_empty() {
            self.delegate
                .handle_raft_committed_entries(ctx, state.pending_entries);
            if let Some(ref mut s) = self.delegate.yield_state {
                // So the delegate is expected to yield the CPU.
                // It can either be executing another `CommitMerge` in pending_msgs
                // or has been written too much data.
                s.pending_msgs = state.pending_msgs;
                return false;
            }
        }

        if !state.pending_msgs.is_empty() {
            self.handle_tasks(ctx, &mut state.pending_msgs);
        }

        if self.delegate.yield_state.is_some() {
            return false;
        }

        true
    }

    fn logs_up_to_date_for_merge<W: WriteBatch + WriteBatchVecExt<RocksEngine>>(
        &mut self,
        ctx: &mut ApplyContext<W>,
        catch_up_logs: CatchUpLogs,
    ) {
        fail_point!("after_handle_catch_up_logs_for_merge");
        fail_point!(
            "after_handle_catch_up_logs_for_merge_1003",
            self.delegate.id() == 1003,
            |_| {}
        );

        let region_id = self.delegate.region_id();
        info!(
            "source logs are all applied now";
            "region_id" => region_id,
            "peer_id" => self.delegate.id(),
        );
        // The source peer fsm will be destroyed when the target peer executes `on_ready_commit_merge`
        // and sends `merge result` to the source peer fsm.
        self.destroy(ctx);
        catch_up_logs
            .logs_up_to_date
            .store(region_id, Ordering::SeqCst);
        // To trigger the target apply fsm
        if let Some(mailbox) = ctx.router.mailbox(catch_up_logs.target_region_id) {
            let _ = mailbox.force_send(Msg::Noop);
        } else {
            error!(
                "failed to get mailbox, are we shutting down?";
                "region_id" => region_id,
                "peer_id" => self.delegate.id(),
            );
        }
    }

    #[allow(unused_mut)]
    fn handle_snapshot<W: WriteBatch + WriteBatchVecExt<RocksEngine>>(
        &mut self,
        apply_ctx: &mut ApplyContext<W>,
        cb: SnapshotCallback<RocksEngine>,
        sync: bool,
    ) {
        let applied_index = self.delegate.apply_state.get_applied_index();
        if self.delegate.pending_remove || self.delegate.stopped {
            let e = Err(box_err!("transfer leader won't exec"));
            cb(
                e,
                &self.delegate.region,
                &self.delegate.apply_state,
                self.delegate.applied_index_term,
            );
            return;
        }
        let mut need_sync = sync;
        if need_sync {
            need_sync = apply_ctx
                .apply_res
                .iter()
                .any(|res| res.region_id == self.delegate.region_id())
                && self.delegate.last_sync_apply_index != applied_index;
        }
        if need_sync {
            if apply_ctx.timer.is_none() {
                apply_ctx.timer = Some(Instant::now_coarse());
            }
            apply_ctx.prepare_write_batch();
            self.delegate.write_apply_state(apply_ctx.kv_wb_mut());
            fail_point!(
                "apply_on_handle_snapshot_1_1",
                self.delegate.id == 1 && self.delegate.region_id() == 1,
                |_| unimplemented!()
            );

            apply_ctx.flush();
            // For now, it's more like last_flush_apply_index.
            // TODO: Update it only when `flush()` returns true.
            self.delegate.last_sync_apply_index = applied_index;
        }
        let snap = apply_ctx.engine.snapshot();
        cb(
            Ok(snap),
            &self.delegate.region,
            &self.delegate.apply_state,
            self.delegate.applied_index_term,
        );
        fail_point!(
            "apply_on_handle_snapshot_finish_1_1",
            self.delegate.id == 1 && self.delegate.region_id() == 1,
            |_| unimplemented!()
        );
    }

    fn handle_change<W: WriteBatch + WriteBatchVecExt<RocksEngine>>(
        &mut self,
        apply_ctx: &mut ApplyContext<W>,
        cmd: ChangeCmd,
        region_epoch: RegionEpoch,
        cb: Callback<RocksEngine>,
    ) {
        let (observe_id, region_id, enabled) = match cmd {
            ChangeCmd::RegisterObserver {
                observe_id,
                region_id,
                enabled,
            } => {
                assert!(!self
                    .delegate
                    .observe_cmd
                    .as_ref()
                    .map_or(false, |o| o.enabled.load(Ordering::SeqCst)));
                (observe_id, region_id, Some(enabled))
            }
            ChangeCmd::Snapshot {
                observe_id,
                region_id,
            } => (observe_id, region_id, None),
        };

        assert_eq!(self.delegate.region_id(), region_id);
        let resp = match compare_region_epoch(
            &region_epoch,
            &self.delegate.region,
            false, /* check_conf_ver */
            true,  /* check_ver */
            true,  /* include_region */
        ) {
            Ok(()) => {
                // Commit the writebatch for ensuring the following snapshot can get all previous writes.
                if apply_ctx.kv_wb.is_some() && apply_ctx.kv_wb().count() > 0 {
                    apply_ctx.commit(&mut self.delegate);
                }
                ReadResponse {
                    response: Default::default(),
                    snapshot: Some(RegionSnapshot::<RocksEngine>::from_snapshot(
                        apply_ctx.engine.snapshot().into_sync(),
                        self.delegate.region.clone(),
                    )),
                }
            }
            Err(e) => ReadResponse {
                response: cmd_resp::new_error(e),
                snapshot: None,
            },
        };
        if let Some(enabled) = enabled {
            // TODO(cdc): take observe_cmd when enabled is false.
            self.delegate.observe_cmd = Some(ObserveCmd {
                id: observe_id,
                enabled,
            });
        } else if let Some(observe_cmd) = self.delegate.observe_cmd.as_mut() {
            observe_cmd.id = observe_id;
        }

        cb.invoke_read(resp);
    }

    fn handle_tasks<W: WriteBatch + WriteBatchVecExt<RocksEngine>>(
        &mut self,
        apply_ctx: &mut ApplyContext<W>,
        msgs: &mut Vec<Msg>,
    ) {
        let mut channel_timer = None;
        let mut drainer = msgs.drain(..);
        loop {
            match drainer.next() {
                Some(Msg::Apply { start, apply }) => {
                    if channel_timer.is_none() {
                        channel_timer = Some(start);
                    }
                    self.handle_apply(apply_ctx, apply);
                    if let Some(ref mut state) = self.delegate.yield_state {
                        state.pending_msgs = drainer.collect();
                        break;
                    }
                }
                Some(Msg::Proposal(prop)) => self.handle_proposal(prop),
                Some(Msg::Registration(reg)) => self.handle_registration(reg),
                Some(Msg::Destroy(d)) => self.handle_destroy(apply_ctx, d),
                Some(Msg::LogsUpToDate(cul)) => self.logs_up_to_date_for_merge(apply_ctx, cul),
                Some(Msg::Noop) => {}
                Some(Msg::Snapshot { cb, sync, .. }) => self.handle_snapshot(apply_ctx, cb, sync),
                Some(Msg::Change {
                    cmd,
                    region_epoch,
                    cb,
                }) => self.handle_change(apply_ctx, cmd, region_epoch, cb),
                #[cfg(any(test, feature = "testexport"))]
                Some(Msg::Validate(_, f)) => f((&self.delegate, apply_ctx.enable_sync_log)),
                None => break,
            }
        }
        if let Some(timer) = channel_timer {
            let elapsed = duration_to_sec(timer.elapsed());
            APPLY_TASK_WAIT_TIME_HISTOGRAM.observe(elapsed);
        }
    }
}

impl Fsm for ApplyFsm {
    type Message = Msg;

    #[inline]
    fn is_stopped(&self) -> bool {
        self.delegate.stopped
    }

    #[inline]
    fn set_mailbox(&mut self, mailbox: Cow<'_, BasicMailbox<Self>>)
    where
        Self: Sized,
    {
        self.mailbox = Some(mailbox.into_owned());
    }

    #[inline]
    fn take_mailbox(&mut self) -> Option<BasicMailbox<Self>>
    where
        Self: Sized,
    {
        self.mailbox.take()
    }
}

impl Drop for ApplyFsm {
    fn drop(&mut self) {
        self.delegate.clear_all_commands_as_stale();
    }
}

pub struct ControlMsg;

pub struct ControlFsm;

impl Fsm for ControlFsm {
    type Message = ControlMsg;

    #[inline]
    fn is_stopped(&self) -> bool {
        true
    }
}

pub struct ApplyPoller<W: WriteBatch + WriteBatchVecExt<RocksEngine>> {
    msg_buf: Vec<Msg>,
    apply_ctx: ApplyContext<W>,
    messages_per_tick: usize,
    cfg_tracker: Tracker<Config>,
}

impl<W: WriteBatch + WriteBatchVecExt<RocksEngine>> PollHandler<ApplyFsm, ControlFsm>
    for ApplyPoller<W>
{
    fn begin(&mut self, _batch_size: usize) {
        if let Some(incoming) = self.cfg_tracker.any_new() {
            match Ord::cmp(&incoming.messages_per_tick, &self.messages_per_tick) {
                CmpOrdering::Greater => {
                    self.msg_buf.reserve(incoming.messages_per_tick);
                    self.messages_per_tick = incoming.messages_per_tick;
                }
                CmpOrdering::Less => {
                    self.msg_buf.shrink_to(incoming.messages_per_tick);
                    self.messages_per_tick = incoming.messages_per_tick;
                }
                _ => {}
            }
            self.apply_ctx.enable_sync_log = incoming.sync_log;
        }
    }

    /// There is no control fsm in apply poller.
    fn handle_control(&mut self, _: &mut ControlFsm) -> Option<usize> {
        unimplemented!()
    }

    fn handle_normal(&mut self, normal: &mut ApplyFsm) -> Option<usize> {
        let mut expected_msg_count = None;
        normal.delegate.written = false;
        if normal.delegate.yield_state.is_some() {
            if normal.delegate.wait_merge_state.is_some() {
                // We need to query the length first, otherwise there is a race
                // condition that new messages are queued after resuming and before
                // query the length.
                expected_msg_count = Some(normal.receiver.len());
            }
            if !normal.resume_pending(&mut self.apply_ctx) {
                return expected_msg_count;
            }
            expected_msg_count = None;
        }
        while self.msg_buf.len() < self.messages_per_tick {
            match normal.receiver.try_recv() {
                Ok(msg) => self.msg_buf.push(msg),
                Err(TryRecvError::Empty) => {
                    expected_msg_count = Some(0);
                    break;
                }
                Err(TryRecvError::Disconnected) => {
                    normal.delegate.stopped = true;
                    expected_msg_count = Some(0);
                    break;
                }
            }
        }
        normal.handle_tasks(&mut self.apply_ctx, &mut self.msg_buf);
        if normal.delegate.wait_merge_state.is_some() {
            // Check it again immediately as catching up logs can be very fast.
            expected_msg_count = Some(0);
        } else if normal.delegate.yield_state.is_some() {
            // Let it continue to run next time.
            expected_msg_count = None;
        }
        expected_msg_count
    }

    fn end(&mut self, fsms: &mut [Box<ApplyFsm>]) {
        let is_synced = self.apply_ctx.flush();
        if is_synced {
            for fsm in fsms {
                fsm.delegate.last_sync_apply_index = fsm.delegate.apply_state.get_applied_index();
            }
        }
    }
}

pub struct Builder<W: WriteBatch + WriteBatchVecExt<RocksEngine>> {
    tag: String,
    cfg: Arc<VersionTrack<Config>>,
    coprocessor_host: CoprocessorHost,
    importer: Arc<SSTImporter>,
    engine: RocksEngine,
    sender: Notifier,
    router: ApplyRouter,
    _phantom: PhantomData<W>,
}

impl<W: WriteBatch + WriteBatchVecExt<RocksEngine>> Builder<W> {
    pub fn new<T, C>(
        builder: &RaftPollerBuilder<T, C>,
        sender: Notifier,
        router: ApplyRouter,
    ) -> Builder<W> {
        Builder::<W> {
            tag: format!("[store {}]", builder.store.get_id()),
            cfg: builder.cfg.clone(),
            coprocessor_host: builder.coprocessor_host.clone(),
            importer: builder.importer.clone(),
            engine: builder.engines.kv.clone(),
            _phantom: PhantomData,
            sender,
            router,
        }
    }
}

impl<W: WriteBatch + WriteBatchVecExt<RocksEngine>> HandlerBuilder<ApplyFsm, ControlFsm>
    for Builder<W>
{
    type Handler = ApplyPoller<W>;

    fn build(&mut self) -> ApplyPoller<W> {
        let cfg = self.cfg.value();
        ApplyPoller::<W> {
            msg_buf: Vec::with_capacity(cfg.messages_per_tick),
            apply_ctx: ApplyContext::new(
                self.tag.clone(),
                self.coprocessor_host.clone(),
                self.importer.clone(),
                self.engine.clone(),
                self.router.clone(),
                self.sender.clone(),
                &cfg,
            ),
            messages_per_tick: cfg.messages_per_tick,
            cfg_tracker: self.cfg.clone().tracker(self.tag.clone()),
        }
    }
}

#[derive(Clone)]
pub struct ApplyRouter {
    pub router: BatchRouter<ApplyFsm, ControlFsm>,
}

impl Deref for ApplyRouter {
    type Target = BatchRouter<ApplyFsm, ControlFsm>;

    fn deref(&self) -> &BatchRouter<ApplyFsm, ControlFsm> {
        &self.router
    }
}

impl DerefMut for ApplyRouter {
    fn deref_mut(&mut self) -> &mut BatchRouter<ApplyFsm, ControlFsm> {
        &mut self.router
    }
}

impl ApplyRouter {
    pub fn schedule_task(&self, region_id: u64, msg: Msg) {
        let reg = match self.try_send(region_id, msg) {
            Either::Left(Ok(())) => return,
            Either::Left(Err(TrySendError::Disconnected(msg))) | Either::Right(msg) => match msg {
                Msg::Registration(reg) => reg,
                Msg::Proposal(props) => {
                    info!(
                        "target region is not found, drop proposals";
                        "region_id" => region_id
                    );
                    for p in props.props {
                        let cmd = PendingCmd::new(p.index, p.term, p.cb);
                        notify_region_removed(props.region_id, props.id, cmd);
                    }
                    return;
                }
                Msg::Apply { .. } | Msg::Destroy(_) | Msg::Noop => {
                    info!(
                        "target region is not found, drop messages";
                        "region_id" => region_id
                    );
                    return;
                }
                Msg::Snapshot { .. } => {
                    warn!(
                        "region is removed before taking snapshot, are we shutting down?";
                        "region_id" => region_id
                    );
                    return;
                }
                Msg::LogsUpToDate(cul) => {
                    warn!(
                        "region is removed before merged, are we shutting down?";
                        "region_id" => region_id,
                        "merge" => ?cul.merge,
                    );
                    return;
                }
                Msg::Change {
                    cmd: ChangeCmd::RegisterObserver { region_id, .. },
                    cb,
                    ..
                }
                | Msg::Change {
                    cmd: ChangeCmd::Snapshot { region_id, .. },
                    cb,
                    ..
                } => {
                    warn!("target region is not found";
                            "region_id" => region_id);
                    let resp = ReadResponse {
                        response: cmd_resp::new_error(Error::RegionNotFound(region_id)),
                        snapshot: None,
                    };
                    cb.invoke_read(resp);
                    return;
                }
                #[cfg(any(test, feature = "testexport"))]
                Msg::Validate(_, _) => return,
            },
            Either::Left(Err(TrySendError::Full(_))) => unreachable!(),
        };

        // Messages in one region are sent in sequence, so there is no race here.
        // However, this can't be handled inside control fsm, as messages can be
        // queued inside both queue of control fsm and normal fsm, which can reorder
        // messages.
        let (sender, apply_fsm) = ApplyFsm::from_registration(reg);
        let mailbox = BasicMailbox::new(sender, apply_fsm);
        self.register(region_id, mailbox);
    }
}

pub struct ApplyBatchSystem {
    system: BatchSystem<ApplyFsm, ControlFsm>,
}

impl Deref for ApplyBatchSystem {
    type Target = BatchSystem<ApplyFsm, ControlFsm>;

    fn deref(&self) -> &BatchSystem<ApplyFsm, ControlFsm> {
        &self.system
    }
}

impl DerefMut for ApplyBatchSystem {
    fn deref_mut(&mut self) -> &mut BatchSystem<ApplyFsm, ControlFsm> {
        &mut self.system
    }
}

impl ApplyBatchSystem {
    pub fn schedule_all<'a>(&self, peers: impl Iterator<Item = &'a Peer>) {
        let mut mailboxes = Vec::with_capacity(peers.size_hint().0);
        for peer in peers {
            let (tx, fsm) = ApplyFsm::from_peer(peer);
            mailboxes.push((peer.region().get_id(), BasicMailbox::new(tx, fsm)));
        }
        self.router().register_all(mailboxes);
    }
}

pub fn create_apply_batch_system(cfg: &Config) -> (ApplyRouter, ApplyBatchSystem) {
    let (tx, _) = loose_bounded(usize::MAX);
    let (router, system) = batch_system::create_system(
        cfg.apply_pool_size,
        cfg.apply_max_batch_size,
        tx,
        Box::new(ControlFsm),
    );
    (ApplyRouter { router }, ApplyBatchSystem { system })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::atomic::*;
    use std::sync::*;
    use std::time::*;

    use crate::coprocessor::*;
    use crate::store::msg::WriteResponse;
    use crate::store::peer_storage::RAFT_INIT_LOG_INDEX;
    use crate::store::util::{new_learner_peer, new_peer};
    use engine_rocks::{util::new_engine, RocksEngine, RocksWriteBatch, WRITE_BATCH_MAX_KEYS};
    use engine_traits::{Peekable as PeekableTrait, WriteBatchExt};
    use kvproto::metapb::{self, RegionEpoch};
    use kvproto::raft_cmdpb::*;
    use protobuf::Message;
    use tempfile::{Builder, TempDir};
    use uuid::Uuid;

    use crate::store::{Config, RegionTask};
    use test_sst_importer::*;
    use tikv_util::config::VersionTrack;
    use tikv_util::worker::dummy_scheduler;

    use super::*;

    pub fn create_tmp_engine(path: &str) -> (TempDir, RocksEngine) {
        let path = Builder::new().prefix(path).tempdir().unwrap();
        let engine = new_engine(
            path.path().join("db").to_str().unwrap(),
            None,
            ALL_CFS,
            None,
        )
        .unwrap();
        (path, engine)
    }

    pub fn create_tmp_importer(path: &str) -> (TempDir, Arc<SSTImporter>) {
        let dir = Builder::new().prefix(path).tempdir().unwrap();
        let importer = Arc::new(SSTImporter::new(dir.path()).unwrap());
        (dir, importer)
    }

    pub fn new_entry(term: u64, index: u64, req: Option<RaftCmdRequest>) -> Entry {
        let mut e = Entry::default();
        e.set_index(index);
        e.set_term(term);
        if let Some(r) = req {
            e.set_data(r.write_to_bytes().unwrap())
        }
        e
    }

    #[test]
    fn test_should_sync_log() {
        // Admin command
        let mut req = RaftCmdRequest::default();
        req.mut_admin_request()
            .set_cmd_type(AdminCmdType::ComputeHash);
        assert_eq!(should_sync_log(&req), true);

        // IngestSst command
        let mut req = Request::default();
        req.set_cmd_type(CmdType::IngestSst);
        req.set_ingest_sst(IngestSstRequest::default());
        let mut cmd = RaftCmdRequest::default();
        cmd.mut_requests().push(req);
        assert_eq!(should_write_to_engine(&cmd), true);
        assert_eq!(should_sync_log(&cmd), true);

        // Normal command
        let req = RaftCmdRequest::default();
        assert_eq!(should_sync_log(&req), false);
    }

    #[test]
    fn test_should_write_to_engine() {
        // ComputeHash command
        let mut req = RaftCmdRequest::default();
        req.mut_admin_request()
            .set_cmd_type(AdminCmdType::ComputeHash);
        assert_eq!(should_write_to_engine(&req), true);

        // IngestSst command
        let mut req = Request::default();
        req.set_cmd_type(CmdType::IngestSst);
        req.set_ingest_sst(IngestSstRequest::default());
        let mut cmd = RaftCmdRequest::default();
        cmd.mut_requests().push(req);
        assert_eq!(should_write_to_engine(&cmd), true);
    }

    fn validate<F>(router: &ApplyRouter, region_id: u64, validate: F)
    where
        F: FnOnce(&ApplyDelegate) + Send + 'static,
    {
        let (validate_tx, validate_rx) = mpsc::channel();
        router.schedule_task(
            region_id,
            Msg::Validate(
                region_id,
                Box::new(move |(delegate, _): (&ApplyDelegate, _)| {
                    validate(delegate);
                    validate_tx.send(()).unwrap();
                }),
            ),
        );
        validate_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    }

    // Make sure msgs are handled in the same batch.
    fn batch_messages(router: &ApplyRouter, region_id: u64, msgs: Vec<Msg>) {
        let (notify1, wait1) = mpsc::channel();
        let (notify2, wait2) = mpsc::channel();
        router.schedule_task(
            region_id,
            Msg::Validate(
                region_id,
                Box::new(move |_| {
                    notify1.send(()).unwrap();
                    wait2.recv().unwrap();
                }),
            ),
        );
        wait1.recv().unwrap();

        for msg in msgs {
            router.schedule_task(region_id, msg);
        }

        notify2.send(()).unwrap();
    }

    fn fetch_apply_res(receiver: &::std::sync::mpsc::Receiver<PeerMsg<RocksEngine>>) -> ApplyRes {
        match receiver.recv_timeout(Duration::from_secs(3)) {
            Ok(PeerMsg::ApplyRes { res, .. }) => match res {
                TaskRes::Apply(res) => res,
                e => panic!("unexpected res {:?}", e),
            },
            e => panic!("unexpected res {:?}", e),
        }
    }

    #[test]
    fn test_basic_flow() {
        let (tx, rx) = mpsc::channel();
        let sender = Notifier::Sender(tx);
        let (_tmp, engine) = create_tmp_engine("apply-basic");
        let (_dir, importer) = create_tmp_importer("apply-basic");
        let cfg = Arc::new(VersionTrack::new(Config::default()));
        let (router, mut system) = create_apply_batch_system(&cfg.value());
        let (region_scheduler, snapshot_rx) = dummy_scheduler();
        let builder = super::Builder::<RocksWriteBatch> {
            tag: "test-store".to_owned(),
            cfg,
            coprocessor_host: CoprocessorHost::default(),
            importer,
            sender,
            _phantom: PhantomData,
            engine: engine.clone(),
            router: router.clone(),
        };
        system.spawn("test-basic".to_owned(), builder);

        let mut reg = Registration::default();
        reg.id = 1;
        reg.region.set_id(2);
        reg.apply_state.set_applied_index(3);
        reg.term = 4;
        reg.applied_index_term = 5;
        router.schedule_task(2, Msg::Registration(reg.clone()));
        let reg_ = reg.clone();
        validate(&router, 2, move |delegate| {
            assert_eq!(delegate.id, 1);
            assert_eq!(delegate.tag, "[region 2] 1");
            assert_eq!(delegate.region, reg_.region);
            assert!(!delegate.pending_remove);
            assert_eq!(delegate.apply_state, reg_.apply_state);
            assert_eq!(delegate.term, reg_.term);
            assert_eq!(delegate.applied_index_term, reg_.applied_index_term);
        });

        let (resp_tx, resp_rx) = mpsc::channel();
        let p = Proposal::new(
            false,
            1,
            0,
            Callback::Write(Box::new(move |resp: WriteResponse| {
                resp_tx.send(resp.response).unwrap();
            })),
        );
        let region_proposal = RegionProposal::new(1, 1, vec![p]);
        router.schedule_task(1, Msg::Proposal(region_proposal));
        // unregistered region should be ignored and notify failed.
        let resp = resp_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(resp.get_header().get_error().has_region_not_found());
        assert!(rx.try_recv().is_err());

        let (cc_tx, cc_rx) = mpsc::channel();
        let pops = vec![
            Proposal::new(false, 2, 0, Callback::None),
            Proposal::new(
                true,
                3,
                0,
                Callback::Write(Box::new(move |write: WriteResponse| {
                    cc_tx.send(write.response).unwrap();
                })),
            ),
        ];
        let region_proposal = RegionProposal::new(1, 2, pops);
        router.schedule_task(2, Msg::Proposal(region_proposal));
        validate(&router, 2, move |delegate| {
            assert_eq!(
                delegate.pending_cmds.normals.back().map(|c| c.index),
                Some(2)
            );
            assert_eq!(
                delegate.pending_cmds.conf_change.as_ref().map(|c| c.index),
                Some(3)
            );
        });
        assert!(rx.try_recv().is_err());

        let p = Proposal::new(true, 4, 0, Callback::None);
        let region_proposal = RegionProposal::new(1, 2, vec![p]);
        router.schedule_task(2, Msg::Proposal(region_proposal));
        validate(&router, 2, |delegate| {
            assert_eq!(
                delegate.pending_cmds.conf_change.as_ref().map(|c| c.index),
                Some(4)
            );
        });
        assert!(rx.try_recv().is_err());
        // propose another conf change should mark previous stale.
        let cc_resp = cc_rx.try_recv().unwrap();
        assert!(cc_resp.get_header().get_error().has_stale_command());

        router.schedule_task(
            1,
            Msg::apply(Apply::new(1, 1, vec![new_entry(2, 3, None)], 2, 2, 3)),
        );
        // non registered region should be ignored.
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());

        router.schedule_task(2, Msg::apply(Apply::new(2, 11, vec![], 3, 2, 3)));
        // empty entries should be ignored.
        let reg_term = reg.term;
        validate(&router, 2, move |delegate| {
            assert_eq!(delegate.term, reg_term);
        });
        assert!(rx.try_recv().is_err());

        let apply_state_key = keys::apply_state_key(2);
        assert!(engine
            .get_msg_cf::<RaftApplyState>(CF_RAFT, &apply_state_key)
            .unwrap()
            .is_none());
        // Make sure Apply and Snapshot are in the same batch.
        let (tx, _) = mpsc::sync_channel(0);
        let snap_task = GenSnapTask::new(2, 0, tx);
        let cb: SnapshotCallback<RocksEngine> =
            Box::new(move |snap_ret, _region, apply_state, applied_index_term| {
                if let Ok(snap) = snap_ret {
                    let _ = snap_task.generate_and_schedule_snapshot(
                        snap,
                        applied_index_term,
                        apply_state.clone(),
                        &region_scheduler,
                    );
                }
            });

        batch_messages(
            &router,
            2,
            vec![
                Msg::apply(Apply::new(2, 11, vec![new_entry(5, 4, None)], 3, 5, 4)),
                Msg::Snapshot {
                    cb,
                    region_id: 0,
                    sync: true,
                },
            ],
        );
        let apply_res = match rx.recv_timeout(Duration::from_secs(3)) {
            Ok(PeerMsg::ApplyRes { res, .. }) => match res {
                TaskRes::Apply(res) => res,
                e => panic!("unexpected apply result: {:?}", e),
            },
            e => panic!("unexpected apply result: {:?}", e),
        };
        let apply_state = match snapshot_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Some(RegionTask::Gen { kv_snap, .. })) => kv_snap
                .get_msg_cf(CF_RAFT, &apply_state_key)
                .unwrap()
                .unwrap(),
            e => panic!("unexpected apply result: {:?}", e),
        };
        assert_eq!(apply_res.region_id, 2);
        assert_eq!(apply_res.apply_state, apply_state);
        assert_eq!(apply_res.apply_state.get_applied_index(), 4);
        assert!(apply_res.exec_res.is_empty());
        // empty entry will make applied_index step forward and should write apply state to engine.
        assert_eq!(apply_res.metrics.written_keys, 1);
        assert_eq!(apply_res.applied_index_term, 5);
        validate(&router, 2, |delegate| {
            assert_eq!(delegate.term, 11);
            assert_eq!(delegate.applied_index_term, 5);
            assert_eq!(delegate.apply_state.get_applied_index(), 4);
            assert_eq!(
                delegate.apply_state.get_applied_index(),
                delegate.last_sync_apply_index
            );
        });

        router.schedule_task(2, Msg::destroy(2, true));
        let (region_id, peer_id) = match rx.recv_timeout(Duration::from_secs(3)) {
            Ok(PeerMsg::ApplyRes { res, .. }) => match res {
                TaskRes::Destroy { region_id, peer_id } => (region_id, peer_id),
                e => panic!("expected destroy result, but got {:?}", e),
            },
            e => panic!("expected destroy result, but got {:?}", e),
        };
        assert_eq!(peer_id, 1);
        assert_eq!(region_id, 2);

        // Stopped peer should be removed.
        let (resp_tx, resp_rx) = mpsc::channel();
        let p = Proposal::new(
            false,
            1,
            0,
            Callback::Write(Box::new(move |resp: WriteResponse| {
                resp_tx.send(resp.response).unwrap();
            })),
        );
        let region_proposal = RegionProposal::new(1, 2, vec![p]);
        router.schedule_task(2, Msg::Proposal(region_proposal));
        // unregistered region should be ignored and notify failed.
        let resp = resp_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(
            resp.get_header().get_error().has_region_not_found(),
            "{:?}",
            resp
        );
        assert!(rx.try_recv().is_err());

        system.shutdown();
    }

    struct EntryBuilder {
        entry: Entry,
        req: RaftCmdRequest,
    }

    impl EntryBuilder {
        fn new(index: u64, term: u64) -> EntryBuilder {
            let req = RaftCmdRequest::default();
            let mut entry = Entry::default();
            entry.set_index(index);
            entry.set_term(term);
            EntryBuilder { entry, req }
        }

        fn capture_resp(
            self,
            router: &ApplyRouter,
            id: u64,
            region_id: u64,
            tx: Sender<RaftCmdResponse>,
        ) -> EntryBuilder {
            // TODO: may need to support conf change.
            let prop = Proposal::new(
                false,
                self.entry.get_index(),
                self.entry.get_term(),
                Callback::Write(Box::new(move |resp: WriteResponse| {
                    tx.send(resp.response).unwrap();
                })),
            );
            router.schedule_task(
                region_id,
                Msg::Proposal(RegionProposal::new(id, region_id, vec![prop])),
            );
            self
        }

        fn epoch(mut self, conf_ver: u64, version: u64) -> EntryBuilder {
            let mut epoch = RegionEpoch::default();
            epoch.set_version(version);
            epoch.set_conf_ver(conf_ver);
            self.req.mut_header().set_region_epoch(epoch);
            self
        }

        fn put(self, key: &[u8], value: &[u8]) -> EntryBuilder {
            self.add_put_req(None, key, value)
        }

        fn put_cf(self, cf: &str, key: &[u8], value: &[u8]) -> EntryBuilder {
            self.add_put_req(Some(cf), key, value)
        }

        fn add_put_req(mut self, cf: Option<&str>, key: &[u8], value: &[u8]) -> EntryBuilder {
            let mut cmd = Request::default();
            cmd.set_cmd_type(CmdType::Put);
            if let Some(cf) = cf {
                cmd.mut_put().set_cf(cf.to_owned());
            }
            cmd.mut_put().set_key(key.to_vec());
            cmd.mut_put().set_value(value.to_vec());
            self.req.mut_requests().push(cmd);
            self
        }

        fn delete(self, key: &[u8]) -> EntryBuilder {
            self.add_delete_req(None, key)
        }

        fn delete_cf(self, cf: &str, key: &[u8]) -> EntryBuilder {
            self.add_delete_req(Some(cf), key)
        }

        fn delete_range(self, start_key: &[u8], end_key: &[u8]) -> EntryBuilder {
            self.add_delete_range_req(None, start_key, end_key)
        }

        fn delete_range_cf(self, cf: &str, start_key: &[u8], end_key: &[u8]) -> EntryBuilder {
            self.add_delete_range_req(Some(cf), start_key, end_key)
        }

        fn add_delete_req(mut self, cf: Option<&str>, key: &[u8]) -> EntryBuilder {
            let mut cmd = Request::default();
            cmd.set_cmd_type(CmdType::Delete);
            if let Some(cf) = cf {
                cmd.mut_delete().set_cf(cf.to_owned());
            }
            cmd.mut_delete().set_key(key.to_vec());
            self.req.mut_requests().push(cmd);
            self
        }

        fn add_delete_range_req(
            mut self,
            cf: Option<&str>,
            start_key: &[u8],
            end_key: &[u8],
        ) -> EntryBuilder {
            let mut cmd = Request::default();
            cmd.set_cmd_type(CmdType::DeleteRange);
            if let Some(cf) = cf {
                cmd.mut_delete_range().set_cf(cf.to_owned());
            }
            cmd.mut_delete_range().set_start_key(start_key.to_vec());
            cmd.mut_delete_range().set_end_key(end_key.to_vec());
            self.req.mut_requests().push(cmd);
            self
        }

        fn ingest_sst(mut self, meta: &SstMeta) -> EntryBuilder {
            let mut cmd = Request::default();
            cmd.set_cmd_type(CmdType::IngestSst);
            cmd.mut_ingest_sst().set_sst(meta.clone());
            self.req.mut_requests().push(cmd);
            self
        }

        fn split(mut self, splits: BatchSplitRequest) -> EntryBuilder {
            let mut req = AdminRequest::default();
            req.set_cmd_type(AdminCmdType::BatchSplit);
            req.set_splits(splits);
            self.req.set_admin_request(req);
            self
        }

        fn build(mut self) -> Entry {
            self.entry.set_data(self.req.write_to_bytes().unwrap());
            self.entry
        }
    }

    #[derive(Clone, Default)]
    struct ApplyObserver {
        pre_admin_count: Arc<AtomicUsize>,
        pre_query_count: Arc<AtomicUsize>,
        post_admin_count: Arc<AtomicUsize>,
        post_query_count: Arc<AtomicUsize>,
        cmd_batches: RefCell<Vec<CmdBatch>>,
        cmd_sink: Option<Arc<Mutex<Sender<CmdBatch>>>>,
    }

    impl Coprocessor for ApplyObserver {}

    impl QueryObserver for ApplyObserver {
        fn pre_apply_query(&self, _: &mut ObserverContext<'_>, _: &[Request]) {
            self.pre_query_count.fetch_add(1, Ordering::SeqCst);
        }

        fn post_apply_query(&self, _: &mut ObserverContext<'_>, _: &mut Vec<Response>) {
            self.post_query_count.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl CmdObserver for ApplyObserver {
        fn on_prepare_for_apply(&self, observe_id: ObserveID, region_id: u64) {
            self.cmd_batches
                .borrow_mut()
                .push(CmdBatch::new(observe_id, region_id));
        }

        fn on_apply_cmd(&self, observe_id: ObserveID, region_id: u64, cmd: Cmd) {
            self.cmd_batches
                .borrow_mut()
                .last_mut()
                .expect("should exist some cmd batch")
                .push(observe_id, region_id, cmd);
        }

        fn on_flush_apply(&self) {
            if !self.cmd_batches.borrow().is_empty() {
                let batches = self.cmd_batches.replace(Vec::default());
                for b in batches {
                    if let Some(sink) = self.cmd_sink.as_ref() {
                        sink.lock().unwrap().send(b).unwrap();
                    }
                }
            }
        }
    }

    #[test]
    fn test_handle_raft_committed_entries() {
        let (_path, engine) = create_tmp_engine("test-delegate");
        let (import_dir, importer) = create_tmp_importer("test-delegate");
        let obs = ApplyObserver::default();
        let mut host = CoprocessorHost::default();
        host.registry
            .register_query_observer(1, BoxQueryObserver::new(obs.clone()));

        let (tx, rx) = mpsc::channel();
        let sender = Notifier::Sender(tx);
        let cfg = Arc::new(VersionTrack::new(Config::default()));
        let (router, mut system) = create_apply_batch_system(&cfg.value());
        let builder = super::Builder::<RocksWriteBatch> {
            tag: "test-store".to_owned(),
            cfg,
            sender,
            _phantom: PhantomData,
            coprocessor_host: host,
            importer: importer.clone(),
            engine: engine.clone(),
            router: router.clone(),
        };
        system.spawn("test-handle-raft".to_owned(), builder);

        let mut reg = Registration::default();
        reg.id = 3;
        reg.region.set_id(1);
        reg.region.mut_peers().push(new_peer(2, 3));
        reg.region.set_end_key(b"k5".to_vec());
        reg.region.mut_region_epoch().set_conf_ver(1);
        reg.region.mut_region_epoch().set_version(3);
        router.schedule_task(1, Msg::Registration(reg));

        let (capture_tx, capture_rx) = mpsc::channel();
        let put_entry = EntryBuilder::new(1, 1)
            .put(b"k1", b"v1")
            .put(b"k2", b"v1")
            .put(b"k3", b"v1")
            .epoch(1, 3)
            .capture_resp(&router, 3, 1, capture_tx.clone())
            .build();
        router.schedule_task(1, Msg::apply(Apply::new(1, 1, vec![put_entry], 0, 1, 1)));
        let resp = capture_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(!resp.get_header().has_error(), "{:?}", resp);
        assert_eq!(resp.get_responses().len(), 3);
        let dk_k1 = keys::data_key(b"k1");
        let dk_k2 = keys::data_key(b"k2");
        let dk_k3 = keys::data_key(b"k3");
        assert_eq!(engine.get_value(&dk_k1).unwrap().unwrap(), b"v1");
        assert_eq!(engine.get_value(&dk_k2).unwrap().unwrap(), b"v1");
        assert_eq!(engine.get_value(&dk_k3).unwrap().unwrap(), b"v1");
        validate(&router, 1, |delegate| {
            assert_eq!(delegate.applied_index_term, 1);
            assert_eq!(delegate.apply_state.get_applied_index(), 1);
        });
        fetch_apply_res(&rx);

        let put_entry = EntryBuilder::new(2, 2)
            .put_cf(CF_LOCK, b"k1", b"v1")
            .epoch(1, 3)
            .build();
        router.schedule_task(1, Msg::apply(Apply::new(1, 2, vec![put_entry], 1, 2, 2)));
        let apply_res = fetch_apply_res(&rx);
        assert_eq!(apply_res.region_id, 1);
        assert_eq!(apply_res.apply_state.get_applied_index(), 2);
        assert_eq!(apply_res.applied_index_term, 2);
        assert!(apply_res.exec_res.is_empty());
        assert!(apply_res.metrics.written_bytes >= 5);
        assert_eq!(apply_res.metrics.written_keys, 2);
        assert_eq!(apply_res.metrics.size_diff_hint, 5);
        assert_eq!(apply_res.metrics.lock_cf_written_bytes, 5);
        assert_eq!(
            engine.get_value_cf(CF_LOCK, &dk_k1).unwrap().unwrap(),
            b"v1"
        );

        let put_entry = EntryBuilder::new(3, 2)
            .put(b"k2", b"v2")
            .epoch(1, 1)
            .capture_resp(&router, 3, 1, capture_tx.clone())
            .build();
        router.schedule_task(1, Msg::apply(Apply::new(1, 2, vec![put_entry], 2, 2, 3)));
        let resp = capture_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(resp.get_header().get_error().has_epoch_not_match());
        let apply_res = fetch_apply_res(&rx);
        assert_eq!(apply_res.applied_index_term, 2);
        assert_eq!(apply_res.apply_state.get_applied_index(), 3);

        let put_entry = EntryBuilder::new(4, 2)
            .put(b"k3", b"v3")
            .put(b"k5", b"v5")
            .epoch(1, 3)
            .capture_resp(&router, 3, 1, capture_tx.clone())
            .build();
        router.schedule_task(1, Msg::apply(Apply::new(1, 2, vec![put_entry], 3, 2, 4)));
        let resp = capture_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(resp.get_header().get_error().has_key_not_in_region());
        let apply_res = fetch_apply_res(&rx);
        assert_eq!(apply_res.applied_index_term, 2);
        assert_eq!(apply_res.apply_state.get_applied_index(), 4);
        // a writebatch should be atomic.
        assert_eq!(engine.get_value(&dk_k3).unwrap().unwrap(), b"v1");

        EntryBuilder::new(5, 2)
            .capture_resp(&router, 3, 1, capture_tx.clone())
            .build();
        let put_entry = EntryBuilder::new(5, 3)
            .delete(b"k1")
            .delete_cf(CF_LOCK, b"k1")
            .delete_cf(CF_WRITE, b"k1")
            .epoch(1, 3)
            .capture_resp(&router, 3, 1, capture_tx.clone())
            .build();
        router.schedule_task(1, Msg::apply(Apply::new(1, 3, vec![put_entry], 4, 3, 5)));
        let resp = capture_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        // stale command should be cleared.
        assert!(resp.get_header().get_error().has_stale_command());
        let resp = capture_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(!resp.get_header().has_error(), "{:?}", resp);
        assert!(engine.get_value(&dk_k1).unwrap().is_none());
        let apply_res = fetch_apply_res(&rx);
        assert_eq!(apply_res.metrics.lock_cf_written_bytes, 3);
        assert_eq!(apply_res.metrics.delete_keys_hint, 2);
        assert_eq!(apply_res.metrics.size_diff_hint, -9);

        let delete_entry = EntryBuilder::new(6, 3)
            .delete(b"k5")
            .epoch(1, 3)
            .capture_resp(&router, 3, 1, capture_tx.clone())
            .build();
        router.schedule_task(1, Msg::apply(Apply::new(1, 3, vec![delete_entry], 5, 3, 6)));
        let resp = capture_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(resp.get_header().get_error().has_key_not_in_region());
        fetch_apply_res(&rx);

        let delete_range_entry = EntryBuilder::new(7, 3)
            .delete_range(b"", b"")
            .epoch(1, 3)
            .capture_resp(&router, 3, 1, capture_tx.clone())
            .build();
        router.schedule_task(
            1,
            Msg::apply(Apply::new(1, 3, vec![delete_range_entry], 6, 3, 7)),
        );
        let resp = capture_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(resp.get_header().get_error().has_key_not_in_region());
        assert_eq!(engine.get_value(&dk_k3).unwrap().unwrap(), b"v1");
        fetch_apply_res(&rx);

        let delete_range_entry = EntryBuilder::new(8, 3)
            .delete_range_cf(CF_DEFAULT, b"", b"k5")
            .delete_range_cf(CF_LOCK, b"", b"k5")
            .delete_range_cf(CF_WRITE, b"", b"k5")
            .epoch(1, 3)
            .capture_resp(&router, 3, 1, capture_tx.clone())
            .build();
        router.schedule_task(
            1,
            Msg::apply(Apply::new(1, 3, vec![delete_range_entry], 7, 3, 8)),
        );
        let resp = capture_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(!resp.get_header().has_error(), "{:?}", resp);
        assert!(engine.get_value(&dk_k1).unwrap().is_none());
        assert!(engine.get_value(&dk_k2).unwrap().is_none());
        assert!(engine.get_value(&dk_k3).unwrap().is_none());
        fetch_apply_res(&rx);

        // UploadSST
        let sst_path = import_dir.path().join("test.sst");
        let mut sst_epoch = RegionEpoch::default();
        sst_epoch.set_conf_ver(1);
        sst_epoch.set_version(3);
        let sst_range = (0, 100);
        let (mut meta1, data1) = gen_sst_file(&sst_path, sst_range);
        meta1.set_region_id(1);
        meta1.set_region_epoch(sst_epoch);
        let mut file1 = importer.create(&meta1).unwrap();
        file1.append(&data1).unwrap();
        file1.finish().unwrap();
        let (mut meta2, data2) = gen_sst_file(&sst_path, sst_range);
        meta2.set_region_id(1);
        meta2.mut_region_epoch().set_conf_ver(1);
        meta2.mut_region_epoch().set_version(1234);
        let mut file2 = importer.create(&meta2).unwrap();
        file2.append(&data2).unwrap();
        file2.finish().unwrap();

        // IngestSst
        let put_ok = EntryBuilder::new(9, 3)
            .capture_resp(&router, 3, 1, capture_tx.clone())
            .put(&[sst_range.0], &[sst_range.1])
            .epoch(0, 3)
            .build();
        // Add a put above to test flush before ingestion.
        let ingest_ok = EntryBuilder::new(10, 3)
            .capture_resp(&router, 3, 1, capture_tx.clone())
            .ingest_sst(&meta1)
            .epoch(0, 3)
            .build();
        let ingest_epoch_not_match = EntryBuilder::new(11, 3)
            .capture_resp(&router, 3, 1, capture_tx.clone())
            .ingest_sst(&meta2)
            .epoch(0, 3)
            .build();
        let entries = vec![put_ok, ingest_ok, ingest_epoch_not_match];
        router.schedule_task(1, Msg::apply(Apply::new(1, 3, entries, 8, 3, 11)));
        let resp = capture_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(!resp.get_header().has_error(), "{:?}", resp);
        let resp = capture_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(!resp.get_header().has_error(), "{:?}", resp);
        check_db_range(&engine, sst_range);
        let resp = capture_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(resp.get_header().has_error());
        let apply_res = fetch_apply_res(&rx);
        assert_eq!(apply_res.applied_index_term, 3);
        assert_eq!(apply_res.apply_state.get_applied_index(), 10);
        // Two continuous writes inside the same region will yield.
        let apply_res = fetch_apply_res(&rx);
        assert_eq!(apply_res.applied_index_term, 3);
        assert_eq!(apply_res.apply_state.get_applied_index(), 11);

        let mut entries = vec![];
        for i in 0..WRITE_BATCH_MAX_KEYS {
            let put_entry = EntryBuilder::new(i as u64 + 12, 3)
                .put(b"k", b"v")
                .epoch(1, 3)
                .capture_resp(&router, 3, 1, capture_tx.clone())
                .build();
            entries.push(put_entry);
        }
        router.schedule_task(
            1,
            Msg::apply(Apply::new(
                1,
                3,
                entries,
                11,
                3,
                WRITE_BATCH_MAX_KEYS as u64 + 11,
            )),
        );
        for _ in 0..WRITE_BATCH_MAX_KEYS {
            capture_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        }
        let index = WRITE_BATCH_MAX_KEYS + 11;
        let apply_res = fetch_apply_res(&rx);
        assert_eq!(apply_res.apply_state.get_applied_index(), index as u64);
        assert_eq!(obs.pre_query_count.load(Ordering::SeqCst), index);
        assert_eq!(obs.post_query_count.load(Ordering::SeqCst), index);

        system.shutdown();
    }

    #[test]
    fn test_cmd_observer() {
        let (_path, engine) = create_tmp_engine("test-delegate");
        let (_import_dir, importer) = create_tmp_importer("test-delegate");
        let mut host = CoprocessorHost::default();
        let mut obs = ApplyObserver::default();
        let (sink, cmdbatch_rx) = mpsc::channel();
        obs.cmd_sink = Some(Arc::new(Mutex::new(sink)));
        host.registry
            .register_cmd_observer(1, BoxCmdObserver::new(obs));

        let (tx, rx) = mpsc::channel();
        let sender = Notifier::Sender(tx);
        let cfg = Config::default();
        let (router, mut system) = create_apply_batch_system(&cfg);
        let _phantom = engine.write_batch();
        let builder = super::Builder::<RocksWriteBatch> {
            tag: "test-store".to_owned(),
            cfg: Arc::new(VersionTrack::new(cfg)),
            sender,
            coprocessor_host: host,
            importer,
            engine,
            _phantom: PhantomData,
            router: router.clone(),
        };
        system.spawn("test-handle-raft".to_owned(), builder);

        let mut reg = Registration::default();
        reg.id = 3;
        reg.region.set_id(1);
        reg.region.mut_peers().push(new_peer(2, 3));
        reg.region.set_end_key(b"k5".to_vec());
        reg.region.mut_region_epoch().set_conf_ver(1);
        reg.region.mut_region_epoch().set_version(3);
        let region_epoch = reg.region.get_region_epoch().clone();
        router.schedule_task(1, Msg::Registration(reg));

        let put_entry = EntryBuilder::new(1, 1)
            .put(b"k1", b"v1")
            .put(b"k2", b"v1")
            .put(b"k3", b"v1")
            .epoch(1, 3)
            .build();
        router.schedule_task(1, Msg::apply(Apply::new(1, 1, vec![put_entry], 0, 1, 1)));
        fetch_apply_res(&rx);
        // It must receive nothing because no region registered.
        cmdbatch_rx
            .recv_timeout(Duration::from_millis(100))
            .unwrap_err();
        let (block_tx, block_rx) = mpsc::channel::<()>();
        router.schedule_task(
            1,
            Msg::Validate(
                1,
                Box::new(move |_| {
                    // Block the apply worker
                    block_rx.recv().unwrap();
                }),
            ),
        );
        let put_entry = EntryBuilder::new(2, 2)
            .put(b"k0", b"v0")
            .epoch(1, 3)
            .build();
        router.schedule_task(1, Msg::apply(Apply::new(1, 2, vec![put_entry], 1, 2, 2)));
        // Register cmd observer to region 1.
        let enabled = Arc::new(AtomicBool::new(true));
        let observe_id = ObserveID::new();
        router.schedule_task(
            1,
            Msg::Change {
                region_epoch: region_epoch.clone(),
                cmd: ChangeCmd::RegisterObserver {
                    observe_id,
                    region_id: 1,
                    enabled: enabled.clone(),
                },
                cb: Callback::Read(Box::new(|resp: ReadResponse<_>| {
                    assert!(!resp.response.get_header().has_error());
                    assert!(resp.snapshot.is_some());
                    let snap = resp.snapshot.unwrap();
                    assert_eq!(snap.get_value(b"k0").unwrap().unwrap(), b"v0");
                })),
            },
        );
        // Unblock the apply worker
        block_tx.send(()).unwrap();
        fetch_apply_res(&rx);
        let (capture_tx, capture_rx) = mpsc::channel();
        let put_entry = EntryBuilder::new(3, 2)
            .put_cf(CF_LOCK, b"k1", b"v1")
            .epoch(1, 3)
            .capture_resp(&router, 3, 1, capture_tx)
            .build();
        router.schedule_task(1, Msg::apply(Apply::new(1, 2, vec![put_entry], 2, 2, 3)));
        fetch_apply_res(&rx);
        let resp = capture_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(!resp.get_header().has_error(), "{:?}", resp);
        assert_eq!(resp.get_responses().len(), 1);
        let cmd_batch = cmdbatch_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(resp, cmd_batch.into_iter(1).next().unwrap().response);

        let put_entry1 = EntryBuilder::new(4, 2)
            .put(b"k2", b"v2")
            .epoch(1, 3)
            .build();
        let put_entry2 = EntryBuilder::new(5, 2)
            .put(b"k2", b"v2")
            .epoch(1, 3)
            .build();
        router.schedule_task(
            1,
            Msg::apply(Apply::new(1, 2, vec![put_entry1, put_entry2], 3, 2, 5)),
        );
        let cmd_batch = cmdbatch_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(2, cmd_batch.len());

        // Stop observer regoin 1.
        enabled.store(false, Ordering::SeqCst);
        let put_entry = EntryBuilder::new(6, 2)
            .put(b"k2", b"v2")
            .epoch(1, 3)
            .build();
        router.schedule_task(1, Msg::apply(Apply::new(1, 2, vec![put_entry], 5, 2, 6)));
        // Must not receive new cmd.
        cmdbatch_rx
            .recv_timeout(Duration::from_millis(100))
            .unwrap_err();

        // Must response a RegionNotFound error.
        router.schedule_task(
            2,
            Msg::Change {
                region_epoch,
                cmd: ChangeCmd::RegisterObserver {
                    observe_id,
                    region_id: 2,
                    enabled,
                },
                cb: Callback::Read(Box::new(|resp: ReadResponse<_>| {
                    assert!(resp
                        .response
                        .get_header()
                        .get_error()
                        .has_region_not_found());
                    assert!(resp.snapshot.is_none());
                })),
            },
        );

        system.shutdown();
    }

    #[test]
    fn test_check_sst_for_ingestion() {
        let mut sst = SstMeta::default();
        let mut region = Region::default();

        // Check uuid and cf name
        assert!(check_sst_for_ingestion(&sst, &region).is_err());
        sst.set_uuid(Uuid::new_v4().as_bytes().to_vec());
        sst.set_cf_name(CF_DEFAULT.to_owned());
        check_sst_for_ingestion(&sst, &region).unwrap();
        sst.set_cf_name("test".to_owned());
        assert!(check_sst_for_ingestion(&sst, &region).is_err());
        sst.set_cf_name(CF_WRITE.to_owned());
        check_sst_for_ingestion(&sst, &region).unwrap();

        // Check region id
        region.set_id(1);
        sst.set_region_id(2);
        assert!(check_sst_for_ingestion(&sst, &region).is_err());
        sst.set_region_id(1);
        check_sst_for_ingestion(&sst, &region).unwrap();

        // Check region epoch
        region.mut_region_epoch().set_conf_ver(1);
        assert!(check_sst_for_ingestion(&sst, &region).is_err());
        sst.mut_region_epoch().set_conf_ver(1);
        check_sst_for_ingestion(&sst, &region).unwrap();
        region.mut_region_epoch().set_version(1);
        assert!(check_sst_for_ingestion(&sst, &region).is_err());
        sst.mut_region_epoch().set_version(1);
        check_sst_for_ingestion(&sst, &region).unwrap();

        // Check region range
        region.set_start_key(vec![2]);
        region.set_end_key(vec![8]);
        sst.mut_range().set_start(vec![1]);
        sst.mut_range().set_end(vec![8]);
        assert!(check_sst_for_ingestion(&sst, &region).is_err());
        sst.mut_range().set_start(vec![2]);
        assert!(check_sst_for_ingestion(&sst, &region).is_err());
        sst.mut_range().set_end(vec![7]);
        check_sst_for_ingestion(&sst, &region).unwrap();
    }

    fn new_split_req(key: &[u8], id: u64, children: Vec<u64>) -> SplitRequest {
        let mut req = SplitRequest::default();
        req.set_split_key(key.to_vec());
        req.set_new_region_id(id);
        req.set_new_peer_ids(children);
        req
    }

    struct SplitResultChecker<'a> {
        engine: RocksEngine,
        origin_peers: &'a [metapb::Peer],
        epoch: Rc<RefCell<RegionEpoch>>,
    }

    impl<'a> SplitResultChecker<'a> {
        fn check(&self, start: &[u8], end: &[u8], id: u64, children: &[u64], check_initial: bool) {
            let key = keys::region_state_key(id);
            let state: RegionLocalState = self.engine.get_msg_cf(CF_RAFT, &key).unwrap().unwrap();
            assert_eq!(state.get_state(), PeerState::Normal);
            assert_eq!(state.get_region().get_id(), id);
            assert_eq!(state.get_region().get_start_key(), start);
            assert_eq!(state.get_region().get_end_key(), end);
            let expect_peers: Vec<_> = self
                .origin_peers
                .iter()
                .zip(children)
                .map(|(p, new_id)| {
                    let mut new_peer = metapb::Peer::clone(p);
                    new_peer.set_id(*new_id);
                    new_peer
                })
                .collect();
            assert_eq!(state.get_region().get_peers(), expect_peers.as_slice());
            assert!(!state.has_merge_state(), "{:?}", state);
            let epoch = self.epoch.borrow();
            assert_eq!(*state.get_region().get_region_epoch(), *epoch);
            if !check_initial {
                return;
            }
            let key = keys::apply_state_key(id);
            let initial_state: RaftApplyState =
                self.engine.get_msg_cf(CF_RAFT, &key).unwrap().unwrap();
            assert_eq!(initial_state.get_applied_index(), RAFT_INIT_LOG_INDEX);
            assert_eq!(
                initial_state.get_truncated_state().get_index(),
                RAFT_INIT_LOG_INDEX
            );
            assert_eq!(
                initial_state.get_truncated_state().get_term(),
                RAFT_INIT_LOG_INDEX
            );
        }
    }

    fn error_msg(resp: &RaftCmdResponse) -> &str {
        resp.get_header().get_error().get_message()
    }

    #[test]
    fn test_split() {
        let (_path, engine) = create_tmp_engine("test-delegate");
        let (_import_dir, importer) = create_tmp_importer("test-delegate");
        let mut reg = Registration::default();
        reg.id = 3;
        reg.term = 1;
        reg.region.set_id(1);
        reg.region.set_end_key(b"k5".to_vec());
        reg.region.mut_region_epoch().set_version(3);
        let region_epoch = reg.region.get_region_epoch().clone();
        let peers = vec![new_peer(2, 3), new_peer(4, 5), new_learner_peer(6, 7)];
        reg.region.set_peers(peers.clone().into());
        let (tx, _rx) = mpsc::channel();
        let sender = Notifier::Sender(tx);
        let mut host = CoprocessorHost::default();
        let mut obs = ApplyObserver::default();
        let (sink, cmdbatch_rx) = mpsc::channel();
        obs.cmd_sink = Some(Arc::new(Mutex::new(sink)));
        host.registry
            .register_cmd_observer(1, BoxCmdObserver::new(obs));
        let cfg = Arc::new(VersionTrack::new(Config::default()));
        let (router, mut system) = create_apply_batch_system(&cfg.value());
        let builder = super::Builder::<RocksWriteBatch> {
            tag: "test-store".to_owned(),
            cfg,
            sender,
            importer,
            _phantom: PhantomData,
            coprocessor_host: host,
            engine: engine.clone(),
            router: router.clone(),
        };
        system.spawn("test-split".to_owned(), builder);

        router.schedule_task(1, Msg::Registration(reg.clone()));
        let enabled = Arc::new(AtomicBool::new(true));
        let observe_id = ObserveID::new();
        router.schedule_task(
            1,
            Msg::Change {
                region_epoch: region_epoch.clone(),
                cmd: ChangeCmd::RegisterObserver {
                    observe_id,
                    region_id: 1,
                    enabled: enabled.clone(),
                },
                cb: Callback::Read(Box::new(|resp: ReadResponse<_>| {
                    assert!(!resp.response.get_header().has_error(), "{:?}", resp);
                    assert!(resp.snapshot.is_some());
                })),
            },
        );

        let mut index_id = 1;
        let (capture_tx, capture_rx) = mpsc::channel();
        let epoch = Rc::new(RefCell::new(reg.region.get_region_epoch().to_owned()));
        let epoch_ = epoch.clone();
        let mut exec_split = |router: &ApplyRouter, reqs| {
            let epoch = epoch_.borrow();
            let split = EntryBuilder::new(index_id, 1)
                .split(reqs)
                .epoch(epoch.get_conf_ver(), epoch.get_version())
                .capture_resp(router, 3, 1, capture_tx.clone())
                .build();
            router.schedule_task(
                1,
                Msg::apply(Apply::new(1, 1, vec![split], index_id - 1, 1, index_id)),
            );
            index_id += 1;
            capture_rx.recv_timeout(Duration::from_secs(3)).unwrap()
        };

        let mut splits = BatchSplitRequest::default();
        splits.set_right_derive(true);
        splits.mut_requests().push(new_split_req(b"k1", 8, vec![]));
        let resp = exec_split(&router, splits.clone());
        // 3 followers are required.
        assert!(error_msg(&resp).contains("id count"), "{:?}", resp);
        cmdbatch_rx.recv_timeout(Duration::from_secs(3)).unwrap();

        splits.mut_requests().clear();
        let resp = exec_split(&router, splits.clone());
        // Empty requests should be rejected.
        assert!(error_msg(&resp).contains("missing"), "{:?}", resp);

        splits
            .mut_requests()
            .push(new_split_req(b"k6", 8, vec![9, 10, 11]));
        let resp = exec_split(&router, splits.clone());
        // Out of range keys should be rejected.
        assert!(
            resp.get_header().get_error().has_key_not_in_region(),
            "{:?}",
            resp
        );

        splits
            .mut_requests()
            .push(new_split_req(b"", 8, vec![9, 10, 11]));
        let resp = exec_split(&router, splits.clone());
        // Empty key should be rejected.
        assert!(error_msg(&resp).contains("missing"), "{:?}", resp);

        splits.mut_requests().clear();
        splits
            .mut_requests()
            .push(new_split_req(b"k2", 8, vec![9, 10, 11]));
        splits
            .mut_requests()
            .push(new_split_req(b"k1", 8, vec![9, 10, 11]));
        let resp = exec_split(&router, splits.clone());
        // keys should be in ascend order.
        assert!(error_msg(&resp).contains("invalid"), "{:?}", resp);

        splits.mut_requests().clear();
        splits
            .mut_requests()
            .push(new_split_req(b"k1", 8, vec![9, 10, 11]));
        splits
            .mut_requests()
            .push(new_split_req(b"k2", 8, vec![9, 10]));
        let resp = exec_split(&router, splits.clone());
        // All requests should be checked.
        assert!(error_msg(&resp).contains("id count"), "{:?}", resp);
        let checker = SplitResultChecker {
            engine,
            origin_peers: &peers,
            epoch: epoch.clone(),
        };

        splits.mut_requests().clear();
        splits
            .mut_requests()
            .push(new_split_req(b"k1", 8, vec![9, 10, 11]));
        let resp = exec_split(&router, splits.clone());
        // Split should succeed.
        assert!(!resp.get_header().has_error(), "{:?}", resp);
        let mut new_version = epoch.borrow().get_version() + 1;
        epoch.borrow_mut().set_version(new_version);
        checker.check(b"", b"k1", 8, &[9, 10, 11], true);
        checker.check(b"k1", b"k5", 1, &[3, 5, 7], false);

        splits.mut_requests().clear();
        splits
            .mut_requests()
            .push(new_split_req(b"k4", 12, vec![13, 14, 15]));
        splits.set_right_derive(false);
        let resp = exec_split(&router, splits.clone());
        // Right derive should be respected.
        assert!(!resp.get_header().has_error(), "{:?}", resp);
        new_version = epoch.borrow().get_version() + 1;
        epoch.borrow_mut().set_version(new_version);
        checker.check(b"k4", b"k5", 12, &[13, 14, 15], true);
        checker.check(b"k1", b"k4", 1, &[3, 5, 7], false);

        splits.mut_requests().clear();
        splits
            .mut_requests()
            .push(new_split_req(b"k2", 16, vec![17, 18, 19]));
        splits
            .mut_requests()
            .push(new_split_req(b"k3", 20, vec![21, 22, 23]));
        splits.set_right_derive(true);
        let resp = exec_split(&router, splits.clone());
        // Right derive should be respected.
        assert!(!resp.get_header().has_error(), "{:?}", resp);
        new_version = epoch.borrow().get_version() + 2;
        epoch.borrow_mut().set_version(new_version);
        checker.check(b"k1", b"k2", 16, &[17, 18, 19], true);
        checker.check(b"k2", b"k3", 20, &[21, 22, 23], true);
        checker.check(b"k3", b"k4", 1, &[3, 5, 7], false);

        splits.mut_requests().clear();
        splits
            .mut_requests()
            .push(new_split_req(b"k31", 24, vec![25, 26, 27]));
        splits
            .mut_requests()
            .push(new_split_req(b"k32", 28, vec![29, 30, 31]));
        splits.set_right_derive(false);
        let resp = exec_split(&router, splits);
        // Right derive should be respected.
        assert!(!resp.get_header().has_error(), "{:?}", resp);
        new_version = epoch.borrow().get_version() + 2;
        epoch.borrow_mut().set_version(new_version);
        checker.check(b"k3", b"k31", 1, &[3, 5, 7], false);
        checker.check(b"k31", b"k32", 24, &[25, 26, 27], true);
        checker.check(b"k32", b"k4", 28, &[29, 30, 31], true);

        let (tx, rx) = mpsc::channel();
        enabled.store(false, Ordering::SeqCst);
        router.schedule_task(
            1,
            Msg::Change {
                region_epoch,
                cmd: ChangeCmd::RegisterObserver {
                    observe_id,
                    region_id: 1,
                    enabled: Arc::new(AtomicBool::new(true)),
                },
                cb: Callback::Read(Box::new(move |resp: ReadResponse<_>| {
                    assert!(
                        resp.response.get_header().get_error().has_epoch_not_match(),
                        "{:?}",
                        resp
                    );
                    assert!(resp.snapshot.is_none());
                    tx.send(()).unwrap();
                })),
            },
        );
        rx.recv_timeout(Duration::from_millis(500)).unwrap();

        system.shutdown();
    }

    #[test]
    fn pending_cmd_leak() {
        let res = panic_hook::recover_safe(|| {
            let _cmd = PendingCmd::new(1, 1, Callback::None);
        });
        res.unwrap_err();
    }

    #[test]
    fn pending_cmd_leak_dtor_not_abort() {
        let res = panic_hook::recover_safe(|| {
            let _cmd = PendingCmd::new(1, 1, Callback::None);
            panic!("Don't abort");
            // It would abort and fail if there was a double-panic in PendingCmd dtor.
        });
        res.unwrap_err();
    }
}
