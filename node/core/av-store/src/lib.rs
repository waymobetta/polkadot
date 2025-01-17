// Copyright 2020 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Implements a `AvailabilityStoreSubsystem`.

#![recursion_limit="256"]
#![warn(missing_docs)]

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, SystemTimeError, UNIX_EPOCH};

use parity_scale_codec::{Encode, Decode};
use futures::{select, channel::oneshot, future::{self, Either}, Future, FutureExt};
use futures_timer::Delay;
use kvdb_rocksdb::{Database, DatabaseConfig};
use kvdb::{KeyValueDB, DBTransaction};

use polkadot_primitives::v1::{
	Hash, AvailableData, BlockNumber, CandidateEvent, ErasureChunk, ValidatorIndex, CandidateHash,
};
use polkadot_subsystem::{
	FromOverseer, OverseerSignal, SubsystemError, Subsystem, SubsystemContext, SpawnedSubsystem,
	ActiveLeavesUpdate,
	errors::{ChainApiError, RuntimeApiError},
};
use polkadot_node_subsystem_util::metrics::{self, prometheus};
use polkadot_subsystem::messages::{
	AllMessages, AvailabilityStoreMessage, ChainApiMessage, RuntimeApiMessage, RuntimeApiRequest,
};

const LOG_TARGET: &str = "availability";

mod columns {
	pub const DATA: u32 = 0;
	pub const META: u32 = 1;
	pub const NUM_COLUMNS: u32 = 2;
}

