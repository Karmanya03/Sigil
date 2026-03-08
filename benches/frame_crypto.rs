use criterion::{Criterion, criterion_group, criterion_main};

fn bench_frame_encrypt(c: &mut Criterion) {
    // TODO: benchmark encrypt_frame with realistic frame sizes
    c.bench_function("frame_encrypt_1kb", |b| {
        b.iter(|| {
            // placeholder
            let _x = 1 + 1;
        })
    });
}

criterion_group!(benches, bench_frame_encrypt);
criterion_main!(benches);
