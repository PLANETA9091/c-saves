//! c-saves — CRUSSTY module: Rust-native world storage for Paper 26.3.
//!
//! Boot flow (cplugin_init, runs before the kernel starts):
//!   1. register byte hooks (pure-Rust save-cycle probes via weave, ASM
//!      data-path patches for RegionFileStorage via the SDK helper),
//!   2. spawn the shim-setup thread: once RegionFileStorage loads, define the
//!      bridge class into its loader, bind natives (RegisterNatives), define
//!      the ASM helper, then retransform RegionFileStorage so the byte hook
//!      replaces five method bodies with bridges into the engine.

use cplug_abi::{CPluginApi, JavaVmPtr};
use jvmti_bindings::prelude::*;
use std::sync::atomic::Ordering;

pub mod anvil;
mod anvil_import;
pub mod chunknbt;
mod engine;
mod natives;
mod shim;
pub mod store;

const RFS: &str = "net/minecraft/world/level/chunk/storage/RegionFileStorage";
const MCS: &str = "net/minecraft/server/MinecraftServer";
const MRIO: &str = "ca/spottedleaf/moonrise/patches/chunk_system/io/MoonriseRegionFileIO";

const READ_DATA: &str = "ca/spottedleaf/moonrise/patches/chunk_system/io/MoonriseRegionFileIO$RegionDataController$ReadData";
const WRITE_DATA: &str = "ca/spottedleaf/moonrise/patches/chunk_system/io/MoonriseRegionFileIO$RegionDataController$WriteData";

/// Apply the pure-Rust save-cycle probes to a class's bytes.
fn patch_save_cycle(name: &str, bytes: Vec<u8>) -> Option<Vec<u8>> {
    let mut b = bytes;
    let mut patched = 0usize;
    let target_class = shim::shim_name(); // internal form
    if name == MCS {
        for (m, d, h) in [
            ("saveAllChunks", "(ZZZ)Z", "onSaveStart"),
            ("saveAllChunks", "(ZZZZ)Z", "onSaveStart"),
            ("autoSave", "()V", "onAutosave"),
        ] {
            let attempt = std::mem::take(&mut b);
            match cplug_sdk::weave::insert_call_at_start(
                attempt, m, d, target_class, h, "()V", None,
            ) {
                Ok(n) => {
                    b = n;
                    patched += 1;
                }
                Err(e) => eprintln!("[c-saves] {name}.{m}{d}: insert failed: {e}"),
            }
        }
    } else if name == MRIO {
        for (m, d) in [
            ("flush", "(Lnet/minecraft/server/MinecraftServer;)V"),
            ("flush", "(Lnet/minecraft/server/level/ServerLevel;)V"),
        ] {
            let attempt = std::mem::take(&mut b);
            match cplug_sdk::weave::insert_call_at_start(
                attempt, m, d, target_class, "onSaveEnd", "()V", None,
            ) {
                Ok(n) => {
                    b = n;
                    patched += 1;
                }
                Err(e) => eprintln!("[c-saves] MRIO.{m}: insert failed: {e}"),
            }
        }
    } else {
        return None;
    }
    if patched > 0 {
        eprintln!("[c-saves] {name}: injected {patched} save-cycle probes");
        Some(b)
    } else {
        None
    }
}

static RFS_GREF: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
/// Original RFS classfile bytes stashed at class-load time.
static RFS_ORIG: std::sync::Mutex<Option<Vec<u8>>> = std::sync::Mutex::new(None);
/// Patched bytes computed OFFLINE by the setup thread; the retransform
/// callback only hands these over (no ASM, no class loading in callbacks).
static RFS_PATCHED: std::sync::Mutex<Option<Vec<u8>>> = std::sync::Mutex::new(None);

