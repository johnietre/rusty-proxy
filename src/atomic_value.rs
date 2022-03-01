// A basic (and bad) implementation of Go's atomic.Value
// TODO: Do Orderings
use std::sync::{
    atomic::{AtomicPtr, Ordering},
    Arc,
};

#[derive(Clone)]
pub struct AtomicValue<T>(Arc<AtomicPtr<Arc<T>>>);

impl<T> AtomicValue<T> {
    pub fn new(val: T) -> Self {
        let p = Box::into_raw(Box::new(Arc::new(val)));
        Self(Arc::new(AtomicPtr::new(p)))
    }

    pub fn load(&self) -> Option<Arc<T>> {
        let p = self.0.load(Ordering::Relaxed);
        if p.is_null() {
            None
        } else {
            unsafe { Some((*p).clone()) }
        }
    }

    pub fn store(&self, val: T) {
        let old = self
            .0
            .swap(Box::into_raw(Box::new(Arc::new(val))), Ordering::Relaxed);
        if !old.is_null() {
            unsafe {
                Box::from_raw(old);
            }
        }
    }
}

impl<T> Drop for AtomicValue<T> {
    fn drop(&mut self) {
        let p = self.0.load(Ordering::Relaxed);
        if !p.is_null() {
            unsafe {
                Box::from_raw(p);
            }
        }
    }
}

pub struct AtomicValueUnchecked<T>(Arc<Value<T>>);

impl<T> AtomicValueUnchecked<T> {
    pub fn new(val: T) -> Self {
        Self(Arc::new(Value::new(val)))
    }

    pub fn load(&self) -> Arc<T> {
        unsafe { (*self.0 .0.load(Ordering::Relaxed)).clone() }
    }

    pub fn store(&self, val: T) {
        unsafe {
            Box::from_raw(
                self.0
                     .0
                    .swap(Box::into_raw(Box::new(Arc::new(val))), Ordering::Relaxed),
            );
        }
    }
}

impl<T> Clone for AtomicValueUnchecked<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

pub(crate) struct Value<V>(pub(crate) AtomicPtr<Arc<V>>);

impl<V> Value<V> {
    pub(crate) fn new(val: V) -> Self {
        Self(AtomicPtr::new(Box::into_raw(Box::new(Arc::new(val)))))
    }

    pub(crate) fn from_ptr(ptr: *mut Arc<V>) -> Self {
        Self(AtomicPtr::new(ptr))
    }
}

impl<V> Drop for Value<V> {
    fn drop(&mut self) {
        let p = self.0.swap(std::ptr::null_mut(), Ordering::Relaxed);
        if !p.is_null() {
            unsafe {
                Box::from_raw(p);
            }
        }
    }
}
