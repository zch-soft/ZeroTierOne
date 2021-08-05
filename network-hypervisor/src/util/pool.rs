use std::mem::size_of;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Weak};

use parking_lot::Mutex;

/// Trait for objects that create and reset poolable objects.
pub trait PoolFactory<O> {
    fn create(&self) -> O;
    fn reset(&self, obj: &mut O);
}

struct PoolEntry<O, F: PoolFactory<O>> {
    obj: O,
    return_pool: Weak<PoolInner<O, F>>,
}

struct PoolInner<O, F: PoolFactory<O>>(F, Mutex<Vec<*mut PoolEntry<O, F>>>);

/// Container for pooled objects that have been checked out of the pool.
///
/// When this is dropped the object is returned to the pool or if the pool or is
/// dropped if the pool has been dropped. There is also an into_raw() and from_raw()
/// functionality that allows conversion to/from naked pointers to O for
/// interoperation with C/C++ APIs.
///
/// Note that pooled objects are not clonable. If you want to share them use Rc<>
/// or Arc<>.
#[repr(transparent)]
pub struct Pooled<O, F: PoolFactory<O>>(*mut PoolEntry<O, F>);

impl<O, F: PoolFactory<O>> Pooled<O, F> {
    /// Get a raw pointer to the object wrapped by this pooled object container.
    /// The returned raw pointer MUST be restored into a Pooled instance with
    /// from_raw() or memory will leak.
    #[inline(always)]
    pub unsafe fn into_raw(self) -> *mut O {
        debug_assert!(!self.0.is_null());
        debug_assert_eq!(self.0.cast::<u8>(), (&mut (*self.0).obj as *mut O).cast::<u8>());
        let ptr = self.0.cast::<O>();
        std::mem::forget(self);
        ptr
    }

    /// Restore a raw pointer from into_raw() into a Pooled object.
    /// The supplied pointer MUST have been obtained from a Pooled object or
    /// undefined behavior will occur. Pointers from other sources can't be used
    /// here. None is returned if the pointer is null.
    #[inline(always)]
    pub unsafe fn from_raw(raw: *mut O) -> Option<Self> {
        if !raw.is_null() {
            Some(Self(raw.cast()))
        } else {
            None
        }
    }
}

impl<O, F: PoolFactory<O>> Deref for Pooled<O, F> {
    type Target = O;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        debug_assert!(!self.0.is_null());
        unsafe { &(*self.0).obj }
    }
}

impl<O, F: PoolFactory<O>> AsRef<O> for Pooled<O, F> {
    #[inline(always)]
    fn as_ref(&self) -> &O {
        debug_assert!(!self.0.is_null());
        unsafe { &(*self.0).obj }
    }
}

impl<O, F: PoolFactory<O>> DerefMut for Pooled<O, F> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut Self::Target {
        debug_assert!(!self.0.is_null());
        unsafe { &mut (*self.0).obj }
    }
}

impl<O, F: PoolFactory<O>> AsMut<O> for Pooled<O, F> {
    #[inline(always)]
    fn as_mut(&mut self) -> &mut O {
        debug_assert!(!self.0.is_null());
        unsafe { &mut (*self.0).obj }
    }
}

impl<O, F: PoolFactory<O>> Drop for Pooled<O, F> {
    fn drop(&mut self) {
        unsafe {
            Weak::upgrade(&(*self.0).return_pool).map_or_else(|| {
                drop(Box::from_raw(self.0))
            }, |p| {
                p.0.reset(&mut (*self.0).obj);
                p.1.lock().push(self.0)
            })
        }
    }
}

/// An object pool for Reusable objects.
/// Checked out objects are held by a guard object that returns them when dropped if
/// the pool still exists or drops them if the pool has itself been dropped.
pub struct Pool<O, F: PoolFactory<O>>(Arc<PoolInner<O, F>>);

impl<O, F: PoolFactory<O>> Pool<O, F> {
    pub fn new(initial_stack_capacity: usize, factory: F) -> Self {
        Self(Arc::new(PoolInner::<O, F>(factory, Mutex::new(Vec::with_capacity(initial_stack_capacity)))))
    }

    /// Get a pooled object, or allocate one if the pool is empty.
    pub fn get(&self) -> Pooled<O, F> {
        Pooled::<O, F>(self.0.1.lock().pop().map_or_else(|| {
            Box::into_raw(Box::new(PoolEntry::<O, F> {
                obj: self.0.0.create(),
                return_pool: Arc::downgrade(&self.0),
            }))
        }, |obj| {
            debug_assert!(!obj.is_null());
            obj
        }))
    }

    /// Get approximate memory use in bytes (does not include checked out objects).
    #[inline(always)]
    pub fn pool_memory_bytes(&self) -> usize {
        self.0.1.lock().len() * (size_of::<PoolEntry<O, F>>() + size_of::<usize>())
    }

    /// Dispose of all pooled objects, freeing any memory they use.
    /// If get() is called after this new objects will be allocated, and any outstanding
    /// objects will still be returned on drop unless the pool itself is dropped. This can
    /// be done to free some memory if there has been a spike in memory use.
    pub fn purge(&self) {
        let mut p = self.0.1.lock();
        for obj in p.iter() {
            drop(unsafe { Box::from_raw(*obj) });
        }
        p.clear();
    }
}

impl<O, F: PoolFactory<O>> Drop for Pool<O, F> {
    fn drop(&mut self) {
        self.purge();
    }
}

unsafe impl<O, F: PoolFactory<O>> Sync for Pool<O, F> {}

unsafe impl<O, F: PoolFactory<O>> Send for Pool<O, F> {}

#[cfg(test)]
mod tests {
    use std::ops::DerefMut;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use crate::util::pool::*;

    struct TestPoolFactory;

    impl PoolFactory<String> for TestPoolFactory {
        fn create(&self) -> String {
            String::new()
        }

        fn reset(&self, obj: &mut String) {
            obj.clear();
        }
    }

    #[test]
    fn threaded_pool_use() {
        let p: Arc<Pool<String, TestPoolFactory>> = Arc::new(Pool::new(2, TestPoolFactory{}));
        let ctr = Arc::new(AtomicUsize::new(0));
        for _ in 0..64 {
            let p2 = p.clone();
            let ctr2 = ctr.clone();
            let _ = std::thread::spawn(move || {
                for _ in 0..16384 {
                    let mut o1 = p2.get();
                    o1.push('a');
                    let mut o2 = p2.get();
                    drop(o1);
                    o2.push('b');
                    ctr2.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
        loop {
            std::thread::sleep(Duration::from_millis(100));
            if ctr.load(Ordering::Relaxed) >= 16384 * 64 {
                break;
            }
        }
        //println!("pool memory size: {}", p.pool_memory_bytes());
    }
}
