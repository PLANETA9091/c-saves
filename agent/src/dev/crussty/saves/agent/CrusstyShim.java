package dev.crussty.saves.agent;

import ca.spottedleaf.moonrise.patches.chunk_system.io.MoonriseRegionFileIO.RegionDataController.IORunnable;
import ca.spottedleaf.moonrise.patches.chunk_system.io.MoonriseRegionFileIO.RegionDataController.ReadData;
import ca.spottedleaf.moonrise.patches.chunk_system.io.MoonriseRegionFileIO.RegionDataController.ReadData.ReadResult;
import ca.spottedleaf.moonrise.patches.chunk_system.io.MoonriseRegionFileIO.RegionDataController.WriteData;
import ca.spottedleaf.moonrise.patches.chunk_system.io.MoonriseRegionFileIO.RegionDataController.WriteData.WriteResult;
import ca.spottedleaf.moonrise.patches.chunk_system.storage.ChunkSystemRegionFile;
import ca.spottedleaf.moonrise.patches.nbt_rw.FastNBTParser;
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.File;
import java.io.IOException;
import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.nio.file.Path;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.atomic.AtomicLong;
import net.minecraft.nbt.CompoundTag;
import net.minecraft.nbt.NbtIo;
import net.minecraft.nbt.NbtAccounter;
import net.minecraft.nbt.StreamTagVisitor;
import net.minecraft.world.level.ChunkPos;
import net.minecraft.world.level.chunk.storage.RegionFile;
import net.minecraft.world.level.chunk.storage.RegionFileStorage;

/**
 * Standalone agent bridge (v0.9.62): replaces the bodies of RegionFileStorage's
 * read/write entry points (ASM replace_body, see CrusstyAgent) and routes chunk
 * payloads into the native c-saves storage engine — WITHOUT the CRUSSTY kernel.
 *
 * This is the exact semantics of the product shim (shim/src/.../SavesShim.java,
 * defined + RegisterNatives-bound by the kernel): same ReadData ownership
 * marker, same vanilla transcriptions for the inactive path, same freshness
 * evidence (Status=minecraft:full && InhabitedTime==0). The only differences:
 * the natives resolve through STANDARD JNI exports (Java_dev_crussty_saves_
 * agent_CrusstyShim_n*) — one-line pass-throughs into natives::n_* — and the
 * container dir is a plain folder (world/c-saves/<kind>) instead of the
 * kernel's colon-tagged path. Payload contract in BOTH directions: UNCOMPRESSED
 * NBT bytes exactly as NbtIo.write would produce (root tag, empty name).
 *
 * CI asserts on the stats line printed by dumpStats() (shutdown hook): served
 * reads / misses / writes per storage kind prove the server actually READS its
 * spawn chunks from the container on a cold restart.
 */
public final class CrusstyShim {

    /** Storage kinds, mirrored on the native side. */
    public static final int KIND_CHUNK = 0;
    public static final int KIND_ENTITY = 1;
    public static final int KIND_POI = 2;

    /** Per-storage attach state: handle + kind (identity-keyed). Chunk IO
     * threads call attach concurrently — must be a ConcurrentHashMap. */
    private static final Map<RegionFileStorage, int[]> HANDLES = new ConcurrentHashMap<>();
    private static Field FOLDER_FIELD;
    private static Method GET_REGION_FILE;

    // ---- session counters (the CI gate reads the dumpStats line) ----
    static final AtomicLong READ_SERVED = new AtomicLong();
    static final AtomicLong READ_MISS = new AtomicLong();
    static final AtomicLong WRITE_OK = new AtomicLong();
    static final AtomicLong WRITE_FAIL = new AtomicLong();
    static final AtomicLong VANILLA_FALLBACK = new AtomicLong();
    static volatile boolean ACTIVE_SEEN;

    /** reads/writes per kind: index = KIND_*, [served, miss, written] */
    private static final AtomicLong[][] KIND_STATS = {
        { new AtomicLong(), new AtomicLong(), new AtomicLong() },
        { new AtomicLong(), new AtomicLong(), new AtomicLong() },
        { new AtomicLong(), new AtomicLong(), new AtomicLong() },
    };

    private CrusstyShim() {}

    // ------------------------------------------------------------------
    // natives (resolved by STANDARD JNI: Java_dev_crussty_saves_agent_...)
    // ------------------------------------------------------------------
    public static native boolean nActive();
    public static native int nAttach(String kind, String level, String containerDir);
    public static native byte[] nReadChunk(int handle, int cx, int cz);
    public static native boolean nWriteChunk(int handle, int cx, int cz, byte[] nbt, boolean fresh);
    public static native boolean nHasChunk(int handle, int cx, int cz);
    public static native boolean nBeginSave();
    public static native boolean nEndSave();
    public static native void nMarkAutosave();
    /** v0.9.63: {rn, rp50, rp90, rp99, rmax, wn, wp50, wp90, wp99, wmax} in µs. */
    public static native long[] nLatSnapshot();

