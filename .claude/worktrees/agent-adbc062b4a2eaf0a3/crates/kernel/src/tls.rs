use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

pub struct TlsKeys {
    next: AtomicU32,
    destructors: Mutex<Vec<Option<u64>>>,
}

impl TlsKeys {
    pub fn new() -> Self {
        Self {
            next: AtomicU32::new(0),
            destructors: Mutex::new(Vec::new()),
        }
    }

    pub fn create_key(&self, dtor: u64) -> u32 {
        let key = self.next.fetch_add(1, Ordering::Relaxed);
        let mut vec = self.destructors.lock().unwrap();
        if key as usize >= vec.len() {
            vec.resize(key as usize + 1, None);
        }
        vec[key as usize] = if dtor == 0 { None } else { Some(dtor) };
        key
    }

    pub fn get_dtor(&self, key: u32) -> Option<u64> {
        let vec = self.destructors.lock().unwrap();
        vec.get(key as usize).and_then(|x| *x)
    }

    pub fn max_key(&self) -> usize {
        self.destructors.lock().unwrap().len()
    }
}
