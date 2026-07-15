// Copyright 2026 The Grin Developers
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::core::core::hash::{Hash, Hashed};
use crate::core::core::{BlockHeader, OutputIdentifier};
use crate::core::ser::{self, PMMRable, Readable, Reader, Writeable, Writer};
use crate::store::ChainStore;
use crate::types::Tip;
use crate::util::ToHex;
use byteorder::{BigEndian, ByteOrder};
use croaring::{Bitmap, Portable};
use grin_util::secp::pedersen::RangeProof;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

const TREE_DIRS: [&str; 2] = ["output", "rangeproof"];
const KERNEL_DIR: &str = "kernel";
const APPEND_FILES: [&str; 3] = ["pmmr_hash.bin", "pmmr_data.bin", "pmmr_size.bin"];
const SIDECAR_FILES: [&str; 2] = ["pmmr_leaf.bin", "pmmr_prun.bin"];

#[derive(Clone, Debug)]
pub(crate) struct FileState {
	len: u64,
	checksum: Hash,
}

impl Writeable for FileState {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), ser::Error> {
		writer.write_u64(self.len)?;
		self.checksum.write(writer)
	}
}

impl Readable for FileState {
	fn read<R: Reader>(reader: &mut R) -> Result<Self, ser::Error> {
		Ok(Self {
			len: reader.read_u64()?,
			checksum: Hash::read(reader)?,
		})
	}
}

#[derive(Clone, Debug)]
pub(crate) struct TreeState {
	append_lens: [u64; 3],
	sidecars: [FileState; 2],
}

impl Writeable for TreeState {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), ser::Error> {
		for len in self.append_lens {
			writer.write_u64(len)?;
		}
		for file in &self.sidecars {
			file.write(writer)?;
		}
		Ok(())
	}
}

impl Readable for TreeState {
	fn read<R: Reader>(reader: &mut R) -> Result<Self, ser::Error> {
		Ok(Self {
			append_lens: [reader.read_u64()?, reader.read_u64()?, reader.read_u64()?],
			sidecars: [FileState::read(reader)?, FileState::read(reader)?],
		})
	}
}

#[derive(Clone, Debug)]
pub(crate) struct SidecarCheckpoint {
	pub tip: Tip,
	output_mmr_size: u64,
	kernel_mmr_size: u64,
	output_root: Hash,
	range_proof_root: Hash,
	kernel_root: Hash,
	trees: [TreeState; 2],
	kernel_append_lens: [u64; 3],
}

impl Writeable for SidecarCheckpoint {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), ser::Error> {
		self.tip.write(writer)?;
		writer.write_u64(self.output_mmr_size)?;
		writer.write_u64(self.kernel_mmr_size)?;
		self.output_root.write(writer)?;
		self.range_proof_root.write(writer)?;
		self.kernel_root.write(writer)?;
		for tree in &self.trees {
			tree.write(writer)?;
		}
		for len in self.kernel_append_lens {
			writer.write_u64(len)?;
		}
		Ok(())
	}
}

impl Readable for SidecarCheckpoint {
	fn read<R: Reader>(reader: &mut R) -> Result<Self, ser::Error> {
		Ok(Self {
			tip: Tip::read(reader)?,
			output_mmr_size: reader.read_u64()?,
			kernel_mmr_size: reader.read_u64()?,
			output_root: Hash::read(reader)?,
			range_proof_root: Hash::read(reader)?,
			kernel_root: Hash::read(reader)?,
			trees: [TreeState::read(reader)?, TreeState::read(reader)?],
			kernel_append_lens: [reader.read_u64()?, reader.read_u64()?, reader.read_u64()?],
		})
	}
}

#[derive(Clone, Debug)]
pub(crate) struct SidecarManifest {
	pub selected: SidecarCheckpoint,
	pub previous: Option<SidecarCheckpoint>,
}

impl Writeable for SidecarManifest {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), ser::Error> {
		self.selected.write(writer)?;
		match &self.previous {
			Some(previous) => {
				writer.write_u8(1)?;
				previous.write(writer)
			}
			None => writer.write_u8(0),
		}
	}
}

impl Readable for SidecarManifest {
	fn read<R: Reader>(reader: &mut R) -> Result<Self, ser::Error> {
		let selected = SidecarCheckpoint::read(reader)?;
		let previous = match reader.read_u8()? {
			0 => None,
			1 => Some(SidecarCheckpoint::read(reader)?),
			_ => return Err(ser::Error::CorruptedData),
		};
		Ok(Self { selected, previous })
	}
}

