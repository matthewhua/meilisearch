use grenad::{CompressionType, Reader, Writer};
use heed::types::{ByteSlice, DecodeIgnore};
use heed::{BytesDecode, BytesEncode, Error};
use log::debug;
use roaring::RoaringBitmap;
use std::cmp;
use std::fs::File;
use std::num::{NonZeroU8, NonZeroUsize};
use std::ops::RangeFrom;
use time::OffsetDateTime;

use crate::error::InternalError;
use crate::heed_codec::facet::{
    FacetLevelValueF64Codec, FacetLevelValueU32Codec, FacetStringLevelZeroCodec,
    FacetStringLevelZeroValueCodec, FacetStringZeroBoundsValueCodec,
};
use crate::heed_codec::CboRoaringBitmapCodec;
use crate::update::index_documents::{create_writer, write_into_lmdb_database, writer_into_reader};
use crate::{FieldId, Index, Result};

pub struct Facets<'t, 'u, 'i> {
    wtxn: &'t mut heed::RwTxn<'i, 'u>,
    index: &'i Index,
    pub(crate) chunk_compression_type: CompressionType,
    pub(crate) chunk_compression_level: Option<u32>,
    level_group_size: NonZeroUsize,
    min_level_size: NonZeroUsize,
}

impl<'t, 'u, 'i> Facets<'t, 'u, 'i> {
    pub fn new(wtxn: &'t mut heed::RwTxn<'i, 'u>, index: &'i Index) -> Facets<'t, 'u, 'i> {
        Facets {
            wtxn,
            index,
            chunk_compression_type: CompressionType::None,
            chunk_compression_level: None,
            level_group_size: NonZeroUsize::new(4).unwrap(),
            min_level_size: NonZeroUsize::new(5).unwrap(),
        }
    }

    pub fn level_group_size(&mut self, value: NonZeroUsize) -> &mut Self {
        self.level_group_size = NonZeroUsize::new(cmp::max(value.get(), 2)).unwrap();
        self
    }

    pub fn min_level_size(&mut self, value: NonZeroUsize) -> &mut Self {
        self.min_level_size = value;
        self
    }

    #[logging_timer::time("Facets::{}")]
    pub fn execute(self) -> Result<()> {
        self.index.set_updated_at(self.wtxn, &OffsetDateTime::now_utc())?;
        // We get the faceted fields to be able to create the facet levels.
        let faceted_fields = self.index.faceted_fields_ids(self.wtxn)?;

        debug!("Computing and writing the facet values levels docids into LMDB on disk...");

        for field_id in faceted_fields {
            // Clear the facet string levels.
            clear_field_string_levels(
                self.wtxn,
                self.index.facet_id_string_docids.remap_types::<ByteSlice, DecodeIgnore>(),
                field_id,
            )?;

            let (facet_string_levels, string_documents_ids) = compute_facet_strings_levels(
                self.wtxn,
                self.index.facet_id_string_docids,
                self.chunk_compression_type,
                self.chunk_compression_level,
                self.level_group_size,
                self.min_level_size,
                field_id,
            )?;

            self.index.put_string_faceted_documents_ids(
                self.wtxn,
                field_id,
                &string_documents_ids,
            )?;
            for facet_strings_levels in facet_string_levels {
                write_into_lmdb_database(
                    self.wtxn,
                    *self.index.facet_id_string_docids.as_polymorph(),
                    facet_strings_levels,
                    |_, _| {
                        Err(InternalError::IndexingMergingKeys { process: "facet string levels" })?
                    },
                )?;
            }

            // Clear the facet number levels.
            clear_field_number_levels(self.wtxn, self.index.facet_id_f64_docids, field_id)?;

            let (facet_number_levels_2, number_documents_ids) = compute_facet_number_levels(
                self.wtxn,
                self.index.facet_id_f64_docids,
                self.chunk_compression_type,
                self.chunk_compression_level,
                self.level_group_size,
                self.min_level_size,
                field_id,
            )?;

            self.index.put_number_faceted_documents_ids(
                self.wtxn,
                field_id,
                &number_documents_ids,
            )?;

            for facet_number_levels in facet_number_levels_2 {
                write_into_lmdb_database(
                    self.wtxn,
                    *self.index.facet_id_f64_docids.as_polymorph(),
                    facet_number_levels,
                    |_, _| {
                        Err(InternalError::IndexingMergingKeys { process: "facet number levels" })?
                    },
                )?;
            }
        }

        Ok(())
    }
}

