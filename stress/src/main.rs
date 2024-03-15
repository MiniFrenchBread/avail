#[macro_use]
extern crate tracing;

use std::{error::Error, num::NonZeroU16, str::FromStr, time::Duration};

use avail_core::{header::HeaderExtension, AppExtrinsic, HeaderVersion};
use frame_system::limits::BlockLength;
use kate::{gridgen::EvaluationGrid, Seed};
use sp_core::H256;
use sp_runtime::{Perbill, SaturatedConversion};
use tokio::time::{sleep_until, Instant};
use tracing::Level;

pub fn get_empty_header(data_root: H256, version: HeaderVersion) -> HeaderExtension {
	use avail_core::{header::extension::v3, DataLookup};
	let empty_commitment: Vec<u8> = vec![0];
	let empty_app_lookup = DataLookup::new_empty();

	match version {
		HeaderVersion::V3 => {
			use avail_core::kate_commitment::v3::KateCommitment;
			let kate = KateCommitment::new(1, 4, data_root, empty_commitment);
			v3::HeaderExtension {
				app_lookup: empty_app_lookup,
				commitment: kate,
			}
			.into()
		},
	}
}

pub fn build_commitment(grid: &EvaluationGrid, log: bool) -> Result<Vec<u8>, String> {
	use kate::gridgen::AsBytes;
	use once_cell::sync::Lazy;

	// couscous has pp for degree upto 1024
	static PMP: Lazy<kate::pmp::m1_blst::M1NoPrecomp> =
		Lazy::new(kate::couscous::multiproof_params);

	let mut timer = std::time::Instant::now();

	let poly_grid = grid
		.make_polynomial_grid()
		.map_err(|e| format!("Make polynomial grid failed: {e:?}"))?;
	if log {
		info!("make polynomial used {:?}ms", timer.elapsed().as_millis());
		timer = std::time::Instant::now();
	}

	let extended_grid = poly_grid
		.extended_commitments(&*PMP, 2)
		.map_err(|e| format!("Grid extension failed: {e:?}"))?;
	if log {
		info!(
			"extend commitments used {:?}ms",
			timer.elapsed().as_millis()
		);
	}

	let mut commitment = Vec::new();
	for c in extended_grid.iter() {
		match c.to_bytes() {
			Ok(bytes) => commitment.extend(bytes),
			Err(e) => return Err(format!("Commitment serialization failed: {:?}", e)),
		}
	}

	Ok(commitment)
}

pub fn build_grid(
	app_extrinsics: &[AppExtrinsic],
	block_length: BlockLength,
	seed: Seed,
) -> Result<EvaluationGrid, String> {
	const MIN_WIDTH: usize = 4;
	let grid = EvaluationGrid::from_extrinsics(
		app_extrinsics.to_vec(),
		MIN_WIDTH,
		block_length.cols.0.saturated_into(), // even if we run on a u16 target this is fine
		block_length.rows.0.saturated_into(),
		seed,
	)
	.map_err(|e| format!("Grid construction failed: {e:?}"))?;

	Ok(grid)
}

