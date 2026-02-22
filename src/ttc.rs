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

#[cfg(test)]
mod tests {
    use fontcull_read_fonts::{FileRef, TableProvider};

    use super::*;

    #[test]
    fn test_ttc_roundtrip() -> Result<()> {
        // Include the TTC file
        let input_data =
            std::fs::read("Sarasa-Regular.ttc").context("Failed to read input TTC file")?;

        // Parse input TTC
        let input_file = FileRef::new(&input_data[..]).context("Failed to parse input TTC file")?;

        // Extract fonts from input TTC
        let input_fonts: Vec<FontRef> = input_file
            .fonts()
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to collect fonts from input TTC")?;

        eprintln!("Input TTC contains {} fonts", input_fonts.len());

        // Reconstruct using build_ttc
        let output_data = build_collection(&input_fonts).context("Failed to rebuild TTC")?;

        eprintln!(
            "Input size: {} bytes, Output size: {} bytes",
            input_data.len(),
            output_data.len()
        );

        // Parse output TTC
        let output_file =
            FileRef::new(&output_data[..]).context("Failed to parse output TTC file")?;

        // Extract fonts from output TTC
        let output_fonts: Vec<FontRef> = output_file
            .fonts()
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to collect fonts from output TTC")?;

        eprintln!("Output TTC contains {} fonts", output_fonts.len());

        // Basic checks
        assert_eq!(input_fonts.len(), output_fonts.len(), "Font count mismatch");

        // Compare each font in detail
        for (i, (in_font, out_font)) in input_fonts.iter().zip(output_fonts.iter()).enumerate() {
            eprintln!("\n=== Comparing Font {} ===", i);

            // Compare key metadata
            let in_head = in_font
                .head()
                .context(format!("Input font {} has no head table", i))?;
            let out_head = out_font
                .head()
                .context(format!("Output font {} has no head table", i))?;

            eprintln!(
                "Font {}: Input units_per_em={}, Output units_per_em={}",
                i,
                in_head.units_per_em(),
                out_head.units_per_em()
            );

            // Compare essential table presence
            let in_tables: Vec<Tag> = in_font
                .table_directory()
                .table_records()
                .into_iter()
                .map(|r| r.tag())
                .collect();
            let out_tables: Vec<Tag> = out_font
                .table_directory()
                .table_records()
                .into_iter()
                .map(|r| r.tag())
                .collect();

            eprintln!(
                "Font {}: Input tables: {:?}",
                i,
                in_tables
                    .iter()
                    .map(|t| format!("{}", t))
                    .collect::<Vec<_>>()
            );
            eprintln!(
                "Font {}: Output tables: {:?}",
                i,
                out_tables
                    .iter()
                    .map(|t| format!("{}", t))
                    .collect::<Vec<_>>()
            );

            // Check if all essential tables are present
            if in_tables != out_tables {
                eprintln!("Warning: Font {} has different table lists", i);
                let missing: Vec<_> = in_tables
                    .iter()
                    .filter(|t| !out_tables.contains(t))
                    .collect();
                let extra: Vec<_> = out_tables
                    .iter()
                    .filter(|t| !in_tables.contains(t))
                    .collect();

                if !missing.is_empty() {
                    eprintln!("  Missing in output: {:?}", missing);
                }
                if !extra.is_empty() {
                    eprintln!("  Extra in output: {:?}", extra);
                }
            }

            // Compare table sizes for common tables
            for &tag in &in_tables {
                if out_tables.contains(&tag) {
                    let in_size = in_font.table_data(tag).map(|d| d.len()).unwrap_or(0);
                    let out_size = out_font.table_data(tag).map(|d| d.len()).unwrap_or(0);

                    if in_size != out_size {
                        eprintln!(
                            "Font {}: Table {} size differs: {} -> {}",
                            i, tag, in_size, out_size
                        );
                    }
                }
            }
        }

        // Compute checksums for debugging
        let input_cksum: u32 = input_data
            .iter()
            .map(|&b| b as u32)
            .fold(0, |a, b| a.wrapping_add(b));
        let output_cksum: u32 = output_data
            .iter()
            .map(|&b| b as u32)
            .fold(0, |a, b| a.wrapping_add(b));

        eprintln!(
            "\nInput checksum: {:08x}, Output checksum: {:08x}",
            input_cksum, output_cksum
        );

        // // For now, just ensure the roundtrip produces valid TTCs
        // assert!(
        //     output_fonts.len() > 0,
        //     "Output TTC should contain at least one font"
        // );

        assert_eq!(input_cksum, output_cksum);

        assert_eq!(input_data.to_vec(), output_data);

        Ok(())
    }
}
