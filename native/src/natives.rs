//! Raw JNI natives bound to the shim class (RegisterNatives).

use crate::engine::{self, Registry, ACTIVE};
use crate::store::Store;
use jvmti_bindings::prelude::*;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

pub static REGISTRY: OnceLock<Arc<Registry>> = OnceLock::new();

fn registry() -> &'static Arc<Registry> {
    REGISTRY.get_or_init(|| Arc::new(Registry::new()))
}

fn key(cx: i32, cz: i32) -> u64 {
    ((cx as u64) << 32) | (cz as u64 & 0xffff_ffff)
}

unsafe fn jstr_to_string(env: *mut jni_sys::JNIEnv, s: jni_sys::jstring) -> String {
    if s.is_null() {
        return String::new();
    }
    let get = (**env).v1_1.GetStringUTFChars;
    let ptr = get(env, s, std::ptr::null_mut());
    if ptr.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    while *ptr.add(len) != 0 {
        len += 1;
    }
    let bytes = std::slice::from_raw_parts(ptr as *const u8, len);
    let out = String::from_utf8_lossy(bytes).into_owned();
    let rel = (**env).v1_1.ReleaseStringUTFChars;
    rel(env, s, ptr);
    out
}

unsafe fn byte_array_from_jni(env: *mut jni_sys::JNIEnv, arr: jni_sys::jbyteArray) -> Vec<u8> {
    if arr.is_null() {
        return Vec::new();
    }
    let len = ((**env).v1_1.GetArrayLength)(env, arr as jni_sys::jarray) as usize;
    let mut out = vec![0u8; len];
    if len > 0 {
        let crit = (**env).v1_2.GetPrimitiveArrayCritical;
        let p = crit(env, arr as jni_sys::jarray, std::ptr::null_mut());
        if !p.is_null() {
            std::ptr::copy_nonoverlapping(p as *const u8, out.as_mut_ptr(), len);
            let rel = (**env).v1_2.ReleasePrimitiveArrayCritical;
            rel(env, arr as jni_sys::jarray, p, jni_sys::JNI_OK as i32);
        }
    }
    out
}

unsafe fn vec_to_byte_array(env: *mut jni_sys::JNIEnv, data: &[u8]) -> jni_sys::jbyteArray {
    let new = (**env).v1_1.NewByteArray;
    let arr = new(env, data.len() as i32);
    if arr.is_null() {
        return std::ptr::null_mut();
    }
    if !data.is_empty() {
        let set = (**env).v1_1.SetByteArrayRegion;
        let signed = std::slice::from_raw_parts(data.as_ptr() as *const i8, data.len());
        set(env, arr, 0, data.len() as i32, signed.as_ptr());
    }
    arr
}

// ---------------------------------------------------------------------------
// exports (bound via RegisterNatives; names are the Java-side method names)
// ---------------------------------------------------------------------------

pub type JniEnvPtr = *mut jni_sys::JNIEnv;

#[no_mangle]
pub extern "system" fn n_active(_env: JniEnvPtr, _cls: jni_sys::jclass) -> jni_sys::jboolean {
    ACTIVE.load(std::sync::atomic::Ordering::Acquire) as jni_sys::jboolean
}

#[no_mangle]
pub extern "system" fn n_attach(
    env: JniEnvPtr,
    _cls: jni_sys::jclass,
    kind: jni_sys::jstring,
    level: jni_sys::jstring,
    dir: jni_sys::jstring,
) -> jni_sys::jint {
    unsafe {
        let kind = jstr_to_string(env, kind);
        let level = jstr_to_string(env, level);
        let dir = jstr_to_string(env, dir);
        let idx = registry().attach(&kind, &level, &dir) as i32;
        ACTIVE.store(true, std::sync::atomic::Ordering::Release);
        idx
    }
}

#[no_mangle]
pub extern "system" fn n_read_chunk(
    env: JniEnvPtr,
    _cls: jni_sys::jclass,
    handle: i32,
    x: i32,
    z: i32,
) -> jni_sys::jbyteArray {
    unsafe {
        let Some(entry) = registry().get(handle as usize) else {
            return std::ptr::null_mut();
        };
        let t0 = Instant::now();
        let data = entry.0.read_chunk(key(x, z));
        let us = t0.elapsed().as_micros() as u64;
        engine::note_read_latency(us);
        match data {
            Some(d) => vec_to_byte_array(env, &d),
            None => std::ptr::null_mut(),
        }
    }
}

#[no_mangle]
pub extern "system" fn n_write_chunk(
    env: JniEnvPtr,
    _cls: jni_sys::jclass,
    handle: i32,
    x: i32,
    z: i32,
    data: jni_sys::jbyteArray,
) -> jni_sys::jboolean {
    unsafe {
        let Some(entry) = registry().get(handle as usize) else {
            return false;
        };
        let bytes = byte_array_from_jni(env, data);
        if bytes.is_empty() {
            return false;
        }
        let t0 = Instant::now();
        let r = entry.0.write_chunk(key(x, z), &bytes, entry.1);
        let us = t0.elapsed().as_micros() as u64;
        engine::note_write_latency(us);
        r.is_ok() as jni_sys::jboolean
    }
}

#[no_mangle]
pub extern "system" fn n_begin_save(_env: JniEnvPtr, _cls: jni_sys::jclass) -> jni_sys::jboolean {
    // append-only writes are already durable to the OS page cache; end_save
    // performs the fsync barrier.
    true as jni_sys::jboolean
}

#[no_mangle]
pub extern "system" fn n_end_save(_env: JniEnvPtr, _cls: jni_sys::jclass) -> jni_sys::jboolean {
    registry().sync_all();
    true as jni_sys::jboolean
}

#[no_mangle]
pub extern "system" fn n_mark_autosave(_env: JniEnvPtr, _cls: jni_sys::jclass) {
    engine::note_autosave();
}

/// Bind all natives to a defined shim class (called from shim.rs).
pub fn register_all(env: &JniEnv, cls: jni::jclass) -> bool {
    crate::shim::register_natives_raw(env, cls)
}

/// Public for tests / maintenance binaries.
pub fn store_for(idx: usize) -> Option<Arc<Store>> {
    registry().get(idx).map(|e| e.0)
}