    /** set -Dcsaves.wlog=true to log every chunk write */
    static final boolean WRITE_LOG = Boolean.getBoolean("csaves.wlog")
            || System.getenv("CSAVES_WLOG") != null;

    // ------------------------------------------------------------------
    // v0.9.63: the A/B mode + the JVM-side latency meters.
    //
    // -Dcsaves.mode=anvil routes EVERY patched body through the faithful
    // vanilla transcriptions (real region-file IO on a real .mca world) —
    // one agent build measures both storages on the same server. The engine
    // meters must stay EMPTY in anvil mode (rn=0 is itself a CI gate:
    // vanilla mode provably never touches the container).
    //
    // FETCH / STORE_WRITE time the per-chunk work as the server thread
    // experiences it, ONLY for samples that actually returned data:
    // container — the nReadChunk call incl. JNI marshalling; anvil — the
    // vanilla transcription incl. region open + zlib inflate. Contract-miss
    // fetches (None -> the chunk system regenerates) are identical worldgen
    // work in both worlds and are excluded from both histograms.
    // ------------------------------------------------------------------
    static final boolean ANVIL_MODE = "anvil".equals(
            System.getProperty("csaves.mode", "container"));

    /** Linear µs slots up to 1023 (exact values), floor-log2 buckets above —
     * the exact mirror of the engine's native histogram (engine.rs). */
    static final class Lat {
        private final java.util.concurrent.atomic.AtomicLongArray slots =
                new java.util.concurrent.atomic.AtomicLongArray(1047);
        private final AtomicLong max = new AtomicLong();
        private final AtomicLong n = new AtomicLong();

        void add(long us) {
            n.incrementAndGet();
            slots.incrementAndGet(slotOf(us));
            max.accumulateAndGet(us, Math::max);
        }

        private static int slotOf(long us) {
            if (us < 1024) return (int) us;
            int k = 63 - Long.numberOfLeadingZeros(us);
            return k >= 32 ? 1046 : 1024 + (k - 10);
        }

        private static long slotLower(int slot) {
            if (slot < 1024) return slot;
            if (slot < 1046) return 1L << (slot - 1024 + 10);
            return 1L << 32;
        }

        /** {n, p50, p90, p99, max} in µs (all 0 when empty). */
        long[] snap() {
            long count = 0;
            for (int i = 0; i < slots.length(); i++) count += slots.get(i);
            if (count == 0) return new long[5];
            return new long[] { count, pct(count, 50), pct(count, 90), pct(count, 99), max.get() };
        }

        private long pct(long count, int q) {
            long rank = (q * count + 99) / 100; // 1-indexed, ceil
            long cum = 0;
            for (int i = 0; i < slots.length(); i++) {
                cum += slots.get(i);
                if (cum >= rank) return slotLower(i);
            }
            return slotLower(1046);
        }
    }

    /** per-chunk fetch cost (served samples only), as the server thread sees it */
    static final Lat FETCH = new Lat();
    /** per-chunk store cost (successful samples only) */
    static final Lat STORE_WRITE = new Lat();
    static volatile long END_SAVE_MS;

