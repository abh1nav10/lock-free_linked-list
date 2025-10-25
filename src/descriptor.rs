#![allow(dead_code)]
#![allow(unused_must_use)]
#![allow(unused)]
use crate::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize};
use crate::{Deleter, DropBox, DropPointer, HazPtrHolder, HazPtrObject};
use crate::{LinkedList, Node};
use std::mem::MaybeUninit;
use std::ops::DerefMut;
use std::sync::atomic::Ordering;

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
pub(crate) struct Descriptor<T> {
    current: *mut Node<T>, // newest change to solve the biggest problems
    success: AtomicBool,
    next: *mut Node<T>,
    status: AtomicUsize,
    pending: AtomicBool,
    op: Operation,
    deleter: &'static dyn Deleter,
    retired: AtomicBool,
    // we use maybeuninit to make sure that we can have unitialized instances when we create the
    // descriptor in both insertion and deletion.. also to ensure safe getting back of the T on
    // delete we have to introduce another flag to check whether or not the taken value pointer has
    // been swapped by any helper or the main thread to actually store in it a maybeuninit which
    // contains the T... therefore we introduce the init_stored field which if loads to false then
    // we do not assume_init() on the maybeuninit
    taken_value: AtomicPtr<MaybeUninit<T>>,
    init_stored: AtomicBool,
}

enum SwapResult {
    Success,
    //Failure will include both failure in swapping and failure in the operation that occured later
    //as in both the cases we have to loop back
    Failure,
}

unsafe impl<T> Send for Descriptor<T> where T: Send {}
unsafe impl<T> Sync for Descriptor<T> where T: Send {}