#[derive(Debug, thiserror::Error)]
#[allow(missing_docs)]
pub enum Error {
	#[error(transparent)]
	RuntimeApi(#[from] RuntimeApiError),

	#[error(transparent)]
	ChainApi(#[from] ChainApiError),

	#[error(transparent)]
	Erasure(#[from] erasure::Error),

	#[error(transparent)]
	Io(#[from] io::Error),

	#[error(transparent)]
	Oneshot(#[from] oneshot::Canceled),

	#[error(transparent)]
	Subsystem(#[from] SubsystemError),

	#[error(transparent)]
	Time(#[from] SystemTimeError),

	#[error("Custom databases are not supported")]
	CustomDatabase,
}

impl Error {
	fn trace(&self) {
		match self {
			// don't spam the log with spurious errors
			Self::RuntimeApi(_) |
			Self::Oneshot(_) => tracing::debug!(target: LOG_TARGET, err = ?self),
			// it's worth reporting otherwise
			_ => tracing::warn!(target: LOG_TARGET, err = ?self),
		}
	}
}

/// A wrapper type for delays.
#[derive(Debug, Decode, Encode, Eq)]
enum PruningDelay {
	/// This pruning should be triggered after this `Duration` from UNIX_EPOCH.
	In(Duration),

	/// Data is in the state where it has no expiration.
	Indefinite,
}

impl PruningDelay {
	fn now() -> Result<Self, Error> {
		Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.into())
	}

	fn into_the_future(duration: Duration) -> Result<Self, Error> {
		Ok(Self::In(SystemTime::now().duration_since(UNIX_EPOCH)? + duration))
	}

	fn as_duration(&self) -> Option<Duration> {
		match self {
			PruningDelay::In(d) => Some(*d),
			PruningDelay::Indefinite => None,
		}
	}
}

impl From<Duration> for PruningDelay {
	fn from(d: Duration) -> Self {
		Self::In(d)
	}
}

impl PartialEq for PruningDelay {
	fn eq(&self, other: &Self) -> bool {
		match (self, other) {
			(PruningDelay::In(this), PruningDelay::In(that)) => {this == that},
			(PruningDelay::Indefinite, PruningDelay::Indefinite) => true,
			_ => false,
		}
	}
}

impl PartialOrd for PruningDelay {
	fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
		match (self, other) {
			(PruningDelay::In(this), PruningDelay::In(that)) => this.partial_cmp(that),
			(PruningDelay::In(_), PruningDelay::Indefinite) => Some(Ordering::Less),
			(PruningDelay::Indefinite, PruningDelay::In(_)) => Some(Ordering::Greater),
			(PruningDelay::Indefinite, PruningDelay::Indefinite) => Some(Ordering::Equal),
		}
	}
}

impl Ord for PruningDelay {
	fn cmp(&self, other: &Self) -> Ordering {
		match (self, other) {
			(PruningDelay::In(this), PruningDelay::In(that)) => this.cmp(that),
			(PruningDelay::In(_), PruningDelay::Indefinite) => Ordering::Less,
			(PruningDelay::Indefinite, PruningDelay::In(_)) => Ordering::Greater,
			(PruningDelay::Indefinite, PruningDelay::Indefinite) => Ordering::Equal,
		}
	}
}

/// A key for chunk pruning records.
const CHUNK_PRUNING_KEY: [u8; 14] = *b"chunks_pruning";

/// A key for PoV pruning records.
const POV_PRUNING_KEY: [u8; 11] = *b"pov_pruning";

/// A key for a cached value of next scheduled PoV pruning.
const NEXT_POV_PRUNING: [u8; 16] = *b"next_pov_pruning";

/// A key for a cached value of next scheduled chunk pruning.
const NEXT_CHUNK_PRUNING: [u8; 18] = *b"next_chunk_pruning";

/// The following constants are used under normal conditions:

/// Stored block is kept available for 1 hour.
const KEEP_STORED_BLOCK_FOR: Duration = Duration::from_secs(60 * 60);

/// Finalized block is kept for 1 day.
const KEEP_FINALIZED_BLOCK_FOR: Duration = Duration::from_secs(24 * 60 * 60);

/// Keep chunk of the finalized block for 1 day + 1 hour.
const KEEP_FINALIZED_CHUNK_FOR: Duration = Duration::from_secs(25 * 60 * 60);

/// At which point in time since UNIX_EPOCH we need to wakeup and do next pruning of blocks.
/// Essenially this is the first element in the sorted array of pruning data,
/// we just want to cache it here to avoid lifting the whole array just to look at the head.
///
/// This record exists under `NEXT_POV_PRUNING` key, if it does not either:
///  a) There are no records and nothing has to be pruned.
///  b) There are records but all of them are in `Included` state and do not have exact time to
///     be pruned.
#[derive(Decode, Encode)]
struct NextPoVPruning(Duration);

impl NextPoVPruning {
	// After which duration from `now` this should fire.
	fn should_fire_in(&self) -> Result<Duration, Error> {
		Ok(self.0.checked_sub(SystemTime::now().duration_since(UNIX_EPOCH)?).unwrap_or_default())
	}
}

/// At which point in time since UNIX_EPOCH we need to wakeup and do next pruning of chunks.
/// Essentially this is the first element in the sorted array of pruning data,
/// we just want to cache it here to avoid lifting the whole array just to look at the head.
///
/// This record exists under `NEXT_CHUNK_PRUNING` key, if it does not either:
///  a) There are no records and nothing has to be pruned.
///  b) There are records but all of them are in `Included` state and do not have exact time to
///     be pruned.
#[derive(Decode, Encode)]
struct NextChunkPruning(Duration);

impl NextChunkPruning {
	// After which amount of seconds into the future from `now` this should fire.
	fn should_fire_in(&self) -> Result<Duration, Error> {
		Ok(self.0.checked_sub(SystemTime::now().duration_since(UNIX_EPOCH)?).unwrap_or_default())
	}
}

/// Struct holding pruning timing configuration.
/// The only purpose of this structure is to use different timing
/// configurations in production and in testing.
#[derive(Clone)]
struct PruningConfig {
	/// How long should a stored block stay available.
	keep_stored_block_for: Duration,

	/// How long should a finalized block stay available.
	keep_finalized_block_for: Duration,

	/// How long should a chunk of a finalized block stay available.
	keep_finalized_chunk_for: Duration,
}

impl Default for PruningConfig {
	fn default() -> Self {
		Self {
			keep_stored_block_for: KEEP_STORED_BLOCK_FOR,
			keep_finalized_block_for: KEEP_FINALIZED_BLOCK_FOR,
			keep_finalized_chunk_for: KEEP_FINALIZED_CHUNK_FOR,
		}
	}
}

#[derive(Debug, Decode, Encode, Eq, PartialEq)]
enum CandidateState {
	Stored,
	Included,
	Finalized,
}

#[derive(Debug, Decode, Encode, Eq)]
struct PoVPruningRecord {
	candidate_hash: CandidateHash,
	block_number: BlockNumber,
	candidate_state: CandidateState,
	prune_at: PruningDelay,
}

impl PartialEq for PoVPruningRecord {
	fn eq(&self, other: &Self) -> bool {
		self.candidate_hash == other.candidate_hash
	}
}

impl Ord for PoVPruningRecord {
	fn cmp(&self, other: &Self) -> Ordering {
		if self.candidate_hash == other.candidate_hash {
			return Ordering::Equal;
		}

		self.prune_at.cmp(&other.prune_at)
	}
}

impl PartialOrd for PoVPruningRecord {
	fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
		Some(self.cmp(other))
	}
}

#[derive(Debug, Decode, Encode, Eq)]
struct ChunkPruningRecord {
	candidate_hash: CandidateHash,
	block_number: BlockNumber,
	candidate_state: CandidateState,
	chunk_index: u32,
	prune_at: PruningDelay,
}

impl PartialEq for ChunkPruningRecord {
	fn eq(&self, other: &Self) -> bool {
		self.candidate_hash == other.candidate_hash &&
			self.chunk_index == other.chunk_index
	}
}

impl Ord for ChunkPruningRecord {
	fn cmp(&self, other: &Self) -> Ordering {
		if self.candidate_hash == other.candidate_hash {
			return Ordering::Equal;
		}

		self.prune_at.cmp(&other.prune_at)
	}
}

impl PartialOrd for ChunkPruningRecord {
	fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
		Some(self.cmp(other))
	}
}

/// An implementation of the Availability Store subsystem.
pub struct AvailabilityStoreSubsystem {
	pruning_config: PruningConfig,
	inner: Arc<dyn KeyValueDB>,
	metrics: Metrics,
}

impl AvailabilityStoreSubsystem {
	// Perform pruning of PoVs
	#[tracing::instrument(level = "trace", skip(self), fields(subsystem = LOG_TARGET))]
	fn prune_povs(&self) -> Result<(), Error> {
		let _timer = self.metrics.time_prune_povs();

		let mut tx = DBTransaction::new();
		let mut pov_pruning = pov_pruning(&self.inner).unwrap_or_default();
		let now = PruningDelay::now()?;

		tracing::trace!(target: LOG_TARGET, "Pruning PoVs");
		let outdated_records_count = pov_pruning.iter()
			.take_while(|r| r.prune_at <= now)
			.count();

		for record in pov_pruning.drain(..outdated_records_count) {
			tracing::trace!(target: LOG_TARGET, record = ?record, "Removing record");
			tx.delete(
				columns::DATA,
				available_data_key(&record.candidate_hash).as_slice(),
			);
		}

		put_pov_pruning(&self.inner, Some(tx), pov_pruning)?;

		Ok(())
	}

	// Perform pruning of chunks.
	#[tracing::instrument(level = "trace", skip(self), fields(subsystem = LOG_TARGET))]
	fn prune_chunks(&self) -> Result<(), Error> {
		let _timer = self.metrics.time_prune_chunks();

		let mut tx = DBTransaction::new();
		let mut chunk_pruning = chunk_pruning(&self.inner).unwrap_or_default();
		let now = PruningDelay::now()?;

		tracing::trace!(target: LOG_TARGET, "Pruning Chunks");
		let outdated_records_count = chunk_pruning.iter()
			.take_while(|r| r.prune_at <= now)
			.count();

		for record in chunk_pruning.drain(..outdated_records_count) {
			tracing::trace!(target: LOG_TARGET, record = ?record, "Removing record");
			tx.delete(
				columns::DATA,
				erasure_chunk_key(&record.candidate_hash, record.chunk_index).as_slice(),
			);
		}

		put_chunk_pruning(&self.inner, Some(tx), chunk_pruning)?;

		Ok(())
	}

	// Return a `Future` that either resolves when another PoV pruning has to happen
	// or is indefinitely `pending` in case no pruning has to be done.
	// Just a helper to `select` over multiple things at once.
	#[tracing::instrument(level = "trace", skip(self), fields(subsystem = LOG_TARGET))]
	fn maybe_prune_povs(&self) -> Result<impl Future<Output = ()>, Error> {
		let future = match get_next_pov_pruning_time(&self.inner) {
			Some(pruning) => {
				Either::Left(Delay::new(pruning.should_fire_in()?))
			}
			None => Either::Right(future::pending::<()>()),
		};

		Ok(future)
	}

	// Return a `Future` that either resolves when another chunk pruning has to happen
	// or is indefinitely `pending` in case no pruning has to be done.
	// Just a helper to `select` over multiple things at once.
	#[tracing::instrument(level = "trace", skip(self), fields(subsystem = LOG_TARGET))]
	fn maybe_prune_chunks(&self) -> Result<impl Future<Output = ()>, Error> {
		let future = match get_next_chunk_pruning_time(&self.inner) {
			Some(pruning) => {
				Either::Left(Delay::new(pruning.should_fire_in()?))
			}
			None => Either::Right(future::pending::<()>()),
		};

		Ok(future)
	}
}

fn available_data_key(candidate_hash: &CandidateHash) -> Vec<u8> {
	(candidate_hash, 0i8).encode()
}

fn erasure_chunk_key(candidate_hash: &CandidateHash, index: u32) -> Vec<u8> {
	(candidate_hash, index, 0i8).encode()
}

#[derive(Encode, Decode)]
struct StoredAvailableData {
	data: AvailableData,
	n_validators: u32,
}

/// Configuration for the availability store.
pub struct Config {
	/// Total cache size in megabytes. If `None` the default (128 MiB per column) is used.
	pub cache_size: Option<usize>,
	/// Path to the database.
	pub path: PathBuf,
}

impl std::convert::TryFrom<sc_service::config::DatabaseConfig> for Config {
	type Error = Error;

	fn try_from(config: sc_service::config::DatabaseConfig) -> Result<Self, Self::Error> {
		let path = config.path().ok_or(Error::CustomDatabase)?;

		Ok(Self {
			// substrate cache size is improper here; just use the default
			cache_size: None,
			// DB path is a sub-directory of substrate db path to give two properties:
			// 1: column numbers don't conflict with substrate
			// 2: commands like purge-chain work without further changes
			path: path.join("parachains").join("av-store"),
		})
	}
}

impl AvailabilityStoreSubsystem {
	/// Create a new `AvailabilityStoreSubsystem` with a given config on disk.
	pub fn new_on_disk(config: Config, metrics: Metrics) -> io::Result<Self> {
		let mut db_config = DatabaseConfig::with_columns(columns::NUM_COLUMNS);

		if let Some(cache_size) = config.cache_size {
			let mut memory_budget = HashMap::new();

			for i in 0..columns::NUM_COLUMNS {
				memory_budget.insert(i, cache_size / columns::NUM_COLUMNS as usize);
			}
			db_config.memory_budget = memory_budget;
		}

		let path = config.path.to_str().ok_or_else(|| io::Error::new(
			io::ErrorKind::Other,
			format!("Bad database path: {:?}", config.path),
		))?;

		std::fs::create_dir_all(&path)?;
		let db = Database::open(&db_config, &path)?;

		Ok(Self {
			pruning_config: PruningConfig::default(),
			inner: Arc::new(db),
			metrics,
		})
	}

	#[cfg(test)]
	fn new_in_memory(inner: Arc<dyn KeyValueDB>, pruning_config: PruningConfig) -> Self {
		Self {
			pruning_config,
			inner,
			metrics: Metrics(None),
		}
	}
}

fn get_next_pov_pruning_time(db: &Arc<dyn KeyValueDB>) -> Option<NextPoVPruning> {
	query_inner(db, columns::META, &NEXT_POV_PRUNING)
}

fn get_next_chunk_pruning_time(db: &Arc<dyn KeyValueDB>) -> Option<NextChunkPruning> {
	query_inner(db, columns::META, &NEXT_CHUNK_PRUNING)
}

#[tracing::instrument(skip(subsystem, ctx), fields(subsystem = LOG_TARGET))]
async fn run<Context>(mut subsystem: AvailabilityStoreSubsystem, mut ctx: Context)
where
	Context: SubsystemContext<Message=AvailabilityStoreMessage>,
{
	loop {
		let res = run_iteration(&mut subsystem, &mut ctx).await;
		match res {
			Err(e) => {
				e.trace();
			}
			Ok(true) => {
				tracing::info!(target: LOG_TARGET, "received `Conclude` signal, exiting");
				break;
			},
			Ok(false) => continue,
		}
	}
}

#[tracing::instrument(level = "trace", skip(subsystem, ctx), fields(subsystem = LOG_TARGET))]
async fn run_iteration<Context>(subsystem: &mut AvailabilityStoreSubsystem, ctx: &mut Context)
	-> Result<bool, Error>
where
	Context: SubsystemContext<Message=AvailabilityStoreMessage>,
{
	// Every time the following two methods are called a read from DB is performed.
	// But given that these are very small values which are essentially a newtype
	// wrappers around `Duration` (`NextChunkPruning` and `NextPoVPruning`) and also the
	// fact of the frequent reads itself we assume these to end up cached in the memory
	// anyway and thus these db reads to be reasonably fast.
	let pov_pruning_time = subsystem.maybe_prune_povs()?;
	let chunk_pruning_time = subsystem.maybe_prune_chunks()?;

	let mut pov_pruning_time = pov_pruning_time.fuse();
	let mut chunk_pruning_time = chunk_pruning_time.fuse();

	select! {
		incoming = ctx.recv().fuse() => {
			match incoming? {
				FromOverseer::Signal(OverseerSignal::Conclude) => return Ok(true),
				FromOverseer::Signal(OverseerSignal::ActiveLeaves(
					ActiveLeavesUpdate { activated, .. })
				) => {
					for (activated, _span) in activated.into_iter() {
						process_block_activated(ctx, &subsystem.inner, activated, &subsystem.metrics).await?;
					}
				}
				FromOverseer::Signal(OverseerSignal::BlockFinalized(_hash, number)) => {
					process_block_finalized(subsystem, &subsystem.inner, number).await?;
				}
				FromOverseer::Communication { msg } => {
					process_message(subsystem, ctx, msg).await?;
				}
			}
		}
		_ = pov_pruning_time => {
			subsystem.prune_povs()?;
		}
		_ = chunk_pruning_time => {
			subsystem.prune_chunks()?;
		}
		complete => return Ok(true),
	}

	Ok(false)
}

/// As soon as certain block is finalized its pruning records and records of all
/// blocks that we keep that are `older` than the block in question have to be updated.
///
/// The state of data has to be changed from
/// `CandidateState::Included` to `CandidateState::Finalized` and their pruning times have
/// to be updated to `now` + keep_finalized_{block, chunk}_for`.
#[tracing::instrument(level = "trace", skip(subsystem, db), fields(subsystem = LOG_TARGET))]
async fn process_block_finalized(
	subsystem: &AvailabilityStoreSubsystem,
	db: &Arc<dyn KeyValueDB>,
	block_number: BlockNumber,
) -> Result<(), Error> {
	let _timer = subsystem.metrics.time_process_block_finalized();

	if let Some(mut pov_pruning) = pov_pruning(db) {
		// Since the records are sorted by time in which they need to be pruned and not by block
		// numbers we have to iterate through the whole collection here.
		for record in pov_pruning.iter_mut() {
			if record.block_number <= block_number {
				tracing::trace!(
					target: LOG_TARGET,
					block_number = %record.block_number,
					"Updating pruning record for finalized block",
				);

				record.prune_at = PruningDelay::into_the_future(
					subsystem.pruning_config.keep_finalized_block_for
				)?;
				record.candidate_state = CandidateState::Finalized;
			}
		}

		put_pov_pruning(db, None, pov_pruning)?;
	}

	if let Some(mut chunk_pruning) = chunk_pruning(db) {
		for record in chunk_pruning.iter_mut() {
			if record.block_number <= block_number {
				tracing::trace!(
					target: LOG_TARGET,
					block_number = %record.block_number,
					"Updating chunk pruning record for finalized block",
				);

				record.prune_at = PruningDelay::into_the_future(
					subsystem.pruning_config.keep_finalized_chunk_for
				)?;
				record.candidate_state = CandidateState::Finalized;
			}
		}

		put_chunk_pruning(db, None, chunk_pruning)?;
	}

	Ok(())
}

#[tracing::instrument(level = "trace", skip(ctx, db, metrics), fields(subsystem = LOG_TARGET))]
async fn process_block_activated<Context>(
	ctx: &mut Context,
	db: &Arc<dyn KeyValueDB>,
	hash: Hash,
	metrics: &Metrics,
) -> Result<(), Error>
where
	Context: SubsystemContext<Message=AvailabilityStoreMessage>
{
	let _timer = metrics.time_block_activated();

	let events = match request_candidate_events(ctx, hash).await {
		Ok(events) => events,
		Err(err) => {
			tracing::debug!(target: LOG_TARGET, err = ?err, "requesting candidate events failed");
			return Ok(());
		}
	};

	tracing::trace!(target: LOG_TARGET, hash = %hash, "block activated");
	let mut included = HashSet::new();

	for event in events.into_iter() {
		if let CandidateEvent::CandidateIncluded(receipt, _) = event {
			tracing::trace!(
				target: LOG_TARGET,
				hash = %receipt.hash(),
				"Candidate {:?} was included", receipt.hash(),
			);
			included.insert(receipt.hash());
		}
	}

	if let Some(mut pov_pruning) = pov_pruning(db) {
		for record in pov_pruning.iter_mut() {
			if included.contains(&record.candidate_hash) {
				record.prune_at = PruningDelay::Indefinite;
				record.candidate_state = CandidateState::Included;
			}
		}

		pov_pruning.sort();

		put_pov_pruning(db, None, pov_pruning)?;
	}

	if let Some(mut chunk_pruning) = chunk_pruning(db) {
		for record in chunk_pruning.iter_mut() {
			if included.contains(&record.candidate_hash) {
				record.prune_at = PruningDelay::Indefinite;
				record.candidate_state = CandidateState::Included;
			}
		}

		chunk_pruning.sort();

		put_chunk_pruning(db, None, chunk_pruning)?;
	}

	Ok(())
}

#[tracing::instrument(level = "trace", skip(ctx), fields(subsystem = LOG_TARGET))]
async fn request_candidate_events<Context>(
	ctx: &mut Context,
	hash: Hash,
) -> Result<Vec<CandidateEvent>, Error>
where
	Context: SubsystemContext<Message=AvailabilityStoreMessage>
{
	let (tx, rx) = oneshot::channel();

	let msg = AllMessages::RuntimeApi(RuntimeApiMessage::Request(
		hash,
		RuntimeApiRequest::CandidateEvents(tx),
	));

	ctx.send_message(msg.into()).await;

	Ok(rx.await??)
}

#[tracing::instrument(level = "trace", skip(subsystem, ctx), fields(subsystem = LOG_TARGET))]
async fn process_message<Context>(
	subsystem: &mut AvailabilityStoreSubsystem,
	ctx: &mut Context,
	msg: AvailabilityStoreMessage,
) -> Result<(), Error>
where
	Context: SubsystemContext<Message=AvailabilityStoreMessage>
{
	use AvailabilityStoreMessage::*;

	let _timer = subsystem.metrics.time_process_message();

	match msg {
		QueryAvailableData(hash, tx) => {
			tx.send(available_data(&subsystem.inner, &hash).map(|d| d.data)).map_err(|_| oneshot::Canceled)?;
		}
		QueryDataAvailability(hash, tx) => {
			let result = available_data(&subsystem.inner, &hash).is_some();

			tracing::trace!(
				target: LOG_TARGET,
				candidate_hash = ?hash,
				availability = ?result,
				"Queried data availability",
			);

			tx.send(result).map_err(|_| oneshot::Canceled)?;
		}
		QueryChunk(hash, id, tx) => {
			tx.send(get_chunk(subsystem, &hash, id)?).map_err(|_| oneshot::Canceled)?;
		}
		QueryChunkAvailability(hash, id, tx) => {
			let result = get_chunk(subsystem, &hash, id).map(|r| r.is_some());

			tracing::trace!(
				target: LOG_TARGET,
				candidate_hash = ?hash,
				availability = ?result,
				"Queried chunk availability",
			);

			tx.send(result?).map_err(|_| oneshot::Canceled)?;
		}
		StoreChunk { candidate_hash, relay_parent, validator_index, chunk, tx } => {
			let chunk_index = chunk.index;
			// Current block number is relay_parent block number + 1.
			let block_number = get_block_number(ctx, relay_parent).await? + 1;
			let result = store_chunk(subsystem, &candidate_hash, validator_index, chunk, block_number);

			tracing::trace!(
				target: LOG_TARGET,
				%chunk_index,
				?candidate_hash,
				%block_number,
				?result,
				"Stored chunk",
			);

			match result {
				Err(e) => {
					tx.send(Err(())).map_err(|_| oneshot::Canceled)?;
					return Err(e);
				}
				Ok(()) => {
					tx.send(Ok(())).map_err(|_| oneshot::Canceled)?;
				}
			}
		}
		StoreAvailableData(hash, id, n_validators, av_data, tx) => {
			let result = store_available_data(subsystem, &hash, id, n_validators, av_data);

			tracing::trace!(target: LOG_TARGET, candidate_hash = ?hash, ?result, "Stored available data");

			match result {
				Err(e) => {
					tx.send(Err(())).map_err(|_| oneshot::Canceled)?;
					return Err(e);
				}
				Ok(()) => {
					tx.send(Ok(())).map_err(|_| oneshot::Canceled)?;
				}
			}
		}
	}

	Ok(())
}

fn available_data(
	db: &Arc<dyn KeyValueDB>,
	candidate_hash: &CandidateHash,
) -> Option<StoredAvailableData> {
	query_inner(db, columns::DATA, &available_data_key(candidate_hash))
}

fn pov_pruning(db: &Arc<dyn KeyValueDB>) -> Option<Vec<PoVPruningRecord>> {
	query_inner(db, columns::META, &POV_PRUNING_KEY)
}

fn chunk_pruning(db: &Arc<dyn KeyValueDB>) -> Option<Vec<ChunkPruningRecord>> {
	query_inner(db, columns::META, &CHUNK_PRUNING_KEY)
}

#[tracing::instrument(level = "trace", skip(db, tx), fields(subsystem = LOG_TARGET))]
fn put_pov_pruning(
	db: &Arc<dyn KeyValueDB>,
	tx: Option<DBTransaction>,
	mut pov_pruning: Vec<PoVPruningRecord>,
) -> Result<(), Error> {
	let mut tx = tx.unwrap_or_default();

	pov_pruning.sort();

	tx.put_vec(
		columns::META,
		&POV_PRUNING_KEY,
		pov_pruning.encode(),
	);

	match pov_pruning.get(0) {
		// We want to wake up in case we have some records that are not scheduled to be kept
		// indefinitely (data is included and waiting to move to the finalized state) and so
		// the is at least one value that is not `PruningDelay::Indefinite`.
		Some(PoVPruningRecord { prune_at: PruningDelay::In(prune_at), .. }) => {
			tx.put_vec(
				columns::META,
				&NEXT_POV_PRUNING,
				NextPoVPruning(*prune_at).encode(),
			);
		}
		_ => {
			// If there is no longer any records, delete the cached pruning time record.
			tx.delete(
				columns::META,
				&NEXT_POV_PRUNING,
			);
		}
	}

	db.write(tx)?;

	Ok(())
}

#[tracing::instrument(level = "trace", skip(db, tx), fields(subsystem = LOG_TARGET))]
fn put_chunk_pruning(
	db: &Arc<dyn KeyValueDB>,
	tx: Option<DBTransaction>,
	mut chunk_pruning: Vec<ChunkPruningRecord>,
) -> Result<(), Error> {
	let mut tx = tx.unwrap_or_default();

	chunk_pruning.sort();

	tx.put_vec(
		columns::META,
		&CHUNK_PRUNING_KEY,
		chunk_pruning.encode(),
	);

	match chunk_pruning.get(0) {
		Some(ChunkPruningRecord { prune_at: PruningDelay::In(prune_at), .. }) => {
			tx.put_vec(
				columns::META,
				&NEXT_CHUNK_PRUNING,
				NextChunkPruning(*prune_at).encode(),
			);
		}
		_ => {
			tx.delete(
				columns::META,
				&NEXT_CHUNK_PRUNING,
			);
		}
	}

	db.write(tx)?;

	Ok(())
}

// produces a block number by block's hash.
// in the the event of an invalid `block_hash`, returns `Ok(0)`
async fn get_block_number<Context>(
	ctx: &mut Context,
	block_hash: Hash,
) -> Result<BlockNumber, Error>
where
	Context: SubsystemContext<Message=AvailabilityStoreMessage>,
{
	let (tx, rx) = oneshot::channel();

	ctx.send_message(AllMessages::ChainApi(ChainApiMessage::BlockNumber(block_hash, tx))).await;

	Ok(rx.await??.map(|number| number).unwrap_or_default())
}

#[tracing::instrument(level = "trace", skip(subsystem, available_data), fields(subsystem = LOG_TARGET))]
fn store_available_data(
	subsystem: &mut AvailabilityStoreSubsystem,
	candidate_hash: &CandidateHash,
	id: Option<ValidatorIndex>,
	n_validators: u32,
	available_data: AvailableData,
) -> Result<(), Error> {
	let _timer = subsystem.metrics.time_store_available_data();

	let mut tx = DBTransaction::new();

	let block_number = available_data.validation_data.block_number;

	if let Some(index) = id {
		let chunks = get_chunks(&available_data, n_validators as usize, &subsystem.metrics)?;
		store_chunk(
			subsystem,
			candidate_hash,
			n_validators,
			chunks[index as usize].clone(),
			block_number,
		)?;
	}

	let stored_data = StoredAvailableData {
		data: available_data,
		n_validators,
	};

	let mut pov_pruning = pov_pruning(&subsystem.inner).unwrap_or_default();
	let prune_at = PruningDelay::into_the_future(subsystem.pruning_config.keep_stored_block_for)?;

	if let Some(next_pruning) = prune_at.as_duration() {
		tx.put_vec(
			columns::META,
			&NEXT_POV_PRUNING,
			NextPoVPruning(next_pruning).encode(),
		);
	}

	let pruning_record = PoVPruningRecord {
		candidate_hash: *candidate_hash,
		block_number,
		candidate_state: CandidateState::Stored,
		prune_at,
	};

	let idx = pov_pruning.binary_search(&pruning_record).unwrap_or_else(|insert_idx| insert_idx);

	pov_pruning.insert(idx, pruning_record);

	tx.put_vec(
		columns::DATA,
		available_data_key(&candidate_hash).as_slice(),
		stored_data.encode(),
	);

	tx.put_vec(
		columns::META,
		&POV_PRUNING_KEY,
		pov_pruning.encode(),
	);

	subsystem.inner.write(tx)?;

	Ok(())
}

#[tracing::instrument(level = "trace", skip(subsystem), fields(subsystem = LOG_TARGET))]
fn store_chunk(
	subsystem: &mut AvailabilityStoreSubsystem,
	candidate_hash: &CandidateHash,
	_n_validators: u32,
	chunk: ErasureChunk,
	block_number: BlockNumber,
) -> Result<(), Error> {
	let _timer = subsystem.metrics.time_store_chunk();

	let mut tx = DBTransaction::new();

	let dbkey = erasure_chunk_key(candidate_hash, chunk.index);

	let mut chunk_pruning = chunk_pruning(&subsystem.inner).unwrap_or_default();
	let prune_at = PruningDelay::into_the_future(subsystem.pruning_config.keep_stored_block_for)?;

	if let Some(delay) = prune_at.as_duration() {
		tx.put_vec(
			columns::META,
			&NEXT_CHUNK_PRUNING,
			NextChunkPruning(delay).encode(),
		);
	}

	let pruning_record = ChunkPruningRecord {
		candidate_hash: candidate_hash.clone(),
		block_number,
		candidate_state: CandidateState::Stored,
		chunk_index: chunk.index,
		prune_at,
	};

	let idx = chunk_pruning.binary_search(&pruning_record).unwrap_or_else(|insert_idx| insert_idx);

	chunk_pruning.insert(idx, pruning_record);

	tx.put_vec(
		columns::DATA,
		&dbkey,
		chunk.encode(),
	);

	tx.put_vec(
		columns::META,
		&CHUNK_PRUNING_KEY,
		chunk_pruning.encode(),
	);

	subsystem.inner.write(tx)?;

	Ok(())
}

#[tracing::instrument(level = "trace", skip(subsystem), fields(subsystem = LOG_TARGET))]
fn get_chunk(
	subsystem: &mut AvailabilityStoreSubsystem,
	candidate_hash: &CandidateHash,
	index: u32,
) -> Result<Option<ErasureChunk>, Error> {
	let _timer = subsystem.metrics.time_get_chunk();

	if let Some(chunk) = query_inner(
		&subsystem.inner,
		columns::DATA,
		&erasure_chunk_key(candidate_hash, index)
	) {
		return Ok(Some(chunk));
	}

	if let Some(data) = available_data(&subsystem.inner, candidate_hash) {
		let mut chunks = get_chunks(&data.data, data.n_validators as usize, &subsystem.metrics)?;
		let desired_chunk = chunks.get(index as usize).cloned();
		for chunk in chunks.drain(..) {
			store_chunk(
				subsystem,
				candidate_hash,
				data.n_validators,
				chunk,
				data.data.validation_data.block_number,
			)?;
		}
		return Ok(desired_chunk);
	}

	Ok(None)
}

fn query_inner<D: Decode>(
	db: &Arc<dyn KeyValueDB>,
	column: u32,
	key: &[u8],
) -> Option<D> {
	match db.get(column, key) {
		Ok(Some(raw)) => {
			let res = D::decode(&mut &raw[..]).expect("all stored data serialized correctly; qed");
			Some(res)
		}
		Ok(None) => None,
		Err(e) => {
			tracing::warn!(target: LOG_TARGET, err = ?e, "Error reading from the availability store");
			None
		}
	}
}

impl<Context> Subsystem<Context> for AvailabilityStoreSubsystem
where
	Context: SubsystemContext<Message = AvailabilityStoreMessage>,
{
	fn start(self, ctx: Context) -> SpawnedSubsystem {
		let future = run(self, ctx)
			.map(|_| Ok(()))
			.boxed();

		SpawnedSubsystem {
			name: "availability-store-subsystem",
			future,
		}
	}
}

#[tracing::instrument(level = "trace", skip(metrics), fields(subsystem = LOG_TARGET))]
fn get_chunks(data: &AvailableData, n_validators: usize, metrics: &Metrics) -> Result<Vec<ErasureChunk>, Error> {
	let chunks = erasure::obtain_chunks_v1(n_validators, data)?;
	metrics.on_chunks_received(chunks.len());
	let branches = erasure::branches(chunks.as_ref());

	Ok(chunks
		.iter()
		.zip(branches.map(|(proof, _)| proof))
		.enumerate()
		.map(|(index, (chunk, proof))| ErasureChunk {
			chunk: chunk.clone(),
			proof,
			index: index as u32,
		})
		.collect()
	)
}

#[derive(Clone)]
struct MetricsInner {
	received_availability_chunks_total: prometheus::Counter<prometheus::U64>,
	prune_povs: prometheus::Histogram,
	prune_chunks: prometheus::Histogram,
	process_block_finalized: prometheus::Histogram,
	block_activated: prometheus::Histogram,
	process_message: prometheus::Histogram,
	store_available_data: prometheus::Histogram,
	store_chunk: prometheus::Histogram,
	get_chunk: prometheus::Histogram,
}

/// Availability metrics.
#[derive(Default, Clone)]
pub struct Metrics(Option<MetricsInner>);

impl Metrics {
	fn on_chunks_received(&self, count: usize) {
		if let Some(metrics) = &self.0 {
			use core::convert::TryFrom as _;
			// assume usize fits into u64
			let by = u64::try_from(count).unwrap_or_default();
			metrics.received_availability_chunks_total.inc_by(by);
		}
	}

	/// Provide a timer for `prune_povs` which observes on drop.
	fn time_prune_povs(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.prune_povs.start_timer())
	}

	/// Provide a timer for `prune_chunks` which observes on drop.
	fn time_prune_chunks(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.prune_chunks.start_timer())
	}

	/// Provide a timer for `process_block_finalized` which observes on drop.
	fn time_process_block_finalized(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.process_block_finalized.start_timer())
	}