    /** Shutdown-barrier stats dump (registered by the agent): fsync + counts. */
    public static void dumpStats() {
        StringBuilder sb = new StringBuilder();
        // v0.9.63: in ANVIL mode nothing ever attaches (by design — vanilla
        // IO must not touch the engine), so "never active" is the EXPECTED
        // state there and the latency section still has to print (the anvil
        // meters live on the JVM side). Only a non-anvil process with no IO
        // attached keeps the legacy marker.
        if (!ACTIVE_SEEN && !ANVIL_MODE) {
            sb.append("[c-saves] stats: never active (no IO attached)");
        } else {
            long t0 = System.nanoTime();
            try {
                nEndSave(); // the durable barrier the server's normal save path would run
            } catch (Throwable t) {
                sb.append("[c-saves] endSave failed: ").append(t).append("; ");
            }
            END_SAVE_MS = (System.nanoTime() - t0) / 1_000_000;
            sb.append("[c-saves] stats: block read=").append(KIND_STATS[KIND_CHUNK][0])
                .append('/').append(KIND_STATS[KIND_CHUNK][1]).append(" served/miss, written=")
                .append(KIND_STATS[KIND_CHUNK][2])
                .append(" | entities read=").append(KIND_STATS[KIND_ENTITY][0])
                .append('/').append(KIND_STATS[KIND_ENTITY][1]).append(", written=")
                .append(KIND_STATS[KIND_ENTITY][2])
                .append(" | poi read=").append(KIND_STATS[KIND_POI][0])
                .append('/').append(KIND_STATS[KIND_POI][1]).append(", written=")
                .append(KIND_STATS[KIND_POI][2])
                .append(" | totals served=").append(READ_SERVED).append(" miss=").append(READ_MISS)
                .append(" writeOk=").append(WRITE_OK).append(" writeFail=").append(WRITE_FAIL)
                .append(" vanillaFallback=").append(VANILLA_FALLBACK);
            // ---- v0.9.63: the latency section (all values µs) ----
            sb.append(" | mode=").append(ANVIL_MODE ? "anvil" : "container");
            try {
                long[] e = nLatSnapshot();
                if (e != null && e.length == 10) {
                    sb.append(" | eng rn=").append(e[0]).append(" rp50=").append(e[1])
                      .append(" rp90=").append(e[2]).append(" rp99=").append(e[3]).append(" rmax=").append(e[4])
                      .append(" wn=").append(e[5]).append(" wp50=").append(e[6])
                      .append(" wp90=").append(e[7]).append(" wp99=").append(e[8]).append(" wmax=").append(e[9]);
                }
            } catch (Throwable t) {
                sb.append(" | eng lat failed: ").append(t);
            }
            long[] f = FETCH.snap();
            sb.append(" | jvm fn=").append(f[0]).append(" fp50=").append(f[1])
              .append(" fp90=").append(f[2]).append(" fp99=").append(f[3]).append(" fmax=").append(f[4]);
            long[] sw = STORE_WRITE.snap();
            sb.append(" | store swn=").append(sw[0]).append(" swp50=").append(sw[1])
              .append(" swp90=").append(sw[2]).append(" swp99=").append(sw[3]).append(" swmax=").append(sw[4]);
            sb.append(" | endSaveMs=").append(END_SAVE_MS);
        }
        String line = sb.toString();
        System.err.println(line);
        try {
            String f = System.getProperty("csaves.statsfile");
            if (f != null) {
                java.nio.file.Files.writeString(java.nio.file.Path.of(f), line + System.lineSeparator(),
                        java.nio.file.StandardOpenOption.CREATE, java.nio.file.StandardOpenOption.APPEND);
            }
        } catch (Throwable ignored) {
        }
    }

    // ------------------------------------------------------------------
    // save-cycle hooks (called from the shutdown hook by the agent)
    // ------------------------------------------------------------------
    public static void onSaveBarrier() {
        if (nActive()) nEndSave();
    }

    // ------------------------------------------------------------------
    // storage attach (lazy, identity-cached)
    // ------------------------------------------------------------------
    private static int kindOf(RegionFileStorage st) {
        String t = st.info().type();
        if (t == null) return KIND_CHUNK;
        return switch (t) {
            case "entities" -> KIND_ENTITY;
            case "poi" -> KIND_POI;
            default -> KIND_CHUNK;
        };
    }

    /** Attach on first use of a storage (idempotent, identity-keyed). */
    private static int ensureAttach(RegionFileStorage st) {
        int[] h = HANDLES.get(st);
        if (h != null) return h[0];
        synchronized (HANDLES) {
            h = HANDLES.get(st);
            if (h != null) return h[0];
            int kind = kindOf(st);
            String containerDir;
            try {
                if (FOLDER_FIELD == null) {
                    FOLDER_FIELD = RegionFileStorage.class.getDeclaredField("folder");
                    FOLDER_FIELD.setAccessible(true);
                }
                Path folder = (Path) FOLDER_FIELD.get(st);
                File world = folder.toFile().getParentFile();
                containerDir = new File(world, "c-saves" + File.separatorChar + kindName(kind))
                        .getAbsolutePath().replace(File.separatorChar, '/');
            } catch (Throwable t) {
                containerDir = "c-saves/" + kindName(kind);
            }
            int handle = nAttach(kindName(kind), st.info().level(), containerDir);
            HANDLES.put(st, new int[]{handle, kind});
            ACTIVE_SEEN = true;
            System.err.println("[c-saves] attached kind=" + kindName(kind)
                    + " level=" + st.info().level() + " container=" + containerDir);
            return handle;
        }
    }

    private static int kindOfHandle(RegionFileStorage st) {
        int[] h = HANDLES.get(st);
        return h != null ? h[1] : kindOf(st);
    }

    private static String kindName(int kind) {
        return kind == KIND_ENTITY ? "entities" : kind == KIND_POI ? "poi" : "region";
    }

