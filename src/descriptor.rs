#![allow(dead_code)]
#![allow(unused_must_use)]
#![allow(unused)]
use crate::Node;
use crate::{Deleter, DropBox, DropPointer, HazPtrHolder, HazPtrObject};
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
    tail_ptr: &'a AtomicPtr<Node<T>>,
    next: *mut Node<T>,
    status: AtomicUsize,
    pending: AtomicBool,
    op: Operation,
    deleter: &'static dyn Deleter,
    retired: AtomicBool,
    taken_value: Option<T>,
}

unsafe impl<'a, T> Send for Descriptor<'a, T> where T: Send {}
unsafe impl<'a, T> Sync for Descriptor<'a, T> where T: Send {}

// The linked list based FIFO queue will have two raw descriptors, one for insertion through head
// and one for deletion through tail. No other raw descriptors will be created as that would
// violate the safety requirements. It most likely will corrupt our list and in many ways can
// cause undefined behaviour.
pub(crate) struct RawDescriptor<'a, T> {
    descriptor: AtomicPtr<Descriptor<'a, T>>,
}

// not required as it is auto implemented but i am doing it for clarity purposes
unsafe impl<'a, T> Send for RawDescriptor<'a, T> where T: Send {}
unsafe impl<'a, T> Sync for RawDescriptor<'a, T> where T: Sync {}

impl<'a, T> Drop for RawDescriptor<'a, T> {
    fn drop(&mut self) {
        let mut holder = HazPtrHolder::default();
        let mut guard = unsafe { holder.load(&self.descriptor) };
        if let Some(ref mut thing) = guard {
            let deleter = unsafe { (*thing.data).deleter };
            std::mem::drop(guard);
            let mut new_holder = HazPtrHolder::default();
            let wrapper =
                unsafe { new_holder.swap(&self.descriptor, std::ptr::null_mut(), deleter) };
            if let Some(mut wrapper) = wrapper {
                if unsafe {
                    (&(*wrapper.inner).retired).compare_exchange(
                        false,
                        true,
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    )
                }
                .is_ok()
                {
                    wrapper.retire();
                }
            }
        } else {
            return;
        }
    }
}