pub(crate) fn checkpoint(
	root: &str,
	header: &BlockHeader,
	previous: Option<&SidecarManifest>,
) -> io::Result<SidecarManifest> {
	let generation = header.hash().to_hex();
	let mut trees = Vec::with_capacity(TREE_DIRS.len());
	for tree in TREE_DIRS {
		let dir = tree_dir(root, tree);
		let mut append_lens = [0; 3];
		for (index, file) in APPEND_FILES.iter().enumerate() {
			append_lens[index] = file_len(&dir.join(file))?;
		}
		let mut sidecars = Vec::with_capacity(SIDECAR_FILES.len());
		for file in SIDECAR_FILES {
			let source = dir.join(file);
			let data = fs::read(&source)?;
			let state = FileState {
				len: data.len() as u64,
				checksum: data.hash(),
			};
			write_atomic(&generation_path(&source, &generation), &data)?;
			sidecars.push(state);
		}
		trees.push(TreeState {
			append_lens,
			sidecars: sidecars.try_into().expect("two sidecar files"),
		});
	}
	let kernel_dir = tree_dir(root, KERNEL_DIR);
	let mut kernel_append_lens = [0; 3];
	for (index, file) in APPEND_FILES.iter().enumerate() {
		kernel_append_lens[index] = file_len(&kernel_dir.join(file))?;
	}

	Ok(SidecarManifest {
		selected: SidecarCheckpoint {
			tip: Tip::from_header(header),
			output_mmr_size: header.output_mmr_size,
			kernel_mmr_size: header.kernel_mmr_size,
			output_root: header.output_root,
			range_proof_root: header.range_proof_root,
			kernel_root: header.kernel_root,
			trees: trees.try_into().expect("two PMMR trees"),
			kernel_append_lens,
		},
		previous: previous.map(|manifest| manifest.selected.clone()),
	})
}

/// Restore the selected generation before any PMMR backend opens its fixed files.
pub(crate) fn recover(root: &str, store: &ChainStore) -> Result<bool, String> {
	if let Some(target) = store.pibd_in_progress().map_err(|e| e.to_string())? {
		let progress = if store.has_pibd_head().map_err(|e| e.to_string())? {
			Some(store.pibd_head().map_err(|e| e.to_string())?)
		} else {
			None
		};
		let target_exists = store.get_block_header(&target.last_block_h).is_ok();
		let body_head = store.head().map_err(|e| e.to_string())?;
		let marker_matches = progress.is_some_and(|progress| {
			body_head.last_block_h != target.last_block_h
				&& (progress.height < target.height
					|| (progress.height == target.height
						&& progress.last_block_h == target.last_block_h))
		});
		if target_exists && marker_matches {
			return Ok(false);
		}
		warn!("ignoring stale PIBD marker for {}", target.last_block_h);
		let mut batch = store.batch().map_err(|e| e.to_string())?;
		batch.clear_pibd_in_progress().map_err(|e| e.to_string())?;
		batch.commit().map_err(|e| e.to_string())?;
	}
	let Some(manifest) = store.sidecar_manifest().map_err(|e| e.to_string())? else {
		return Ok(false);
	};
	if store.head().map_err(|e| e.to_string())? != manifest.selected.tip {
		// A legacy binary advanced the fixed paths. Let normal setup validate that
		// state, then import it as a fresh generation.
		return Ok(true);
	}

	let selected_error = match restore(root, &manifest.selected) {
		Ok(()) => return Ok(false),
		Err(error) => error,
	};
	let previous = manifest.previous.ok_or_else(|| {
		format!(
			"archive sidecar checkpoint is damaged and no retained generation is available: {}",
			selected_error
		)
	})?;
	restore(root, &previous)
		.map_err(|error| format!("archive sidecar checkpoints are not recoverable: {}", error))?;
	let mut batch = store.batch().map_err(|e| e.to_string())?;
	batch
		.save_body_head(&previous.tip)
		.map_err(|e| e.to_string())?;
	batch
		.save_sidecar_manifest(&SidecarManifest {
			selected: previous,
			previous: None,
		})
		.map_err(|e| e.to_string())?;
	batch.commit().map_err(|e| e.to_string())?;
	Ok(false)
}

pub(crate) fn cleanup(root: &str, manifest: &SidecarManifest) {
	let mut retained = vec![manifest.selected.tip.last_block_h.to_hex()];
	if let Some(previous) = &manifest.previous {
		retained.push(previous.tip.last_block_h.to_hex());
	}
	for tree in TREE_DIRS {
		let dir = tree_dir(root, tree);
		let Ok(entries) = fs::read_dir(dir) else {
			continue;
		};
		for entry in entries.flatten() {
			let name = entry.file_name().to_string_lossy().into_owned();
			if !SIDECAR_FILES
				.iter()
				.any(|file| name.starts_with(&format!("{}.generation-", file)))
			{
				continue;
			}
			if !retained.iter().any(|generation| name.ends_with(generation)) {
				let _ = fs::remove_file(entry.path());
			}
		}
	}
}

