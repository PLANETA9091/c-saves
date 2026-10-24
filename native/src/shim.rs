//! Shim class embedding: rename to a per-.so unique binary name, define into
//! the kernel classloader, bind natives via RegisterNatives.

use crate::natives;
use jvmti_bindings::prelude::*;
use std::ffi::CString;
use std::sync::OnceLock;

const SHIM_ORIG: &str = "dev/crussty/saves/SavesShim";
const NOOP_ORIG: &str = "dev/crussty/saves/SavesShimNoop";
const SHIM_BYTES: &[u8] = include_bytes!("shim/SavesShim.class");
const NOOP_BYTES: &[u8] = include_bytes!("shim/SavesShimNoop.class");

static SHIM_NAME: OnceLock<String> = OnceLock::new();
pub static SHIM_DEFINED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Unique per-.so shim binary name (address of a private static, mirrors the
/// cplug-sdk pattern; a folded constant would collide across modules).
pub fn shim_name() -> &'static str {
    SHIM_NAME.get_or_init(|| {
        static SENTINEL: u8 = 0;
        let addr = &SENTINEL as *const u8 as usize;
        format!("{SHIM_ORIG}{addr:x}")
    })
}

fn u16at(b: &[u8], p: usize) -> u16 {
    u16::from_be_bytes([b[p], b[p + 1]])
}

/// Rename CP Utf8 entries carrying the original class names (bare + `L..;`
/// descriptor form). All CP references are by index, so everything else is
/// copied verbatim.
fn rename_class(src: &[u8], pairs: &[(&str, String)]) -> Option<Vec<u8>> {
    if src.len() < 10 || u32::from_be_bytes(src[0..4].try_into().ok()?) != 0xCAFE_BABE {
        return None;
    }
    let cp_count = u16at(src, 8) as usize;
    let mut p = 10usize;
    let mut out = Vec::with_capacity(src.len() + 64);
    out.extend_from_slice(&src[..10]);
    let mut idx = 1usize;
    while idx < cp_count {
        let start = p;
        let tag = src[p];
        p += 1;
        match tag {
            1 => {
                let l = u16at(src, p) as usize;
                p += 2;
                let payload = &src[p..p + l];
                p += l;
                let txt = std::str::from_utf8(payload).ok();
                let mut replaced = None;
                if let Some(t) = txt {
                    for (orig, new) in pairs {
                        if t == *orig {
                            replaced = Some(new.clone());
                            break;
                        }
                        let d = format!("L{orig};");
                        if t == d {
                            replaced = Some(format!("L{new};"));
                            break;
                        }
                    }
                }
                match replaced {
                    Some(n) => {
                        out.push(1);
                        out.extend_from_slice(&(n.len() as u16).to_be_bytes());
                        out.extend_from_slice(n.as_bytes());
                    }
                    None => out.extend_from_slice(&src[start..p]),
                }
            }
            7 | 8 | 16 | 19 | 20 => {
                p += 2;
                out.extend_from_slice(&src[start..p]);
            }
            9 | 10 | 11 | 12 | 17 | 18 => {
                p += 4;
                out.extend_from_slice(&src[start..p]);
            }
            15 => {
                p += 3;
                out.extend_from_slice(&src[start..p]);
            }
            3 | 4 => {
                p += 4;
                out.extend_from_slice(&src[start..p]);
            }
            5 | 6 => {
                p += 8;
                out.extend_from_slice(&src[start..p]);
                idx += 1;
            }
            _ => return None,
        }
        idx += 1;
    }
    out.extend_from_slice(&src[p..]);
    Some(out)
}

/// Define the (renamed) shim classes into `loader` and RegisterNatives.
/// Returns true on success.
pub fn define_and_bind(env: &JniEnv, loader: jni::jobject) -> bool {
    let name = shim_name().to_string();
    let noop_name = format!("{name}Noop");
    let pairs = [
        (SHIM_ORIG, name.clone()),
        (NOOP_ORIG, noop_name.clone()),
    ];
    let Some(noop_b) = rename_class(NOOP_BYTES, &pairs) else {
        eprintln!("[c-saves] noop class rename failed");
        return false;
    };
    let Some(shim_b) = rename_class(SHIM_BYTES, &pairs) else {
        eprintln!("[c-saves] shim class rename failed");
        return false;
    };
    let Some(_noop_cls) = env.define_class(&noop_name, loader, &noop_b) else {
        if env.exception_check() {
            env.exception_describe();
            env.exception_clear();
        }
        eprintln!("[c-saves] define_class({noop_name}) failed");
        return false;
    };
    let Some(shim_cls) = env.define_class(&name, loader, &shim_b) else {
        if env.exception_check() {
            env.exception_describe();
            env.exception_clear();
        }
        eprintln!("[c-saves] define_class({name}) failed");
        return false;
    };
    if !natives::register_all(env, shim_cls) {
        eprintln!("[c-saves] RegisterNatives failed");
        return false;
    }
    SHIM_DEFINED.store(true, std::sync::atomic::Ordering::SeqCst);
    eprintln!("[c-saves] shim class defined: {name} (natives bound)");
    true
}

/// RegisterNatives table (names are Java method names on the shim).
pub fn register_natives_raw(env: &JniEnv, cls: jni::jclass) -> bool {
    // jvmti-bindings' sys::jni types are C-layout identical to jni-sys;
    // cross-crate nominal pointers are bridged through c_void casts.
    let cls_raw = cls as *mut std::ffi::c_void as jni_sys::jclass;
    let entries: &[(&str, &str, *mut std::ffi::c_void)] = &[
        ("nActive", "()Z", natives::n_active as *mut _),
        ("nAttach", "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)I", natives::n_attach as *mut _),
        ("nReadChunk", "(III)[B", natives::n_read_chunk as *mut _),
        ("nWriteChunk", "(III[B)Z", natives::n_write_chunk as *mut _),
        ("nBeginSave", "()Z", natives::n_begin_save as *mut _),
        ("nEndSave", "()Z", natives::n_end_save as *mut _),
        ("nMarkAutosave", "()V", natives::n_mark_autosave as *mut _),
    ];
    let mut methods: Vec<jni_sys::JNINativeMethod> = Vec::with_capacity(entries.len());
    let mut cnames = Vec::new();
    let mut csigs = Vec::new();
    for (n, s, f) in entries {
        cnames.push(CString::new(*n).unwrap());
        csigs.push(CString::new(*s).unwrap());
        methods.push(jni_sys::JNINativeMethod {
            name: cnames.last().unwrap().as_ptr() as *mut i8,
            signature: csigs.last().unwrap().as_ptr() as *mut i8,
            fnPtr: *f,
        });
    }
    let env_ptr = env.raw() as *mut jni_sys::JNIEnv;
    unsafe {
        let rn = (**env_ptr).v1_1.RegisterNatives;
        let rc = (rn)(env_ptr, cls_raw, methods.as_ptr(), entries.len() as i32);
        if rc != jni_sys::JNI_OK {
            if env.exception_check() {
                env.exception_describe();
                env.exception_clear();
            }
            return false;
        }
    }
    true
}
