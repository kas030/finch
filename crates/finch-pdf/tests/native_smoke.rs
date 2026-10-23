//! Native validation: FINCH_PDFIUM_PATH must point to the bundled library.
//! cargo test -p finch-pdf --test native_smoke -- --ignored --nocapture
use finch_pdf::*;
use std::{
    fmt::Write,
    path::PathBuf,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

fn fixture() -> Vec<u8> {
    let content = "1 0 0 rg 20 50 80 40 re f\n0 1 0 rg 500 250 80 80 re f\n0 0 0 rg BT /F1 18 Tf 72 700 Td (Finch research search SEARCH) Tj ET";
    let rotated = "0 0 1 rg 20 40 60 20 re f\n0 0 0 rg BT /F1 12 Tf 30 240 Td (Rotated page) Tj ET";
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R /Outlines 8 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 800] /Resources << /Font << /F1 5 0 R >> >> /Contents 6 0 R /Annots [10 0 R 11 0 R] >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 300] /Rotate 90 /Resources << /Font << /F1 5 0 R >> >> /Contents 7 0 R >>".to_string(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
        format!("<< /Length {} >>\nstream\n{rotated}\nendstream", rotated.len()),
        "<< /Type /Outlines /First 9 0 R /Last 9 0 R /Count 1 >>".to_string(),
        "<< /Title (Second page) /Parent 8 0 R /Dest [4 0 R /Fit] >>".to_string(),
        "<< /Type /Annot /Subtype /Link /Rect [50 600 150 620] /Border [0 0 0] /A << /S /URI /URI (https://example.org/paper) >> >>".to_string(),
        "<< /Type /Annot /Subtype /Link /Rect [50 550 150 570] /Border [0 0 0] /Dest [4 0 R /Fit] >>".to_string(),
        "<< /Title (Finch native test) /Author (Finch) >>".to_string(),
    ];
    let mut pdf = String::from("%PDF-1.7\n");
    let mut offsets = vec![0usize];
    for (index, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        writeln!(&mut pdf, "{} 0 obj\n{}\nendobj", index + 1, object).unwrap();
    }
    let xref = pdf.len();
    writeln!(
        &mut pdf,
        "xref\n0 {}\n0000000000 65535 f ",
        objects.len() + 1
    )
    .unwrap();
    for offset in &offsets[1..] {
        writeln!(&mut pdf, "{offset:010} 00000 n ").unwrap();
    }
    writeln!(
        &mut pdf,
        "trailer\n<< /Size {} /Root 1 0 R /Info 12 0 R >>\nstartxref\n{xref}\n%%EOF",
        objects.len() + 1
    )
    .unwrap();
    pdf.into_bytes()
}

