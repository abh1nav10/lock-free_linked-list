#![allow(dead_code)]
#![allow(unused_must_use)]
#![allow(unused)]
use crate::HazPtrHolder;
use crate::Retired;
use std::sync::atomic::Ordering;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize};

pub struct Descriptor<T> {
    ptr: AtomicPtr<T>,
    next: *mut T,
    status: AtomicUsize,
    pending: AtomicBool,
}

pub struct RawDescriptor<T> {
    descriptor: AtomicPtr<Descriptor<T>>,
}

impl<T> Descriptor<T> {
    pub fn new(ptr: AtomicPtr<T>, next: *mut T) -> Self {
        Self {
            ptr,
            next,
            status: AtomicUsize::new(0),
            pending: AtomicBool::new(true),
        }
    }
}

impl<T> RawDescriptor<T> {
    pub fn new() -> Self {
        let descriptor = Box::into_raw(Box::new(Descriptor {
            ptr: AtomicPtr::new(std::ptr::null_mut()),
            next: std::ptr::null_mut(),
            status: AtomicUsize::new(1),
            pending: AtomicBool::new(false),
        }));
        Self {
            descriptor: AtomicPtr::new(descriptor),
        }
    }

    pub fn try_or_help(&self, ptr: AtomicPtr<T>, next: *mut T) {
        let new = Box::into_raw(Box::new(Descriptor::new(ptr, next)));
        loop {
            match unsafe {
                (*(&self.descriptor).load(Ordering::Relaxed)) // Hazptpr inclusion
                    .pending
                    .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            } {
                Ok(_) => {
                    self.descriptor.store(new, Ordering::Relaxed);
                    // Hazptr inclusion to be done...the old one will be retired here
                    match unsafe { (&(*new).status).load(Ordering::Relaxed) } {
                        0 => unsafe {
                            (&(*new).ptr).store(next, Ordering::Release);

                            (&(*new).status).compare_exchange(
                                0,
                                1,
                                Ordering::Relaxed,
                                Ordering::Relaxed,
                            );

                            (&(*new).pending).compare_exchange(
                                true,
                                false,
                                Ordering::Relaxed,
                                Ordering::Relaxed,
                            );
                        },
                        1 => unsafe {
                            (&(*new).pending).compare_exchange(
                                true,
                                false,
                                Ordering::Relaxed,
                                Ordering::Relaxed,
                            );
                        },
                        _ => panic!(),
                    }
                }

                Err(_) => {
                    let current = self.descriptor.load(Ordering::Acquire); //HazPtr to be used

                    unsafe {
                        match (&(*current).status).load(Ordering::Relaxed) {
                            0 => {
                                (&(*current).ptr).store(next, Ordering::Release);
                                // Hazptr to be used...old one will be retired here
                                (&(*current).status).compare_exchange(
                                    0,
                                    1,
                                    Ordering::Relaxed,
                                    Ordering::Relaxed,
                                );
                                (&(*current).pending).compare_exchange(
                                    true,
                                    false,
                                    Ordering::Relaxed,
                                    Ordering::Relaxed,
                                );
                            }
                            1 => {
                                (&(*current).pending).compare_exchange(
                                    true,
                                    false,
                                    Ordering::Relaxed,
                                    Ordering::Relaxed,
                                );
                            }
                            _ => panic!(),
                        }
                    }
                }
            }
        }
    }
}
