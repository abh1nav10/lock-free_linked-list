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

pub(crate) struct Descriptor<'a, T> {
    ptr: &'a AtomicPtr<T>,
    next: *mut T,
    // 0: CAS on the ptr needs to be done
    // 1: the ptr has already been updated, but the status field is yet to be updated
    status: AtomicUsize,
    pending: AtomicBool,
    creator: &'static dyn Deleter,
    // this field provides the caller with the freedom to specify what method
    // of the Mile trait which T implements will be called
    after: usize,
}

// The Mile trait provides the user with a way to perform some other operations
// on the type T which is bounded by this trait after the swap has happened.
// This is common in data structures like linked lists where nodes need to be updated
// and provides a good way to update other things if any in a fully concurrent lock free manner;
// For types that do not need such updates the default implementation of the trait
// can safely be used.
pub trait Mile {
    fn first(ptr1: *mut Self, ptr2: *mut Self) {
        return;
    }
    fn second(ptr1: *mut Self, ptr2: *mut Self) {
        return;
    }
}

pub(crate) struct RawDescriptor<'a, T> {
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
    pub fn new(ptr: &'a AtomicPtr<T>, next: *mut T, after: usize) -> Self {
        Self {
            ptr,
            next,
            status: AtomicUsize::new(0),
            pending: AtomicBool::new(true),
            creator: &DELETER1,
            after,
        }
    }
}

impl<'a, T> RawDescriptor<'a, T>
where
    T: Mile,
{
    pub fn new() -> Self {
        Self {
            descriptor: AtomicPtr::new(std::ptr::null_mut()),
        }
    }

    pub fn try_or_help(&self, ptr: &'a AtomicPtr<T>, next: *mut T, after: usize) {
        let new = Box::into_raw(Box::new(Descriptor::new(ptr, next, after)));
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
                        _ => unreachable!(),
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
                                    let ptr2 = unsafe { (*current).ptr.load(Ordering::Acquire) };
                                    if after == 1 {
                                        Mile::first(next, ptr2);
                                    } else {
                                        Mile::second(next, ptr2);
                                    }
                                    (&(*new).status).store(1, Ordering::Release);

                                    (&(*new).pending).store(false, Ordering::Release);
                                },
                                1 => unsafe {
                                    (&(*new).pending).store(false, Ordering::Release);
                                },
                                _ => unreachable!(),
                            }
                            break;
                        }

                        Err(_) => {
                            let b = unsafe { (*current.deref_mut()).next };
                            match (&(*current.deref_mut()).status).load(Ordering::Acquire) {
                                0 => {
                                    (&(*current.deref_mut()).ptr).store(b, Ordering::Release);
                                    let ptr2 = unsafe { (*current).ptr.load(Ordering::Acquire) };
                                    let after = unsafe { (*current).after };
                                    if after == 1 {
                                        Mile::first(next, ptr2);
                                    } else {
                                        Mile::second(next, ptr2);
                                    }
                                    (&(*current).status).store(1, Ordering::Release);
                                    (&(*current).pending).store(false, Ordering::Release);
                                }
                                1 => {
                                    (&(*current).pending).store(false, Ordering::Release);
                                }
                                _ => unreachable!(),
                            }
                        }
                    }
                }
            }
        }
    }
}
