use anyhow::{bail, Result};
use regex::Regex;
use usvg::{NodeExt, PathBbox};

/// Splits a stream of multiple SVGs (returned by dvisvgm).
pub fn split_svgs(bytes: &[u8]) -> Result<Vec<&[u8]>> {
    let mut reader = quick_xml::Reader::from_bytes(bytes);
    let mut cuts = vec![];
    let mut last_pos = 0;
    loop {
        match reader.read_event_unbuffered()? {
            quick_xml::events::Event::Decl(_) => cuts.push(last_pos),
            quick_xml::events::Event::Eof => break,
            _ => {}
        }
        last_pos = reader.buffer_position();
    }
    cuts.push(bytes.len());
    Ok(cuts.windows(2).map(|w| &bytes[w[0]..w[1]]).collect())
}

/// Finds paths and images in an SVG and computes their bboxes.
pub fn paths_to_bboxes(tree: &usvg::Tree) -> Vec<PathBbox> {
    tree.root()
        .descendants()
        .filter(|node| !node.has_children())
        .filter_map(|node| node.calculate_bbox())
        .collect()
}

/// Parses raw svg data to a usvg Tree.
///
/// Under DVI/XDV mode, dvisvgm embeds fonts into the svg that unfortunately will not be recognized
/// by usvg's parser by default (because it does not support @font-face), so we have to do some
/// hacks here to help it.
pub fn parse_to_tree(svg_data: &[u8]) -> Result<usvg::Tree> {
    let mut reader = quick_xml::Reader::from_bytes(svg_data);
    let mut options = usvg::Options::default();

    let font_face_regex = Regex::new(
        // Follows the format of dvisvgm's FontWriter::writeCSSFontFace, defined in FontWriter.cpp.
        r"@font-face\{font-family:(\w+);src:url\(data:application/x-font-(\w+);base64,([-A-Za-z0-9+/=]+)\) format\('\w+'\);\}",
    )?;

    loop {
        match reader.read_event_unbuffered()? {
            quick_xml::events::Event::Eof => break,
            quick_xml::events::Event::CData(e) => {
                let inner = e.into_inner();
                let cdata = String::from_utf8_lossy(&inner);
                for capture in font_face_regex.captures_iter(&cdata) {
                    let font_family = capture.get(1).unwrap().as_str();
                    let _font_format = capture.get(2).unwrap().as_str();
                    let font_data = base64::decode(capture.get(3).unwrap().as_str())?;
                    options
                        .fontdb
                        .load_font_data(patch_font(&font_data, font_family)?);
                }
            }
            _ => {}
        }
    }

    let tree = usvg::Tree::from_data(svg_data, &options.to_ref())?;
    Ok(tree)
}

