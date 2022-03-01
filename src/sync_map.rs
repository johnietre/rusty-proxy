// An basic (and bad) implementation of Go's sync.Map
// TODO: Do Orderings
// TODO: Comments
use std::collections::HashMap;
use std::mem;
use std::ptr::null_mut;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

use crate::atomic_value::{AtomicValueUnchecked as AVU, Value};

const EXPUNGED: *const u8 = &0u8 as *const u8;

fn expunged<T>() -> *mut T {
    EXPUNGED as usize as *mut T
}

fn is_expunged<T>(ptr: *mut T) -> bool {
    ptr as usize == EXPUNGED as usize
}

pub struct SyncMap<V> {
    read: AVU<ReadOnly<V>>,
    dirty: Arc<Mutex<HashMap<Arc<String>, Entry<V>>>>,
    misses: Arc<AtomicUsize>,
}

impl<V> SyncMap<V> {
    pub fn new() -> Self {
        Self {
            read: AVU::new(ReadOnly {
                m: HashMap::new(),
                amended: false,
            }),
            dirty: Arc::new(Mutex::new(HashMap::new())),
            misses: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn load(&self, key: &String) -> Option<Arc<V>> {
        self.status_check("load");
        let read = self.read.load();
        read.m
            .get(key)
            .cloned()
            .or_else(|| {
                if read.amended {
                    let mut dirty = self.dirty.lock().unwrap();
                    let read = self.read.load();
                    read.m.get(key).cloned().or_else(|| {
                        if read.amended {
                            let e = dirty.get(key).cloned();
                            // function missLocked
                            let misses = self.misses.fetch_add(1, Ordering::SeqCst) + 1;
                            if misses >= dirty.len() {
                                let m = mem::replace(&mut *dirty, HashMap::new());
                                self.read.store(ReadOnly { m, amended: false });
                                self.misses.store(0, Ordering::Relaxed);
                            }
                            // end function missLocked
                            e
                        } else {
                            None
                        }
                    })
                } else {
                    None
                }
            })
            .and_then(|e| e.load())
    }

    pub fn store(&self, key: String, value: V) {
        self.status_check("store");
        let value_ptr = Box::into_raw(Box::new(Arc::new(value)));
        if let Some(e) = self.read.load().m.get(&key) {
            if e.try_store(value_ptr) {
                return;
            }
        }
        let mut dirty = self.dirty.lock().unwrap();
        let read = self.read.load();
        if let Some(e) = read.m.get(&key) {
            if e.unexpunge_locked() {
                dirty.insert(Arc::new(key), e.clone());
            }
            e.store_locked(value_ptr);
        } else if let Some(e) = dirty.get(&key) {
            e.store_locked(value_ptr);
        } else {
            if !read.amended {
                // function dirtyLocked
                if dirty.is_empty() {
                    *dirty = self
                        .read
                        .load()
                        .m
                        .iter()
                        .filter(|(_, e)| !e.try_expunge_locked())
                        .map(|(k, e)| (k.clone(), e.clone()))
                        .collect();
                }
                // end function dirtyLocked
                self.read.store(ReadOnly {
                    m: read.m.clone(),
                    amended: true,
                });
            }
            dirty.insert(Arc::new(key), Entry::from_ptr(value_ptr));
        }
    }

    pub fn load_or_store(&self, key: String, value: V) -> Option<Arc<V>> {
        self.status_check("load_or_store");
        let value_ptr = Box::into_raw(Box::new(Arc::new(value)));
        if let Some(e) = self.read.load().m.get(&key) {
            match e.try_load_or_store(value_ptr) {
                (Some(v), true) => {
                    unsafe {
                        Box::from_raw(value_ptr);
                    }
                    return Some(v);
                }
                (None, true) => return None,
                _ => (),
            }
        }
        let mut dirty = self.dirty.lock().unwrap();
        let read = self.read.load();
        if let Some(e) = read.m.get(&key) {
            if e.unexpunge_locked() {
                dirty.insert(Arc::new(key), e.clone());
            }
            let v = e.try_load_or_store(value_ptr).0;
            if v.is_some() {
                unsafe {
                    Box::from_raw(value_ptr);
                }
            }
            v
        } else if let Some(e) = dirty.get(&key) {
            let v = e.try_load_or_store(value_ptr).0;
            // function missLocked
            let misses = self.misses.fetch_add(1, Ordering::SeqCst) + 1;
            if misses >= dirty.len() {
                let m = mem::replace(&mut *dirty, HashMap::new());
                self.read.store(ReadOnly {
                    m: m,
                    amended: false,
                });
                self.misses.store(0, Ordering::Relaxed);
            }
            // end function missLocked
            if v.is_some() {
                unsafe {
                    Box::from_raw(value_ptr);
                }
            }
            v
        } else {
            if !read.amended {
                // function dirtyLocked
                if dirty.is_empty() {
                    *dirty = self
                        .read
                        .load()
                        .m
                        .iter()
                        .filter(|(_, e)| !e.try_expunge_locked())
                        .map(|(k, e)| (k.clone(), e.clone()))
                        .collect();
                }
                // end function dirtyLocked
                self.read.store(ReadOnly {
                    m: read.m.clone(),
                    amended: true,
                });
            }
            dirty.insert(Arc::new(key), Entry::from_ptr(value_ptr));
            unsafe { Some((*value_ptr).clone()) }
        }
    }

    pub fn load_and_delete(&self, key: &String) -> Option<Arc<V>> {
        self.status_check("load_and_delete");
        let read = self.read.load();
        read.m
            .get(key)
            .cloned()
            .or_else(|| {
                if read.amended {
                    let mut dirty = self.dirty.lock().unwrap();
                    let read = self.read.load();
                    read.m.get(key).cloned().or_else(|| {
                        if read.amended {
                            let e = dirty.remove(key);
                            // function missLocked
                            let misses = self.misses.fetch_add(1, Ordering::SeqCst) + 1;
                            if misses >= dirty.len() {
                                let m = mem::replace(&mut *dirty, HashMap::new());
                                self.read.store(ReadOnly {
                                    m: m,
                                    amended: false,
                                });
                                self.misses.store(0, Ordering::Relaxed);
                            }
                            // end function missLocked
                            e
                        } else {
                            None
                        }
                    })
                } else {
                    None
                }
            })
            .and_then(|e| e.delete())
    }

    pub fn delete(&self, key: &String) {
        self.status_check("delete");
        self.load_and_delete(key);
    }

    pub fn keys(&self) -> Vec<Arc<String>> {
        let mut read = self.read.load();
        if read.amended {
            let mut dirty = self.dirty.lock().unwrap();
            read = self.read.load();
            if read.amended {
                let m = mem::replace(&mut *dirty, HashMap::new());
                self.read.store(ReadOnly { m, amended: false });
                read = self.read.load();
                self.misses.store(0, Ordering::Relaxed);
            }
        }
        read.m
            .iter()
            .filter_map(|(k, e)| e.load().map(|_| Arc::clone(k)))
            .collect()
    }

    pub fn values(&self) -> Vec<Arc<V>> {
        let mut read = self.read.load();
        if read.amended {
            let mut dirty = self.dirty.lock().unwrap();
            read = self.read.load();
            if read.amended {
                let m = mem::replace(&mut *dirty, HashMap::new());
                self.read.store(ReadOnly { m, amended: false });
                read = self.read.load();
                self.misses.store(0, Ordering::Relaxed);
            }
        }
        read.m.values().filter_map(|e| e.load()).collect()
    }

    pub fn key_values(&self) -> Vec<(Arc<String>, Arc<V>)> {
        let mut read = self.read.load();
        if read.amended {
            let mut dirty = self.dirty.lock().unwrap();
            read = self.read.load();
            if read.amended {
                let m = mem::replace(&mut *dirty, HashMap::new());
                self.read.store(ReadOnly { m, amended: false });
                read = self.read.load();
                self.misses.store(0, Ordering::Relaxed);
            }
        }
        read.m
            .iter()
            .filter_map(|(k, e)| e.load().map(|v| (Arc::clone(k), v)))
            .collect()
    }

    #[allow(unreachable_code)]
    #[allow(unused_variables)]
    fn status_check(&self, what: &str) {
        return;
        let r = self.read.load();
        let d = self.dirty.lock().unwrap();
        let m = self.misses.load(Ordering::Relaxed);
        println!(
            "{} => read.m.len: {}, read.amended: {}, dirty.len: {}, misses: {}",
            what,
            r.m.len(),
            r.amended,
            d.len(),
            m
        );
    }
}

impl<V> Clone for SyncMap<V> {
    fn clone(&self) -> Self {
        Self {
            read: self.read.clone(),
            dirty: self.dirty.clone(),
            misses: self.misses.clone(),
        }
    }
}

struct ReadOnly<V> {
    m: HashMap<Arc<String>, Entry<V>>,
    amended: bool,
}

struct Entry<V> {
    p: Arc<Value<V>>,
}

impl<V> Entry<V> {
    fn from_ptr(ptr: *mut Arc<V>) -> Self {
        Self {
            p: Arc::new(Value::from_ptr(ptr)),
        }
    }

    fn load(&self) -> Option<Arc<V>> {
        let p = self.p.0.load(Ordering::Relaxed);
        if p.is_null() || is_expunged(p) {
            None
        } else {
            unsafe { Some((*p).clone()) }
        }
    }

    fn try_store(&self, ptr: *mut Arc<V>) -> bool {
        loop {
            let p = self.p.0.load(Ordering::Relaxed);
            if is_expunged(p) {
                break false;
            }
            let p = self
                .p
                .0
                .compare_exchange(p, ptr, Ordering::SeqCst, Ordering::Relaxed);
            if let Ok(p) = p {
                if !p.is_null() {
                    unsafe {
                        Box::from_raw(p);
                    }
                }
                break true;
            }
        }
    }

    // Returns if it was unexpunged
    fn unexpunge_locked(&self) -> bool {
        self.p
            .0
            .compare_exchange(expunged(), null_mut(), Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
    }

    fn store_locked(&self, ptr: *mut Arc<V>) {
        //self.p.0.store(ptr, Ordering::Relaxed);
        let p = self.p.0.swap(ptr, Ordering::Relaxed);
        if !p.is_null() && !is_expunged(p) {
            unsafe {
                Box::from_raw(p);
            }
        }
    }

    // Returns if it was expunged
    fn try_expunge_locked(&self) -> bool {
        let mut p = self.p.0.load(Ordering::Relaxed);
        while p.is_null() {
            let s = self
                .p
                .0
                .compare_exchange(null_mut(), expunged(), Ordering::SeqCst, Ordering::Relaxed)
                .is_ok();
            if s {
                return true;
            }
            p = self.p.0.load(Ordering::Relaxed);
        }
        is_expunged(p)
    }

    // Returns None, false if the operation failed somehow, None true if there
    // was no value loaded (new stored), and Some(v) true if the value was
    // loaded
    fn try_load_or_store(&self, ptr: *mut Arc<V>) -> (Option<Arc<V>>, bool) {
        unsafe {
            let p = self.p.0.load(Ordering::Relaxed);
            if is_expunged(p) {
                return (None, false);
            } else if !p.is_null() {
                return (Some((*p).clone()), true);
            }
            loop {
                let s = self
                    .p
                    .0
                    .compare_exchange(null_mut(), ptr, Ordering::SeqCst, Ordering::Relaxed)
                    .is_ok();
                if s {
                    break (None, true);
                }
                let p = self.p.0.load(Ordering::Relaxed);
                if is_expunged(p) {
                    break (None, false);
                } else if !p.is_null() {
                    break (Some((*p).clone()), true);
                }
            }
        }
    }

    fn delete(&self) -> Option<Arc<V>> {
        loop {
            let p = self.p.0.load(Ordering::Relaxed);
            if p.is_null() || is_expunged(p) {
                break None;
            }
            let s = self
                .p
                .0
                .compare_exchange(p, null_mut(), Ordering::SeqCst, Ordering::Relaxed)
                .is_ok();
            if s {
                unsafe {
                    break Some((*Box::from_raw(p)).clone());
                }
            }
        }
    }
}

impl<V> Clone for Entry<V> {
    fn clone(&self) -> Self {
        Self { p: self.p.clone() }
    }
}
