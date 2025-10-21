#![allow(dead_code)]
use crate::RawDescriptor;
use std::marker::PhantomData;
use std::sync::atomic::Ordering;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize};

pub(crate) struct Node<T> {
    pub(crate) value: T,
    pub(crate) prev: AtomicPtr<Node<T>>,
    pub(crate) retired: AtomicBool,
    pub(crate) value_moved: AtomicBool,
}

impl<T> Node<T> {
    fn new(value: T) -> Self {
        Self {
            value,
            prev: AtomicPtr::new(std::ptr::null_mut()),
            // this field is to prevent that retirement of the same node more than once
            retired: AtomicBool::new(false),
            value_moved: AtomicBool::new(false),
        }
    }
}

pub struct LinkedList<T> {
    length: AtomicUsize,
    head: AtomicPtr<Node<T>>,
    tail: AtomicPtr<Node<T>>,
    marker: PhantomData<Node<T>>,
}

unsafe impl<T> Send for LinkedList<T> where T: Send {}
unsafe impl<T> Sync for LinkedList<T> where T: Sync {}

impl<T> LinkedList<T> {
    pub fn new() -> Self {
        Self {
            length: AtomicUsize::new(0),
            head: AtomicPtr::new(std::ptr::null_mut()),
            tail: AtomicPtr::new(std::ptr::null_mut()),
            marker: PhantomData,
        }
    }

    pub fn insert_from_head<'a>(&'a self, value: T, raw_descriptor: &RawDescriptor<'a, T>) {
        let boxed = Box::into_raw(Box::new(Node::new(value)));
        loop {
            let current = self.head.load(Ordering::Acquire);
            if current.is_null() {
                match self.head.compare_exchange(
                    std::ptr::null_mut(),
                    boxed,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        // We dont CAS the tail because we dont have a method to insert from the
                        // tail side.Therefore any possibility of some other thread inserting a
                        // tail after we swap the head but before we manage to store the tail does
                        // not  exist.
                        // Updating the tail is the only reason we have this loop in the first
                        // place otherwise the insert method has got all the capability to handle
                        // the case where head is an atomic pointer storing a null pointer.
                        self.tail.store(boxed, Ordering::Release);
                        println!("Reached insert");
                        self.length.fetch_add(1, Ordering::Relaxed);
                        break;
                    }
                    Err(_) => {
                        continue;
                    }
                }
            } else {
                raw_descriptor.insert(&self.head, &self.tail, boxed);
                println!("Reached insert");
                self.length.fetch_add(1, Ordering::Relaxed);
                break;
            }
        }
    }

    pub fn delete_from_tail<'a>(&'a self, raw_descriptor: &RawDescriptor<'a, T>) -> Option<T> {
        let ret = raw_descriptor.delete(&self.head, &self.tail);
        if ret.is_some() {
            println!("Reached decrement subcount");
            self.length.fetch_sub(1, Ordering::Relaxed);
        }
        return ret;
    }

    pub fn length(&self) -> usize {
        self.length.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    #[test]
    fn test() {
        let new = &LinkedList::new();
        let raw = &RawDescriptor::new();
        std::thread::scope(|s| {
            for i in 0..5 {
                s.spawn(move || {
                    new.insert_from_head(i, &raw);
                });
            }
        });
        std::thread::scope(|s| {
            for _ in 0..5 {
                s.spawn(move || {
                    let _ = new.delete_from_tail(&raw);
                });
            }
        });
        let len = new.length();
        assert_eq!(0 as usize, len);
    }
}
