#[macro_use]
extern crate tracing;

use std::{error::Error, str::FromStr, time::Duration};

use avail_core::{header::HeaderExtension, AppExtrinsic, HeaderVersion};
use frame_system::limits::BlockLength;
use kate::{gridgen::EvaluationGrid, Seed};
use sp_core::H256;
use sp_runtime::SaturatedConversion;
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

pub fn build_commitment(grid: &EvaluationGrid) -> Result<Vec<u8>, String> {
	use kate::gridgen::AsBytes;
	use once_cell::sync::Lazy;

	// couscous has pp for degree upto 1024
	static PMP: Lazy<kate::pmp::m1_blst::M1NoPrecomp> =
		Lazy::new(kate::couscous::multiproof_params);

	let poly_grid = grid
		.make_polynomial_grid()
		.map_err(|e| format!("Make polynomial grid failed: {e:?}"))?;

	let extended_grid = poly_grid
		.extended_commitments(&*PMP, 2)
		.map_err(|e| format!("Grid extension failed: {e:?}"))?;

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
) -> HeaderExtension {
	use avail_base::metrics::avail::HeaderExtensionBuilderMetrics;
	use avail_core::header::extension::v3;

	let build_extension_start = std::time::Instant::now();

	// Build the grid
	let timer = std::time::Instant::now();
	let maybe_grid = build_grid(app_extrinsics, block_length, seed);
	// Evaluation Grid Build Time Metrics
	HeaderExtensionBuilderMetrics::observe_evaluation_grid_build_time(timer.elapsed());
	let grid = match maybe_grid {
		Ok(res) => res,
		Err(message) => {
			error!("NODE_CRITICAL_ERROR_001 - A critical error has occurred: {message:?}.");
			error!("NODE_CRITICAL_ERROR_001 - If you see this, please warn Avail team and raise an issue.");
			HeaderExtensionBuilderMetrics::observe_total_execution_time(
				build_extension_start.elapsed(),
			);
			return get_empty_header(data_root, version);
		},
	};
	info!("grid dims: {:?}", grid.dims());

	// Build the commitment
	let timer = std::time::Instant::now();
	let maybe_commitment = build_commitment(&grid);
	// Commitment Build Time Metrics
	HeaderExtensionBuilderMetrics::observe_commitment_build_time(timer.elapsed());
	let commitment = match maybe_commitment {
		Ok(res) => res,
		Err(message) => {
			error!("NODE_CRITICAL_ERROR_002 - A critical error has occurred: {message:?}.");
			error!("NODE_CRITICAL_ERROR_002 - If you see this, please warn Avail team and raise an issue.");
			HeaderExtensionBuilderMetrics::observe_total_execution_time(
				build_extension_start.elapsed(),
			);
			return get_empty_header(data_root, version);
		},
	};

	// Note that this uses the original dims, _not the extended ones_
	let rows = grid.dims().rows().get();
	let cols = grid.dims().cols().get();

	// Grid Metrics
	HeaderExtensionBuilderMetrics::observe_grid_rows(rows as f64);
	HeaderExtensionBuilderMetrics::observe_grid_cols(cols as f64);

	let app_lookup = grid.lookup().clone();

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

	// Total Execution Time Metrics
	HeaderExtensionBuilderMetrics::observe_total_execution_time(build_extension_start.elapsed());

	header_extension
}

fn build(
	app_extrinsics: Vec<AppExtrinsic>,
	data_root: H256,
	block_length: BlockLength,
	block_number: u32,
	version: HeaderVersion,
) -> HeaderExtension {
	let seed: [u8; 32] = [0; 32];

	build_extension(
		&app_extrinsics,
		data_root,
		block_length,
		block_number,
		seed,
		version,
	)
}

const N: usize = 64;
const RPS: u32 = 20;
const BLOB_SIZE: usize = 512 * 1024;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
	// enable backtraces
	std::env::set_var("RUST_BACKTRACE", "1");
	tracing_subscriber::fmt()
		.with_max_level(Level::from_str("info").unwrap())
		.init();

	let (result_tx, mut result_rx) = tokio::sync::mpsc::unbounded_channel();
	let mut broadcast_txs = vec![];
	for _ in 0..N {
		let (task_tx, mut task_rx) = tokio::sync::mpsc::unbounded_channel();
		let tx = result_tx.clone();
		tokio::spawn(async move {
			let extrinsics = vec![AppExtrinsic {
				app_id: avail_core::AppId(0),
				data: vec![0; BLOB_SIZE],
			}];
			let block_length = BlockLength::default();
			while let Some(id) = task_rx.recv().await {
				build(
					extrinsics.clone(),
					H256::zero(),
					block_length.clone(),
					0,
					HeaderVersion::V3,
				);
				if let Err(e) = tx.send(id) {
					println!("failed to send #{:?} result back: {:?}", id, e);
				}
			}
		});
		broadcast_txs.push(task_tx);
	}

	tokio::spawn(async move {
		while let Some(id) = result_rx.recv().await {
			log::info!("{:?} task done.", id);
		}
	});

	let mut id = 0;
	let mut thread_id = 0;
	loop {
		for _ in 0..RPS {
			id += 1;
			if let Err(e) = broadcast_txs[thread_id].send(id) {
				println!("failed to send task #{:?}: {:?}", id, e);
			}
			println!("sent task #{:?} to thread #{:?}", id, thread_id);
			thread_id = (thread_id + 1) % N;
		}
		tokio::time::sleep(Duration::from_secs(1)).await;
	}
}