struct FixtureDirectory(PathBuf);
impl Drop for FixtureDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
#[ignore = "requires bundled PDFium; run explicitly with FINCH_PDFIUM_PATH"]
fn native_reader_render_search_navigation_cancellation_and_shutdown() {
    let library =
        std::env::var_os("FINCH_PDFIUM_PATH").expect("FINCH_PDFIUM_PATH must name PDFium");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = FixtureDirectory(
        std::env::temp_dir().join(format!("finch-pdf-{}-{nonce}", std::process::id())),
    );
    std::fs::create_dir_all(&directory.0).unwrap();
    let pdf = directory.0.join("论文阅读.pdf");
    std::fs::write(&pdf, fixture()).unwrap();
    let worker = PdfWorker::spawn_with_cache_budget(Some(library.clone().into()), 0).unwrap();
    let events = worker.events();
    assert!(matches!(
        events
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .result
            .unwrap(),
        PdfResponse::Ready
    ));
    assert!(matches!(
        PdfWorker::spawn(Some(library.clone().into())),
        Err(PdfError::AlreadyRunning)
    ));
    worker.set_generation(1);
    let mut id = 0;
    let mut request = |command| {
        id += 1;
        worker
            .try_send(PdfRequest {
                id,
                generation: 1,
                priority: Priority::Visible,
                command,
            })
            .unwrap();
        let event = events.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(event.id, id);
        assert_eq!(event.generation, 1);
        event.result
    };
    match request(PdfCommand::Open {
        path: pdf.clone(),
        password: None,
    })
    .unwrap()
    {
        PdfResponse::Opened(info) => {
            assert_eq!(info.page_count, 2);
            assert_eq!(info.first_page.width, 600.0);
            assert_eq!(info.title.as_deref(), Some("Finch native test"));
        }
        other => panic!("unexpected {other:?}"),
    }
    let start = Instant::now();
    match request(PdfCommand::RenderTile {
        key: TileKey {
            page: 0,
            scale_milli: 1000,
            x: 0,
            y: 512,
        },
    })
    .unwrap()
    {
        PdfResponse::Tile(tile) => {
            assert_eq!((tile.width, tile.height), (512, 288));
            assert_eq!(tile.rgba.len(), 512 * 288 * 4);
            let pixel = ((720 - 512) * 512 + 40) * 4;
            assert_eq!(&tile.rgba[pixel..pixel + 4], &[255, 0, 0, 255]);
            assert_eq!(&tile.rgba[0..4], &[255, 255, 255, 255]);
        }
        other => panic!("unexpected {other:?}"),
    }
    println!("native visible tile: {:?}", start.elapsed());
    match request(PdfCommand::Metadata { path: pdf.clone() }).unwrap() {
        PdfResponse::Metadata {
            title, page_count, ..
        } => {
            assert_eq!(title.as_deref(), Some("Finch native test"));
            assert_eq!(page_count, 2);
        }
        other => panic!("unexpected {other:?}"),
    }
    // Analytic golden image: the green rectangle crosses both 512px seams.
    // Every pixel in the region must agree with the PDF vector geometry.
    let mut assembled = vec![0u8; 600 * 800 * 4];
    for y in [0, 512] {
        for x in [0, 512] {
            let key = TileKey {
                page: 0,
                scale_milli: 1000,
                x,
                y,
            };
            let tile = match request(PdfCommand::RenderTile { key }).unwrap() {
                PdfResponse::Tile(tile) => tile,
                other => panic!("unexpected {other:?}"),
            };
            for row in 0..tile.height as usize {
                let destination = ((y as usize + row) * 600 + x as usize) * 4;
                let source = row * tile.width as usize * 4;
                assembled[destination..destination + tile.width as usize * 4]
                    .copy_from_slice(&tile.rgba[source..source + tile.width as usize * 4]);
            }
        }
    }
    for y in 450..570 {
        for x in 480..595 {
            let expected: [u8; 4] = if (500..580).contains(&x) && (470..550).contains(&y) {
                [0, 255, 0, 255]
            } else {
                [255; 4]
            };
            let pixel = (y * 600 + x) * 4;
            assert_eq!(
                &assembled[pixel..pixel + 4],
                &expected,
                "tile seam mismatch at ({x}, {y})"
            );
        }
    }
    match request(PdfCommand::Text { page: 0 }).unwrap() {
        PdfResponse::Text(text) => {
            assert!(text.text.contains("Finch research search SEARCH"));
            let first = text.chars.iter().find(|ch| ch.character == 'F').unwrap();
            assert!(first.bounds.x >= 71.0 && first.bounds.x < 90.0);
            assert!(first.bounds.y > 80.0 && first.bounds.y < 101.0);
        }
        other => panic!("unexpected {other:?}"),
    }
    match request(PdfCommand::Search {
        page: 0,
        query: "search".into(),
        case_sensitive: false,
    })
    .unwrap()
    {
        PdfResponse::Search { hits, .. } => {
            assert_eq!(hits.len(), 3);
            assert!(hits.iter().all(|hit| !hit.bounds.is_empty()));
        }
        other => panic!("unexpected {other:?}"),
    }
    match request(PdfCommand::Search {
        page: 0,
        query: "SEARCH".into(),
        case_sensitive: true,
    })
    .unwrap()
    {
        PdfResponse::Search { hits, .. } => assert_eq!(hits.len(), 1),
        other => panic!("unexpected {other:?}"),
    }
    match request(PdfCommand::Bookmarks).unwrap() {
        PdfResponse::Bookmarks(bookmarks) => {
            assert_eq!(bookmarks.len(), 1);
            assert_eq!(bookmarks[0].page, Some(1));
            assert_eq!(bookmarks[0].title, "Second page");
        }
        other => panic!("unexpected {other:?}"),
    }
    match request(PdfCommand::Links { page: 0 }).unwrap() {
        PdfResponse::Links { links, .. } => {
            assert_eq!(links.len(), 2);
            assert!(
                matches!(&links[0].target, LinkTarget::Uri(uri) if uri == "https://example.org/paper")
            );
            assert!((links[0].bounds.y - 180.0).abs() < 0.1);
            assert!(matches!(links[1].target, LinkTarget::Page(1)));
        }
        other => panic!("unexpected {other:?}"),
    }
    match request(PdfCommand::PageInfo { page: 1 }).unwrap() {
        PdfResponse::PageInfo { size, .. } => assert_eq!((size.width, size.height), (300.0, 200.0)),
        other => panic!("unexpected {other:?}"),
    }
    let rotated = match request(PdfCommand::Text { page: 1 }).unwrap() {
        PdfResponse::Text(text) => text,
        other => panic!("unexpected {other:?}"),
    };
    let first = rotated.chars.iter().find(|ch| ch.character == 'R').unwrap();
    assert!(first.bounds.x > 239.0 && first.bounds.x < 255.0);
    assert!(first.bounds.y > 29.0 && first.bounds.y < 40.0);
    assert!(matches!(
        request(PdfCommand::PageInfo { page: 2 }),
        Err(PdfError::InvalidPage(2))
    ));
    let encrypted = directory.0.join("encrypted.pdf");
    std::fs::write(&encrypted, include_bytes!("fixtures/encrypted.pdf")).unwrap();
    assert!(matches!(
        request(PdfCommand::Open {
            path: encrypted.clone(),
            password: None
        }),
        Err(PdfError::PasswordRequired)
    ));
    assert!(matches!(
        request(PdfCommand::Open {
            path: encrypted.clone(),
            password: Some("wrong".into())
        }),
        Err(PdfError::PasswordRequired)
    ));
    assert!(matches!(
        request(PdfCommand::Open {
            path: encrypted,
            password: Some("finch".into())
        }),
        Ok(PdfResponse::Opened(_))
    ));
    match request(PdfCommand::Text { page: 0 }).unwrap() {
        PdfResponse::Text(text) => assert!(text.text.contains("Finch password protected paper")),
        other => panic!("unexpected {other:?}"),
    }
    let corrupt = directory.0.join("corrupt.pdf");
    std::fs::write(&corrupt, b"not a PDF").unwrap();
    assert!(matches!(
        request(PdfCommand::Open {
            path: corrupt,
            password: None
        }),
        Err(PdfError::Native(_))
    ));
    assert!(matches!(
        request(PdfCommand::Text { page: 0 }),
        Err(PdfError::NoDocument)
    ));
    assert!(matches!(
        request(PdfCommand::Open {
            path: directory.0.join("missing.pdf"),
            password: None
        }),
        Err(PdfError::Io(_))
    ));
    request(PdfCommand::Open {
        path: pdf.clone(),
        password: None,
    })
    .unwrap();
    {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&pdf)
            .unwrap()
            .write_all(b"\n% file revision changed\n")
            .unwrap();
    }
    assert!(matches!(
        request(PdfCommand::Text { page: 0 }),
        Err(PdfError::FileChanged)
    ));
    request(PdfCommand::Open {
        path: pdf,
        password: None,
    })
    .unwrap();
    worker.set_generation(2);
    worker
        .try_send(PdfRequest {
            id: 100,
            generation: 1,
            priority: Priority::Visible,
            command: PdfCommand::Text { page: 0 },
        })
        .unwrap();
    assert!(matches!(
        events.recv_timeout(Duration::from_secs(10)).unwrap().result,
        Err(PdfError::Cancelled)
    ));
    // Drop must unblock a producer even if the bounded output queue is full.
    for id in 200..220 {
        worker
            .try_send(PdfRequest {
                id,
                generation: 2,
                priority: Priority::Visible,
                command: PdfCommand::PageInfo { page: 0 },
            })
            .unwrap();
    }
    drop(worker);
    let replacement = PdfWorker::spawn(Some(library.into())).unwrap();
    assert!(matches!(
        replacement
            .events()
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .result
            .unwrap(),
        PdfResponse::Ready
    ));
    drop(replacement);
}
