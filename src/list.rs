#![allow(dead_code)]
use crate::Descriptor;
use crate::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize};
use std::marker::PhantomData;
use std::sync::atomic::Ordering;

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
    pub(crate) head: AtomicPtr<Node<T>>,
    pub(crate) tail: AtomicPtr<Node<T>>,
    pub(crate) descriptor: AtomicPtr<Descriptor<T>>,
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
            descriptor: AtomicPtr::new(std::ptr::null_mut()),
            marker: PhantomData,
        }
    }

    pub fn insert_from_head<'a>(&self, value: T) {
        let boxed = Box::into_raw(Box::new(Node::new(value)));
        loop {
            let current = self.head.load(Ordering::SeqCst);
            if current.is_null() {
                match self.head.compare_exchange(
                    std::ptr::null_mut(),
                    boxed,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => {
                        // We dont CAS the tail because we dont have a method to insert from the
                        // tail side.Therefore any possibility of some other thread inserting a
                        // tail after we swap the head but before we manage to store the tail does
                        // not  exist.
                        // Updating the tail is the only reason we have this loop in the first
                        // place otherwise the insert method has got all the capability to handle
                        // the case where head is an atomic pointer storing a null pointer.
                        self.tail.store(boxed, Ordering::SeqCst);
                        //println!("Reached insert");
                        self.length.fetch_add(1, Ordering::SeqCst);
                        break;
                    }
                    Err(_) => {
                        continue;
                    }
                }
            } else {
                self.insert(boxed);
                //println!("Reached insert");
                self.length.fetch_add(1, Ordering::SeqCst);
                break;
            }
        }
    }

    pub fn delete_from_tail<'a>(&self) -> Option<T> {
        let ret = self.delete();
        if ret.is_some() {
            //println!("Reached decrement subcount");
            self.length.fetch_sub(1, Ordering::SeqCst);
        }
        return ret;
    }

    pub fn length(&self) -> usize {
        self.length.load(Ordering::Relaxed)
    }
}
