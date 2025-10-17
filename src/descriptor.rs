#![allow(dead_code)]
#![allow(unused_must_use)]
#![allow(unused)]
use crate::Node;
use crate::{Deleter, DropBox, DropPointer, HazPtrHolder, HazPtrObject, Retired};
use std::ops::DerefMut;
use std::sync::atomic::Ordering;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize};

static DELETER1: DropBox = DropBox::new();
static DELETER2: DropPointer = DropPointer::new();

#[derive(Copy, Clone)]
pub(crate) enum Operation {
    Insert,
    Delete,
}

// Status field helped other helper threads to help efficiently by looking at how much
// of the task has been completed and the pending field was introduced to keep a broad eye
// on whether the entire task has been completed. It was there for other threads to try
// to CAS from false to true thereby disallowing other threads from taking a position
// in the raw descriptor. However, I found out that doing so leaves a vulnerability.
// There might be other helper threads who in the process of helping try to store
// false again into the pending field which would allow other threads to swap the
// descriptor field in the raw dexcriptor again. Thus we have to prevent this by using
// AtomicUsize here as well.
pub(crate) struct Descriptor<'a, T> {
    ptr: &'a AtomicPtr<Node<T>>,
    next: *mut Node<T>,
    status: AtomicUsize,
    pending: AtomicBool,
    op: Operation,
    deleter: &'static dyn Deleter,
}

// The linked list based FIFO queue will have two raw descriptors, one for insertion through head
// and one for deletion through tail. No other raw descriptors will be created as that would
// violate the safety requirements. It most likely will corrupt our list and in many ways can
// cause undefined behaviour.
pub(crate) struct RawDescriptor<'a, T> {
    descriptor: AtomicPtr<Descriptor<'a, T>>,
}

impl<'a, T> Drop for RawDescriptor<'a, T> {
    fn drop(&mut self) {
        let mut holder = HazPtrHolder::default();
        let mut guard = unsafe { holder.load(&self.descriptor) };
        if let Some(ref mut thing) = guard {
            let deleter = thing.deref_mut().deleter;
            std::mem::drop(guard);
            let mut new_holder = HazPtrHolder::default();
            let wrapper =
                unsafe { new_holder.swap(&self.descriptor, std::ptr::null_mut(), deleter) };
            if let Some(mut wrapper) = wrapper {
                wrapper.retire();
            }
        } else {
            return;
        }
    }
}

impl<'a, T> Descriptor<'a, T> {
    fn new(
        ptr: &'a AtomicPtr<Node<T>>,
        next: *mut Node<T>,
        status: AtomicUsize,
        pending: AtomicBool,
        op: Operation,
        deleter: &'static dyn Deleter,
    ) -> Self {
        Self {
            ptr,
            next,
            status,
            pending,
            op,
            deleter,
        }
    }
}

impl<'a, T> RawDescriptor<'a, T> {
    pub fn new() -> Self {
        Self {
            descriptor: AtomicPtr::new(std::ptr::null_mut()),
        }
    }
}

