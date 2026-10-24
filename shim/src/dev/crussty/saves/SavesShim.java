package dev.crussty.saves;

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
import net.minecraft.nbt.CompoundTag;
import net.minecraft.nbt.NbtIo;
import net.minecraft.nbt.NbtAccounter;
import net.minecraft.nbt.StreamTagVisitor;
import net.minecraft.world.level.ChunkPos;
import net.minecraft.world.level.chunk.storage.RegionFile;
import net.minecraft.world.level.chunk.storage.RegionFileStorage;

/**
 * c-saves bridge: replaces the bodies of RegionFileStorage's read/write entry
 * points (ASM replace_body) and routes chunk payloads into the native CRUSSTY
 * storage engine. Payload contract in BOTH directions: UNCOMPRESSED NBT bytes
 * exactly as NbtIo.write would produce (root tag, empty name).
 *
 * This class is defined into the kernel's classloader by the c-saves module
 * (unique name per .so, see module-side renaming) and its natives are bound
 * with RegisterNatives — the class file never hardcodes its own binary name.
 */
public final class SavesShim {

    /** Storage kinds, mirrored on the native side. */
    public static final int KIND_CHUNK = 0;
    public static final int KIND_ENTITY = 1;
    public static final int KIND_POI = 2;

    /** Per-storage attach state: handle + kind (identity-keyed). Chunk IO
     * threads call attach concurrently — must be a ConcurrentHashMap. */
    private static final Map<RegionFileStorage, int[]> HANDLES = new ConcurrentHashMap<>();
    private static final Map<RegionFileStorage, byte[]> STORAGES = new ConcurrentHashMap<>();
    private static Field FOLDER_FIELD;
    private static Method GET_REGION_FILE;

    private SavesShim() {}

    // ------------------------------------------------------------------
    // natives (registered by the c-saves module .so via RegisterNatives)
    // ------------------------------------------------------------------
    public static native boolean nActive();
    public static native int nAttach(String kind, String level, String containerDir);
    public static native byte[] nReadChunk(int handle, int cx, int cz);
    public static native boolean nWriteChunk(int handle, int cx, int cz, byte[] nbt);
    public static native boolean nBeginSave();
    public static native boolean nEndSave();
    public static native void nMarkAutosave();

    /** set -Dcsaves.wlog=true to log every chunk write */
    static final boolean WRITE_LOG = Boolean.getBoolean("csaves.wlog")
            || System.getenv("CSAVES_WLOG") != null;

    // ------------------------------------------------------------------
    // save-cycle probes (weave::insert_call_at_start targets, ()V statics)
    // ------------------------------------------------------------------
    public static void onSaveStart() {
        if (nActive()) nBeginSave();
    }
    public static void onSaveEnd() {
        if (nActive()) nEndSave();
    }
    public static void onAutosave() {
        if (nActive()) nMarkAutosave();
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
        return attach(st);
    }

    private static int attach(RegionFileStorage st) {
        int[] h = HANDLES.get(st);
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
            containerDir = new File(world, "c-saves" + File.pathSeparatorChar + kindName(kind))
                    .getAbsolutePath().replace(File.pathSeparatorChar, '/');
        } catch (Throwable t) {
            containerDir = "c-saves/" + kindName(kind);
        }
        int handle = nAttach(kindName(kind), st.info().level(), containerDir);
        HANDLES.put(st, new int[]{handle, kind});
        STORAGES.put(st, kindName(kind).getBytes());
        return handle;
    }

    private static int kindOfHandle(RegionFileStorage st) {
        int[] h = HANDLES.get(st);
        return h != null ? h[1] : kindOf(st);
    }

    private static String kindName(int kind) {
        return kind == KIND_ENTITY ? "entities" : kind == KIND_POI ? "poi" : "region";
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
    // pass and the task would retry forever (silent 28%-spawn-prep hang).
    // ------------------------------------------------------------------
    static final int RECALC_OWNED = Integer.MIN_VALUE;

    public static ReadData rfReadData(RegionFileStorage st, int x, int z) throws IOException {
        int h = ensureAttach(st);
        if (nActive()) {
            byte[] b = nReadChunk(h, x, z);
            if (b != null) {
                return new ReadData(ReadResult.HAS_DATA,
                        new DataInputStream(new ByteArrayInputStream(b)), null, RECALC_OWNED);
            }
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
        ReadData rd = new ReadData(ReadResult.HAS_DATA, in, null, recalc);
        return rd;
    }

    // ------------------------------------------------------------------
    // RegionFileStorage.moonrise$startWrite(int, int, CompoundTag) replacement
    // ==================================================================
    // Active: serialize the tag to uncompressed NBT (the exact bytes we keep
    // in the container), hand it to the engine, and return a WriteData whose
    // commit closure is a no-op — nothing ever touches the region file.
    // ------------------------------------------------------------------
    public static WriteData rfStartWrite(RegionFileStorage st, int x, int z, CompoundTag tag) throws IOException {
        if (tag == null) {
            return new WriteData(null, WriteResult.DELETE, null, null);
        }
        int h = ensureAttach(st);
        if (nActive()) {
            ByteArrayOutputStream bos = new ByteArrayOutputStream(1 << 14);
            NbtIo.write(tag, new DataOutputStream(bos));
            boolean ok = nWriteChunk(h, x, z, bos.toByteArray());
            if (!ok || WRITE_LOG) {
                System.err.println("[c-saves] startWrite(" + x + "," + z + ") status="
                    + tag.getString("Status") + " stored=" + ok);
            }
            return new WriteData(tag, WriteResult.WRITE,
                    new DataOutputStream(new ByteArrayOutputStream(0)), SavesShimNoop.INSTANCE);
        }
        return vanillaStartWrite(st, x, z, tag);
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
        int h = ensureAttach(st);
        if (nActive()) {
            byte[] b = nReadChunk(h, pos.x(), pos.z());
            if (b != null) {
                return FastNBTParser.nbtIoRead(new DataInputStream(new ByteArrayInputStream(b)));
            }
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
        int h = ensureAttach(st);
        if (nActive()) {
            ByteArrayOutputStream bos = new ByteArrayOutputStream(1 << 14);
            NbtIo.write(tag, new DataOutputStream(bos));
            boolean ok = nWriteChunk(h, pos.x(), pos.z(), bos.toByteArray());
            if (!ok || WRITE_LOG) {
                System.err.println("[c-saves] write(" + pos.x() + "," + pos.z() + ") status="
                    + tag.getString("Status") + " stored=" + ok);
            }
            return;
        }
        vanillaWrite(st, pos, tag);
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
        int h = ensureAttach(st);
        if (nActive()) {
            byte[] b = nReadChunk(h, pos.x(), pos.z());
            if (b != null) {
                NbtIo.parse(new DataInputStream(new ByteArrayInputStream(b)), visitor, NbtAccounter.unlimitedHeap());
                return;
            }
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
