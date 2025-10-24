#[cfg(test)]
mod queue_test {
    use ruby::descriptor::RawDescriptor;
    use ruby::list::LinkedList;
    use std::time::Instant;
    #[test]
    fn test_one() {
        let current = Instant::now();
        let new = &LinkedList::new();
        let raw = &RawDescriptor::new();
        std::thread::scope(|s| {
            for i in 0..10 {
                s.spawn(move || {
                    new.insert_from_head(i, &raw);
                });
            }
        });
        std::thread::scope(|s| {
            for _ in 0..10 {
                s.spawn(move || {
                    let _ = new.delete_from_tail(&raw);
                });
            }
        });
        assert_eq!(0 as usize, new.length());
        let time_taken = current.elapsed();
        println!("{:?}", time_taken.as_micros());
    }
}

