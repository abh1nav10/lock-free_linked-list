use std::collections::HashSet;
use std::convert::AsRef;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;
use std::sync::atomic::Ordering;
use std::sync::atomic::{AtomicBool, AtomicPtr};

pub(crate) static SHARED_DOMAIN: HazPtrDomain = HazPtrDomain {
    list: HazPtrs {
        head: AtomicPtr::new(std::ptr::null_mut()),
    },
    ret: Retired {
        head: AtomicPtr::new(std::ptr::null_mut()),
    },
};

#[derive(Default)]
pub struct HazPtrHolder(Option<&'static HazPtr>);

pub struct Guard<'a, T> {
    hazptr: &'static HazPtr,
    pub(crate) data: *mut T,
    _marker: PhantomData<&'a T>,
}

impl<T> AsRef<T> for Guard<'_, T> {
    fn as_ref(&self) -> &T {
        &(*self)
    }
}

impl<T> Deref for Guard<'_, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        unsafe { &(*self.data) }
    }
}

///SAFETY:
///  This method can cause safety issues so it must be handled with care.
///  If two threads deref_mut the guard to the same underlying T we will
///  then have two mutable pointers to the same thing. If they are used to
///  read or write at the same time, we will run into undefined behaviour.
impl<T> DerefMut for Guard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut (*self.data) }
    }
}

impl<T> Drop for Guard<'_, T> {
    fn drop(&mut self) {
        self.hazptr
            .ptr
            .store(std::ptr::null_mut(), Ordering::SeqCst);
        self.hazptr.flag.store(true, Ordering::SeqCst);
    }
}

impl HazPtrHolder {
    /// SAFETY:
    ///   1. The user must pass a valid pointer. Passing in invalid pointers such as a misaligned
    ///      one will cause undefined behaviour.
    ///   2. If a null pointer is passed that will be taken care of by the implementation as we
    ///      have made sure using NonNull that it does not get dereferenced.
    pub unsafe fn load<'a, T>(&'a mut self, ptr: &'_ AtomicPtr<T>) -> Option<Guard<'a, T>> {
        let hazptr = if let Some(t) = self.0 {
            t
        } else {
            let ptr = SHARED_DOMAIN.acquire();
            self.0 = Some(ptr);
            ptr
        };
        let mut ptr1 = ptr.load(Ordering::SeqCst);
        let ret = loop {
            hazptr.protect(ptr1 as *mut ());
            let ptr2 = ptr.load(Ordering::SeqCst);
            if ptr1 == ptr2 {
                if let Some(_) = NonNull::new(ptr1) {
                    let data = ptr1;
                    break Some(Guard {
                        hazptr: &hazptr,
                        data: data,
                        _marker: PhantomData,
                    });
                } else {
                    break None;
                }
            } else {
                ptr1 = ptr2;
            }
        };
        return ret;
    }

    ///SAFETY:
    ///  1. Swap ensures that the old pointer gets retired. The user must make sure that similar to
    ///     the load method, a valid pointer is passed failing which will cause undefined
    ///     behaviour.
    ///  2. Calling the swap method with a retired pointer will cause the retired pointer to be
    ///     retired again which will lead to it being double reclaimed leading to undefined
    ///     behaviour. The user must ensure that this does not happen.
    pub unsafe fn swap<T>(
        &mut self,
        atomic: &'_ AtomicPtr<T>,
        ptr: *mut T,
        deleter: &'static dyn Deleter,
    ) -> Option<HazPtrObjectWrapper<'_, T>> {
        let current = atomic.load(Ordering::SeqCst);
        atomic.store(ptr, Ordering::SeqCst);
        if current.is_null() {
            return None;
        } else {
            let wrapper = HazPtrObjectWrapper {
                inner: current,
                domain: &SHARED_DOMAIN,
                deleter: deleter,
            };
            return Some(wrapper);
        }
    }

    ///SAFETY:
    ///  1. This method provides a way to get the wrapper to call the retire method if the user is
    ///     not relying on swap. It must be used with care as repeatedly using load without
    ///     using this method and calling retire on it will lead to memory leaks.
    pub unsafe fn get_wrapper<T>(
        &mut self,
        atomic: &'_ AtomicPtr<T>,
        deleter: &'static dyn Deleter,
    ) -> Option<HazPtrObjectWrapper<'_, T>> {
        let current = atomic.load(Ordering::SeqCst);
        atomic.store(std::ptr::null_mut(), Ordering::SeqCst);
        if current.is_null() {
            return None;
        } else {
            let wrapper = HazPtrObjectWrapper {
                inner: current,
                domain: &SHARED_DOMAIN,
                deleter: deleter,
            };
            return Some(wrapper);
        }
    }
}