	/// Provide a timer for `block_activated` which observes on drop.
	fn time_block_activated(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.block_activated.start_timer())
	}

	/// Provide a timer for `process_message` which observes on drop.
	fn time_process_message(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.process_message.start_timer())
	}

	/// Provide a timer for `store_available_data` which observes on drop.
	fn time_store_available_data(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.store_available_data.start_timer())
	}

	/// Provide a timer for `store_chunk` which observes on drop.
	fn time_store_chunk(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.store_chunk.start_timer())
	}

	/// Provide a timer for `get_chunk` which observes on drop.
	fn time_get_chunk(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.get_chunk.start_timer())
	}
}

impl metrics::Metrics for Metrics {
	fn try_register(registry: &prometheus::Registry) -> Result<Self, prometheus::PrometheusError> {
		let metrics = MetricsInner {
			received_availability_chunks_total: prometheus::register(
				prometheus::Counter::new(
					"parachain_received_availability_chunks_total",
					"Number of availability chunks received.",
				)?,
				registry,
			)?,
			prune_povs: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"parachain_av_store_prune_povs",
						"Time spent within `av_store::prune_povs`",
					)
				)?,
				registry,
			)?,
			prune_chunks: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"parachain_av_store_prune_chunks",
						"Time spent within `av_store::prune_chunks`",
					)
				)?,
				registry,
			)?,
			process_block_finalized: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"parachain_av_store_process_block_finalized",
						"Time spent within `av_store::block_finalized`",
					)
				)?,
				registry,
			)?,
			block_activated: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"parachain_av_store_block_activated",
						"Time spent within `av_store::block_activated`",
					)
				)?,
				registry,
			)?,
			process_message: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"parachain_av_store_process_message",
						"Time spent within `av_store::process_message`",
					)
				)?,
				registry,
			)?,
			store_available_data: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"parachain_av_store_store_available_data",
						"Time spent within `av_store::store_available_data`",
					)
				)?,
				registry,
			)?,
			store_chunk: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"parachain_av_store_store_chunk",
						"Time spent within `av_store::store_chunk`",
					)
				)?,
				registry,
			)?,
			get_chunk: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"parachain_av_store_get_chunk",
						"Time spent within `av_store::get_chunk`",
					)
				)?,
				registry,
			)?,
		};
		Ok(Metrics(Some(metrics)))
	}
}

#[cfg(test)]
mod tests;