fn build_extension(
	app_extrinsics: &[AppExtrinsic],
	data_root: H256,
	block_length: BlockLength,
	_block_number: u32,
	seed: Seed,
	version: HeaderVersion,
	log: bool,
) -> HeaderExtension {
	use avail_core::header::extension::v3;

	// Build the grid
	let mut timer = std::time::Instant::now();
	let maybe_grid = build_grid(app_extrinsics, block_length, seed);
	let grid = match maybe_grid {
		Ok(res) => {
			if log {
				info!("build grid used {:?}ms", timer.elapsed().as_millis());
				timer = std::time::Instant::now();
			}
			res
		},
		Err(message) => {
			error!("NODE_CRITICAL_ERROR_001 - A critical error has occurred: {message:?}.");
			error!("NODE_CRITICAL_ERROR_001 - If you see this, please warn Avail team and raise an issue.");
			return get_empty_header(data_root, version);
		},
	};
	match grid.extend_columns(NonZeroU16::new(2).expect("2>0")) {
		Ok(extended_grid) => {
			if log {
				info!("extended grid dims: {:?}", extended_grid.dims());
				info!("extend grid used {:?}ms", timer.elapsed().as_millis());
				timer = std::time::Instant::now();
			}
		},
		Err(message) => {
			error!("Error extending grid {:?}", message);
			return get_empty_header(data_root, version);
		},
	}

	// Build the commitment
	let maybe_commitment = build_commitment(&grid, log);
	let commitment = match maybe_commitment {
		Ok(res) => {
			if log {
				info!("build commitment used {:?}ms", timer.elapsed().as_millis());
			}
			res
		},
		Err(message) => {
			error!("NODE_CRITICAL_ERROR_002 - A critical error has occurred: {message:?}.");
			error!("NODE_CRITICAL_ERROR_002 - If you see this, please warn Avail team and raise an issue.");
			return get_empty_header(data_root, version);
		},
	};

	// Note that this uses the original dims, _not the extended ones_
	let rows = grid.dims().rows().get();
	let cols = grid.dims().cols().get();

	let app_lookup = grid.lookup().clone();

	if log {
		info!(
			"grid lookup len: {:?}, grid dims: {:?}, commit length: {:?}",
			grid.lookup().len(),
			grid.dims(),
			commitment.len()
		);
	}

	let header_extension = match version {
		HeaderVersion::V3 => {
			use avail_core::kate_commitment::v3::KateCommitment;
			let kate = KateCommitment::new(rows, cols, data_root, commitment);
			v3::HeaderExtension {
				app_lookup,
				commitment: kate,
			}
			.into()
		},
	};

	header_extension
}

fn build(
	app_extrinsics: Vec<AppExtrinsic>,
	data_root: H256,
	block_length: BlockLength,
	block_number: u32,
	version: HeaderVersion,
	log: bool,
) -> HeaderExtension {
	let seed: [u8; 32] = rand::random();

	build_extension(
		&app_extrinsics,
		data_root,
		block_length,
		block_number,
		seed,
		version,
		log,
	)
}

const N: usize = 64;
const RPS: u32 = 1;
const BLOB_SIZE: usize = 496 * 1024 * 16 - 10;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
	// enable backtraces
	std::env::set_var("RUST_BACKTRACE", "1");
	tracing_subscriber::fmt()
		.with_max_level(Level::from_str("info").unwrap())
		.init();

	let mut broadcast_txs = vec![];
	for _ in 0..N {
		let (task_tx, mut task_rx) = tokio::sync::mpsc::unbounded_channel();
		tokio::spawn(async move {
			let mut data = vec![];
			for _ in 0..BLOB_SIZE {
				data.push(rand::random());
			}
			let extrinsics = vec![AppExtrinsic {
				app_id: avail_core::AppId(0),
				data,
			}];

			let block_length =
				BlockLength::max_with_normal_ratio(10 * 1024 * 1024, Perbill::from_percent(75));
			while let Some(id) = task_rx.recv().await {
				let ts = Instant::now();
				build(
					extrinsics.clone(),
					H256::zero(),
					block_length.clone(),
					0,
					HeaderVersion::V3,
					id == 1,
				);
				info!(
					"task #{:?} done. time elapsed: {:?}ms.",
					id,
					ts.elapsed().as_millis()
				);
			}
		});
		broadcast_txs.push(task_tx);
	}

	let mut id = 0;
	let mut thread_id = 0;
	let mut ts = Instant::now();
	loop {
		for _ in 0..RPS {
			id += 1;
			if let Err(e) = broadcast_txs[thread_id].send(id) {
				info!("failed to send task #{:?}: {:?}", id, e);
			}
			info!("task #{:?} sent to thread #{:?}", id, thread_id);
			thread_id = (thread_id + 1) % N;
		}
		ts += Duration::from_secs(1);
		sleep_until(ts).await;
	}
}
