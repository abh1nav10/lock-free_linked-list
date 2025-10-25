#![allow(unexpected_cfgs)]

#[cfg(test)]
#[cfg(loom)]
mod loom_tests {
    use ruby::descriptor::RawDescriptor;
    use ruby::list::LinkedList;
    use std::sync::Arc;
    #[test]
    fn concurrency_test() {
        loom::model(|| {
            let new = Arc::new(LinkedList::new());
            let raw_descriptor = Arc::new(RawDescriptor::new(&new));
            let cloned1 = Arc::clone(&new);
            let cloned2 = Arc::clone(&new);
            let raw_cloned1 = Arc::clone(&raw_descriptor);
            let raw_cloned2 = Arc::clone(&raw_descriptor);
            let t1 = loom::thread::spawn(move || {
                cloned1.insert_from_head(2, &raw_cloned1);
            });
            let t2 = loom::thread::spawn(move || {
                let _ = cloned2.delete_from_tail(&raw_cloned2);
            });
            t1.join().unwrap();
            t2.join().unwrap();
        });
    }
}

#[cfg(test)]
#[cfg(loom)]
mod hazard_test {
    use loom::sync::Arc;
    use ruby::hazard::{DropBox, HazPtrHolder, HazPtrObject};
    use ruby::sync::atomic::{AtomicPtr, AtomicUsize};
    use std::sync::atomic::Ordering;
    struct CountDrops(Arc<AtomicUsize>);
    impl Drop for CountDrops {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    impl CountDrops {
        fn get_number_of_drops(&self) -> usize {
            self.0.load(Ordering::Relaxed)
        }
    }
    #[test]
    fn test_hazard() {
        loom::model(|| {
            let new = Arc::new(AtomicUsize::new(0));
            let check = CountDrops(new.clone());
            let value1 = CountDrops(new.clone());
            let value2 = CountDrops(new.clone());
            let boxed1 = Box::into_raw(Box::new(value1));
            let boxed2 = Box::into_raw(Box::new(value2));
            let atm_ptr = AtomicPtr::new(boxed1);
            let mut holder = HazPtrHolder::default();
            let guard = unsafe { holder.load(&atm_ptr) };
            static DROPBOX: DropBox = DropBox::new();
            std::mem::drop(guard);
            if let Some(mut wrapper) = unsafe { holder.swap(&atm_ptr, boxed2, &DROPBOX) } {
                wrapper.retire();
            }
            assert_eq!(check.get_number_of_drops(), 1 as usize);
            let _ = unsafe { Box::from_raw(boxed2) };
            std::mem::drop(check);
        });
    }
}
