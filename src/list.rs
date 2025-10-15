#![allow(unused_imports)]
#![allow(dead_code)]
use crate::Descriptor;
use crate::HazPtrHolder;
use crate::Mile;
use crate::RawDescriptor;
use crate::Retired;
use std::marker::PhantomData;
use std::ops::DerefMut;
use std::sync::atomic::Ordering;
use std::sync::atomic::{AtomicPtr, AtomicUsize};

pub struct Node<T> {
    value: T,
    prev: AtomicPtr<Node<T>>,
    next: AtomicPtr<Node<T>>,
}

impl<T> Node<T> {
    fn new(value: T) -> Self {
        Self {
            value,
            prev: AtomicPtr::new(std::ptr::null_mut()),
            next: AtomicPtr::new(std::ptr::null_mut()),
        }
    }
}

impl<T> Mile for Node<T> {
    fn first(ptr1: *mut Self, ptr2: *mut Self) {
        unsafe {
            (*ptr1).next.store(ptr2, Ordering::Release);
            (*ptr2).prev.store(ptr1, Ordering::Release);
        }
    }
    fn second(ptr1: *mut Self, ptr2: *mut Self) {
        unsafe {
            (*ptr1).prev.store(ptr2, Ordering::Release);
            (*ptr2).next.store(ptr1, Ordering::Release);
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

    pub fn insert_from_head(&self, value: T) {
        let boxed = Box::into_raw(Box::new(Node::new(value)));
        let mut hazholder = HazPtrHolder::default();
        loop {
            let current = unsafe { hazholder.load(&self.head) };
            if current.is_none() {
                match self.head.compare_exchange_weak(
                    std::ptr::null_mut(),
                    boxed,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        let _ = self.tail.compare_exchange(
                            std::ptr::null_mut(),
                            boxed,
                            Ordering::AcqRel,
                            Ordering::Relaxed,
                        );
                        self.length.fetch_add(1, Ordering::Relaxed);
                        break;
                    }
                    Err(_) => {
                        continue;
                    }
                }
            } else {
                let mut guard = current.expect("Has to be there");
                // the try_or_help method needs to be called
            }
        }
    }
}