pub(crate) struct HazPtr {
    ptr: AtomicPtr<()>,
    next: AtomicPtr<HazPtr>,
    flag: AtomicBool,
}

impl HazPtr {
    pub fn protect(&self, ptr: *mut ()) {
        self.ptr.store(ptr, Ordering::SeqCst);
    }
}

pub(crate) trait HazPtrObject {
    fn domain<'a>(&'a self) -> &'a HazPtrDomain;
    fn retire(&mut self);
}

pub struct HazPtrObjectWrapper<'a, T> {
    inner: *mut T,
    domain: &'a HazPtrDomain,
    deleter: &'static dyn Deleter,
}

impl<T> Deref for HazPtrObjectWrapper<'_, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        unsafe { &(*self.inner) }
    }
}

impl<T> DerefMut for HazPtrObjectWrapper<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut (*self.inner) }
    }
}

impl<T> HazPtrObject for HazPtrObjectWrapper<'_, T> {
    fn domain<'a>(&'a self) -> &'a HazPtrDomain {
        self.domain
    }

    ///SAFETY:
    ///  The user must make sure that a retired pointer is not retired again.
    fn retire(&mut self) {
        if self.inner.is_null() {
            return;
        }
        let domain = self.domain();
        let current = (&domain.ret.head).load(Ordering::SeqCst);
        loop {
            let ret = Ret {
                ptr: self.inner as *mut dyn Uniform,
                next: AtomicPtr::new(std::ptr::null_mut()),
                deleter: self.deleter,
            };
            if current.is_null() {
                let boxed = Box::leak(Box::new(ret));
                if domain
                    .ret
                    .head
                    .compare_exchange(
                        std::ptr::null_mut(),
                        boxed,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    )
                    .is_err()
                {
                    let drop = unsafe { Box::from_raw(boxed) };
                    std::mem::drop(drop);
                } else {
                    unsafe { (&domain.ret).reclaim(&domain.list) };
                    break;
                }
            } else {
                ret.next.store(current, Ordering::SeqCst);
                let boxed = Box::leak(Box::new(ret));
                if domain
                    .ret
                    .head
                    .compare_exchange(current, boxed, Ordering::SeqCst, Ordering::SeqCst)
                    .is_err()
                {
                    let drop = unsafe { Box::from_raw(boxed) };
                    std::mem::drop(drop);
                } else {
                    unsafe { (&domain.ret).reclaim(&domain.list) };
                    break;
                }
            }
        }
    }
}

pub struct HazPtrDomain {
    list: HazPtrs,
    ret: Retired,
}

impl HazPtrDomain {
    pub fn acquire(&self) -> &'static HazPtr {
        if self.list.head.load(Ordering::SeqCst).is_null() {
            let hazptr = HazPtr {
                ptr: AtomicPtr::new(std::ptr::null_mut()),
                next: AtomicPtr::new(std::ptr::null_mut()),
                flag: AtomicBool::new(false),
            };
            let raw = Box::into_raw(Box::new(hazptr));
            if self
                .list
                .head
                .compare_exchange(
                    std::ptr::null_mut(),
                    raw,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_ok()
            {
                return unsafe { &*raw };
            } else {
                let drop = unsafe { Box::from_raw(raw) };
                std::mem::drop(drop);
            }
        }
        let mut current = (&self.list.head).load(Ordering::SeqCst);
        while !current.is_null() {
            if unsafe { &(*current).flag }
                .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return unsafe { &(*current) };
            } else {
                current = unsafe { (&(*current).next).load(Ordering::SeqCst) };
            }
        }
        let mut now = &self.list.head;
        loop {
            let mut new = HazPtr {
                ptr: AtomicPtr::new(std::ptr::null_mut()),
                next: AtomicPtr::new(std::ptr::null_mut()),
                flag: AtomicBool::new(false),
            };
            new.next = AtomicPtr::new(now.load(Ordering::SeqCst));
            let boxed = Box::into_raw(Box::new(new));
            if self
                .list
                .head
                .compare_exchange(
                    now.load(Ordering::SeqCst),
                    boxed,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_ok()
            {
                return unsafe { &*boxed };
            } else {
                now = &self.list.head;
                let drop = unsafe { Box::from_raw(boxed) };
                std::mem::drop(drop);
                while !current.is_null() {
                    let flag = unsafe { &(*current).flag };
                    if flag
                        .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                    {
                        return unsafe { &(*current) };
                    } else {
                        current = unsafe { (&(*current).next).load(Ordering::SeqCst) };
                    }
                }
            }
        }
    }
}