/// Replace the five RegionFileStorage bodies with shim bridges (ASM path).
/// Runs inside the retransform callback: must NOT define classes or resolve
/// heavy machinery here (loader-lock deadlock) — everything is warmed by the
/// setup thread beforehand.
fn patch_rfs(bytes: &[u8]) -> Option<Vec<u8>> {
    // retransform path: serve precomputed bytes (never compute in-callback)
    let cached = RFS_PATCHED.lock().unwrap().clone();
    if let Some(p) = cached {
        eprintln!("[c-saves] RFS retransform: serving precomputed patched bytes");
        return Some(p);
    }
    // initial load: stash originals for offline patching (setup thread does
    // the ASM work outside any callback; class loading is legal there).
    let mut orig = RFS_ORIG.lock().unwrap();
    if orig.is_none() {
        *orig = Some(bytes.to_vec());
        eprintln!("[c-saves] RFS: original classfile stashed ({} bytes)", bytes.len());
    }
    None
}
/// OFFLINE computation of the five body replacements (never run inside a
/// JVMTI callback: ASM's COMPUTE_FRAMES may resolve classes).
fn patch_rfs_offline(bytes: &[u8]) -> Option<Vec<u8>> {
    let shim_internal = shim::shim_name();
    let specs: [(&str, &str, &str, &str, Vec<(u16, u8)>); 6] = [
        (
            "moonrise$finishRead",
            "(IILca/spottedleaf/moonrise/patches/chunk_system/io/MoonriseRegionFileIO$RegionDataController$ReadData;)Lnet/minecraft/nbt/CompoundTag;",
            "rfFinishRead",
            "(Lnet/minecraft/world/level/chunk/storage/RegionFileStorage;IILca/spottedleaf/moonrise/patches/chunk_system/io/MoonriseRegionFileIO$RegionDataController$ReadData;)Lnet/minecraft/nbt/CompoundTag;",
            vec![(0, b'L'), (1, b'I'), (2, b'I'), (3, b'L')],
        ),
        (
            "moonrise$readData",
            "(II)Lca/spottedleaf/moonrise/patches/chunk_system/io/MoonriseRegionFileIO$RegionDataController$ReadData;",
            "rfReadData",
            "(Lnet/minecraft/world/level/chunk/storage/RegionFileStorage;II)Lca/spottedleaf/moonrise/patches/chunk_system/io/MoonriseRegionFileIO$RegionDataController$ReadData;",
            vec![(0, b'L'), (1, b'I'), (2, b'I')],
        ),
        (
            "moonrise$startWrite",
            "(IILnet/minecraft/nbt/CompoundTag;)Lca/spottedleaf/moonrise/patches/chunk_system/io/MoonriseRegionFileIO$RegionDataController$WriteData;",
            "rfStartWrite",
            "(Lnet/minecraft/world/level/chunk/storage/RegionFileStorage;IILnet/minecraft/nbt/CompoundTag;)Lca/spottedleaf/moonrise/patches/chunk_system/io/MoonriseRegionFileIO$RegionDataController$WriteData;",
            vec![(0, b'L'), (1, b'I'), (2, b'I'), (3, b'L')],
        ),
        (
            "read",
            "(Lnet/minecraft/world/level/ChunkPos;)Lnet/minecraft/nbt/CompoundTag;",
            "rfRead",
            "(Lnet/minecraft/world/level/chunk/storage/RegionFileStorage;Lnet/minecraft/world/level/ChunkPos;)Lnet/minecraft/nbt/CompoundTag;",
            vec![(0, b'L'), (1, b'L')],
        ),
        (
            "write",
            "(Lnet/minecraft/world/level/ChunkPos;Lnet/minecraft/nbt/CompoundTag;)V",
            "rfWrite",
            "(Lnet/minecraft/world/level/chunk/storage/RegionFileStorage;Lnet/minecraft/world/level/ChunkPos;Lnet/minecraft/nbt/CompoundTag;)V",
            vec![(0, b'L'), (1, b'L'), (2, b'L')],
        ),
        (
            "scanChunk",
            "(Lnet/minecraft/world/level/ChunkPos;Lnet/minecraft/nbt/StreamTagVisitor;)V",
            "rfScan",
            "(Lnet/minecraft/world/level/chunk/storage/RegionFileStorage;Lnet/minecraft/world/level/ChunkPos;Lnet/minecraft/nbt/StreamTagVisitor;)V",
            vec![(0, b'L'), (1, b'L'), (2, b'L')],
        ),
    ];
    let mut current: Option<Vec<u8>> = None;
    let mut applied = 0usize;
    for (m, d, bridge, bridge_desc, slots) in &specs {
        let arg_specs: Vec<cplug_sdk::asm::ArgSpec> = slots
            .iter()
            .map(|(slot, ty)| cplug_sdk::asm::ArgSpec::Local { slot: *slot, ty: *ty })
            .collect();
        let spec = cplug_sdk::asm::ReplaceBody {
            method_name: m,
            method_desc: d,
            bridge_owner: shim_internal,
            bridge_name: bridge,
            bridge_desc,
            args: &arg_specs,
        };
        let src = current.as_deref().unwrap_or(bytes);
        let out = cplug_sdk::jni_util::with_attached(|env| {
            let cls = *RFS_GREF.get()? as jni::jclass;
            let loader = ASM_LOADER.get()?.to_ptr();
            let _ = cls; // loader is what define_class needs
            cplug_sdk::asm::replace_body(env, loader, src, &spec)
        });
        match out.flatten() {
            Some(patched) => {
                current = Some(patched);
                applied += 1;
                eprintln!("[c-saves] offline patch ok: {m}");
            }
            None => eprintln!("[c-saves] offline patch FAILED: {m}"),
        }
    }
    if applied == 6 {
        eprintln!("[c-saves] RegionFileStorage: 6 data-path bodies replaced with native bridges");
        current
    } else {
        None
    }
}

