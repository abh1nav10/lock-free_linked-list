#[cfg(test)]
mod queue_test {
    use ruby::list::LinkedList;
    use std::time::Instant;
    #[test]
    fn test_one() {
        let current = Instant::now();
        let new = &LinkedList::new();
        std::thread::scope(|s| {
            for i in 0..10 {
                s.spawn(move || {
                    new.insert_from_head(i);
                });
            }
        });
        std::thread::scope(|s| {
            for _ in 0..10 {
                s.spawn(move || {
                    let _ = new.delete_from_tail();
                });
            }
        });
        assert_eq!(0 as usize, new.length());
        let time_taken = current.elapsed();
        println!("{:?}", time_taken.as_micros());
    }
}
