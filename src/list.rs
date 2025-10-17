#![allow(dead_code)]
use crate::hazard::{DropBox, DropPointer};
use crate::{Operation, RawDescriptor};
use std::marker::PhantomData;
use std::sync::atomic::Ordering;
use std::sync::atomic::{AtomicPtr, AtomicUsize};

static BOXDELETER: DropBox = DropBox::new();
static PTRDELETER: DropPointer = DropPointer::new();

pub struct Node<T> {
    pub(crate) value: T,
    pub(crate) prev: AtomicPtr<Node<T>>,
}

impl<T> Node<T> {
    fn new(value: T) -> Self {
        Self {
            value,
            prev: AtomicPtr::new(std::ptr::null_mut()),
        }
    }
}

pub struct LinkedList<'a, T> {
    length: AtomicUsize,
    head: AtomicPtr<Node<T>>,
    tail: AtomicPtr<Node<T>>,
    head_descriptor: RawDescriptor<'a, T>,
    // 'a here is basically the lifetime of the head and tail which is in simple words the linked
    // list itself. Its like saying that the linked list is valid only for as long as the linked
    // list is valid.
    tail_descriptor: RawDescriptor<'a, T>,
    marker: PhantomData<Node<T>>,
}

unsafe impl<'a, T> Send for LinkedList<'a, T> where T: Send {}
unsafe impl<'a, T> Sync for LinkedList<'a, T> where T: Sync {}

impl<'a, T> LinkedList<'a, T> {
    pub fn new() -> Self {
        let raw_one = RawDescriptor::new();
        let raw_two = RawDescriptor::new();
        Self {
            length: AtomicUsize::new(0),
            head: AtomicPtr::new(std::ptr::null_mut()),
            tail: AtomicPtr::new(std::ptr::null_mut()),
            head_descriptor: raw_one,
            tail_descriptor: raw_two,
            marker: PhantomData,
        }
    }

    pub fn insert_from_head(&'a self, value: T) {
        let boxed = Box::into_raw(Box::new(Node::new(value)));
        // we don't need to store the head into a hazard pointer because it is already loaded into
        // one when necessary in the insert_head method in descriptor.rs. Therefore we can directly
        // pass a reference to the self.head into the initiate method. If we see that the head
        // points to null then we can try to CAS directly, else we will have to call the initialize
        // method.
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
                (&self.head_descriptor).insert(
                    &self.head,
                    boxed,
                    Operation::InsertHead,
                    &BOXDELETER,
                );
                break;
            }
        }
    }

    pub fn delete_from_tail(&'a self) -> Option<T> {
        let mut next = unsafe {
            (*self.tail.load(Ordering::Acquire))
                .prev
                .load(Ordering::Acquire)
        };
        loop {
            if next.is_null() {
                return None;
            } else {
                self.tail_descriptor
                    .insert(&self.tail, next, Operation::DeleteTail, &BOXDELETER);
            }
        }
    }
}
