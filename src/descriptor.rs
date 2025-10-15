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

pub trait Completer {
    fn first<F>(&self, ptr: *mut F);
    fn second<F>(&self, ptr: *mut F);
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

    pub fn try_or_help<F>(&self, ptr: &'a AtomicPtr<T>, next: *mut T, ds: F)
    where
        F: Completer,
    {
        let new = Box::into_raw(Box::new(Descriptor::new(ptr, next)));
        let mut holder = HazPtrHolder::default();
        loop {
            ///SAFETY:
            ///    Caller must ensure that the list does not get dropped before the call to
            ///    this method completes
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
                            (&(*new).status).store(1, Ordering::Release);
                            (&(*new).pending).store(false, Ordering::Release);
                        },
                        1 => unsafe {
                            (&(*new).pending).store(false, Ordering::Release);
                        },
                        _ => panic!("Status field not initialized correctly"),
                    }
                    break;
                } else {
                    let mut current = guard.expect("Has to be there");
                    match unsafe {
                        (*(current.deref_mut())).pending.compare_exchange(
                            false,
                            true,
                            Ordering::AcqRel,
                            Ordering::Relaxed,
                        )
                    } {
                        Ok(_) => {
                            let deleter =
                                unsafe { (*(&self.descriptor).load(Ordering::Acquire)).creator };
                            let mut temp_holder = HazPtrHolder::default();
                            let mut wrapper =
                                unsafe { temp_holder.swap(&self.descriptor, new, deleter) };
                            if let Some(mut object) = wrapper {
                                object.retire();
                            }
                            match unsafe { (&(*new).status).load(Ordering::Acquire) } {
                                0 => unsafe {
                                    (&(*new).ptr).store(next, Ordering::Release);
                                    (&(*new).status).store(1, Ordering::Release);

                                    (&(*new).pending).store(false, Ordering::Release);
                                },
                                1 => unsafe {
                                    (&(*new).pending).store(false, Ordering::Release);
                                },
                                _ => panic!("Status field not initialized correctly"),
                            }
                            break;
                        }

                        Err(_) => {
                            let b = unsafe { (*current.deref_mut()).next };
                            match (&(*current.deref_mut()).status).load(Ordering::Acquire) {
                                0 => {
                                    (&(*current.deref_mut()).ptr).store(b, Ordering::Release);
                                    (&(*current).status).store(1, Ordering::Release);
                                    (&(*current).pending).store(false, Ordering::Release);
                                }
                                1 => {
                                    (&(*current).pending).store(false, Ordering::Release);
                                }
                                _ => panic!("Status field not initialized correctly"),
                            }
                        }
                    }
                }
            }
        }
    }
}
