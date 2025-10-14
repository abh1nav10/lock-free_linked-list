#![allow(dead_code)]
#![allow(unused_must_use)]
#![allow(unused)]
use crate::Deleter;
use crate::DropBox;
use crate::DropPointer;
use crate::HazPtrHolder;
use crate::HazPtrObject;
use crate::Retired;
use std::ops::DerefMut;
use std::sync::atomic::Ordering;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize};

static DELETER1: DropBox = DropBox::new();
static DELETER2: DropPointer = DropPointer::new();

pub struct Descriptor<'a, T> {
    ptr: &'a AtomicPtr<T>,
    next: *mut T,
    // 0: CAS on the ptr needs to be done
    // 1: the ptr has already been updated, but the status field is yet to be updated
    status: AtomicUsize,
    pending: AtomicBool,
    creator: &'static dyn Deleter,
}

pub struct RawDescriptor<'a, T> {
    descriptor: AtomicPtr<Descriptor<'a, T>>,
}

impl<'a, T> Drop for RawDescriptor<'a, T> {
    fn drop(&mut self) {
        let mut holder = HazPtrHolder::default();
        // not using hazptrs in this method will also work fine
        // as it is the last thing being called
        let raw = unsafe { holder.load(&self.descriptor) };
        if raw.is_none() {
            return;
        }
        let deleter = unsafe { (*(raw.expect("Has to be there").deref_mut())).creator };
        let mut wrapper = unsafe { holder.swap(&self.descriptor, std::ptr::null_mut(), deleter) };
        if let Some(mut wrapper) = wrapper {
            wrapper.retire();
        } else {
            return;
        }
    }
}

impl<'a, T> Descriptor<'a, T> {
    pub fn new(ptr: &'a AtomicPtr<T>, next: *mut T) -> Self {
        Self {
            ptr,
            next,
            status: AtomicUsize::new(0),
            pending: AtomicBool::new(true),
            creator: &DELETER1,
        }
    }
}

impl<'a, T> RawDescriptor<'a, T> {
    pub fn new() -> Self {
        Self {
            descriptor: AtomicPtr::new(std::ptr::null_mut()),
        }
    }

    pub fn try_or_help(&self, ptr: &'a AtomicPtr<T>, next: *mut T) {
        let new = Box::into_raw(Box::new(Descriptor::new(ptr, next)));
        let mut holder = HazPtrHolder::default();
        loop {
            let mut guard = unsafe { holder.load(&self.descriptor) };
            if guard.is_none() {
                if self
                    .descriptor
                    .compare_exchange(
                        std::ptr::null_mut(),
                        new,
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    match unsafe { (&(*new).status).load(Ordering::Acquire) } {
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
                                Ordering::AcqRel,
                                Ordering::Relaxed,
                            );
                        },
                        1 => unsafe {
                            (&(*new).pending).compare_exchange(
                                true,
                                false,
                                Ordering::AcqRel,
                                Ordering::Relaxed,
                            );
                        },
                        _ => panic!(),
                    }
                    break;
                } else {
                    match unsafe {
                        (*(guard.expect("Has to be there").deref_mut() as *mut Descriptor<T>))
                            .pending
                            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                    } {
                        Ok(_) => {
                            let deleter =
                                unsafe { (*(&self.descriptor).load(Ordering::Acquire)).creator };
                            let mut wrapper =
                                unsafe { holder.swap(&self.descriptor, new, deleter) };
                            if let Some(mut object) = wrapper {
                                object.retire();
                            }
                            match unsafe { (&(*new).status).load(Ordering::Acquire) } {
                                0 => unsafe {
                                    (&(*new).ptr).store(next, Ordering::Release);

                                    (&(*new).status).compare_exchange(
                                        0,
                                        1,
                                        Ordering::AcqRel,
                                        Ordering::Relaxed,
                                    );

                                    (&(*new).pending).compare_exchange(
                                        true,
                                        false,
                                        Ordering::AcqRel,
                                        Ordering::Relaxed,
                                    );
                                },
                                1 => unsafe {
                                    (&(*new).pending).compare_exchange(
                                        true,
                                        false,
                                        Ordering::AcqRel,
                                        Ordering::Relaxed,
                                    );
                                },
                                _ => panic!(),
                            }
                            break;
                        }

                        Err(_) => {
                            let current = self.descriptor.load(Ordering::Acquire); //HazPtr to be used

                            unsafe {
                                match (&(*current).status).load(Ordering::Acquire) {
                                    0 => {
                                        (&(*current).ptr).store(next, Ordering::Release);
                                        // Hazptr to be used...old one will be retired here
                                        (&(*current).status).compare_exchange(
                                            0,
                                            1,
                                            Ordering::AcqRel,
                                            Ordering::Relaxed,
                                        );
                                        (&(*current).pending).compare_exchange(
                                            true,
                                            false,
                                            Ordering::AcqRel,
                                            Ordering::Relaxed,
                                        );
                                    }
                                    1 => {
                                        (&(*current).pending).compare_exchange(
                                            true,
                                            false,
                                            Ordering::AcqRel,
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
    }
}