fn compute_facet_number_levels<'t>(
    rtxn: &'t heed::RoTxn,
    db: heed::Database<FacetLevelValueF64Codec, CboRoaringBitmapCodec>,
    compression_type: CompressionType,
    compression_level: Option<u32>,
    level_group_size: NonZeroUsize,
    min_level_size: NonZeroUsize,
    field_id: FieldId,
) -> Result<(Vec<Reader<File>>, RoaringBitmap)> {
    let first_level_size = db
        .remap_key_type::<ByteSlice>()
        .prefix_iter(rtxn, &field_id.to_be_bytes())?
        .remap_types::<DecodeIgnore, DecodeIgnore>()
        .fold(Ok(0usize), |count, result| result.and(count).map(|c| c + 1))?;

    let level_0_start = (field_id, 0, f64::MIN, f64::MIN);

    // Groups sizes are always a power of the original level_group_size and therefore a group
    // always maps groups of the previous level and never splits previous levels groups in half.
    let group_size_iter = (1u8..)
        .map(|l| (l, level_group_size.get().pow(l as u32)))
        .take_while(|(_, s)| first_level_size / *s >= min_level_size.get())
        .collect::<Vec<_>>();

    let mut number_document_ids = RoaringBitmap::new();

    if let Some((top_level, _)) = group_size_iter.last() {
        let subwriters =
            recursive_compute_levels::<FacetLevelValueF64Codec, CboRoaringBitmapCodec, f64>(
                rtxn,
                db,
                compression_type,
                compression_level,
                *top_level,
                level_0_start,
                &(level_0_start..),
                first_level_size,
                level_group_size,
                &mut |bitmaps, _, _| {
                    for bitmap in bitmaps {
                        number_document_ids |= bitmap;
                    }
                    Ok(())
                },
                &|_i, (_field_id, _level, left, _right)| *left,
                &|bitmap| bitmap,
                &|writer, level, left, right, docids| {
                    write_number_entry(writer, field_id, level.get(), left, right, &docids)?;
                    Ok(())
                },
            )?;

        Ok((subwriters, number_document_ids))
    } else {
        let mut documents_ids = RoaringBitmap::new();
        for result in db.range(rtxn, &(level_0_start..))?.take(first_level_size) {
            let (_key, docids) = result?;
            documents_ids |= docids;
        }

        Ok((vec![], documents_ids))
    }
}

