use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use docs_rs::storage::{compress, decompress, CompressionAlgorithm};

pub fn regex_capture_matches(c: &mut Criterion) {
    // this isn't a great benchmark because it only tests on one file
    // ideally we would build a whole crate and compress each file, taking the average
    let html = std::fs::read_to_string("benches/struct.CaptureMatches.html").unwrap();
    let html_slice = html.as_bytes();

    c.benchmark_group("regex html")
        .throughput(Throughput::Bytes(html_slice.len() as u64))
        .bench_function("compress zstd", |b| {
            b.iter(|| compress(black_box(html_slice), CompressionAlgorithm::Zstd));
        })
        .bench_function("decompress zstd", |b| {
            b.iter(|| {
                decompress(
                    black_box(html_slice),
                    CompressionAlgorithm::Zstd,
                    5 * 1024 * 1024,
                )
            });
        })
        .bench_function("compress bzip2", |b| {
            b.iter(|| compress(black_box(html_slice), CompressionAlgorithm::Bzip2));
        })
        .bench_function("decompress bzip2", |b| {
            b.iter(|| {
                decompress(
                    black_box(html_slice),
                    CompressionAlgorithm::Bzip2,
                    5 * 1024 * 1024,
                )
            });
        });
}

criterion_group!(compression, regex_capture_matches);
criterion_main!(compression);
