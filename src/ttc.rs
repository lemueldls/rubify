use anyhow::{Context, Result};
use fontcull_read_fonts::{FontRef, TopLevelTable, tables::cff::Cff, types::Tag};
use fontcull_write_fonts::tables::{glyf::Glyf, loca::Loca};
use rustc_hash::FxHashMap;

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
    let mut table_cache: FxHashMap<(Tag, Vec<u8>), u32> = FxHashMap::default();
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

            // Only share tables that are usually safe and heavy
            let can_share = matches!(tag, Glyf::TAG | Cff::TAG | Loca::TAG);

            let rel_offset = if can_share {
                if let Some(&off) = table_cache.get(&(tag, table_data.clone())) {
                    off
                } else {
                    while table_data_block.len() % 4 != 0 {
                        table_data_block.push(0);
                    }

                    let off = table_data_block.len() as u32;
                    table_cache.insert((tag, table_data.clone()), off);
                    table_data_block.extend(&table_data);

                    off
                }
            } else {
                while table_data_block.len() % 4 != 0 {
                    table_data_block.push(0);
                }

                let off = table_data_block.len() as u32;
                table_data_block.extend(&table_data);

                off
            };

            out.extend_from_slice(&tag.to_be_bytes());
            out.extend_from_slice(&record.checksum().to_be_bytes());
            out.extend_from_slice(&rel_offset.to_be_bytes());
            out.extend_from_slice(&(table_data.len() as u32).to_be_bytes());
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