impl<'a, T> RawDescriptor<'a, T> {
    pub fn insert(
        &self,
        ptr: &'a AtomicPtr<Node<T>>,
        next: *mut Node<T>,
        deleter: &'static dyn Deleter,
    ) {
        loop {
            let new_descriptor: *mut Descriptor<'a, T> = Box::into_raw(Box::new(Descriptor::new(
                ptr,
                next,
                AtomicUsize::new(0),
                AtomicBool::new(true),
                Operation::Insert,
                deleter,
            )));
            let status = unsafe { &(*new_descriptor).status };
            let pending = unsafe { &(*new_descriptor).pending };
            if self.descriptor.load(Ordering::Acquire).is_null() {
                if self
                    .swap_null_insert(new_descriptor, status, ptr, pending, next)
                    .is_ok()
                {
                    break;
                }
            } else {
                // The idea is to first load the current descriptor into a hazard pointer if we do
                // not get back a guard we just simply loop back. If we get back a guard we try to
                // get to the pending field of the descriptor and check whether it is true or
                // false, if not false then we just help. If it is false we try to CAS the descriptor
                // with our new descriptor, if successful we proceed with our operation, otherwise
                // we just help.
                let mut pending_holder = HazPtrHolder::default();
                let mut pending_holder_guard = unsafe { pending_holder.load(&self.descriptor) };
                if let Some(mut thing) = pending_holder_guard {
                    if !thing.deref_mut().pending.load(Ordering::Acquire) {
                        if self
                            .descriptor
                            .compare_exchange(
                                thing.deref_mut() as *mut Descriptor<'a, T>,
                                new_descriptor,
                                Ordering::AcqRel,
                                Ordering::Relaxed,
                            )
                            .is_ok()
                        {
                            let raw = thing.deref_mut() as *mut Descriptor<'a, T>;
                            let mut swapholder = HazPtrHolder::default();
                            let atomic_ptr_descriptor: AtomicPtr<Descriptor<'a, T>> =
                                AtomicPtr::new(raw);
                            let mut wrapper = unsafe {
                                swapholder.swap(
                                    &atomic_ptr_descriptor,
                                    std::ptr::null_mut(),
                                    ((*raw).deleter),
                                )
                            };
                            if let Some(mut wrapper) = wrapper {
                                wrapper.retire();
                            }
                            Self::recursive_insert(
                                status,
                                ptr,
                                pending,
                                next,
                                ptr.load(Ordering::Acquire),
                            );
                            break;
                        } else {
                            let drop = unsafe { Box::from_raw(new_descriptor) };
                            std::mem::drop(drop);
                            self.help(
                                thing.deref_mut().op,
                                thing.deref_mut() as *mut Descriptor<'a, T>,
                            );
                        }
                    } else {
                        let drop = unsafe { Box::from_raw(new_descriptor) };
                        std::mem::drop(drop);
                        self.help(
                            thing.deref_mut().op,
                            thing.deref_mut() as *mut Descriptor<'a, T>,
                        );
                    }
                }
            }
        }
    }

    // The swap_null function is called when we find that the pointer to the descriptor is null.
    // The function tries to compare and exchange expecting a null pointer which if succeeds we
    // initiate the recurive call and if fails we move forward to see if there is anyone we can
    // help before looping back.
    fn swap_null_insert(
        &self,
        new_descriptor: *mut Descriptor<'a, T>,
        status: &'_ AtomicUsize,
        ptr: &'_ AtomicPtr<Node<T>>,
        pending: &'_ AtomicBool,
        next: *mut Node<T>,
    ) -> Result<(), ()> {
        if self
            .descriptor
            .compare_exchange(
                std::ptr::null_mut(),
                new_descriptor,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            let mut holder = HazPtrHolder::default();
            let atomic_ptr = AtomicPtr::new(next);
            let mut guard = unsafe { holder.load(&atomic_ptr) };
            let forward = if let Some(mut ptr) = guard {
                ptr.deref_mut() as *mut Node<T>
            } else {
                std::ptr::null_mut()
            };
            Self::recursive_insert(status, ptr, pending, forward, std::ptr::null_mut());
            Ok(())
        } else {
            Err(())
        }
    }

    fn help(&self, op: Operation, raw: *mut Descriptor<'a, T>) {
        let mut node_holder = HazPtrHolder::default();
        let mut node_guard = unsafe { node_holder.load(&(*raw).ptr) };
        let mut next_node_holder = HazPtrHolder::default();
        // We need this atomic pointer solely for guarding the next with a hazard pointer,
        // this is done because the next field inside the descriptor struct is suppossed to
        // be a raw pointer to Node<T> but the load method of HazPtrHolder requires an
        // atomic pointer.
        let next_atomic_ptr = unsafe { AtomicPtr::new((*raw).next) };
        let mut next_node_guard = unsafe { next_node_holder.load(&next_atomic_ptr) };
        let current_node = if let Some(mut thing) = next_node_guard {
            thing.deref_mut() as *mut Node<T>
        } else {
            std::ptr::null_mut()
        };

        let status = unsafe { &(*raw).status };
        let pending = unsafe { &(*raw).pending };
        let next = unsafe { (*raw).next };
        let pointer = unsafe { &(*raw).ptr };
        match op {
            Operation::Insert => {
                Self::recursive_insert(status, pointer, pending, next, current_node);
            }
            Operation::Delete => {
                Self::recursive_delete(status, pointer, pending, current_node);
            }
        }
    }

    /// The recursive function allows threads to see how much of the task of a given descriptor is
    /// completed and help accordingly.
    /// Stack overflow won't be an issue because the steps are bounded by a fixed maxima.
    fn recursive_insert(
        status: &'_ AtomicUsize,
        ptr: &'_ AtomicPtr<Node<T>>,
        pending: &'_ AtomicBool,
        next: *mut Node<T>,
        current_node: *mut Node<T>,
    ) {
        match pending.load(Ordering::Acquire) {
            true => match status.load(Ordering::Acquire) {
                0 => {
                    ptr.compare_exchange(current_node, next, Ordering::AcqRel, Ordering::Relaxed);
                    // Using store instead of CAS can corrupt the process, assume that the status
                    // has already reached to 2 but there is still a possibility of it being
                    // 1 by other threads who were in the process of helping.
                    status.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Relaxed);
                    Self::recursive_insert(status, ptr, pending, next, current_node);
                }
                1 => {
                    Self::insert_head(next, current_node);
                    status.compare_exchange(1, 2, Ordering::AcqRel, Ordering::Relaxed);
                    Self::recursive_insert(status, ptr, pending, next, current_node);
                }
                2 => {
                    // instead of doing this maybe i will have to swap the descriptor pointer
                    pending.store(false, Ordering::Release);
                }
                _ => unreachable!(),
            },
            false => return,
        }
    }

    // Updates the fields in accordance with head insertion.
    fn insert_head(new: *mut Node<T>, old: *mut Node<T>) {
        // Only checking if old is null does not suffice when deletion from tail is allowed. It
        // needs be stored into a hazard pointer to prevent undefined behaviour because deletion
        // might have removed that node after our is_null check but before dereferencing it. Hazard
        // pointers do allow deletion but at the same time prevent undefined behaviour.
        let mut holder = HazPtrHolder::default();
        // Atomic pointer is needed because the load method on the HazPtrHolder accepts a reference
        // to an atomic pointer as the input.
        let atomic_ptr = AtomicPtr::new(old);
        let mut guard = unsafe { holder.load(&atomic_ptr) };
        if guard.is_some() {
            unsafe {
                (&(*old).prev).store(new, Ordering::Release);
            }
        }
    }

    pub fn delete(&self, ptr: &'a AtomicPtr<Node<T>>, deleter: &'static dyn Deleter) -> Option<T> {
        loop {
            let new = Box::into_raw(Box::new(Descriptor::new(
                ptr,
                std::ptr::null_mut(),
                AtomicUsize::new(0),
                AtomicBool::new(true),
                Operation::Delete,
                deleter,
            )));
            let status = unsafe { &(*new).status };
            let pending = unsafe { &(*new).pending };
            if ptr.load(Ordering::Acquire).is_null() {
                return None;
            }

            let mut descriptor_holder = HazPtrHolder::default();
            if self.descriptor.load(Ordering::Acquire).is_null() {
                let mut holder = HazPtrHolder::default();
                let guard = unsafe { holder.load(ptr) };
                let current_node = if let Some(mut thing) = guard {
                    thing.deref_mut() as *mut Node<T>
                } else {
                    std::ptr::null_mut()
                };
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
                    // take ownership of the T inside the node
                    let value = if !current_node.is_null() {
                        Some(unsafe { std::ptr::read(&(*current_node).value) })
                    } else {
                        None
                    };
                    Self::recursive_delete(status, ptr, pending, current_node);
                    break None;
                }
            } else {
                let mut descriptor_holder = HazPtrHolder::default();
                let mut descriptor_guard = unsafe { descriptor_holder.load(&self.descriptor) };
                if let Some(mut thing) = descriptor_guard {
                    if !thing.deref_mut().pending.load(Ordering::Acquire) {
                        let current_raw = thing.deref_mut() as *mut Descriptor<T>;
                        let mut aholder = HazPtrHolder::default();
                        let mut aguard = unsafe { aholder.load(ptr) };
                        let forw = if let Some(mut guard) = aguard {
                            guard.deref_mut() as *mut Node<T>
                        } else {
                            std::ptr::null_mut()
                        };
                        if self
                            .descriptor
                            .compare_exchange(current_raw, new, Ordering::AcqRel, Ordering::Relaxed)
                            .is_ok()
                        {
                            let mut holder = HazPtrHolder::default();
                            let atomic_ptr =
                                AtomicPtr::new(thing.deref_mut() as *mut Descriptor<'a, T>);
                            let mut wrapper = unsafe {
                                holder.swap(&atomic_ptr, std::ptr::null_mut(), &DELETER1)
                            };
                            if let Some(mut wrapper) = wrapper {
                                wrapper.retire();
                            }
                            // take ownership of T in the node
                            let value = if !forw.is_null() {
                                Some(unsafe { std::ptr::read(&(*forw).value) })
                            } else {
                                None
                            };
                            Self::recursive_delete(status, ptr, pending, forw);
                            break value;
                        } else {
                            let drop = unsafe { Box::from_raw(new) };
                            std::mem::drop(drop);
                            self.help(
                                thing.deref_mut().op,
                                thing.deref_mut() as *mut Descriptor<'a, T>,
                            );
                        }
                    } else {
                        let drop = unsafe { Box::from_raw(new) };
                        std::mem::drop(drop);
                        self.help(
                            thing.deref_mut().op,
                            thing.deref_mut() as *mut Descriptor<'a, T>,
                        );
                    }
                }
            }
        }
    }

    fn recursive_delete(
        status: &'_ AtomicUsize,
        ptr: &'_ AtomicPtr<Node<T>>,
        pending: &'_ AtomicBool,
        current_node: *mut Node<T>,
    ) {
        match pending.load(Ordering::Acquire) {
            true => match status.load(Ordering::Acquire) {
                0 => {
                    if current_node.is_null() {
                        return;
                    }
                    let prev = unsafe { (*current_node).prev.load(Ordering::Acquire) };
                    ptr.compare_exchange(current_node, prev, Ordering::AcqRel, Ordering::Relaxed);
                    status.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Relaxed);
                    Self::recursive_delete(status, ptr, pending, current_node);
                }
                1 => {
                    if unsafe {
                        (*current_node)
                            .retired
                            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                            .is_ok()
                    } {
                        let mut hazholder = HazPtrHolder::default();
                        let atomic_ptr = AtomicPtr::new(current_node);
                        let mut wrapper =
                            unsafe { hazholder.swap(&atomic_ptr, std::ptr::null_mut(), &DELETER1) };
                        if let Some(mut wrapper) = wrapper {
                            wrapper.retire();
                        }
                    }
                    status.compare_exchange(1, 2, Ordering::AcqRel, Ordering::Relaxed);
                    Self::recursive_delete(status, ptr, pending, current_node);
                }
                2 => {
                    pending.store(false, Ordering::Release);
                }
                _ => unreachable!(),
            },
            false => return,
        }
    }
}
