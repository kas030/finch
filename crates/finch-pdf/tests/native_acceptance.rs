//! Large native acceptance fixtures are generated into a unique temporary
//! directory and removed afterwards; no large PDF is stored in the repository.
//! cargo test -p finch-pdf --test native_acceptance -- --ignored --nocapture
use finch_pdf::*;
use std::{
    fs::File,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

struct PdfWriter {
    file: BufWriter<File>,
    offsets: Vec<u64>,
    position: u64,
}
impl PdfWriter {
    fn new(path: &Path) -> Self {
        let mut result = Self {
            file: BufWriter::new(File::create(path).unwrap()),
            offsets: vec![0],
            position: 0,
        };
        result.bytes(b"%PDF-1.7\n");
        result
    }
    fn bytes(&mut self, bytes: &[u8]) {
        self.file.write_all(bytes).unwrap();
        self.position += bytes.len() as u64;
    }
    fn start(&mut self) {
        self.offsets.push(self.position);
        self.bytes(format!("{} 0 obj\n", self.offsets.len() - 1).as_bytes());
    }
    fn end(&mut self) {
        self.bytes(b"\nendobj\n");
    }
    fn object(&mut self, object: &str) {
        self.start();
        self.bytes(object.as_bytes());
        self.end();
    }
    fn stream(&mut self, data: &[u8]) {
        self.start();
        self.bytes(format!("<< /Length {} >>\nstream\n", data.len()).as_bytes());
        self.bytes(data);
        self.bytes(b"\nendstream");
        self.end();
    }
    fn finish(mut self) {
        let xref = self.position;
        self.bytes(format!("xref\n0 {}\n0000000000 65535 f \n", self.offsets.len()).as_bytes());
        for index in 1..self.offsets.len() {
            self.bytes(format!("{:010} 00000 n \n", self.offsets[index]).as_bytes());
        }
        self.bytes(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF",
                self.offsets.len()
            )
            .as_bytes(),
        );
        self.file.flush().unwrap();
    }
}

fn thousand_pages(path: &Path) {
    let mut pdf = PdfWriter::new(path);
    pdf.object("<< /Type /Catalog /Pages 2 0 R >>");
    let kids = (3..1003)
        .map(|id| format!("{id} 0 R"))
        .collect::<Vec<_>>()
        .join(" ");
    pdf.object(&format!("<< /Type /Pages /Kids [{kids}] /Count 1000 >>"));
    for index in 0..1000 {
        pdf.object(&format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {} 800] /Contents 1003 0 R >>",
            600 + index % 3 * 10
        ));
    }
    pdf.stream(b"0 0 1 rg 20 20 40 40 re f");
    pdf.finish();
}

fn scan_500_mb(path: &Path) {
    const PAGES: usize = 31;
    const IMAGE_BYTES: usize = 2400 * 2400 * 3;
    let mut pdf = PdfWriter::new(path);
    pdf.object("<< /Type /Catalog /Pages 2 0 R >>");
    let kids = (0..PAGES)
        .map(|index| format!("{} 0 R", 3 + index * 3))
        .collect::<Vec<_>>()
        .join(" ");
    pdf.object(&format!("<< /Type /Pages /Kids [{kids}] /Count {PAGES} >>"));
    let block = [245, 247, 242].repeat(2400 * 32);
    for index in 0..PAGES {
        let image = 5 + index * 3;
        let content = image - 1;
        pdf.object(&format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 600] /Resources << /XObject << /Scan {image} 0 R >> >> /Contents {content} 0 R >>"));
        pdf.stream(b"q 600 0 0 600 0 0 cm /Scan Do Q");
        pdf.start();
        pdf.bytes(format!("<< /Type /XObject /Subtype /Image /Width 2400 /Height 2400 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Length {IMAGE_BYTES} >>\nstream\n").as_bytes());
        for _ in 0..75 {
            pdf.bytes(&block);
        }
        pdf.bytes(b"\nendstream");
        pdf.end();
    }
    pdf.finish();
}

fn slow_vector_page(path: &Path) {
    let mut pdf = PdfWriter::new(path);
    pdf.object("<< /Type /Catalog /Pages 2 0 R >>");
    pdf.object("<< /Type /Pages /Kids [3 0 R] /Count 1 >>");
    pdf.object("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 512 512] /Contents 4 0 R >>");
    let mut stream = String::with_capacity(6_000_000);
    stream.push_str("0.1 0.2 0.3 rg\n");
    for index in 0..250_000 {
        use std::fmt::Write;
        writeln!(
            &mut stream,
            "{} {} 2 2 re f",
            index % 500,
            index / 500 % 500
        )
        .unwrap();
    }
    pdf.stream(stream.as_bytes());
    pdf.finish();
}

struct FixtureDirectory(PathBuf);
impl Drop for FixtureDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(windows)]
fn process_memory() -> (usize, usize) {
    #[repr(C)]
    struct Counters {
        size: u32,
        faults: u32,
        peak: usize,
        current: usize,
        paged_peak: usize,
        paged: usize,
        nonpaged_peak: usize,
        nonpaged: usize,
        pagefile: usize,
        peak_pagefile: usize,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut std::ffi::c_void;
        fn K32GetProcessMemoryInfo(
            process: *mut std::ffi::c_void,
            counters: *mut Counters,
            size: u32,
        ) -> i32;
    }
    let mut counters: Counters = unsafe { std::mem::zeroed() };
    counters.size = std::mem::size_of::<Counters>() as u32;
    let ok = unsafe {
        K32GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters,
            std::mem::size_of::<Counters>() as u32,
        )
    };
    assert_ne!(ok, 0);
    (counters.current, counters.peak)
}
#[cfg(not(windows))]
fn process_memory() -> (usize, usize) {
    (0, 0)
}