fn restore(root: &str, checkpoint: &SidecarCheckpoint) -> io::Result<()> {
	let generation = checkpoint.tip.last_block_h.to_hex();
	for (index, tree) in TREE_DIRS.iter().enumerate() {
		let element_size = if index == 0 {
			<OutputIdentifier as PMMRable>::elmt_size().expect("fixed output size") as u64
		} else {
			<RangeProof as PMMRable>::elmt_size().expect("fixed rangeproof size") as u64
		};
		validate_append_files(
			&tree_dir(root, tree),
			&checkpoint.trees[index],
			element_size,
		)?;
	}
	let kernel_dir = tree_dir(root, KERNEL_DIR);
	validate_variable_append_files(&kernel_dir, checkpoint.kernel_append_lens)?;
	for (index, tree) in TREE_DIRS.iter().enumerate() {
		let dir = tree_dir(root, tree);
		for (file_index, file) in SIDECAR_FILES.iter().enumerate() {
			let fixed = dir.join(file);
			let data = fs::read(generation_path(&fixed, &generation))?;
			let expected = &checkpoint.trees[index].sidecars[file_index];
			if data.len() as u64 != expected.len || data.hash() != expected.checksum {
				return Err(io::Error::new(
					io::ErrorKind::InvalidData,
					format!("invalid {} sidecar generation", tree),
				));
			}
			std::panic::catch_unwind(|| Bitmap::deserialize::<Portable>(&data)).map_err(|_| {
				io::Error::new(io::ErrorKind::InvalidData, "malformed PMMR sidecar bitmap")
			})?;
			write_atomic(&fixed, &data)?;
		}
		truncate_append_files(&dir, checkpoint.trees[index].append_lens)?;
	}
	truncate_append_files(&kernel_dir, checkpoint.kernel_append_lens)?;
	Ok(())
}

fn validate_append_files(dir: &Path, state: &TreeState, element_size: u64) -> io::Result<()> {
	for (index, file) in APPEND_FILES.iter().enumerate() {
		let actual = file_len(&dir.join(file))?;
		if actual < state.append_lens[index] {
			return Err(io::Error::new(
				io::ErrorKind::UnexpectedEof,
				format!(
					"{} is shorter than its checkpoint",
					dir.join(file).display()
				),
			));
		}
	}
	if state.append_lens[0] % Hash::LEN as u64 != 0 {
		return Err(io::Error::new(
			io::ErrorKind::InvalidData,
			"misaligned PMMR hash file",
		));
	}
	if state.append_lens[1] % element_size != 0 || state.append_lens[2] != 0 {
		return Err(io::Error::new(
			io::ErrorKind::InvalidData,
			"misaligned PMMR data file",
		));
	}
	Ok(())
}

fn validate_variable_append_files(dir: &Path, expected: [u64; 3]) -> io::Result<()> {
	for (index, file) in APPEND_FILES.iter().enumerate() {
		if file_len(&dir.join(file))? < expected[index] {
			return Err(io::Error::new(
				io::ErrorKind::UnexpectedEof,
				format!(
					"{} is shorter than its checkpoint",
					dir.join(file).display()
				),
			));
		}
	}
	if expected[0] % Hash::LEN as u64 != 0 || expected[2] % 10 != 0 {
		return Err(io::Error::new(
			io::ErrorKind::InvalidData,
			"misaligned variable-size PMMR files",
		));
	}
	if expected == [0, 0, 0] {
		return Ok(());
	}
	let size_data = fs::read(dir.join(APPEND_FILES[2]))?;
	let mut end = 0;
	for entry in size_data[..expected[2] as usize].chunks_exact(10) {
		let offset = BigEndian::read_u64(&entry[..8]);
		let size = BigEndian::read_u16(&entry[8..]) as u64;
		if offset != end || offset + size > expected[1] {
			return Err(io::Error::new(
				io::ErrorKind::InvalidData,
				"invalid variable-size PMMR framing",
			));
		}
		end = offset + size;
	}
	if end != expected[1] {
		return Err(io::Error::new(
			io::ErrorKind::InvalidData,
			"variable-size PMMR data boundary does not match its size index",
		));
	}
	Ok(())
}