fn compute_facet_strings_levels<'t>(
    rtxn: &'t heed::RoTxn,
    db: heed::Database<FacetStringLevelZeroCodec, FacetStringLevelZeroValueCodec>,
    compression_type: CompressionType,
    compression_level: Option<u32>,
    level_group_size: NonZeroUsize,
    min_level_size: NonZeroUsize,
    field_id: FieldId,
) -> Result<(Vec<Reader<File>>, RoaringBitmap)> {
    let first_level_size = db
        .remap_key_type::<ByteSlice>()
        .prefix_iter(rtxn, &field_id.to_be_bytes())?
        .remap_types::<DecodeIgnore, DecodeIgnore>()
        .fold(Ok(0usize), |count, result| result.and(count).map(|c| c + 1))?;

    let level_0_start = (field_id, "");

    // Groups sizes are always a power of the original level_group_size and therefore a group
    // always maps groups of the previous level and never splits previous levels groups in half.
    let group_size_iter = (1u8..)
        .map(|l| (l, level_group_size.get().pow(l as u32)))
        .take_while(|(_, s)| first_level_size / *s >= min_level_size.get())
        .collect::<Vec<_>>();

    let mut strings_document_ids = RoaringBitmap::new();

    if let Some((top_level, _)) = group_size_iter.last() {
        let subwriters = recursive_compute_levels::<
            FacetStringLevelZeroCodec,
            FacetStringLevelZeroValueCodec,
            (u32, &str),
        >(
            rtxn,
            db,
            compression_type,
            compression_level,
            *top_level,
            level_0_start,
            &(level_0_start..),
            first_level_size,
            level_group_size,
            &mut |bitmaps, _, _| {
                for bitmap in bitmaps {
                    strings_document_ids |= bitmap;
                }
                Ok(())
            },
            &|i, (_field_id, value)| (i as u32, *value),
            &|value| value.1,
            &|writer, level, start_bound, end_bound, docids| {
                write_string_entry(writer, field_id, level, start_bound, end_bound, docids)?;
                Ok(())
            },
        )?;

        Ok((subwriters, strings_document_ids))
    } else {
        let mut documents_ids = RoaringBitmap::new();
        for result in db.range(rtxn, &(level_0_start..))?.take(first_level_size) {
            let (_key, (_original_value, docids)) = result?;
            documents_ids |= docids;
        }

        Ok((vec![], documents_ids))
    }
}

fn recursive_compute_levels<'t, KeyCodec, ValueCodec, Bound>(
    rtxn: &'t heed::RoTxn,
    db: heed::Database<KeyCodec, ValueCodec>,
    compression_type: CompressionType,
    compression_level: Option<u32>,
    level: u8,
    level_0_start: <KeyCodec as BytesDecode<'t>>::DItem,
    level_0_range: &'t RangeFrom<<KeyCodec as BytesDecode<'t>>::DItem>,
    level_0_size: usize,
    level_group_size: NonZeroUsize,
    computed_group_bitmap: &mut dyn FnMut(&[RoaringBitmap], Bound, Bound) -> Result<()>,
    bound_from_db_key: &dyn for<'a> Fn(usize, &'a <KeyCodec as BytesDecode<'t>>::DItem) -> Bound,
    bitmap_from_db_value: &dyn Fn(<ValueCodec as BytesDecode<'t>>::DItem) -> RoaringBitmap,
    write_entry: &dyn Fn(&mut Writer<File>, NonZeroU8, Bound, Bound, RoaringBitmap) -> Result<()>,
) -> Result<Vec<Reader<File>>>
where
    KeyCodec: for<'a> BytesEncode<'a>
        + for<'a> BytesDecode<'a, DItem = <KeyCodec as BytesEncode<'a>>::EItem>,
    for<'a> <KeyCodec as BytesEncode<'a>>::EItem: Sized,
    ValueCodec: for<'a> BytesEncode<'a>
        + for<'a> BytesDecode<'a, DItem = <ValueCodec as BytesEncode<'a>>::EItem>,
    for<'a> <ValueCodec as BytesEncode<'a>>::EItem: Sized,
    Bound: Copy,
{
    if level == 0 {
        // base case for the recursion

        let mut bitmaps = vec![];

        let mut start_bound = bound_from_db_key(0, &level_0_start);
        let mut end_bound = bound_from_db_key(0, &level_0_start);
        let mut first_iteration_for_new_group = true;
        for (i, db_result_item) in db.range(rtxn, level_0_range)?.take(level_0_size).enumerate() {
            let (key, value) = db_result_item?;

            let bound = bound_from_db_key(i, &key);
            let docids = bitmap_from_db_value(value);

            if first_iteration_for_new_group {
                start_bound = bound;
                first_iteration_for_new_group = false;
            }
            end_bound = bound;
            bitmaps.push(docids);

            if bitmaps.len() == level_group_size.get() {
                computed_group_bitmap(&bitmaps, start_bound, end_bound)?;
                first_iteration_for_new_group = true;
                bitmaps.clear();
            }
        }
        if !bitmaps.is_empty() {
            computed_group_bitmap(&bitmaps, start_bound, end_bound)?;
            bitmaps.clear();
        }
        // level 0 is already stored in the DB
        return Ok(vec![]);
    } else {
        let mut cur_writer =
            create_writer(compression_type, compression_level, tempfile::tempfile()?);

        let mut range_for_bitmaps = vec![];
        let mut bitmaps = vec![];

        let mut sub_writers = recursive_compute_levels(
            rtxn,
            db,
            compression_type,
            compression_level,
            level - 1,
            level_0_start,
            level_0_range,
            level_0_size,
            level_group_size,
            &mut |sub_bitmaps: &[RoaringBitmap], start_range, end_range| {
                let mut combined_bitmap = RoaringBitmap::default();
                for bitmap in sub_bitmaps {
                    combined_bitmap |= bitmap;
                }
                range_for_bitmaps.push((start_range, end_range));

                bitmaps.push(combined_bitmap);
                if bitmaps.len() == level_group_size.get() {
                    let start_bound = range_for_bitmaps.first().unwrap().0;
                    let end_bound = range_for_bitmaps.last().unwrap().1;
                    computed_group_bitmap(&bitmaps, start_bound, end_bound)?;
                    for (bitmap, (start_bound, end_bound)) in
                        bitmaps.drain(..).zip(range_for_bitmaps.drain(..))
                    {
                        write_entry(
                            &mut cur_writer,
                            NonZeroU8::new(level).unwrap(),
                            start_bound,
                            end_bound,
                            bitmap,
                        )?;
                    }
                }
                Ok(())
            },
            bound_from_db_key,
            bitmap_from_db_value,
            write_entry,
        )?;
        if !bitmaps.is_empty() {
            let start_range = range_for_bitmaps.first().unwrap().0;
            let end_range = range_for_bitmaps.last().unwrap().1;
            computed_group_bitmap(&bitmaps, start_range, end_range)?;
            for (bitmap, (left, right)) in bitmaps.drain(..).zip(range_for_bitmaps.drain(..)) {
                write_entry(&mut cur_writer, NonZeroU8::new(level).unwrap(), left, right, bitmap)?;
            }
        }

        sub_writers.push(writer_into_reader(cur_writer)?);
        return Ok(sub_writers);
    }
}