#[test]
#[ignore = "requires PDFium and generates a temporary 511 MiB scanned PDF"]
fn large_documents_and_cancellation_after_progressive_pause() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = FixtureDirectory(
        std::env::temp_dir().join(format!("finch-large-{}-{nonce}", std::process::id())),
    );
    std::fs::create_dir_all(&directory.0).unwrap();
    let many = directory.0.join("1000-pages.pdf");
    let scan = directory.0.join("500mb-scan.pdf");
    let slow = directory.0.join("slow-vectors.pdf");
    thousand_pages(&many);
    scan_500_mb(&scan);
    slow_vector_page(&slow);
    assert!(std::fs::metadata(&scan).unwrap().len() > 500 * 1024 * 1024);
    let worker = PdfWorker::spawn_with_cache_budget(
        Some(
            std::env::var_os("FINCH_PDFIUM_PATH")
                .expect("FINCH_PDFIUM_PATH is required")
                .into(),
        ),
        0,
    )
    .unwrap();
    let events = worker.events();
    assert!(matches!(
        events
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .result
            .unwrap(),
        PdfResponse::Ready
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
        let event = events.recv_timeout(Duration::from_secs(30)).unwrap();
        assert_eq!(event.id, id);
        event.result.unwrap()
    };
    let start = Instant::now();
    match request(PdfCommand::Open {
        path: many,
        password: None,
    }) {
        PdfResponse::Opened(info) => assert_eq!(info.page_count, 1000),
        other => panic!("unexpected {other:?}"),
    }
    match request(PdfCommand::RenderTile {
        key: TileKey {
            page: 999,
            scale_milli: 1000,
            x: 0,
            y: 512,
        },
    }) {
        PdfResponse::Tile(tile) => assert_eq!(tile.height, 288),
        other => panic!("unexpected {other:?}"),
    }
    println!(
        "1000-page open and page1000 tile: {:?}; process memory {:?}",
        start.elapsed(),
        process_memory()
    );
    let start = Instant::now();
    match request(PdfCommand::Open {
        path: scan,
        password: None,
    }) {
        PdfResponse::Opened(info) => assert_eq!(info.page_count, 31),
        other => panic!("unexpected {other:?}"),
    }
    match request(PdfCommand::RenderTile {
        key: TileKey {
            page: 30,
            scale_milli: 1000,
            x: 0,
            y: 0,
        },
    }) {
        PdfResponse::Tile(tile) => {
            assert_eq!((tile.width, tile.height), (512, 512));
            assert_eq!(
                &tile.rgba[256 * 512 * 4..256 * 512 * 4 + 4],
                &[245, 247, 242, 255]
            );
        }
        other => panic!("unexpected {other:?}"),
    }
    let (current, peak) = process_memory();
    println!(
        "511MiB scan open and last-page tile: {:?}; working set {current}, peak {peak}",
        start.elapsed()
    );
    #[cfg(windows)]
    assert!(
        current < 200 * 1024 * 1024,
        "reading one scan must not load the whole 511MiB file into memory"
    );
    request(PdfCommand::Open {
        path: slow,
        password: None,
    });
    let old_pauses = worker.stats().progressive_pauses;
    worker
        .try_send(PdfRequest {
            id: 1000,
            generation: 1,
            priority: Priority::Visible,
            command: PdfCommand::RenderTile {
                key: TileKey {
                    page: 0,
                    scale_milli: 1000,
                    x: 0,
                    y: 0,
                },
            },
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while worker.stats().progressive_pauses == old_pauses {
        assert!(
            Instant::now() < deadline,
            "native render never reached a progressive pause"
        );
        assert!(
            events.is_empty(),
            "heavy native render finished before reaching a pause"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let cancelled = Instant::now();
    worker.set_generation(2);
    let event = events.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(event.id, 1000);
    assert!(matches!(event.result, Err(PdfError::Cancelled)));
    assert!(worker.stats().cancelled_renders >= 1);
    println!(
        "cancellation after native progressive pause: {:?}; stats {:?}",
        cancelled.elapsed(),
        worker.stats()
    );
    for generation in 3..35 {
        worker.set_generation(generation);
        worker
            .try_send(PdfRequest {
                id: 2000 + generation,
                generation,
                priority: Priority::Prefetch,
                command: PdfCommand::RenderTile {
                    key: TileKey {
                        page: 0,
                        scale_milli: 2000,
                        x: 0,
                        y: 0,
                    },
                },
            })
            .unwrap();
    }
    worker.set_generation(35);
    worker
        .try_send(PdfRequest {
            id: 3000,
            generation: 35,
            priority: Priority::Visible,
            command: PdfCommand::PageInfo { page: 0 },
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let event = events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap();
        if event.id == 3000 {
            assert!(matches!(event.result, Ok(PdfResponse::PageInfo { .. })));
            break;
        }
    }
    drop(worker);
}
