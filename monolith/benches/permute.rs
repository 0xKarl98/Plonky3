use criterion::{criterion_group, criterion_main, Criterion};
use p3_field::AbstractField;
use p3_mersenne_31::Mersenne31;
use p3_monolith::{MonolithM31, MonolithMdsMatrixM31};

fn permute_benchmark(c: &mut Criterion) {
    let mds = MonolithMdsMatrixM31::<6>;
    let monolith: MonolithM31<_, 16, 5> = MonolithM31::new(mds);

    let mut input: [Mersenne31; 16] = [Mersenne31::zero(); 16];
    for (i, inp) in input.iter_mut().enumerate() {
        *inp = Mersenne31::from_canonical_usize(i);
    }

    c.bench_function("monolith permutation", |b| {
        b.iter(|| monolith.permutation(&mut input))
    });
}

criterion_group!(benches, permute_benchmark);
criterion_main!(benches);
