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
pub enum Operation {
    InsertHead,
    DeleteTail,
    Iterate(usize),
}

pub struct Descriptor<'a, T> {
    ptr: &'a AtomicPtr<Node<T>>,
    next: *mut Node<T>,
    status: AtomicUsize,
    pending: AtomicBool,
    op: Operation,
    deleter: &'static dyn Deleter,
}

pub struct RawDescriptor<'a, T> {
    descriptor: AtomicPtr<Descriptor<'a, T>>,
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
    pub fn initiate(
        &self,
        ptr: &'a AtomicPtr<Node<T>>,
        next: *mut Node<T>,
        deleter: &'static dyn Deleter,
        op: Operation,
    ) {
        let new_descriptor = Box::into_raw(Box::new(Descriptor::new(
            ptr,
            next,
            AtomicUsize::new(0),
            AtomicBool::new(true),
            op,
            deleter,
        )));
        let next = unsafe { (*new_descriptor).next };
        let status = unsafe { &(*new_descriptor).status };
        let pending = unsafe { &(*new_descriptor).pending };
        let op = unsafe { (*new_descriptor).op };
        loop {
            let a = self.descriptor.load(Ordering::Acquire);
            // let mut guard = unsafe { holder.load(&self.descriptor) };
            if a.is_null() {
                if self
                    .swap_null(new_descriptor, status, ptr, pending, next, op)
                    .is_ok()
                {
                    break;
                }
            } else {
                // Success in the CAS means that the descriptor and the node are not going to be
                // dropped as long as the this descriptor finishes its task. The node is also not
                // is guaranteed by the fact that for now we will support insertion and deletion
                // from separate ends and the logic of deletion to deal with edge cases that may
                // cause corruption will be dealt in detail on implementing the delete method
                if (unsafe { &(*a).pending })
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    let mut swapholder = HazPtrHolder::default();
                    let mut wrapper =
                        unsafe { swapholder.swap(&self.descriptor, new_descriptor, (*a).deleter) };
                    if let Some(mut wrapper) = wrapper {
                        wrapper.retire();
                    }
                    Self::recursive(status, ptr, pending, next, ptr.load(Ordering::Acquire), op);
                    break;
                } else {
                    self.help();
                }
            }
        }
    }

    // The swap_null function is called when we find that the pointer to the descriptor is null.
    // The function tries to compare and exchange expecting a null pointer which if succeeds we
    // initiate the recurive call and if fails we move forward to see if there is anyone we can
    // help before looping back.
    fn swap_null(
        &self,
        new_descriptor: *mut Descriptor<'a, T>,
        status: &'_ AtomicUsize,
        ptr: &'_ AtomicPtr<Node<T>>,
        pending: &'_ AtomicBool,
        next: *mut Node<T>,
        op: Operation,
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
            Self::recursive(status, ptr, pending, next, std::ptr::null_mut(), op);
            Ok(())
        } else {
            Err(())
        }
    }

    /// The help function when called first loads the descriptors into a hazard pointer, it it gets
    /// back None it immediately loops back as there is no point forward. If it gets some it then
    /// loads the current pointer to the node into a hazard pointer and does the same process. It
    /// then does the same thing for the next node which has to be stored in the pointer. The way
    /// this gurantees safety from the danger of corrupting the nodes of the linked list is the
    /// fact that for every operation there is a specific descriptor. If all the nodes are loaded
    /// successfully and they are the same as what we expected, then everyting obviosly goes well.
    /// But there are many edge cases to consider. Assume after we load everything and move on to
    /// the recursive function call some other thread already finishes the operation and then some
    /// other thread tries to delete the node that we are about to dereference. There would be a
    /// danger of dereferencing null pointers which is prevented by hazard pointer. Hence we are
    /// safe from the point of view. Another is that assume we load the descriptor and before we
    /// load the node some other thread inserts new one. Now when we load the ptr we are actually
    /// looking at a different node than what we expected. If we update the previous feld of this
    /// node the list will get corrupted. This is prevented by that unique descriptors for every
    /// operation wherein if ever a new node is seen which is different from what was expected then
    /// the status field will invariably point to completed in which case our thread will not
    /// perform the dereferencing and the update of the previous field, but instead loop back.
    fn help(&self) {
        let mut descriptor_holder = HazPtrHolder::default();
        let mut descriptor_guard = unsafe { descriptor_holder.load(&self.descriptor) };
        if descriptor_guard.is_some() {
            let pointer = descriptor_guard.expect("Has to be there");
            let mut node_holder = HazPtrHolder::default();
            let mut node_guard =
                unsafe { node_holder.load(&(*self.descriptor.load(Ordering::Acquire)).ptr) };
            if node_guard.is_some() {
                let mut next_node_holder = HazPtrHolder::default();
                // We need this atomic pointer solely for guarding the next with a hazard pointer,
                // this is done because the next field inside the descriptor struct is suppossed to
                // be a raw pointer to Node<T> but the load method of HazPtrHolder requires an
                // atomic pointer.
                let next_atomic_ptr =
                    unsafe { AtomicPtr::new((*self.descriptor.load(Ordering::Acquire)).next) };
                let mut next_node_guard = unsafe { next_node_holder.load(&next_atomic_ptr) };
                if next_node_guard.is_some() {
                    let status = &pointer.status;
                    let pending = &pointer.pending;
                    let next = pointer.next;
                    let op = pointer.op;
                    let pointer = &pointer.ptr;
                    Self::recursive(
                        status,
                        pointer,
                        pending,
                        next,
                        node_guard.expect("Has to be there").deref_mut() as *mut Node<T>,
                        op,
                    );
                }
            }
        }
    }

    /// The recursive function allows threads to see how much of the task of a given descriptor is
    /// completed and help accordingly.
    /// Stack overflow won't be an issue because the steps are bounded by a fixed maxima.
    fn recursive(
        status: &'_ AtomicUsize,
        ptr: &'_ AtomicPtr<Node<T>>,
        pending: &'_ AtomicBool,
        next: *mut Node<T>,
        current_node: *mut Node<T>,
        op: Operation,
    ) {
        match pending.load(Ordering::Acquire) {
            true => match status.load(Ordering::Acquire) {
                0 => {
                    ptr.store(next, Ordering::Release);
                    status.store(1, Ordering::Release);
                    Self::recursive(status, ptr, pending, next, current_node, op);
                }
                1 => {
                    match op {
                        Operation::InsertHead => {
                            Self::insert_head(next, current_node);
                        }
                        Operation::DeleteTail => panic!("Deletion not yet supported"),
                        Operation::Iterate(n) => panic!("Iteration not yet supported"),
                    }
                    status.store(2, Ordering::Release);
                }
                2 => {
                    pending.store(false, Ordering::Release);
                }
                _ => unreachable!(),
            },
            false => return,
        }
    }

    // Updates the fields in accordance with head insertion.
    fn insert_head(new: *mut Node<T>, old: *mut Node<T>) {
        if old.is_null() {
            return;
        }
        unsafe {
            (&(*new).next).store(old, Ordering::Release);
            (&(*old).prev).store(new, Ordering::Release);
        }
    }

    fn delete_tail(new: *mut Node<T>, old: *mut Node<T>) {
        todo!()
    }

    fn iterate() {
        todo!()
    }
}
