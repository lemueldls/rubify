use std::hash::Hasher;

use anyhow::{Context, Result};
use fontcull_read_fonts::{FontRef, TopLevelTable, tables::cff::Cff, types::Tag};
use fontcull_write_fonts::tables::{glyf::Glyf, loca::Loca};
use rustc_hash::{FxHashMap, FxHasher};

fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = FxHasher::default();
    hasher.write(bytes);
    hasher.finish()
}

fn append_table(block: &mut Vec<u8>, data: &[u8]) -> u32 {
    while !block.len().is_multiple_of(4) {
        block.push(0);
    }

    let off = block.len() as u32;
    block.extend_from_slice(data);

    off
}

pub fn build_collection(fonts: &[FontRef]) -> Result<Vec<u8>> {
    let mut out = Vec::new();

    // TTC header
    out.extend_from_slice(b"ttcf"); // Tag
    out.extend_from_slice(&1u16.to_be_bytes()); // Major
    out.extend_from_slice(&0u16.to_be_bytes()); // Minor
    out.extend_from_slice(&(fonts.len() as u32).to_be_bytes());

    let offset_table_start = out.len();

    for _ in 0..fonts.len() {
        out.extend_from_slice(&0u32.to_be_bytes());
    }

    let mut font_offsets = Vec::new();
    let mut table_cache: FxHashMap<(Tag, u64), (u32, Vec<u8>)> = FxHashMap::default();
    let mut table_data_block = Vec::new();

    // Process and rewrite each font
    for font in fonts {
        font_offsets.push(out.len() as u32);
        let records = font.table_directory().table_records();
        let num_tables = records.len() as u16;

        // Write OffsetTable header
        out.extend_from_slice(&0x00010000u32.to_be_bytes()); // sfntVersion
        out.extend_from_slice(&num_tables.to_be_bytes());
        let entry_selector = (num_tables as f32).log2().floor() as u16;
        let search_range = (2u16.pow(entry_selector as u32)) * 16;
        out.extend_from_slice(&search_range.to_be_bytes());
        out.extend_from_slice(&entry_selector.to_be_bytes());
        out.extend_from_slice(&(num_tables * 16 - search_range).to_be_bytes());

        for record in records {
            let tag = record.tag();
            let table_data = font
                .table_data(tag)
                .context("Table missing")?
                .as_ref()
                .to_vec();
            let table_len = table_data.len() as u32;

            // Only share tables that are usually safe and heavy
            let can_share = matches!(tag, Glyf::TAG | Cff::TAG | Loca::TAG);

            let rel_offset = if can_share {
                let hash = hash_bytes(&table_data);

                if let Some((off, existing)) = table_cache.get(&(tag, hash)) {
                    if existing == &table_data {
                        // Identical to a previously written table: reuse it
                        *off
                    } else {
                        append_table(&mut table_data_block, &table_data)
                    }
                } else {
                    let off = append_table(&mut table_data_block, &table_data);
                    table_cache.insert((tag, hash), (off, table_data));

                    off
                }
            } else {
                append_table(&mut table_data_block, &table_data)
            };

            out.extend_from_slice(&tag.to_be_bytes());
            out.extend_from_slice(&record.checksum().to_be_bytes());
            out.extend_from_slice(&rel_offset.to_be_bytes());
            out.extend_from_slice(&table_len.to_be_bytes());
        }
    }

    // Fix up absolute offsets

    let data_block_start = out.len() as u32;

    for (i, &off) in font_offsets.iter().enumerate() {
        let pos = offset_table_start + (i * 4);
        out[pos..pos + 4].copy_from_slice(&off.to_be_bytes());
    }

    for &f_off in &font_offsets {
        let num_tables =
            u16::from_be_bytes(out[f_off as usize + 4..f_off as usize + 6].try_into()?);

        for i in 0..num_tables {
            let off_pos = (f_off as usize + 12) + (i as usize * 16) + 8;
            let rel = u32::from_be_bytes(out[off_pos..off_pos + 4].try_into()?);
            out[off_pos..off_pos + 4].copy_from_slice(&(data_block_start + rel).to_be_bytes());
        }
    }

    out.extend(table_data_block);

    Ok(out)
}
