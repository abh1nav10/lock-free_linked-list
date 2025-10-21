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
                if self.swap_null_insert(new_descriptor).is_ok() {
                    return;
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
                if let Some(ref mut thing) = pending_holder_guard {
                    let mut new_descriptor_holder = HazPtrHolder::default();
                    let mut new_descriptor_guard = unsafe {
                        new_descriptor_holder
                            .load(&AtomicPtr::new(new_descriptor))
                            .expect("Has to be there")
                    };
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
                            let mut swapholder = HazPtrHolder::default();
                            let mut wrapper = unsafe {
                                swapholder.swap(
                                    &AtomicPtr::new(thing.data),
                                    std::ptr::null_mut(),
                                    ((*thing.data).deleter),
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
                            self.loop_insert(new_descriptor_guard.data);
                            std::mem::drop(pending_holder_guard);
                            std::mem::drop(new_descriptor_guard);
                            HazPtrHolder::try_reclaim();
                            break;
                        } else {
                            std::mem::drop(new_descriptor_guard);
                            self.help(thing.data);
                            std::mem::drop(pending_holder_guard);
                            HazPtrHolder::try_reclaim();
                        }
                    } else {
                        std::mem::drop(new_descriptor_guard);
                        self.help(thing.data);
                        std::mem::drop(pending_holder_guard);
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
    fn swap_null_insert(&self, new_descriptor: *mut Descriptor<'a, T>) -> Result<(), ()> {
        let mut new_descriptor_holder = HazPtrHolder::default();
        let mut new_descriptor_guard = unsafe {
            new_descriptor_holder
                .load(&AtomicPtr::new(new_descriptor))
                .expect("Has to be there")
        };
        if self
            .descriptor
            .compare_exchange(
                std::ptr::null_mut(),
                new_descriptor_guard.data,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            self.loop_insert(new_descriptor_guard.data);
            HazPtrHolder::try_reclaim();
            Ok(())
        } else {
            Err(())
        }
    }

    fn help(&self, current_descriptor: *mut Descriptor<'a, T>) {
        let mut holder = HazPtrHolder::default();
        let mut guard = unsafe { holder.load(&AtomicPtr::new(current_descriptor)) };
        if guard.is_none() {
            return;
        }
        let actual_guard = guard.expect("Has to be there");
        let op = unsafe { (*actual_guard.data).op };
        match op {
            Operation::Insert => {
                self.loop_insert(current_descriptor);
            }
            Operation::Delete => {
                self.loop_delete(current_descriptor);
            }
        }
    }

    // note down later why the recursive approach did not work and had to switch to loop based
    // approach
    fn loop_insert(&self, current_descriptor: *mut Descriptor<'a, T>) {
        let mut descriptor_holder = HazPtrHolder::default();
        let mut descriptor_guard =
            unsafe { descriptor_holder.load(&AtomicPtr::new(current_descriptor)) };
        if descriptor_guard.is_none() {
            return;
        }
        let actual_descriptor_guard = descriptor_guard.expect("Has to be there");
        let next = unsafe { (*actual_descriptor_guard.data).next };
        if next.is_null() {
            return;
        }
        let mut next_ptr_holder = HazPtrHolder::default();
        let mut next_ptr_guard = unsafe { next_ptr_holder.load(&AtomicPtr::new(next)) };
        let mut head_ptr_holder = HazPtrHolder::default();
        let head_ptr = unsafe { &(*actual_descriptor_guard.data).ptr };
        // we dont check for the head_ptr_guard to be none because we are fine with the head being
        // a null pointer as we are inserting
        let mut head_ptr_guard = unsafe { head_ptr_holder.load(head_ptr) };
        // the logic here is that if a load some other pointer then there can be two scenarios that
        // exist...either i load one which is completely distinct from this descriptor's operation
        // in which case the pending and status fields will prevent us from doing any harm and
        // another case it that it is possible that some helper thread or even the initiator
        // thread loads it after the new head has been inserted in which case the only possibility
        // is that we will jump to status field 2 and then wait for the threads that are on status
        // field 1 to finish the operation.. lock freedom is maintained because multiple threads
        // can get to status field 1 and complete the operation alongside with each other
        let current = if let Some(ref mut guard) = head_ptr_guard {
            guard.data
        } else {
            std::ptr::null_mut()
        };
        let pending = unsafe { &(*actual_descriptor_guard.data).pending };
        let status = unsafe { &(*actual_descriptor_guard.data).status };
        loop {
            match pending.load(Ordering::Acquire) {
                true => match status.load(Ordering::Acquire) {
                    0 => {
                        Self::insert_head(next, current);
                        // Using store instead of CAS can corrupt the process, assume that the status
                        // has already reached to 2 but there is still a possibility of it being
                        // 1 by other threads who were in the process of helping.
                        status.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Relaxed);
                        continue;
                    }
                    1 => {
                        // instead of doing this maybe i will have to swap the descriptor pointer
                        status.store(2, Ordering::Release);
                        // ABA issue is prevented by the status fields... it just cannot happen because
                        // the memory that current points to cannot be freed once loaded into a hazard
                        // pointer as we do above...so the status fields combine with the storing of 2
                        // in the status field before the swapping of the head pointer to prevent
                        // corruption and undefined behaviour scenarios
                        head_ptr.compare_exchange(
                            current,
                            next,
                            Ordering::AcqRel,
                            Ordering::Relaxed,
                        );
                        pending.store(false, Ordering::Release);
                        break;
                    }
                    2 => {
                        continue;
                    }
                    _ => unreachable!(),
                },
                false => return,
            }
        }
    }

    // Updates the fields in accordance with head insertion.
    fn insert_head(new: *mut Node<T>, old: *mut Node<T>) {
        // explicit checking of next through hazards before storing it into the prev field of the
        // old...this gives an extra layer of safety because it is possible that some helper
        // thread or the initiator thread called this function after the process was completed and
        // a new operation of deleting had also been completed in between then this thing becomes
        // ridiculous and can have safety downsides and may possibly corrupt the list
        let mut holder = HazPtrHolder::default();
        let mut guard = unsafe { holder.load(&AtomicPtr::new(new)) };
        if guard.is_none() {
            return;
        }
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
            // we can maybe holder load the new descriptor
            if self.descriptor.load(Ordering::Acquire).is_null() {
                let mut new_descriptor_holder = HazPtrHolder::default();
                let mut new_descriptor_guard = unsafe {
                    new_descriptor_holder
                        .load(&AtomicPtr::new(new))
                        .expect("Has to be there")
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
                    self.loop_delete(new_descriptor_guard.data);
                    let taken_value = unsafe { (*new_descriptor_guard.data).taken_value.take() };
                    std::mem::drop(new_descriptor_guard);
                    HazPtrHolder::try_reclaim();
                    break taken_value;
                }
            } else {
                let mut descriptor_holder = HazPtrHolder::default();
                let mut descriptor_guard = unsafe { descriptor_holder.load(&self.descriptor) };
                if let Some(ref mut thing) = descriptor_guard {
                    let mut new_holder = HazPtrHolder::default();
                    let mut new_guard = unsafe {
                        new_holder
                            .load(&AtomicPtr::new(new))
                            .expect("Has to be there")
                    };
                    if unsafe { !(*thing.data).pending.load(Ordering::Acquire) } {
                        if self
                            .descriptor
                            .compare_exchange(thing.data, new, Ordering::AcqRel, Ordering::Relaxed)
                            .is_ok()
                        {
                            let mut swap_holder = HazPtrHolder::default();
                            let mut wrapper = unsafe {
                                swap_holder.swap(
                                    &AtomicPtr::new(thing.data),
                                    std::ptr::null_mut(),
                                    &DELETER1,
                                )
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
                            self.loop_delete(new);
                            let taken_value = unsafe { (*new_guard.data).taken_value.take() };
                            std::mem::drop(new_guard);
                            std::mem::drop(descriptor_guard);
                            HazPtrHolder::try_reclaim();
                            break taken_value;
                        } else {
                            let drop = unsafe { Box::from_raw(new) };
                            std::mem::drop(drop);
                            self.help(thing.data);
                            std::mem::drop(descriptor_guard);
                            std::mem::drop(new_guard);
                            HazPtrHolder::try_reclaim();
                        }
                    } else {
                        let drop = unsafe { Box::from_raw(new) };
                        std::mem::drop(drop);
                        self.help(thing.data);
                        std::mem::drop(descriptor_guard);
                        std::mem::drop(new_guard);
                        HazPtrHolder::try_reclaim();
                    }
                }
            }
        }
    }

    fn loop_delete(&self, current_descriptor: *mut Descriptor<'a, T>) {
        let mut descriptor_holder = HazPtrHolder::default();
        let mut descriptor_guard =
            unsafe { descriptor_holder.load(&AtomicPtr::new(current_descriptor)) };
        if descriptor_guard.is_none() {
            return;
        }
        let actual_descriptor_guard = descriptor_guard.expect("Has to be there");
        let tail_ptr = unsafe { (*actual_descriptor_guard.data).tail_ptr };
        let mut tail_ptr_holder = HazPtrHolder::default();
        let mut tail_ptr_guard = unsafe { tail_ptr_holder.load(&tail_ptr) };
        if tail_ptr_guard.is_none() {
            return;
        }
        let actual_tail_ptr_guard = tail_ptr_guard.expect("Has to be there");
        let mut head_ptr_holder = HazPtrHolder::default();
        let head_ptr = unsafe { (*actual_descriptor_guard.data).ptr };
        let mut head_ptr_guard = unsafe { head_ptr_holder.load(&head_ptr) };
        if head_ptr_guard.is_none() {
            return;
        }
        let pending = unsafe { &(*actual_descriptor_guard.data).pending };
        let status = unsafe { &(*actual_descriptor_guard.data).status };
        // the idea is that the pending and status field combine with the storing of status number
        // 3 in the status field by the threads which are at the point of mutating the tail_ptr
        // ensure that the list does not get corrupted...if the pointer that we load is entirely
        // different from what the descriptor expected then our status fields will save us from
        // corrupting the list... and storing of 3 in makes sure that nobody can do anything on the
        // tail pointer after it has been updated
        loop {
            match pending.load(Ordering::Acquire) {
                true => match status.load(Ordering::Acquire) {
                    0 => {
                        let prev =
                            unsafe { (*actual_tail_ptr_guard.data).prev.load(Ordering::Acquire) };
                        if prev.is_null() {
                            tail_ptr.compare_exchange(
                                actual_tail_ptr_guard.data,
                                std::ptr::null_mut(),
                                Ordering::AcqRel,
                                Ordering::Relaxed,
                            );
                        }

                        status.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Relaxed);
                        continue;
                    }
                    1 => {
                        if unsafe {
                            (&(*actual_tail_ptr_guard.data).value_moved)
                                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                                .is_ok()
                        } {
                            println!("Giving out value");
                            let taken_value =
                                unsafe { std::ptr::read(&(*actual_tail_ptr_guard.data).value) };
                            unsafe {
                                (*actual_descriptor_guard.data).taken_value = Some(taken_value);
                            }
                        }

                        status.compare_exchange(1, 2, Ordering::AcqRel, Ordering::Relaxed);
                        continue;
                    }
                    2 => {
                        let prev =
                            unsafe { (*actual_tail_ptr_guard.data).prev.load(Ordering::Acquire) };

                        status.compare_exchange(2, 3, Ordering::AcqRel, Ordering::Relaxed);
                        tail_ptr.compare_exchange(
                            actual_tail_ptr_guard.data,
                            prev,
                            Ordering::AcqRel,
                            Ordering::Relaxed,
                        );
                        if unsafe {
                            (*actual_tail_ptr_guard)
                                .retired
                                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                                .is_ok()
                        } {
                            let mut hazholder = HazPtrHolder::default();
                            let mut wrapper = unsafe {
                                hazholder.swap(
                                    &AtomicPtr::new(actual_tail_ptr_guard.data),
                                    std::ptr::null_mut(),
                                    &DELETER1,
                                )
                            };
                            if let Some(mut wrapper) = wrapper {
                                wrapper.retire();
                            }
                        }

                        pending.store(false, Ordering::Release);
                        break;
                    }
                    3 => {
                        continue;
                    }
                    _ => unreachable!(),
                },
                false => return,
            }
        }
    }
}