pub(crate) struct HazPtrs {
    head: AtomicPtr<HazPtr>,
}

pub struct Retired {
    head: AtomicPtr<Ret>,
}

pub trait Uniform {}

impl<T> Uniform for T {}

pub(crate) struct Ret {
    ptr: *mut dyn Uniform,
    next: AtomicPtr<Ret>,
    deleter: &'static dyn Deleter,
}

pub trait Deleter {
    fn delete(&self, ptr: *mut dyn Uniform);
}

/// SAFETY:
///   1. The user would have to pass an instance of one of the two zero sized types defined below:
///     DropBox and DropPointer on the basis of how the actual raw pointer to the underlying type
///     was created. This is necessary because using the drop_in_place() method on every pointer will
///     not dealloate the instance of the box for all those pointers created using Box::into_raw().
///   2. The user must create the instance using static as the trait object must have a static
///      lifetime because we never know when the delete method on that deleter will be called.
///      Using static does not come with any memory overhead as the underlying type would be a zero
///      sized type.
pub struct DropBox;

impl DropBox {
    pub const fn new() -> Self {
        DropBox
    }
}

impl Deleter for DropBox {
    fn delete(&self, ptr: *mut dyn Uniform) {
        if let Some(_) = NonNull::new(ptr) {
            let drop = unsafe { Box::from_raw(ptr) };
            std::mem::drop(drop);
        }
    }
}

pub struct DropPointer;

impl DropPointer {
    pub const fn new() -> Self {
        DropPointer
    }
}

impl Deleter for DropPointer {
    fn delete(&self, ptr: *mut dyn Uniform) {
        if let Some(_) = NonNull::new(ptr) {
            unsafe {
                std::ptr::drop_in_place(ptr);
            }
        }
    }
}

impl Retired {
    /// SAFETY:
    ///    The user must make sure that the reclaim method is not called on the list of retired
    ///    pointers contaning two similar pointers as this will lead to the same pointers being
    ///    dereferenced leading to undefined behaviour.
    unsafe fn reclaim<'a>(&self, domain: &'a HazPtrs) {
        let mut set = HashSet::new();
        let mut current = (&(domain.head)).load(Ordering::SeqCst);
        while !current.is_null() {
            let a = unsafe { (*current).ptr.load(Ordering::SeqCst) };
            set.insert(a);
            current = unsafe { (&(*current).next).load(Ordering::SeqCst) };
        }
        let mut remaining = std::ptr::null_mut();
        let mut now = (self.head).swap(std::ptr::null_mut(), Ordering::SeqCst);
        while !now.is_null() {
            let check = unsafe { (*now).ptr };
            if !set.contains(&(check as *mut ())) {
                let deleter = unsafe { (*now).deleter };
                deleter.delete(check);
                let go = now;
                now = unsafe { ((*now).next).load(Ordering::SeqCst) };
                let drop = unsafe { Box::from_raw(go) };
                std::mem::drop(drop);
            } else {
                let next = unsafe { ((*now).next).load(Ordering::SeqCst) };
                unsafe { (*now).next.store(remaining, Ordering::SeqCst) };
                if remaining.is_null() {
                    remaining = now;
                    unsafe {
                        (*remaining)
                            .next
                            .store(std::ptr::null_mut(), Ordering::SeqCst);
                    }
                } else {
                    unsafe {
                        (*now).next.store(remaining, Ordering::SeqCst);
                    }
                    remaining = now;
                }
                now = next;
            }
        }
        self.head.swap(remaining, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    struct CountDrops(Arc<AtomicUsize>);
    impl Drop for CountDrops {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    impl CountDrops {
        fn get_number_of_drops(&self) -> usize {
            self.0.load(Ordering::Relaxed)
        }
    }
    #[test]
    fn test() {
        let new = Arc::new(AtomicUsize::new(0));
        let check = CountDrops(new.clone());
        let value1 = CountDrops(new.clone());
        let value2 = CountDrops(new.clone());
        let boxed1 = Box::into_raw(Box::new(value1));
        let boxed2 = Box::into_raw(Box::new(value2));
        let atm_ptr = AtomicPtr::new(boxed1);
        let mut holder = HazPtrHolder::default();
        let guard = unsafe { holder.load(&atm_ptr) };
        static DROPBOX: DropBox = DropBox::new();
        std::mem::drop(guard);
        if let Some(mut wrapper) = unsafe { holder.swap(&atm_ptr, boxed2, &DROPBOX) } {
            wrapper.retire();
        }
        assert_eq!(check.get_number_of_drops(), 1 as usize);
    }
}