    // ------------------------------------------------------------------
    // v0.9.64 DIAGNOSTIC PROBES (method-entry instrumentation, -Dcsaves.probe):
    // they log the scheduler's load-vs-generate decisions for the tracked
    // chunk (0,0) WITHOUT altering behavior — prependBody keeps the original
    // body. The goal is to catch the spawn-window regeneration decision that
    // never consults the patched IO surface.
    // ------------------------------------------------------------------
    private static void probe(String tag, int x, int z, Object... kv) {
        if (x != 0 || z != 0) return;
        StringBuilder sb = new StringBuilder("[c-saves] PROBE ").append(tag)
                .append(" (0,0)");
        for (int i = 0; i + 1 < kv.length; i += 2) {
            sb.append(' ').append(kv[i]).append('=').append(kv[i + 1]);
        }
        System.err.println(sb);
    }

    public static void probeSCL4(int x, int z, Object status, boolean gen) {
        probe("scheduleChunkLoad6", x, z, "status", status, "gen", gen);
    }

    public static void probeSCL7(int x, int z, boolean b0, Object status, boolean gen) {
        probe("scheduleChunkLoad7", x, z, "b0", b0, "status", status, "gen", gen);
    }

    public static void probeSync3(int x, int z, Object status) {
        probe("syncLoad", x, z, "status", status);
    }

    public static void probeBCL4(int x, int z, Object status, Object prio) {
        probe("beginChunkLoadForNonFullSync", x, z, "status", status);
    }

    private static volatile boolean SCHED_STACK_DUMPPED;
    public static void probeSched6(int x, int z, Object status, Object holder, Object list, Object prio) {
        probe("schedule", x, z, "status", status, "holder", holder);
        if ((x == 0 && z == 0) && !SCHED_STACK_DUMPPED) {
            SCHED_STACK_DUMPPED = true;
            StackTraceElement[] st = new Throwable().getStackTrace();
            StringBuilder ts = new StringBuilder("[c-saves] SCHED(0,0) first-call stack:");
            for (int i = 1; i < Math.min(st.length, 16); i++) ts.append(" \n  at ").append(st[i]);
            System.err.println(ts);
        }
    }

    // ------------------------------------------------------------------
    // v0.9.62 diagnostic + belt-and-braces: controller-level write entry.
    // The storage-level moonrise$startWrite body was patched and verified
    // (readData through the SAME interface dispatch serves 1587 misses),
    // yet Paper 26.3's stop-save wrote full .mca files with ZERO entries
    // into the patched storage body — so the write is bridged one level
    // HIGHER, on the data controllers themselves, and the effective cache
    // class is printed once for the record.
    // ------------------------------------------------------------------
    private static volatile boolean CACHE_CLASS_LOGGED;

    public static WriteData dcStartWrite(Object ctrl, int x, int z, CompoundTag tag) throws IOException {
        RegionFileStorage rfs;
        try {
            rfs = ((ca.spottedleaf.moonrise.patches.chunk_system.io.MoonriseRegionFileIO.RegionDataController) ctrl).getCache();
        } catch (Throwable t) {
            System.err.println("[c-saves] dcStartWrite getCache failed: " + t);
            throw new IOException(t);
        }
        if (!CACHE_CLASS_LOGGED) {
            CACHE_CLASS_LOGGED = true;
            System.err.println("[c-saves] controller cache class: " + rfs.getClass().getName()
                    + " (loader=" + rfs.getClass().getClassLoader() + ")");
        }
        if (ANVIL_MODE) {
            if (tag == null) {
                return new WriteData(null, WriteResult.DELETE, null, null);
            }
            long t0 = System.nanoTime();
            WriteData wd = vanillaStartWrite(rfs, x, z, tag);
            STORE_WRITE.add((System.nanoTime() - t0) / 1000);
            return wd;
        }
        final int kind = ctrl instanceof ca.spottedleaf.moonrise.patches.chunk_system.io.datacontroller.EntityDataController
                ? KIND_ENTITY
                : ctrl instanceof ca.spottedleaf.moonrise.patches.chunk_system.io.datacontroller.PoiDataController
                    ? KIND_POI : KIND_CHUNK;
        if (WRITE_LOG) System.err.println("[c-saves] ENTER dcStartWrite(" + kindName(kind) + ") " + x + "," + z
                + " tag=" + (tag == null ? "null" : tag.getStringOr("Status", "?")));
        if (tag == null) {
            return new WriteData(null, WriteResult.DELETE, null, null);
        }
        int h = ensureAttach(rfs);
        if (nActive()) {
            long t0 = System.nanoTime();
            ByteArrayOutputStream bos = new ByteArrayOutputStream(1 << 14);
            NbtIo.write(tag, new DataOutputStream(bos));
            boolean ok = nWriteChunk(h, x, z, bos.toByteArray(), isFreshChunk(tag));
            STORE_WRITE.add((System.nanoTime() - t0) / 1000);
            if (ok) {
                WRITE_OK.incrementAndGet();
                KIND_STATS[kind][2].incrementAndGet();
            } else {
                WRITE_FAIL.incrementAndGet();
            }
            if (!ok || WRITE_LOG) {
                System.err.println("[c-saves] dcStartWrite(" + x + "," + z + ") stored=" + ok);
            }
            return new WriteData(tag, WriteResult.WRITE,
                    new DataOutputStream(new ByteArrayOutputStream(0)), NoopCommit.INSTANCE);
        }
        VANILLA_FALLBACK.incrementAndGet();
        return vanillaStartWrite(rfs, x, z, tag);
    }