fn clear_field_number_levels<'t>(
    wtxn: &'t mut heed::RwTxn,
    db: heed::Database<FacetLevelValueF64Codec, CboRoaringBitmapCodec>,
    field_id: FieldId,
) -> heed::Result<()> {
    let left = (field_id, 1, f64::MIN, f64::MIN);
    let right = (field_id, u8::MAX, f64::MAX, f64::MAX);
    let range = left..=right;
    db.delete_range(wtxn, &range).map(drop)
}

fn clear_field_string_levels<'t>(
    wtxn: &'t mut heed::RwTxn,
    db: heed::Database<ByteSlice, DecodeIgnore>,
    field_id: FieldId,
) -> heed::Result<()> {
    let left = (field_id, NonZeroU8::new(1).unwrap(), u32::MIN, u32::MIN);
    let right = (field_id, NonZeroU8::new(u8::MAX).unwrap(), u32::MAX, u32::MAX);
    let range = left..=right;
    db.remap_key_type::<FacetLevelValueU32Codec>().delete_range(wtxn, &range).map(drop)
}

fn write_number_entry(
    writer: &mut Writer<File>,
    field_id: FieldId,
    level: u8,
    left: f64,
    right: f64,
    ids: &RoaringBitmap,
) -> Result<()> {
    let key = (field_id, level, left, right);
    let key = FacetLevelValueF64Codec::bytes_encode(&key).ok_or(Error::Encoding)?;
    let data = CboRoaringBitmapCodec::bytes_encode(&ids).ok_or(Error::Encoding)?;
    writer.insert(&key, &data)?;
    Ok(())
}
fn write_string_entry(
    writer: &mut Writer<File>,
    field_id: FieldId,
    level: NonZeroU8,
    (left_id, left_value): (u32, &str),
    (right_id, right_value): (u32, &str),
    docids: RoaringBitmap,
) -> Result<()> {
    let key = (field_id, level, left_id, right_id);
    let key = FacetLevelValueU32Codec::bytes_encode(&key).ok_or(Error::Encoding)?;
    let data = match level.get() {
        1 => (Some((left_value, right_value)), docids),
        _ => (None, docids),
    };
    let data = FacetStringZeroBoundsValueCodec::<CboRoaringBitmapCodec>::bytes_encode(&data)
        .ok_or(Error::Encoding)?;
    writer.insert(&key, &data)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use crate::db_snap;
    use crate::documents::documents_batch_reader_from_objects;
    use crate::index::tests::TempIndex;

    #[test]
    fn test_facets_number() {
        let test =
            |name: &str, group_size: Option<NonZeroUsize>, min_level_size: Option<NonZeroUsize>| {
                let mut index = TempIndex::new_with_map_size(4096 * 1000 * 10); // 40MB
                index.index_documents_config.autogenerate_docids = true;
                index.index_documents_config.facet_level_group_size = group_size;
                index.index_documents_config.facet_min_level_size = min_level_size;

                index
                    .update_settings(|settings| {
                        settings.set_filterable_fields(
                            IntoIterator::into_iter(["facet".to_owned(), "facet2".to_owned()])
                                .collect(),
                        );
                    })
                    .unwrap();

                let mut documents = vec![];
                for i in 0..1_000 {
                    documents.push(serde_json::json!({ "facet": i }).as_object().unwrap().clone());
                }
                for i in 0..100 {
                    documents.push(serde_json::json!({ "facet2": i }).as_object().unwrap().clone());
                }
                let documents = documents_batch_reader_from_objects(documents);

                index.add_documents(documents).unwrap();

                db_snap!(index, facet_id_f64_docids, name);
            };

        test("default", None, None);
        test("tiny_groups_tiny_levels", NonZeroUsize::new(1), NonZeroUsize::new(1));
        test("small_groups_small_levels", NonZeroUsize::new(2), NonZeroUsize::new(2));
        test("small_groups_large_levels", NonZeroUsize::new(2), NonZeroUsize::new(128));
        test("large_groups_small_levels", NonZeroUsize::new(16), NonZeroUsize::new(2));
        test("large_groups_large_levels", NonZeroUsize::new(16), NonZeroUsize::new(256));
    }

    #[test]
    fn test_facets_string() {
        let test = |name: &str,
                    group_size: Option<NonZeroUsize>,
                    min_level_size: Option<NonZeroUsize>| {
            let mut index = TempIndex::new_with_map_size(4096 * 1000 * 10); // 40MB
            index.index_documents_config.autogenerate_docids = true;
            index.index_documents_config.facet_level_group_size = group_size;
            index.index_documents_config.facet_min_level_size = min_level_size;

            index
                .update_settings(|settings| {
                    settings.set_filterable_fields(
                        IntoIterator::into_iter(["facet".to_owned(), "facet2".to_owned()])
                            .collect(),
                    );
                })
                .unwrap();

            let mut documents = vec![];
            for i in 0..100 {
                documents.push(
                    serde_json::json!({ "facet": format!("s{i:X}") }).as_object().unwrap().clone(),
                );
            }
            for i in 0..10 {
                documents.push(
                    serde_json::json!({ "facet2": format!("s{i:X}") }).as_object().unwrap().clone(),
                );
            }
            let documents = documents_batch_reader_from_objects(documents);

            index.add_documents(documents).unwrap();

            db_snap!(index, facet_id_string_docids, name);
        };

        test("default", None, None);
        test("tiny_groups_tiny_levels", NonZeroUsize::new(1), NonZeroUsize::new(1));
    }
}