/// Patch a TTF font generated by dvisvgm so that fontdb's database is happy with it.
///
/// A problem with dvisvgm's subsetted font file is that is does not have a name. Here we modify the
/// name table and manually add a record to it.
///
/// Checksums are not updated because ttf_parser does not check them by default anyway.
fn patch_font(font: &[u8], family: &str) -> Result<Vec<u8>> {
    debug_assert!(family.is_ascii()); // Need to check because we are going to encode the name as
                                      // Mac encoding which works with ASCII only. We could use Unicode, but TTF requires Unicode
                                      // names to be encoded in UTF16BE and there's no easy way to do that in Rust without third-party
                                      // libraries.
    let read_u16 = |offset: usize| u16::from_be_bytes(font[offset..offset + 2].try_into().unwrap());
    let read_u32 = |offset: usize| u32::from_be_bytes(font[offset..offset + 4].try_into().unwrap());

    let n_tables = read_u16(4) as usize;
    let (offset, length, table_dir_entry_offset) = {
        let mut table_dir_entry_offset = 0;
        let mut table_offset = 0;
        let mut table_length = 0;
        for i in 0..n_tables {
            let offset = i * 16 + 12;
            let table_name = &font[offset..offset + 4];
            if b"name" == table_name {
                table_dir_entry_offset = offset;
                table_offset = read_u32(offset + 8) as usize;
                table_length = read_u32(offset + 12) as usize;
            }
        }
        if table_length == 0 {
            bail!("font missing name table");
        }
        (table_offset, table_length, table_dir_entry_offset)
    };
    let format = read_u16(offset);
    if format != 0 {
        // Could happen if it's OTF font.
        bail!("wrong name table version in font")
    }
    let mut n_records = read_u16(offset + 2) as usize;
    let string_offset = offset + (read_u16(offset + 4) as usize);
    let mut string = font[string_offset..offset + length].to_vec();
    let mut has_name = false;
    let mut has_post = false;
    for i in 0..n_records {
        let record_offset = offset + 6 + 12 * i;
        let platform_id = read_u16(record_offset);
        let name_id = read_u16(record_offset + 6);
        if platform_id == 1 || platform_id == 0
        // Unicode or MacRoman
        {
            match name_id {
                1 /* family name */ => has_name = true,
                6 /* postscript name */ => has_post = true,
                _ => {}
            }
        }
    }

    let mut result = font.to_vec();
    let new_offset = result.len();
    // We'll write the modified name table at the end of the original font.
    result.extend_from_slice(&font[offset..offset + 6 + 12 * n_records]);
    if !has_name || !has_post {
        let name_offset = string.len() as u16;
        let name_length = family.len() as u16;
        // Add the new name to the end of the string slice.
        string.extend_from_slice(family.as_bytes());
        if !has_name {
            n_records += 1;
            //                         Mac Roman English name
            result.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 1]);
            result.extend(name_length.to_be_bytes());
            result.extend(name_offset.to_be_bytes());
        }
        if !has_post {
            n_records += 1;
            //                         Mac Roman English postscript name
            result.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 6]);
            result.extend(name_length.to_be_bytes());
            result.extend(name_offset.to_be_bytes());
        }
    }
    result.extend(string);
    // Update n_records in the new table.
    result[new_offset + 2..new_offset + 4].copy_from_slice(&(n_records as u16).to_be_bytes());
    // Update string offset in the new table.
    result[new_offset + 4..new_offset + 6]
        .copy_from_slice(&(6 + 12 * n_records as u16).to_be_bytes());
    // Update the table offset in the directory.
    result[table_dir_entry_offset + 8..table_dir_entry_offset + 12]
        .copy_from_slice(&(new_offset as u32).to_be_bytes());
    // Update the table length in the directory.
    let new_table_length = result.len() - new_offset;
    result[table_dir_entry_offset + 12..table_dir_entry_offset + 16]
        .copy_from_slice(&(new_table_length as u32).to_be_bytes());
    Ok(result)
}

/// Given a slice of bounding boxes and a y range, compute the x range that exactly covers all
/// bounding boxes which have non-empty intersection with the y range. There is a tolerance term
/// for robustness, because dvisvgm and synctex aren't always very accurate.
pub fn x_range_for_y_range(
    bboxes: &[PathBbox],
    y_min: f64,
    y_max: f64,
    tol: f64,
    margin: f64,
) -> Option<(f64, f64)> {
    let mut x_min = f64::MAX;
    let mut x_max = f64::MIN;
    let y_min = y_min - tol;
    let y_max = y_max + tol;
    for bbox in bboxes {
        if y_min.max(bbox.top()) <= y_max.min(bbox.bottom()) {
            x_min = x_min.min(bbox.left());
            x_max = x_max.max(bbox.right());
        }
    }
    if x_min == f64::MAX {
        None
    } else {
        Some((x_min - margin, x_max + margin))
    }
}

// TODO: perhaps merge the function below with the function above, to save one full traversal of
// bboxes.
pub fn refine_y_range(bboxes: &[PathBbox], y_min: f64, y_max: f64, tol: f64) -> (f64, f64) {
    let mut new_y_min = f64::MAX;
    let mut new_y_max = f64::MIN;
    let y_min = y_min - tol;
    let y_max = y_max + tol;
    for bbox in bboxes {
        // if y_min <= bbox.top() && bbox.bottom() <= y_max {
        if y_min.max(bbox.top()) <= y_max.min(bbox.bottom()) {
            new_y_min = new_y_min.min(bbox.top());
            new_y_max = new_y_max.max(bbox.bottom());
        }
    }
    if new_y_min == f64::MAX {
        (y_min + tol, y_max - tol)
    } else {
        (new_y_min, new_y_max)
    }
}