/// JVMTI GetClassLoader for a loaded class.
fn class_loader_of(cls: jni::jclass) -> Option<jni::jobject> {
    cplug_sdk::jni_util::with_attached(|env| {
        let jvmti = Jvmti::new(cplug_sdk::vm() as *mut jni::JavaVM).ok()?;
        let loader = jvmti.get_class_loader(cls).ok()?;
        Some(loader)
    })
    .flatten()
}

/// Loader pointer stash (raw; the loader lives for the process lifetime).
struct LoaderPtr(usize);
impl LoaderPtr {
    fn to_ptr(&self) -> jni::jobject {
        self.0 as jni::jobject
    }
}
static ASM_LOADER: std::sync::OnceLock<LoaderPtr> = std::sync::OnceLock::new();

#[unsafe(no_mangle)]
pub unsafe extern "C" fn cplugin_init(
    api: *const CPluginApi,
    vm: JavaVmPtr,
    _options: *const std::ffi::c_char,
) -> i32 {
    eprintln!("[c-saves] cplugin_init: Rust world-storage engine booting");
    cplug_sdk::init(api, vm);

    // engine + maintenance thread
    let reg = natives::REGISTRY.get_or_init(|| {
        let r = std::sync::Arc::new(engine::Registry::new());
        r
    });
    engine::spawn_maintenance(reg.clone());

    // save-cycle probes: pure-Rust, applied inline at class load
    cplug_sdk::hooks::register_bytes("**", |name, data| {
        if name == MCS || name == MRIO {
            patch_save_cycle(name, data.to_vec())
        } else if name == RFS {
            patch_rfs(data)
        } else {
            None
        }
    });

    // shim setup: wait for RegionFileStorage, define bridge class into its
    // loader, bind natives, then force the retransform that applies patches.
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(2500));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(240);
        loop {
            if cplug_sdk::classes::find_class(RFS).is_some() {
                break;
            }
            if std::time::Instant::now() > deadline {
                eprintln!("[c-saves] FATAL: RegionFileStorage never loaded; storage takeover aborted");
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
        let ok = cplug_sdk::jni_util::with_attached(|env| {
            let Some(cls) = cplug_sdk::classes::find_class(RFS) else { return Some(false) };
            let Some(loader) = class_loader_of(cls.as_jclass()) else {
                eprintln!("[c-saves] no class loader for RFS");
                return Some(false);
            };
            let defined = shim::define_and_bind(env, loader);
            if !defined {
                return Some(false);
            }
            // Warm the SDK ASM helper NOW (defines + resolves ASM classes on
            // this thread) — never inside the retransform callback.
            cplug_sdk::asm::ensure_defined(env, loader);
            let gref = env.new_global_ref(cls.as_jclass());
            let _ = RFS_GREF.set(gref as usize);
            let _ = ASM_LOADER.set(LoaderPtr(loader as usize));
            eprintln!("[c-saves] ASM helper warmed, RFS class cached");
            Some(true)
        })
        .flatten()
        .unwrap_or(false);
        if !ok {
            eprintln!("[c-saves] FATAL: shim definition failed; storage takeover aborted");
            return;
        }
        // OFFLINE: compute the five body replacements now (class loading is
        // legal on this thread — NOT inside any JVMTI callback).
        let orig = {
            let mut o = RFS_ORIG.lock().unwrap();
            o.take()
        };
        let Some(orig) = orig else {
            eprintln!("[c-saves] FATAL: RFS original bytes not stashed (class loaded before module?)");
            return;
        };
        let patched = match patch_rfs_offline(&orig) {
            Some(p) => p,
            None => {
                eprintln!("[c-saves] FATAL: offline RFS patch computation failed; vanilla IO stays");
                return;
            }
        };
        *RFS_PATCHED.lock().unwrap() = Some(patched);

        // retransform → byte hook serves the precomputed bytes
        let rc = cplug_sdk::retransform_class(RFS);
        if rc != 0 {
            eprintln!("[c-saves] FATAL: retransform failed rc={rc}");
            return;
        }
        eprintln!("[c-saves] storage takeover armed (Paper 26.3 Moonrise data path)");
    });

    0
}
