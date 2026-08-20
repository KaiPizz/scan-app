//! `--recompress <folder>`: one-off batch conversion of the library's JPEG
//! PDFs (≈2 MB/page) into bilevel G4 PDFs (≈60 KB/page), in place.
//!
//! Per file: read with the app's own extractor (foreign/locked PDFs are
//! skipped, never touched) → every JPEG page goes through the live binarizer
//! → new PDF rendered → **verified by reading it back** (page count, G4,
//! dimensions, rotation) → atomic replace → original modification time
//! restored so the library's "Data" column and sort order do not change.
//! A log file is written next to the folder (the release build has no console).

use crate::document::{
    EncodedPage, PageEncoding, ScannedPage, extract_pdf_pages, extract_pdf_pages_bytes,
    rebinarize_page, render_pdf,
};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Summary of one run (also what the tests assert on).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub converted: usize,
    pub already_bilevel: usize,
    pub skipped_foreign: usize,
    pub failed: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

/// Runs the batch over `root` (recursively). Returns a process exit code:
/// 0 = no failures, 1 = at least one file failed, 2 = folder unreadable.
pub fn run(root: &Path) -> i32 {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let log_path = root.join(format!("recompress-{stamp}.log"));
    let mut log = fs::File::create(&log_path).ok();
    let mut emit = |line: &str| {
        println!("{line}");
        if let Some(file) = log.as_mut() {
            let _ = writeln!(file, "{line}");
        }
    };
    let pdfs = match collect_pdfs(root) {
        Ok(pdfs) => pdfs,
        Err(error) => {
            emit(&format!("BŁĄD: nie można odczytać folderu {}: {error}", root.display()));
            return 2;
        }
    };
    emit(&format!("Kompresja: {} plików PDF w {}", pdfs.len(), root.display()));
    let summary = run_over(&pdfs, root, &mut emit);
    emit(&format!(
        "RAZEM: przekonwertowano {} · już czarno-białe {} · pominięto (obce) {} · błędy {} · {:.1} MB → {:.1} MB",
        summary.converted,
        summary.already_bilevel,
        summary.skipped_foreign,
        summary.failed,
        summary.bytes_before as f64 / 1_048_576.0,
        summary.bytes_after as f64 / 1_048_576.0,
    ));
    if summary.failed > 0 { 1 } else { 0 }
}