fn truncate_append_files(dir: &Path, expected: [u64; 3]) -> io::Result<()> {
	for (index, file) in APPEND_FILES.iter().enumerate() {
		let path = dir.join(file);
		let actual = file_len(&path)?;
		if actual < expected[index] {
			return Err(io::Error::new(
				io::ErrorKind::UnexpectedEof,
				format!("{} is shorter than its checkpoint", path.display()),
			));
		}
		if actual > expected[index] {
			OpenOptions::new()
				.write(true)
				.open(&path)?
				.set_len(expected[index])?;
		}
	}
	if dir.exists() {
		sync_dir(dir)?;
	}
	Ok(())
}

fn tree_dir(root: &str, tree: &str) -> PathBuf {
	Path::new(root).join("txhashset").join(tree)
}

fn generation_path(path: &Path, generation: &str) -> PathBuf {
	path.with_file_name(format!(
		"{}.generation-{}",
		path.file_name()
			.expect("sidecar file name")
			.to_string_lossy(),
		generation
	))
}

fn file_len(path: &Path) -> io::Result<u64> {
	match fs::metadata(path) {
		Ok(metadata) => Ok(metadata.len()),
		Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
		Err(error) => Err(error),
	}
}

fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
	let temporary = path.with_extension("archive-sync.tmp");
	fs::write(&temporary, data)?;
	File::open(&temporary)?.sync_all()?;
	fs::rename(&temporary, path)?;
	sync_dir(path.parent().expect("sidecar parent"))
}

fn sync_dir(path: &Path) -> io::Result<()> {
	File::open(path)?.sync_all()
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::time::{SystemTime, UNIX_EPOCH};

	#[test]
	fn restores_sidecars_and_truncates_unpublished_suffixes() {
		crate::core::global::set_local_chain_type(
			crate::core::global::ChainTypes::AutomatedTesting,
		);
		let root = std::env::temp_dir().join(format!(
			"grin-sidecar-{}-{}",
			std::process::id(),
			SystemTime::now()
				.duration_since(UNIX_EPOCH)
				.unwrap()
				.as_nanos()
		));
		let mut leaf_bitmap = Bitmap::new();
		leaf_bitmap.add(1);
		let leaf_data = leaf_bitmap.serialize::<Portable>();
		let prune_data = Bitmap::new().serialize::<Portable>();
		for (index, tree) in TREE_DIRS.iter().enumerate() {
			let dir = tree_dir(root.to_str().unwrap(), tree);
			fs::create_dir_all(&dir).unwrap();
			fs::write(dir.join(APPEND_FILES[0]), vec![0; 32]).unwrap();
			if index == 0 {
				fs::write(dir.join(APPEND_FILES[1]), vec![0; 34]).unwrap();
			} else {
				let size = <RangeProof as PMMRable>::elmt_size().unwrap() as usize;
				fs::write(dir.join(APPEND_FILES[1]), vec![0; size]).unwrap();
			}
			fs::write(dir.join(SIDECAR_FILES[0]), &leaf_data).unwrap();
			fs::write(dir.join(SIDECAR_FILES[1]), &prune_data).unwrap();
		}
		let kernel_dir = tree_dir(root.to_str().unwrap(), KERNEL_DIR);
		fs::create_dir_all(&kernel_dir).unwrap();
		fs::write(kernel_dir.join(APPEND_FILES[0]), vec![0; 32]).unwrap();
		fs::write(kernel_dir.join(APPEND_FILES[1]), vec![0; 5]).unwrap();
		let mut size_entry = vec![0; 10];
		BigEndian::write_u16(&mut size_entry[8..], 5);
		fs::write(kernel_dir.join(APPEND_FILES[2]), size_entry).unwrap();

		let header = BlockHeader::default();
		let manifest = checkpoint(root.to_str().unwrap(), &header, None).unwrap();
		for tree in TREE_DIRS {
			let dir = tree_dir(root.to_str().unwrap(), tree);
			fs::write(dir.join(SIDECAR_FILES[0]), b"damaged").unwrap();
			fs::write(dir.join(APPEND_FILES[0]), vec![0; 64]).unwrap();
		}
		fs::write(kernel_dir.join(APPEND_FILES[1]), vec![0; 10]).unwrap();

		restore(root.to_str().unwrap(), &manifest.selected).unwrap();
		for tree in TREE_DIRS {
			let dir = tree_dir(root.to_str().unwrap(), tree);
			assert_eq!(fs::read(dir.join(SIDECAR_FILES[0])).unwrap(), leaf_data);
			assert_eq!(fs::read(dir.join(APPEND_FILES[0])).unwrap(), vec![0; 32]);
		}
		assert_eq!(
			fs::read(kernel_dir.join(APPEND_FILES[1])).unwrap(),
			vec![0; 5]
		);
		fs::remove_dir_all(root).unwrap();
	}
}