    // ------------------------------------------------------------------
    // RegionFileStorage.moonrise$doesRegionFileNotExistNoIO(int, int)
    // replacement.
    // ==================================================================
    // THE SILENT-REGENERATION FIX. Vanilla semantics: "does the region .mca
    // file possibly not exist" — Moonrise consults it to skip reads it
    // believes cannot succeed. With zero .mca files on disk the vanilla
    // answer is ALWAYS true, so the chunk system regenerates chunks WITHOUT
    // ever consulting the container (observed live: spawn chunks rewritten
    // by worldgen on every boot while miss==0, overwriting a live edit that
    // WAS stored). The container IS the region file now: a chunk exists iff
    // the engine knows its key (blob or regeneration contract). Vanilla
    // fallback (agent inactive): the private doesRegionFilePossiblyExist,
    // called reflectively — its own body is untouched by the patch.
    // ------------------------------------------------------------------
    private static Method DOES_POSSIBLY_EXIST;

    public static boolean rfDoesNotExistNoIO(RegionFileStorage st, int x, int z) throws IOException {
        if (!ANVIL_MODE) {
            int h = ensureAttach(st);
            if (nActive()) {
                return !nHasChunk(h, x, z);
            }
        }
        try {
            if (DOES_POSSIBLY_EXIST == null) {
                DOES_POSSIBLY_EXIST = RegionFileStorage.class.getDeclaredMethod(
                        "doesRegionFilePossiblyExist", long.class);
                DOES_POSSIBLY_EXIST.setAccessible(true);
            }
            long region = ((long) (x >> 5) << 32) | ((long) ((z >> 5) & 0xffff_ffffL) & 0xffff_ffffL);
            return !((Boolean) DOES_POSSIBLY_EXIST.invoke(st, region));
        } catch (ReflectiveOperationException e) {
            throw new IOException("c-saves: doesRegionFilePossiblyExist reflection failed", e);
        }
    }

    // ------------------------------------------------------------------
    // RegionFileStorage.moonrise$readData(int, int) replacement
    // ==================================================================
    // Active: serve the chunk NBT straight from the c-saves container as an
    // UNCOMPRESSED stream — the zero-decompression read path. Miss: fall back
    // to a faithful transcription of the original body (import phase).
    //
    // ReadData.recalculateCount doubles as an OWNERSHIP MARKER: vanilla
    // recalc counts are >= 0, so MIN_VALUE tags data served by this shim.
    // moonrise$finishRead (patched below) uses it to skip the regionfile
    // recovery checks — with no .mca files on disk those checks can never
    // pass and the task would retry forever (silent spawn-prep hang).
    // ------------------------------------------------------------------
    static final int RECALC_OWNED = Integer.MIN_VALUE;

    public static ReadData rfReadData(RegionFileStorage st, int x, int z) throws IOException {
        if (WRITE_LOG) System.err.println("[c-saves] ENTER readData " + x + "," + z);
        if (ANVIL_MODE) {
            long t0 = System.nanoTime();
            ReadData rd = vanillaReadData(st, x, z);
            // HAS_DATA only: a NO_DATA lookup is a near-free probe, not a
            // served chunk — timing it would flatter the anvil baseline.
            if (rd.input() != null) {
                FETCH.add((System.nanoTime() - t0) / 1000);
            }
            return rd;
        }
        int h = ensureAttach(st);
        if (nActive()) {
            long t0 = System.nanoTime();
            byte[] b = nReadChunk(h, x, z);
            if (b != null) {
                FETCH.add((System.nanoTime() - t0) / 1000);
                if (WRITE_LOG) System.err.println("[c-saves] readData " + x + "," + z + " SERVED " + b.length + "B");
                READ_SERVED.incrementAndGet();
                KIND_STATS[kindOfHandle(st)][0].incrementAndGet();
                return new ReadData(ReadResult.HAS_DATA,
                        new DataInputStream(new ByteArrayInputStream(b)), null, RECALC_OWNED);
            }
            if (WRITE_LOG) System.err.println("[c-saves] readData " + x + "," + z + " MISS");
            READ_MISS.incrementAndGet();
            KIND_STATS[kindOfHandle(st)][1].incrementAndGet();
        } else {
            VANILLA_FALLBACK.incrementAndGet();
        }
        return vanillaReadData(st, x, z);
    }