impl<'a, T> Descriptor<'a, T> {
    fn new(
        ptr: &'a AtomicPtr<Node<T>>,
        tail_ptr: &'a AtomicPtr<Node<T>>,
        next: *mut Node<T>,
        status: AtomicUsize,
        pending: AtomicBool,
        op: Operation,
        deleter: &'static dyn Deleter,
        retired: AtomicBool,
        taken_value: Option<T>,
    ) -> Self {
        Self {
            ptr,
            tail_ptr,
            next,
            status,
            pending,
            op,
            deleter,
            retired,
            taken_value,
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
        ptr_tail: &'a AtomicPtr<Node<T>>,
        next: *mut Node<T>,
    ) {
        loop {
            let new_descriptor: *mut Descriptor<'a, T> = Box::into_raw(Box::new(Descriptor::new(
                ptr,
                ptr_tail,
                next,
                AtomicUsize::new(0),
                AtomicBool::new(true),
                Operation::Insert,
                &DELETER1,
                AtomicBool::new(false),
                None,
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
                let mut new_guard = HazPtrHolder::default();
                let new_atm_ptr = AtomicPtr::new(new_descriptor);
                let mut new_desc_guard =
                    unsafe { new_guard.load(&new_atm_ptr).expect("Has to be there") };
                let mut pending_holder = HazPtrHolder::default();
                let mut pending_holder_guard = unsafe { pending_holder.load(&self.descriptor) };
                let mut new_holder = HazPtrHolder::default();
                let new_ptr = AtomicPtr::new(new_descriptor);
                let mut new_guard = unsafe { new_holder.load(&new_ptr).expect("Has to be there") };
                if let Some(mut thing) = pending_holder_guard {
                    if !unsafe { (*thing.data).pending.load(Ordering::Acquire) } {
                        if self
                            .descriptor
                            .compare_exchange(
                                thing.data,
                                new_descriptor,
                                Ordering::AcqRel,
                                Ordering::Relaxed,
                            )
                            .is_ok()
                        {
                            let raw = thing.data;
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
                                // this code path ensures that the descriptor is retired only once
                                // but double retirement can arise due to the drop implementation
                                // therefore we introduce the retired field in descriptor...
                                // calling the CAS in this code path seems redundant as the code
                                // path already guarantees safety but this has to be done to ensure
                                // that the retired field is updated to prevent the drop
                                // implementation from double retiring
                                if unsafe {
                                    (*wrapper.inner)
                                        .retired
                                        .compare_exchange(
                                            false,
                                            true,
                                            Ordering::AcqRel,
                                            Ordering::Relaxed,
                                        )
                                        .is_ok()
                                } {
                                    wrapper.retire();
                                }
                            }
                            Self::recursive_insert(
                                status,
                                ptr,
                                pending,
                                next,
                                ptr.load(Ordering::Acquire),
                            );
                            std::mem::drop(thing);
                            std::mem::drop(new_guard);
                            std::mem::drop(new_desc_guard);
                            HazPtrHolder::try_reclaim();
                            break;
                        } else {
                            let drop = unsafe { Box::from_raw(new_descriptor) };
                            std::mem::drop(drop);
                            self.help(unsafe { (*thing.data).op }, thing.data);
                            /*std::mem::drop(thing);
                            std::mem::drop(new_guard);*/
                            HazPtrHolder::try_reclaim();
                        }
                    } else {
                        let drop = unsafe { Box::from_raw(new_descriptor) };
                        std::mem::drop(drop);
                        self.help(unsafe { (*thing.data).op }, thing.data);
                        /*std::mem::drop(thing);
                        std::mem::drop(new_guard);*/
                        HazPtrHolder::try_reclaim();
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
        let mut desc_holder = HazPtrHolder::default();
        let desc_atomic_ptr = AtomicPtr::new(new_descriptor);
        let desc_guard = unsafe { desc_holder.load(&desc_atomic_ptr) };
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
            let forward = if let Some(ref mut ptr) = guard {
                ptr.data
            } else {
                std::ptr::null_mut()
            };
            Self::recursive_insert(status, ptr, pending, forward, std::ptr::null_mut());
            std::mem::drop(desc_guard.expect("Has to be there"));
            if guard.is_some() {
                std::mem::drop(guard);
            }
            HazPtrHolder::try_reclaim();
            Ok(())
        } else {
            Err(())
        }
    }

    fn help(&self, op: Operation, raw: *mut Descriptor<'a, T>) {
        match op {
            Operation::Insert => {
                let mut node_holder = HazPtrHolder::default();
                let mut node_guard = unsafe { node_holder.load(&(*raw).ptr) }; //flaw
                let mut next_node_holder = HazPtrHolder::default();
                // We need this atomic pointer solely for guarding the next with a hazard pointer,
                // this is done because the next field inside the descriptor struct is suppossed to
                // be a raw pointer to Node<T> but the load method of HazPtrHolder requires an
                // atomic pointer.
                let next_atomic_ptr = unsafe { AtomicPtr::new((*raw).next) };
                let mut next_node_guard = unsafe {
                    next_node_holder
                        .load(&next_atomic_ptr)
                        .expect("Has to be there")
                };
                let next = next_node_guard.data;

                let current_node = if let Some(mut thing) = node_guard {
                    thing.data
                } else {
                    std::ptr::null_mut()
                }; // look for the recursive insert method that you dont dereference it

                let status = unsafe { &(*raw).status };
                let pending = unsafe { &(*raw).pending };
                let pointer = unsafe { &(*raw).ptr };
                Self::recursive_insert(status, pointer, pending, next, current_node);
            }
            Operation::Delete => {
                let mut node_holder = HazPtrHolder::default();
                let mut node_guard = unsafe { node_holder.load(&(*raw).tail_ptr) }; //flaw
                let mut next_node_holder = HazPtrHolder::default();

                let current_node = if let Some(mut thing) = node_guard {
                    thing.data
                } else {
                    std::ptr::null_mut()
                };

                let status = unsafe { &(*raw).status };
                let pending = unsafe { &(*raw).pending };
                let head_ptr = unsafe { &(*raw).ptr }; // We dont load the tail pointer into a hazptrholder because it really doesnt matter
                let tail_pointer = unsafe { &(*raw).tail_ptr };

                Self::recursive_delete(raw, status, head_ptr, tail_pointer, pending, current_node);
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
                    Self::insert_head(next, current_node);
                    // Using store instead of CAS can corrupt the process, assume that the status
                    // has already reached to 2 but there is still a possibility of it being
                    // 1 by other threads who were in the process of helping.
                    status.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Relaxed);
                    Self::recursive_insert(status, ptr, pending, next, current_node);
                }
                1 => {
                    // instead of doing this maybe i will have to swap the descriptor pointer
                    status.store(2, Ordering::Relaxed);
                    ptr.compare_exchange(current_node, next, Ordering::AcqRel, Ordering::Relaxed);
                    pending.store(false, Ordering::Relaxed);
                }
                2 => {
                    Self::recursive_insert(status, ptr, pending, next, current_node);
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
        //let mut holder = HazPtrHolder::default();
        // Atomic pointer is needed because the load method on the HazPtrHolder accepts a reference
        // to an atomic pointer as the input.
        // let atomic_ptr = AtomicPtr::new(old);
        //let mut guard = unsafe { holder.load(&atomic_ptr) };

        unsafe {
            if old.is_null() {
                return;
            } else {
                (&(*old).prev).store(new, Ordering::Release);
            }
        }
    }

    pub fn delete(
        &self,
        ptr: &'a AtomicPtr<Node<T>>,
        tail_ptr: &'a AtomicPtr<Node<T>>,
    ) -> Option<T> {
        loop {
            let new = Box::into_raw(Box::new(Descriptor::new(
                ptr,
                tail_ptr,
                std::ptr::null_mut(),
                AtomicUsize::new(0),
                AtomicBool::new(true),
                Operation::Delete,
                &DELETER1,
                AtomicBool::new(false),
                None,
            )));
            let status = unsafe { &(*new).status };
            let pending = unsafe { &(*new).pending };

            let mut descriptor_holder = HazPtrHolder::default();
            if self.descriptor.load(Ordering::Acquire).is_null() {
                let mut holder = HazPtrHolder::default();
                let mut guard = unsafe { holder.load(ptr) };
                let mut desc_holder = HazPtrHolder::default();
                let mut desc_ptr = AtomicPtr::new(new);
                let mut a_guard = unsafe { desc_holder.load(&desc_ptr).expect("Has to be there") };
                let current_node = if let Some(ref mut thing) = guard {
                    thing.data
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
                    Self::recursive_delete(
                        a_guard.data,
                        status,
                        ptr,
                        tail_ptr,
                        pending,
                        current_node,
                    );
                    let taken_value = unsafe { (*a_guard.data).taken_value.take() };
                    std::mem::drop(a_guard);
                    if guard.is_some() {
                        std::mem::drop(guard);
                    }
                    HazPtrHolder::try_reclaim();
                    break taken_value;
                }
            } else {
                let mut descriptor_holder = HazPtrHolder::default();
                let mut descriptor_guard = unsafe { descriptor_holder.load(&self.descriptor) };
                if let Some(mut thing) = descriptor_guard {
                    let mut new_holder = HazPtrHolder::default();
                    let new_ptr = AtomicPtr::new(new);
                    let mut new_guard =
                        unsafe { new_holder.load(&new_ptr).expect("Has to be there") };
                    let current_raw = thing.data;
                    let mut aholder = HazPtrHolder::default();
                    let mut aguard = unsafe { aholder.load(ptr) };
                    if unsafe { !(*thing.data).pending.load(Ordering::Acquire) } {
                        if self
                            .descriptor
                            .compare_exchange(current_raw, new, Ordering::AcqRel, Ordering::Relaxed)
                            .is_ok()
                        {
                            let forw = if let Some(ref mut guard) = aguard {
                                guard.data
                            } else {
                                std::ptr::null_mut()
                            };
                            let mut holder = HazPtrHolder::default();
                            let atomic_ptr = AtomicPtr::new(thing.data);
                            let mut wrapper = unsafe {
                                holder.swap(&atomic_ptr, std::ptr::null_mut(), &DELETER1)
                            };
                            if let Some(mut wrapper) = wrapper {
                                if unsafe {
                                    (*wrapper.inner)
                                        .retired
                                        .compare_exchange(
                                            false,
                                            true,
                                            Ordering::AcqRel,
                                            Ordering::Relaxed,
                                        )
                                        .is_ok()
                                } {
                                    wrapper.retire();
                                }
                            }
                            Self::recursive_delete(
                                new_guard.data,
                                status,
                                ptr,
                                tail_ptr,
                                pending,
                                forw,
                            );
                            let taken_value = unsafe { (*new_guard.data).taken_value.take() };
                            std::mem::drop(new_guard);
                            if aguard.is_some() {
                                std::mem::drop(aguard);
                            }
                            std::mem::drop(thing);
                            HazPtrHolder::try_reclaim();
                            break taken_value;
                        } else {
                            let drop = unsafe { Box::from_raw(new) };
                            std::mem::drop(drop);
                            self.help(unsafe { (*thing.data).op }, thing.data);
                            /*std::mem::drop(thing);
                            std::mem::drop(new_guard);*/
                            HazPtrHolder::try_reclaim();
                        }
                    } else {
                        let drop = unsafe { Box::from_raw(new) };
                        std::mem::drop(drop);
                        self.help(unsafe { (*thing.data).op }, thing.data);
                        /*std::mem::drop(thing);
                        std::mem::drop(new_guard);*/
                        HazPtrHolder::try_reclaim();
                    }
                }
            }
        }
    }

    fn recursive_delete(
        new_descriptor: *mut Descriptor<'a, T>,
        status: &'_ AtomicUsize,
        ptr: &'_ AtomicPtr<Node<T>>,
        ptr_tail: &'_ AtomicPtr<Node<T>>,
        pending: &'_ AtomicBool,
        current_node: *mut Node<T>,
    ) {
        match pending.load(Ordering::Acquire) {
            true => match status.load(Ordering::Acquire) {
                0 => {
                    if current_node.is_null() {
                        pending.store(false, Ordering::Relaxed);
                        return;
                    }
                    let prev = unsafe { (*current_node).prev.load(Ordering::Acquire) };
                    // this is also an issue
                    if prev.is_null() {
                        ptr.compare_exchange(
                            current_node,
                            std::ptr::null_mut(),
                            Ordering::AcqRel,
                            Ordering::Relaxed,
                        );
                    }

                    status.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Relaxed);
                    Self::recursive_delete(
                        new_descriptor,
                        status,
                        ptr,
                        ptr_tail,
                        pending,
                        current_node,
                    );
                }
                1 => {
                    if unsafe {
                        (&(*current_node).value_moved)
                            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                            .is_ok()
                    } {
                        println!("Giving out value");
                        let taken_value = unsafe { std::ptr::read(&(*current_node).value) };
                        unsafe {
                            (*new_descriptor).taken_value = Some(taken_value);
                        }
                    }

                    status.compare_exchange(1, 2, Ordering::AcqRel, Ordering::Relaxed);
                    Self::recursive_delete(
                        new_descriptor,
                        status,
                        ptr,
                        ptr_tail,
                        pending,
                        current_node,
                    );
                }
                2 => {
                    let prev = unsafe { (*current_node).prev.load(Ordering::Acquire) };
                    status.store(3, Ordering::Relaxed);
                    ptr_tail.compare_exchange(
                        current_node,
                        prev,
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    );
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
                    pending.store(false, Ordering::Relaxed);
                }
                3 => {
                    Self::recursive_delete(
                        new_descriptor,
                        status,
                        ptr,
                        ptr_tail,
                        pending,
                        current_node,
                    );
                }
                _ => unreachable!(),
            },
            false => return,
        }
    }
}
