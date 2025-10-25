use criterion::{Criterion, criterion_group, criterion_main};
use ruby::LinkedList;
use std::collections::LinkedList as StdLinkedList;
use std::sync::Mutex;

fn std_mutex_list() {
    let new = &Mutex::new(StdLinkedList::new());
    std::thread::scope(|s| {
        for i in 0..10 {
            s.spawn(move || {
                new.lock().unwrap().push_front(i);
            });
        }
        for _ in 0..10 {
            s.spawn(move || {
                new.lock().unwrap().pop_back();
            });
        }
    });
}

fn ruby() {
    let new = &LinkedList::new();
    std::thread::scope(|s| {
        for i in 0..10 {
            s.spawn(move || {
                new.insert_from_head(i);
            });
        }
        for _ in 0..10 {
            s.spawn(move || {
                new.delete_from_tail();
            });
        }
    });
}

fn benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("Bravo");
    group.bench_function("Std", |b| b.iter(|| std_mutex_list()));
    group.bench_function("Ruby", |b| b.iter(|| ruby()));
    group.finish();
}

criterion_group! {name = benchmarks; config = Criterion::default(); targets = benchmark}
criterion_main!(benchmarks);