    // ------------------------------------------------------------------
    // RegionFileStorage.moonrise$finishRead(int, int, ReadData) replacement
    // ==================================================================
    // Shim-owned ReadData (marker recalc): parse the uncompressed NBT and
    // return the tag directly. Vanilla recalc path: faithful transcription
    // of the original body (recovery-aware re-reads for .mca fallback).
    // ------------------------------------------------------------------
    public static CompoundTag rfFinishRead(RegionFileStorage st, int x, int z, ReadData readData) throws IOException {
        if (ANVIL_MODE) {
            return vanillaFinishRead(st, x, z, readData);
        }
        if (readData.recalculateCount() == RECALC_OWNED) {
            DataInputStream in = readData.input();
            try {
                return FastNBTParser.nbtIoRead(in);
            } finally {
                in.close();
            }
        }
        return vanillaFinishRead(st, x, z, readData);
    }

    /** Transcription of the original RegionFileStorage.moonrise$finishRead body. */
    private static CompoundTag vanillaFinishRead(RegionFileStorage st, int x, int z, ReadData readData) throws IOException {
        CompoundTag tag = FastNBTParser.nbtIoRead(readData.input());
        readData.input().close();
        RegionFile rf = st.moonrise$getRegionFileIfExists(x, z);
        if (rf == null || ((ChunkSystemRegionFile) (Object) rf).moonrise$getRecalculateCount()
                != readData.recalculateCount()) {
            return null;
        }
        ChunkPos pos = new ChunkPos(x, z);
        ChunkPos got = ca.spottedleaf.moonrise.patches.chunk_system.storage.RegionFileRecovery
                .getChunkCoordinate(tag);
        if (pos.equals(got)) {
            return tag;
        }
        return null;
    }

    /** Transcription of the original RegionFileStorage.moonrise$readData body. */
    private static ReadData vanillaReadData(RegionFileStorage st, int x, int z) throws IOException {
        RegionFile rf = st.moonrise$getRegionFileIfExists(x, z);
        int recalc = 0;
        DataInputStream in = null;
        if (rf != null) {
            synchronized (rf) {
                in = rf.getChunkDataInputStream(new ChunkPos(x, z));
                recalc = ((ChunkSystemRegionFile) (Object) rf).moonrise$getRecalculateCount();
            }
        }
        if (in == null) {
            return new ReadData(ReadResult.NO_DATA, null, null, recalc);
        }
        return new ReadData(ReadResult.HAS_DATA, in, null, recalc);
    }

    // ------------------------------------------------------------------
    // RegionFileStorage.moonrise$startWrite(int, int, CompoundTag) replacement
    // ==================================================================
    // Active: serialize the tag to uncompressed NBT (the exact bytes we keep
    // in the container), hand it to the engine, and return a WriteData whose
    // commit closure is a no-op — nothing ever touches the region file.
    // ------------------------------------------------------------------
    public static WriteData rfStartWrite(RegionFileStorage st, int x, int z, CompoundTag tag) throws IOException {
        if (WRITE_LOG) System.err.println("[c-saves] ENTER startWrite " + x + "," + z + " tag=" + (tag == null ? "null" : tag.getStringOr("Status", "?")));
        if (ANVIL_MODE) {
            if (tag == null) {
                return new WriteData(null, WriteResult.DELETE, null, null);
            }
            long t0 = System.nanoTime();
            WriteData wd = vanillaStartWrite(st, x, z, tag);
            STORE_WRITE.add((System.nanoTime() - t0) / 1000);
            return wd;
        }
        if (tag == null) {
            return new WriteData(null, WriteResult.DELETE, null, null);
        }
        int h = ensureAttach(st);
        if (nActive()) {
            long t0 = System.nanoTime();
            ByteArrayOutputStream bos = new ByteArrayOutputStream(1 << 14);
            NbtIo.write(tag, new DataOutputStream(bos));
            // per-write freshness evidence from the tag itself — pristine
            // contracts may form only on genuinely fresh chunks (v0.9.47).
            boolean ok = nWriteChunk(h, x, z, bos.toByteArray(), isFreshChunk(tag));
            STORE_WRITE.add((System.nanoTime() - t0) / 1000);
            if (ok) {
                WRITE_OK.incrementAndGet();
                KIND_STATS[kindOfHandle(st)][2].incrementAndGet();
            } else {
                WRITE_FAIL.incrementAndGet();
            }
            if (!ok || WRITE_LOG) {
                System.err.println("[c-saves] startWrite(" + x + "," + z + ") status="
                    + tag.getStringOr("Status", "") + " stored=" + ok);
            }
            return new WriteData(tag, WriteResult.WRITE,
                    new DataOutputStream(new ByteArrayOutputStream(0)), NoopCommit.INSTANCE);
        }
        VANILLA_FALLBACK.incrementAndGet();
        return vanillaStartWrite(st, x, z, tag);
    }

