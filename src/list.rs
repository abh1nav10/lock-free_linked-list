#![allow(dead_code)]
use crate::RawDescriptor;
use std::marker::PhantomData;
use std::sync::atomic::Ordering;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize};

pub(crate) struct Node<T> {
    pub(crate) value: T,
    pub(crate) prev: AtomicPtr<Node<T>>,
    pub(crate) retired: AtomicBool,
}

impl<T> Node<T> {
    fn new(value: T) -> Self {
        Self {
            value,
            prev: AtomicPtr::new(std::ptr::null_mut()),
            // this field is to prevent that retirement of the same node more than once
            retired: AtomicBool::new(false),
        }
    }
}

pub struct LinkedList<'a, T> {
    length: AtomicUsize,
    head: AtomicPtr<Node<T>>,
    tail: AtomicPtr<Node<T>>,
    raw_descriptor: RawDescriptor<'a, T>,
    // 'a here is basically the lifetime of the head and tail which is in simple words the linked
    // list itself. Its like saying that the linked list is valid only for as long as the linked
    // list is valid.
    marker: PhantomData<Node<T>>,
}

unsafe impl<'a, T> Send for LinkedList<'a, T> where T: Send {}
unsafe impl<'a, T> Sync for LinkedList<'a, T> where T: Sync {}

impl<'a, T> LinkedList<'a, T> {
    pub fn new() -> Self {
        let raw_one = RawDescriptor::new();
        Self {
            length: AtomicUsize::new(0),
            head: AtomicPtr::new(std::ptr::null_mut()),
            tail: AtomicPtr::new(std::ptr::null_mut()),
            raw_descriptor: raw_one,
            marker: PhantomData,
        }
    }

    pub fn insert_from_head(&'a self, value: T) {
        let boxed = Box::into_raw(Box::new(Node::new(value)));
        loop {
            let current = self.head.load(Ordering::Acquire);
            if current.is_null() {
                match self.head.compare_exchange_weak(
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
                        self.length.fetch_add(1, Ordering::Relaxed);
                        break;
                    }
                    Err(_) => {
                        continue;
                    }
                }
            } else {
                (&self.raw_descriptor).insert(&self.head, boxed);
                break;
            }
        }
    }

    pub fn delete_from_tail(&'a self) -> Option<T> {
        self.raw_descriptor.delete(&self.tail)
    }

    pub fn length(&self) -> usize {
        self.length.load(Ordering::Relaxed)
    }
}