fn collect_pdfs(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else if name.to_ascii_lowercase().ends_with(".pdf") {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

pub fn run_over(pdfs: &[PathBuf], root: &Path, emit: &mut dyn FnMut(&str)) -> Summary {
    let mut summary = Summary::default();
    for path in pdfs {
        let rel = path.strip_prefix(root).unwrap_or(path).display().to_string();
        match recompress_file(path) {
            Ok(Outcome::Converted { pages, before, after }) => {
                summary.converted += 1;
                summary.bytes_before += before;
                summary.bytes_after += after;
                emit(&format!(
                    "{rel}: {:.1} MB → {:.2} MB ({pages} stron)",
                    before as f64 / 1_048_576.0,
                    after as f64 / 1_048_576.0
                ));
            }
            Ok(Outcome::AlreadyBilevel) => {
                summary.already_bilevel += 1;
                emit(&format!("{rel}: już czarno-biały, bez zmian"));
            }
            Ok(Outcome::Foreign(reason)) => {
                summary.skipped_foreign += 1;
                emit(&format!("{rel}: POMINIĘTO — {reason}"));
            }
            Err(error) => {
                summary.failed += 1;
                emit(&format!("{rel}: BŁĄD — {error}"));
            }
        }
    }
    summary
}

enum Outcome {
    Converted { pages: usize, before: u64, after: u64 },
    AlreadyBilevel,
    Foreign(String),
}

fn recompress_file(path: &Path) -> Result<Outcome, String> {
    let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
    let before = metadata.len();
    let modified = metadata.modified().ok();
    let pages = match extract_pdf_pages(path) {
        Ok(pages) => pages,
        // Not our layout (or OCR/annotations/forms): leave it alone.
        Err(reason) => return Ok(Outcome::Foreign(reason)),
    };
    if pages.iter().all(|(page, _)| page.encoding == PageEncoding::G4) {
        return Ok(Outcome::AlreadyBilevel);
    }
    let converted = convert_pages(&pages)?;
    let refs: Vec<(&ScannedPage, u8)> = converted
        .iter()
        .map(|(page, turns)| (page, *turns))
        .collect();
    let bytes = render_pdf(&refs)?;
    verify(&bytes, &pages)?;
    crate::atomic_file::write(path, &bytes).map_err(|error| error.to_string())?;
    if let Some(modified) = modified {
        // Best effort: a failed timestamp restore only affects sort order.
        if let Ok(file) = fs::OpenOptions::new().write(true).open(path) {
            let _ = file.set_modified(modified);
        }
    }
    Ok(Outcome::Converted {
        pages: pages.len(),
        before,
        after: bytes.len() as u64,
    })
}

/// Binarizes JPEG pages (in parallel), passes G4 pages through unchanged.
fn convert_pages(pages: &[(EncodedPage, u8)]) -> Result<Vec<(ScannedPage, u8)>, String> {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .clamp(1, 4);
    let chunk = pages.len().div_ceil(threads).max(1);
    let results: Vec<Result<Vec<(ScannedPage, u8)>, String>> = std::thread::scope(|scope| {
        let handles: Vec<_> = pages
            .chunks(chunk)
            .map(|slice| {
                scope.spawn(move || {
                    slice
                        .iter()
                        .map(|(page, turns)| {
                            let converted = match page.encoding {
                                PageEncoding::G4 => crate::document::page_from_encoded(page.clone())?,
                                PageEncoding::Jpeg => rebinarize_page(page)?,
                            };
                            Ok((converted, *turns))
                        })
                        .collect::<Result<Vec<_>, String>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|_| Err("wątek konwersji zakończył się awarią".to_owned()))
            })
            .collect()
    });
    let mut out = Vec::with_capacity(pages.len());
    for result in results {
        out.extend(result?);
    }
    Ok(out)
}

/// The new PDF must read back through the same extractor with the same page
/// count, rotations and dimensions, every page G4 — otherwise nothing is written.
fn verify(bytes: &[u8], original: &[(EncodedPage, u8)]) -> Result<(), String> {
    let reread = extract_pdf_pages_bytes(bytes)
        .map_err(|error| format!("weryfikacja nowego PDF nie powiodła się: {error}"))?;
    if reread.len() != original.len() {
        return Err(format!(
            "weryfikacja: {} stron zamiast {}",
            reread.len(),
            original.len()
        ));
    }
    for (index, ((new_page, new_turns), (old_page, old_turns))) in
        reread.iter().zip(original).enumerate()
    {
        if new_page.encoding != PageEncoding::G4
            || new_turns != old_turns
            || (new_page.width, new_page.height) != (old_page.width, old_page.height)
            || new_page.bytes.len() < 16
        {
            return Err(format!("weryfikacja: strona {} nie zgadza się", index + 1));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{A4_HEIGHT_PX, A4_WIDTH_PX, ColorMode};
    use image::{Rgb, RgbImage};
    use std::time::Duration;

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "skaner-recompress-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("Sektor")).expect("dir");
        dir
    }

    fn page(mode: ColorMode) -> ScannedPage {
        let mut image = RgbImage::from_pixel(A4_WIDTH_PX, A4_HEIGHT_PX, Rgb([245, 245, 245]));
        for y in 500..540 {
            for x in 300..1700 {
                image.put_pixel(x, y, Rgb([20, 20, 20]));
            }
        }
        crate::document::process_page_test_image(image, mode)
    }

    #[test]
    fn converts_jpeg_pdfs_keeps_g4_and_foreign_and_restores_mtime() {
        let dir = test_dir("mix");
        let jpeg_pdf = dir.join("Sektor").join("stary.pdf");
        let g4_pdf = dir.join("nowy.pdf");
        let foreign_pdf = dir.join("obcy.pdf");
        let colour = page(ColorMode::Color);
        fs::write(&jpeg_pdf, render_pdf(&[(&colour, 0), (&colour, 3)]).unwrap()).unwrap();
        let bilevel = page(ColorMode::BlackWhite);
        let g4_bytes = render_pdf(&[(&bilevel, 1)]).unwrap();
        fs::write(&g4_pdf, &g4_bytes).unwrap();
        fs::write(&foreign_pdf, b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\ntrailer\n<<>>\n%%EOF").unwrap();
        let old_mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        fs::OpenOptions::new().write(true).open(&jpeg_pdf).unwrap().set_modified(old_mtime).unwrap();
        let before = fs::metadata(&jpeg_pdf).unwrap().len();

        let pdfs = collect_pdfs(&dir).unwrap();
        assert_eq!(pdfs.len(), 3);
        let mut lines = Vec::new();
        let summary = run_over(&pdfs, &dir, &mut |line| lines.push(line.to_owned()));
        assert_eq!(summary.converted, 1, "{lines:?}");
        assert_eq!(summary.already_bilevel, 1);
        assert_eq!(summary.skipped_foreign, 1);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.bytes_before, before);

        let reread = extract_pdf_pages(&jpeg_pdf).expect("converted PDF is ours");
        assert_eq!(reread.len(), 2);
        assert!(reread.iter().all(|(page, _)| page.encoding == PageEncoding::G4));
        assert_eq!(reread[1].1, 3, "rotation preserved");
        assert!(fs::metadata(&jpeg_pdf).unwrap().len() < before / 5);
        let restored = fs::metadata(&jpeg_pdf).unwrap().modified().unwrap();
        assert!(restored.duration_since(old_mtime).unwrap() < Duration::from_secs(2));
        assert_eq!(fs::read(&g4_pdf).unwrap(), g4_bytes, "G4 PDF untouched");
        assert!(fs::read(&foreign_pdf).unwrap().starts_with(b"%PDF-1.4\n1 0 obj"));
        assert!(lines.iter().any(|line| line.contains("obcy.pdf: POMINIĘTO")));
        let _ = fs::remove_dir_all(&dir);
    }
}