impl<T> Drop for LinkedList<T> {
    fn drop(&mut self) {
        let mut holder = HazPtrHolder::default();
        let mut guard = unsafe { holder.load(&self.descriptor) };
        if let Some(ref mut thing) = guard {
            let deleter = unsafe { (*thing.data).deleter };
            let mut swap_holder = HazPtrHolder::default();
            let wrapper = unsafe {
                swap_holder.swap(&AtomicPtr::new(thing.data), std::ptr::null_mut(), deleter)
            };
            if let Some(mut wrapper) = wrapper {
                if unsafe {
                    (&(*wrapper.inner).retired).compare_exchange(
                        false,
                        true,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
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
        std::mem::drop(guard);
        HazPtrHolder::try_reclaim();
    }
}

impl<T> Descriptor<T> {
    fn new(
        current: *mut Node<T>,
        success: AtomicBool,
        next: *mut Node<T>,
        status: AtomicUsize,
        pending: AtomicBool,
        op: Operation,
        deleter: &'static dyn Deleter,
        retired: AtomicBool,
        taken_value: AtomicPtr<MaybeUninit<T>>,
        init_stored: AtomicBool,
    ) -> Self {
        Self {
            current,
            success,
            next,
            status,
            pending,
            op,
            deleter,
            retired,
            taken_value,
            init_stored,
        }
    }
}

impl<T> LinkedList<T> {
    pub(crate) fn insert(&self, next: *mut Node<T>) {
        loop {
            let mut current_node_holder = HazPtrHolder::default();
            let mut current_node_guard = unsafe { current_node_holder.load(&self.head) };
            let current_node = if let Some(ref mut guard) = current_node_guard {
                guard.data
            } else {
                std::ptr::null_mut()
            };
            let uninit = Box::into_raw(Box::new(MaybeUninit::uninit()));
            let new_descriptor: *mut Descriptor<T> = Box::into_raw(Box::new(Descriptor::new(
                current_node,
                AtomicBool::new(false),
                next,
                AtomicUsize::new(0),
                AtomicBool::new(true),
                Operation::Insert,
                &DELETER1,
                AtomicBool::new(false),
                AtomicPtr::new(uninit),
                AtomicBool::new(false),
            )));
            if self.descriptor.load(Ordering::Acquire).is_null() {
                if let SwapResult::Success = self.swap_null_insert(new_descriptor) {
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
                    if !unsafe { (*thing.data).pending.load(Ordering::SeqCst) } {
                        if self
                            .descriptor
                            .compare_exchange(
                                thing.data,
                                new_descriptor,
                                Ordering::SeqCst,
                                Ordering::SeqCst,
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
                                            Ordering::SeqCst,
                                            Ordering::SeqCst,
                                        )
                                        .is_ok()
                                } {
                                    wrapper.retire();
                                }
                            }
                            self.loop_insert(new_descriptor_guard.data);
                            // we now check whether the operation was actually successful
                            if unsafe {
                                (*new_descriptor_guard.data).success.load(Ordering::SeqCst)
                            } {
                                std::mem::drop(pending_holder_guard);
                                std::mem::drop(new_descriptor_guard);
                                std::mem::drop(current_node_guard);
                                HazPtrHolder::try_reclaim();
                                break;
                            } else {
                                std::mem::drop(pending_holder_guard);
                                std::mem::drop(current_node_guard);
                                std::mem::drop(new_descriptor_guard);
                                HazPtrHolder::try_reclaim();
                                // loop back as the operation failed at a later stage
                                continue;
                            }
                        } else {
                            std::mem::drop(new_descriptor_guard);
                            std::mem::drop(current_node_guard);
                            let _ = unsafe { Box::from_raw(new_descriptor) };
                            self.help(thing.data);
                            std::mem::drop(pending_holder_guard);
                            HazPtrHolder::try_reclaim();
                        }
                    } else {
                        std::mem::drop(new_descriptor_guard);
                        std::mem::drop(current_node_guard);
                        let _ = unsafe { Box::from_raw(new_descriptor) };
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
    fn swap_null_insert(&self, new_descriptor: *mut Descriptor<T>) -> SwapResult {
        let mut new_descriptor_holder = HazPtrHolder::default();
        let mut new_descriptor_guard = unsafe {
            new_descriptor_holder
                .load(&AtomicPtr::new(new_descriptor))
                .expect("Has to be there")
        };
        // respecting stack frames...therefore loading it into hazard pointers
        let mut current_node_holder = HazPtrHolder::default();
        let mut current_node_guard = unsafe {
            current_node_holder.load(&AtomicPtr::new((*new_descriptor_guard.data).current))
        };
        if self
            .descriptor
            .compare_exchange(
                std::ptr::null_mut(),
                new_descriptor_guard.data,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
        {
            self.loop_insert(new_descriptor_guard.data);
            if unsafe { (*new_descriptor_guard.data).success.load(Ordering::Acquire) } {
                std::mem::drop(new_descriptor_guard);
                std::mem::drop(current_node_guard);
                HazPtrHolder::try_reclaim();
                return SwapResult::Success;
            } else {
                std::mem::drop(new_descriptor_guard);
                std::mem::drop(current_node_guard);
                HazPtrHolder::try_reclaim();
                return SwapResult::Failure;
            }
        } else {
            return SwapResult::Failure;
        }
    }

    fn help(&self, current_descriptor: *mut Descriptor<T>) {
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
    fn loop_insert(&self, current_descriptor: *mut Descriptor<T>) {
        let mut descriptor_holder = HazPtrHolder::default();
        let mut descriptor_guard =
            unsafe { descriptor_holder.load(&AtomicPtr::new(current_descriptor)) };
        if descriptor_guard.is_none() {
            return;
        }
        let actual_descriptor_guard = descriptor_guard.expect("Has to be there");
        let next = unsafe { (*actual_descriptor_guard.data).next };
        if next.is_null() {
            unsafe {
                (*actual_descriptor_guard.data)
                    .pending
                    .store(false, Ordering::SeqCst);
            }
            return;
        }
        let mut next_ptr_holder = HazPtrHolder::default();
        let mut next_ptr_guard = unsafe { next_ptr_holder.load(&AtomicPtr::new(next)) };
        let mut head_ptr_holder = HazPtrHolder::default();
        let head_ptr = &self.head;
        // we dont check for the head_ptr_guard to be none because we are fine with the head being
        // a null pointer as we are inserting
        let mut head_ptr_guard = unsafe {
            head_ptr_holder.load(&AtomicPtr::new((*actual_descriptor_guard.data).current))
        };
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
            match pending.load(Ordering::SeqCst) {
                true => match status.load(Ordering::SeqCst) {
                    1 => {
                        Self::insert_head(next, current);
                        head_ptr.compare_exchange(
                            current,
                            next,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        );
                        pending.store(false, Ordering::SeqCst);
                        break;
                    }
                    0 => {
                        let now = head_ptr.load(Ordering::SeqCst);
                        if now != current {
                            pending.store(false, Ordering::SeqCst);
                            return;
                        }
                        unsafe {
                            (*actual_descriptor_guard.data)
                                .success
                                .store(true, Ordering::SeqCst);
                        }
                        status.compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst);
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
        let mut old_holder = HazPtrHolder::default();
        let mut guard = unsafe { old_holder.load(&AtomicPtr::new(old)) };
        if guard.is_none() {
            return;
        }
        unsafe {
            (&(*old).prev).store(new, Ordering::SeqCst);
        }
    }

    pub(crate) fn delete(&self) -> Option<T> {
        loop {
            let mut current_node_holder = HazPtrHolder::default();
            let mut current_node_guard = unsafe { current_node_holder.load(&self.tail) };
            if current_node_guard.is_none() {
                //println!("should not be printed for the current test case");
                return None;
            }
            let mut actual_current_node_guard = current_node_guard.expect("Has to be there");
            let uninit = Box::into_raw(Box::new(MaybeUninit::uninit()));
            let new = Box::into_raw(Box::new(Descriptor::new(
                actual_current_node_guard.data,
                AtomicBool::new(false),
                std::ptr::null_mut(),
                AtomicUsize::new(0),
                AtomicBool::new(true),
                Operation::Delete,
                &DELETER1,
                AtomicBool::new(false),
                AtomicPtr::new(uninit),
                AtomicBool::new(false),
            )));
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
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    )
                    .is_ok()
                {
                    self.loop_delete(new_descriptor_guard.data);
                    if unsafe { (*new_descriptor_guard.data).success.load(Ordering::SeqCst) } {
                        // possibly redundant but fine
                        if unsafe {
                            (*new_descriptor_guard.data)
                                .init_stored
                                .load(Ordering::SeqCst)
                        } {
                            let init_ptr = unsafe {
                                (*new_descriptor_guard.data)
                                    .taken_value
                                    .swap(std::ptr::null_mut(), Ordering::SeqCst)
                            };
                            let owned_init = unsafe { Box::from_raw(init_ptr) };
                            let taken_value = unsafe { owned_init.assume_init() };

                            std::mem::drop(new_descriptor_guard);
                            std::mem::drop(actual_current_node_guard);
                            HazPtrHolder::try_reclaim();
                            return Some(*taken_value);
                        } else {
                            std::mem::drop(new_descriptor_guard);
                            std::mem::drop(actual_current_node_guard);
                            HazPtrHolder::try_reclaim();
                            return None;
                        }
                    } else {
                        std::mem::drop(new_descriptor_guard);
                        std::mem::drop(actual_current_node_guard);
                        HazPtrHolder::try_reclaim();
                        continue;
                    }
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
                    if unsafe { !(*thing.data).pending.load(Ordering::SeqCst) } {
                        if self
                            .descriptor
                            .compare_exchange(thing.data, new, Ordering::SeqCst, Ordering::SeqCst)
                            .is_ok()
                        {
                            //println!("After descriptor swap");
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
                                            Ordering::SeqCst,
                                            Ordering::SeqCst,
                                        )
                                        .is_ok()
                                } {
                                    wrapper.retire();
                                }
                            }
                            self.loop_delete(new);
                            if unsafe { (*new_guard.data).success.load(Ordering::SeqCst) } {
                                if unsafe { (*new_guard.data).init_stored.load(Ordering::SeqCst) } {
                                    let init_ptr = unsafe {
                                        (*new_guard.data)
                                            .taken_value
                                            .swap(std::ptr::null_mut(), Ordering::SeqCst)
                                    };
                                    let owned_init = unsafe { Box::from_raw(init_ptr) };
                                    let taken_value = unsafe { owned_init.assume_init() };
                                    std::mem::drop(new_guard);
                                    std::mem::drop(descriptor_guard);
                                    std::mem::drop(actual_current_node_guard);
                                    HazPtrHolder::try_reclaim();
                                    break Some(*taken_value);
                                } else {
                                    std::mem::drop(new_guard);
                                    std::mem::drop(descriptor_guard);
                                    std::mem::drop(actual_current_node_guard);
                                    break None;
                                }
                            } else {
                                std::mem::drop(new_guard);
                                std::mem::drop(descriptor_guard);
                                std::mem::drop(actual_current_node_guard);
                                HazPtrHolder::try_reclaim();
                            }
                        } else {
                            let drop = unsafe { Box::from_raw(new) };
                            std::mem::drop(drop);
                            self.help(thing.data);
                            std::mem::drop(descriptor_guard);
                            std::mem::drop(actual_current_node_guard);
                            std::mem::drop(new_guard);
                            HazPtrHolder::try_reclaim();
                        }
                    } else {
                        let drop = unsafe { Box::from_raw(new) };
                        std::mem::drop(drop);
                        self.help(thing.data);
                        std::mem::drop(descriptor_guard);
                        std::mem::drop(actual_current_node_guard);
                        std::mem::drop(new_guard);
                        HazPtrHolder::try_reclaim();
                    }
                }
            }
        }
    }

    fn loop_delete(&self, current_descriptor: *mut Descriptor<T>) {
        //println!("a");
        let mut descriptor_holder = HazPtrHolder::default();
        let mut descriptor_guard =
            unsafe { descriptor_holder.load(&AtomicPtr::new(current_descriptor)) };
        if descriptor_guard.is_none() {
            return;
        }
        //println!("dg");
        let actual_descriptor_guard = descriptor_guard.expect("Has to be there");
        let tail_ptr = &self.tail;
        let mut tail_ptr_holder = HazPtrHolder::default();
        // load the current from the descriptor and not directly from the pointer
        let mut tail_ptr_guard = unsafe {
            tail_ptr_holder.load(&AtomicPtr::new((*actual_descriptor_guard.data).current))
        };
        if tail_ptr_guard.is_none() {
            unsafe {
                (*actual_descriptor_guard.data)
                    .pending
                    .store(false, Ordering::SeqCst);
            }
            return;
        }
        //println!("tg");
        let actual_tail_ptr_guard = tail_ptr_guard.expect("Has to be there");
        //let prev_ptr = unsafe {(*actual_tail_ptr_guard.data).prev.load(Ordering::Acquire)};
        let mut prev_ptr_holder = HazPtrHolder::default();
        let mut prev_ptr_guard =
            unsafe { prev_ptr_holder.load(&(*actual_tail_ptr_guard.data).prev) };
        let prev = if let Some(ref mut guard) = prev_ptr_guard {
            guard.data
        } else {
            std::ptr::null_mut()
        };
        let mut head_ptr_holder = HazPtrHolder::default();
        let head_ptr = &self.head;
        let mut head_ptr_guard = unsafe { head_ptr_holder.load(head_ptr) };
        if head_ptr_guard.is_none() {
            // store false in the pending field of the descriptor...as there is a possibility of
            // helpers reaching here only after the update has been done...so they return early
            // without changing the pending...assume one thread does the CAS on the head_ptr and
            // the tail_ptr and then gets preempted.. all helpers will then spin perpetually or
            // untill that thread gets rescheduled...so before returning we store false in the
            // pending field to avoid such a scenario
            unsafe {
                (*actual_descriptor_guard.data)
                    .pending
                    .store(false, Ordering::SeqCst);
            }
            return;
        }
        //println!("hg");
        let pending = unsafe { &(*actual_descriptor_guard.data).pending };
        let status = unsafe { &(*actual_descriptor_guard.data).status };
        loop {
            match pending.load(Ordering::SeqCst) {
                true => match status.load(Ordering::SeqCst) {
                    2 => {
                        unsafe {
                            (*actual_descriptor_guard.data)
                                .success
                                .store(true, Ordering::SeqCst)
                        }
                        tail_ptr.compare_exchange(
                            actual_tail_ptr_guard.data,
                            prev,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        );
                        //println!("Once every delete");
                        pending.store(false, Ordering::SeqCst);
                        if unsafe {
                            (*actual_tail_ptr_guard)
                                .retired
                                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
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
                        return;
                    }
                    1 => {
                        //println!("a");
                        let taken_value =
                            unsafe { std::ptr::read(&(*actual_tail_ptr_guard.data).value) };
                        let mut init = MaybeUninit::uninit();
                        unsafe { init.write(taken_value) };
                        let init_ptr = Box::into_raw(Box::new(init));
                        unsafe {
                            (*actual_descriptor_guard.data)
                                .taken_value
                                .store(init_ptr, Ordering::SeqCst);
                            (*actual_descriptor_guard.data)
                                .init_stored
                                .store(true, Ordering::SeqCst);
                        }
                        status.compare_exchange(1, 2, Ordering::SeqCst, Ordering::SeqCst);
                        continue;
                    }
                    0 => {
                        let current = tail_ptr.load(Ordering::SeqCst);
                        // the idea is to make the swapping of the tail_ptr the last step
                        // therefore... helper threads will help when required and will just
                        // instantly return when helping is not required or when pointer that we
                        // expected to be stored into the tail_ptr is not actually there
                        //println!("0");
                        if current != actual_tail_ptr_guard.data {
                            pending.store(false, Ordering::SeqCst);
                            break;
                        }
                        if prev.is_null() {
                            head_ptr.compare_exchange(
                                actual_tail_ptr_guard.data,
                                std::ptr::null_mut(),
                                Ordering::SeqCst,
                                Ordering::SeqCst,
                            );
                        }
                        //println!("hgswap");
                        status.compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst);
                        continue;
                    }
                    _ => unreachable!(),
                },
                false => return,
            }
        }
    }
}
