// Copyright 2020 TiKV Project Authors. Licensed under Apache-2.0.

//! This trait contains miscellaneous features that have
//! not been carefully factored into other traits.
//!
//! FIXME: Things here need to be moved elsewhere.

use crate::cf_names::CFNamesExt;
use crate::errors::Result;
use crate::import::{ImportExt, IngestExternalFileOptions};
use crate::iterable::{Iterable, Iterator};
use crate::mutable::Mutable;
use crate::options::IterOptions;
use crate::range::Range;
use crate::sst::{SstExt, SstWriter, SstWriterBuilder};
use crate::write_batch::{WriteBatch, WriteBatchExt};

use tikv_util::keybuilder::KeyBuilder;

pub const MAX_DELETE_BATCH_SIZE: usize = 256;

const MAX_DELETE_COUNT_BY_KEY: usize = 2048;

#[derive(Clone)]
pub enum DeleteStrategy {
    DeleteByKey,
    DeleteByRange,
    DeleteByWriter { sst_path: String },
}

pub trait MiscExt: Iterable + WriteBatchExt + CFNamesExt + SstExt + ImportExt {
    fn is_titan(&self) -> bool {
        false
    }

    fn flush(&self, sync: bool) -> Result<()>;

    fn flush_cf(&self, cf: &str, sync: bool) -> Result<()>;

    fn delete_files_in_range_cf(
        &self,
        cf: &str,
        start_key: &[u8],
        end_key: &[u8],
        include_end: bool,
    ) -> Result<()>;

    fn delete_all_in_range(
        &self,
        strategy: DeleteStrategy,
        start_key: &[u8],
        end_key: &[u8],
    ) -> Result<()> {
        if start_key >= end_key {
            return Ok(());
        }

        for cf in self.cf_names() {
            self.delete_all_in_range_cf(cf, strategy.clone(), start_key, end_key)?;
        }

        Ok(())
    }

    fn delete_all_in_range_cf_by_ingest(
        &self,
        cf: &str,
        start_key: &[u8],
        end_key: &[u8],
        writer: &mut Self::SstWriter,
    ) -> Result<()> {
        let start = KeyBuilder::from_slice(start_key, 0, 0);
        let end = KeyBuilder::from_slice(end_key, 0, 0);
        let mut opts = IterOptions::new(Some(start), Some(end), false);
        if self.is_titan() {
            // Cause DeleteFilesInRange may expose old blob index keys, setting key only for Titan
            // to avoid referring to missing blob files.
            opts.set_key_only(true);
        }
        let mut it = self.iterator_cf_opt(cf, opts)?;
        let mut it_valid = it.seek(start_key.into())?;

        while it_valid {
            writer.delete(it.key())?;
            it_valid = it.next()?;
        }
        Ok(())
    }

    fn delete_all_in_range_cf(
        &self,
        cf: &str,
        strategy: DeleteStrategy,
        start_key: &[u8],
        end_key: &[u8],
    ) -> Result<()> {
        let mut wb = self.write_batch();
        let start = KeyBuilder::from_slice(start_key, 0, 0);
        let end = KeyBuilder::from_slice(end_key, 0, 0);
        let mut opts = IterOptions::new(Some(start), Some(end), false);
        if self.is_titan() {
            // Cause DeleteFilesInRange may expose old blob index keys, setting key only for Titan
            // to avoid referring to missing blob files.
            opts.set_key_only(true);
        }

        match strategy {
            DeleteStrategy::DeleteByRange => {
                wb.delete_range_cf(cf, start_key, end_key)?;
                return self.write(&wb);
            }
            DeleteStrategy::DeleteByKey => {
                let mut it = self.iterator_cf_opt(cf, opts)?;
                let mut it_valid = it.seek(start_key.into())?;
                while it_valid {
                    wb.delete_cf(cf, it.key())?;
                    if wb.count() >= MAX_DELETE_BATCH_SIZE {
                        // Can't use write_without_wal here.
                        // Otherwise it may cause dirty data when applying snapshot.
                        self.write(&wb)?;
                        wb.clear();
                    }
                    it_valid = it.next()?;
                }
                if wb.count() > 0 {
                    self.write(&wb)?;
                }
            }
            DeleteStrategy::DeleteByWriter { sst_path } => {
                let mut data: Vec<Vec<u8>> = vec![];
                let mut it = self.iterator_cf_opt(cf, opts)?;
                let mut it_valid = it.seek(start_key.into())?;
                while it_valid {
                    if data.len() > MAX_DELETE_COUNT_BY_KEY {
                        let builder = Self::SstWriterBuilder::new().set_db(self).set_cf(cf);
                        let mut writer = builder.build(sst_path.as_str())?;
                        for key in data.iter() {
                            writer.delete(key).unwrap();
                        }
                        let start_key = it.key().to_vec();
                        drop(it);
                        self.delete_all_in_range_cf_by_ingest(
                            cf,
                            &start_key,
                            end_key,
                            &mut writer,
                        )?;
                        writer.finish()?;
                        let handle = self.cf_handle(cf)?;
                        let mut opt = Self::IngestExternalFileOptions::new();
                        opt.move_files(true);
                        return self.ingest_external_file_cf(handle, &opt, &[sst_path.as_str()]);
                    }
                    data.push(it.key().to_vec());
                    it_valid = it.next()?;
                }
                let mut wb = self.write_batch();
                for key in data.iter() {
                    wb.delete_cf(cf, key)?;
                    if wb.count() >= MAX_DELETE_BATCH_SIZE {
                        self.write(&wb)?;
                        wb.clear();
                    }
                }
                if wb.count() > 0 {
                    self.write(&wb)?;
                }
            }
        }
        Ok(())
    }

    fn delete_all_files_in_range(&self, start_key: &[u8], end_key: &[u8]) -> Result<()> {
        if start_key >= end_key {
            return Ok(());
        }

        for cf in self.cf_names() {
            self.delete_files_in_range_cf(cf, start_key, end_key, false)?;
        }

        Ok(())
    }

    /// Return the approximate number of records and size in the range of memtables of the cf.
    fn get_approximate_memtable_stats_cf(&self, cf: &str, range: &Range) -> Result<(u64, u64)>;

    fn ingest_maybe_slowdown_writes(&self, cf: &str) -> Result<bool>;

    /// Gets total used size of rocksdb engine, including:
    /// *  total size (bytes) of all SST files.
    /// *  total size (bytes) of active and unflushed immutable memtables.
    /// *  total size (bytes) of all blob files.
    ///
    fn get_engine_used_size(&self) -> Result<u64>;

    /// Roughly deletes files in multiple ranges.
    ///
    /// Note:
    ///    - After this operation, some keys in the range might still exist in the database.
    ///    - After this operation, some keys in the range might be removed from existing snapshot,
    ///      so you shouldn't expect to be able to read data from the range using existing snapshots
    ///      any more.
    ///
    /// Ref: https://github.com/facebook/rocksdb/wiki/Delete-A-Range-Of-Keys
    fn roughly_cleanup_ranges(&self, ranges: &[(Vec<u8>, Vec<u8>)]) -> Result<()>;

    fn path(&self) -> &str;

    fn sync_wal(&self) -> Result<()>;

    /// Check whether a database exists at a given path
    fn exists(path: &str) -> bool;

    /// Dump stats about the database into a string.
    ///
    /// For debugging. The format and content is unspecified.
    fn dump_stats(&self) -> Result<String>;

    fn get_latest_sequence_number(&self) -> u64;

    fn get_oldest_snapshot_sequence_number(&self) -> Option<u64>;
}