    /** No-op commit closure: the engine already has the bytes. */
    public static final class NoopCommit implements IORunnable {
        public static final NoopCommit INSTANCE = new NoopCommit();
        private NoopCommit() {}
        @Override
        public void run(RegionFile rf) throws IOException {
        }
    }

    /**
     * Freshness evidence, read from the chunk tag the server itself handed
     * us: the chunk must be FULLY generated (Status=minecraft:full — proto-
     * stage saves are stored honestly and never contracted, so a later full
     * regen cannot flip them) and never player-inhabited (InhabitedTime==0 —
     * a player standing in/near the chunk ticks it, so any player-visible
     * interaction leaves evidence). An absent InhabitedTime key is treated as
     * NOT fresh (conservative: only a tag that carries the zero is evidence
     * of freshness).
     */
    static boolean isFreshChunk(CompoundTag tag) {
        if (!"minecraft:full".equals(tag.getStringOr("Status", ""))) {
            return false;
        }
        return tag.contains("InhabitedTime") && tag.getLongOr("InhabitedTime", -1L) == 0L;
    }

    /** Transcription of the original RegionFileStorage.moonrise$startWrite body. */
    private static WriteData vanillaStartWrite(RegionFileStorage st, int x, int z, CompoundTag tag) throws IOException {
        ChunkPos pos = new ChunkPos(x, z);
        RegionFile rf = regionFileFor(st, pos, true);
        WriteData wd = ((ChunkSystemRegionFile) (Object) rf).moonrise$startWrite(tag, pos);
        try {
            NbtIo.write(tag, wd.output());
            wd.output().close();
        } catch (IOException e) {
            wd.output().close();
            throw e;
        }
        return wd;
    }

    // ------------------------------------------------------------------
    // RegionFileStorage.read(ChunkPos) replacement (legacy sync path used by
    // SimpleRegionStorage / IOWorker: entities + poi + misc scans).
    // ------------------------------------------------------------------
    public static CompoundTag rfRead(RegionFileStorage st, ChunkPos pos) throws IOException {
        if (ANVIL_MODE) {
            return vanillaRead(st, pos);
        }
        int h = ensureAttach(st);
        if (nActive()) {
            byte[] b = nReadChunk(h, pos.x(), pos.z());
            if (b != null) {
                READ_SERVED.incrementAndGet();
                KIND_STATS[kindOfHandle(st)][0].incrementAndGet();
                return FastNBTParser.nbtIoRead(new DataInputStream(new ByteArrayInputStream(b)));
            }
            READ_MISS.incrementAndGet();
            KIND_STATS[kindOfHandle(st)][1].incrementAndGet();
        } else {
            VANILLA_FALLBACK.incrementAndGet();
        }
        return vanillaRead(st, pos);
    }

    /** Transcription of the original RegionFileStorage.read body (recovery check omitted: import-phase only). */
    private static CompoundTag vanillaRead(RegionFileStorage st, ChunkPos pos) throws IOException {
        RegionFile rf = st.moonrise$getRegionFileIfExists(pos.x(), pos.z());
        if (rf == null) return null;
        DataInputStream in;
        synchronized (rf) {
            in = rf.getChunkDataInputStream(pos);
        }
        if (in == null) return null;
        try {
            return FastNBTParser.nbtIoRead(in);
        } finally {
            in.close();
        }
    }

