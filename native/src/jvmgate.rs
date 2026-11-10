//! JVM gate bridge (v0.9.61): standard-JNI exports so a REAL JVM can drive
//! the production natives without the CRUSSTY kernel.
//!
//! The product path is `cplugin_init` → shim define_and_bind → RegisterNatives
//! (JVM TI). That path needs the kernel; a plain `java` process has no way to
//! call the natives. This module closes the gap WITHOUT duplicating logic:
//! every export below is a one-line pass-through into `natives::n_*` — the
//! same registry, the same store, the same freshness channel the shim binds.
//! It exists so CI can prove the Java side of the integration (byte[]
//! marshalling both ways, null = NO_DATA, restart into a fresh JVM process)
//! against the real engine on a real JVM.
//!
//! Java side: `dev.crussty.saves.jvmgate.JvmGate` (java/ in the repo root),
//! loaded with System.load; the JVM resolves these Java_* symbols itself.

use crate::natives;
use jni_sys::{jboolean, jbyteArray, jclass, jint, JNIEnv, jstring};

// Each wrapper mirrors the exact JNI signature of its natives::n_* twin.

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_crussty_saves_jvmgate_JvmGate_nActive(
    env: *mut JNIEnv,
    cls: jclass,
) -> jboolean {
    natives::n_active(env, cls)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_crussty_saves_jvmgate_JvmGate_nAttach(
    env: *mut JNIEnv,
    cls: jclass,
    kind: jstring,
    level: jstring,
    dir: jstring,
) -> jint {
    natives::n_attach(env, cls, kind, level, dir)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_crussty_saves_jvmgate_JvmGate_nReadChunk(
    env: *mut JNIEnv,
    cls: jclass,
    handle: jint,
    x: jint,
    z: jint,
) -> jbyteArray {
    natives::n_read_chunk(env, cls, handle, x, z)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_crussty_saves_jvmgate_JvmGate_nWriteChunk(
    env: *mut JNIEnv,
    cls: jclass,
    handle: jint,
    x: jint,
    z: jint,
    data: jbyteArray,
    fresh: jboolean,
) -> jboolean {
    natives::n_write_chunk(env, cls, handle, x, z, data, fresh)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_crussty_saves_jvmgate_JvmGate_nBeginSave(
    env: *mut JNIEnv,
    cls: jclass,
) -> jboolean {
    natives::n_begin_save(env, cls)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_crussty_saves_jvmgate_JvmGate_nEndSave(
    env: *mut JNIEnv,
    cls: jclass,
) -> jboolean {
    natives::n_end_save(env, cls)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_crussty_saves_jvmgate_JvmGate_nMarkAutosave(
    env: *mut JNIEnv,
    cls: jclass,
) {
    natives::n_mark_autosave(env, cls)
}