    // ------------------------------------------------------------------
    // RegionFileStorage.write(ChunkPos, CompoundTag) replacement.
    // ------------------------------------------------------------------
    public static void rfWrite(RegionFileStorage st, ChunkPos pos, CompoundTag tag) throws IOException {
        if (WRITE_LOG) System.err.println("[c-saves] ENTER write " + pos.x() + "," + pos.z());
        if (ANVIL_MODE) {
            vanillaWrite(st, pos, tag);
            return;
        }
        int h = ensureAttach(st);
        if (nActive()) {
            long t0 = System.nanoTime();
            ByteArrayOutputStream bos = new ByteArrayOutputStream(1 << 14);
            NbtIo.write(tag, new DataOutputStream(bos));
            boolean ok = nWriteChunk(h, pos.x(), pos.z(), bos.toByteArray(), isFreshChunk(tag));
            STORE_WRITE.add((System.nanoTime() - t0) / 1000);
            if (ok) {
                WRITE_OK.incrementAndGet();
                KIND_STATS[kindOfHandle(st)][2].incrementAndGet();
            } else {
                WRITE_FAIL.incrementAndGet();
            }
            if (!ok || WRITE_LOG) {
                System.err.println("[c-saves] write(" + pos.x() + "," + pos.z() + ") status="
                    + tag.getStringOr("Status", "") + " stored=" + ok);
            }
            return;
        }
        VANILLA_FALLBACK.incrementAndGet();
        vanillaWrite(st, pos, tag);
    }

    // ------------------------------------------------------------------
    // RegionFileStorage.moonrise$finishWrite(int, int, WriteData) replacement.
    // Shim-owned WriteData carries the NoopCommit closure — nothing to commit
    // and, critically, NOTHING TO CREATE: the vanilla body runs the closure
    // against getRegionFile(pos, create=true), which would materialize empty
    // .mca files for every touched region even though the bytes are already
    // in the container (the zero-.mca guarantee). Import phase: faithful
    // transcription (commit closure against the real region file).
    // ------------------------------------------------------------------
    public static void rfFinishWrite(RegionFileStorage st, int x, int z, WriteData wd) throws IOException {
        if (wd == null || wd.write() == null || wd.write() instanceof NoopCommit) {
            return;
        }
        wd.write().run(regionFileFor(st, new ChunkPos(x, z), true));
    }

    /** Transcription of the original RegionFileStorage.write body. */
    private static void vanillaWrite(RegionFileStorage st, ChunkPos pos, CompoundTag tag) throws IOException {
        RegionFile rf = st.moonrise$getRegionFileIfExists(pos.x(), pos.z());
        if (rf == null) {
            rf = regionFileFor(st, pos, true);
        } else {
            // mirror getRegionFile(pos, true) cache bookkeeping via the same
            // accessor the original body uses after the exists-check
            rf = regionFileFor(st, pos, true);
        }
        rf.clear(pos);
        DataOutputStream out = rf.getChunkDataOutputStream(pos);
        try {
            NbtIo.write(tag, out);
        } finally {
            out.close();
        }
    }

    /** Private getRegionFile(ChunkPos, boolean) via reflection (fallback/import path only). */
    private static RegionFile regionFileFor(RegionFileStorage st, ChunkPos pos, boolean create) throws IOException {
        try {
            if (GET_REGION_FILE == null) {
                GET_REGION_FILE = RegionFileStorage.class.getDeclaredMethod("getRegionFile", ChunkPos.class, boolean.class);
                GET_REGION_FILE.setAccessible(true);
            }
            return (RegionFile) GET_REGION_FILE.invoke(st, pos, create);
        } catch (ReflectiveOperationException e) {
            throw new IOException("c-saves: getRegionFile reflection failed", e);
        }
    }

    // ------------------------------------------------------------------
    // RegionFileStorage.scanChunk(ChunkPos, StreamTagVisitor) replacement.
    // Streams the visitor over the stored uncompressed NBT.
    // ------------------------------------------------------------------
    public static void rfScan(RegionFileStorage st, ChunkPos pos, StreamTagVisitor visitor) throws IOException {
        if (ANVIL_MODE) {
            vanillaScan(st, pos, visitor);
            return;
        }
        int h = ensureAttach(st);
        if (nActive()) {
            byte[] b = nReadChunk(h, pos.x(), pos.z());
            if (b != null) {
                READ_SERVED.incrementAndGet();
                KIND_STATS[kindOfHandle(st)][0].incrementAndGet();
                NbtIo.parse(new DataInputStream(new ByteArrayInputStream(b)), visitor, NbtAccounter.unlimitedHeap());
                return;
            }
            READ_MISS.incrementAndGet();
            KIND_STATS[kindOfHandle(st)][1].incrementAndGet();
        } else {
            VANILLA_FALLBACK.incrementAndGet();
        }
        vanillaScan(st, pos, visitor);
    }

    private static void vanillaScan(RegionFileStorage st, ChunkPos pos, StreamTagVisitor visitor) throws IOException {
        RegionFile rf = st.moonrise$getRegionFileIfExists(pos.x(), pos.z());
        if (rf == null) return;
        DataInputStream in;
        synchronized (rf) {
            in = rf.getChunkDataInputStream(pos);
        }
        if (in == null) return;
        try {
            NbtIo.parse(in, visitor, NbtAccounter.unlimitedHeap());
        } finally {
            in.close();
        }
    }
}
